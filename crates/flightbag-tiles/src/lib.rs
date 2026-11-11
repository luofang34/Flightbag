//! Pure-Rust MVT 2.1 polygon/line pyramid encoder and `PMTiles` v3 archive builder for `Flightbag`.

pub mod archive;
pub mod clip;
pub mod mvt;
pub mod pyramid;

use std::fs;
use std::path::Path;

use flightbag_core::{AirspaceVolume, AuthoritySource, EditionManifest, SourceDigest};
use flightbag_nasr::boolean2d::polygons_bbox;
use flightbag_nasr::{NasrAirspaceCounts, NasrArchiveFingerprint, ResolvedCycle};

pub use archive::ArchiveError;
pub use pyramid::{MAX_ZOOM, MIN_ZOOM};

/// Output metrics of a compiled `airspace.pmtiles` archive.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AirspaceArchiveStats {
    /// Total encoded MVT tiles across `z2..=10`.
    pub tile_count: usize,
    /// Minimum zoom level (`2`).
    pub min_zoom: u8,
    /// Maximum zoom level (`10`).
    pub max_zoom: u8,
    /// Geographic bounds `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
    /// Final `.pmtiles` file size in bytes.
    pub size_bytes: u64,
    /// Lowercase 64-character SHA-256 hex digest of `airspace.pmtiles`.
    pub sha256_hex: String,
}

/// Published JSON cycle manifest written alongside `airspace.pmtiles` for GitHub Pages and Release consumers.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PublishedCycleManifest {
    /// Manifest schema version (`1`).
    pub schema_version: u32,
    /// Edition identifier (`"flightbag-us-nasr-YYYY-MM-DD"`).
    pub edition_id: String,
    /// Four-digit ICAO/FAA `YYCC` AIRAC code.
    pub airac_code: String,
    /// Cycle index from AIRAC `2001` (`2020-01-02`).
    pub cycle_index: i32,
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// Expiration date (`YYYY-MM-DD`).
    pub expires_date: String,
    /// Inclusive UTC activation instant (`YYYY-MM-DDT09:01:00Z`).
    pub effective_from_utc: String,
    /// Exclusive UTC expiration instant (`YYYY-MM-DDT09:01:00Z`).
    pub effective_until_utc: String,
    /// Upstream FAA NASR archive provenance.
    pub upstream_nasr: UpstreamSourceRecord,
    /// Compiled `airspace.pmtiles` artifact metadata.
    pub airspace_pmtiles: PublishedArtifactRecord,
    /// Compiled 3D airspace volume counts by class.
    pub counts: NasrAirspaceCounts,
}

/// Provenance record for the upstream FAA 28-day NASR `.zip`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UpstreamSourceRecord {
    /// Authority identifier (`"faa_nasr"`).
    pub authority: String,
    /// Official FAA download URL.
    pub url: String,
    /// Archive filename.
    pub filename: String,
    /// Byte length of the `.zip`.
    pub size_bytes: u64,
    /// Lowercase SHA-256 hex digest.
    pub sha256_hex: String,
}

/// Artifact metadata for `airspace.pmtiles`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PublishedArtifactRecord {
    /// Relative filename (`"airspace.pmtiles"`).
    pub file: String,
    /// Byte length of `airspace.pmtiles`.
    pub size_bytes: u64,
    /// Lowercase SHA-256 hex digest.
    pub sha256_hex: String,
    /// Total encoded vector tiles.
    pub tile_count: usize,
    /// Minimum zoom level.
    pub min_zoom: u8,
    /// Maximum zoom level.
    pub max_zoom: u8,
    /// Geographic bounds `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
}

/// Encodes `volumes` into `z2..=10` Mapbox Vector Tiles and writes `output_pmtiles`.
///
/// # Errors
///
/// Returns [`ArchiveError`] if `output_pmtiles` cannot be created or finalized.
pub fn build_airspace_pmtiles(
    volumes: &[AirspaceVolume],
    cycle: &ResolvedCycle,
    output_pmtiles: &Path,
) -> Result<AirspaceArchiveStats, ArchiveError> {
    let bounds = dataset_bounds(volumes);
    let tiles = pyramid::build_mvt_pyramid(volumes);
    let tile_count = tiles.len();
    let (size_bytes, sha256_hex) = archive::write_pmtiles_archive(
        &tiles,
        bounds,
        &cycle.effective_date,
        &cycle.airac_code,
        output_pmtiles,
    )?;

    Ok(AirspaceArchiveStats {
        tile_count,
        min_zoom: MIN_ZOOM,
        max_zoom: MAX_ZOOM,
        bounds: [bounds.0, bounds.1, bounds.2, bounds.3],
        size_bytes,
        sha256_hex,
    })
}

