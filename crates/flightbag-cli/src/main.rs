//! Command-line compiler and cycle-sync utility for `Flightbag` 28-day NASR 3D airspace
//! and CIFP (`FAACIFP18`) editions, 56-day FAA Visual/Enroute raster chart `PMTiles`, and
//! rolling SHA-256 manifests.

mod index;
mod ipfs;

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use flightbag_nasr::charts::{
    RasterChartSeries, US_CHART_BOUNDS, harvest_raster_chart_tiles, probe_chart_series,
    resolve_chart_cycle,
};
use flightbag_nasr::cifp::{
    CifpCounts, cifp_url, compile_cifp_zip, fetch_cifp_zip, fingerprint_local_cifp_zip,
    write_cifp_pack,
};
use flightbag_nasr::cycles::{CycleSpec, now_utc, resolve_cycle};
use flightbag_nasr::fetch::{
    Transport, UreqTransport, fetch_nasr_zip, fingerprint_local_zip, probe_upstream_archive,
};
use flightbag_nasr::{GeoBbox, NasrAirspaceCounts, compile_nasr_airspace_zip};
use flightbag_tiles::{
    IpfsOptions, RollingManifest, build_airspace_pmtiles, build_raster_chart_pmtiles,
    build_rolling_manifest, select_active_entry, write_chart_manifest, write_cycle_manifest,
    write_local_vfr_sectional_style, write_local_vfr_style,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

const USAGE: &str = "\
EXPERIMENTAL PROTOTYPE — NOT FOR NAVIGATION OR FLIGHT.

Usage:
  flightbag plan [--now <YYYY-MM-DDTHH:MM:SSZ>]
  flightbag probe-nasr [--cycle <current|upcoming|YYYY-MM-DD>]
  flightbag probe-cifp [--cycle <current|upcoming|YYYY-MM-DD>]
  flightbag probe-charts [--cycle <current|upcoming|YYYY-MM-DD>] [--series <all|vfr-all|vfr-sectional|vfr-terminal|ifr-low|ifr-high>]
  flightbag build-nasr --out-dir <DIR> [--cycle <current|upcoming|YYYY-MM-DD>] [--cache-dir <DIR>] [--zip-path <FILE>] [--bbox <min_lon,min_lat,max_lon,max_lat>] [--skip-qa-floors]
  flightbag build-cifp --out-dir <DIR> [--cycle <current|upcoming|YYYY-MM-DD>] [--cache-dir <DIR>] [--zip-path <FILE>] [--bbox <min_lon,min_lat,max_lon,max_lat>] [--skip-qa-floors]
  flightbag build-charts --out-dir <DIR> [--series <vfr-all|vfr-sectional|vfr-terminal|ifr-low|ifr-high|all>] [--cycle <current|upcoming|YYYY-MM-DD>] [--cache-dir <DIR>] [--bbox <min_lon,min_lat,max_lon,max_lat>] [--max-zoom <Z>] [--threads <N>]
  flightbag manifest --cycles-dir <DIR> --url-template <TEMPLATE> --out <FILE> [--ipfs-gateway <URL>] [--ipfs-only <FILE>]...
  flightbag ipfs-cid [--write] <FILE>...
  flightbag ipfs-dir --cycle-dir <DIR> [--block-out <FILE>]
  flightbag index --manifest <FILE> --out <FILE> [--now <YYYY-MM-DDTHH:MM:SSZ>] [--site-max-bytes <N>]
  flightbag sync --manifest <URL_OR_FILE> --store-dir <DIR> [--authority <SLUG>]
";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    Help,
    Plan {
        now: Option<String>,
    },
    Index(index::IndexArgs),
    IpfsCid(ipfs::IpfsCidArgs),
    IpfsDir(ipfs::IpfsDirArgs),
    ProbeNasr {
        cycle: String,
    },
    ProbeCifp {
        cycle: String,
    },
    ProbeCharts {
        cycle: String,
        series: String,
    },
    BuildNasr {
        cycle: String,
        cache_dir: PathBuf,
        out_dir: PathBuf,
        zip_path: Option<PathBuf>,
        bbox: Option<String>,
        skip_qa_floors: bool,
    },
    BuildCifp {
        cycle: String,
        cache_dir: PathBuf,
        out_dir: PathBuf,
        zip_path: Option<PathBuf>,
        bbox: Option<String>,
        skip_qa_floors: bool,
    },
    BuildCharts {
        cycle: String,
        series: String,
        cache_dir: PathBuf,
        out_dir: PathBuf,
        bbox: Option<String>,
        max_zoom: Option<u8>,
        threads: usize,
    },
    Manifest {
        cycles_dir: PathBuf,
        url_template: String,
        ipfs: IpfsOptions,
        out_path: PathBuf,
    },
    Sync {
        manifest_source: String,
        store_dir: PathBuf,
        authority: Option<String>,
    },
}

