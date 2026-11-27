//! Pure-Rust FAA 28-day `d-TPP` terminal procedure plates fetcher and `FBDTPP01` pack compiler.
//!
//! Fetches `https://aeronav.faa.gov/d-tpp/<AIRAC>/xml_data/d-TPP_Metafile.xml`, selects terminal
//! charts (`APD`, `IAP`, `DP`, `ODP`, `SID`, `STAR`) for a target airport roster, caches individual
//! GeoPDF plates under `cache/<effective_date>/dtpp_pdfs/`, and packs them into an atomic,
//! SHA-256-verified `FBDTPP01` mmap container (`dtpp_plates.bin`) plus `dtpp_faa_dtpp.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use flightbag_core::{AuthoritySource, EditionManifest, SourceDigest};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cycles::ResolvedCycle;
use crate::fetch::{FetchError, NasrArchiveFingerprint, Transport, UreqTransport};

/// Official FAA 28-day `d-TPP` publication base directory.
pub const DTPP_BASE_URL: &str = "https://aeronav.faa.gov/d-tpp";

/// Binary magic header of `dtpp_plates.bin`.
pub const FBDTPP01_MAGIC: [u8; 8] = *b"FBDTPP01";

/// Default CONUS bounds advertised for `dtpp_plates.bin`.
pub const DEFAULT_DTPP_BOUNDS: [f64; 4] = [-179.9, 17.5, -64.0, 71.6];

/// Default curated airport roster keeping `dtpp_plates.bin` well below the 90 MB static-site limit.
pub const DEFAULT_DTPP_AIRPORTS: &[&str] = &[
    "KATL", "KBOS", "KCDW", "KEWR", "KHSV", "KJFK", "KLGA", "KMEM", "KMMU", "KPHL", "KTEB", "KTTN",
    "N51",
];

const DISCLAIMER: &str = "EXPERIMENTAL PROTOTYPE — NOT FOR NAVIGATION OR FLIGHT. \
Unverified research and visualization output; never use for real-world navigation, flight planning, \
or operational aviation decisions.";

/// Summary counts of a compiled `FBDTPP01` plate pack.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DtppCounts {
    /// Distinct airports represented in the pack.
    pub airports: usize,
    /// Total catalog rows (`APD`, `IAP`, `DP`, `STAR`).
    pub plate_records: usize,
    /// Distinct PDF blobs packed in `dtpp_plates.bin`.
    pub unique_pdfs: usize,
}

/// One JSON catalog row stored inside `dtpp_plates.bin` (`FBDTPP01`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DtppCatalogEntry {
    /// Preferred uppercase identifier (`ICAO` when present, else `FAA`).
    pub airport_ident: String,
    /// FAA 3/4-character identifier (`apt_ident`).
    pub faa_ident: String,
    /// Four-character ICAO identifier (`icao_ident`, empty when none).
    pub icao_ident: String,
    /// FAA `d-TPP` chart code (`"APD"`, `"IAP"`, `"DP"`, `"ODP"`, `"SID"`, `"STAR"`).
    pub chart_code: String,
    /// Human-readable procedure/chart title (`chart_name`).
    pub chart_name: String,
    /// Canonical uppercase PDF filename (`pdf_name`, e.g. `"05075R22.PDF"`).
    pub pdf_name: String,
    /// Linked ARINC 424-18 identifier from `<faanfd18>` (may be empty).
    pub cifp_ident: String,
    /// Byte offset relative to the start of the concatenated PDF blob section.
    pub rel_offset: u64,
    /// Exact byte length of the PDF payload.
    pub length: u64,
    /// Lowercase 64-character SHA-256 hex digest of the PDF payload.
    pub sha256_hex: String,
}

/// Complete compiled `FBDTPP01` terminal plate pack for one 28-day AIRAC cycle.
#[derive(Clone, Debug)]
pub struct CompiledDtpp {
    /// Verified 4-digit AIRAC code from `<digital_tpp cycle="...">`.
    pub airac_code: String,
    /// Ordered catalog entries with resolved blob offsets and PDF digests.
    pub catalog: Vec<DtppCatalogEntry>,
    /// Deduplicated raw PDF payloads ordered to match `catalog` offsets.
    pub pdf_blobs: Vec<Vec<u8>>,
    /// Geographic bounding box `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
    /// Airport, record, and unique-PDF counts.
    pub counts: DtppCounts,
}

/// Artifact metadata for `dtpp_plates.bin`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DtppBinaryArtifact {
    /// Relative filename (`"dtpp_plates.bin"`).
    pub file: String,
    /// Exact byte size of `dtpp_plates.bin`.
    pub size_bytes: u64,
    /// Lowercase 64-char SHA-256 hex digest of `dtpp_plates.bin`.
    pub sha256_hex: String,
    /// Geographic bounds `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
}

