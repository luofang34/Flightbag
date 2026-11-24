//! IPFS records of a published cycle: per-artifact CID companions, the cycle directory, and the
//! annotation of rolling-manifest entries with content identifiers and gateway locations.
//!
//! Everything here is a pure function of files in a cycle directory. Whether a gateway really
//! serves a cycle is established by the publisher, which records it in [`PIN_RECORD_FILE`]; a
//! manifest only repeats a gateway location that record vouches for, so two runs over the same
//! published files write identical manifests whatever the gateway does in between.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::archive::ArchiveError;
use crate::manifest::RollingEntry;
use crate::unixfs::{DirectoryEntry, DirectoryNode, ImportParams, ImportedFile, directory_node};

/// Suffix of the companion written beside an artifact (`airspace.pmtiles.cid.json`).
pub const CID_RECORD_SUFFIX: &str = ".cid.json";

/// Per-cycle record of a directory a gateway was verified to serve.
pub const PIN_RECORD_FILE: &str = "ipfs-pin.json";

/// Import parameters under which a published CID is reproducible from the artifact bytes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IpfsImport {
    /// CID version (`1`).
    pub cid_version: u8,
    /// Multihash function (`"sha2-256"`).
    pub multihash: String,
    /// Whether leaves are raw blocks rather than `UnixFS`-wrapped ones (`true`).
    pub raw_leaves: bool,
    /// Chunker in `ipfs add --chunker` notation (`"size-1048576"`).
    pub chunker: String,
    /// File DAG layout (`"balanced"`).
    pub layout: String,
    /// Most links of one file node (`174`).
    pub max_links: usize,
    /// Directory representation (`"basic"`: one `dag-pb` node, links ordered by name).
    pub directory: String,
}

impl IpfsImport {
    /// Describes [`ImportParams::PUBLISHED`].
    pub fn published() -> Self {
        Self {
            cid_version: 1,
            multihash: "sha2-256".to_owned(),
            raw_leaves: true,
            chunker: format!("size-{}", ImportParams::PUBLISHED.chunk_bytes),
            layout: "balanced".to_owned(),
            max_links: ImportParams::PUBLISHED.max_links,
            directory: "basic".to_owned(),
        }
    }
}

/// Companion of one artifact (`<file>.cid.json`) tying its CID to its bytes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactCidRecord {
    /// Record schema version (`1`).
    pub schema_version: u32,
    /// Artifact filename.
    pub file: String,
    /// Exact artifact byte length.
    pub size_bytes: u64,
    /// Lowercase SHA-256 hex digest of the artifact.
    pub sha256_hex: String,
    /// CIDv1 of the artifact imported with `import`.
    pub cid: String,
    /// Total size of the artifact's DAG blocks, as a directory link to it records.
    pub dag_bytes: u64,
    /// Parameters that reproduce `cid`.
    pub import: IpfsImport,
}

impl ArtifactCidRecord {
    /// Records `imported` (made with [`ImportParams::PUBLISHED`]) under the filename `file`.
    pub fn new(file: &str, imported: ImportedFile) -> Self {
        Self {
            schema_version: 1,
            file: file.to_owned(),
            size_bytes: imported.size_bytes,
            sha256_hex: imported.sha256_hex,
            cid: imported.cid,
            dag_bytes: imported.dag_bytes,
            import: IpfsImport::published(),
        }
    }
}

/// Directory of one cycle a gateway served correctly when the publisher last probed it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinRecord {
    /// Record schema version (`1`).
    pub schema_version: u32,
    /// Effective date of the cycle (`YYYY-MM-DD`).
    pub effective_date: String,
    /// CID of the verified cycle directory.
    pub directory_cid: String,
    /// Gateway base URL the range probes passed against.
    pub gateway: String,
    /// Providers that hold the pin (`"kubo"`, `"pinning_service"`).
    #[serde(default)]
    pub providers: Vec<String>,
}

/// IPFS section of a rolling manifest.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IpfsPublication {
    /// Parameters that reproduce every `ipfs_cid` and `directory_cid` listed.
    pub import: IpfsImport,
    /// Cycle directories keyed by effective date.
    pub cycles: BTreeMap<String, IpfsCycle>,
}

/// `UnixFS` directory holding every artifact of one cycle under its filename.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IpfsCycle {
    /// CID of the directory; `<directory_cid>/<file>` addresses one artifact.
    pub directory_cid: String,
    /// Verified gateway URL of the directory, or `None` when no gateway is known to serve it.
    pub gateway_url: Option<String>,
}

/// Where the bytes of an artifact are published.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStorage {
    /// A Release asset, mirrored on the static site when small enough.
    #[default]
    Release,
    /// IPFS only: too large for the static site and never uploaded as a Release asset.
    Ipfs,
}

/// How a rolling manifest refers to IPFS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IpfsOptions {
    /// Gateway base URL whose verified cycles are listed as download locations.
    pub gateway: Option<String>,
    /// Filenames of artifacts whose storage is [`ArtifactStorage::Ipfs`].
    pub ipfs_only_files: Vec<String>,
}

/// CID companions and directory of one cycle directory on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleDirectory {
    /// Companions found in the cycle directory, ordered by filename.
    pub records: Vec<ArtifactCidRecord>,
    /// Directory node linking every recorded artifact.
    pub node: DirectoryNode,
}

