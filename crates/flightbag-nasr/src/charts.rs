//! 56-day FAA VFR Sectional, VFR Terminal Area (`TAC`), and IFR Enroute raster chart cycle
//! resolution, 1-byte publication probing, GeoTIFF archive caching, and sparse Web Mercator
//! `tilemap`-guided tile harvesting.

use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use flightbag_core::{AiracCycle, AuthoritySource};
use sha2::{Digest, Sha256};
use time::{Date, Duration, OffsetDateTime};

use crate::GeoBbox;
use crate::cycles::{
    AIRAC_EPOCH_DATE, CYCLE_DAYS, CycleSpec, NasrCycleError, airac_from_effective_date,
    format_iso_date, resolved_from_airac,
};
use crate::fetch::{FetchError, NasrArchiveFingerprint, Transport, UreqTransport};

const CHART_CYCLE_DAYS: i32 = CYCLE_DAYS * 2;
const FAA_ARCGIS_BASE: &str =
    "https://tiles.arcgis.com/tiles/ssFJjBXIUyZDrSYZ/arcgis/rest/services";
/// Maximum query dimension per `MapServer/tilemap` call (`<= 32` avoids oversize adjustments).
const TILEMAP_BLOCK: u32 = 32;
/// Esri Compact Cache v2 `.bundlx` stride (`128 x 128` tiles). Any `tilemap` request crossing a
/// `128`-tile boundary returns `"adjusted": true` truncated at the bundle seam.
const ESRI_BUNDLE_STRIDE: u32 = 128;
const MAX_MERCATOR_LAT: f64 = 85.051_128_78;
/// Automatic horizon safety margin (`~72 km`) added around regional `--bbox` requests so
/// oblique (`70°..89°` pitch) and cockpit views never clip into bare basemap tiles ahead of the nose.
const REGIONAL_HORIZON_PAD_DEG: f64 = 0.65;
const REGIONAL_MAX_SPAN_LON_DEG: f64 = 40.0;
const REGIONAL_MAX_SPAN_LAT_DEG: f64 = 25.0;

/// Default bounding box covering US 50 states + Puerto Rico/USVI (`[-179.9, 17.5, -64.0, 71.6]`).
pub const US_CHART_BOUNDS: GeoBbox = GeoBbox {
    min_lon: -179.9,
    min_lat: 17.5,
    max_lon: -64.0,
    max_lat: 71.6,
};

/// Official FAA Aeronautical Information Services raster chart series.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum RasterChartSeries {
    /// VFR Sectional Aeronautical Charts (`1:500,000`, `z8..=12`).
    VfrSectional,
    /// VFR Terminal Area Charts (`TAC`, `1:250,000`, `z10..=12`).
    VfrTerminal,
    /// IFR Enroute Low Altitude Charts (`z7..=11`).
    IfrLow,
    /// IFR Enroute High Altitude Charts (`z5..=9`).
    IfrHigh,
}

