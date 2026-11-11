//! Atomic, SHA-256-verified HTTP downloader and byte-range prober for FAA NASR archives.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cycles::ResolvedCycle;

/// User-Agent header identifying automated `Flightbag` AIRAC builds to FAA servers.
pub const USER_AGENT: &str = "Flightbag/0.1 (+https://github.com/luofang34/Flightbag)";

const CHUNK_BYTES: usize = 1024 * 1024;

/// Verified metadata of a downloaded or cache-hit NASR `.zip` archive.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NasrArchiveFingerprint {
    /// Effective date (`YYYY-MM-DD`).
    pub cycle: String,
    /// Four-digit `YYCC` code.
    pub airac_code: String,
    /// Archive filename.
    pub filename: String,
    /// Source URL.
    pub url: String,
    /// Lowercase 64-char SHA-256 hex digest.
    pub sha256_hex: String,
    /// Archive byte length.
    pub size_bytes: u64,
    /// `true` when existing cached bytes passed `.sha256` verification without re-downloading.
    pub cached: bool,
    /// Local path to the verified `.zip`.
    pub path: PathBuf,
}

/// Errors encountered while probing or fetching FAA NASR archives.
#[derive(Debug, Error)]
pub enum FetchError {
    /// Network transport error.
    #[error("HTTP request to {url} failed: {reason}")]
    Network {
        /// Requested URL.
        url: String,
        /// Underlying transport explanation.
        reason: String,
    },
    /// Unexpected HTTP status code.
    #[error("{url} returned HTTP {status}")]
    Status {
        /// Requested URL.
        url: String,
        /// HTTP status code.
        status: u16,
    },
    /// Empty HTTP body.
    #[error("{url} returned an empty body")]
    EmptyBody {
        /// Requested URL.
        url: String,
    },
    /// SHA-256 mismatch on cached or downloaded archive.
    #[error("SHA-256 mismatch at {path}: actual {actual}, expected {expected}")]
    ChecksumMismatch {
        /// File path.
        path: PathBuf,
        /// Computed SHA-256 hex.
        actual: String,
        /// Expected SHA-256 hex.
        expected: String,
    },
    /// Filesystem cache error.
    #[error("filesystem I/O error at {path}: {source}")]
    Io {
        /// File or directory path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Minimal HTTP response abstraction enabling deterministic offline tests.
pub struct TransportResponse {
    /// HTTP status code (`200`, `206`, `404`, etc.).
    pub status: u16,
    /// Streaming response body.
    pub body: Box<dyn Read>,
}

/// Blocking HTTP GET transport.
pub trait Transport {
    /// Issues an HTTP GET with an optional `Range` header.
    ///
    /// # Errors
    ///
    /// Returns [`FetchError::Network`] if no HTTP response was received.
    fn get_blocking(&self, url: &str, range: Option<&str>)
    -> Result<TransportResponse, FetchError>;
}

/// Production HTTP transport backed by `ureq` with retry backoff.
pub struct UreqTransport {
    agent: ureq::Agent,
    max_attempts: u8,
}

impl UreqTransport {
    /// Constructs a production transport with a 10-minute stream timeout and 3 attempts.
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(600))
                .build(),
            max_attempts: 3,
        }
    }

    fn call_once(&self, url: &str, range: Option<&str>) -> Result<TransportResponse, FetchError> {
        let mut req = self.agent.get(url);
        if let Some(range_val) = range {
            req = req.set("Range", range_val);
        }
        match req.call() {
            Ok(resp) | Err(ureq::Error::Status(_, resp)) => Ok(TransportResponse {
                status: resp.status(),
                body: Box::new(resp.into_reader()),
            }),
            Err(err) => Err(FetchError::Network {
                url: url.to_owned(),
                reason: err.to_string(),
            }),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for UreqTransport {
    fn get_blocking(
        &self,
        url: &str,
        range: Option<&str>,
    ) -> Result<TransportResponse, FetchError> {
        let mut attempt: u8 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            match self.call_once(url, range) {
                Ok(resp)
                    if matches!(resp.status, 429 | 500 | 502 | 503 | 504)
                        && attempt < self.max_attempts =>
                {
                    std::thread::sleep(Duration::from_secs(u64::from(attempt) * 2));
                }
                Ok(resp) => return Ok(resp),
                Err(err) if attempt < self.max_attempts => {
                    let _ = err;
                    std::thread::sleep(Duration::from_secs(u64::from(attempt) * 2));
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// Probes whether FAA has published `cycle`'s 28-day NASR archive using a 1-byte `Range` GET.
///
/// # Errors
///
/// Returns [`FetchError`] on network failure or unexpected server status.
pub fn probe_nasr_published(
    transport: &dyn Transport,
    cycle: &ResolvedCycle,
) -> Result<bool, FetchError> {
    let resp = transport.get_blocking(&cycle.nasr_url, Some("bytes=0-0"))?;
    match resp.status {
        200 | 206 => Ok(true),
        404 | 410 => Ok(false),
        status => Err(FetchError::Status {
            url: cycle.nasr_url.clone(),
            status,
        }),
    }
}

/// Hashes an existing local `.zip` file without network access, producing a [`NasrArchiveFingerprint`].
///
/// # Errors
///
/// Returns [`FetchError::Io`] or [`FetchError::EmptyBody`] if `zip_path` cannot be read or is empty.
pub fn fingerprint_local_zip(
    cycle: &ResolvedCycle,
    zip_path: &Path,
) -> Result<NasrArchiveFingerprint, FetchError> {
    let (size_bytes, sha256_hex) = hash_file(zip_path)?;
    if size_bytes == 0 {
        return Err(FetchError::EmptyBody {
            url: zip_path.display().to_string(),
        });
    }
    Ok(NasrArchiveFingerprint {
        cycle: cycle.effective_date.clone(),
        airac_code: cycle.airac_code.clone(),
        filename: file_name_str(zip_path),
        url: cycle.nasr_url.clone(),
        sha256_hex,
        size_bytes,
        cached: true,
        path: zip_path.to_path_buf(),
    })
}

/// Downloads `cycle`'s NASR `.zip` into `cache_root/<YYYY-MM-DD>/`, reusing verified cached bytes.
///
/// # Errors
///
/// Returns [`FetchError`] on network, status, I/O, or SHA-256 verification failure.
pub fn fetch_nasr_zip(
    transport: &dyn Transport,
    cycle: &ResolvedCycle,
    cache_root: &Path,
) -> Result<NasrArchiveFingerprint, FetchError> {
    let cycle_dir = cache_root.join(&cycle.effective_date);
    let dest = cycle_dir.join(&cycle.nasr_filename);
    let sidecar = sidecar_path(&dest);

    if dest.is_file() && sidecar.is_file() {
        let recorded = fs::read_to_string(&sidecar)
            .map_err(|source| FetchError::Io {
                path: sidecar.clone(),
                source,
            })?
            .trim()
            .to_ascii_lowercase();
        let (size_bytes, actual) = hash_file(&dest)?;
        if actual != recorded {
            return Err(FetchError::ChecksumMismatch {
                path: dest,
                actual,
                expected: recorded,
            });
        }
        if size_bytes > 0 {
            return Ok(NasrArchiveFingerprint {
                cycle: cycle.effective_date.clone(),
                airac_code: cycle.airac_code.clone(),
                filename: cycle.nasr_filename.clone(),
                url: cycle.nasr_url.clone(),
                sha256_hex: actual,
                size_bytes,
                cached: true,
                path: dest,
            });
        }
    }

    fs::create_dir_all(&cycle_dir).map_err(|source| FetchError::Io {
        path: cycle_dir.clone(),
        source,
    })?;
    let part = dest.with_file_name(format!("{}.part", cycle.nasr_filename));
    let result = download_to_part(transport, &cycle.nasr_url, &part);
    let (size_bytes, sha256_hex) = match result {
        Ok(pair) => pair,
        Err(err) => {
            drop(fs::remove_file(&part));
            return Err(err);
        }
    };
    fs::rename(&part, &dest).map_err(|source| FetchError::Io {
        path: dest.clone(),
        source,
    })?;
    let sidecar_part = sidecar.with_file_name(format!("{}.sha256.part", cycle.nasr_filename));
    fs::write(&sidecar_part, format!("{sha256_hex}\n")).map_err(|source| FetchError::Io {
        path: sidecar_part.clone(),
        source,
    })?;
    fs::rename(&sidecar_part, &sidecar).map_err(|source| FetchError::Io {
        path: sidecar,
        source,
    })?;

    Ok(NasrArchiveFingerprint {
        cycle: cycle.effective_date.clone(),
        airac_code: cycle.airac_code.clone(),
        filename: cycle.nasr_filename.clone(),
        url: cycle.nasr_url.clone(),
        sha256_hex,
        size_bytes,
        cached: false,
        path: dest,
    })
}

fn download_to_part(
    transport: &dyn Transport,
    url: &str,
    part: &Path,
) -> Result<(u64, String), FetchError> {
    let resp = transport.get_blocking(url, None)?;
    if resp.status != 200 {
        return Err(FetchError::Status {
            url: url.to_owned(),
            status: resp.status,
        });
    }
    let mut reader = resp.body;
    let mut out = fs::File::create(part).map_err(|source| FetchError::Io {
        path: part.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let read_len = reader.read(&mut buf).map_err(|source| FetchError::Io {
            path: part.to_path_buf(),
            source,
        })?;
        if read_len == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..read_len) {
            hasher.update(chunk);
            out.write_all(chunk).map_err(|source| FetchError::Io {
                path: part.to_path_buf(),
                source,
            })?;
            total = total.saturating_add(read_len as u64);
        }
    }
    if total == 0 {
        return Err(FetchError::EmptyBody {
            url: url.to_owned(),
        });
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

fn hash_file(path: &Path) -> Result<(u64, String), FetchError> {
    let mut file = fs::File::open(path).map_err(|source| FetchError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let read_len = file.read(&mut buf).map_err(|source| FetchError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if read_len == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..read_len) {
            hasher.update(chunk);
            total = total.saturating_add(read_len as u64);
        }
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

fn sidecar_path(dest: &Path) -> PathBuf {
    dest.with_file_name(format!("{}.sha256", file_name_str(dest)))
}

fn file_name_str(path: &Path) -> String {
    path.file_name()
        .map(|os| os.to_string_lossy().into_owned())
        .unwrap_or_default()
}
