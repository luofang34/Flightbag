//! IPFS content identifiers of published artifacts, computed without an IPFS node.
//!
//! A CID names a Merkle DAG, not a byte string, so it is only reproducible together with the
//! import parameters that shape the DAG. [`ImportParams::PUBLISHED`] fixes them to what
//! `ipfs add --cid-version=1 --raw-leaves --chunker=size-1048576` builds: fixed-size raw leaves
//! under a balanced tree of `dag-pb` `UnixFS` file nodes, hashed with SHA-256. A pinning provider
//! that imports the same bytes with the same parameters must report the same CID, which is what
//! lets a publisher refuse a provider that stored something else.

use std::io::Read;

use sha2::{Digest, Sha256};

const CID_VERSION_1: u8 = 0x01;
const CODEC_RAW: u8 = 0x55;
const CODEC_DAG_PB: u8 = 0x70;
const MULTIHASH_SHA2_256: u8 = 0x12;
const SHA2_256_BYTES: u8 = 0x20;
/// Binary length of a CIDv1 with a SHA-256 multihash.
const CID_BYTES: usize = 36;
const UNIXFS_TYPE_DIRECTORY: u64 = 1;
const UNIXFS_TYPE_FILE: u64 = 2;
const BASE32_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Failure to compute or parse a content identifier.
#[derive(Debug, thiserror::Error)]
pub enum UnixfsError {
    /// The artifact could not be read.
    #[error("reading content failed: {0}")]
    Read(#[from] std::io::Error),
    /// The import parameters cannot shape a DAG.
    #[error("import parameters need a positive chunk size and at least two links per node")]
    Params,
    /// A CID string is not a base32 CIDv1 with a SHA-256 multihash.
    #[error("'{0}' is not a base32 CIDv1 with a SHA-256 multihash")]
    Cid(String),
    /// A directory entry name cannot be a `UnixFS` link name.
    #[error("'{0}' is not a usable directory entry name")]
    EntryName(String),
    /// Two directory entries share one name.
    #[error("directory entry '{0}' is listed twice")]
    DuplicateEntry(String),
}

/// Parameters that decide the DAG, and therefore the CID, of imported bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportParams {
    /// Leaf size in bytes; every leaf but the last holds exactly this many.
    pub chunk_bytes: usize,
    /// Most children of one file node.
    pub max_links: usize,
}

impl ImportParams {
    /// Parameters of every published artifact.
    pub const PUBLISHED: Self = Self {
        chunk_bytes: 1_048_576,
        max_links: 174,
    };
}

/// Content identifier and sizes of one imported file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedFile {
    /// CIDv1 of the DAG root, base32.
    pub cid: String,
    /// File length in bytes.
    pub size_bytes: u64,
    /// Total size of every block of the DAG; a directory link records it for its target.
    pub dag_bytes: u64,
    /// Lowercase SHA-256 hex digest of the file bytes, taken in the same pass.
    pub sha256_hex: String,
}

/// One named child of a `UnixFS` directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    /// Link name (a filename without `/`).
    pub name: String,
    /// CID of the child.
    pub cid: String,
    /// Total DAG size of the child.
    pub dag_bytes: u64,
}

/// A `UnixFS` directory node ready to be stored as one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryNode {
    /// CIDv1 of the directory, base32.
    pub cid: String,
    /// Encoded `dag-pb` block whose SHA-256 the CID carries.
    pub block: Vec<u8>,
}

/// A DAG node while its parents are still being built.
struct Node {
    cid: [u8; CID_BYTES],
    file_bytes: u64,
    dag_bytes: u64,
}

/// Imports `reader` as a `UnixFS` file and returns the CID a node would assign to it.
///
/// # Errors
///
/// Returns [`UnixfsError`] if `params` are degenerate or `reader` fails.
pub fn import_file(
    mut reader: impl Read,
    params: ImportParams,
) -> Result<ImportedFile, UnixfsError> {
    if params.chunk_bytes == 0 || params.max_links < 2 {
        return Err(UnixfsError::Params);
    }
    let mut whole = Sha256::new();
    let mut level = Vec::new();
    let mut chunk = vec![0_u8; params.chunk_bytes];
    loop {
        let filled = fill(&mut reader, &mut chunk)?;
        let data = chunk.get(..filled).unwrap_or_default();
        // A file without bytes is still one (empty) leaf, so it has a CID.
        if filled == 0 && !level.is_empty() {
            break;
        }
        whole.update(data);
        level.push(Node {
            cid: cid_bytes(CODEC_RAW, data),
            file_bytes: filled as u64,
            dag_bytes: filled as u64,
        });
        if filled < params.chunk_bytes {
            break;
        }
    }
    while level.len() > 1 {
        level = level.chunks(params.max_links).map(file_node).collect();
    }
    let root = level.first().ok_or(UnixfsError::Params)?;
    Ok(ImportedFile {
        cid: cid_string(&root.cid),
        size_bytes: root.file_bytes,
        dag_bytes: root.dag_bytes,
        sha256_hex: format!("{:x}", whole.finalize()),
    })
}

