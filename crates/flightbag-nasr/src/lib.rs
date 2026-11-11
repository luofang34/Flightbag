//! Pure-Rust FAA 28-day NASR fetcher and 3D Controlled + Special Use Airspace compiler.
//!
//! Reads a verified `28DaySubscription_Effective_YYYY-MM-DD.zip` archive directly in memory
//! without external GIS dependencies (`GDAL`, `tippecanoe`, or `Python`):
//! - [`cycles`] and [`fetch`] resolve 28-day AIRAC effective dates (`09:01 UTC` cutover),
//!   probe FAA publication status with a 1-byte HTTP range request, and cache verified
//!   archives with `.sha256` sidecars.
//! - [`class_airspace`] compiles `Additional_Data/Shape_Files/Class_Airspace.{shp,dbf}` into
//!   laterally clean, shelf-normalized 3D [`flightbag_core::AirspaceVolume`]s (`Class B, C, D, E`).
//! - [`sua_aixm`] compiles `Additional_Data/AIXM/SAA-AIXM_5_Schema/SaaSubscriberFile.zip`
//!   (`Saa_Sub_File.zip/*.xml`) into 3D Special Use Airspace volumes (`P, R, W, MOA, A, NSA`),
//!   evaluating geodesic arcs/circles and 3D `BASE` / `UNION` / `SUBTR` boolean operations.

pub mod boolean2d;
pub mod class_airspace;
pub mod cycles;
pub mod fetch;
pub mod sua_aixm;

use std::fs::File;
use std::path::Path;

use flightbag_core::AirspaceVolume;
use thiserror::Error;

pub use cycles::{CycleSpec, NasrCycleError, ResolvedCycle};
pub use fetch::{FetchError, NasrArchiveFingerprint, Transport, UreqTransport};

/// Optional geographic bounding box `(min_lon, min_lat, max_lon, max_lat)` in WGS-84 degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoBbox {
    /// Western longitude bound in degrees (`-180..=180`).
    pub min_lon: f64,
    /// Southern latitude bound in degrees (`-90..=90`).
    pub min_lat: f64,
    /// Eastern longitude bound in degrees (`-180..=180`).
    pub max_lon: f64,
    /// Northern latitude bound in degrees (`-90..=90`).
    pub max_lat: f64,
}

impl GeoBbox {
    /// Returns `true` when `other` overlaps this bounding box.
    pub fn intersects(self, other: Self) -> bool {
        self.min_lon <= other.max_lon
            && self.max_lon >= other.min_lon
            && self.min_lat <= other.max_lat
            && self.max_lat >= other.min_lat
    }

    /// Computes the bounding box of a slice of [`world_core::GeodeticPosition`] points.
    pub fn from_positions(points: &[world_core::GeodeticPosition]) -> Option<Self> {
        let first = points.first()?;
        let mut bbox = Self {
            min_lon: first.longitude_deg,
            min_lat: first.latitude_deg,
            max_lon: first.longitude_deg,
            max_lat: first.latitude_deg,
        };
        for pos in points {
            bbox.min_lon = bbox.min_lon.min(pos.longitude_deg);
            bbox.max_lon = bbox.max_lon.max(pos.longitude_deg);
            bbox.min_lat = bbox.min_lat.min(pos.latitude_deg);
            bbox.max_lat = bbox.max_lat.max(pos.latitude_deg);
        }
        Some(bbox)
    }
}

/// Counts of compiled NASR airspace volumes broken down by class and category.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NasrAirspaceCounts {
    /// Total compiled 3D volumes (`Class B..E` + `SUA`).
    pub total: usize,
    /// Class B shelves and cores.
    pub class_b: usize,
    /// Class C shelves and cores.
    pub class_c: usize,
    /// Class D surface cylinders/extensions.
    pub class_d: usize,
    /// Class E (`E2`..`E7`) controlled airspace volumes.
    pub class_e: usize,
    /// Special Use Airspace (`P, R, W, MOA, A, NSA`) volumes.
    pub sua: usize,
}

/// Complete compiled NASR 3D airspace dataset from one 28-day subscription archive.
#[derive(Clone, Debug)]
pub struct CompiledNasrAirspace {
    /// Ordered list of compiled 3D airspace volumes.
    pub volumes: Vec<AirspaceVolume>,
    /// Per-category volume counts.
    pub counts: NasrAirspaceCounts,
}

