use std::path::PathBuf;

use super::*;
use crate::unixfs::import_file;

const GATEWAY: &str = "https://gateway.invalid";

/// A cycle directory under the system temp dir, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("flightbag_ipfs_{test}_{}", std::process::id()));
        drop(fs::remove_dir_all(&root));
        fs::create_dir_all(root.join("2026-10-29")).unwrap();
        Self(root)
    }

    fn cycle(&self) -> PathBuf {
        self.0.join("2026-10-29")
    }

    /// Writes the companion of `bytes` published as `file` and returns the matching entry.
    fn publish(&self, authority: &str, file: &str, bytes: &[u8]) -> RollingEntry {
        let imported = import_file(bytes, ImportParams::PUBLISHED).unwrap();
        let record = ArtifactCidRecord::new(file, imported);
        fs::write(
            self.cycle().join(format!("{file}{CID_RECORD_SUFFIX}")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        entry(authority, file, record.size_bytes, &record.sha256_hex)
    }

    fn vouch(&self, directory_cid: &str, gateway: &str) {
        let record = PinRecord {
            schema_version: 1,
            effective_date: "2026-10-29".to_owned(),
            directory_cid: directory_cid.to_owned(),
            gateway: gateway.to_owned(),
            providers: vec!["kubo".to_owned()],
        };
        fs::write(
            self.cycle().join(PIN_RECORD_FILE),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

fn entry(authority: &str, file: &str, size_bytes: u64, sha256_hex: &str) -> RollingEntry {
    RollingEntry {
        authority: authority.to_owned(),
        effective_date: "2026-10-29".to_owned(),
        expires_date: "2026-11-26".to_owned(),
        airac_code: "2611".to_owned(),
        effective_from_utc: "2026-10-29T09:01:00Z".to_owned(),
        effective_until_utc: "2026-11-26T09:01:00Z".to_owned(),
        file: file.to_owned(),
        url: format!("https://release.invalid/{file}"),
        size_bytes,
        sha256_hex: sha256_hex.to_owned(),
        upstream_sha256_hex: "1".repeat(64),
        min_zoom: 0,
        max_zoom: 10,
        bounds: [-180.0, -90.0, 180.0, 90.0],
        storage: ArtifactStorage::Release,
        ipfs_cid: None,
        mirror_urls: Vec::new(),
    }
}

fn options(gateway: Option<&str>) -> IpfsOptions {
    IpfsOptions {
        gateway: gateway.map(str::to_owned),
        ipfs_only_files: vec!["vfr_sectional.pmtiles".to_owned()],
    }
}

#[test]
fn entries_without_companions_are_left_as_published() {
    let scratch = Scratch::new("no_companions");
    let mut entries = vec![entry("faa_nasr", "airspace.pmtiles", 3, &"0".repeat(64))];
    let before = entries.clone();
    let ipfs = annotate_entries(&mut entries, &scratch.0, &options(Some(GATEWAY))).unwrap();
    assert_eq!(ipfs, None);
    assert_eq!(entries, before);
}

#[test]
fn companions_give_cids_and_a_directory_but_no_gateway_without_a_pin_record() {
    let scratch = Scratch::new("cids_only");
    let mut entries = vec![
        scratch.publish("faa_nasr", "airspace.pmtiles", b"airspace"),
        scratch.publish("faa_vfr_sectional", "vfr_sectional.pmtiles", b"sectional"),
    ];
    let ipfs = annotate_entries(&mut entries, &scratch.0, &options(Some(GATEWAY)))
        .unwrap()
        .unwrap();
    let directory = read_cycle_directory(&scratch.cycle()).unwrap().unwrap();
    let cycle = &ipfs.cycles["2026-10-29"];
    assert_eq!(cycle.directory_cid, directory.node.cid);
    assert_eq!(cycle.gateway_url, None);
    assert_eq!(ipfs.import.chunker, "size-1048576");

    let [airspace, sectional] = entries.as_slice() else {
        panic!("both entries must stay listed");
    };
    assert_eq!(airspace.storage, ArtifactStorage::Release);
    assert_eq!(airspace.url, "https://release.invalid/airspace.pmtiles");
    assert!(airspace.mirror_urls.is_empty());
    assert_eq!(
        airspace.ipfs_cid.as_deref(),
        Some(directory.records[0].cid.as_str())
    );
    assert_eq!(sectional.storage, ArtifactStorage::Ipfs);
    assert_eq!(
        sectional.url,
        format!("ipfs://{}/vfr_sectional.pmtiles", directory.node.cid)
    );
}

#[test]
fn a_pin_record_lists_the_gateway_only_for_its_directory_and_gateway() {
    let scratch = Scratch::new("pin_record");
    let published = vec![
        scratch.publish("faa_nasr", "airspace.pmtiles", b"airspace"),
        scratch.publish("faa_vfr_sectional", "vfr_sectional.pmtiles", b"sectional"),
    ];
    let directory = read_cycle_directory(&scratch.cycle())
        .unwrap()
        .unwrap()
        .node
        .cid;
    let base = format!("{GATEWAY}/ipfs/{directory}");

    scratch.vouch(&directory, &format!("{GATEWAY}/"));
    let mut entries = published.clone();
    let ipfs = annotate_entries(&mut entries, &scratch.0, &options(Some(GATEWAY)))
        .unwrap()
        .unwrap();
    assert_eq!(
        ipfs.cycles["2026-10-29"].gateway_url.as_deref(),
        Some(base.as_str())
    );
    assert_eq!(
        entries[0].mirror_urls,
        vec![format!("{base}/airspace.pmtiles")]
    );
    assert_eq!(entries[0].url, "https://release.invalid/airspace.pmtiles");
    assert_eq!(entries[1].url, format!("{base}/vfr_sectional.pmtiles"));
    assert!(entries[1].mirror_urls.is_empty());

    for (recorded_directory, recorded_gateway, configured) in [
        // The record vouches for a directory with other content.
        (
            "bafybeidhfzynen2lnbbfdg6earhkw47eyo6zizifhqgiuoxiyahwfrnct4",
            GATEWAY,
            Some(GATEWAY),
        ),
        // The record vouches for another gateway than the configured one.
        (directory.as_str(), "https://other.invalid", Some(GATEWAY)),
        // No gateway is configured.
        (directory.as_str(), GATEWAY, None),
    ] {
        scratch.vouch(recorded_directory, recorded_gateway);
        let mut entries = published.clone();
        let ipfs = annotate_entries(&mut entries, &scratch.0, &options(configured))
            .unwrap()
            .unwrap();
        assert_eq!(ipfs.cycles["2026-10-29"].gateway_url, None);
        assert!(entries[0].mirror_urls.is_empty());
        assert!(entries[1].url.starts_with("ipfs://"));
    }
}

#[test]
fn an_ipfs_only_artifact_without_a_companion_is_not_listed() {
    let scratch = Scratch::new("unaddressable");
    let mut entries = vec![
        scratch.publish("faa_nasr", "airspace.pmtiles", b"airspace"),
        entry(
            "faa_vfr_sectional",
            "vfr_sectional.pmtiles",
            9,
            &"0".repeat(64),
        ),
    ];
    annotate_entries(&mut entries, &scratch.0, &options(None)).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].file, "airspace.pmtiles");
}

#[test]
fn a_companion_of_other_bytes_is_an_error_not_a_cid() {
    let scratch = Scratch::new("mismatch");
    let mut stale = scratch.publish("faa_nasr", "airspace.pmtiles", b"airspace");
    stale.sha256_hex = "0".repeat(64);
    let mut entries = vec![stale];
    let result = annotate_entries(&mut entries, &scratch.0, &options(None));
    assert!(matches!(result, Err(ArchiveError::Json(message)) if message.contains("other bytes")));
}

#[test]
fn companions_with_other_import_parameters_are_refused() {
    let scratch = Scratch::new("params");
    scratch.publish("faa_nasr", "airspace.pmtiles", b"airspace");
    let path = scratch
        .cycle()
        .join(format!("airspace.pmtiles{CID_RECORD_SUFFIX}"));
    let mut record: ArtifactCidRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record.import.chunker = "size-262144".to_owned();
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(read_cycle_directory(&scratch.cycle()).is_err());
}

#[test]
fn manifests_without_ipfs_fields_parse() {
    let old = r#"{"schema_version":1,"disclaimer":"d","entries":[{"authority":"faa_nasr",
        "effective_date":"2026-10-29","expires_date":"2026-11-26","airac_code":"2611",
        "effective_from_utc":"2026-10-29T09:01:00Z","effective_until_utc":"2026-11-26T09:01:00Z",
        "file":"airspace.pmtiles","url":"https://release.invalid/airspace.pmtiles","size_bytes":3,
        "sha256_hex":"00","upstream_sha256_hex":"11","min_zoom":2,"max_zoom":10,
        "bounds":[-180.0,-15.0,180.0,82.0]}]}"#;
    let manifest: crate::RollingManifest = serde_json::from_str(old).unwrap();
    assert_eq!(manifest.ipfs, None);
    assert_eq!(manifest.entries[0].storage, ArtifactStorage::Release);
    assert_eq!(manifest.entries[0].ipfs_cid, None);
}