/// Published JSON cycle manifest (`dtpp_faa_dtpp.json`) written alongside `dtpp_plates.bin`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PublishedDtppManifest {
    /// Manifest schema version (`1`).
    pub schema_version: u32,
    /// Mandatory non-navigational disclaimer.
    pub disclaimer: String,
    /// Edition identifier (`"flightbag-us-dtpp-YYYY-MM-DD"`).
    pub edition_id: String,
    /// Authority slug (`"faa_dtpp"`).
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
    /// Official FAA `d-TPP_Metafile.xml` URL.
    pub upstream_url: String,
    /// Lowercase SHA-256 hex digest of `d-TPP_Metafile.xml`.
    pub upstream_sha256_hex: String,
    /// Compiled `dtpp_plates.bin` metadata.
    pub dtpp_binary: DtppBinaryArtifact,
    /// Compiled airport, plate, and unique PDF counts.
    pub counts: DtppCounts,
}

/// Parsed plate metadata record extracted from `d-TPP_Metafile.xml` before PDF download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedDtppRecord {
    /// Preferred uppercase airport identifier (`ICAO` when present, else `FAA`).
    pub airport_ident: String,
    /// FAA `apt_ident`.
    pub faa_ident: String,
    /// ICAO `icao_ident`.
    pub icao_ident: String,
    /// Uppercase chart code (`APD`, `IAP`, `DP`, `ODP`, `SID`, `STAR`).
    pub chart_code: String,
    /// Human-readable chart title.
    pub chart_name: String,
    /// Uppercase PDF filename (`pdf_name`).
    pub pdf_name: String,
    /// `faanfd18` procedure/transition identifier (may be empty).
    pub cifp_ident: String,
}

/// Errors raised while fetching, parsing, or packing FAA `d-TPP` procedure plates.
#[derive(Debug, Error)]
pub enum DtppCompileError {
    /// Filesystem I/O failure.
    #[error("d-TPP filesystem error at {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// Upstream HTTP fetch failure.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// Malformed `d-TPP_Metafile.xml`.
    #[error("invalid d-TPP_Metafile.xml: {0}")]
    Xml(String),
    /// `<digital_tpp cycle="...">` mismatch against expected AIRAC code.
    #[error("d-TPP metafile cycle mismatch: expected AIRAC '{expected}', found '{found}'")]
    CycleMismatch {
        /// Expected 4-digit AIRAC code.
        expected: String,
        /// Actual XML `cycle` attribute.
        found: String,
    },
    /// Downloaded file is not a valid `%PDF-` document.
    #[error("invalid PDF payload for {pdf_name}: missing %PDF- header")]
    InvalidPdf {
        /// Offending PDF filename.
        pdf_name: String,
    },
    /// Binary serialization or manifest error.
    #[error("d-TPP encoding error: {0}")]
    Encode(String),
}

/// Formats the official FAA `d-TPP_Metafile.xml` URL for `cycle`.
pub fn dtpp_metafile_url(cycle: &ResolvedCycle) -> String {
    format!(
        "{DTPP_BASE_URL}/{}/xml_data/d-TPP_Metafile.xml",
        cycle.airac_code
    )
}

/// Formats the official FAA GeoPDF plate URL for `pdf_name` in `cycle`.
pub fn dtpp_pdf_url(cycle: &ResolvedCycle, pdf_name: &str) -> String {
    format!("{DTPP_BASE_URL}/{}/{}", cycle.airac_code, pdf_name)
}

/// Hashes an existing local `d-TPP_Metafile.xml` without network access.
///
/// # Errors
///
/// Returns [`FetchError`] if `xml_path` cannot be read or is empty.
pub fn fingerprint_local_dtpp_metafile(
    cycle: &ResolvedCycle,
    xml_path: &Path,
) -> Result<NasrArchiveFingerprint, FetchError> {
    let bytes = fs::read(xml_path).map_err(|source| FetchError::Io {
        path: xml_path.to_path_buf(),
        source,
    })?;
    if bytes.is_empty() {
        return Err(FetchError::EmptyBody {
            url: xml_path.display().to_string(),
        });
    }
    let sha256_hex = format!("{:x}", Sha256::digest(&bytes));
    let filename = xml_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "d-TPP_Metafile.xml".to_owned());
    Ok(NasrArchiveFingerprint {
        cycle: cycle.effective_date.clone(),
        airac_code: cycle.airac_code.clone(),
        filename,
        url: dtpp_metafile_url(cycle),
        sha256_hex,
        size_bytes: bytes.len() as u64,
        cached: true,
        path: xml_path.to_path_buf(),
    })
}