#[derive(Debug, Error)]
enum CliError {
    #[error("{0}\n\n{USAGE}")]
    Usage(String),
    #[error(transparent)]
    Cycle(#[from] flightbag_nasr::NasrCycleError),
    #[error(transparent)]
    Fetch(#[from] flightbag_nasr::FetchError),
    #[error(transparent)]
    Compile(#[from] flightbag_nasr::NasrCompileError),
    #[error(transparent)]
    Cifp(#[from] flightbag_nasr::CifpCompileError),
    #[error(transparent)]
    Tiles(#[from] flightbag_tiles::ArchiveError),
    #[error("invalid --bbox '{0}'; expected min_lon,min_lat,max_lon,max_lat")]
    InvalidBbox(String),
    #[error("QA sanity floor failed: {0}")]
    QaFloor(String),
    #[error("sync integrity failure: {0}")]
    Sync(String),
    #[error("JSON output failed: {0}")]
    Json(String),
    #[error("content identifier failed: {0}")]
    Ipfs(String),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), CliError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args)? {
        Command::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Command::Plan { now } => run_plan(now.as_deref()),
        Command::Index(args) => index::run_index(&args, now_utc()),
        Command::IpfsCid(args) => ipfs::run_ipfs_cid(&args),
        Command::IpfsDir(args) => ipfs::run_ipfs_dir(&args),
        Command::ProbeNasr { cycle } => run_probe_nasr(&cycle),
        Command::ProbeCifp { cycle } => run_probe_cifp(&cycle),
        Command::ProbeCharts { cycle, series } => run_probe_charts(&cycle, &series),
        Command::BuildNasr {
            cycle,
            cache_dir,
            out_dir,
            zip_path,
            bbox,
            skip_qa_floors,
        } => run_build_nasr(
            &cycle,
            &cache_dir,
            &out_dir,
            zip_path.as_deref(),
            bbox.as_deref(),
            skip_qa_floors,
        ),
        Command::BuildCifp {
            cycle,
            cache_dir,
            out_dir,
            zip_path,
            bbox,
            skip_qa_floors,
        } => run_build_cifp(
            &cycle,
            &cache_dir,
            &out_dir,
            zip_path.as_deref(),
            bbox.as_deref(),
            skip_qa_floors,
        ),
        Command::BuildCharts {
            cycle,
            series,
            cache_dir,
            out_dir,
            bbox,
            max_zoom,
            threads,
        } => run_build_charts(
            &cycle,
            &series,
            &cache_dir,
            &out_dir,
            bbox.as_deref(),
            max_zoom,
            threads,
        ),
        Command::Manifest {
            cycles_dir,
            url_template,
            ipfs,
            out_path,
        } => {
            let manifest = build_rolling_manifest(&cycles_dir, &url_template, &ipfs, &out_path)?;
            let text = serde_json::to_string_pretty(&manifest)
                .map_err(|e| CliError::Json(e.to_string()))?;
            println!("{text}");
            Ok(())
        }
        Command::Sync {
            manifest_source,
            store_dir,
            authority,
        } => run_sync(&manifest_source, &store_dir, authority.as_deref()),
    }
}

fn run_plan(explicit_now: Option<&str>) -> Result<(), CliError> {
    let now = match explicit_now {
        Some(raw) => index::parse_now(raw)?,
        None => now_utc(),
    };
    let nasr_cur = resolve_cycle(CycleSpec::Current, now)?;
    let nasr_next = resolve_cycle(CycleSpec::Upcoming, now)?;
    let chart_cur = resolve_chart_cycle(CycleSpec::Current, now)?;
    let chart_next = resolve_chart_cycle(CycleSpec::Upcoming, now)?;

    let report = serde_json::json!({
        "now_utc": format_iso_utc(now),
        "faa_nasr_28_day": {
            "current": {
                "effective_date": nasr_cur.effective_date,
                "expires_date": nasr_cur.expires_date,
                "airac_code": nasr_cur.airac_code,
                "effective_from_utc": nasr_cur.effective_from_utc,
                "effective_until_utc": nasr_cur.effective_until_utc,
                "url": nasr_cur.nasr_url,
            },
            "upcoming": {
                "effective_date": nasr_next.effective_date,
                "expires_date": nasr_next.expires_date,
                "airac_code": nasr_next.airac_code,
                "effective_from_utc": nasr_next.effective_from_utc,
                "effective_until_utc": nasr_next.effective_until_utc,
                "url": nasr_next.nasr_url,
            }
        },
        "faa_cifp_28_day": {
            "current": {
                "effective_date": nasr_cur.effective_date,
                "expires_date": nasr_cur.expires_date,
                "airac_code": nasr_cur.airac_code,
                "effective_from_utc": nasr_cur.effective_from_utc,
                "effective_until_utc": nasr_cur.effective_until_utc,
                "url": cifp_url(&nasr_cur),
            },
            "upcoming": {
                "effective_date": nasr_next.effective_date,
                "expires_date": nasr_next.expires_date,
                "airac_code": nasr_next.airac_code,
                "effective_from_utc": nasr_next.effective_from_utc,
                "effective_until_utc": nasr_next.effective_until_utc,
                "url": cifp_url(&nasr_next),
            }
        },
        "faa_charts_56_day": {
            "current": chart_cur,
            "upcoming": chart_next,
        }
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| CliError::Json(e.to_string()))?;
    println!("{text}");
    Ok(())
}

fn run_probe_nasr(cycle: &str) -> Result<(), CliError> {
    let spec = CycleSpec::parse(cycle)?;
    let resolved = resolve_cycle(spec, now_utc())?;
    let transport = UreqTransport::new();
    let probe = probe_upstream_archive(&transport, &resolved.nasr_url)?;
    let report = serde_json::json!({
        "cycle": resolved.effective_date,
        "expires_date": resolved.expires_date,
        "airac_code": resolved.airac_code,
        "effective_from_utc": resolved.effective_from_utc,
        "effective_until_utc": resolved.effective_until_utc,
        "url": resolved.nasr_url,
        "published": probe.published,
        "upstream": probe.validators,
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| CliError::Json(e.to_string()))?;
    println!("{text}");
    Ok(())
}

fn run_probe_cifp(cycle: &str) -> Result<(), CliError> {
    let spec = CycleSpec::parse(cycle)?;
    let resolved = resolve_cycle(spec, now_utc())?;
    let transport = UreqTransport::new();
    let probe = probe_upstream_archive(&transport, &cifp_url(&resolved))?;
    let report = serde_json::json!({
        "cycle": resolved.effective_date,
        "expires_date": resolved.expires_date,
        "airac_code": resolved.airac_code,
        "effective_from_utc": resolved.effective_from_utc,
        "effective_until_utc": resolved.effective_until_utc,
        "url": probe.url,
        "published": probe.published,
        "upstream": probe.validators,
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| CliError::Json(e.to_string()))?;
    println!("{text}");
    Ok(())
}

fn run_probe_charts(cycle: &str, series_raw: &str) -> Result<(), CliError> {
    let spec = CycleSpec::parse(cycle)?;
    let resolved = resolve_chart_cycle(spec, now_utc())?;
    let series_list = parse_series_list(series_raw)?;
    let transport = UreqTransport::new();
    let mut reports = Vec::with_capacity(series_list.len());
    for s in series_list {
        reports.push(probe_chart_series(&transport, &resolved, s)?);
    }
    let text = serde_json::to_string_pretty(&reports).map_err(|e| CliError::Json(e.to_string()))?;
    println!("{text}");
    Ok(())
}

fn run_build_nasr(
    cycle: &str,
    cache_dir: &Path,
    out_dir: &Path,
    zip_path: Option<&Path>,
    bbox: Option<&str>,
    skip_qa_floors: bool,
) -> Result<(), CliError> {
    let spec = CycleSpec::parse(cycle)?;
    let resolved = resolve_cycle(spec, now_utc())?;
    let bbox_filter = match bbox {
        Some(raw) => Some(parse_bbox(raw)?),
        None => None,
    };

    let nasr_fp = match zip_path {
        Some(local_zip) => fingerprint_local_zip(&resolved, local_zip)?,
        None => {
            let transport = UreqTransport::new();
            fetch_nasr_zip(&transport, &resolved, cache_dir)?
        }
    };

    eprintln!(
        "==> Compiling NASR {} (AIRAC {}) from {} ({} bytes, cached={})",
        resolved.effective_date,
        resolved.airac_code,
        nasr_fp.path.display(),
        nasr_fp.size_bytes,
        nasr_fp.cached
    );

    let compiled = compile_nasr_airspace_zip(&nasr_fp.path, bbox_filter)?;
    if bbox_filter.is_none() && !skip_qa_floors {
        verify_national_qa_floors(&compiled.counts)?;
    }

    let pmtiles_path = out_dir.join("airspace.pmtiles");
    let manifest_path = out_dir.join("manifest.json");
    let vfr_style_path = out_dir.join("airspace_vfr.json");
    let vfr_sec_style_path = out_dir.join("airspace_vfr_sectional.json");
    let stats = build_airspace_pmtiles(&compiled.volumes, &resolved, &pmtiles_path)?;
    let (_core_manifest, published) = write_cycle_manifest(
        &resolved,
        &nasr_fp,
        &compiled.counts,
        &stats,
        &manifest_path,
    )?;
    write_local_vfr_style(&vfr_style_path)?;
    write_local_vfr_sectional_style(&vfr_sec_style_path)?;

    eprintln!(
        "==> Wrote {} ({} tiles, {} bytes), {}, and {}",
        pmtiles_path.display(),
        stats.tile_count,
        stats.size_bytes,
        manifest_path.display(),
        vfr_style_path.display()
    );
    let summary =
        serde_json::to_string_pretty(&published).map_err(|e| CliError::Json(e.to_string()))?;
    println!("{summary}");
    Ok(())
}

fn run_build_cifp(
    cycle: &str,
    cache_dir: &Path,
    out_dir: &Path,
    zip_path: Option<&Path>,
    bbox: Option<&str>,
    skip_qa_floors: bool,
) -> Result<(), CliError> {
    let spec = CycleSpec::parse(cycle)?;
    let resolved = resolve_cycle(spec, now_utc())?;
    let bbox_filter = match bbox {
        Some(raw) => Some(parse_bbox(raw)?),
        None => None,
    };

    let cifp_fp = match zip_path {
        Some(local_zip) => fingerprint_local_cifp_zip(&resolved, local_zip)?,
        None => {
            let transport = UreqTransport::new();
            fetch_cifp_zip(&transport, &resolved, cache_dir)?
        }
    };

    eprintln!(
        "==> Compiling CIFP {} (AIRAC {}) from {} ({} bytes, cached={})",
        resolved.effective_date,
        resolved.airac_code,
        cifp_fp.path.display(),
        cifp_fp.size_bytes,
        cifp_fp.cached
    );

    let compiled = compile_cifp_zip(&cifp_fp.path, &resolved.airac_code, bbox_filter)?;
    if bbox_filter.is_none() && !skip_qa_floors {
        verify_national_cifp_qa_floors(&compiled.counts)?;
    }

    let bin_path = out_dir.join("cifp_procedures.bin");
    let manifest_path = out_dir.join("cifp_faa_cifp.json");
    let (_core_manifest, published) =
        write_cifp_pack(&compiled, &resolved, &cifp_fp, &bin_path, &manifest_path)?;

    eprintln!(
        "==> Wrote {} ({} bytes: {} airways, {} SIDs, {} STARs, {} IAPs) and {}",
        bin_path.display(),
        published.cifp_binary.size_bytes,
        compiled.counts.airways,
        compiled.counts.sids,
        compiled.counts.stars,
        compiled.counts.approaches,
        manifest_path.display()
    );
    let summary =
        serde_json::to_string_pretty(&published).map_err(|e| CliError::Json(e.to_string()))?;
    println!("{summary}");
    Ok(())
}

fn run_build_charts(
    cycle: &str,
    series_raw: &str,
    cache_dir: &Path,
    out_dir: &Path,
    bbox: Option<&str>,
    max_zoom: Option<u8>,
    threads: usize,
) -> Result<(), CliError> {
    let spec = CycleSpec::parse(cycle)?;
    let resolved = resolve_chart_cycle(spec, now_utc())?;
    let series_list = parse_series_list(series_raw)?;
    let target_bboxes = match bbox {
        Some(raw) => parse_bboxes(raw)?,
        None => vec![US_CHART_BOUNDS],
    };

    let transport = UreqTransport::new();
    let mut built_manifests = Vec::with_capacity(series_list.len());
    for series in series_list {
        let probe = probe_chart_series(&transport, &resolved, series)?;
        if !probe.archive_published {
            return Err(CliError::Sync(format!(
                "FAA 56-day {} edition {} ({}) is not yet published at {}",
                series.authority_slug(),
                resolved.effective_date,
                resolved.airac_code,
                probe.archive_probe_url
            )));
        }

        eprintln!(
            "==> Harvesting {} ({}..{}, {} region(s))...",
            series.title(),
            resolved.effective_date,
            resolved.expires_date,
            target_bboxes.len(),
        );
        let harvested = harvest_raster_chart_tiles(
            &resolved,
            series,
            &target_bboxes,
            max_zoom,
            cache_dir,
            threads,
        )?;
        let out_pmtiles = out_dir.join(series.pmtiles_filename());
        let out_manifest = out_dir.join(format!("chart_{}.json", series.authority_slug()));
        let stats = build_raster_chart_pmtiles(series, &resolved, &harvested, &out_pmtiles)?;
        let (_core, published) = write_chart_manifest(series, &resolved, &stats, &out_manifest)?;
        eprintln!(
            "==> Wrote {} ({} tiles, {} bytes) and {}",
            out_pmtiles.display(),
            stats.tile_count,
            stats.size_bytes,
            out_manifest.display()
        );
        built_manifests.push(published);
    }

    if out_dir.join("airspace.pmtiles").is_file() {
        write_local_vfr_sectional_style(&out_dir.join("airspace_vfr_sectional.json"))?;
    }

    let text = serde_json::to_string_pretty(&built_manifests)
        .map_err(|e| CliError::Json(e.to_string()))?;
    println!("{text}");
    Ok(())
}

fn run_sync(
    manifest_source: &str,
    store_dir: &Path,
    authority_filter: Option<&str>,
) -> Result<(), CliError> {
    let transport = UreqTransport::new();
    let manifest_bytes = if manifest_source.starts_with("http://")
        || manifest_source.starts_with("https://")
    {
        fetch_bytes(&transport, manifest_source)?
    } else {
        fs::read(manifest_source).map_err(|e| CliError::Sync(format!("{manifest_source}: {e}")))?
    };
    let rolling: RollingManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| CliError::Json(e.to_string()))?;

    let now_iso = format_iso_utc(now_utc());
    let mut authorities: Vec<&str> = rolling
        .entries
        .iter()
        .map(|e| e.authority.as_str())
        .collect();
    authorities.sort_unstable();
    authorities.dedup();

    fs::create_dir_all(store_dir)
        .map_err(|e| CliError::Sync(format!("{}: {e}", store_dir.display())))?;

    let mut synced = Vec::new();
    for auth in authorities {
        if let Some(filter) = authority_filter
            && auth != filter
        {
            continue;
        }
        let Some(entry) = select_active_entry(&rolling, auth, &now_iso) else {
            continue;
        };
        let Some(location) = ipfs::sync_location(entry) else {
            synced.push(serde_json::json!({
                "authority": entry.authority,
                "effective_date": entry.effective_date,
                "skipped": "no HTTP location is published for this edition",
            }));
            continue;
        };
        let dest = store_dir.join(&entry.effective_date).join(&entry.file);
        if dest.is_file()
            && let Ok(existing) = fs::read(&dest)
            && existing.len() as u64 == entry.size_bytes
            && format!("{:x}", Sha256::digest(&existing)) == entry.sha256_hex
        {
            synced.push(serde_json::json!({
                "authority": entry.authority,
                "effective_date": entry.effective_date,
                "path": dest,
                "cached": true,
            }));
            continue;
        }

        let payload = if location.starts_with("http://") || location.starts_with("https://") {
            fetch_bytes(&transport, location)?
        } else {
            fs::read(location).map_err(|e| CliError::Sync(format!("{location}: {e}")))?
        };
        if payload.len() as u64 != entry.size_bytes {
            return Err(CliError::Sync(format!(
                "{} byte length mismatch: expected {}, got {}",
                entry.file,
                entry.size_bytes,
                payload.len()
            )));
        }
        let actual_sha = format!("{:x}", Sha256::digest(&payload));
        if actual_sha != entry.sha256_hex {
            return Err(CliError::Sync(format!(
                "{} SHA-256 mismatch: expected {}, got {}",
                entry.file, entry.sha256_hex, actual_sha
            )));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| CliError::Sync(format!("{}: {e}", parent.display())))?;
        }
        let part = dest.with_file_name(format!("{}.part", entry.file));
        fs::write(&part, &payload)
            .map_err(|e| CliError::Sync(format!("{}: {e}", part.display())))?;
        fs::rename(&part, &dest).map_err(|e| CliError::Sync(format!("{}: {e}", dest.display())))?;
        synced.push(serde_json::json!({
            "authority": entry.authority,
            "effective_date": entry.effective_date,
            "path": dest,
            "cached": false,
        }));
    }

    let text = serde_json::to_string_pretty(&synced).map_err(|e| CliError::Json(e.to_string()))?;
    println!("{text}");
    Ok(())
}

fn fetch_bytes(transport: &dyn Transport, url: &str) -> Result<Vec<u8>, CliError> {
    let resp = transport.get_blocking(url, None)?;
    if resp.status != 200 {
        return Err(CliError::Sync(format!("HTTP {} for {url}", resp.status)));
    }
    let mut out = Vec::new();
    let mut reader = resp.body;
    reader
        .read_to_end(&mut out)
        .map_err(|e| CliError::Sync(format!("read {url}: {e}")))?;
    Ok(out)
}

fn format_iso_utc(dt: time::OffsetDateTime) -> String {
    let u = dt.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        u.year(),
        u8::from(u.month()),
        u.day(),
        u.hour(),
        u.minute(),
        u.second()
    )
}

fn parse_series_list(raw: &str) -> Result<Vec<RasterChartSeries>, CliError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "all" => Ok(vec![
            RasterChartSeries::VfrSectional,
            RasterChartSeries::VfrTerminal,
            RasterChartSeries::IfrLow,
            RasterChartSeries::IfrHigh,
        ]),
        "vfr-all" | "vfr" => Ok(vec![
            RasterChartSeries::VfrSectional,
            RasterChartSeries::VfrTerminal,
        ]),
        other => {
            let mut out = Vec::new();
            for token in other.split(',') {
                let Some(s) = RasterChartSeries::parse_slug(token) else {
                    return Err(CliError::Usage(format!("unknown --series '{token}'")));
                };
                out.push(s);
            }
            if out.is_empty() {
                return Err(CliError::Usage("empty --series".into()));
            }
            Ok(out)
        }
    }
}

fn parse_args(args: &[String]) -> Result<Command, CliError> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Ok(Command::Help);
    };
    match sub {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "plan" => parse_plan(args),
        "index" => index::parse_index_cmd(args),
        "ipfs-cid" => ipfs::parse_ipfs_cid(args),
        "ipfs-dir" => ipfs::parse_ipfs_dir(args),
        "probe-nasr" => parse_probe_nasr(args),
        "probe-cifp" => parse_probe_cifp(args),
        "probe-charts" => parse_probe_charts(args),
        "build-nasr" => parse_build_nasr(args),
        "build-cifp" => parse_build_cifp(args),
        "build-charts" => parse_build_charts(args),
        "manifest" => parse_manifest_cmd(args),
        "sync" => parse_sync_cmd(args),
        other => Err(CliError::Usage(format!("unknown subcommand '{other}'"))),
    }
}