/// Builds the `UnixFS` directory holding `entries`, ordered by name as a node stores them.
///
/// # Errors
///
/// Returns [`UnixfsError`] if a name is empty, contains `/`, repeats, or a CID does not parse.
pub fn directory_node(entries: &[DirectoryEntry]) -> Result<DirectoryNode, UnixfsError> {
    let mut sorted: Vec<&DirectoryEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut block = Vec::new();
    let mut previous: Option<&str> = None;
    for entry in sorted {
        if entry.name.is_empty() || entry.name.contains('/') {
            return Err(UnixfsError::EntryName(entry.name.clone()));
        }
        if previous == Some(entry.name.as_str()) {
            return Err(UnixfsError::DuplicateEntry(entry.name.clone()));
        }
        previous = Some(entry.name.as_str());
        push_link(
            &mut block,
            &parse_cid(&entry.cid)?,
            &entry.name,
            entry.dag_bytes,
        );
    }
    let mut data = Vec::new();
    push_varint_field(&mut data, 1, UNIXFS_TYPE_DIRECTORY);
    push_bytes_field(&mut block, 1, &data);
    Ok(DirectoryNode {
        cid: cid_string(&cid_bytes(CODEC_DAG_PB, &block)),
        block,
    })
}

/// Reads until `buffer` is full or the reader ends; a short read alone does not end a chunk.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
    let mut filled = 0;
    while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match reader.read(rest) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn file_node(children: &[Node]) -> Node {
    let file_bytes: u64 = children.iter().map(|child| child.file_bytes).sum();
    let mut block = Vec::new();
    for child in children {
        push_link(&mut block, &child.cid, "", child.dag_bytes);
    }
    let mut data = Vec::new();
    push_varint_field(&mut data, 1, UNIXFS_TYPE_FILE);
    push_varint_field(&mut data, 3, file_bytes);
    for child in children {
        push_varint_field(&mut data, 4, child.file_bytes);
    }
    push_bytes_field(&mut block, 1, &data);
    let below: u64 = children.iter().map(|child| child.dag_bytes).sum();
    Node {
        cid: cid_bytes(CODEC_DAG_PB, &block),
        file_bytes,
        dag_bytes: below + block.len() as u64,
    }
}

/// Appends one `PBLink` to a `PBNode`. Links precede the node's data, and an empty name is
/// still written, because that is the canonical `dag-pb` form the block hash covers.
fn push_link(node: &mut Vec<u8>, cid: &[u8; CID_BYTES], name: &str, dag_bytes: u64) {
    let mut link = Vec::new();
    push_bytes_field(&mut link, 1, cid);
    push_bytes_field(&mut link, 2, name.as_bytes());
    push_varint_field(&mut link, 3, dag_bytes);
    push_bytes_field(node, 2, &link);
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn push_varint_field(out: &mut Vec<u8>, field: u8, value: u64) {
    out.push(field << 3);
    push_varint(out, value);
}

fn push_bytes_field(out: &mut Vec<u8>, field: u8, bytes: &[u8]) {
    out.push(field << 3 | 2);
    push_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn cid_bytes(codec: u8, block: &[u8]) -> [u8; CID_BYTES] {
    let mut cid = [0_u8; CID_BYTES];
    let header = [CID_VERSION_1, codec, MULTIHASH_SHA2_256, SHA2_256_BYTES];
    for (slot, byte) in cid
        .iter_mut()
        .zip(header.into_iter().chain(Sha256::digest(block)))
    {
        *slot = byte;
    }
    cid
}

/// Multibase base32 (`b` prefix, lowercase, unpadded): the default text form of a CIDv1.
fn cid_string(cid: &[u8; CID_BYTES]) -> String {
    let mut text = String::from("b");
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    for byte in cid {
        buffer = buffer << 8 | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            text.push(base32_char(buffer >> bits));
        }
    }
    if bits > 0 {
        text.push(base32_char(buffer << (5 - bits)));
    }
    text
}

fn base32_char(value: u32) -> char {
    BASE32_ALPHABET
        .get((value & 0x1f) as usize)
        .map_or('a', |symbol| char::from(*symbol))
}

fn parse_cid(text: &str) -> Result<[u8; CID_BYTES], UnixfsError> {
    let invalid = || UnixfsError::Cid(text.to_owned());
    let body = text.strip_prefix('b').ok_or_else(invalid)?;
    let mut bytes = Vec::with_capacity(CID_BYTES);
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    for symbol in body.bytes() {
        let value = BASE32_ALPHABET
            .iter()
            .position(|candidate| *candidate == symbol)
            .ok_or_else(invalid)?;
        buffer = (buffer << 5 | value as u32) & 0xffff;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
        }
    }
    let cid: [u8; CID_BYTES] = bytes.try_into().map_err(|_| invalid())?;
    let well_formed = matches!(
        cid,
        [
            CID_VERSION_1,
            CODEC_RAW | CODEC_DAG_PB,
            MULTIHASH_SHA2_256,
            SHA2_256_BYTES,
            ..
        ]
    );
    // Re-encoding rejects text with non-zero padding bits, which would alias another CID.
    if well_formed && cid_string(&cid) == text {
        Ok(cid)
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
mod tests;
