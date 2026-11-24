//! `flightbag ipfs-cid` and `flightbag ipfs-dir`: content identifiers of published artifacts
//! and of the directory that holds one cycle, computed without an IPFS node.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use flightbag_tiles::unixfs::{ImportParams, import_file};
use flightbag_tiles::{ArtifactCidRecord, CID_RECORD_SUFFIX, RollingEntry, read_cycle_directory};

use crate::{CliError, Command};

/// Parsed `flightbag ipfs-cid` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpfsCidArgs {
    /// Artifacts to identify.
    pub files: Vec<PathBuf>,
    /// Whether to write `<file>.cid.json` beside every artifact.
    pub write: bool,
}

/// Parsed `flightbag ipfs-dir` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpfsDirArgs {
    /// Cycle directory holding `*.cid.json` companions.
    pub cycle_dir: PathBuf,
    /// Where to write the encoded directory block for upload to a node.
    pub block_out: Option<PathBuf>,
}

pub fn parse_ipfs_cid(args: &[String]) -> Result<Command, CliError> {
    let mut files = Vec::new();
    let mut write = false;
    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--write" => write = true,
            flag if flag.starts_with("--") => {
                return Err(CliError::Usage(format!(
                    "unknown ipfs-cid argument '{flag}'"
                )));
            }
            file => files.push(PathBuf::from(file)),
        }
    }
    if files.is_empty() {
        return Err(CliError::Usage(
            "ipfs-cid requires at least one <FILE>".into(),
        ));
    }
    Ok(Command::IpfsCid(IpfsCidArgs { files, write }))
}

pub fn parse_ipfs_dir(args: &[String]) -> Result<Command, CliError> {
    let mut cycle_dir: Option<PathBuf> = None;
    let mut block_out: Option<PathBuf> = None;
    let mut rest = args.iter().skip(1);
    while let Some(flag) = rest.next() {
        if matches!(flag.as_str(), "-h" | "--help") {
            return Ok(Command::Help);
        }
        let value = rest
            .next()
            .ok_or_else(|| CliError::Usage(format!("missing value for {flag}")))?;
        match flag.as_str() {
            "--cycle-dir" => cycle_dir = Some(PathBuf::from(value)),
            "--block-out" => block_out = Some(PathBuf::from(value)),
            other => {
                return Err(CliError::Usage(format!(
                    "unknown ipfs-dir argument '{other}'"
                )));
            }
        }
    }
    Ok(Command::IpfsDir(IpfsDirArgs {
        cycle_dir: cycle_dir
            .ok_or_else(|| CliError::Usage("ipfs-dir requires --cycle-dir <DIR>".into()))?,
        block_out,
    }))
}

/// Identifies one artifact; the record carries its SHA-256 so a consumer can tell which bytes
/// the CID was computed from.
pub fn identify(path: &Path) -> Result<ArtifactCidRecord, CliError> {
    let io = |e: std::io::Error| CliError::Ipfs(format!("{}: {e}", path.display()));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CliError::Ipfs(format!("{}: not a file name", path.display())))?;
    let file = fs::File::open(path).map_err(io)?;
    let imported = import_file(BufReader::new(file), ImportParams::PUBLISHED)
        .map_err(|e| CliError::Ipfs(format!("{}: {e}", path.display())))?;
    Ok(ArtifactCidRecord::new(name, imported))
}

pub fn run_ipfs_cid(args: &IpfsCidArgs) -> Result<(), CliError> {
    let mut records = Vec::with_capacity(args.files.len());
    for path in &args.files {
        let record = identify(path)?;
        if args.write {
            let mut companion = path.clone().into_os_string();
            companion.push(CID_RECORD_SUFFIX);
            let mut text = to_pretty(&record)?;
            text.push('\n');
            fs::write(&companion, text)
                .map_err(|e| CliError::Ipfs(format!("{}: {e}", Path::new(&companion).display())))?;
        }
        records.push(record);
    }
    println!("{}", to_pretty(&records)?);
    Ok(())
}

pub fn run_ipfs_dir(args: &IpfsDirArgs) -> Result<(), CliError> {
    let directory = read_cycle_directory(&args.cycle_dir)?;
    if let (Some(path), Some(directory)) = (&args.block_out, &directory) {
        fs::write(path, &directory.node.block)
            .map_err(|e| CliError::Ipfs(format!("{}: {e}", path.display())))?;
    }
    let report = serde_json::json!({
        "directory_cid": directory.as_ref().map(|d| d.node.cid.as_str()),
        "files": directory.as_ref().map_or(&[][..], |d| d.records.as_slice()),
    });
    println!("{}", to_pretty(&report)?);
    Ok(())
}

/// Location `sync` downloads an entry from: the first HTTP(S) one, else a local path. An
/// `ipfs://` address is not downloadable here, so an entry with nothing else yields `None`.
pub fn sync_location(entry: &RollingEntry) -> Option<&str> {
    let is_http = |url: &&str| url.starts_with("http://") || url.starts_with("https://");
    std::iter::once(entry.url.as_str())
        .chain(entry.mirror_urls.iter().map(String::as_str))
        .find(is_http)
        .or_else(|| (!entry.url.starts_with("ipfs://")).then_some(entry.url.as_str()))
}

fn to_pretty<T: serde::Serialize>(value: &T) -> Result<String, CliError> {
    serde_json::to_string_pretty(value).map_err(|e| CliError::Json(e.to_string()))
}

#[cfg(test)]
mod tests;