impl RasterChartSeries {
    /// Parses CLI slug (`"vfr-sectional"`, `"vfr-terminal"`, `"ifr-low"`, `"ifr-high"`).
    pub fn parse_slug(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "vfr-sectional" | "sectional" => Some(Self::VfrSectional),
            "vfr-terminal" | "tac" | "terminal" => Some(Self::VfrTerminal),
            "ifr-low" => Some(Self::IfrLow),
            "ifr-high" => Some(Self::IfrHigh),
            _ => None,
        }
    }

    /// Canonical CLI and manifest authority slug.
    pub const fn authority_slug(self) -> &'static str {
        match self {
            Self::VfrSectional => "faa_vfr_sectional",
            Self::VfrTerminal => "faa_vfr_terminal",
            Self::IfrLow => "faa_ifr_low",
            Self::IfrHigh => "faa_ifr_high",
        }
    }

    /// Corresponding core [`AuthoritySource`] variant.
    pub const fn core_authority(self) -> AuthoritySource {
        match self {
            Self::VfrSectional => AuthoritySource::FaaVfrSectional,
            Self::VfrTerminal => AuthoritySource::FaaVfrTerminal,
            Self::IfrLow => AuthoritySource::FaaIfrLow,
            Self::IfrHigh => AuthoritySource::FaaIfrHigh,
        }
    }

    /// Canonical output `.pmtiles` filename inside a cycle directory.
    pub const fn pmtiles_filename(self) -> &'static str {
        match self {
            Self::VfrSectional => "vfr_sectional.pmtiles",
            Self::VfrTerminal => "vfr_terminal.pmtiles",
            Self::IfrLow => "ifr_low.pmtiles",
            Self::IfrHigh => "ifr_high.pmtiles",
        }
    }

    /// Human-readable chart title.
    pub const fn title(self) -> &'static str {
        match self {
            Self::VfrSectional => "FAA VFR Sectional Charts",
            Self::VfrTerminal => "FAA VFR Terminal Area Charts (TAC)",
            Self::IfrLow => "FAA IFR Enroute Low Altitude Charts",
            Self::IfrHigh => "FAA IFR Enroute High Altitude Charts",
        }
    }

    /// Official FAA AeroNav ArcGIS MapServer service name.
    pub const fn mapserver_name(self) -> &'static str {
        match self {
            Self::VfrSectional => "VFR_Sectional",
            Self::VfrTerminal => "VFR_Terminal",
            Self::IfrLow => "IFR_AreaLow",
            Self::IfrHigh => "IFR_High",
        }
    }

    /// Default `(min_zoom, max_zoom)` LOD range published by FAA AeroNav.
    pub const fn zoom_range(self) -> (u8, u8) {
        match self {
            Self::VfrSectional => (8, 12),
            Self::VfrTerminal => (10, 12),
            Self::IfrLow => (7, 11),
            Self::IfrHigh => (5, 9),
        }
    }

    /// Representative `< 2 GB` FAA 56-day edition archive URL used for 1-byte HTTP `Range` probing.
    pub fn probe_archive_url(self, mm_dd_yyyy: &str) -> String {
        match self {
            Self::VfrSectional => {
                format!("https://aeronav.faa.gov/visual/{mm_dd_yyyy}/sectional-files/New_York.zip")
            }
            Self::VfrTerminal => {
                format!("https://aeronav.faa.gov/visual/{mm_dd_yyyy}/tac-files/New_York_TAC.zip")
            }
            Self::IfrLow => format!("https://aeronav.faa.gov/enroute/{mm_dd_yyyy}/enr_l01.zip"),
            Self::IfrHigh => format!("https://aeronav.faa.gov/enroute/{mm_dd_yyyy}/enr_h01.zip"),
        }
    }

    /// Official FAA bulk GeoTIFF `.zip` URL for the 56-day edition.
    pub fn edition_bundle_url(self, mm_dd_yyyy: &str) -> String {
        match self {
            Self::VfrSectional => {
                format!("https://aeronav.faa.gov/visual/{mm_dd_yyyy}/All_Files/Sectional.zip")
            }
            Self::VfrTerminal => {
                format!("https://aeronav.faa.gov/visual/{mm_dd_yyyy}/All_Files/Terminal.zip")
            }
            Self::IfrLow => format!("https://aeronav.faa.gov/enroute/{mm_dd_yyyy}/enr_l01.zip"),
            Self::IfrHigh => format!("https://aeronav.faa.gov/enroute/{mm_dd_yyyy}/enr_h01.zip"),
        }
    }

    /// Esri MapServer JSON metadata URL (`?f=json`).
    pub fn mapserver_info_url(self) -> String {
        format!(
            "{FAA_ARCGIS_BASE}/{}/MapServer?f=json",
            self.mapserver_name()
        )
    }

    /// Esri MapServer `tilemap` coverage bitmap URL (`{z}/{row}/{col}/{w}/{h}`).
    pub fn tilemap_url(self, zoom: u8, row: u32, col: u32, width: u32, height: u32) -> String {
        format!(
            "{FAA_ARCGIS_BASE}/{}/MapServer/tilemap/{zoom}/{row}/{col}/{width}/{height}?f=json",
            self.mapserver_name()
        )
    }

    /// Esri MapServer XYZ tile URL (note Esri's `{z}/{y}/{x}` row/col order).
    pub fn tile_url(self, zoom: u8, x: u32, y: u32) -> String {
        format!(
            "{FAA_ARCGIS_BASE}/{}/MapServer/tile/{zoom}/{y}/{x}",
            self.mapserver_name()
        )
    }
}

/// Resolved 56-day FAA Visual / Enroute chart edition cycle.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedChartCycle {
    /// Governing `Flightbag` [`AiracCycle`] at the start of the 56-day window.
    #[serde(skip, default = "default_airac")]
    pub airac: AiracCycle,
    /// Odd 28-day AIRAC cycle index (`87` = `2026-09-03`, `89` = `2026-10-29`, `91` = `2026-12-24`).
    pub cycle_index: i32,
    /// Four-digit ICAO/FAA `YYCC` code at edition activation.
    pub airac_code: String,
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// FAA AeroNav folder date (`MM-DD-YYYY`).
    pub faa_folder_date: String,
    /// Expiration date 56 days after `effective_date` (`YYYY-MM-DD`).
    pub expires_date: String,
    /// Inclusive UTC activation timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_from_utc: String,
    /// Exclusive UTC expiration timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_until_utc: String,
}