/// Downloads `cycle`'s `d-TPP_Metafile.xml` into `cache_root/<YYYY-MM-DD>/`, reusing SHA-256-verified cached bytes.
///
/// # Errors
///
/// Returns [`FetchError`] on network, HTTP status, I/O, or checksum verification failure.
pub fn fetch_dtpp_metafile(
    transport: &dyn Transport,
    cycle: &ResolvedCycle,
    cache_root: &Path,
) -> Result<NasrArchiveFingerprint, FetchError> {
    let filename = "d-TPP_Metafile.xml";
    let url = dtpp_metafile_url(cycle);
    let cycle_dir = cache_root.join(&cycle.effective_date);
    let dest = cycle_dir.join(filename);
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
                filename: filename.to_owned(),
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
    let part = dest.with_file_name(format!("{filename}.part"));
    fs::write(&part, &body).map_err(|source| FetchError::Io {
        path: part.clone(),
        source,
    })?;
    fs::rename(&part, &dest).map_err(|source| FetchError::Io {
        path: dest.clone(),
        source,
    })?;
    fs::write(&sidecar, format!("{sha256_hex}  {filename}\n")).map_err(|source| {
        FetchError::Io {
            path: sidecar,
            source,
        }
    })?;

    Ok(NasrArchiveFingerprint {
        cycle: cycle.effective_date.clone(),
        airac_code: cycle.airac_code.clone(),
        filename: filename.to_owned(),
        url,
        sha256_hex,
        size_bytes: body.len() as u64,
        cached: false,
        path: dest,
    })
}

/// Parses `d-TPP_Metafile.xml` at `xml_path`, filtering to `airports` (or all airports when empty)
/// and target chart codes (`APD`, `IAP`, `DP`, `ODP`, `SID`, `STAR`).
///
/// # Errors
///
/// Returns [`DtppCompileError`] on XML errors or cycle attribute mismatch.
pub fn parse_dtpp_metafile(
    xml_path: &Path,
    expected_airac_code: &str,
    airports: &[&str],
) -> Result<Vec<ParsedDtppRecord>, DtppCompileError> {
    let file = File::open(xml_path).map_err(|source| DtppCompileError::Io {
        path: xml_path.to_path_buf(),
        source,
    })?;
    let roster: BTreeSet<String> = airports
        .iter()
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty())
        .collect();

    let mut reader = Reader::from_reader(BufReader::new(file));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::with_capacity(4096);

    let mut saw_root = false;
    let mut cur_airport: Option<(String, String, String)> = None;
    let mut cur_record: Option<RecordScratch> = None;
    let mut cur_field: Option<String> = None;
    let mut parsed = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let tag = local_name_bytes(e.name().as_ref());
                match tag.as_str() {
                    "digital_tpp" => {
                        let cycle_val = attr_value(&e, "cycle").unwrap_or_default();
                        if cycle_val != expected_airac_code {
                            return Err(DtppCompileError::CycleMismatch {
                                expected: expected_airac_code.to_owned(),
                                found: cycle_val,
                            });
                        }
                        saw_root = true;
                    }
                    "airport_name" => {
                        let faa = attr_value(&e, "apt_ident")
                            .unwrap_or_default()
                            .trim()
                            .to_ascii_uppercase();
                        let icao = attr_value(&e, "icao_ident")
                            .unwrap_or_default()
                            .trim()
                            .to_ascii_uppercase();
                        let preferred = if icao.is_empty() {
                            faa.clone()
                        } else {
                            icao.clone()
                        };
                        let in_roster = roster.is_empty()
                            || roster.contains(&preferred)
                            || (!icao.is_empty() && roster.contains(&icao))
                            || (!faa.is_empty() && roster.contains(&faa));
                        cur_airport = in_roster.then_some((preferred, faa, icao));
                    }
                    "record" if cur_airport.is_some() => {
                        cur_record = Some(RecordScratch::default());
                    }
                    _ if cur_record.is_some() => {
                        cur_field = Some(tag);
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) if cur_record.is_some() => {
                if let (Some(rec), Some(field)) = (cur_record.as_mut(), cur_field.as_deref()) {
                    let text = String::from_utf8_lossy(t.as_ref());
                    rec.push_text(field, &text);
                }
            }
            Ok(Event::GeneralRef(g)) if cur_record.is_some() => {
                if let (Some(rec), Some(field)) = (cur_record.as_mut(), cur_field.as_deref()) {
                    let decoded = match g.as_ref() {
                        b"amp" => "&",
                        b"lt" => "<",
                        b"gt" => ">",
                        b"quot" => "\"",
                        b"apos" => "'",
                        _ => "",
                    };
                    rec.push_text(field, decoded);
                }
            }
            Ok(Event::End(e)) => {
                let tag = local_name_bytes(e.name().as_ref());
                match tag.as_str() {
                    "airport_name" => {
                        cur_airport = None;
                        cur_record = None;
                        cur_field = None;
                    }
                    "record" => {
                        if let (Some((apt, faa, icao)), Some(rec)) =
                            (&cur_airport, cur_record.take())
                            && let Some(item) = rec.into_record(apt, faa, icao)
                        {
                            parsed.push(item);
                        }
                        cur_field = None;
                    }
                    _ => {
                        if cur_field.as_deref() == Some(tag.as_str()) {
                            cur_field = None;
                        }
                    }
                }
            }
            Err(err) => {
                return Err(DtppCompileError::Xml(err.to_string()));
            }
            _ => {}
        }
        buf.clear();
    }

    if !saw_root {
        return Err(DtppCompileError::Xml(
            "missing <digital_tpp cycle=\"...\"> root element".to_owned(),
        ));
    }

    Ok(parsed)
}

