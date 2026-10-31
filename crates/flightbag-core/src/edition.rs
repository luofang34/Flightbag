//! Multi-authority aeronautical edition manifest and SHA-256 provenance.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{AiracCycle, FlightbagError};

/// Upstream aeronautical or cartographic authority contributing to an edition.
///
/// Parallel source digests are preserved side-by-side rather than overwriting
/// one authority's observation with another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthoritySource {
    /// FAA National Airspace System Resources (28-day NASR CSV/subscription).
    FaaNasr,
    /// FAA Coded Instrument Flight Procedures (ARINC 424-18 `FAACIFP18`).
    FaaCifpArinc424,
    /// FAA digital Terminal Procedures Publication (`d-TPP` XML + GeoTIFF plates).
    FaaDtpp,
    /// OpenStreetMap aeroway/apron/gate/building snapshot (`PBF` digest).
    OpenStreetMap,
    /// Natural Earth coastline / landmask basemap.
    NaturalEarth,
    /// Regional AIP or custom scenery/navdata source.
    RegionalAip,
}

/// Cryptographic digest of one upstream input archive compiled into an edition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceDigest {
    /// Upstream authority kind.
    pub authority: AuthoritySource,
    /// Upstream release or snapshot label (e.g. `"2026-10-29"` or `"planet-261026"`).
    pub snapshot_label: String,
    /// Lowercase 64-character hex SHA-256 of the upstream archive.
    sha256_hex: [u8; 64],
}

impl SourceDigest {
    /// Creates a validated [`SourceDigest`].
    ///
    /// # Errors
    ///
    /// Returns [`FlightbagError::InvalidSha256Hex`] unless `sha256_hex` is 64 lowercase hex chars.
    pub fn new(
        authority: AuthoritySource,
        snapshot_label: String,
        sha256_hex: &str,
    ) -> Result<Self, FlightbagError> {
        let bytes = sha256_hex.as_bytes();
        if bytes.len() != 64 {
            return Err(FlightbagError::InvalidSha256Hex);
        }
        let mut raw = [0u8; 64];
        for (dst, src) in raw.iter_mut().zip(bytes.iter()) {
            let is_lower_hex = src.is_ascii_digit() || (b'a'..=b'f').contains(src);
            if !is_lower_hex {
                return Err(FlightbagError::InvalidSha256Hex);
            }
            *dst = *src;
        }
        Ok(Self {
            authority,
            snapshot_label,
            sha256_hex: raw,
        })
    }

    /// Lowercase 64-character SHA-256 hex digest.
    pub fn sha256_hex(&self) -> &str {
        core::str::from_utf8(&self.sha256_hex).unwrap_or_default()
    }
}

/// Immutable header of a published `Flightbag` data pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditionManifest {
    /// Monotonic schema version of the `Flightbag` pack format.
    pub schema_version: u32,
    /// Human-readable edition identifier (e.g. `"flightbag-us-2611-r1"`).
    pub edition_id: String,
    /// Governing 28-day AIRAC cycle.
    pub airac: AiracCycle,
    /// Upstream source archives conflated into this edition.
    pub sources: Vec<SourceDigest>,
    /// Optional hex-encoded Ed25519 signature over the pack's canonical digest.
    pub signature_ed25519_hex: Option<String>,
}
