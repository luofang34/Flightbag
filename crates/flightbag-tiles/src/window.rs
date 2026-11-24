//! Wall-clock selection of the effective and the pre-staged edition of every authority.
//!
//! Publishing runs on a best-effort schedule, so the published index never depends on which run
//! produced it: `current` and `upcoming` are a pure function of the listed editions and an
//! instant. An edition whose `effective_from_utc` lies after that instant can only ever be
//! `upcoming`.

use std::collections::BTreeMap;

use crate::PROTOTYPE_DISCLAIMER;
use crate::ipfs::ArtifactStorage;
use crate::manifest::{RollingEntry, RollingManifest};

/// Authority whose current edition backs the legacy top-level `current` block of the index.
pub const NASR_AUTHORITY: &str = "faa_nasr";

/// Published per-cycle directory prefix on the static site.
const SITE_CYCLES_PREFIX: &str = "cycles";

/// Editions of one authority relative to an instant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditionWindow<'a> {
    /// Edition effective at the instant (`effective_from_utc <= now < effective_until_utc`).
    pub current: Option<&'a RollingEntry>,
    /// Earliest edition that becomes effective after the instant.
    pub upcoming: Option<&'a RollingEntry>,
}

/// Selects the effective and the next pre-staged edition of `authority` at `now_iso_utc`
/// (`YYYY-MM-DDTHH:MM:SSZ`, compared lexicographically against the entries' timestamps).
pub fn select_edition_window<'a>(
    manifest: &'a RollingManifest,
    authority: &str,
    now_iso_utc: &str,
) -> EditionWindow<'a> {
    let of_authority = || manifest.entries.iter().filter(|e| e.authority == authority);
    let current = of_authority()
        .filter(|e| {
            e.effective_from_utc.as_str() <= now_iso_utc
                && now_iso_utc < e.effective_until_utc.as_str()
        })
        .max_by(|a, b| a.effective_from_utc.cmp(&b.effective_from_utc));
    let upcoming = of_authority()
        .filter(|e| e.effective_from_utc.as_str() > now_iso_utc)
        .min_by(|a, b| a.effective_from_utc.cmp(&b.effective_from_utc));
    EditionWindow { current, upcoming }
}

/// Returns `true` when `text` has the fixed `YYYY-MM-DDTHH:MM:SSZ` shape that makes
/// lexicographic comparison equal to chronological comparison.
pub fn is_iso_utc_instant(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 20
        && bytes.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

/// Static-site index (`cycles.json`) of the effective and pre-staged edition per authority.
///
/// It carries no generation timestamp: two runs that observe the same editions in the same
/// windows produce identical bytes, which lets a publisher skip redeploying an unchanged site.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CycleIndex {
    /// Index schema version (`4`).
    pub schema_version: u32,
    /// Mandatory safety disclaimer marking all editions as non-navigational.
    pub disclaimer: String,
    /// Site-relative path of the rolling multi-authority manifest.
    pub rolling_manifest_url: String,
    /// Effective NASR edition served under the stable `cycles/current/` alias.
    pub current: Option<CurrentAlias>,
    /// Effective and pre-staged edition of every authority, keyed by authority slug.
    pub editions: BTreeMap<String, AuthorityEditions>,
}

/// Stable `cycles/current/` alias of the effective NASR edition.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CurrentAlias {
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// Four-digit `YYCC` AIRAC code.
    pub airac_code: String,
    /// Inclusive UTC activation instant.
    pub effective_from_utc: String,
    /// Exclusive UTC expiration instant; a client past it must re-read the index.
    pub effective_until_utc: String,
    /// Site-relative cycle manifest path.
    pub manifest_url: String,
    /// Site-relative `airspace.pmtiles` path.
    pub airspace_pmtiles_url: String,
    /// Site-relative MapLibre VFR style path.
    pub vfr_style_url: String,
}

/// Effective and pre-staged edition of one authority.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuthorityEditions {
    /// Edition effective now, or `None` when no listed edition covers the instant.
    pub current: Option<IndexedEdition>,
    /// Next edition, published ahead of its effective instant for pre-download.
    pub upcoming: Option<IndexedEdition>,
}