/// Applies deterministic per-airport chart caps (`APD`: all, `IAP`: 6, `DP`: 2, `STAR`: 2)
/// so the packed `dtpp_plates.bin` stays responsive and well below the static-site file cap.
pub fn select_curated_plates(records: Vec<ParsedDtppRecord>) -> Vec<ParsedDtppRecord> {
    let mut by_airport: BTreeMap<String, Vec<ParsedDtppRecord>> = BTreeMap::new();
    for rec in records {
        by_airport
            .entry(rec.airport_ident.clone())
            .or_default()
            .push(rec);
    }

    let mut selected = Vec::new();
    for (_airport, group) in by_airport {
        let mut apds = Vec::new();
        let mut iaps = Vec::new();
        let mut dps = Vec::new();
        let mut stars = Vec::new();

        for rec in group {
            match rec.chart_code.as_str() {
                "APD" => apds.push(rec),
                "IAP" => iaps.push(rec),
                "DP" | "ODP" | "SID" => dps.push(rec),
                "STAR" => stars.push(rec),
                _ => {}
            }
        }

        apds.sort_by(|a, b| {
            a.chart_name
                .cmp(&b.chart_name)
                .then_with(|| a.pdf_name.cmp(&b.pdf_name))
        });
        iaps.sort_by(|a, b| {
            iap_priority(&a.chart_name)
                .cmp(&iap_priority(&b.chart_name))
                .then_with(|| a.chart_name.cmp(&b.chart_name))
                .then_with(|| a.pdf_name.cmp(&b.pdf_name))
        });
        dps.sort_by(|a, b| {
            a.cifp_ident
                .is_empty()
                .cmp(&b.cifp_ident.is_empty())
                .then_with(|| a.chart_name.cmp(&b.chart_name))
                .then_with(|| a.pdf_name.cmp(&b.pdf_name))
        });
        stars.sort_by(|a, b| {
            a.cifp_ident
                .is_empty()
                .cmp(&b.cifp_ident.is_empty())
                .then_with(|| a.chart_name.cmp(&b.chart_name))
                .then_with(|| a.pdf_name.cmp(&b.pdf_name))
        });

        selected.extend(apds);
        selected.extend(iaps.into_iter().take(6));
        selected.extend(dps.into_iter().take(2));
        selected.extend(stars.into_iter().take(2));
    }

    selected
}