const fn default_airac() -> AiracCycle {
    AiracCycle::from_index(0)
}

/// Resolves the governing 56-day FAA Visual / Enroute chart edition for `spec` at `now_utc`.
///
/// FAA Visual (`Sectional`, `TAC`, `Helicopter`) and Enroute (`IFR Low/High`) charts publish on
/// every **odd** 28-day AIRAC index since `2020-01-02` (`index % 2 == 1`, e.g. `2026-09-03`,
/// `2026-10-29`, `2026-12-24`) and remain effective across two consecutive 28-day cycles (`56 days`).
///
/// # Errors
///
/// Returns [`NasrCycleError`] if an explicit date predates `2020-01-02` or is off the 28-day grid.
pub fn resolve_chart_cycle(
    spec: CycleSpec,
    now_utc: OffsetDateTime,
) -> Result<ResolvedChartCycle, NasrCycleError> {
    let base_airac = match spec {
        CycleSpec::Current => {
            snap_to_56_day_edition(AiracCycle::active_at_unix_seconds(now_utc.unix_timestamp()))
        }
        CycleSpec::Upcoming => {
            let cur = snap_to_56_day_edition(AiracCycle::active_at_unix_seconds(
                now_utc.unix_timestamp(),
            ));
            AiracCycle::from_index(cur.index().saturating_add(2))
        }
        CycleSpec::EffectiveDate(d) => snap_to_56_day_edition(airac_from_effective_date(d)?),
    };
    let resolved_28 = resolved_from_airac(base_airac)?;
    let effective_jd = AIRAC_EPOCH_DATE
        .to_julian_day()
        .checked_add(base_airac.index().saturating_mul(CYCLE_DAYS))
        .ok_or(NasrCycleError::Overflow)?;
    let effective = Date::from_julian_day(effective_jd).map_err(|_| NasrCycleError::Overflow)?;
    let expires = effective
        .checked_add(Duration::days(i64::from(CHART_CYCLE_DAYS)))
        .ok_or(NasrCycleError::Overflow)?;
    let expires_date = format_iso_date(expires);
    let faa_folder_date = format!(
        "{:02}-{:02}-{:04}",
        u8::from(effective.month()),
        effective.day(),
        effective.year()
    );

    Ok(ResolvedChartCycle {
        airac: base_airac,
        cycle_index: base_airac.index(),
        airac_code: resolved_28.airac_code,
        effective_from_utc: format!("{}T09:01:00Z", resolved_28.effective_date),
        effective_until_utc: format!("{expires_date}T09:01:00Z"),
        effective_date: resolved_28.effective_date,
        faa_folder_date,
        expires_date,
    })
}

fn snap_to_56_day_edition(airac: AiracCycle) -> AiracCycle {
    if airac.index().rem_euclid(2) == 1 {
        airac
    } else {
        AiracCycle::from_index((airac.index() - 1).max(1))
    }
}

/// Publication probe status for one FAA raster chart series.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChartProbeReport {
    /// Chart series slug (`"faa_vfr_sectional"`, etc.).
    pub authority: String,
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// Expiration date (`YYYY-MM-DD`).
    pub expires_date: String,
    /// Governing AIRAC code (`YYCC`).
    pub airac_code: String,
    /// Probed 56-day FAA GeoTIFF archive URL.
    pub archive_probe_url: String,
    /// Whether the 56-day GeoTIFF edition is live on `aeronav.faa.gov`.
    pub archive_published: bool,
    /// FAA ArcGIS MapServer `documentInfo.subject` string (when reachable).
    pub mapserver_subject: Option<String>,
}

/// Probes whether FAA has published `cycle`'s 56-day raster chart edition and inspects the live
/// MapServer `documentInfo.subject` metadata.
///
/// # Errors
///
/// Returns [`FetchError`] on network transport failure or unexpected server status.
pub fn probe_chart_series(
    transport: &dyn Transport,
    cycle: &ResolvedChartCycle,
    series: RasterChartSeries,
) -> Result<ChartProbeReport, FetchError> {
    let probe_url = series.probe_archive_url(&cycle.faa_folder_date);
    let resp = transport.get_blocking(&probe_url, Some("bytes=0-0"))?;
    let archive_published = match resp.status {
        200 | 206 => true,
        404 | 410 => false,
        status => {
            return Err(FetchError::Status {
                url: probe_url,
                status,
            });
        }
    };

    let mapserver_subject = fetch_json_value(transport, &series.mapserver_info_url())
        .ok()
        .and_then(|v| {
            v.pointer("/documentInfo/subject")
                .and_then(|s| s.as_str())
                .map(str::to_owned)
        });

    Ok(ChartProbeReport {
        authority: series.authority_slug().to_owned(),
        effective_date: cycle.effective_date.clone(),
        expires_date: cycle.expires_date.clone(),
        airac_code: cycle.airac_code.clone(),
        archive_probe_url: probe_url,
        archive_published,
        mapserver_subject,
    })
}