fn parse_plan(args: &[String]) -> Result<Command, CliError> {
    match args.get(1).map(String::as_str) {
        None => Ok(Command::Plan { now: None }),
        Some("-h" | "--help") => Ok(Command::Help),
        Some("--now") => {
            let val = args
                .get(2)
                .ok_or_else(|| CliError::Usage("missing value for --now".into()))?;
            Ok(Command::Plan {
                now: Some(val.clone()),
            })
        }
        Some(other) => Err(CliError::Usage(format!("unknown plan argument '{other}'"))),
    }
}

fn parse_probe_cifp(args: &[String]) -> Result<Command, CliError> {
    let mut cycle = "current".to_owned();
    let mut idx = 1_usize;
    while let Some(flag) = args.get(idx).map(String::as_str) {
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--cycle" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cycle".into()))?;
                cycle.clone_from(val);
                idx += 2;
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown probe-cifp argument '{other}'"
                )));
            }
        }
    }
    Ok(Command::ProbeCifp { cycle })
}

fn parse_probe_nasr(args: &[String]) -> Result<Command, CliError> {
    let mut cycle = "current".to_owned();
    let mut idx = 1_usize;
    while let Some(flag) = args.get(idx).map(String::as_str) {
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--cycle" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cycle".into()))?;
                cycle.clone_from(val);
                idx += 2;
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown probe-nasr argument '{other}'"
                )));
            }
        }
    }
    Ok(Command::ProbeNasr { cycle })
}

