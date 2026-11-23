//! Pure-Rust FAA 28-day CIFP (`FAACIFP18` ARINC 424-18) fetcher, two-pass airway & terminal
//! procedure compiler, and `FBCIFP01` zero-copy binary pack writer (`cifp_procedures.bin`).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use flightbag_core::{
    AirwayRouteClass, AltitudeConstraintKind, AuthoritySource, CifpAirway, CifpAirwayFix,
    CifpProcedure, CifpProcedureLeg, CifpTransition, CifpWaypointKind, EditionManifest,
    PathTerminator, ProcedureKind, ProcedurePhase, SourceDigest,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::GeoBbox;
use crate::cycles::ResolvedCycle;
use crate::fetch::{FetchError, NasrArchiveFingerprint, Transport};

/// Official FAA 28-day CIFP publication base directory.
pub const CIFP_BASE_URL: &str = "https://aeronav.faa.gov/Upload_313-d/cifp";

/// Binary magic header of `cifp_procedures.bin`.
pub const FBCIFP01_MAGIC: [u8; 8] = *b"FBCIFP01";

/// Schema version of the `FBCIFP01` binary format.
pub const FBCIFP01_VERSION: u32 = 1;

const DISCLAIMER: &str = "EXPERIMENTAL PROTOTYPE — NOT FOR NAVIGATION OR FLIGHT. \
Unverified research and visualization output; never use for real-world navigation, flight planning, \
or operational aviation decisions.";

/// Summary entity and leg counts compiled from `FAACIFP18`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CifpCounts {
    /// Surveyed airports (`PA`).
    pub airports: usize,
    /// VHF VOR/VORTAC/DME stations (`D `).
    pub vhf_navaids: usize,
    /// Non-directional beacons (`DB`).
    pub ndbs: usize,
    /// Enroute RNAV/IFR waypoints (`EA`).
    pub enroute_waypoints: usize,
    /// Terminal RNAV/IFR waypoints (`PC`).
    pub terminal_waypoints: usize,
    /// Runway landing thresholds (`PG`).
    pub runway_thresholds: usize,
    /// Enroute airways (`ER`).
    pub airways: usize,
    /// Total airway fix sequence records.
    pub airway_fixes: usize,
    /// Standard Instrument Departures (`PD` / `SID`).
    pub sids: usize,
    /// Standard Terminal Arrivals (`PE` / `STAR`).
    pub stars: usize,
    /// Instrument Approach Procedures (`PF` / `IAP`).
    pub approaches: usize,
    /// Total primary procedure legs across `PD`, `PE`, and `PF`.
    pub procedure_legs: usize,
}

/// Complete compiled CIFP airway and terminal procedure dataset for one 28-day AIRAC cycle.
#[derive(Clone, Debug)]
pub struct CompiledCifp {
    /// Verified 4-digit AIRAC code from `HDR01` (`"2611"`).
    pub airac_code: String,
    /// Sorted enroute airways (`V`, `J`, `T`, `Q`, ATS).
    pub airways: Vec<CifpAirway>,
    /// Sorted airport terminal procedures (`(airport_padded_8, kind, proc_padded_8)`).
    pub procedures: Vec<CifpProcedure>,
    /// Geographic bounding box `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
    /// Entity and leg counts.
    pub counts: CifpCounts,
}

/// Artifact metadata for `cifp_procedures.bin`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CifpBinaryArtifact {
    /// Relative filename (`"cifp_procedures.bin"`).
    pub file: String,
    /// Exact byte size of `cifp_procedures.bin`.
    pub size_bytes: u64,
    /// Lowercase 64-char SHA-256 hex digest of `cifp_procedures.bin`.
    pub sha256_hex: String,
    /// Geographic bounds `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
}

/// Published JSON cycle manifest (`cifp_faa_cifp.json`) written alongside `cifp_procedures.bin`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PublishedCifpManifest {
    /// Manifest schema version (`1`).
    pub schema_version: u32,
    /// Mandatory non-navigational disclaimer.
    pub disclaimer: String,
    /// Edition identifier (`"flightbag-us-cifp-YYYY-MM-DD"`).
    pub edition_id: String,
    /// Authority slug (`"faa_cifp"`).
    pub authority: String,
    /// Governing 4-digit AIRAC code (`"2611"`).
    pub airac_code: String,
    /// Cycle index from AIRAC `2001` (`2020-01-02`).
    pub cycle_index: i32,
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// Expiration date (`YYYY-MM-DD`, 28 days after `effective_date`).
    pub expires_date: String,
    /// Inclusive UTC activation timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_from_utc: String,
    /// Exclusive UTC expiration timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_until_utc: String,
    /// Official FAA CIFP `.zip` URL.
    pub upstream_url: String,
    /// Lowercase SHA-256 hex digest of `CIFP_YYMMDD.zip`.
    pub upstream_sha256_hex: String,
    /// Compiled `cifp_procedures.bin` metadata.
    pub cifp_binary: CifpBinaryArtifact,
    /// Compiled entity and leg counts.
    pub counts: CifpCounts,
}

/// Errors raised while fetching, parsing, or serializing a CIFP archive.
#[derive(Debug, Error)]
pub enum CifpCompileError {
    /// Filesystem I/O failure.
    #[error("CIFP filesystem error at {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// OS error.
        #[source]
        source: std::io::Error,
    },
    /// Invalid ZIP container or missing `FAACIFP18` entry.
    #[error("invalid CIFP zip archive ({context}): {reason}")]
    Zip {
        /// Archive path or entry name.
        context: String,
        /// Failure explanation.
        reason: String,
    },
    /// `HDR01FAACIFP18` missing or AIRAC cycle mismatch.
    #[error("FAACIFP18 header mismatch: expected AIRAC '{expected}', found '{found}'")]
    HeaderMismatch {
        /// Expected 4-digit AIRAC code.
        expected: String,
        /// Actual header value.
        found: String,
    },
    /// Binary table overflow or manifest error.
    #[error("CIFP serialization error: {0}")]
    Encode(String),
}

