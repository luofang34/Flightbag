//! Hilbert-ordered `PMTiles` v3 writer and SHA-256 cycle manifest serializer.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::pyramid::{MAX_ZOOM, MIN_ZOOM};

/// Errors encountered while writing `PMTiles` archives or JSON manifests.
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// Filesystem I/O failure.
    #[error("archive I/O failed at {path}: {source}")]
    Io {
        /// Output path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// `pmtiles` writer failure.
    #[error("PMTiles writer failed at {path}: {reason}")]
    Writer {
        /// Output path.
        path: PathBuf,
        /// Explanation from `pmtiles`.
        reason: String,
    },
    /// Out-of-range tile coordinate.
    #[error("invalid tile coordinate z{zoom}/{x}/{y}")]
    Coordinate {
        /// Zoom level.
        zoom: u8,
        /// Tile X.
        x: u32,
        /// Tile Y.
        y: u32,
    },
    /// JSON serialization error.
    #[error("manifest serialization failed: {0}")]
    Json(String),
}

/// Writes `tiles` into a Hilbert-ordered, gzip-compressed `PMTiles` v3 archive at `path`,
/// returning `(size_bytes, sha256_hex)`.
///
/// # Errors
///
/// Returns [`ArchiveError`] if `path` cannot be written or finalized.
pub fn write_pmtiles_archive(
    tiles: &BTreeMap<(u8, u32, u32), Vec<u8>>,
    bounds: (f64, f64, f64, f64),
    cycle_date: &str,
    airac_code: &str,
    path: &Path,
) -> Result<(u64, String), ArchiveError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ArchiveError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let metadata = tilejson_metadata(cycle_date, airac_code);
    let part_path = path.with_extension("pmtiles.part");
    let file = fs::File::create(&part_path).map_err(|source| ArchiveError::Io {
        path: part_path.clone(),
        source,
    })?;

    let center_lon = ((bounds.0 + bounds.2) * 0.5).clamp(-180.0, 180.0);
    let center_lat = ((bounds.1 + bounds.3) * 0.5).clamp(-85.0, 85.0);

    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .internal_compression(Compression::Gzip)
        .tile_compression(Compression::Gzip)
        .min_zoom(MIN_ZOOM)
        .max_zoom(MAX_ZOOM)
        .bounds(bounds.0, bounds.1, bounds.2, bounds.3)
        .center(center_lon, center_lat)
        .center_zoom(5)
        .metadata(&metadata.to_string())
        .create(file)
        .map_err(|err| ArchiveError::Writer {
            path: part_path.clone(),
            reason: err.to_string(),
        })?;

    let mut ordered: Vec<(TileId, &Vec<u8>)> = Vec::with_capacity(tiles.len());
    for (&(zoom, x, y), payload) in tiles {
        let coord =
            TileCoord::new(zoom, x, y).map_err(|_| ArchiveError::Coordinate { zoom, x, y })?;
        ordered.push((TileId::from(coord), payload));
    }
    ordered.sort_by_key(|(id, _)| *id);

    for (id, payload) in ordered {
        writer
            .add_tile(TileCoord::from(id), payload)
            .map_err(|err| ArchiveError::Writer {
                path: part_path.clone(),
                reason: err.to_string(),
            })?;
    }

    writer.finalize().map_err(|err| ArchiveError::Writer {
        path: part_path.clone(),
        reason: err.to_string(),
    })?;

    fs::rename(&part_path, path).map_err(|source| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    hash_file_sha256(path)
}

fn tilejson_metadata(cycle_date: &str, airac_code: &str) -> serde_json::Value {
    serde_json::json!({
        "name": format!("Flightbag US NASR 3D Airspace ({cycle_date} / AIRAC {airac_code} — Experimental Prototype, Not for Navigation or Flight)"),
        "description": "EXPERIMENTAL PROTOTYPE — NOT FOR NAVIGATION OR FLIGHT. FAA 28-day NASR Controlled Airspace (Class B, C, D, E) and Special Use Airspace (P, R, W, MOA, A, NSA) 3D volumes for visualization and simulation only.",
        "attribution": "FAA NASR (Experimental Prototype — Not for Navigation or Flight)",
        "version": airac_code,
        "vector_layers": [
            {
                "id": "airspace",
                "description": "3D volumetric airspace polygons with legal & display vertical limits (MSL/AGL)",
                "minzoom": MIN_ZOOM,
                "maxzoom": MAX_ZOOM,
                "fields": {
                    "airspace_key": "String",
                    "category": "String",
                    "class": "String",
                    "local_type": "String",
                    "ident": "String",
                    "name": "String",
                    "sector": "String",
                    "lower_m": "Number",
                    "upper_m": "Number",
                    "display_upper_m": "Number",
                    "lower_ft": "Number",
                    "upper_ft": "Number",
                    "lower_datum": "String",
                    "upper_datum": "String",
                    "display_upper_datum": "String",
                    "datum_mode": "String",
                    "floating_floor": "Boolean",
                    "is_enroute_blanket": "Boolean",
                    "hours": "String",
                    "agency": "String"
                }
            },
            {
                "id": "airspace_boundary",
                "description": "3D airspace boundary polylines for elevated upper/lower chart rims",
                "minzoom": MIN_ZOOM,
                "maxzoom": MAX_ZOOM,
                "fields": {
                    "airspace_key": "String",
                    "category": "String",
                    "class": "String",
                    "local_type": "String",
                    "ident": "String",
                    "lower_m": "Number",
                    "upper_m": "Number",
                    "display_upper_m": "Number",
                    "lower_datum": "String",
                    "display_upper_datum": "String",
                    "floating_floor": "Boolean",
                    "is_enroute_blanket": "Boolean"
                }
            }
        ]
    })
}

fn hash_file_sha256(path: &Path) -> Result<(u64, String), ArchiveError> {
    let mut file = fs::File::open(path).map_err(|source| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 1024 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf).map_err(|source| ArchiveError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
            total = total.saturating_add(n as u64);
        }
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}
