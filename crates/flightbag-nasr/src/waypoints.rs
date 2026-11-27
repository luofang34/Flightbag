//! Pure-Rust FAA 28-day NASR waypoint, navaid, and airport index compiler (`SIMWPT01`).
//!
//! Reads `APT_BASE.csv`, `NAV_BASE.csv`, and `FIX_BASE.csv` from the nested `CSV_Data/*_CSV.zip`
//! archive inside `28DaySubscription_Effective_YYYY-MM-DD.zip` and writes a sorted, memory-mapped
//! `SIMWPT01` binary index (`nasr_waypoints.bin`) alongside its `.sha256` sidecar and
//! `waypoints_faa_nasr_waypoints.json` manifest.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};

use flightbag_core::{AuthoritySource, EditionManifest, SourceDigest};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cycles::ResolvedCycle;
use crate::fetch::NasrArchiveFingerprint;

/// Binary magic header of `nasr_waypoints.bin`.
pub const SIMWPT01_MAGIC: [u8; 8] = *b"SIMWPT01";

/// Schema version of the `SIMWPT01` binary format.
pub const SIMWPT01_VERSION: u32 = 1;

const KIND_AIRPORT: u8 = 0;
const KIND_VORTAC: u8 = 1;
const KIND_VORDME: u8 = 2;
const KIND_VOR: u8 = 3;
const KIND_TACAN: u8 = 4;
const KIND_NDB: u8 = 5;
const KIND_FIX: u8 = 6;
const KIND_VFR: u8 = 7;

const DISCLAIMER: &str = "EXPERIMENTAL PROTOTYPE — NOT FOR NAVIGATION OR FLIGHT. \
Unverified research and visualization output; never use for real-world navigation, flight planning, \
or operational aviation decisions.";

/// Record counts compiled from `APT_BASE.csv`, `NAV_BASE.csv`, and `FIX_BASE.csv`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WaypointsCounts {
    /// Open public-use / civil airports (`APT_BASE.csv`).
    pub airports: usize,
    /// Operational VHF/TACAN/NDB radio navaids (`NAV_BASE.csv`).
    pub navaids: usize,
    /// Enroute IFR & VFR fixes (`FIX_BASE.csv`).
    pub fixes: usize,
    /// Total compiled navigation points (`airports + navaids + fixes`).
    pub total: usize,
}

/// One encoded navigation point entry before string-pool packing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaypointRecord {
    /// Uppercase ASCII alphanumeric identifier (`1..=7` chars, NUL-padded).
    pub ident: [u8; 8],
    /// Numeric point classification (`0=Airport..7=VFR`).
    pub kind: u8,
    /// Two-letter US state/territory code (NUL-padded).
    pub state: [u8; 2],
    /// WGS-84 latitude in micro-degrees (`1e-6` deg).
    pub lat_e6: i32,
    /// WGS-84 longitude in micro-degrees (`1e-6` deg).
    pub lon_e6: i32,
    /// Clean ASCII description stored in the deduplicated string pool.
    pub detail: Vec<u8>,
}

/// Complete compiled `SIMWPT01` navigation waypoint dataset for one 28-day AIRAC cycle.
#[derive(Clone, Debug)]
pub struct CompiledWaypoints {
    /// Sorted waypoint records (`(ident, kind, state)` ascending).
    pub records: Vec<WaypointRecord>,
    /// Geographic bounding box `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
    /// Category and total counts.
    pub counts: WaypointsCounts,
}

/// Artifact metadata for `nasr_waypoints.bin`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WaypointsBinaryArtifact {
    /// Relative filename (`"nasr_waypoints.bin"`).
    pub file: String,
    /// Exact byte size of `nasr_waypoints.bin`.
    pub size_bytes: u64,
    /// Lowercase 64-char SHA-256 hex digest of `nasr_waypoints.bin`.
    pub sha256_hex: String,
    /// Geographic bounds `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
}

