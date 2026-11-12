//! Command-line compiler for `Flightbag` 28-day NASR 3D airspace editions and `PMTiles` archives.

use std::path::PathBuf;
use std::process::ExitCode;

use flightbag_nasr::cycles::{CycleSpec, now_utc, resolve_cycle};
use flightbag_nasr::fetch::{
    UreqTransport, fetch_nasr_zip, fingerprint_local_zip, probe_nasr_published,
};
use flightbag_nasr::{GeoBbox, NasrAirspaceCounts, compile_nasr_airspace_zip};
use flightbag_tiles::{build_airspace_pmtiles, write_cycle_manifest};
use thiserror::Error;

const USAGE: &str = "\
EXPERIMENTAL PROTOTYPE — NOT FOR NAVIGATION OR FLIGHT.

Usage:
  flightbag probe-nasr [--cycle <current|upcoming|YYYY-MM-DD>]
  flightbag build-nasr --out-dir <DIR> [--cycle <current|upcoming|YYYY-MM-DD>] [--cache-dir <DIR>] [--zip-path <FILE>] [--bbox <min_lon,min_lat,max_lon,max_lat>] [--skip-qa-floors]
";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    Help,
    ProbeNasr {
        cycle: String,
    },
    BuildNasr {
        cycle: String,
        cache_dir: PathBuf,
        out_dir: PathBuf,
        zip_path: Option<PathBuf>,
        bbox: Option<String>,
        skip_qa_floors: bool,
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
    Tiles(#[from] flightbag_tiles::ArchiveError),
    #[error("invalid --bbox '{0}'; expected min_lon,min_lat,max_lon,max_lat")]
    InvalidBbox(String),
    #[error("QA sanity floor failed: {0}")]
    QaFloor(String),
    #[error("JSON output failed: {0}")]
    Json(String),
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
        Command::ProbeNasr { cycle } => {
            let spec = CycleSpec::parse(&cycle)?;
            let resolved = resolve_cycle(spec, now_utc())?;
            let transport = UreqTransport::new();
            let published = probe_nasr_published(&transport, &resolved)?;
            let report = serde_json::json!({
                "cycle": resolved.effective_date,
                "expires_date": resolved.expires_date,
                "airac_code": resolved.airac_code,
                "effective_from_utc": resolved.effective_from_utc,
                "effective_until_utc": resolved.effective_until_utc,
                "url": resolved.nasr_url,
                "published": published,
            });
            let text =
                serde_json::to_string_pretty(&report).map_err(|e| CliError::Json(e.to_string()))?;
            println!("{text}");
            Ok(())
        }
        Command::BuildNasr {
            cycle,
            cache_dir,
            out_dir,
            zip_path,
            bbox,
            skip_qa_floors,
        } => {
            let spec = CycleSpec::parse(&cycle)?;
            let resolved = resolve_cycle(spec, now_utc())?;
            let bbox_filter = match bbox {
                Some(ref raw) => Some(parse_bbox(raw)?),
                None => None,
            };

            let nasr_fp = match zip_path {
                Some(ref local_zip) => fingerprint_local_zip(&resolved, local_zip)?,
                None => {
                    let transport = UreqTransport::new();
                    fetch_nasr_zip(&transport, &resolved, &cache_dir)?
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
            let stats = build_airspace_pmtiles(&compiled.volumes, &resolved, &pmtiles_path)?;
            let (_core_manifest, published) = write_cycle_manifest(
                &resolved,
                &nasr_fp,
                &compiled.counts,
                &stats,
                &manifest_path,
            )?;

            eprintln!(
                "==> Wrote {} ({} tiles, {} bytes) + {}",
                pmtiles_path.display(),
                stats.tile_count,
                stats.size_bytes,
                manifest_path.display()
            );
            let summary = serde_json::to_string_pretty(&published)
                .map_err(|e| CliError::Json(e.to_string()))?;
            println!("{summary}");
            Ok(())
        }
    }
}

fn parse_args(args: &[String]) -> Result<Command, CliError> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Ok(Command::Help);
    };
    match sub {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "probe-nasr" => {
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
        "build-nasr" => {
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
                        let val = args.get(idx + 1).ok_or_else(|| {
                            CliError::Usage("missing value for --cache-dir".into())
                        })?;
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
                        let val = args.get(idx + 1).ok_or_else(|| {
                            CliError::Usage("missing value for --zip-path".into())
                        })?;
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
        other => Err(CliError::Usage(format!("unknown subcommand '{other}'"))),
    }
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
        ("Class B", counts.class_b, 300),
        ("Class C", counts.class_c, 300),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_and_build_commands() {
        let probe = parse_args(&["probe-nasr".into(), "--cycle".into(), "upcoming".into()]);
        assert_eq!(
            probe.ok(),
            Some(Command::ProbeNasr {
                cycle: "upcoming".into()
            })
        );

        let build = parse_args(&[
            "build-nasr".into(),
            "--cycle".into(),
            "2026-10-29".into(),
            "--cache-dir".into(),
            "data/cache".into(),
            "--out-dir".into(),
            "dist/cycles/2026-10-29".into(),
            "--skip-qa-floors".into(),
        ]);
        assert_eq!(
            build.ok(),
            Some(Command::BuildNasr {
                cycle: "2026-10-29".into(),
                cache_dir: PathBuf::from("data/cache"),
                out_dir: PathBuf::from("dist/cycles/2026-10-29"),
                zip_path: None,
                bbox: None,
                skip_qa_floors: true,
            })
        );
    }
}