/// Harvested Web Mercator raster tiles and aggregate SHA-256 provenance for one chart series.
pub struct HarvestedRasterPyramid {
    /// Raw PNG/JPEG tile bytes keyed by `(zoom, x, y)`.
    pub tiles: BTreeMap<(u8, u32, u32), Vec<u8>>,
    /// Minimum zoom harvested.
    pub min_zoom: u8,
    /// Maximum zoom harvested.
    pub max_zoom: u8,
    /// Geographic bounding box `[min_lon, min_lat, max_lon, max_lat]`.
    pub bounds: [f64; 4],
    /// Canonical SHA-256 hex digest over sorted `(z, x, y, len, payload)` entries (upstream digest).
    pub upstream_tiles_sha256_hex: String,
    /// Total raw tile payload bytes.
    pub total_tile_bytes: u64,
}

/// Harvests non-empty Web Mercator raster tiles for `series` inside `bboxes` across
/// `min_zoom..=max_zoom_override`, guided by FAA's `MapServer/tilemap` sparse presence bitmaps
/// and backed by SHA-256-verified local tile caching under `cache_root`.
///
/// # Errors
///
/// Returns [`FetchError`] if `tilemap` metadata or tile downloads fail.
pub fn harvest_raster_chart_tiles(
    cycle: &ResolvedChartCycle,
    series: RasterChartSeries,
    bboxes: &[GeoBbox],
    max_zoom_override: Option<u8>,
    cache_root: &Path,
    worker_threads: usize,
) -> Result<HarvestedRasterPyramid, FetchError> {
    let effective_bboxes = if bboxes.is_empty() {
        std::slice::from_ref(&US_CHART_BOUNDS)
    } else {
        bboxes
    };
    let (default_min_z, default_max_z) = series.zoom_range();
    let min_zoom = default_min_z;
    let max_zoom = max_zoom_override.map_or(default_max_z, |z| z.clamp(min_zoom, default_max_z));

    let transport = UreqTransport::new();
    let mut coords = Vec::new();
    let mut covered_bboxes = Vec::with_capacity(effective_bboxes.len().saturating_add(1));
    // For VFR Sectional, IFR Low, and IFR High, always harvest the full 50-state + Caribbean
    // continental root grid (Alaska, CONUS, Hawaii, Puerto Rico/USVI) so whole-globe z0..8
    // overviews cover the entire United States even when regional high-zoom corridors are built.
    if !bboxes.is_empty() {
        let national_max_z: Option<u8> = match series {
            RasterChartSeries::VfrSectional => Some(min_zoom),
            RasterChartSeries::IfrLow => Some(8_u8.min(max_zoom)),
            RasterChartSeries::IfrHigh => Some(8_u8.min(max_zoom)),
            RasterChartSeries::VfrTerminal => None,
        };
        if let Some(nat_max) = national_max_z {
            let us_root = bbox_tile_range(US_CHART_BOUNDS, min_zoom);
            covered_bboxes.push(tile_range_geo_bounds(us_root, min_zoom));
            for z in min_zoom..=nat_max {
                let nat_range = subtree_tile_range(us_root, min_zoom, z);
                collect_present_tiles(&transport, series, z, nat_range, &mut coords)?;
            }
        }
        // Also harvest crisp z9 across all 48 contiguous states + PR so CONUS never drops to z8.
        if series == RasterChartSeries::VfrSectional && max_zoom >= 9 {
            let conus = GeoBbox {
                min_lon: -125.0,
                min_lat: 24.4,
                max_lon: -66.0,
                max_lat: 49.5,
            };
            let conus_root = bbox_tile_range(conus, min_zoom);
            let conus_z9 = subtree_tile_range(conus_root, min_zoom, 9);
            collect_present_tiles(&transport, series, 9, conus_z9, &mut coords)?;
        }
    }
    for &bbox in effective_bboxes {
        let padded = pad_regional_bbox_for_horizon(bbox);
        let root_range = bbox_tile_range(padded, min_zoom);
        covered_bboxes.push(tile_range_geo_bounds(root_range, min_zoom));
        for zoom in min_zoom..=max_zoom {
            let level_range = subtree_tile_range(root_range, min_zoom, zoom);
            collect_present_tiles(&transport, series, zoom, level_range, &mut coords)?;
        }
    }
    coords.sort_unstable();
    coords.dedup();

    let tile_cache_dir = cache_root
        .join("charts")
        .join(&cycle.effective_date)
        .join(series.authority_slug());
    fs::create_dir_all(&tile_cache_dir).map_err(|source| FetchError::Io {
        path: tile_cache_dir.clone(),
        source,
    })?;

    let threads = worker_threads.clamp(1, 12);
    let queue = Mutex::new(coords.into_iter());
    let results: Mutex<BTreeMap<(u8, u32, u32), Vec<u8>>> = Mutex::new(BTreeMap::new());
    let first_err: Mutex<Option<FetchError>> = Mutex::new(None);

    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let worker_http = UreqTransport::new();
                loop {
                    if first_err
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        break;
                    }
                    let next_coord = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .next();
                    let Some((z, x, y)) = next_coord else {
                        break;
                    };
                    match fetch_or_load_cached_tile(
                        &worker_http,
                        series,
                        (z, x, y),
                        &tile_cache_dir,
                    ) {
                        Ok(Some(bytes)) => {
                            results
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert((z, x, y), bytes);
                        }
                        Ok(None) => {}
                        Err(err) => {
                            let mut slot = first_err
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if slot.is_none() {
                                *slot = Some(err);
                            }
                            break;
                        }
                    }
                }
            });
        }
    });

    if let Some(err) = first_err
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Err(err);
    }

    let tiles = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut agg_hasher = Sha256::new();
    let mut total_tile_bytes: u64 = 0;
    for (&(z, x, y), payload) in &tiles {
        agg_hasher.update([z]);
        agg_hasher.update(x.to_le_bytes());
        agg_hasher.update(y.to_le_bytes());
        agg_hasher.update((payload.len() as u64).to_le_bytes());
        agg_hasher.update(payload);
        total_tile_bytes = total_tile_bytes.saturating_add(payload.len() as u64);
    }

    let union_bbox = covered_bboxes.iter().copied().fold(
        covered_bboxes.first().copied().unwrap_or(US_CHART_BOUNDS),
        |acc, b| GeoBbox {
            min_lon: acc.min_lon.min(b.min_lon),
            min_lat: acc.min_lat.min(b.min_lat),
            max_lon: acc.max_lon.max(b.max_lon),
            max_lat: acc.max_lat.max(b.max_lat),
        },
    );

    Ok(HarvestedRasterPyramid {
        tiles,
        min_zoom,
        max_zoom,
        bounds: [
            union_bbox.min_lon,
            union_bbox.min_lat,
            union_bbox.max_lon,
            union_bbox.max_lat,
        ],
        upstream_tiles_sha256_hex: format!("{:x}", agg_hasher.finalize()),
        total_tile_bytes,
    })
}