/// Compiles `dtpp_plates.bin` inputs from `records` by fetching/caching PDFs via HTTP.
///
/// # Errors
///
/// Returns [`DtppCompileError`] if any selected PDF cannot be downloaded or fails `%PDF-` check.
pub fn compile_dtpp_records(
    cycle: &ResolvedCycle,
    records: Vec<ParsedDtppRecord>,
    cache_root: &Path,
    threads: usize,
    custom_transport: Option<&dyn Transport>,
) -> Result<CompiledDtpp, DtppCompileError> {
    let mut unique_names: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for rec in &records {
        if seen.insert(rec.pdf_name.clone()) {
            unique_names.push(rec.pdf_name.clone());
        }
    }

    let pdf_cache_dir = cache_root.join(&cycle.effective_date).join("dtpp_pdfs");
    fs::create_dir_all(&pdf_cache_dir).map_err(|source| DtppCompileError::Io {
        path: pdf_cache_dir.clone(),
        source,
    })?;

    let pdf_map = fetch_all_pdfs(
        cycle,
        &unique_names,
        &pdf_cache_dir,
        threads,
        custom_transport,
    )?;

    let mut pdf_blobs = Vec::with_capacity(unique_names.len());
    let mut meta_by_name: BTreeMap<String, (u64, u64, String)> = BTreeMap::new();
    let mut next_offset: u64 = 0;

    for name in &unique_names {
        let bytes = pdf_map
            .get(name)
            .ok_or_else(|| DtppCompileError::Encode(format!("missing PDF buffer for {name}")))?;
        let length = bytes.len() as u64;
        let sha256_hex = format!("{:x}", Sha256::digest(bytes));
        meta_by_name.insert(name.clone(), (next_offset, length, sha256_hex));
        next_offset = next_offset.saturating_add(length);
        pdf_blobs.push(bytes.clone());
    }

    let mut catalog = Vec::with_capacity(records.len());
    let mut airports = BTreeSet::new();
    for rec in records {
        let (rel_offset, length, sha256_hex) =
            meta_by_name.get(&rec.pdf_name).cloned().ok_or_else(|| {
                DtppCompileError::Encode(format!("missing blob offset for {}", rec.pdf_name))
            })?;
        airports.insert(rec.airport_ident.clone());
        catalog.push(DtppCatalogEntry {
            airport_ident: rec.airport_ident,
            faa_ident: rec.faa_ident,
            icao_ident: rec.icao_ident,
            chart_code: rec.chart_code,
            chart_name: rec.chart_name,
            pdf_name: rec.pdf_name,
            cifp_ident: rec.cifp_ident,
            rel_offset,
            length,
            sha256_hex,
        });
    }

    let counts = DtppCounts {
        airports: airports.len(),
        plate_records: catalog.len(),
        unique_pdfs: pdf_blobs.len(),
    };

    Ok(CompiledDtpp {
        airac_code: cycle.airac_code.clone(),
        catalog,
        pdf_blobs,
        bounds: DEFAULT_DTPP_BOUNDS,
        counts,
    })
}

/// Encodes `compiled` into a deterministic `FBDTPP01` binary image (`dtpp_plates.bin`).
///
/// # Errors
///
/// Returns [`DtppCompileError::Encode`] if JSON serialization fails or JSON length exceeds `u32`.
pub fn encode_dtpp_binary(compiled: &CompiledDtpp) -> Result<Vec<u8>, DtppCompileError> {
    let json_bytes = serde_json::to_vec(&compiled.catalog)
        .map_err(|e| DtppCompileError::Encode(e.to_string()))?;
    let json_len = u32::try_from(json_bytes.len())
        .map_err(|_| DtppCompileError::Encode("catalog json_len overflow".into()))?;
    let blob_total: usize = compiled.pdf_blobs.iter().map(Vec::len).sum();

    let mut out = Vec::with_capacity(
        12_usize
            .saturating_add(json_bytes.len())
            .saturating_add(blob_total),
    );
    out.extend_from_slice(&FBDTPP01_MAGIC);
    out.extend_from_slice(&json_len.to_le_bytes());
    out.extend_from_slice(&json_bytes);
    for blob in &compiled.pdf_blobs {
        out.extend_from_slice(blob);
    }
    Ok(out)
}

