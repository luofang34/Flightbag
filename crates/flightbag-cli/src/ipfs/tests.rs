use flightbag_tiles::ArtifactStorage;

use super::*;

fn scratch(test: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("flightbag_cli_ipfs_{test}_{}", std::process::id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

fn entry(url: &str, mirror_urls: &[&str]) -> RollingEntry {
    RollingEntry {
        authority: "faa_vfr_sectional".to_owned(),
        effective_date: "2026-10-29".to_owned(),
        expires_date: "2026-12-24".to_owned(),
        airac_code: "2611".to_owned(),
        effective_from_utc: "2026-10-29T09:01:00Z".to_owned(),
        effective_until_utc: "2026-12-24T09:01:00Z".to_owned(),
        file: "vfr_sectional.pmtiles".to_owned(),
        url: url.to_owned(),
        size_bytes: 1,
        sha256_hex: "0".repeat(64),
        upstream_sha256_hex: "1".repeat(64),
        min_zoom: 0,
        max_zoom: 11,
        bounds: [-180.0, -90.0, 180.0, 90.0],
        storage: ArtifactStorage::Ipfs,
        ipfs_cid: None,
        mirror_urls: mirror_urls.iter().map(|url| (*url).to_owned()).collect(),
    }
}

#[test]
fn parses_ipfs_commands() {
    assert_eq!(
        parse_ipfs_cid(&args(&["ipfs-cid", "--write", "a.pmtiles", "b.bin"])).ok(),
        Some(Command::IpfsCid(IpfsCidArgs {
            files: vec![PathBuf::from("a.pmtiles"), PathBuf::from("b.bin")],
            write: true,
        }))
    );
    assert!(parse_ipfs_cid(&args(&["ipfs-cid"])).is_err());
    assert!(parse_ipfs_cid(&args(&["ipfs-cid", "--pin", "a"])).is_err());
    assert_eq!(
        parse_ipfs_dir(&args(&[
            "ipfs-dir",
            "--cycle-dir",
            "d",
            "--block-out",
            "d.block"
        ]))
        .ok(),
        Some(Command::IpfsDir(IpfsDirArgs {
            cycle_dir: PathBuf::from("d"),
            block_out: Some(PathBuf::from("d.block")),
        }))
    );
    assert!(parse_ipfs_dir(&args(&["ipfs-dir"])).is_err());
    assert!(parse_ipfs_dir(&args(&["ipfs-dir", "--cycle-dir"])).is_err());
}

#[test]
fn companions_written_for_a_cycle_yield_its_directory_block() {
    let dir = scratch("cycle");
    // One artifact of a single leaf and one spanning two, so the directory links both kinds.
    let small = dir.join("cifp_procedures.bin");
    let large = dir.join("airspace.pmtiles");
    fs::write(&small, b"procedures").unwrap();
    fs::write(&large, vec![7_u8; ImportParams::PUBLISHED.chunk_bytes + 1]).unwrap();
    run_ipfs_cid(&IpfsCidArgs {
        files: vec![small.clone(), large.clone()],
        write: true,
    })
    .unwrap();

    let record: ArtifactCidRecord =
        serde_json::from_slice(&fs::read(dir.join("airspace.pmtiles.cid.json")).unwrap()).unwrap();
    assert_eq!(record, identify(&large).unwrap());
    assert_eq!(record.file, "airspace.pmtiles");
    assert_eq!(record.size_bytes, 1_048_577);
    assert!(record.cid.starts_with("bafybei"));
    assert!(identify(&small).unwrap().cid.starts_with("bafkrei"));

    let block_out = dir.join("directory.block");
    run_ipfs_dir(&IpfsDirArgs {
        cycle_dir: dir.clone(),
        block_out: Some(block_out.clone()),
    })
    .unwrap();
    let directory = read_cycle_directory(&dir).unwrap().unwrap();
    assert_eq!(fs::read(&block_out).unwrap(), directory.node.block);
    assert_eq!(directory.records.len(), 2);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_cycle_without_companions_has_no_directory() {
    let dir = scratch("empty");
    let block_out = dir.join("directory.block");
    run_ipfs_dir(&IpfsDirArgs {
        cycle_dir: dir.clone(),
        block_out: Some(block_out.clone()),
    })
    .unwrap();
    assert!(!block_out.exists());
    assert!(identify(&dir.join("missing.pmtiles")).is_err());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn sync_downloads_over_http_and_never_from_an_ipfs_address() {
    let gateway = "https://gateway.invalid/ipfs/bafy/vfr_sectional.pmtiles";
    assert_eq!(sync_location(&entry(gateway, &[])), Some(gateway));
    assert_eq!(
        sync_location(&entry("ipfs://bafy/vfr_sectional.pmtiles", &[])),
        None
    );
    assert_eq!(
        sync_location(&entry("ipfs://bafy/vfr_sectional.pmtiles", &[gateway])),
        Some(gateway)
    );
    assert_eq!(
        sync_location(&entry("dist/vfr_sectional.pmtiles", &[gateway])),
        Some(gateway)
    );
    assert_eq!(
        sync_location(&entry("dist/vfr_sectional.pmtiles", &[])),
        Some("dist/vfr_sectional.pmtiles")
    );
}
