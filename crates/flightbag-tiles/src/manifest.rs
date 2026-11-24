//! Rolling `v99n62`-style multi-authority manifest (`manifest.json`) and raster chart manifests.

use std::fs;
use std::path::Path;

use flightbag_core::{EditionManifest, SourceDigest};
use flightbag_nasr::PublishedCifpManifest;
use flightbag_nasr::charts::{HarvestedRasterPyramid, RasterChartSeries, ResolvedChartCycle};

use crate::archive::{ArchiveError, RasterArchiveParams, write_raster_pmtiles_archive};
use crate::ipfs::{ArtifactStorage, IpfsOptions, IpfsPublication, annotate_entries};
use crate::{PROTOTYPE_DISCLAIMER, PublishedArtifactRecord, PublishedCycleManifest};

/// Canonical MapLibre VFR Sectional + TAC + 3D Airspace style JSON.
pub const AIRSPACE_VFR_SECTIONAL_STYLE_JSON: &str =
    include_str!("../../../styles/airspace_vfr_sectional.json");

/// Output metrics of a compiled raster chart `.pmtiles` archive (`vfr_sectional.pmtiles`, etc.).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RasterArchiveStats {
    /// Authority slug (`"faa_vfr_sectional"`, `"faa_vfr_terminal"`, `"faa_ifr_low"`, `"faa_ifr_high"`).
    pub authority: String,
    /// Output filename inside the cycle directory.
    pub file: String,
    /// Total encoded raster tiles.
    pub tile_count: usize,
    /// Minimum zoom level.
    pub min_zoom: u8,
    /// Maximum zoom level.
    pub max_zoom: u8,
    /// Geographic bounds `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
    /// Final `.pmtiles` file size in bytes.
    pub size_bytes: u64,
    /// Lowercase 64-character SHA-256 hex digest of the `.pmtiles` archive.
    pub sha256_hex: String,
    /// Canonical upstream SHA-256 hex digest over harvested raster tiles.
    pub upstream_sha256_hex: String,
}

/// Per-series raster chart edition manifest (`chart_<authority>.json`) written alongside a raster `.pmtiles`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PublishedChartManifest {
    /// Manifest schema version (`1`).
    pub schema_version: u32,
    /// Mandatory non-navigational disclaimer.
    pub disclaimer: String,
    /// Edition identifier (`"flightbag-faa_vfr_sectional-YYYY-MM-DD"`).
    pub edition_id: String,
    /// Authority slug (`"faa_vfr_sectional"`, etc.).
    pub authority: String,
    /// Governing 4-digit AIRAC code at the start of the 56-day chart window.
    pub airac_code: String,
    /// Cycle index from AIRAC `2001` (`2020-01-02`).
    pub cycle_index: i32,
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// Expiration date (`YYYY-MM-DD`, 56 days after `effective_date`).
    pub expires_date: String,
    /// Inclusive UTC activation timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_from_utc: String,
    /// Exclusive UTC expiration timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_until_utc: String,
    /// Official FAA upstream archive URL for the 56-day edition.
    pub upstream_url: String,
    /// Canonical SHA-256 hex digest of the harvested upstream tile set.
    pub upstream_sha256_hex: String,
    /// Compiled raster `.pmtiles` artifact metadata.
    pub chart_pmtiles: PublishedArtifactRecord,
}

/// Rolling multi-cycle, multi-authority pointer manifest (`flightbag-manifest/manifest.json`),
/// modeled after `v99n62`'s `navdata-manifest/manifest.json` so offline/EFB/simulator clients can
/// pre-stage upcoming editions before cutover and verify every download with SHA-256.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RollingManifest {
    /// Manifest schema version (`2`).
    pub schema_version: u32,
    /// Mandatory safety disclaimer marking all entries as non-navigational.
    pub disclaimer: String,
    /// Import parameters and cycle directories behind the entries' `ipfs_cid`, or `None` when
    /// no listed artifact has a recorded CID.
    #[serde(default)]
    pub ipfs: Option<IpfsPublication>,
    /// Published artifact entries sorted deterministically by `(authority, effective_from_utc)`.
    pub entries: Vec<RollingEntry>,
}