/// Writes `dtpp_plates.bin`, `dtpp_plates.bin.sha256`, and `dtpp_faa_dtpp.json`, returning
/// validated [`EditionManifest`] and [`PublishedDtppManifest`] records.
///
/// # Errors
///
/// Returns [`DtppCompileError`] if encoding, digest validation, or file writing fails.
pub fn write_dtpp_pack(
    compiled: &CompiledDtpp,
    cycle: &ResolvedCycle,
    upstream: &NasrArchiveFingerprint,
    bin_path: &Path,
    manifest_path: &Path,
) -> Result<(EditionManifest, PublishedDtppManifest), DtppCompileError> {
    let bin_bytes = encode_dtpp_binary(compiled)?;
    let size_bytes = bin_bytes.len() as u64;
    let sha256_hex = format!("{:x}", Sha256::digest(&bin_bytes));

    if let Some(parent) = bin_path.parent() {
        fs::create_dir_all(parent).map_err(|source| DtppCompileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let part = bin_path.with_extension("bin.part");
    fs::write(&part, &bin_bytes).map_err(|source| DtppCompileError::Io {
        path: part.clone(),
        source,
    })?;
    fs::rename(&part, bin_path).map_err(|source| DtppCompileError::Io {
        path: bin_path.to_path_buf(),
        source,
    })?;

    let bin_filename = bin_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dtpp_plates.bin".to_owned());
    let sidecar_path = bin_path.with_file_name(format!("{bin_filename}.sha256"));
    fs::write(&sidecar_path, format!("{sha256_hex}  {bin_filename}\n")).map_err(|source| {
        DtppCompileError::Io {
            path: sidecar_path,
            source,
        }
    })?;

    let dtpp_digest = SourceDigest::new(
        AuthoritySource::FaaDtpp,
        cycle.effective_date.clone(),
        &upstream.sha256_hex,
    )
    .map_err(|e| DtppCompileError::Encode(e.to_string()))?;

    let edition_id = format!("flightbag-us-dtpp-{}", cycle.effective_date);
    let core_manifest = EditionManifest {
        schema_version: 1,
        edition_id: edition_id.clone(),
        airac: cycle.airac,
        sources: vec![dtpp_digest],
        signature_ed25519_hex: None,
    };

    let published = PublishedDtppManifest {
        schema_version: 1,
        disclaimer: DISCLAIMER.to_owned(),
        edition_id,
        authority: "faa_dtpp".to_owned(),
        airac_code: cycle.airac_code.clone(),
        cycle_index: cycle.cycle_index,
        effective_date: cycle.effective_date.clone(),
        expires_date: cycle.expires_date.clone(),
        effective_from_utc: cycle.effective_from_utc.clone(),
        effective_until_utc: cycle.effective_until_utc.clone(),
        upstream_url: upstream.url.clone(),
        upstream_sha256_hex: upstream.sha256_hex.clone(),
        dtpp_binary: DtppBinaryArtifact {
            file: bin_filename,
            size_bytes,
            sha256_hex,
            bounds: compiled.bounds,
        },
        counts: compiled.counts.clone(),
    };

    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent).map_err(|source| DtppCompileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut json_bytes = serde_json::to_vec_pretty(&published)
        .map_err(|e| DtppCompileError::Encode(e.to_string()))?;
    json_bytes.push(b'\n');
    fs::write(manifest_path, json_bytes).map_err(|source| DtppCompileError::Io {
        path: manifest_path.to_path_buf(),
        source,
    })?;

    Ok((core_manifest, published))
}

fn fetch_all_pdfs(
    cycle: &ResolvedCycle,
    names: &[String],
    cache_dir: &Path,
    threads: usize,
    custom_transport: Option<&dyn Transport>,
) -> Result<BTreeMap<String, Vec<u8>>, DtppCompileError> {
    if let Some(transport) = custom_transport {
        let mut out = BTreeMap::new();
        for name in names {
            let bytes = fetch_one_pdf(transport, cycle, name, cache_dir)?;
            out.insert(name.clone(), bytes);
        }
        return Ok(out);
    }

    let worker_count = threads.clamp(1, 8).min(names.len().max(1));
    if worker_count <= 1 {
        let transport = UreqTransport::new();
        let mut out = BTreeMap::new();
        for name in names {
            let bytes = fetch_one_pdf(&transport, cycle, name, cache_dir)?;
            out.insert(name.clone(), bytes);
        }
        return Ok(out);
    }

    let next_idx = AtomicUsize::new(0);
    let results: Vec<Result<Vec<u8>, DtppCompileError>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let next_ref = &next_idx;
            handles.push(scope.spawn(move || {
                let transport = UreqTransport::new();
                let mut local = Vec::new();
                loop {
                    let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                    let Some(name) = names.get(idx) else {
                        break;
                    };
                    local.push((idx, fetch_one_pdf(&transport, cycle, name, cache_dir)));
                }
                local
            }));
        }

        let mut merged: Vec<Option<Result<Vec<u8>, DtppCompileError>>> =
            (0..names.len()).map(|_| None).collect();
        for handle in handles {
            match handle.join() {
                Ok(items) => {
                    for (idx, res) in items {
                        if let Some(slot) = merged.get_mut(idx) {
                            *slot = Some(res);
                        }
                    }
                }
                Err(_) => {
                    return vec![Err(DtppCompileError::Encode(
                        "d-TPP PDF fetch worker panicked".to_owned(),
                    ))];
                }
            }
        }
        merged
            .into_iter()
            .map(|opt| {
                opt.unwrap_or_else(|| {
                    Err(DtppCompileError::Encode(
                        "missing d-TPP PDF fetch result".to_owned(),
                    ))
                })
            })
            .collect()
    });

    let mut out = BTreeMap::new();
    for (name, res) in names.iter().zip(results) {
        out.insert(name.clone(), res?);
    }
    Ok(out)
}