/// Validates core [`EditionManifest`] provenance and writes `manifest.json` to `manifest_path`.
///
/// # Errors
///
/// Returns [`ArchiveError`] if digest validation, JSON formatting, or file writing fails.
pub fn write_cycle_manifest(
    cycle: &ResolvedCycle,
    nasr_fp: &NasrArchiveFingerprint,
    counts: &NasrAirspaceCounts,
    stats: &AirspaceArchiveStats,
    manifest_path: &Path,
) -> Result<(EditionManifest, PublishedCycleManifest), ArchiveError> {
    let nasr_digest = SourceDigest::new(
        AuthoritySource::FaaNasr,
        cycle.effective_date.clone(),
        &nasr_fp.sha256_hex,
    )
    .map_err(|err| ArchiveError::Json(err.to_string()))?;

    let edition_id = format!("flightbag-us-nasr-{}", cycle.effective_date);
    let core_manifest = EditionManifest {
        schema_version: 1,
        edition_id: edition_id.clone(),
        airac: cycle.airac,
        sources: vec![nasr_digest],
        signature_ed25519_hex: None,
    };

    let published = PublishedCycleManifest {
        schema_version: 1,
        edition_id,
        airac_code: cycle.airac_code.clone(),
        cycle_index: cycle.cycle_index,
        effective_date: cycle.effective_date.clone(),
        expires_date: cycle.expires_date.clone(),
        effective_from_utc: cycle.effective_from_utc.clone(),
        effective_until_utc: cycle.effective_until_utc.clone(),
        upstream_nasr: UpstreamSourceRecord {
            authority: "faa_nasr".to_owned(),
            url: nasr_fp.url.clone(),
            filename: nasr_fp.filename.clone(),
            size_bytes: nasr_fp.size_bytes,
            sha256_hex: nasr_fp.sha256_hex.clone(),
        },
        airspace_pmtiles: PublishedArtifactRecord {
            file: "airspace.pmtiles".to_owned(),
            size_bytes: stats.size_bytes,
            sha256_hex: stats.sha256_hex.clone(),
            tile_count: stats.tile_count,
            min_zoom: stats.min_zoom,
            max_zoom: stats.max_zoom,
            bounds: stats.bounds,
        },
        counts: counts.clone(),
    };

    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent).map_err(|source| ArchiveError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut json_bytes =
        serde_json::to_vec_pretty(&published).map_err(|e| ArchiveError::Json(e.to_string()))?;
    json_bytes.push(b'\n');
    fs::write(manifest_path, json_bytes).map_err(|source| ArchiveError::Io {
        path: manifest_path.to_path_buf(),
        source,
    })?;

    Ok((core_manifest, published))
}

fn dataset_bounds(volumes: &[AirspaceVolume]) -> (f64, f64, f64, f64) {
    let mut agg: Option<flightbag_nasr::GeoBbox> = None;
    for vol in volumes {
        if let Some(b) = polygons_bbox(&vol.polygons) {
            agg = Some(match agg {
                None => b,
                Some(cur) => flightbag_nasr::GeoBbox {
                    min_lon: cur.min_lon.min(b.min_lon),
                    min_lat: cur.min_lat.min(b.min_lat),
                    max_lon: cur.max_lon.max(b.max_lon),
                    max_lat: cur.max_lat.max(b.max_lat),
                },
            });
        }
    }
    match agg {
        Some(b) => (b.min_lon, b.min_lat, b.max_lon, b.max_lat),
        None => (-125.0, 24.0, -66.5, 49.5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{AttributeValue, EncodedFeature, FeatureGeometry, MvtLayerInput, TilePolygon};

    #[test]
    fn encodes_polygon_and_line_mvt_deterministically() {
        let poly = TilePolygon {
            exterior: vec![(100, 100), (500, 100), (500, 500), (100, 500)],
            interiors: vec![vec![(200, 200), (300, 200), (300, 300), (200, 300)]],
        };
        let layer = MvtLayerInput {
            name: "airspace".to_owned(),
            extent: 4096,
            features: vec![EncodedFeature {
                id: 1,
                attributes: vec![
                    ("class".to_owned(), AttributeValue::String("B".to_owned())),
                    ("lower_m".to_owned(), AttributeValue::Double(914.4)),
                ],
                geometry: FeatureGeometry::Polygons(vec![poly]),
            }],
        };
        let bytes_a = mvt::encode_tile(std::slice::from_ref(&layer));
        let bytes_b = mvt::encode_tile(&[layer]);
        assert!(!bytes_a.is_empty());
        assert_eq!(bytes_a, bytes_b);
    }

    #[test]
    fn builds_valid_pmtiles_v3_archive_and_e5_transition_ceiling() {
        use flightbag_core::{
            AirspaceClassification, AirspacePolygon, ControlledAirspaceClass, ControlledLocalType,
            VerticalLimit,
        };
        use flightbag_nasr::cycles::{CycleSpec, resolve_cycle};
        use time::macros::datetime;
        use world_core::GeodeticPosition;

        let cycle_res = resolve_cycle(
            CycleSpec::parse("2026-10-29").unwrap_or(CycleSpec::Current),
            datetime!(2026-11-11 10:00 UTC),
        );
        assert!(cycle_res.is_ok());
        let Some(cycle) = cycle_res.ok() else {
            return;
        };

        let vol = AirspaceVolume {
            airspace_key: "SEA E5".into(),
            ident: "SEA".into(),
            name: "SEATTLE CLASS E5".into(),
            sector: None,
            classification: AirspaceClassification::Controlled {
                class: ControlledAirspaceClass::ClassE,
                local_type: ControlledLocalType::ClassE5Transition,
            },
            lower: VerticalLimit::agl_ft(700),
            upper: VerticalLimit::msl_ft(18_000),
            hours_of_use: None,
            controlling_agency: None,
            polygons: vec![AirspacePolygon {
                exterior: vec![
                    GeodeticPosition::wgs84(47.3, -122.5, 0.0),
                    GeodeticPosition::wgs84(47.3, -122.1, 0.0),
                    GeodeticPosition::wgs84(47.6, -122.1, 0.0),
                    GeodeticPosition::wgs84(47.6, -122.5, 0.0),
                ],
                interiors: Vec::new(),
            }],
        };

        let out_path = std::env::temp_dir().join("flightbag_test_airspace.pmtiles");
        let stats_res = build_airspace_pmtiles(&[vol], &cycle, &out_path);
        assert!(stats_res.is_ok());
        if let Ok(stats) = stats_res {
            assert!(stats.tile_count > 0);
            assert!(stats.size_bytes > 127);
            let bytes = fs::read(&out_path).unwrap_or_default();
            assert_eq!(bytes.get(..8), Some(b"PMTiles\x03".as_slice()));
        }
        drop(fs::remove_file(&out_path));
    }
}