/// Errors raised while parsing or boolean-normalizing a NASR subscription archive.
#[derive(Debug, Error)]
pub enum NasrCompileError {
    /// Filesystem I/O failure opening the archive.
    #[error("cannot open NASR archive {path}: {source}")]
    Io {
        /// Archive path.
        path: std::path::PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// Outer or nested ZIP container could not be read.
    #[error("invalid NASR zip container ({context}): {source}")]
    Zip {
        /// Entry or container description.
        context: String,
        /// Underlying zip error.
        #[source]
        source: zip::result::ZipError,
    },
    /// `Class_Airspace.{shp,dbf}` reader error.
    #[error("invalid Class_Airspace shapefile: {0}")]
    Shapefile(String),
    /// SAA AIXM 5.0 XML parse error.
    #[error("invalid SAA AIXM XML in {entry}: {reason}")]
    Xml {
        /// Nested XML filename.
        entry: String,
        /// Explanation of the XML defect.
        reason: String,
    },
    /// A compiled volume failed [`AirspaceVolume::validate`].
    #[error("compiled airspace volume {ident} failed validation: {source}")]
    Contract {
        /// Identifier of the invalid volume.
        ident: String,
        /// Core validation error.
        #[source]
        source: flightbag_core::FlightbagError,
    },
}

/// Compiles both Controlled Airspace (`Class B, C, D, E`) and Special Use Airspace
/// (`P, R, W, MOA, A, NSA`) from a local `28DaySubscription_Effective_YYYY-MM-DD.zip`.
///
/// # Errors
///
/// Returns [`NasrCompileError`] if any required shapefile or AIXM entry is missing or malformed.
pub fn compile_nasr_airspace_zip(
    zip_path: &Path,
    bbox_filter: Option<GeoBbox>,
) -> Result<CompiledNasrAirspace, NasrCompileError> {
    let file = File::open(zip_path).map_err(|source| NasrCompileError::Io {
        path: zip_path.to_path_buf(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|source| NasrCompileError::Zip {
        context: zip_path.display().to_string(),
        source,
    })?;

    let mut volumes = class_airspace::compile_class_airspace(&mut archive, bbox_filter)?;
    let sua_volumes = sua_aixm::compile_sua_airspace(&mut archive, bbox_filter)?;
    volumes.extend(sua_volumes);

    let mut counts = NasrAirspaceCounts {
        total: volumes.len(),
        ..NasrAirspaceCounts::default()
    };
    for volume in &volumes {
        volume
            .validate()
            .map_err(|source| NasrCompileError::Contract {
                ident: volume.ident.clone(),
                source,
            })?;
        match volume.classification {
            flightbag_core::AirspaceClassification::Controlled { class, .. } => match class {
                flightbag_core::ControlledAirspaceClass::ClassB => {
                    counts.class_b = counts.class_b.saturating_add(1);
                }
                flightbag_core::ControlledAirspaceClass::ClassC => {
                    counts.class_c = counts.class_c.saturating_add(1);
                }
                flightbag_core::ControlledAirspaceClass::ClassD => {
                    counts.class_d = counts.class_d.saturating_add(1);
                }
                flightbag_core::ControlledAirspaceClass::ClassE => {
                    counts.class_e = counts.class_e.saturating_add(1);
                }
            },
            flightbag_core::AirspaceClassification::SpecialUse(_) => {
                counts.sua = counts.sua.saturating_add(1);
            }
        }
    }

    Ok(CompiledNasrAirspace { volumes, counts })
}

/// Normalizes a raw NASR identifier or name into a valid `1..=16` ASCII `world-notices` key.
pub fn canonical_airspace_key(raw: &str, fallback_prefix: &str) -> String {
    let mut out = String::with_capacity(16);
    for ch in raw.trim().chars() {
        if out.len() >= 16 {
            break;
        }
        let upper = ch.to_ascii_uppercase();
        if upper.is_ascii_alphanumeric() || matches!(upper, '-' | ' ' | '/') {
            out.push(upper);
        }
    }
    let trimmed = out.trim().to_owned();
    if !trimmed.is_empty() {
        return trimmed;
    }
    let mut fb = String::new();
    for ch in fallback_prefix.chars().take(16) {
        let upper = ch.to_ascii_uppercase();
        if upper.is_ascii_alphanumeric() || upper == '-' {
            fb.push(upper);
        }
    }
    if fb.is_empty() { "NASR".to_owned() } else { fb }
}