/// Published JSON cycle manifest (`waypoints_faa_nasr_waypoints.json`) written alongside `nasr_waypoints.bin`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PublishedWaypointsManifest {
    /// Manifest schema version (`1`).
    pub schema_version: u32,
    /// Mandatory non-navigational disclaimer.
    pub disclaimer: String,
    /// Edition identifier (`"flightbag-us-waypoints-YYYY-MM-DD"`).
    pub edition_id: String,
    /// Authority slug (`"faa_nasr_waypoints"`).
    pub authority: String,
    /// Governing 4-digit AIRAC code (`"2612"`).
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
    /// Official FAA NASR `.zip` URL.
    pub upstream_url: String,
    /// Lowercase SHA-256 hex digest of `28DaySubscription_Effective_YYYY-MM-DD.zip`.
    pub upstream_sha256_hex: String,
    /// Compiled `nasr_waypoints.bin` metadata.
    pub waypoints_binary: WaypointsBinaryArtifact,
    /// Compiled waypoint counts.
    pub counts: WaypointsCounts,
}

/// Errors raised while reading NASR CSV tables or encoding `nasr_waypoints.bin`.
#[derive(Debug, Error)]
pub enum WaypointsCompileError {
    /// Filesystem I/O failure.
    #[error("NASR waypoints I/O error at {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// Missing or corrupt outer/inner ZIP container.
    #[error("invalid NASR waypoints zip ({context}): {reason}")]
    Zip {
        /// Container or entry context.
        context: String,
        /// Failure explanation.
        reason: String,
    },
    /// Required CSV header column missing or stream unreadable.
    #[error("invalid NASR CSV table {entry}: {reason}")]
    Csv {
        /// CSV filename.
        entry: String,
        /// Failure explanation.
        reason: String,
    },
    /// Binary table overflow or manifest validation error.
    #[error("NASR waypoints encoding error: {0}")]
    Encode(String),
}

/// Compiles `APT_BASE.csv`, `NAV_BASE.csv`, and `FIX_BASE.csv` from `nasr_zip_path`.
///
/// # Errors
///
/// Returns [`WaypointsCompileError`] if the archive or its nested CSV ZIP cannot be read or parsed.
pub fn compile_nasr_waypoints_zip(
    nasr_zip_path: &Path,
) -> Result<CompiledWaypoints, WaypointsCompileError> {
    let file = File::open(nasr_zip_path).map_err(|source| WaypointsCompileError::Io {
        path: nasr_zip_path.to_path_buf(),
        source,
    })?;
    let mut outer = zip::ZipArchive::new(file).map_err(|err| WaypointsCompileError::Zip {
        context: nasr_zip_path.display().to_string(),
        reason: err.to_string(),
    })?;

    let csv_zip_name = (0..outer.len())
        .filter_map(|i| outer.by_index(i).ok().map(|f| f.name().to_owned()))
        .find(|name| name.starts_with("CSV_Data/") && name.ends_with("_CSV.zip"))
        .ok_or_else(|| WaypointsCompileError::Zip {
            context: nasr_zip_path.display().to_string(),
            reason: "no CSV_Data/*_CSV.zip entry found".to_owned(),
        })?;

    let inner_bytes = {
        let mut entry = outer
            .by_name(&csv_zip_name)
            .map_err(|err| WaypointsCompileError::Zip {
                context: csv_zip_name.clone(),
                reason: err.to_string(),
            })?;
        let mut buf = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
        entry
            .read_to_end(&mut buf)
            .map_err(|err| WaypointsCompileError::Zip {
                context: csv_zip_name.clone(),
                reason: err.to_string(),
            })?;
        buf
    };

    let mut inner = zip::ZipArchive::new(Cursor::new(inner_bytes)).map_err(|err| {
        WaypointsCompileError::Zip {
            context: csv_zip_name,
            reason: err.to_string(),
        }
    })?;

    let mut records = Vec::new();
    let mut bounds = BoundsTracker::default();
    let mut counts = WaypointsCounts::default();

    {
        let apt_file = inner
            .by_name("APT_BASE.csv")
            .map_err(|err| WaypointsCompileError::Zip {
                context: "APT_BASE.csv".to_owned(),
                reason: err.to_string(),
            })?;
        parse_apt_csv(
            BufReader::new(apt_file),
            &mut records,
            &mut bounds,
            &mut counts,
        )?;
    }

    {
        let nav_file = inner
            .by_name("NAV_BASE.csv")
            .map_err(|err| WaypointsCompileError::Zip {
                context: "NAV_BASE.csv".to_owned(),
                reason: err.to_string(),
            })?;
        parse_nav_csv(
            BufReader::new(nav_file),
            &mut records,
            &mut bounds,
            &mut counts,
        )?;
    }

    {
        let fix_file = inner
            .by_name("FIX_BASE.csv")
            .map_err(|err| WaypointsCompileError::Zip {
                context: "FIX_BASE.csv".to_owned(),
                reason: err.to_string(),
            })?;
        parse_fix_csv(
            BufReader::new(fix_file),
            &mut records,
            &mut bounds,
            &mut counts,
        )?;
    }

    records.sort_by(|a, b| {
        a.ident
            .cmp(&b.ident)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.state.cmp(&b.state))
    });
    counts.total = records.len();

    Ok(CompiledWaypoints {
        records,
        bounds: bounds.finish(),
        counts,
    })
}