/// Formats the canonical FAA CIFP archive filename (`"CIFP_261029.zip"`) for `cycle`.
pub fn cifp_filename(cycle: &ResolvedCycle) -> String {
    let b = cycle.effective_date.as_bytes();
    let yy = b
        .get(2..4)
        .and_then(|s| std::str::from_utf8(s).ok())
        .unwrap_or("00");
    let mm = b
        .get(5..7)
        .and_then(|s| std::str::from_utf8(s).ok())
        .unwrap_or("00");
    let dd = b
        .get(8..10)
        .and_then(|s| std::str::from_utf8(s).ok())
        .unwrap_or("00");
    format!("CIFP_{yy}{mm}{dd}.zip")
}

/// Formats the official FAA 28-day CIFP download URL for `cycle`.
pub fn cifp_url(cycle: &ResolvedCycle) -> String {
    format!("{CIFP_BASE_URL}/{}", cifp_filename(cycle))
}

/// Probes whether FAA has published `cycle`'s 28-day CIFP archive via a 1-byte `Range` GET.
///
/// # Errors
///
/// Returns [`FetchError`] on network failure or unexpected HTTP status.
pub fn probe_cifp_published(
    transport: &dyn Transport,
    cycle: &ResolvedCycle,
) -> Result<bool, FetchError> {
    let url = cifp_url(cycle);
    let resp = transport.get_blocking(&url, Some("bytes=0-0"))?;
    match resp.status {
        200 | 206 => Ok(true),
        404 | 410 => Ok(false),
        status => Err(FetchError::Status { url, status }),
    }
}

/// Hashes an existing local `CIFP_YYMMDD.zip` without network access.
///
/// # Errors
///
/// Returns [`FetchError`] if `zip_path` cannot be read or is empty.
pub fn fingerprint_local_cifp_zip(
    cycle: &ResolvedCycle,
    zip_path: &Path,
) -> Result<NasrArchiveFingerprint, FetchError> {
    let bytes = fs::read(zip_path).map_err(|source| FetchError::Io {
        path: zip_path.to_path_buf(),
        source,
    })?;
    if bytes.is_empty() {
        return Err(FetchError::EmptyBody {
            url: zip_path.display().to_string(),
        });
    }
    let sha256_hex = format!("{:x}", Sha256::digest(&bytes));
    let filename = zip_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| cifp_filename(cycle));
    Ok(NasrArchiveFingerprint {
        cycle: cycle.effective_date.clone(),
        airac_code: cycle.airac_code.clone(),
        filename,
        url: cifp_url(cycle),
        sha256_hex,
        size_bytes: bytes.len() as u64,
        cached: true,
        path: zip_path.to_path_buf(),
    })
}

/// Downloads `cycle`'s `CIFP_YYMMDD.zip` into `cache_root/<YYYY-MM-DD>/`, reusing SHA-256-verified cached bytes.
///
/// # Errors
///
/// Returns [`FetchError`] on network, HTTP status, I/O, or SHA-256 mismatch.
pub fn fetch_cifp_zip(
    transport: &dyn Transport,
    cycle: &ResolvedCycle,
    cache_root: &Path,
) -> Result<NasrArchiveFingerprint, FetchError> {
    let filename = cifp_filename(cycle);
    let url = cifp_url(cycle);
    let cycle_dir = cache_root.join(&cycle.effective_date);
    let dest = cycle_dir.join(&filename);
    let sidecar = cycle_dir.join(format!("{filename}.sha256"));

    if dest.is_file() && sidecar.is_file() {
        let recorded = fs::read_to_string(&sidecar)
            .map_err(|source| FetchError::Io {
                path: sidecar.clone(),
                source,
            })?
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = fs::read(&dest).map_err(|source| FetchError::Io {
            path: dest.clone(),
            source,
        })?;
        let actual = format!("{:x}", Sha256::digest(&bytes));
        if actual != recorded {
            return Err(FetchError::ChecksumMismatch {
                path: dest,
                actual,
                expected: recorded,
            });
        }
        if !bytes.is_empty() {
            return Ok(NasrArchiveFingerprint {
                cycle: cycle.effective_date.clone(),
                airac_code: cycle.airac_code.clone(),
                filename,
                url,
                sha256_hex: actual,
                size_bytes: bytes.len() as u64,
                cached: true,
                path: dest,
            });
        }
    }

    fs::create_dir_all(&cycle_dir).map_err(|source| FetchError::Io {
        path: cycle_dir.clone(),
        source,
    })?;
    let resp = transport.get_blocking(&url, None)?;
    if resp.status != 200 {
        return Err(FetchError::Status {
            url,
            status: resp.status,
        });
    }
    let mut body = Vec::new();
    let mut reader = resp.body;
    reader
        .read_to_end(&mut body)
        .map_err(|source| FetchError::Io {
            path: dest.clone(),
            source,
        })?;
    if body.is_empty() {
        return Err(FetchError::EmptyBody { url });
    }
    let sha256_hex = format!("{:x}", Sha256::digest(&body));
    let part = cycle_dir.join(format!("{filename}.part"));
    fs::write(&part, &body).map_err(|source| FetchError::Io {
        path: part.clone(),
        source,
    })?;
    fs::rename(&part, &dest).map_err(|source| FetchError::Io {
        path: dest.clone(),
        source,
    })?;
    fs::write(&sidecar, format!("{sha256_hex}\n")).map_err(|source| FetchError::Io {
        path: sidecar,
        source,
    })?;

    Ok(NasrArchiveFingerprint {
        cycle: cycle.effective_date.clone(),
        airac_code: cycle.airac_code.clone(),
        filename,
        url,
        sha256_hex,
        size_bytes: body.len() as u64,
        cached: false,
        path: dest,
    })
}