fn fetch_one_pdf(
    transport: &dyn Transport,
    cycle: &ResolvedCycle,
    pdf_name: &str,
    cache_dir: &Path,
) -> Result<Vec<u8>, DtppCompileError> {
    let dest = cache_dir.join(pdf_name);
    if dest.is_file() {
        let cached = fs::read(&dest).map_err(|source| DtppCompileError::Io {
            path: dest.clone(),
            source,
        })?;
        if cached.starts_with(b"%PDF-") {
            return Ok(cached);
        }
    }

    let url = dtpp_pdf_url(cycle, pdf_name);
    let resp = transport.get_blocking(&url, None)?;
    if resp.status != 200 {
        return Err(DtppCompileError::Fetch(FetchError::Status {
            url,
            status: resp.status,
        }));
    }
    let mut body = Vec::new();
    let mut reader = resp.body;
    reader
        .read_to_end(&mut body)
        .map_err(|source| DtppCompileError::Io {
            path: dest.clone(),
            source,
        })?;
    if !body.starts_with(b"%PDF-") {
        return Err(DtppCompileError::InvalidPdf {
            pdf_name: pdf_name.to_owned(),
        });
    }

    let part = cache_dir.join(format!("{pdf_name}.part"));
    fs::write(&part, &body).map_err(|source| DtppCompileError::Io {
        path: part.clone(),
        source,
    })?;
    fs::rename(&part, &dest).map_err(|source| DtppCompileError::Io { path: dest, source })?;
    Ok(body)
}

#[derive(Default)]
struct RecordScratch {
    chart_code: String,
    chart_name: String,
    pdf_name: String,
    cifp_ident: String,
}

impl RecordScratch {
    fn push_text(&mut self, field: &str, text: &str) {
        match field {
            "chart_code" => self.chart_code.push_str(text),
            "chart_name" => self.chart_name.push_str(text),
            "pdf_name" => self.pdf_name.push_str(text),
            "faanfd18" => self.cifp_ident.push_str(text),
            _ => {}
        }
    }

    fn into_record(
        self,
        airport_ident: &str,
        faa_ident: &str,
        icao_ident: &str,
    ) -> Option<ParsedDtppRecord> {
        let chart_code = self.chart_code.trim().to_ascii_uppercase();
        if !matches!(
            chart_code.as_str(),
            "APD" | "IAP" | "DP" | "ODP" | "SID" | "STAR"
        ) {
            return None;
        }
        let pdf_name = self.pdf_name.trim().to_ascii_uppercase();
        if pdf_name.is_empty() {
            return None;
        }
        let chart_name = self.chart_name.trim().to_owned();
        if chart_name.is_empty() {
            return None;
        }
        Some(ParsedDtppRecord {
            airport_ident: airport_ident.to_owned(),
            faa_ident: faa_ident.to_owned(),
            icao_ident: icao_ident.to_owned(),
            chart_code,
            chart_name,
            pdf_name,
            cifp_ident: self.cifp_ident.trim().to_ascii_uppercase(),
        })
    }
}

fn iap_priority(chart_name: &str) -> u8 {
    let upper = chart_name.to_ascii_uppercase();
    if upper.starts_with("ILS ") {
        0
    } else if upper.starts_with("RNAV (GPS)") {
        1
    } else if upper.starts_with("RNAV (RNP)") {
        2
    } else if upper.starts_with("LOC ") {
        3
    } else if upper.starts_with("VOR ") {
        4
    } else if upper.starts_with("GLS ") {
        5
    } else {
        6
    }
}

fn local_name_bytes(raw: &[u8]) -> String {
    let slice = match raw.iter().rposition(|&b| b == b':') {
        Some(pos) => raw.get(pos + 1..).unwrap_or(raw),
        None => raw,
    };
    String::from_utf8_lossy(slice).into_owned()
}