/// Encodes `compiled` into a deterministic `SIMWPT01` binary image (`nasr_waypoints.bin`).
///
/// # Errors
///
/// Returns [`WaypointsCompileError::Encode`] if counts or string pool offsets overflow `u32`.
pub fn encode_waypoints_binary(
    compiled: &CompiledWaypoints,
) -> Result<Vec<u8>, WaypointsCompileError> {
    let record_count = u32::try_from(compiled.records.len())
        .map_err(|_| WaypointsCompileError::Encode("record_count overflow".into()))?;

    let mut string_pool: Vec<u8> = Vec::new();
    let mut pool_offsets: BTreeMap<&[u8], (u32, u8)> = BTreeMap::new();
    let mut table: Vec<u8> = Vec::with_capacity(compiled.records.len().saturating_mul(24));

    for rec in &compiled.records {
        let (offset, length) = if rec.detail.is_empty() {
            (0_u32, 0_u8)
        } else if let Some(&cached) = pool_offsets.get(rec.detail.as_slice()) {
            cached
        } else {
            let offset = u32::try_from(string_pool.len())
                .map_err(|_| WaypointsCompileError::Encode("string_pool offset overflow".into()))?;
            let max_slice = rec.detail.len().min(255);
            let slice = rec.detail.get(..max_slice).unwrap_or(&[]);
            let length = u8::try_from(slice.len()).unwrap_or(255);
            string_pool.extend_from_slice(slice);
            pool_offsets.insert(rec.detail.as_slice(), (offset, length));
            (offset, length)
        };

        table.extend_from_slice(&rec.ident);
        table.push(rec.kind);
        table.push(length);
        table.extend_from_slice(&rec.state);
        table.extend_from_slice(&rec.lat_e6.to_le_bytes());
        table.extend_from_slice(&rec.lon_e6.to_le_bytes());
        table.extend_from_slice(&offset.to_le_bytes());
    }

    let mut out = Vec::with_capacity(
        16_usize
            .saturating_add(table.len())
            .saturating_add(string_pool.len()),
    );
    out.extend_from_slice(&SIMWPT01_MAGIC);
    out.extend_from_slice(&SIMWPT01_VERSION.to_le_bytes());
    out.extend_from_slice(&record_count.to_le_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&string_pool);
    Ok(out)
}