/// Extracts `FAACIFP18` from `zip_path` and compiles all enroute airways (`ER`) and airport
/// terminal procedures (`PD`, `PE`, `PF`).
///
/// # Errors
///
/// Returns [`CifpCompileError`] if `FAACIFP18` is missing or fails header cycle verification.
pub fn compile_cifp_zip(
    zip_path: &Path,
    expected_airac_code: &str,
    bbox_filter: Option<GeoBbox>,
) -> Result<CompiledCifp, CifpCompileError> {
    let file = File::open(zip_path).map_err(|source| CifpCompileError::Io {
        path: zip_path.to_path_buf(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|err| CifpCompileError::Zip {
        context: zip_path.display().to_string(),
        reason: err.to_string(),
    })?;
    let mut entry_bytes = Vec::new();
    {
        let mut entry = archive
            .by_name("FAACIFP18")
            .map_err(|err| CifpCompileError::Zip {
                context: "FAACIFP18".to_owned(),
                reason: err.to_string(),
            })?;
        entry
            .read_to_end(&mut entry_bytes)
            .map_err(|source| CifpCompileError::Io {
                path: zip_path.to_path_buf(),
                source,
            })?;
    }
    compile_cifp_bytes(&entry_bytes, expected_airac_code, bbox_filter)
}

#[derive(Default)]
struct FixDictionary {
    airports: BTreeMap<String, (i32, i32)>,
    runways: BTreeMap<(String, String), (i32, i32)>,
    terminal_by_apt: BTreeMap<(String, String), (i32, i32)>,
    terminal_by_reg: BTreeMap<([u8; 2], String), (i32, i32)>,
    enroute_by_reg: BTreeMap<([u8; 2], String), (i32, i32)>,
    enroute_any: BTreeMap<String, (i32, i32)>,
    vhf_by_reg: BTreeMap<([u8; 2], String), (i32, i32)>,
    vhf_any: BTreeMap<String, (i32, i32)>,
    ndb_by_reg: BTreeMap<([u8; 2], String), (i32, i32)>,
    ndb_any: BTreeMap<String, (i32, i32)>,
}

impl FixDictionary {
    fn resolve(
        &self,
        airport_ctx: &str,
        fix_ident: &str,
        region: [u8; 2],
        sec: &[u8],
    ) -> (CifpWaypointKind, Option<(i32, i32)>) {
        if fix_ident.is_empty() {
            return (CifpWaypointKind::Unfixed, None);
        }
        match sec {
            b"PA" => (
                CifpWaypointKind::Airport,
                self.airports.get(fix_ident).copied(),
            ),
            b"PG" => (
                CifpWaypointKind::RunwayThreshold,
                self.runways
                    .get(&(airport_ctx.to_owned(), fix_ident.to_owned()))
                    .copied(),
            ),
            b"PC" => {
                let by_apt = self
                    .terminal_by_apt
                    .get(&(airport_ctx.to_owned(), fix_ident.to_owned()))
                    .copied();
                let pos = by_apt.or_else(|| {
                    self.terminal_by_reg
                        .get(&(region, fix_ident.to_owned()))
                        .copied()
                });
                (CifpWaypointKind::TerminalWaypoint, pos)
            }
            b"EA" => {
                let pos = self
                    .enroute_by_reg
                    .get(&(region, fix_ident.to_owned()))
                    .copied()
                    .or_else(|| self.enroute_any.get(fix_ident).copied());
                (CifpWaypointKind::EnrouteWaypoint, pos)
            }
            b"D " => {
                let pos = self
                    .vhf_by_reg
                    .get(&(region, fix_ident.to_owned()))
                    .copied()
                    .or_else(|| self.vhf_any.get(fix_ident).copied());
                (CifpWaypointKind::VhfNavaid, pos)
            }
            b"DB" | b"PN" => {
                let pos = self
                    .ndb_by_reg
                    .get(&(region, fix_ident.to_owned()))
                    .copied()
                    .or_else(|| self.ndb_any.get(fix_ident).copied());
                (CifpWaypointKind::Ndb, pos)
            }
            _ => {
                let pos = self
                    .enroute_by_reg
                    .get(&(region, fix_ident.to_owned()))
                    .copied()
                    .or_else(|| {
                        self.vhf_by_reg
                            .get(&(region, fix_ident.to_owned()))
                            .copied()
                    })
                    .or_else(|| self.airports.get(fix_ident).copied());
                (CifpWaypointKind::EnrouteWaypoint, pos)
            }
        }
    }
}

/// Compiles raw `FAACIFP18` bytes (`132`-column ARINC 424-18 records) into deterministic
/// [`CompiledCifp`] airways and procedures.
///
/// # Errors
///
/// Returns [`CifpCompileError::HeaderMismatch`] if `HDR01` does not carry `expected_airac_code`.
pub fn compile_cifp_bytes(
    faacifp18_bytes: &[u8],
    expected_airac_code: &str,
    bbox_filter: Option<GeoBbox>,
) -> Result<CompiledCifp, CifpCompileError> {
    let mut lines_iter = faacifp18_bytes
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l));

    let header = lines_iter.next().unwrap_or(b"");
    let found_airac = ascii_trim(cols(header, 36, 39));
    if !header.starts_with(b"HDR01") || found_airac != expected_airac_code {
        return Err(CifpCompileError::HeaderMismatch {
            expected: expected_airac_code.to_owned(),
            found: found_airac,
        });
    }

    let mut dict = FixDictionary::default();
    let mut counts = CifpCounts::default();

    // Pass 1: Coordinate dictionary (`PA`, `PG`, `PC`, `EA`, `D `, `DB`).
    for line in faacifp18_bytes
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
    {
        if line.len() < 132 || line.first() != Some(&b'S') {
            continue;
        }
        if !matches!(col_byte(line, 22), Some(b'0' | b'1')) {
            continue;
        }
        match col_byte(line, 5) {
            Some(b'P') => match col_byte(line, 13) {
                Some(b'A') => {
                    let apt = ascii_trim(cols(line, 7, 10));
                    if !apt.is_empty()
                        && let Some(pos) = parse_lat_lon_e6(cols(line, 33, 41), cols(line, 42, 51))
                    {
                        dict.airports.insert(apt, pos);
                        counts.airports = counts.airports.saturating_add(1);
                    }
                }
                Some(b'G') => {
                    let apt = ascii_trim(cols(line, 7, 10));
                    let rwy = ascii_trim(cols(line, 14, 18));
                    if !apt.is_empty()
                        && !rwy.is_empty()
                        && let Some(pos) = parse_lat_lon_e6(cols(line, 33, 41), cols(line, 42, 51))
                    {
                        dict.runways.insert((apt, rwy), pos);
                        counts.runway_thresholds = counts.runway_thresholds.saturating_add(1);
                    }
                }
                Some(b'C') => {
                    let apt = ascii_trim(cols(line, 7, 10));
                    let fix = ascii_trim(cols(line, 14, 18));
                    let reg = two_bytes(cols(line, 20, 21));
                    if !fix.is_empty()
                        && let Some(pos) = parse_lat_lon_e6(cols(line, 33, 41), cols(line, 42, 51))
                    {
                        if !apt.is_empty() {
                            dict.terminal_by_apt.insert((apt, fix.clone()), pos);
                        }
                        dict.terminal_by_reg.insert((reg, fix), pos);
                        counts.terminal_waypoints = counts.terminal_waypoints.saturating_add(1);
                    }
                }
                _ => {}
            },
            Some(b'E') if col_byte(line, 6) == Some(b'A') => {
                let fix = ascii_trim(cols(line, 14, 18));
                let reg = two_bytes(cols(line, 20, 21));
                if !fix.is_empty()
                    && let Some(pos) = parse_lat_lon_e6(cols(line, 33, 41), cols(line, 42, 51))
                {
                    dict.enroute_any.entry(fix.clone()).or_insert(pos);
                    dict.enroute_by_reg.insert((reg, fix), pos);
                    counts.enroute_waypoints = counts.enroute_waypoints.saturating_add(1);
                }
            }
            Some(b'D') => match col_byte(line, 6) {
                Some(b' ') => {
                    let ident = ascii_trim(cols(line, 14, 17));
                    let reg = two_bytes(cols(line, 20, 21));
                    let pos = parse_lat_lon_e6(cols(line, 56, 64), cols(line, 65, 74))
                        .or_else(|| parse_lat_lon_e6(cols(line, 33, 41), cols(line, 42, 51)));
                    if !ident.is_empty()
                        && let Some(p) = pos
                    {
                        dict.vhf_any.entry(ident.clone()).or_insert(p);
                        dict.vhf_by_reg.insert((reg, ident), p);
                        counts.vhf_navaids = counts.vhf_navaids.saturating_add(1);
                    }
                }
                Some(b'B') => {
                    let ident = ascii_trim(cols(line, 14, 17));
                    let reg = two_bytes(cols(line, 20, 21));
                    if !ident.is_empty()
                        && let Some(p) = parse_lat_lon_e6(cols(line, 33, 41), cols(line, 42, 51))
                    {
                        dict.ndb_any.entry(ident.clone()).or_insert(p);
                        dict.ndb_by_reg.insert((reg, ident), p);
                        counts.ndbs = counts.ndbs.saturating_add(1);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    // Pass 2: Assemble `ER` airways and `PD`/`PE`/`PF` airport procedures.
    let mut airway_map: BTreeMap<[u8; 8], (String, Vec<CifpAirwayFix>)> = BTreeMap::new();
    type ProcKey = ([u8; 8], u8, [u8; 8]);
    let mut proc_map: BTreeMap<ProcKey, CifpProcedure> = BTreeMap::new();
    let mut min_lon = 180.0_f64;
    let mut min_lat = 90.0_f64;
    let mut max_lon = -180.0_f64;
    let mut max_lat = -90.0_f64;
    let mut any_coord = false;

    let mut observe_coord = |(lat_e6, lon_e6): (i32, i32)| {
        let lat = f64::from(lat_e6) / 1_000_000.0;
        let lon = f64::from(lon_e6) / 1_000_000.0;
        min_lon = min_lon.min(lon);
        max_lon = max_lon.max(lon);
        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
        any_coord = true;
    };

    for line in faacifp18_bytes
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
    {
        if line.len() < 132 || line.first() != Some(&b'S') {
            continue;
        }
        // Col 39 (`0-based 38`) is the Continuation Record No on both `ER` and `PD/PE/PF`.
        if !matches!(col_byte(line, 39), Some(b'0' | b'1')) {
            continue;
        }

        if cols(line, 5, 6) == b"ER" {
            let route_ident = ascii_trim(cols(line, 14, 18));
            let fix_ident = ascii_trim(cols(line, 30, 34));
            if route_ident.is_empty() || fix_ident.is_empty() {
                continue;
            }
            let seq_num = parse_u16_ascii(cols(line, 26, 29));
            let reg = two_bytes(cols(line, 35, 36));
            let sec = cols(line, 37, 38);
            let (kind, pos_opt) = dict.resolve("", &fix_ident, reg, sec);
            let Some(pos) = pos_opt else {
                continue;
            };
            if let Some(bbox) = bbox_filter
                && !point_in_bbox(pos, bbox)
            {
                continue;
            }
            observe_coord(pos);
            let course_dist_blank = ascii_trim(cols(line, 71, 78)).is_empty();
            let min_alt_raw = ascii_trim(cols(line, 84, 88));
            let max_alt_raw = ascii_trim(cols(line, 94, 98));
            let min_alt_ft = parse_arinc_altitude_ft(&min_alt_raw);
            let max_alt_ft = parse_arinc_altitude_ft(&max_alt_raw);
            let segment_break_after = min_alt_raw.is_empty() && course_dist_blank;

            let key = pad_ident_8(&route_ident);
            let entry = airway_map
                .entry(key)
                .or_insert_with(|| (route_ident, Vec::new()));
            entry.1.push(CifpAirwayFix {
                seq_num,
                fix_ident,
                icao_region: reg,
                kind,
                latitude_e6: pos.0,
                longitude_e6: pos.1,
                min_alt_ft,
                max_alt_ft,
                segment_break_after,
            });
            counts.airway_fixes = counts.airway_fixes.saturating_add(1);
        } else if col_byte(line, 5) == Some(b'P')
            && let Some(proc_kind) = match col_byte(line, 13) {
                Some(b'D') => Some(ProcedureKind::Sid),
                Some(b'E') => Some(ProcedureKind::Star),
                Some(b'F') => Some(ProcedureKind::Approach),
                _ => None,
            }
        {
            let airport_ident = ascii_trim(cols(line, 7, 10));
            let procedure_ident = ascii_trim(cols(line, 14, 19));
            if airport_ident.is_empty() || procedure_ident.is_empty() {
                continue;
            }
            if let Some(bbox) = bbox_filter
                && let Some(&apt_pos) = dict.airports.get(&airport_ident)
                && !point_in_bbox(apt_pos, bbox)
            {
                continue;
            }
            let route_type = col_byte(line, 20).unwrap_or(b' ');
            let transition_ident = ascii_trim(cols(line, 21, 25));
            let phase = classify_phase(proc_kind, route_type, &transition_ident);
            let seq_num = parse_u16_ascii(cols(line, 27, 29));
            let fix_ident = ascii_trim(cols(line, 30, 34));
            let fix_reg = two_bytes(cols(line, 35, 36));
            let fix_sec = cols(line, 37, 38);
            let (kind, pos_opt) = dict.resolve(&airport_ident, &fix_ident, fix_reg, fix_sec);
            if let Some(pos) = pos_opt {
                observe_coord(pos);
            }

            let desc = cols(line, 40, 43);
            let d2 = desc.get(2).copied().unwrap_or(b' ');
            let d3 = desc.get(3).copied().unwrap_or(b' ');
            let is_iaf = d3 == b'A';
            let is_faf = d3 == b'F';
            let is_mapt = d3 == b'M';

            let path_term = PathTerminator::from_str_2(&ascii_trim(cols(line, 48, 49)));
            let alt_desc = col_byte(line, 83).unwrap_or(b' ');
            let alt1_ft = parse_arinc_altitude_ft(&ascii_trim(cols(line, 85, 89)));
            let alt2_ft = parse_arinc_altitude_ft(&ascii_trim(cols(line, 90, 94)));
            let alt_constraint = classify_alt_constraint(alt_desc, alt1_ft, alt2_ft);
            let speed_limit_kt = parse_u16_ascii(cols(line, 100, 102));

            let pkey: ProcKey = (
                pad_ident_8(&airport_ident),
                proc_kind.code(),
                pad_ident_8(&procedure_ident),
            );
            let proc_entry = proc_map.entry(pkey).or_insert_with(|| CifpProcedure {
                airport_ident: airport_ident.clone(),
                kind: proc_kind,
                procedure_ident: procedure_ident.clone(),
                route_type,
                transitions: Vec::new(),
            });
            if phase == ProcedurePhase::CommonCore {
                proc_entry.route_type = route_type;
            }

            let trans_idx = if let Some(pos_idx) = proc_entry
                .transitions
                .iter()
                .position(|t| t.phase == phase && t.transition_ident == transition_ident)
            {
                pos_idx
            } else {
                proc_entry.transitions.push(CifpTransition {
                    transition_ident,
                    phase,
                    route_type,
                    legs: Vec::new(),
                });
                proc_entry.transitions.len().saturating_sub(1)
            };

            if let Some(trans_slot) = proc_entry.transitions.get_mut(trans_idx) {
                let prev_missed = trans_slot.legs.last().is_some_and(|l| l.is_missed_approach);
                let is_missed_approach = prev_missed || d2 == b'M';
                let (lat_e6, lon_e6, has_coordinates) = match pos_opt {
                    Some((la, lo)) => (la, lo, true),
                    None => (0, 0, false),
                };
                trans_slot.legs.push(CifpProcedureLeg {
                    seq_num,
                    fix_ident,
                    kind,
                    path_terminator: path_term,
                    has_coordinates,
                    latitude_e6: lat_e6,
                    longitude_e6: lon_e6,
                    alt_constraint,
                    alt1_ft,
                    alt2_ft,
                    speed_limit_kt,
                    is_missed_approach,
                    is_iaf,
                    is_faf,
                    is_mapt,
                });
                counts.procedure_legs = counts.procedure_legs.saturating_add(1);
            }
        }
    }

    let mut airways = Vec::with_capacity(airway_map.len());
    for (_key, (route_ident, mut fixes)) in airway_map {
        if let Some(last) = fixes.last_mut() {
            last.segment_break_after = true;
        }
        let route_class = AirwayRouteClass::from_route_ident(&route_ident);
        airways.push(CifpAirway {
            route_ident,
            route_class,
            fixes,
        });
    }
    counts.airways = airways.len();

    let mut procedures = Vec::with_capacity(proc_map.len());
    for (_key, mut proc_item) in proc_map {
        proc_item.transitions.sort_by(|a, b| {
            a.phase
                .cmp(&b.phase)
                .then_with(|| a.transition_ident.cmp(&b.transition_ident))
        });
        match proc_item.kind {
            ProcedureKind::Sid => counts.sids = counts.sids.saturating_add(1),
            ProcedureKind::Star => counts.stars = counts.stars.saturating_add(1),
            ProcedureKind::Approach => counts.approaches = counts.approaches.saturating_add(1),
        }
        procedures.push(proc_item);
    }

    let bounds = if any_coord {
        [min_lon, min_lat, max_lon, max_lat]
    } else {
        [-180.0, -14.58, 180.0, 82.0]
    };

    Ok(CompiledCifp {
        airac_code: expected_airac_code.to_owned(),
        airways,
        procedures,
        bounds,
        counts,
    })
}

/// Encodes `compiled` into a deterministic `FBCIFP01` binary image (`cifp_procedures.bin`).
///
/// # Errors
///
/// Returns [`CifpCompileError::Encode`] if any table exceeds `u32` or `u16` format bounds.
pub fn encode_cifp_binary(compiled: &CompiledCifp) -> Result<Vec<u8>, CifpCompileError> {
    let airway_count = u32::try_from(compiled.airways.len())
        .map_err(|_| CifpCompileError::Encode("airway_count overflow".into()))?;
    let total_awy_fixes: usize = compiled.airways.iter().map(|a| a.fixes.len()).sum();
    let airway_fix_count = u32::try_from(total_awy_fixes)
        .map_err(|_| CifpCompileError::Encode("airway_fix_count overflow".into()))?;

    let procedure_count = u32::try_from(compiled.procedures.len())
        .map_err(|_| CifpCompileError::Encode("procedure_count overflow".into()))?;
    let total_transitions: usize = compiled
        .procedures
        .iter()
        .map(|p| p.transitions.len())
        .sum();
    let transition_count = u32::try_from(total_transitions)
        .map_err(|_| CifpCompileError::Encode("transition_count overflow".into()))?;
    let total_legs: usize = compiled
        .procedures
        .iter()
        .flat_map(|p| p.transitions.iter())
        .map(|t| t.legs.len())
        .sum();
    let leg_count = u32::try_from(total_legs)
        .map_err(|_| CifpCompileError::Encode("leg_count overflow".into()))?;

    let total_bytes = 48_usize
        .saturating_add(compiled.airways.len().saturating_mul(16))
        .saturating_add(total_awy_fixes.saturating_mul(24))
        .saturating_add(compiled.procedures.len().saturating_mul(24))
        .saturating_add(total_transitions.saturating_mul(16))
        .saturating_add(total_legs.saturating_mul(32));

    let mut out = Vec::with_capacity(total_bytes);

    // Header (`48` bytes).
    out.extend_from_slice(&FBCIFP01_MAGIC);
    out.extend_from_slice(&FBCIFP01_VERSION.to_le_bytes());
    let mut airac_4 = [b'0'; 4];
    for (dst, src) in airac_4.iter_mut().zip(compiled.airac_code.bytes()) {
        *dst = src;
    }
    out.extend_from_slice(&airac_4);
    out.extend_from_slice(&airway_count.to_le_bytes());
    out.extend_from_slice(&airway_fix_count.to_le_bytes());
    out.extend_from_slice(&procedure_count.to_le_bytes());
    out.extend_from_slice(&transition_count.to_le_bytes());
    out.extend_from_slice(&leg_count.to_le_bytes());
    out.extend_from_slice(&[0_u8; 12]);

    // Table 1: Airways (`16` bytes each).
    let mut next_awy_fix_idx: u32 = 0;
    for awy in &compiled.airways {
        let fix_cnt = u16::try_from(awy.fixes.len()).map_err(|_| {
            CifpCompileError::Encode(format!("airway {} exceeds u16 fixes", awy.route_ident))
        })?;
        out.extend_from_slice(&pad_ident_8(&awy.route_ident));
        out.push(awy.route_class.code());
        out.push(0);
        out.extend_from_slice(&fix_cnt.to_le_bytes());
        out.extend_from_slice(&next_awy_fix_idx.to_le_bytes());
        next_awy_fix_idx = next_awy_fix_idx.saturating_add(u32::from(fix_cnt));
    }

    // Table 2: Airway Fixes (`24` bytes each).
    for awy in &compiled.airways {
        for fx in &awy.fixes {
            out.extend_from_slice(&pad_ident_8(&fx.fix_ident));
            out.extend_from_slice(&fx.icao_region);
            out.push(fx.kind.code());
            out.push(u8::from(fx.segment_break_after));
            out.extend_from_slice(&fx.latitude_e6.to_le_bytes());
            out.extend_from_slice(&fx.longitude_e6.to_le_bytes());
            let min_h = u16::try_from((fx.min_alt_ft / 100).min(65_535)).unwrap_or(u16::MAX);
            let max_h = u16::try_from((fx.max_alt_ft / 100).min(65_535)).unwrap_or(u16::MAX);
            out.extend_from_slice(&min_h.to_le_bytes());
            out.extend_from_slice(&max_h.to_le_bytes());
        }
    }

    // Table 3: Procedures (`24` bytes each).
    let mut next_trans_idx: u32 = 0;
    for proc_item in &compiled.procedures {
        let tr_cnt = u16::try_from(proc_item.transitions.len()).map_err(|_| {
            CifpCompileError::Encode(format!(
                "procedure {} exceeds u16 transitions",
                proc_item.procedure_ident
            ))
        })?;
        out.extend_from_slice(&pad_ident_8(&proc_item.airport_ident));
        out.extend_from_slice(&pad_ident_8(&proc_item.procedure_ident));
        out.push(proc_item.kind.code());
        out.push(proc_item.route_type);
        out.extend_from_slice(&tr_cnt.to_le_bytes());
        out.extend_from_slice(&next_trans_idx.to_le_bytes());
        next_trans_idx = next_trans_idx.saturating_add(u32::from(tr_cnt));
    }

    // Table 4: Procedure Transitions (`16` bytes each).
    let mut next_leg_idx: u32 = 0;
    for proc_item in &compiled.procedures {
        for tr in &proc_item.transitions {
            let lg_cnt = u16::try_from(tr.legs.len()).map_err(|_| {
                CifpCompileError::Encode(format!(
                    "transition {} exceeds u16 legs",
                    tr.transition_ident
                ))
            })?;
            out.extend_from_slice(&pad_ident_8(&tr.transition_ident));
            out.push(tr.phase.code());
            out.push(tr.route_type);
            out.extend_from_slice(&lg_cnt.to_le_bytes());
            out.extend_from_slice(&next_leg_idx.to_le_bytes());
            next_leg_idx = next_leg_idx.saturating_add(u32::from(lg_cnt));
        }
    }

    // Table 5: Procedure Legs (`32` bytes each).
    for proc_item in &compiled.procedures {
        for tr in &proc_item.transitions {
            for leg in &tr.legs {
                let mut flags = 0_u8;
                if leg.has_coordinates {
                    flags |= 1 << 0;
                }
                if leg.is_missed_approach {
                    flags |= 1 << 1;
                }
                if leg.is_iaf {
                    flags |= 1 << 2;
                }
                if leg.is_faf {
                    flags |= 1 << 3;
                }
                if leg.is_mapt {
                    flags |= 1 << 4;
                }
                out.extend_from_slice(&pad_ident_8(&leg.fix_ident));
                out.extend_from_slice(&leg.path_terminator.0);
                out.push(leg.kind.code());
                out.push(leg.alt_constraint.code());
                out.push(flags);
                out.push(0);
                out.extend_from_slice(&leg.speed_limit_kt.to_le_bytes());
                out.extend_from_slice(&leg.latitude_e6.to_le_bytes());
                out.extend_from_slice(&leg.longitude_e6.to_le_bytes());
                out.extend_from_slice(&leg.alt1_ft.to_le_bytes());
                out.extend_from_slice(&leg.alt2_ft.to_le_bytes());
            }
        }
    }

    Ok(out)
}

/// Writes `cifp_procedures.bin`, `cifp_procedures.bin.sha256`, and `cifp_faa_cifp.json`, returning
/// validated [`EditionManifest`] and [`PublishedCifpManifest`] records.
///
/// # Errors
///
/// Returns [`CifpCompileError`] if encoding, digest validation, or file writing fails.
pub fn write_cifp_pack(
    compiled: &CompiledCifp,
    cycle: &ResolvedCycle,
    upstream: &NasrArchiveFingerprint,
    bin_path: &Path,
    manifest_path: &Path,
) -> Result<(EditionManifest, PublishedCifpManifest), CifpCompileError> {
    let bin_bytes = encode_cifp_binary(compiled)?;
    let size_bytes = bin_bytes.len() as u64;
    let sha256_hex = format!("{:x}", Sha256::digest(&bin_bytes));

    if let Some(parent) = bin_path.parent() {
        fs::create_dir_all(parent).map_err(|source| CifpCompileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let part = bin_path.with_extension("bin.part");
    fs::write(&part, &bin_bytes).map_err(|source| CifpCompileError::Io {
        path: part.clone(),
        source,
    })?;
    fs::rename(&part, bin_path).map_err(|source| CifpCompileError::Io {
        path: bin_path.to_path_buf(),
        source,
    })?;

    let bin_filename = bin_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cifp_procedures.bin".to_owned());
    let sidecar_path = bin_path.with_file_name(format!("{bin_filename}.sha256"));
    fs::write(&sidecar_path, format!("{sha256_hex}  {bin_filename}\n")).map_err(|source| {
        CifpCompileError::Io {
            path: sidecar_path,
            source,
        }
    })?;

    let cifp_digest = SourceDigest::new(
        AuthoritySource::FaaCifpArinc424,
        cycle.effective_date.clone(),
        &upstream.sha256_hex,
    )
    .map_err(|e| CifpCompileError::Encode(e.to_string()))?;

    let edition_id = format!("flightbag-us-cifp-{}", cycle.effective_date);
    let core_manifest = EditionManifest {
        schema_version: 1,
        edition_id: edition_id.clone(),
        airac: cycle.airac,
        sources: vec![cifp_digest],
        signature_ed25519_hex: None,
    };

    let published = PublishedCifpManifest {
        schema_version: 1,
        disclaimer: DISCLAIMER.to_owned(),
        edition_id,
        authority: "faa_cifp".to_owned(),
        airac_code: cycle.airac_code.clone(),
        cycle_index: cycle.cycle_index,
        effective_date: cycle.effective_date.clone(),
        expires_date: cycle.expires_date.clone(),
        effective_from_utc: cycle.effective_from_utc.clone(),
        effective_until_utc: cycle.effective_until_utc.clone(),
        upstream_url: upstream.url.clone(),
        upstream_sha256_hex: upstream.sha256_hex.clone(),
        cifp_binary: CifpBinaryArtifact {
            file: bin_filename,
            size_bytes,
            sha256_hex,
            bounds: compiled.bounds,
        },
        counts: compiled.counts.clone(),
    };

    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent).map_err(|source| CifpCompileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut json_bytes = serde_json::to_vec_pretty(&published)
        .map_err(|e| CifpCompileError::Encode(e.to_string()))?;
    json_bytes.push(b'\n');
    fs::write(manifest_path, json_bytes).map_err(|source| CifpCompileError::Io {
        path: manifest_path.to_path_buf(),
        source,
    })?;

    Ok((core_manifest, published))
}

fn cols(line: &[u8], from_1based: usize, to_1based: usize) -> &[u8] {
    line.get(from_1based.saturating_sub(1)..to_1based)
        .unwrap_or(&[])
}

fn col_byte(line: &[u8], col_1based: usize) -> Option<u8> {
    line.get(col_1based.saturating_sub(1)).copied()
}

fn ascii_trim(raw: &[u8]) -> String {
    let mut start = 0_usize;
    let mut end = raw.len();
    while start < end && raw.get(start) == Some(&b' ') {
        start += 1;
    }
    while end > start && raw.get(end.saturating_sub(1)) == Some(&b' ') {
        end -= 1;
    }
    let mut out = String::with_capacity(end.saturating_sub(start));
    if let Some(slice) = raw.get(start..end) {
        for &b in slice {
            if b.is_ascii() {
                out.push((b as char).to_ascii_uppercase());
            }
        }
    }
    out
}

fn two_bytes(raw: &[u8]) -> [u8; 2] {
    [
        raw.first().copied().unwrap_or(b' ').to_ascii_uppercase(),
        raw.get(1).copied().unwrap_or(b' ').to_ascii_uppercase(),
    ]
}

fn pad_ident_8(ident: &str) -> [u8; 8] {
    let mut out = [0_u8; 8];
    for (dst, src) in out.iter_mut().zip(ident.bytes()) {
        *dst = src.to_ascii_uppercase();
    }
    out
}

fn parse_u16_ascii(raw: &[u8]) -> u16 {
    let s = ascii_trim(raw);
    s.parse::<u16>().unwrap_or(0)
}

fn parse_arinc_altitude_ft(trimmed: &str) -> u32 {
    if trimmed.is_empty() || trimmed == "UNKNN" {
        return 0;
    }
    if let Some(fl) = trimmed.strip_prefix("FL") {
        return fl.trim().parse::<u32>().unwrap_or(0).saturating_mul(100);
    }
    trimmed.parse::<u32>().unwrap_or(0)
}

fn classify_alt_constraint(alt_desc: u8, alt1_ft: u32, alt2_ft: u32) -> AltitudeConstraintKind {
    if alt1_ft == 0 {
        return AltitudeConstraintKind::None;
    }
    match alt_desc {
        b'+' => AltitudeConstraintKind::AtOrAbove,
        b'-' => AltitudeConstraintKind::AtOrBelow,
        b'B' if alt2_ft > 0 => AltitudeConstraintKind::Between,
        _ => AltitudeConstraintKind::At,
    }
}

fn classify_phase(kind: ProcedureKind, route_type: u8, transition_ident: &str) -> ProcedurePhase {
    match kind {
        ProcedureKind::Sid => match route_type {
            b'1' | b'4' | b'F' => ProcedurePhase::RunwayTransition,
            b'3' | b'6' | b'S' | b'V' => ProcedurePhase::EnrouteOrFeederTransition,
            _ => {
                if transition_ident.starts_with("RW") {
                    ProcedurePhase::RunwayTransition
                } else {
                    ProcedurePhase::CommonCore
                }
            }
        },
        ProcedureKind::Star => match route_type {
            b'1' | b'4' | b'7' | b'F' => ProcedurePhase::EnrouteOrFeederTransition,
            b'3' | b'6' | b'9' => ProcedurePhase::RunwayTransition,
            _ => {
                if transition_ident.starts_with("RW") {
                    ProcedurePhase::RunwayTransition
                } else {
                    ProcedurePhase::CommonCore
                }
            }
        },
        ProcedureKind::Approach => {
            if route_type == b'A' {
                ProcedurePhase::EnrouteOrFeederTransition
            } else {
                ProcedurePhase::CommonCore
            }
        }
    }
}

fn parse_lat_lon_e6(lat_raw: &[u8], lon_raw: &[u8]) -> Option<(i32, i32)> {
    let lat_deg = parse_dms_centisec(lat_raw, 2, b'N', b'S')?;
    let lon_deg = parse_dms_centisec(lon_raw, 3, b'E', b'W')?;
    let lat_e6 = (lat_deg * 1_000_000.0).round() as i32;
    let lon_e6 = (lon_deg * 1_000_000.0).round() as i32;
    Some((lat_e6, lon_e6))
}

fn parse_dms_centisec(raw: &[u8], deg_digits: usize, pos_hem: u8, neg_hem: u8) -> Option<f64> {
    let (&hem, rest) = raw.split_first()?;
    if rest.len() != deg_digits.saturating_add(6) || !rest.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let parse_slice = |s: &[u8]| -> f64 {
        f64::from(s.iter().fold(0_u32, |acc, &b| {
            acc.saturating_mul(10) + u32::from(b - b'0')
        }))
    };
    let deg = parse_slice(rest.get(..deg_digits)?);
    let min = parse_slice(rest.get(deg_digits..deg_digits + 2)?);
    let csec = parse_slice(rest.get(deg_digits + 2..)?);
    let mag = deg + min / 60.0 + (csec / 100.0) / 3600.0;
    if hem == pos_hem {
        Some(mag)
    } else if hem == neg_hem {
        Some(-mag)
    } else {
        None
    }
}

fn point_in_bbox((lat_e6, lon_e6): (i32, i32), bbox: GeoBbox) -> bool {
    let lat = f64::from(lat_e6) / 1_000_000.0;
    let lon = f64::from(lon_e6) / 1_000_000.0;
    lon >= bbox.min_lon && lon <= bbox.max_lon && lat >= bbox.min_lat && lat <= bbox.max_lat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_arinc424_sample_and_encodes_fbcifp01_binary() {
        let mut lines: Vec<String> = Vec::new();
        let pad132 = |s: &str| -> String {
            let mut out = s.to_owned();
            while out.len() < 132 {
                out.push(' ');
            }
            out
        };
        lines.push(pad132(
            "HDR01FAACIFP18      001P013203973042611  07-OCT-202615:55:03  U.S.A. DOT FAA",
        ));
        lines.push(pad132(
            "SUSAP N51 K6AN51     0     055YHN40345803W074441214W010000190         1800018000C    MNAR    SOLBERG",
        ));
        lines.push(pad132(
            "SUSAP N51 K6GRW22    0055982190 N40350356W074435838         +0015900162131940050R",
        ));
        lines.push(pad132(
            "SUSAP N51 K6PCDODLY K60    W     N40410000W074380000                       W0100     NAR           DODLY",
        ));
        lines.push(pad132(
            "SUSAP N51 K6PCCAVZA K60    W     N40380000W074410000                       W0100     NAR           CAVZA",
        ));
        lines.push(pad132(
            "SUSAP N51 K6PCMOSSE K60    W     N40310000W074480000                       W0100     NAR           MOSSE",
        ));
        lines.push(pad132(
            "SUSAEAENRT   PIYIN K60    W     N40450000W074350000                       W0100     NAR           PIYIN",
        ));
        lines.push(pad132(
            "SUSAD        SBJ   K6011290VDHW N40345894W074443048    N40345894W074443048W0100001902     NARSOLBERG",
        ));
        lines.push(pad132(
            "SUSAD        MXE   K6011320VDHW N39550500W075401500    N39550500W075401500W0100004702     NARMODENA",
        ));
        lines.push(pad132(
            "SUSAER       V3          0100MXE  K6D 0V    O                         04500380     03000     17500",
        ));
        lines.push(pad132(
            "SUSAER       V3          0110SBJ  K6D 0V    O                                                                      ",
        ));
        lines.push(pad132(
            "SUSAP N51 K6FR22   APIYIN 010PIYINK6EA0E  A    IF                                             18000",
        ));
        lines.push(pad132(
            "SUSAP N51 K6FR22   APIYIN 020DODLYK6PC0EE B 010TF                                 + 03000",
        ));
        lines.push(pad132(
            "SUSAP N51 K6FR22   R      010DODLYK6PC0E  I    IF                                 + 03000     18000",
        ));
        lines.push(pad132(
            "SUSAP N51 K6FR22   R      020CAVZAK6PC1E  F 010TF                                 + 01800",
        ));
        lines.push(pad132(
            "SUSAP N51 K6FR22   R      020CAVZAK6PC2WN          N          ALNAV",
        ));
        lines.push(pad132(
            "SUSAP N51 K6FR22   R      030RW22 K6PG0GY M 031TF                                   00202",
        ));
        lines.push(pad132(
            "SUSAP N51 K6FR22   R      040         0  M     CA                     2196        + 00589",
        ));
        lines.push(pad132(
            "SUSAP N51 K6FR22   R      050MOSSEK6PC0EY   010DF                                 + 02600",
        ));

        let payload = lines.join("\n");
        let compiled = compile_cifp_bytes(payload.as_bytes(), "2611", None).expect("compile ok");
        assert_eq!(compiled.counts.airways, 1);
        assert_eq!(compiled.counts.approaches, 1);
        assert_eq!(compiled.procedures[0].airport_ident, "N51");
        assert_eq!(compiled.procedures[0].procedure_ident, "R22");

        let active_legs = compiled.procedures[0].assemble_legs(Some("PIYIN"), None, false);
        let fix_seq: Vec<&str> = active_legs.iter().map(|l| l.fix_ident.as_str()).collect();
        assert_eq!(fix_seq, vec!["PIYIN", "DODLY", "CAVZA", "RW22"]);

        let bin = encode_cifp_binary(&compiled).expect("encode ok");
        assert_eq!(&bin[0..8], b"FBCIFP01");
        assert_eq!(&bin[12..16], b"2611");
    }
}