fn attr_value(elem: &BytesStart<'_>, target: &str) -> Option<String> {
    for attr_res in elem.attributes().with_checks(false) {
        let Ok(attr) = attr_res else {
            continue;
        };
        if local_name_bytes(attr.key.as_ref()) == target {
            return Some(String::from_utf8_lossy(&attr.value).into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::cycles::{CycleSpec, resolve_cycle};
    use crate::fetch::{TransportResponse, UpstreamValidators};
    use time::macros::datetime;

    use super::*;

    struct MockDtppTransport {
        xml: Vec<u8>,
        pdfs: BTreeMap<String, Vec<u8>>,
    }

    impl Transport for MockDtppTransport {
        fn get_blocking(
            &self,
            url: &str,
            _range: Option<&str>,
        ) -> Result<TransportResponse, FetchError> {
            if url.ends_with("d-TPP_Metafile.xml") {
                return Ok(TransportResponse {
                    status: 200,
                    body: Box::new(Cursor::new(self.xml.clone())),
                    validators: UpstreamValidators::default(),
                });
            }
            for (name, bytes) in &self.pdfs {
                if url.ends_with(name) {
                    return Ok(TransportResponse {
                        status: 200,
                        body: Box::new(Cursor::new(bytes.clone())),
                        validators: UpstreamValidators::default(),
                    });
                }
            }
            Ok(TransportResponse {
                status: 404,
                body: Box::new(Cursor::new(Vec::new())),
                validators: UpstreamValidators::default(),
            })
        }
    }

    #[test]
    fn parses_dtpp_metafile_and_writes_fbdtpp01_archive() {
        let dir = std::env::temp_dir().join(format!("flightbag_dtpp_test_{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).unwrap();

        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<digital_tpp cycle="2612" from_edate="0901Z  11/26/26" to_edate="0901Z  12/24/26">
  <state_code ID="NJ" state_fullname="NEW JERSEY">
    <city_name ID="NEWARK" volume="NE-2">
      <airport_name ID="NEWARK LIBERTY INTL" apt_ident="EWR" icao_ident="KEWR">
        <record><chart_code>MIN</chart_code><chart_name>TAKEOFF MINIMUMS</chart_name><pdf_name>NETO.PDF</pdf_name><faanfd18/></record>
        <record><chart_code>APD</chart_code><chart_name>AIRPORT DIAGRAM</chart_name><pdf_name>00285AD.PDF</pdf_name><faanfd18/></record>
        <record><chart_code>IAP</chart_code><chart_name>ILS OR LOC RWY 22L</chart_name><pdf_name>00285IL22L.PDF</pdf_name><faanfd18>I22L</faanfd18></record>
        <record><chart_code>DP</chart_code><chart_name>LIBERTY FIVE</chart_name><pdf_name>00285LIBERTY.PDF</pdf_name><faanfd18>LIB5.LIB</faanfd18></record>
      </airport_name>
      <airport_name ID="TETERBORO" apt_ident="TEB" icao_ident="KTEB">
        <record><chart_code>APD</chart_code><chart_name>AIRPORT DIAGRAM</chart_name><pdf_name>00890AD.PDF</pdf_name><faanfd18/></record>
      </airport_name>
    </city_name>
  </state_code>
</digital_tpp>
"#;
        let mut pdfs = BTreeMap::new();
        pdfs.insert("00285AD.PDF".to_owned(), b"%PDF-1.4\nEWR AD\n".to_vec());
        pdfs.insert(
            "00285IL22L.PDF".to_owned(),
            b"%PDF-1.4\nEWR I22L\n".to_vec(),
        );
        pdfs.insert(
            "00285LIBERTY.PDF".to_owned(),
            b"%PDF-1.4\nEWR LIB5\n".to_vec(),
        );
        let transport = MockDtppTransport {
            xml: xml.as_bytes().to_vec(),
            pdfs,
        };

        let cycle = resolve_cycle(
            CycleSpec::EffectiveDate(crate::cycles::parse_iso_date("2026-11-26").unwrap()),
            datetime!(2026-11-26 12:00 UTC),
        )
        .unwrap();

        let cache = dir.join("cache");
        let fp = fetch_dtpp_metafile(&transport, &cycle, &cache).unwrap();
        assert!(!fp.cached);
        let fp_cached = fetch_dtpp_metafile(&transport, &cycle, &cache).unwrap();
        assert!(fp_cached.cached);

        let raw_records = parse_dtpp_metafile(&fp.path, "2612", &["KEWR"]).unwrap();
        assert_eq!(raw_records.len(), 3);
        let selected = select_curated_plates(raw_records);
        let compiled = compile_dtpp_records(&cycle, selected, &cache, 1, Some(&transport)).unwrap();
        assert_eq!(compiled.counts.airports, 1);
        assert_eq!(compiled.counts.plate_records, 3);
        assert_eq!(compiled.counts.unique_pdfs, 3);

        let out_dir = dir.join("out");
        let bin_path = out_dir.join("dtpp_plates.bin");
        let manifest_path = out_dir.join("dtpp_faa_dtpp.json");
        let (_core, published) =
            write_dtpp_pack(&compiled, &cycle, &fp, &bin_path, &manifest_path).unwrap();

        let bytes = fs::read(&bin_path).unwrap();
        assert_eq!(&bytes[0..8], b"FBDTPP01");
        let json_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let entries: Vec<DtppCatalogEntry> =
            serde_json::from_slice(&bytes[12..12 + json_len]).unwrap();
        assert_eq!(entries.len(), 3);
        let blob_base = 12 + json_len;
        for entry in &entries {
            let start = blob_base + entry.rel_offset as usize;
            let end = start + entry.length as usize;
            assert!(bytes[start..end].starts_with(b"%PDF-"));
        }
        assert_eq!(published.authority, "faa_dtpp");
        assert!(out_dir.join("dtpp_plates.bin.sha256").is_file());
        assert!(manifest_path.is_file());

        drop(fs::remove_dir_all(&dir));
    }
}