/// Writes `nasr_waypoints.bin`, `nasr_waypoints.bin.sha256`, and `waypoints_faa_nasr_waypoints.json`,
/// returning validated [`EditionManifest`] and [`PublishedWaypointsManifest`] records.
///
/// # Errors
///
/// Returns [`WaypointsCompileError`] if encoding, digest validation, or file writing fails.
pub fn write_waypoints_pack(
    compiled: &CompiledWaypoints,
    cycle: &ResolvedCycle,
    upstream: &NasrArchiveFingerprint,
    bin_path: &Path,
    manifest_path: &Path,
) -> Result<(EditionManifest, PublishedWaypointsManifest), WaypointsCompileError> {
    let bin_bytes = encode_waypoints_binary(compiled)?;
    let size_bytes = bin_bytes.len() as u64;
    let sha256_hex = format!("{:x}", Sha256::digest(&bin_bytes));

    if let Some(parent) = bin_path.parent() {
        fs::create_dir_all(parent).map_err(|source| WaypointsCompileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let part = bin_path.with_extension("bin.part");
    fs::write(&part, &bin_bytes).map_err(|source| WaypointsCompileError::Io {
        path: part.clone(),
        source,
    })?;
    fs::rename(&part, bin_path).map_err(|source| WaypointsCompileError::Io {
        path: bin_path.to_path_buf(),
        source,
    })?;

    let bin_filename = bin_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nasr_waypoints.bin".to_owned());
    let sidecar_path = bin_path.with_file_name(format!("{bin_filename}.sha256"));
    fs::write(&sidecar_path, format!("{sha256_hex}  {bin_filename}\n")).map_err(|source| {
        WaypointsCompileError::Io {
            path: sidecar_path,
            source,
        }
    })?;

    let nasr_digest = SourceDigest::new(
        AuthoritySource::FaaNasr,
        cycle.effective_date.clone(),
        &upstream.sha256_hex,
    )
    .map_err(|e| WaypointsCompileError::Encode(e.to_string()))?;

    let edition_id = format!("flightbag-us-waypoints-{}", cycle.effective_date);
    let core_manifest = EditionManifest {
        schema_version: 1,
        edition_id: edition_id.clone(),
        airac: cycle.airac,
        sources: vec![nasr_digest],
        signature_ed25519_hex: None,
    };

    let published = PublishedWaypointsManifest {
        schema_version: 1,
        disclaimer: DISCLAIMER.to_owned(),
        edition_id,
        authority: "faa_nasr_waypoints".to_owned(),
        airac_code: cycle.airac_code.clone(),
        cycle_index: cycle.cycle_index,
        effective_date: cycle.effective_date.clone(),
        expires_date: cycle.expires_date.clone(),
        effective_from_utc: cycle.effective_from_utc.clone(),
        effective_until_utc: cycle.effective_until_utc.clone(),
        upstream_url: upstream.url.clone(),
        upstream_sha256_hex: upstream.sha256_hex.clone(),
        waypoints_binary: WaypointsBinaryArtifact {
            file: bin_filename,
            size_bytes,
            sha256_hex,
            bounds: compiled.bounds,
        },
        counts: compiled.counts.clone(),
    };

    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent).map_err(|source| WaypointsCompileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut json_bytes = serde_json::to_vec_pretty(&published)
        .map_err(|e| WaypointsCompileError::Encode(e.to_string()))?;
    json_bytes.push(b'\n');
    fs::write(manifest_path, json_bytes).map_err(|source| WaypointsCompileError::Io {
        path: manifest_path.to_path_buf(),
        source,
    })?;

    Ok((core_manifest, published))
}

#[derive(Default)]
struct BoundsTracker {
    any: bool,
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
}

impl BoundsTracker {
    fn observe(&mut self, lat: f64, lon: f64) {
        if !self.any {
            self.min_lon = lon;
            self.max_lon = lon;
            self.min_lat = lat;
            self.max_lat = lat;
            self.any = true;
        } else {
            self.min_lon = self.min_lon.min(lon);
            self.max_lon = self.max_lon.max(lon);
            self.min_lat = self.min_lat.min(lat);
            self.max_lat = self.max_lat.max(lat);
        }
    }

    fn finish(self) -> [f64; 4] {
        if self.any {
            [self.min_lon, self.min_lat, self.max_lon, self.max_lat]
        } else {
            [-180.0, -14.58, 180.0, 82.0]
        }
    }
}

fn parse_apt_csv<R: BufRead>(
    mut reader: R,
    out: &mut Vec<WaypointRecord>,
    bounds: &mut BoundsTracker,
    counts: &mut WaypointsCounts,
) -> Result<(), WaypointsCompileError> {
    let mut csv = CsvStream::new(&mut reader, "APT_BASE.csv")?;
    let col_status = csv.require_col("ARPT_STATUS")?;
    let col_site_type = csv.require_col("SITE_TYPE_CODE")?;
    let col_ownership = csv.require_col("OWNERSHIP_TYPE_CODE")?;
    let col_arpt_id = csv.require_col("ARPT_ID")?;
    let col_icao_id = csv.require_col("ICAO_ID")?;
    let col_lat = csv.require_col("LAT_DECIMAL")?;
    let col_lon = csv.require_col("LONG_DECIMAL")?;
    let col_name = csv.require_col("ARPT_NAME")?;
    let col_city = csv.require_col("CITY")?;
    let col_state = csv.require_col("STATE_CODE")?;

    while csv.next_row(&mut reader)? {
        if !csv.col(col_status).trim().eq_ignore_ascii_case("O") {
            continue;
        }
        let site_type = csv.col(col_site_type).trim().to_ascii_uppercase();
        let ownership = csv.col(col_ownership).trim().to_ascii_uppercase();
        if site_type != "A" && site_type != "C" && ownership != "PU" {
            continue;
        }
        let arpt_id = csv.col(col_arpt_id).trim().to_ascii_uppercase();
        let icao_id = csv.col(col_icao_id).trim().to_ascii_uppercase();
        let primary_id = if icao_id.len() == 4 && icao_id.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            icao_id
        } else {
            arpt_id
        };
        let Some(ident) = encode_ident(&primary_id) else {
            continue;
        };
        let Some((lat, lon, lat_e6, lon_e6)) = parse_lat_lon(csv.col(col_lat), csv.col(col_lon))
        else {
            continue;
        };
        let name = title_case(csv.col(col_name));
        let city = title_case(csv.col(col_city));
        let detail_str = if !name.is_empty() && !city.is_empty() && !name.contains(&city) {
            format!("{name} - {city}")
        } else {
            name
        };
        bounds.observe(lat, lon);
        out.push(WaypointRecord {
            ident,
            kind: KIND_AIRPORT,
            state: encode_state(csv.col(col_state)),
            lat_e6,
            lon_e6,
            detail: ascii_clean(&detail_str, 56),
        });
        counts.airports = counts.airports.saturating_add(1);
    }
    Ok(())
}

fn parse_nav_csv<R: BufRead>(
    mut reader: R,
    out: &mut Vec<WaypointRecord>,
    bounds: &mut BoundsTracker,
    counts: &mut WaypointsCounts,
) -> Result<(), WaypointsCompileError> {
    let mut csv = CsvStream::new(&mut reader, "NAV_BASE.csv")?;
    let col_status = csv.require_col("NAV_STATUS")?;
    let col_id = csv.require_col("NAV_ID")?;
    let col_type = csv.require_col("NAV_TYPE")?;
    let col_lat = csv.require_col("LAT_DECIMAL")?;
    let col_lon = csv.require_col("LONG_DECIMAL")?;
    let col_name = csv.require_col("NAME")?;
    let col_freq = csv.require_col("FREQ")?;
    let col_chan = csv.require_col("CHAN")?;
    let col_state = csv.require_col("STATE_CODE")?;

    while csv.next_row(&mut reader)? {
        if !csv
            .col(col_status)
            .to_ascii_uppercase()
            .contains("OPERATIONAL")
        {
            continue;
        }
        let Some(ident) = encode_ident(&csv.col(col_id).trim().to_ascii_uppercase()) else {
            continue;
        };
        let Some(kind) = classify_navaid(csv.col(col_type)) else {
            continue;
        };
        let Some((lat, lon, lat_e6, lon_e6)) = parse_lat_lon(csv.col(col_lat), csv.col(col_lon))
        else {
            continue;
        };
        let name = title_case(csv.col(col_name));
        let freq = csv.col(col_freq).trim();
        let chan = csv.col(col_chan).trim();
        let detail_str = if !freq.is_empty() {
            let unit = if kind == KIND_NDB { "kHz" } else { "MHz" };
            if name.is_empty() {
                format!("{freq} {unit}")
            } else {
                format!("{name} ({freq} {unit})")
            }
        } else if !chan.is_empty() {
            if name.is_empty() {
                format!("CH {chan}")
            } else {
                format!("{name} (CH {chan})")
            }
        } else {
            name
        };
        bounds.observe(lat, lon);
        out.push(WaypointRecord {
            ident,
            kind,
            state: encode_state(csv.col(col_state)),
            lat_e6,
            lon_e6,
            detail: ascii_clean(&detail_str, 64),
        });
        counts.navaids = counts.navaids.saturating_add(1);
    }
    Ok(())
}

fn parse_fix_csv<R: BufRead>(
    mut reader: R,
    out: &mut Vec<WaypointRecord>,
    bounds: &mut BoundsTracker,
    counts: &mut WaypointsCounts,
) -> Result<(), WaypointsCompileError> {
    let mut csv = CsvStream::new(&mut reader, "FIX_BASE.csv")?;
    let col_id = csv.require_col("FIX_ID")?;
    let col_lat = csv.require_col("LAT_DECIMAL")?;
    let col_lon = csv.require_col("LONG_DECIMAL")?;
    let col_use = csv.require_col("FIX_USE_CODE")?;
    let col_artcc_low = csv.require_col("ARTCC_ID_LOW")?;
    let col_artcc_high = csv.require_col("ARTCC_ID_HIGH")?;
    let col_state = csv.require_col("STATE_CODE")?;

    while csv.next_row(&mut reader)? {
        let ident_upper = csv.col(col_id).trim().to_ascii_uppercase();
        let Some(ident) = encode_ident(&ident_upper) else {
            continue;
        };
        let Some((lat, lon, lat_e6, lon_e6)) = parse_lat_lon(csv.col(col_lat), csv.col(col_lon))
        else {
            continue;
        };
        let kind = if csv.col(col_use).trim().to_ascii_uppercase().contains("VFR")
            || ident_upper.starts_with("VP")
        {
            KIND_VFR
        } else {
            KIND_FIX
        };
        let low = csv.col(col_artcc_low).trim();
        let high = csv.col(col_artcc_high).trim();
        let artcc = if !low.is_empty() { low } else { high }.to_ascii_uppercase();
        let detail_str = if artcc.is_empty() {
            "Published Fix".to_owned()
        } else {
            format!("{artcc} Center")
        };
        bounds.observe(lat, lon);
        out.push(WaypointRecord {
            ident,
            kind,
            state: encode_state(csv.col(col_state)),
            lat_e6,
            lon_e6,
            detail: ascii_clean(&detail_str, 40),
        });
        counts.fixes = counts.fixes.saturating_add(1);
    }
    Ok(())
}

struct CsvStream {
    entry_name: String,
    headers: BTreeMap<String, usize>,
    row: Vec<String>,
    buf: Vec<u8>,
}

impl CsvStream {
    fn new<R: BufRead>(reader: &mut R, entry_name: &str) -> Result<Self, WaypointsCompileError> {
        let mut stream = Self {
            entry_name: entry_name.to_owned(),
            headers: BTreeMap::new(),
            row: Vec::new(),
            buf: Vec::with_capacity(4096),
        };
        if !stream.next_row(reader)? {
            return Err(WaypointsCompileError::Csv {
                entry: stream.entry_name,
                reason: "empty CSV header".to_owned(),
            });
        }
        let header_row = std::mem::take(&mut stream.row);
        for (idx, name) in header_row.into_iter().enumerate() {
            let clean = name.trim().trim_start_matches('\u{feff}').to_owned();
            stream.headers.insert(clean, idx);
        }
        Ok(stream)
    }

    fn require_col(&self, name: &str) -> Result<usize, WaypointsCompileError> {
        self.headers
            .get(name)
            .copied()
            .ok_or_else(|| WaypointsCompileError::Csv {
                entry: self.entry_name.clone(),
                reason: format!("missing column '{name}'"),
            })
    }

    fn col(&self, idx: usize) -> &str {
        self.row.get(idx).map(String::as_str).unwrap_or("")
    }

    fn next_row<R: BufRead>(&mut self, reader: &mut R) -> Result<bool, WaypointsCompileError> {
        self.row.clear();
        self.buf.clear();
        let mut in_quotes = false;
        let mut started = false;

        loop {
            let start_pos = self.buf.len();
            let read = reader.read_until(b'\n', &mut self.buf).map_err(|err| {
                WaypointsCompileError::Csv {
                    entry: self.entry_name.clone(),
                    reason: err.to_string(),
                }
            })?;
            if read == 0 {
                if !started {
                    return Ok(false);
                }
                break;
            }
            started = true;
            if let Some(chunk) = self.buf.get(start_pos..) {
                for &b in chunk {
                    if b == b'"' {
                        in_quotes = !in_quotes;
                    }
                }
            }
            if !in_quotes {
                break;
            }
        }

        if self.buf.last() == Some(&b'\n') {
            let _ = self.buf.pop();
        }
        if self.buf.last() == Some(&b'\r') {
            let _ = self.buf.pop();
        }

        parse_csv_record_bytes(&self.buf, &mut self.row);
        Ok(true)
    }
}

fn parse_csv_record_bytes(line: &[u8], fields: &mut Vec<String>) {
    fields.clear();
    let mut cur = String::new();
    let mut i = 0_usize;
    let mut in_quotes = false;
    let mut field_start = true;

    while let Some(&b) = line.get(i) {
        if in_quotes {
            if b == b'"' {
                if line.get(i + 1) == Some(&b'"') {
                    cur.push('"');
                    i += 2;
                } else {
                    in_quotes = false;
                    i += 1;
                }
            } else {
                cur.push(char::from(b));
                i += 1;
            }
        } else if field_start && b == b'"' {
            in_quotes = true;
            field_start = false;
            i += 1;
        } else if b == b',' {
            fields.push(std::mem::take(&mut cur));
            field_start = true;
            i += 1;
        } else {
            cur.push(char::from(b));
            field_start = false;
            i += 1;
        }
    }
    fields.push(cur);
}

fn classify_navaid(nav_type: &str) -> Option<u8> {
    let upper = nav_type.trim().to_ascii_uppercase();
    if upper.contains("VORTAC") {
        Some(KIND_VORTAC)
    } else if upper.contains("VOR/DME") || upper.contains("VORDME") {
        Some(KIND_VORDME)
    } else if upper == "VOR" {
        Some(KIND_VOR)
    } else if upper.contains("TACAN") || upper == "DME" {
        Some(KIND_TACAN)
    } else if upper.contains("NDB") {
        Some(KIND_NDB)
    } else {
        None
    }
}

fn encode_ident(upper: &str) -> Option<[u8; 8]> {
    let bytes = upper.as_bytes();
    if bytes.is_empty() || bytes.len() > 7 || !bytes.iter().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let mut out = [0_u8; 8];
    for (dst, src) in out.iter_mut().zip(bytes.iter().copied()) {
        *dst = src;
    }
    Some(out)
}

fn encode_state(raw: &str) -> [u8; 2] {
    let clean = ascii_clean(&raw.to_ascii_uppercase(), 2);
    let mut out = [0_u8; 2];
    for (dst, src) in out.iter_mut().zip(clean.iter().copied()) {
        *dst = src;
    }
    out
}

fn parse_lat_lon(lat_s: &str, lon_s: &str) -> Option<(f64, f64, i32, i32)> {
    let lat = lat_s.trim().parse::<f64>().ok()?;
    let lon = lon_s.trim().parse::<f64>().ok()?;
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
        return None;
    }
    let lat_e6 = (lat * 1_000_000.0).round() as i32;
    let lon_e6 = (lon * 1_000_000.0).round() as i32;
    Some((lat, lon, lat_e6, lon_e6))
}