/// Reads the CID companions of `cycle_dir` and builds the cycle's directory node, or returns
/// `None` when the cycle has no companion.
///
/// # Errors
///
/// Returns [`ArchiveError`] if the directory or a companion cannot be read or parsed, or the
/// companions cannot form a directory.
pub fn read_cycle_directory(cycle_dir: &Path) -> Result<Option<CycleDirectory>, ArchiveError> {
    let io = |source| ArchiveError::Io {
        path: cycle_dir.to_path_buf(),
        source,
    };
    let mut paths = Vec::new();
    for item in fs::read_dir(cycle_dir).map_err(io)? {
        let path = item.map_err(io)?.path();
        let is_record = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(CID_RECORD_SUFFIX));
        if is_record {
            paths.push(path);
        }
    }
    paths.sort();
    let mut records = Vec::new();
    for path in paths {
        let record: ArtifactCidRecord = read_json(&path)?;
        if record.import != IpfsImport::published() {
            return Err(ArchiveError::Json(format!(
                "{}: recorded import parameters are not the published ones",
                path.display()
            )));
        }
        records.push(record);
    }
    if records.is_empty() {
        return Ok(None);
    }
    let entries: Vec<DirectoryEntry> = records
        .iter()
        .map(|record| DirectoryEntry {
            name: record.file.clone(),
            cid: record.cid.clone(),
            dag_bytes: record.dag_bytes,
        })
        .collect();
    let node = directory_node(&entries)
        .map_err(|e| ArchiveError::Json(format!("{}: {e}", cycle_dir.display())))?;
    Ok(Some(CycleDirectory { records, node }))
}

/// Gives every entry its CID and download locations from the companions under `cycles_dir`, and
/// returns the manifest's IPFS section (`None` when no cycle has a companion).
///
/// An artifact stored on IPFS only is listed at the verified gateway, or at its `ipfs://`
/// address when no gateway is verified; without a companion it has no address and is dropped.
///
/// # Errors
///
/// Returns [`ArchiveError`] if a companion is unreadable or describes other bytes than the
/// cycle manifest of the same file.
pub fn annotate_entries(
    entries: &mut Vec<RollingEntry>,
    cycles_dir: &Path,
    options: &IpfsOptions,
) -> Result<Option<IpfsPublication>, ArchiveError> {
    let mut cycles = BTreeMap::new();
    let mut kept = Vec::with_capacity(entries.len());
    for mut entry in std::mem::take(entries) {
        if options.ipfs_only_files.contains(&entry.file) {
            entry.storage = ArtifactStorage::Ipfs;
        }
        let cycle_dir = cycles_dir.join(&entry.effective_date);
        let directory = read_cycle_directory(&cycle_dir)?;
        let record = directory
            .as_ref()
            .and_then(|d| d.records.iter().find(|record| record.file == entry.file));
        let (Some(directory), Some(record)) = (directory.as_ref(), record) else {
            if entry.storage == ArtifactStorage::Release {
                kept.push(entry);
            }
            continue;
        };
        if record.sha256_hex != entry.sha256_hex || record.size_bytes != entry.size_bytes {
            return Err(ArchiveError::Json(format!(
                "{}: {}{CID_RECORD_SUFFIX} describes other bytes than the cycle manifest",
                cycle_dir.display(),
                entry.file
            )));
        }
        let directory_cid = &directory.node.cid;
        let gateway_url = verified_gateway_url(&cycle_dir, directory_cid, options)?;
        entry.ipfs_cid = Some(record.cid.clone());
        let at_gateway = gateway_url
            .as_ref()
            .map(|base| format!("{base}/{}", entry.file));
        match (entry.storage, at_gateway) {
            (ArtifactStorage::Ipfs, Some(url)) => entry.url = url,
            (ArtifactStorage::Ipfs, None) => {
                entry.url = format!("ipfs://{directory_cid}/{}", entry.file);
            }
            (ArtifactStorage::Release, Some(url)) => entry.mirror_urls = vec![url],
            (ArtifactStorage::Release, None) => {}
        }
        cycles.insert(
            entry.effective_date.clone(),
            IpfsCycle {
                directory_cid: directory_cid.clone(),
                gateway_url,
            },
        );
        kept.push(entry);
    }
    *entries = kept;
    Ok((!cycles.is_empty()).then(|| IpfsPublication {
        import: IpfsImport::published(),
        cycles,
    }))
}

/// Gateway URL of a cycle directory, if the pin record of that cycle vouches for exactly this
/// directory at the configured gateway.
fn verified_gateway_url(
    cycle_dir: &Path,
    directory_cid: &str,
    options: &IpfsOptions,
) -> Result<Option<String>, ArchiveError> {
    let Some(gateway) = options.gateway.as_deref() else {
        return Ok(None);
    };
    let path = cycle_dir.join(PIN_RECORD_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let record: PinRecord = read_json(&path)?;
    let gateway = gateway.trim_end_matches('/');
    let vouched =
        record.directory_cid == directory_cid && record.gateway.trim_end_matches('/') == gateway;
    Ok(vouched.then(|| format!("{gateway}/ipfs/{directory_cid}")))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ArchiveError> {
    let bytes = fs::read(path).map_err(|source| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ArchiveError::Json(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests;