fn parse_probe_charts(args: &[String]) -> Result<Command, CliError> {
    let mut cycle = "current".to_owned();
    let mut series = "vfr-all".to_owned();
    let mut idx = 1_usize;
    while let Some(flag) = args.get(idx).map(String::as_str) {
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--cycle" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cycle".into()))?;
                cycle.clone_from(val);
                idx += 2;
            }
            "--series" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --series".into()))?;
                series.clone_from(val);
                idx += 2;
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown probe-charts argument '{other}'"
                )));
            }
        }
    }
    Ok(Command::ProbeCharts { cycle, series })
}

fn parse_build_nasr(args: &[String]) -> Result<Command, CliError> {
    let mut cycle = "current".to_owned();
    let mut cache_dir = PathBuf::from("data/cache");
    let mut out_dir: Option<PathBuf> = None;
    let mut zip_path: Option<PathBuf> = None;
    let mut bbox: Option<String> = None;
    let mut skip_qa_floors = false;

    let mut idx = 1_usize;
    while let Some(flag) = args.get(idx).map(String::as_str) {
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--cycle" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cycle".into()))?;
                cycle.clone_from(val);
                idx += 2;
            }
            "--cache-dir" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cache-dir".into()))?;
                cache_dir = PathBuf::from(val);
                idx += 2;
            }
            "--out-dir" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --out-dir".into()))?;
                out_dir = Some(PathBuf::from(val));
                idx += 2;
            }
            "--zip-path" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --zip-path".into()))?;
                zip_path = Some(PathBuf::from(val));
                idx += 2;
            }
            "--bbox" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --bbox".into()))?;
                bbox = Some(val.clone());
                idx += 2;
            }
            "--skip-qa-floors" => {
                skip_qa_floors = true;
                idx += 1;
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown build-nasr argument '{other}'"
                )));
            }
        }
    }
    let required_out =
        out_dir.ok_or_else(|| CliError::Usage("--out-dir <DIR> is required".into()))?;
    Ok(Command::BuildNasr {
        cycle,
        cache_dir,
        out_dir: required_out,
        zip_path,
        bbox,
        skip_qa_floors,
    })
}