/// One downloadable edition in a [`CycleIndex`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IndexedEdition {
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// Four-digit `YYCC` AIRAC code.
    pub airac_code: String,
    /// Inclusive UTC activation instant.
    pub effective_from_utc: String,
    /// Exclusive UTC expiration instant.
    pub effective_until_utc: String,
    /// Artifact filename.
    pub file: String,
    /// Exact artifact byte length.
    pub size_bytes: u64,
    /// Lowercase SHA-256 hex digest of the artifact.
    pub sha256_hex: String,
    /// Release download URL (HTTP `Range`, but no CORS headers: native clients only), or `None`
    /// for an artifact stored on IPFS only.
    pub release_url: Option<String>,
    /// Site-relative path served with CORS and HTTP `Range`, or `None` when the artifact is too
    /// large to mirror on the static site or is stored on IPFS only.
    pub site_path: Option<String>,
    /// CIDv1 of the artifact under the rolling manifest's `ipfs.import` parameters.
    pub ipfs_cid: Option<String>,
    /// CID of the cycle directory; `<ipfs_directory_cid>/<file>` addresses the artifact.
    pub ipfs_directory_cid: Option<String>,
    /// Verified gateway URL served with CORS and HTTP `Range`, or `None` when no gateway is
    /// known to serve the artifact.
    pub ipfs_url: Option<String>,
}

/// Builds the [`CycleIndex`] of `manifest` at `now_iso_utc`, mirroring on the static site only
/// artifacts of at most `site_max_bytes` bytes.
pub fn build_cycle_index(
    manifest: &RollingManifest,
    now_iso_utc: &str,
    site_max_bytes: u64,
) -> CycleIndex {
    let mut editions = BTreeMap::new();
    for entry in &manifest.entries {
        if editions.contains_key(&entry.authority) {
            continue;
        }
        let window = select_edition_window(manifest, &entry.authority, now_iso_utc);
        if window.current.is_none() && window.upcoming.is_none() {
            continue;
        }
        editions.insert(
            entry.authority.clone(),
            AuthorityEditions {
                current: window.current.map(|e| indexed(manifest, e, site_max_bytes)),
                upcoming: window
                    .upcoming
                    .map(|e| indexed(manifest, e, site_max_bytes)),
            },
        );
    }

    let current = select_edition_window(manifest, NASR_AUTHORITY, now_iso_utc)
        .current
        .filter(|e| on_site(e, site_max_bytes))
        .map(|e| CurrentAlias {
            effective_date: e.effective_date.clone(),
            airac_code: e.airac_code.clone(),
            effective_from_utc: e.effective_from_utc.clone(),
            effective_until_utc: e.effective_until_utc.clone(),
            manifest_url: format!("{SITE_CYCLES_PREFIX}/current/manifest.json"),
            airspace_pmtiles_url: format!("{SITE_CYCLES_PREFIX}/current/{}", e.file),
            vfr_style_url: "styles/airspace_vfr.json".to_owned(),
        });

    CycleIndex {
        schema_version: 4,
        disclaimer: PROTOTYPE_DISCLAIMER.to_owned(),
        rolling_manifest_url: "manifest.json".to_owned(),
        current,
        editions,
    }
}

fn on_site(entry: &RollingEntry, site_max_bytes: u64) -> bool {
    entry.storage == ArtifactStorage::Release && entry.size_bytes <= site_max_bytes
}

fn indexed(
    manifest: &RollingManifest,
    entry: &RollingEntry,
    site_max_bytes: u64,
) -> IndexedEdition {
    let cycle = manifest
        .ipfs
        .as_ref()
        .filter(|_| entry.ipfs_cid.is_some())
        .and_then(|ipfs| ipfs.cycles.get(&entry.effective_date));
    IndexedEdition {
        effective_date: entry.effective_date.clone(),
        airac_code: entry.airac_code.clone(),
        effective_from_utc: entry.effective_from_utc.clone(),
        effective_until_utc: entry.effective_until_utc.clone(),
        file: entry.file.clone(),
        size_bytes: entry.size_bytes,
        sha256_hex: entry.sha256_hex.clone(),
        release_url: (entry.storage == ArtifactStorage::Release).then(|| entry.url.clone()),
        site_path: on_site(entry, site_max_bytes).then(|| {
            format!(
                "{SITE_CYCLES_PREFIX}/{}/{}",
                entry.effective_date, entry.file
            )
        }),
        ipfs_cid: entry.ipfs_cid.clone(),
        ipfs_directory_cid: cycle.map(|c| c.directory_cid.clone()),
        ipfs_url: cycle
            .and_then(|c| c.gateway_url.as_ref())
            .map(|base| format!("{base}/{}", entry.file)),
    }
}

#[cfg(test)]
mod tests;