/// One downloadable cycle-stamped `PMTiles` entry in a [`RollingManifest`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RollingEntry {
    /// Authority slug (`"faa_nasr"`, `"faa_vfr_sectional"`, `"faa_vfr_terminal"`, `"faa_ifr_low"`, `"faa_ifr_high"`).
    pub authority: String,
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// Expiration date (`YYYY-MM-DD`).
    pub expires_date: String,
    /// Four-digit ICAO/FAA `YYCC` AIRAC code.
    pub airac_code: String,
    /// Inclusive UTC activation timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_from_utc: String,
    /// Exclusive UTC expiration timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_until_utc: String,
    /// Artifact filename (`"airspace.pmtiles"`, `"vfr_sectional.pmtiles"`, etc.).
    pub file: String,
    /// Primary download location: the Release asset, or for an artifact stored on IPFS only
    /// its verified gateway URL (`ipfs://<directory CID>/<file>` while no gateway is verified).
    pub url: String,
    /// Where the artifact bytes are published.
    #[serde(default)]
    pub storage: ArtifactStorage,
    /// CIDv1 of the artifact under the manifest's `ipfs.import` parameters.
    #[serde(default)]
    pub ipfs_cid: Option<String>,
    /// Further verified locations of the same bytes (HTTP `Range` and CORS).
    #[serde(default)]
    pub mirror_urls: Vec<String>,
    /// Exact byte length of the `.pmtiles` archive.
    pub size_bytes: u64,
    /// Lowercase 64-character SHA-256 hex digest of the `.pmtiles` archive.
    pub sha256_hex: String,
    /// Lowercase 64-character SHA-256 hex digest of the upstream FAA source dataset.
    pub upstream_sha256_hex: String,
    /// Minimum zoom level stored in the `.pmtiles` archive.
    pub min_zoom: u8,
    /// Maximum zoom level stored in the `.pmtiles` archive.
    pub max_zoom: u8,
    /// Geographic bounds `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
}

/// Writes `harvested` raster chart tiles into a Hilbert-ordered `PMTiles` v3 archive (plus `.sha256`
/// companion sidecar) at `output_pmtiles`.
///
/// # Errors
///
/// Returns [`ArchiveError`] if `output_pmtiles` cannot be created or finalized.
pub fn build_raster_chart_pmtiles(
    series: RasterChartSeries,
    cycle: &ResolvedChartCycle,
    harvested: &HarvestedRasterPyramid,
    output_pmtiles: &Path,
) -> Result<RasterArchiveStats, ArchiveError> {
    let bounds = (
        harvested.bounds[0],
        harvested.bounds[1],
        harvested.bounds[2],
        harvested.bounds[3],
    );
    let title = format!(
        "Flightbag {} ({} / AIRAC {} — Experimental Prototype, Not for Navigation or Flight)",
        series.title(),
        cycle.effective_date,
        cycle.airac_code
    );
    let mut all_tiles = harvested.tiles.clone();
    let synthesize_to_zero = matches!(
        series,
        RasterChartSeries::VfrSectional | RasterChartSeries::IfrLow | RasterChartSeries::IfrHigh
    );
    let effective_min_zoom = if synthesize_to_zero && harvested.max_zoom > 0 {
        let overview_base = 8_u8.min(harvested.max_zoom).max(harvested.min_zoom);
        crate::overview::synthesize_overview_pyramid(&mut all_tiles, overview_base, 0)?;
        0
    } else {
        harvested.min_zoom
    };
    let params = RasterArchiveParams {
        bounds,
        min_zoom: effective_min_zoom,
        max_zoom: harvested.max_zoom,
        title: &title,
        description: PROTOTYPE_DISCLAIMER,
        attribution: "FAA AeroNav (Experimental Prototype — Not for Navigation or Flight)",
        version: &cycle.airac_code,
    };
    let (size_bytes, sha256_hex) =
        write_raster_pmtiles_archive(&all_tiles, &params, output_pmtiles)?;

    Ok(RasterArchiveStats {
        authority: series.authority_slug().to_owned(),
        file: series.pmtiles_filename().to_owned(),
        tile_count: all_tiles.len(),
        min_zoom: effective_min_zoom,
        max_zoom: harvested.max_zoom,
        bounds: harvested.bounds,
        size_bytes,
        sha256_hex,
        upstream_sha256_hex: harvested.upstream_tiles_sha256_hex.clone(),
    })
}