fn parse_build_cifp(args: &[String]) -> Result<Command, CliError> {
    let mut cycle = "current".to_owned();
    let mut cache_dir = PathBuf::from("data/cache");
    let mut out_dir: Option<PathBuf> = None;
    let mut zip_path: Option<PathBuf> = None;
    let mut bbox: Option<String> = None;
    let mut skip_qa_floors = false;

    let mut idx = 1_usize;
    while let Some(flag) = args.get(idx).map(String::as_str) {
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--cycle" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cycle".into()))?;
                cycle.clone_from(val);
                idx += 2;
            }
            "--cache-dir" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cache-dir".into()))?;
                cache_dir = PathBuf::from(val);
                idx += 2;
            }
            "--out-dir" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --out-dir".into()))?;
                out_dir = Some(PathBuf::from(val));
                idx += 2;
            }
            "--zip-path" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --zip-path".into()))?;
                zip_path = Some(PathBuf::from(val));
                idx += 2;
            }
            "--bbox" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --bbox".into()))?;
                bbox = Some(val.clone());
                idx += 2;
            }
            "--skip-qa-floors" => {
                skip_qa_floors = true;
                idx += 1;
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown build-cifp argument '{other}'"
                )));
            }
        }
    }
    let required_out =
        out_dir.ok_or_else(|| CliError::Usage("--out-dir <DIR> is required".into()))?;
    Ok(Command::BuildCifp {
        cycle,
        cache_dir,
        out_dir: required_out,
        zip_path,
        bbox,
        skip_qa_floors,
    })
}