fn ascii_clean(raw: &str, max_len: usize) -> Vec<u8> {
    raw.trim()
        .chars()
        .take(max_len)
        .map(|ch| {
            let code = ch as u32;
            if (32..=126).contains(&code) {
                ch as u8
            } else {
                b' '
            }
        })
        .collect()
}

fn title_case(raw: &str) -> String {
    let mut out = String::with_capacity(raw.trim().len());
    let mut prev_cased = false;
    for ch in raw.trim().chars() {
        if ch.is_alphabetic() {
            if prev_cased {
                for lc in ch.to_lowercase() {
                    out.push(lc);
                }
            } else {
                for uc in ch.to_uppercase() {
                    out.push(uc);
                }
            }
            prev_cased = true;
        } else {
            out.push(ch);
            prev_cased = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::cycles::{CycleSpec, resolve_cycle};
    use time::macros::datetime;
    use zip::write::SimpleFileOptions;

    use super::*;

    #[test]
    fn parses_quoted_csv_fields_and_title_cases() {
        let mut fields = Vec::new();
        parse_csv_record_bytes(
            br#"KTEB,"TETERBORO, ""MAIN""","NJ",40.85,-74.06"#,
            &mut fields,
        );
        assert_eq!(
            fields,
            vec!["KTEB", "TETERBORO, \"MAIN\"", "NJ", "40.85", "-74.06"]
        );
        assert_eq!(title_case("NEWARK LIBERTY INTL"), "Newark Liberty Intl");
        assert_eq!(title_case("O'HARE"), "O'Hare");
    }

    #[test]
    fn compiles_synthetic_nasr_zip_and_encodes_sorted_simwpt01_binary() {
        let dir =
            std::env::temp_dir().join(format!("flightbag_waypoints_test_{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).unwrap();

        let apt_csv = "\
ARPT_STATUS,SITE_TYPE_CODE,OWNERSHIP_TYPE_CODE,ARPT_ID,ICAO_ID,LAT_DECIMAL,LONG_DECIMAL,ARPT_NAME,CITY,STATE_CODE
O,A,PU,EWR,KEWR,40.69248,-74.16868,NEWARK LIBERTY INTL,NEWARK,NJ
C,A,PU,OLD,KOLD,40.0,-74.0,CLOSED APT,NOWHERE,NJ
O,A,PU,N51,,40.62597,-74.73631,SOLBERG-HUNTERDON,READINGTON,NJ
";
        let nav_csv = "\
NAV_STATUS,NAV_ID,NAV_TYPE,LAT_DECIMAL,LONG_DECIMAL,NAME,FREQ,CHAN,STATE_CODE
OPERATIONAL,SBJ,VORTAC,40.58305,-74.74180,SOLBERG,112.9,,NJ
DECOMMISSIONED,OLD,VOR,40.0,-74.0,OLD VOR,110.0,,NJ
";
        let fix_csv = "\
FIX_ID,LAT_DECIMAL,LONG_DECIMAL,FIX_USE_CODE,ARTCC_ID_LOW,ARTCC_ID_HIGH,STATE_CODE
SWEET,40.6,-74.5,CN,,ZNY,NJ
VPMAN,40.7,-74.0,VFR,ZNY,,NY
";

        let mut inner_buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut inner_buf);
            let opts = SimpleFileOptions::default();
            zw.start_file("APT_BASE.csv", opts).unwrap();
            zw.write_all(apt_csv.as_bytes()).unwrap();
            zw.start_file("NAV_BASE.csv", opts).unwrap();
            zw.write_all(nav_csv.as_bytes()).unwrap();
            zw.start_file("FIX_BASE.csv", opts).unwrap();
            zw.write_all(fix_csv.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let outer_zip_path = dir.join("28DaySubscription_Effective_2026-11-26.zip");
        {
            let mut zw = zip::ZipWriter::new(File::create(&outer_zip_path).unwrap());
            let opts = SimpleFileOptions::default();
            zw.start_file("CSV_Data/26_Nov_2026_CSV.zip", opts).unwrap();
            zw.write_all(&inner_buf.into_inner()).unwrap();
            zw.finish().unwrap();
        }

        let compiled = compile_nasr_waypoints_zip(&outer_zip_path).unwrap();
        assert_eq!(compiled.counts.airports, 2);
        assert_eq!(compiled.counts.navaids, 1);
        assert_eq!(compiled.counts.fixes, 2);
        assert_eq!(compiled.counts.total, 5);

        // Verify sorted order by (ident, kind, state).
        let idents: Vec<String> = compiled
            .records
            .iter()
            .map(|r| {
                let end = r.ident.iter().position(|&b| b == 0).unwrap_or(8);
                String::from_utf8_lossy(&r.ident[..end]).into_owned()
            })
            .collect();
        assert_eq!(idents, vec!["KEWR", "N51", "SBJ", "SWEET", "VPMAN"]);

        let cycle = resolve_cycle(
            CycleSpec::EffectiveDate(crate::cycles::parse_iso_date("2026-11-26").unwrap()),
            datetime!(2026-11-26 12:00 UTC),
        )
        .unwrap();
        let upstream = NasrArchiveFingerprint {
            cycle: cycle.effective_date.clone(),
            airac_code: cycle.airac_code.clone(),
            filename: "28DaySubscription_Effective_2026-11-26.zip".to_owned(),
            url: cycle.nasr_url.clone(),
            sha256_hex: "a".repeat(64),
            size_bytes: 123,
            cached: true,
            path: outer_zip_path.clone(),
        };

        let bin_path = dir.join("nasr_waypoints.bin");
        let manifest_path = dir.join("waypoints_faa_nasr_waypoints.json");
        let (_core, published) =
            write_waypoints_pack(&compiled, &cycle, &upstream, &bin_path, &manifest_path).unwrap();

        let bytes = fs::read(&bin_path).unwrap();
        assert_eq!(&bytes[0..8], b"SIMWPT01");
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 5);
        assert_eq!(published.authority, "faa_nasr_waypoints");
        assert_eq!(published.waypoints_binary.size_bytes, bytes.len() as u64);
        assert!(dir.join("nasr_waypoints.bin.sha256").is_file());
        assert!(manifest_path.is_file());

        drop(fs::remove_dir_all(&dir));
    }
}