/// Validates core [`EditionManifest`] provenance and writes `chart_<authority>.json` to `manifest_path`.
///
/// # Errors
///
/// Returns [`ArchiveError`] if digest validation, JSON formatting, or file writing fails.
pub fn write_chart_manifest(
    series: RasterChartSeries,
    cycle: &ResolvedChartCycle,
    stats: &RasterArchiveStats,
    manifest_path: &Path,
) -> Result<(EditionManifest, PublishedChartManifest), ArchiveError> {
    let chart_digest = SourceDigest::new(
        series.core_authority(),
        cycle.effective_date.clone(),
        &stats.upstream_sha256_hex,
    )
    .map_err(|err| ArchiveError::Json(err.to_string()))?;

    let edition_id = format!(
        "flightbag-{}-{}",
        series.authority_slug(),
        cycle.effective_date
    );
    let core_manifest = EditionManifest {
        schema_version: 1,
        edition_id: edition_id.clone(),
        airac: cycle.airac,
        sources: vec![chart_digest],
        signature_ed25519_hex: None,
    };

    let published = PublishedChartManifest {
        schema_version: 1,
        disclaimer: PROTOTYPE_DISCLAIMER.to_owned(),
        edition_id,
        authority: series.authority_slug().to_owned(),
        airac_code: cycle.airac_code.clone(),
        cycle_index: cycle.cycle_index,
        effective_date: cycle.effective_date.clone(),
        expires_date: cycle.expires_date.clone(),
        effective_from_utc: cycle.effective_from_utc.clone(),
        effective_until_utc: cycle.effective_until_utc.clone(),
        upstream_url: series.edition_bundle_url(&cycle.faa_folder_date),
        upstream_sha256_hex: stats.upstream_sha256_hex.clone(),
        chart_pmtiles: PublishedArtifactRecord {
            file: stats.file.clone(),
            size_bytes: stats.size_bytes,
            sha256_hex: stats.sha256_hex.clone(),
            tile_count: stats.tile_count,
            min_zoom: stats.min_zoom,
            max_zoom: stats.max_zoom,
            bounds: stats.bounds,
        },
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

/// Writes an offline-ready copy of [`AIRSPACE_VFR_SECTIONAL_STYLE_JSON`] to `style_path` with
/// `airspace`, `vfr_sectional`, and `vfr_terminal` pointed at local relative `pmtiles://./...` archives
/// (removing raster sources that are not yet present in `style_path`'s parent directory so partial
/// builds render cleanly without missing-source errors).
///
/// # Errors
///
/// Returns [`ArchiveError`] if the embedded style JSON cannot be rewritten or written to disk.
pub fn write_local_vfr_sectional_style(style_path: &Path) -> Result<(), ArchiveError> {
    let mut doc: serde_json::Value = serde_json::from_str(AIRSPACE_VFR_SECTIONAL_STYLE_JSON)
        .map_err(|e| ArchiveError::Json(e.to_string()))?;

    let parent_dir = style_path.parent().unwrap_or_else(|| Path::new("."));
    let has_sectional = parent_dir.join("vfr_sectional.pmtiles").is_file();
    let has_terminal = parent_dir.join("vfr_terminal.pmtiles").is_file();

    if let Some(url_slot) = doc.pointer_mut("/sources/airspace/url") {
        *url_slot = serde_json::Value::String("pmtiles://./airspace.pmtiles".to_owned());
    }
    if has_sectional {
        if let Some(url_slot) = doc.pointer_mut("/sources/vfr_sectional/url") {
            *url_slot = serde_json::Value::String("pmtiles://./vfr_sectional.pmtiles".to_owned());
        }
    } else {
        remove_style_source_and_layer(&mut doc, "vfr_sectional", "vfr-sectional-drape");
    }
    if has_terminal {
        if let Some(url_slot) = doc.pointer_mut("/sources/vfr_terminal/url") {
            *url_slot = serde_json::Value::String("pmtiles://./vfr_terminal.pmtiles".to_owned());
        }
    } else {
        remove_style_source_and_layer(&mut doc, "vfr_terminal", "vfr-terminal-drape");
    }

    if let Some(parent) = style_path.parent() {
        fs::create_dir_all(parent).map_err(|source| ArchiveError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut bytes =
        serde_json::to_vec_pretty(&doc).map_err(|e| ArchiveError::Json(e.to_string()))?;
    bytes.push(b'\n');
    fs::write(style_path, bytes).map_err(|source| ArchiveError::Io {
        path: style_path.to_path_buf(),
        source,
    })
}

fn remove_style_source_and_layer(doc: &mut serde_json::Value, source_id: &str, layer_id: &str) {
    if let Some(sources) = doc
        .get_mut("sources")
        .and_then(serde_json::Value::as_object_mut)
    {
        sources.remove(source_id);
    }
    if let Some(layers) = doc
        .get_mut("layers")
        .and_then(serde_json::Value::as_array_mut)
    {
        layers.retain(|l| l.get("id").and_then(serde_json::Value::as_str) != Some(layer_id));
    }
}

/// Scans cycle subdirectories under `cycles_dir` (`YYYY-MM-DD/manifest.json` and
/// `YYYY-MM-DD/chart_*.json`), expands `url_template` (`{date}` and `{file}` placeholders),
/// records CIDs and gateway locations from the cycles' IPFS companions as `ipfs` directs, and
/// writes a deterministic rolling [`RollingManifest`] to `out_path`.
///
/// # Errors
///
/// Returns [`ArchiveError`] if reading cycle directories or writing `out_path` fails.
pub fn build_rolling_manifest(
    cycles_dir: &Path,
    url_template: &str,
    ipfs: &IpfsOptions,
    out_path: &Path,
) -> Result<RollingManifest, ArchiveError> {
    let mut entries = Vec::new();
    let dir_iter = fs::read_dir(cycles_dir).map_err(|source| ArchiveError::Io {
        path: cycles_dir.to_path_buf(),
        source,
    })?;

    let mut subdirs = Vec::new();
    for item in dir_iter {
        let entry = item.map_err(|source| ArchiveError::Io {
            path: cycles_dir.to_path_buf(),
            source,
        })?;
        let p = entry.path();
        if p.is_dir() {
            let is_iso_date = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(looks_like_iso_date);
            if is_iso_date {
                subdirs.push(p);
            }
        }
    }
    subdirs.sort();

    for cycle_dir in subdirs {
        let nasr_manifest_path = cycle_dir.join("manifest.json");
        if nasr_manifest_path.is_file() {
            let bytes = fs::read(&nasr_manifest_path).map_err(|source| ArchiveError::Io {
                path: nasr_manifest_path.clone(),
                source,
            })?;
            let nasr: PublishedCycleManifest =
                serde_json::from_slice(&bytes).map_err(|e| ArchiveError::Json(e.to_string()))?;
            let url = expand_url_template(
                url_template,
                &nasr.effective_date,
                &nasr.airspace_pmtiles.file,
            );
            entries.push(RollingEntry {
                authority: "faa_nasr".to_owned(),
                effective_date: nasr.effective_date,
                expires_date: nasr.expires_date,
                airac_code: nasr.airac_code,
                effective_from_utc: nasr.effective_from_utc,
                effective_until_utc: nasr.effective_until_utc,
                file: nasr.airspace_pmtiles.file,
                url,
                size_bytes: nasr.airspace_pmtiles.size_bytes,
                sha256_hex: nasr.airspace_pmtiles.sha256_hex,
                upstream_sha256_hex: nasr.upstream_nasr.sha256_hex,
                min_zoom: nasr.airspace_pmtiles.min_zoom,
                max_zoom: nasr.airspace_pmtiles.max_zoom,
                bounds: nasr.airspace_pmtiles.bounds,
                storage: ArtifactStorage::Release,
                ipfs_cid: None,
                mirror_urls: Vec::new(),
            });
        }

        let cifp_manifest_path = cycle_dir.join("cifp_faa_cifp.json");
        if cifp_manifest_path.is_file() {
            let bytes = fs::read(&cifp_manifest_path).map_err(|source| ArchiveError::Io {
                path: cifp_manifest_path.clone(),
                source,
            })?;
            let cifp: PublishedCifpManifest =
                serde_json::from_slice(&bytes).map_err(|e| ArchiveError::Json(e.to_string()))?;
            let url =
                expand_url_template(url_template, &cifp.effective_date, &cifp.cifp_binary.file);
            entries.push(RollingEntry {
                authority: cifp.authority,
                effective_date: cifp.effective_date,
                expires_date: cifp.expires_date,
                airac_code: cifp.airac_code,
                effective_from_utc: cifp.effective_from_utc,
                effective_until_utc: cifp.effective_until_utc,
                file: cifp.cifp_binary.file,
                url,
                size_bytes: cifp.cifp_binary.size_bytes,
                sha256_hex: cifp.cifp_binary.sha256_hex,
                upstream_sha256_hex: cifp.upstream_sha256_hex,
                min_zoom: 0,
                max_zoom: 16,
                bounds: cifp.cifp_binary.bounds,
                storage: ArtifactStorage::Release,
                ipfs_cid: None,
                mirror_urls: Vec::new(),
            });
        }

        let files_iter = fs::read_dir(&cycle_dir).map_err(|source| ArchiveError::Io {
            path: cycle_dir.clone(),
            source,
        })?;
        let mut chart_files = Vec::new();
        for f_item in files_iter {
            let f_entry = f_item.map_err(|source| ArchiveError::Io {
                path: cycle_dir.clone(),
                source,
            })?;
            let fp = f_entry.path();
            if fp
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("chart_") && name.ends_with(".json"))
            {
                chart_files.push(fp);
            }
        }
        chart_files.sort();

        for chart_path in chart_files {
            let bytes = fs::read(&chart_path).map_err(|source| ArchiveError::Io {
                path: chart_path.clone(),
                source,
            })?;
            let chart: PublishedChartManifest =
                serde_json::from_slice(&bytes).map_err(|e| ArchiveError::Json(e.to_string()))?;
            let url = expand_url_template(
                url_template,
                &chart.effective_date,
                &chart.chart_pmtiles.file,
            );
            entries.push(RollingEntry {
                authority: chart.authority,
                effective_date: chart.effective_date,
                expires_date: chart.expires_date,
                airac_code: chart.airac_code,
                effective_from_utc: chart.effective_from_utc,
                effective_until_utc: chart.effective_until_utc,
                file: chart.chart_pmtiles.file,
                url,
                size_bytes: chart.chart_pmtiles.size_bytes,
                sha256_hex: chart.chart_pmtiles.sha256_hex,
                upstream_sha256_hex: chart.upstream_sha256_hex,
                min_zoom: chart.chart_pmtiles.min_zoom,
                max_zoom: chart.chart_pmtiles.max_zoom,
                bounds: chart.chart_pmtiles.bounds,
                storage: ArtifactStorage::Release,
                ipfs_cid: None,
                mirror_urls: Vec::new(),
            });
        }
    }

    entries.sort_by(|a, b| {
        a.authority
            .cmp(&b.authority)
            .then_with(|| a.effective_from_utc.cmp(&b.effective_from_utc))
    });

    let ipfs = annotate_entries(&mut entries, cycles_dir, ipfs)?;
    let manifest = RollingManifest {
        schema_version: 2,
        disclaimer: PROTOTYPE_DISCLAIMER.to_owned(),
        ipfs,
        entries,
    };

    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|source| ArchiveError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut json_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| ArchiveError::Json(e.to_string()))?;
    json_bytes.push(b'\n');
    fs::write(out_path, json_bytes).map_err(|source| ArchiveError::Io {
        path: out_path.to_path_buf(),
        source,
    })?;

    Ok(manifest)
}

/// Selects the entry for `authority` active at ISO-8601 UTC timestamp `now_iso_utc`
/// (`effective_from_utc <= now < effective_until_utc`), falling back to the most recent edition
/// that has already been effective when the manifest carries no active one.
///
/// A pre-staged edition whose `effective_from_utc` lies after `now_iso_utc` is never returned:
/// stale data is recognisable by its expiry, data used ahead of its effective time is not.
pub fn select_active_entry<'a>(
    manifest: &'a RollingManifest,
    authority: &str,
    now_iso_utc: &str,
) -> Option<&'a RollingEntry> {
    manifest
        .entries
        .iter()
        .filter(|e| e.authority == authority && e.effective_from_utc.as_str() <= now_iso_utc)
        .max_by(|a, b| a.effective_from_utc.cmp(&b.effective_from_utc))
}