fn parse_build_charts(args: &[String]) -> Result<Command, CliError> {
    let mut cycle = "current".to_owned();
    let mut series = "vfr-all".to_owned();
    let mut cache_dir = PathBuf::from("data/cache");
    let mut out_dir: Option<PathBuf> = None;
    let mut bbox: Option<String> = None;
    let mut max_zoom: Option<u8> = None;
    let mut threads: usize = 8;

    let mut idx = 1_usize;
    while let Some(flag) = args.get(idx).map(String::as_str) {
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--cycle" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cycle".into()))?;
                cycle.clone_from(val);
                idx += 2;
            }
            "--series" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --series".into()))?;
                series.clone_from(val);
                idx += 2;
            }
            "--cache-dir" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cache-dir".into()))?;
                cache_dir = PathBuf::from(val);
                idx += 2;
            }
            "--out-dir" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --out-dir".into()))?;
                out_dir = Some(PathBuf::from(val));
                idx += 2;
            }
            "--bbox" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --bbox".into()))?;
                bbox = Some(val.clone());
                idx += 2;
            }
            "--max-zoom" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --max-zoom".into()))?;
                let z = val
                    .parse::<u8>()
                    .map_err(|_| CliError::Usage(format!("invalid --max-zoom '{val}'")))?;
                max_zoom = Some(z);
                idx += 2;
            }
            "--threads" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --threads".into()))?;
                threads = val
                    .parse::<usize>()
                    .map_err(|_| CliError::Usage(format!("invalid --threads '{val}'")))?;
                idx += 2;
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown build-charts argument '{other}'"
                )));
            }
        }
    }
    let required_out =
        out_dir.ok_or_else(|| CliError::Usage("--out-dir <DIR> is required".into()))?;
    Ok(Command::BuildCharts {
        cycle,
        series,
        cache_dir,
        out_dir: required_out,
        bbox,
        max_zoom,
        threads,
    })
}

