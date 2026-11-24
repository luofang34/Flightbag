//! Hilbert-ordered `PMTiles` v3 vector & raster writers and SHA-256 companion sidecars.

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

/// Metadata parameters for a compiled raster chart `PMTiles` v3 archive.
pub struct RasterArchiveParams<'a> {
    /// Geographic bounding box `(min_lon, min_lat, max_lon, max_lat)`.
    pub bounds: (f64, f64, f64, f64),
    /// Minimum zoom level stored in the archive.
    pub min_zoom: u8,
    /// Maximum zoom level stored in the archive.
    pub max_zoom: u8,
    /// Archive display title.
    pub title: &'a str,
    /// Archive description (including safety disclaimer).
    pub description: &'a str,
    /// Attribution string.
    pub attribution: &'a str,
    /// Governing cycle / AIRAC version code.
    pub version: &'a str,
}

/// Writes MVT `tiles` into a Hilbert-ordered, gzip-compressed `PMTiles` v3 archive at `path`
/// together with a `<filename>.sha256` sidecar, returning `(size_bytes, sha256_hex)`.
///
/// # Errors
///
/// Returns [`ArchiveError`] if `path` or its `.sha256` companion cannot be written or finalized.
pub fn write_pmtiles_archive(
    tiles: &BTreeMap<(u8, u32, u32), Vec<u8>>,
    bounds: (f64, f64, f64, f64),
    cycle_date: &str,
    airac_code: &str,
    path: &Path,
) -> Result<(u64, String), ArchiveError> {
    let metadata = tilejson_metadata(cycle_date, airac_code);
    write_pmtiles_internal(
        tiles,
        TileType::Mvt,
        Compression::Gzip,
        (MIN_ZOOM, MAX_ZOOM, 5),
        bounds,
        &metadata.to_string(),
        path,
    )
}

/// Writes PNG/JPEG raster chart `tiles` into a Hilbert-ordered `PMTiles` v3 archive at `path`
/// together with a `<filename>.sha256` sidecar, returning `(size_bytes, sha256_hex)`.
///
/// # Errors
///
/// Returns [`ArchiveError`] if `path` or its `.sha256` companion cannot be written or finalized.
pub fn write_raster_pmtiles_archive(
    tiles: &BTreeMap<(u8, u32, u32), Vec<u8>>,
    params: &RasterArchiveParams<'_>,
    path: &Path,
) -> Result<(u64, String), ArchiveError> {
    let tile_type = detect_raster_tile_type(tiles);
    let format_str = match tile_type {
        TileType::Jpeg => "jpg",
        _ => "png",
    };
    let metadata = serde_json::json!({
        "name": params.title,
        "description": params.description,
        "attribution": params.attribution,
        "version": params.version,
        "type": "baselayer",
        "format": format_str,
    });
    let center_zoom = params.min_zoom.midpoint(params.max_zoom);
    write_pmtiles_internal(
        tiles,
        tile_type,
        Compression::None,
        (params.min_zoom, params.max_zoom, center_zoom),
        params.bounds,
        &metadata.to_string(),
        path,
    )
}

fn detect_raster_tile_type(tiles: &BTreeMap<(u8, u32, u32), Vec<u8>>) -> TileType {
    if !tiles.is_empty()
        && tiles
            .values()
            .all(|bytes| bytes.get(..2) == Some(&[0xFF, 0xD8]))
    {
        TileType::Jpeg
    } else {
        TileType::Png
    }
}

fn write_pmtiles_internal(
    tiles: &BTreeMap<(u8, u32, u32), Vec<u8>>,
    tile_type: TileType,
    tile_compression: Compression,
    (min_zoom, max_zoom, center_zoom): (u8, u8, u8),
    bounds: (f64, f64, f64, f64),
    metadata_json: &str,
    path: &Path,
) -> Result<(u64, String), ArchiveError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ArchiveError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let part_path = path.with_extension("pmtiles.part");
    let file = fs::File::create(&part_path).map_err(|source| ArchiveError::Io {
        path: part_path.clone(),
        source,
    })?;

    let center_lon = ((bounds.0 + bounds.2) * 0.5).clamp(-180.0, 180.0);
    let center_lat = ((bounds.1 + bounds.3) * 0.5).clamp(-85.0, 85.0);

    let mut writer = PmTilesWriter::new(tile_type)
        .internal_compression(Compression::Gzip)
        .tile_compression(tile_compression)
        .min_zoom(min_zoom)
        .max_zoom(max_zoom)
        .bounds(bounds.0, bounds.1, bounds.2, bounds.3)
        .center(center_lon, center_lat)
        .center_zoom(center_zoom)
        .metadata(metadata_json)
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

    let (size_bytes, sha256_hex) = hash_file_sha256(path)?;
    write_sha256_sidecar(path, &sha256_hex)?;
    Ok((size_bytes, sha256_hex))
}

/// Writes `<path>.sha256` in standard `shasum -a 256` format (`<hex>  <filename>\n`).
pub fn write_sha256_sidecar(path: &Path, sha256_hex: &str) -> Result<PathBuf, ArchiveError> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive.pmtiles");
    let mut os = path.as_os_str().to_os_string();
    os.push(".sha256");
    let sidecar_path = PathBuf::from(os);
    fs::write(&sidecar_path, format!("{sha256_hex}  {file_name}\n")).map_err(|source| {
        ArchiveError::Io {
            path: sidecar_path.clone(),
            source,
        }
    })?;
    Ok(sidecar_path)
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
                "description": "3D volumetric airspace manifold primitives (walls, floor terraces, roof caps, and full volumes)",
                "minzoom": MIN_ZOOM,
                "maxzoom": MAX_ZOOM,
                "fields": {
                    "airspace_key": "String",
                    "category": "String",
                    "class": "String",
                    "local_type": "String",
                    "part": "String",
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
                "description": "3D airspace boundary polylines for surface-contact rims, elevated floor steps, and outer ceiling rims",
                "minzoom": MIN_ZOOM,
                "maxzoom": MAX_ZOOM,
                "fields": {
                    "airspace_key": "String",
                    "category": "String",
                    "class": "String",
                    "local_type": "String",
                    "ident": "String",
                    "boundary_role": "String",
                    "rim_m": "Number",
                    "rim_datum": "String",
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

/// Computes `(size_bytes, sha256_hex)` for an existing file on disk.
///
/// # Errors
///
/// Returns [`ArchiveError::Io`] if `path` cannot be opened or read.
pub fn hash_file_sha256(path: &Path) -> Result<(u64, String), ArchiveError> {
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