fn expand_url_template(template: &str, effective_date: &str, file: &str) -> String {
    template
        .replace("{date}", effective_date)
        .replace("{file}", file)
}

fn looks_like_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.get(4) == Some(&b'-')
        && b.get(7) == Some(&b'-')
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use flightbag_nasr::charts::resolve_chart_cycle;
    use flightbag_nasr::cycles::CycleSpec;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn builds_raster_pmtiles_and_rolling_manifest_and_selects_active_entry() {
        let cycle_res = resolve_chart_cycle(CycleSpec::Current, datetime!(2026-11-12 12:00 UTC));
        assert!(cycle_res.is_ok());
        let Some(cycle) = cycle_res.ok() else {
            return;
        };

        let mut tiles = BTreeMap::new();
        // Minimal PNG signature header bytes for tile z10/301/385
        tiles.insert(
            (10, 301, 385),
            vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        );
        let harvested = HarvestedRasterPyramid {
            tiles,
            min_zoom: 10,
            max_zoom: 12,
            bounds: [-74.6, 40.35, -73.4, 41.15],
            upstream_tiles_sha256_hex:
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            total_tile_bytes: 8,
        };

        let root = std::env::temp_dir().join("flightbag_test_rolling_manifest");
        drop(fs::remove_dir_all(&root));
        let cycle_dir = root.join("2026-10-29");
        let pmtiles_path = cycle_dir.join("vfr_terminal.pmtiles");
        let chart_manifest_path = cycle_dir.join("chart_faa_vfr_terminal.json");

        let stats_res = build_raster_chart_pmtiles(
            RasterChartSeries::VfrTerminal,
            &cycle,
            &harvested,
            &pmtiles_path,
        );
        assert!(stats_res.is_ok());
        if let Ok(stats) = stats_res {
            assert_eq!(stats.tile_count, 1);
            assert!(cycle_dir.join("vfr_terminal.pmtiles.sha256").is_file());
            assert!(
                write_chart_manifest(
                    RasterChartSeries::VfrTerminal,
                    &cycle,
                    &stats,
                    &chart_manifest_path
                )
                .is_ok()
            );
        }

        let rolling_path = root.join("manifest.json");
        let rolling_res = build_rolling_manifest(
            &root,
            "https://github.com/luofang34/Flightbag/releases/download/airac-{date}/{file}",
            &IpfsOptions::default(),
            &rolling_path,
        );
        assert!(rolling_res.is_ok());
        if let Ok(rolling) = rolling_res {
            assert_eq!(rolling.entries.len(), 1);
            let picked = select_active_entry(&rolling, "faa_vfr_terminal", "2026-11-12T15:00:00Z");
            assert!(picked.is_some_and(|e| {
                e.url
                    == "https://github.com/luofang34/Flightbag/releases/download/airac-2026-10-29/vfr_terminal.pmtiles"
            }));
        }

        drop(fs::remove_dir_all(&root));
    }
}