fn parse_manifest_cmd(args: &[String]) -> Result<Command, CliError> {
    let mut cycles_dir: Option<PathBuf> = None;
    let mut url_template: Option<String> = None;
    let mut out_path: Option<PathBuf> = None;
    let mut ipfs = IpfsOptions::default();
    let mut idx = 1_usize;
    while let Some(flag) = args.get(idx).map(String::as_str) {
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--ipfs-gateway" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --ipfs-gateway".into()))?;
                ipfs.gateway = Some(val.clone());
                idx += 2;
            }
            "--ipfs-only" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --ipfs-only".into()))?;
                ipfs.ipfs_only_files.push(val.clone());
                idx += 2;
            }
            "--cycles-dir" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --cycles-dir".into()))?;
                cycles_dir = Some(PathBuf::from(val));
                idx += 2;
            }
            "--url-template" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --url-template".into()))?;
                url_template = Some(val.clone());
                idx += 2;
            }
            "--out" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --out".into()))?;
                out_path = Some(PathBuf::from(val));
                idx += 2;
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown manifest argument '{other}'"
                )));
            }
        }
    }
    Ok(Command::Manifest {
        cycles_dir: cycles_dir
            .ok_or_else(|| CliError::Usage("--cycles-dir <DIR> is required".into()))?,
        url_template: url_template
            .ok_or_else(|| CliError::Usage("--url-template <TEMPLATE> is required".into()))?,
        ipfs,
        out_path: out_path.ok_or_else(|| CliError::Usage("--out <FILE> is required".into()))?,
    })
}

fn parse_sync_cmd(args: &[String]) -> Result<Command, CliError> {
    let mut manifest_source: Option<String> = None;
    let mut store_dir: Option<PathBuf> = None;
    let mut authority: Option<String> = None;
    let mut idx = 1_usize;
    while let Some(flag) = args.get(idx).map(String::as_str) {
        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "--manifest" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --manifest".into()))?;
                manifest_source = Some(val.clone());
                idx += 2;
            }
            "--store-dir" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --store-dir".into()))?;
                store_dir = Some(PathBuf::from(val));
                idx += 2;
            }
            "--authority" => {
                let val = args
                    .get(idx + 1)
                    .ok_or_else(|| CliError::Usage("missing value for --authority".into()))?;
                authority = Some(val.clone());
                idx += 2;
            }
            other => {
                return Err(CliError::Usage(format!("unknown sync argument '{other}'")));
            }
        }
    }
    Ok(Command::Sync {
        manifest_source: manifest_source
            .ok_or_else(|| CliError::Usage("--manifest <URL_OR_FILE> is required".into()))?,
        store_dir: store_dir
            .ok_or_else(|| CliError::Usage("--store-dir <DIR> is required".into()))?,
        authority,
    })
}

fn parse_bboxes(raw: &str) -> Result<Vec<GeoBbox>, CliError> {
    let mut out = Vec::new();
    for part in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        out.push(parse_bbox(part)?);
    }
    if out.is_empty() {
        return Err(CliError::InvalidBbox(raw.to_owned()));
    }
    Ok(out)
}

