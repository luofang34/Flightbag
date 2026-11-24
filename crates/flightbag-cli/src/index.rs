//! `flightbag index`: writes the static-site `cycles.json` from a rolling manifest at one instant.

use std::fs;
use std::path::PathBuf;

use flightbag_tiles::{RollingManifest, build_cycle_index, is_iso_utc_instant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{CliError, Command};

/// Largest artifact mirrored on the static site unless `--site-max-bytes` overrides it; GitHub
/// Pages rejects files of 100 MB and the whole site is capped near 1 GB.
pub const DEFAULT_SITE_MAX_BYTES: u64 = 90_000_000;

/// Parsed `flightbag index` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexArgs {
    /// Rolling manifest file to index.
    pub manifest_path: PathBuf,
    /// Output `cycles.json` path.
    pub out_path: PathBuf,
    /// Instant deciding `current` versus `upcoming`; wall clock when absent.
    pub now: Option<String>,
    /// Largest artifact given a site-relative path.
    pub site_max_bytes: u64,
}

/// Parses an explicit `--now` value into a UTC instant.
///
/// A publishing run resolves its instant once and hands it to every later step, so a run that
/// straddles a cutover cannot classify the same edition two ways.
pub fn parse_now(raw: &str) -> Result<OffsetDateTime, CliError> {
    if !is_iso_utc_instant(raw) {
        return Err(CliError::Usage(format!(
            "invalid --now '{raw}'; expected YYYY-MM-DDTHH:MM:SSZ"
        )));
    }
    OffsetDateTime::parse(raw, &Rfc3339)
        .map_err(|e| CliError::Usage(format!("invalid --now '{raw}': {e}")))
}

pub fn parse_index_cmd(args: &[String]) -> Result<Command, CliError> {
    let mut manifest_path: Option<PathBuf> = None;
    let mut out_path: Option<PathBuf> = None;
    let mut now: Option<String> = None;
    let mut site_max_bytes = DEFAULT_SITE_MAX_BYTES;
    let mut rest = args.iter().skip(1);
    while let Some(flag) = rest.next() {
        if matches!(flag.as_str(), "-h" | "--help") {
            return Ok(Command::Help);
        }
        let value = rest
            .next()
            .ok_or_else(|| CliError::Usage(format!("missing value for {flag}")))?;
        match flag.as_str() {
            "--manifest" => manifest_path = Some(PathBuf::from(value)),
            "--out" => out_path = Some(PathBuf::from(value)),
            "--now" => now = Some(value.clone()),
            "--site-max-bytes" => {
                site_max_bytes = value
                    .parse()
                    .map_err(|_| CliError::Usage(format!("invalid --site-max-bytes '{value}'")))?;
            }
            other => {
                return Err(CliError::Usage(format!("unknown index argument '{other}'")));
            }
        }
    }
    Ok(Command::Index(IndexArgs {
        manifest_path: manifest_path
            .ok_or_else(|| CliError::Usage("index requires --manifest <FILE>".into()))?,
        out_path: out_path.ok_or_else(|| CliError::Usage("index requires --out <FILE>".into()))?,
        now,
        site_max_bytes,
    }))
}

pub fn run_index(args: &IndexArgs, wall_clock: OffsetDateTime) -> Result<(), CliError> {
    let instant = match &args.now {
        Some(raw) => parse_now(raw)?,
        None => wall_clock,
    };
    let now_iso = crate::format_iso_utc(instant);
    let bytes = fs::read(&args.manifest_path)
        .map_err(|e| CliError::Sync(format!("{}: {e}", args.manifest_path.display())))?;
    let rolling: RollingManifest =
        serde_json::from_slice(&bytes).map_err(|e| CliError::Json(e.to_string()))?;

    let index = build_cycle_index(&rolling, &now_iso, args.site_max_bytes);
    let mut text =
        serde_json::to_string_pretty(&index).map_err(|e| CliError::Json(e.to_string()))?;
    text.push('\n');
    if let Some(parent) = args.out_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| CliError::Sync(format!("{}: {e}", parent.display())))?;
    }
    fs::write(&args.out_path, &text)
        .map_err(|e| CliError::Sync(format!("{}: {e}", args.out_path.display())))?;
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests;