/// Downloads and SHA-256-caches an official FAA 56-day GeoTIFF `.zip` archive (such as
/// `sectional-files/New_York.zip` or `tac-files/New_York_TAC.zip`).
///
/// # Errors
///
/// Returns [`FetchError`] on HTTP or checksum verification failure.
pub fn fetch_chart_geotiff_zip(
    transport: &dyn Transport,
    cycle: &ResolvedChartCycle,
    visual_subfolder: &str,
    zip_filename: &str,
    cache_root: &Path,
) -> Result<NasrArchiveFingerprint, FetchError> {
    let url = format!(
        "https://aeronav.faa.gov/visual/{}/{visual_subfolder}/{zip_filename}",
        cycle.faa_folder_date
    );
    let dest_dir = cache_root
        .join("charts")
        .join(&cycle.effective_date)
        .join("geotiff");
    let dest = dest_dir.join(zip_filename);
    let sidecar = dest_dir.join(format!("{zip_filename}.sha256"));

    if dest.is_file() && sidecar.is_file() {
        let expected = fs::read_to_string(&sidecar)
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
        if actual != expected {
            return Err(FetchError::ChecksumMismatch {
                path: dest,
                actual,
                expected,
            });
        }
        if !bytes.is_empty() {
            return Ok(NasrArchiveFingerprint {
                cycle: cycle.effective_date.clone(),
                airac_code: cycle.airac_code.clone(),
                filename: zip_filename.to_owned(),
                url,
                sha256_hex: actual,
                size_bytes: bytes.len() as u64,
                cached: true,
                path: dest,
            });
        }
    }

    fs::create_dir_all(&dest_dir).map_err(|source| FetchError::Io {
        path: dest_dir.clone(),
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
    let part = dest_dir.join(format!("{zip_filename}.part"));
    fs::write(&part, &body).map_err(|source| FetchError::Io {
        path: part.clone(),
        source,
    })?;
    fs::rename(&part, &dest).map_err(|source| FetchError::Io {
        path: dest.clone(),
        source,
    })?;
    fs::write(&sidecar, format!("{sha256_hex}  {zip_filename}\n")).map_err(|source| {
        FetchError::Io {
            path: sidecar,
            source,
        }
    })?;

    Ok(NasrArchiveFingerprint {
        cycle: cycle.effective_date.clone(),
        airac_code: cycle.airac_code.clone(),
        filename: zip_filename.to_owned(),
        url,
        sha256_hex,
        size_bytes: body.len() as u64,
        cached: false,
        path: dest,
    })
}

fn collect_present_tiles(
    transport: &dyn Transport,
    series: RasterChartSeries,
    zoom: u8,
    (x0, y0, x1, y1): (u32, u32, u32, u32),
    out: &mut Vec<(u8, u32, u32)>,
) -> Result<(), FetchError> {
    let mut row = y0;
    while row <= y1 {
        let bundle_row_rem = ESRI_BUNDLE_STRIDE - (row % ESRI_BUNDLE_STRIDE);
        let req_h = (y1 - row + 1).min(TILEMAP_BLOCK).min(bundle_row_rem);
        let mut row_step = req_h;
        let mut col = x0;
        while col <= x1 {
            let bundle_col_rem = ESRI_BUNDLE_STRIDE - (col % ESRI_BUNDLE_STRIDE);
            let req_w = (x1 - col + 1).min(TILEMAP_BLOCK).min(bundle_col_rem);
            let url = series.tilemap_url(zoom, row, col, req_w, req_h);
            let doc = fetch_json_value(transport, &url)?;
            let loc_left = doc
                .pointer("/location/left")
                .and_then(serde_json::Value::as_u64)
                .map_or(col, |v| v as u32);
            let loc_top = doc
                .pointer("/location/top")
                .and_then(serde_json::Value::as_u64)
                .map_or(row, |v| v as u32);
            let loc_w = doc
                .pointer("/location/width")
                .and_then(serde_json::Value::as_u64)
                .map_or(req_w, |v| v as u32);
            let loc_h = doc
                .pointer("/location/height")
                .and_then(serde_json::Value::as_u64)
                .map_or(req_h, |v| v as u32);
            if let Some(arr) = doc.get("data").and_then(serde_json::Value::as_array)
                && loc_w > 0
            {
                for (idx, flag) in arr.iter().enumerate() {
                    if flag.as_u64() == Some(1) {
                        let idx_u32 = idx as u32;
                        let tx = loc_left.saturating_add(idx_u32 % loc_w);
                        let ty = loc_top.saturating_add(idx_u32 / loc_w);
                        if tx >= x0 && tx <= x1 && ty >= y0 && ty <= y1 {
                            out.push((zoom, tx, ty));
                        }
                    }
                }
            }
            let covered_end_row = loc_top.saturating_add(loc_h);
            let covered_rows = covered_end_row.saturating_sub(row).max(1);
            row_step = row_step.min(covered_rows);
            let next_col = loc_left.saturating_add(loc_w).max(col.saturating_add(1));
            col = next_col;
        }
        row = row.saturating_add(row_step.max(1));
    }
    Ok(())
}

fn fetch_or_load_cached_tile(
    transport: &dyn Transport,
    series: RasterChartSeries,
    (z, x, y): (u8, u32, u32),
    cache_root: &Path,
) -> Result<Option<Vec<u8>>, FetchError> {
    let z_dir = cache_root.join(format!("z{z}"));
    let tile_path = z_dir.join(format!("{x}_{y}.img"));
    let sha_path = z_dir.join(format!("{x}_{y}.sha256"));

    if tile_path.is_file() && sha_path.is_file() {
        let recorded = fs::read_to_string(&sha_path)
            .map_err(|source| FetchError::Io {
                path: sha_path.clone(),
                source,
            })?
            .trim()
            .to_ascii_lowercase();
        let bytes = fs::read(&tile_path).map_err(|source| FetchError::Io {
            path: tile_path.clone(),
            source,
        })?;
        let actual = format!("{:x}", Sha256::digest(&bytes));
        if actual == recorded && !bytes.is_empty() {
            return Ok(Some(bytes));
        }
    }

    fs::create_dir_all(&z_dir).map_err(|source| FetchError::Io {
        path: z_dir.clone(),
        source,
    })?;
    let url = series.tile_url(z, x, y);
    let resp = transport.get_blocking(&url, None)?;
    match resp.status {
        200 => {
            let mut reader = resp.body;
            let mut bytes = Vec::with_capacity(32 * 1024);
            reader
                .read_to_end(&mut bytes)
                .map_err(|source| FetchError::Io {
                    path: tile_path.clone(),
                    source,
                })?;
            if bytes.is_empty() {
                return Ok(None);
            }
            let digest = format!("{:x}", Sha256::digest(&bytes));
            let part_path = z_dir.join(format!("{x}_{y}.img.part"));
            fs::write(&part_path, &bytes).map_err(|source| FetchError::Io {
                path: part_path.clone(),
                source,
            })?;
            fs::rename(&part_path, &tile_path).map_err(|source| FetchError::Io {
                path: tile_path,
                source,
            })?;
            fs::write(&sha_path, format!("{digest}\n")).map_err(|source| FetchError::Io {
                path: sha_path,
                source,
            })?;
            Ok(Some(bytes))
        }
        404 => Ok(None),
        status => Err(FetchError::Status { url, status }),
    }
}

fn fetch_json_value(transport: &dyn Transport, url: &str) -> Result<serde_json::Value, FetchError> {
    let resp = transport.get_blocking(url, None)?;
    if resp.status != 200 {
        return Err(FetchError::Status {
            url: url.to_owned(),
            status: resp.status,
        });
    }
    let mut body = String::new();
    let mut reader = resp.body;
    reader
        .read_to_string(&mut body)
        .map_err(|err| FetchError::Network {
            url: url.to_owned(),
            reason: err.to_string(),
        })?;
    serde_json::from_str(&body).map_err(|err| FetchError::Network {
        url: url.to_owned(),
        reason: err.to_string(),
    })
}

fn bbox_tile_range(bbox: GeoBbox, zoom: u8) -> (u32, u32, u32, u32) {
    let n = 1_u32 << zoom;
    let nf = f64::from(n);
    let lon_to_x = |lon: f64| -> u32 {
        let u = ((lon.clamp(-180.0, 180.0) + 180.0) / 360.0) * nf;
        (u.floor() as i64).clamp(0, i64::from(n) - 1) as u32
    };
    let lat_to_y = |lat: f64| -> u32 {
        let r = lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT).to_radians();
        let v = (1.0 - (r.tan() + 1.0 / r.cos()).ln() / PI) * 0.5 * nf;
        (v.floor() as i64).clamp(0, i64::from(n) - 1) as u32
    };
    let x0 = lon_to_x(bbox.min_lon);
    let x1 = lon_to_x(bbox.max_lon);
    let y0 = lat_to_y(bbox.max_lat);
    let y1 = lat_to_y(bbox.min_lat);
    (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

fn pad_regional_bbox_for_horizon(bbox: GeoBbox) -> GeoBbox {
    let lon_span = (bbox.max_lon - bbox.min_lon).abs();
    let lat_span = (bbox.max_lat - bbox.min_lat).abs();
    if lon_span < REGIONAL_MAX_SPAN_LON_DEG && lat_span < REGIONAL_MAX_SPAN_LAT_DEG {
        GeoBbox {
            min_lon: (bbox.min_lon - REGIONAL_HORIZON_PAD_DEG).clamp(-179.99, 179.99),
            min_lat: (bbox.min_lat - REGIONAL_HORIZON_PAD_DEG)
                .clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT),
            max_lon: (bbox.max_lon + REGIONAL_HORIZON_PAD_DEG).clamp(-179.99, 179.99),
            max_lat: (bbox.max_lat + REGIONAL_HORIZON_PAD_DEG)
                .clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT),
        }
    } else {
        bbox
    }
}

fn subtree_tile_range(
    (rx0, ry0, rx1, ry1): (u32, u32, u32, u32),
    root_zoom: u8,
    zoom: u8,
) -> (u32, u32, u32, u32) {
    let shift = u32::from(zoom.saturating_sub(root_zoom));
    let scale = 1_u32.checked_shl(shift).unwrap_or(1);
    let x0 = rx0.saturating_mul(scale);
    let y0 = ry0.saturating_mul(scale);
    let x1 = rx1
        .saturating_add(1)
        .saturating_mul(scale)
        .saturating_sub(1);
    let y1 = ry1
        .saturating_add(1)
        .saturating_mul(scale)
        .saturating_sub(1);
    (x0, y0, x1, y1)
}

fn tile_range_geo_bounds((x0, y0, x1, y1): (u32, u32, u32, u32), zoom: u8) -> GeoBbox {
    let nf = f64::from(1_u32 << zoom);
    let x_to_lon = |x: u32| (f64::from(x) / nf) * 360.0 - 180.0;
    let y_to_lat = |y: u32| {
        let n = PI - (2.0 * PI * f64::from(y)) / nf;
        n.sinh().atan().to_degrees()
    };
    GeoBbox {
        min_lon: x_to_lon(x0).clamp(-180.0, 180.0),
        min_lat: y_to_lat(y1.saturating_add(1)).clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT),
        max_lon: x_to_lon(x1.saturating_add(1)).clamp(-180.0, 180.0),
        max_lat: y_to_lat(y0).clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn resolves_56_day_visual_chart_cycle_across_even_and_odd_airac_windows() {
        // 2026-11-12 is inside odd AIRAC 89 (2026-10-29): chart cycle = 2026-10-29 .. 2026-12-24.
        let cur = resolve_chart_cycle(CycleSpec::Current, datetime!(2026-11-12 12:00 UTC));
        assert!(cur.is_ok());
        if let Ok(c) = cur {
            assert_eq!(c.effective_date, "2026-10-29");
            assert_eq!(c.faa_folder_date, "10-29-2026");
            assert_eq!(c.expires_date, "2026-12-24");
        }

        // 2026-12-05 is inside even AIRAC 90 (2026-11-26): still governed by 2026-10-29 .. 2026-12-24!
        let mid = resolve_chart_cycle(CycleSpec::Current, datetime!(2026-12-05 12:00 UTC));
        assert!(mid.is_ok());
        if let Ok(c) = mid {
            assert_eq!(c.effective_date, "2026-10-29");
            assert_eq!(c.expires_date, "2026-12-24");
        }

        // Upcoming 56-day chart edition from 2026-11-12 is 2026-12-24 .. 2027-02-18!
        let next = resolve_chart_cycle(CycleSpec::Upcoming, datetime!(2026-11-12 12:00 UTC));
        assert!(next.is_ok());
        if let Ok(n) = next {
            assert_eq!(n.effective_date, "2026-12-24");
            assert_eq!(n.faa_folder_date, "12-24-2026");
            assert_eq!(n.expires_date, "2027-02-18");
        }
    }

    #[test]
    fn regional_bbox_harvest_maintains_complete_quadtree_subtrees_without_coarse_overhang() {
        let norfolk = GeoBbox {
            min_lon: -76.50,
            min_lat: 36.68,
            max_lon: -75.95,
            max_lat: 37.12,
        };
        let padded = pad_regional_bbox_for_horizon(norfolk);
        let root_z8 = bbox_tile_range(padded, 8);
        let leaf_z12 = subtree_tile_range(root_z8, 8, 12);
        let bounds = tile_range_geo_bounds(root_z8, 8);

        // Must cover well past 37.12 N and -75.94 W (eliminating the Norfolk horizon seam).
        assert!(bounds.max_lat > 37.9 && bounds.max_lon > -74.6);
        // Every z8 tile in root_z8 has its complete 16x16 z12 subtree included.
        assert_eq!(leaf_z12.0, root_z8.0 * 16);
        assert_eq!(leaf_z12.1, root_z8.1 * 16);
        assert_eq!(leaf_z12.2, (root_z8.2 + 1) * 16 - 1);
        assert_eq!(leaf_z12.3, (root_z8.3 + 1) * 16 - 1);
    }

    #[test]
    fn tilemap_collector_recovers_all_tiles_across_esri_128_bundle_adjustments() {
        struct EsriBundleMockTransport;
        impl Transport for EsriBundleMockTransport {
            fn get_blocking(
                &self,
                url: &str,
                _range: Option<&str>,
            ) -> Result<crate::fetch::TransportResponse, FetchError> {
                // Parse .../tilemap/{z}/{row}/{col}/{w}/{h}?f=json
                let trimmed = url.split('?').next().unwrap_or(url);
                let parts: Vec<&str> = trimmed.rsplitn(6, '/').collect();
                let req_h = parts
                    .first()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(1);
                let req_w = parts
                    .get(1)
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(1);
                let col = parts
                    .get(2)
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                let row = parts
                    .get(3)
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                // Simulate Esri Compact Cache v2 clamping at 128-tile AND 16-tile boundaries!
                let clamp_16_w = 16 - (col % 16);
                let clamp_16_h = 16 - (row % 16);
                let w = req_w.min(clamp_16_w);
                let h = req_h.min(clamp_16_h);
                let adjusted = w != req_w || h != req_h;
                let ones = vec!["1"; (w * h) as usize].join(",");
                let body = format!(
                    "{{\"adjusted\":{adjusted},\"location\":{{\"left\":{col},\"top\":{row},\"width\":{w},\"height\":{h}}},\"data\":[{ones}]}}"
                );
                Ok(crate::fetch::TransportResponse {
                    status: 200,
                    body: Box::new(std::io::Cursor::new(body.into_bytes())),
                    validators: crate::fetch::UpstreamValidators::default(),
                })
            }
        }

        let mut out = Vec::new();
        // Straddles row = 384 (New York 40.98 N z10 Esri bundle seam) and col = 304.
        let res = collect_present_tiles(
            &EsriBundleMockTransport,
            RasterChartSeries::VfrSectional,
            10,
            (296, 380, 315, 391),
            &mut out,
        );
        assert!(res.is_ok());
        out.sort_unstable();
        out.dedup();
        assert_eq!(out.len(), 20 * 12);
    }
}