fn parse_bbox(raw: &str) -> Result<GeoBbox, CliError> {
    let vals: Vec<f64> = raw
        .split(',')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .collect();
    let Some(&min_lon) = vals.first() else {
        return Err(CliError::InvalidBbox(raw.to_owned()));
    };
    let Some(&min_lat) = vals.get(1) else {
        return Err(CliError::InvalidBbox(raw.to_owned()));
    };
    let Some(&max_lon) = vals.get(2) else {
        return Err(CliError::InvalidBbox(raw.to_owned()));
    };
    let Some(&max_lat) = vals.get(3) else {
        return Err(CliError::InvalidBbox(raw.to_owned()));
    };
    if vals.len() != 4 || min_lon >= max_lon || min_lat >= max_lat {
        return Err(CliError::InvalidBbox(raw.to_owned()));
    }
    Ok(GeoBbox {
        min_lon,
        min_lat,
        max_lon,
        max_lat,
    })
}

fn verify_national_qa_floors(counts: &NasrAirspaceCounts) -> Result<(), CliError> {
    let checks = [
        ("Class B", counts.class_b, 180),
        ("Class C", counts.class_c, 220),
        ("Class D", counts.class_d, 500),
        ("Class E", counts.class_e, 3_500),
        ("SUA", counts.sua, 1_000),
    ];
    for (label, actual, min_expected) in checks {
        if actual < min_expected {
            return Err(CliError::QaFloor(format!(
                "{label} count {actual} < expected national floor {min_expected}"
            )));
        }
    }
    Ok(())
}

fn verify_national_cifp_qa_floors(counts: &CifpCounts) -> Result<(), CliError> {
    let checks = [
        ("CIFP Airports", counts.airports, 10_000),
        ("CIFP Airways", counts.airways, 900),
        ("CIFP SIDs", counts.sids, 500),
        ("CIFP STARs", counts.stars, 500),
        ("CIFP Approaches", counts.approaches, 8_000),
    ];
    for (label, actual, min_expected) in checks {
        if actual < min_expected {
            return Err(CliError::QaFloor(format!(
                "{label} count {actual} < expected national floor {min_expected}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plan_probe_build_manifest_and_sync_commands() {
        assert_eq!(
            parse_args(&["plan".into()]).ok(),
            Some(Command::Plan { now: None })
        );
        assert_eq!(
            parse_args(&["plan".into(), "--now".into(), "2026-11-26T09:01:00Z".into()]).ok(),
            Some(Command::Plan {
                now: Some("2026-11-26T09:01:00Z".into())
            })
        );

        let probe = parse_args(&["probe-nasr".into(), "--cycle".into(), "upcoming".into()]);
        assert_eq!(
            probe.ok(),
            Some(Command::ProbeNasr {
                cycle: "upcoming".into()
            })
        );

        let probe_cifp = parse_args(&["probe-cifp".into(), "--cycle".into(), "2026-10-29".into()]);
        assert_eq!(
            probe_cifp.ok(),
            Some(Command::ProbeCifp {
                cycle: "2026-10-29".into()
            })
        );

        let build_cifp = parse_args(&[
            "build-cifp".into(),
            "--cycle".into(),
            "2026-10-29".into(),
            "--out-dir".into(),
            "dist/cycles/2026-10-29".into(),
        ]);
        assert_eq!(
            build_cifp.ok(),
            Some(Command::BuildCifp {
                cycle: "2026-10-29".into(),
                cache_dir: PathBuf::from("data/cache"),
                out_dir: PathBuf::from("dist/cycles/2026-10-29"),
                zip_path: None,
                bbox: None,
                skip_qa_floors: false,
            })
        );

        let build_charts = parse_args(&[
            "build-charts".into(),
            "--series".into(),
            "vfr-all".into(),
            "--cycle".into(),
            "2026-10-29".into(),
            "--out-dir".into(),
            "dist/cycles/2026-10-29".into(),
            "--bbox".into(),
            "-74.6,40.35,-73.4,41.15".into(),
        ]);
        assert_eq!(
            build_charts.ok(),
            Some(Command::BuildCharts {
                cycle: "2026-10-29".into(),
                series: "vfr-all".into(),
                cache_dir: PathBuf::from("data/cache"),
                out_dir: PathBuf::from("dist/cycles/2026-10-29"),
                bbox: Some("-74.6,40.35,-73.4,41.15".into()),
                max_zoom: None,
                threads: 8,
            })
        );

        let manifest_cmd = parse_args(&[
            "manifest".into(),
            "--cycles-dir".into(),
            "dist/cycles".into(),
            "--url-template".into(),
            "https://example.com/airac-{date}/{file}".into(),
            "--out".into(),
            "dist/manifest.json".into(),
            "--ipfs-gateway".into(),
            "https://gateway.invalid".into(),
            "--ipfs-only".into(),
            "vfr_sectional.pmtiles".into(),
        ]);
        assert_eq!(
            manifest_cmd.ok(),
            Some(Command::Manifest {
                cycles_dir: PathBuf::from("dist/cycles"),
                url_template: "https://example.com/airac-{date}/{file}".into(),
                ipfs: IpfsOptions {
                    gateway: Some("https://gateway.invalid".into()),
                    ipfs_only_files: vec!["vfr_sectional.pmtiles".into()],
                },
                out_path: PathBuf::from("dist/manifest.json"),
            })
        );
    }
}
