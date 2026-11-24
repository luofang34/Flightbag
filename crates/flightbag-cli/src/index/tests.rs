use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn parses_index_arguments_with_site_limit_default() {
    let parsed = parse_index_cmd(&args(&[
        "index",
        "--manifest",
        "dist/manifest.json",
        "--now",
        "2026-11-26T09:01:00Z",
        "--out",
        "dist/cycles.json",
    ]));
    assert_eq!(
        parsed.ok(),
        Some(Command::Index(IndexArgs {
            manifest_path: PathBuf::from("dist/manifest.json"),
            out_path: PathBuf::from("dist/cycles.json"),
            now: Some("2026-11-26T09:01:00Z".to_owned()),
            site_max_bytes: DEFAULT_SITE_MAX_BYTES,
        }))
    );
}

#[test]
fn index_requires_manifest_and_out() {
    assert!(parse_index_cmd(&args(&["index", "--out", "x.json"])).is_err());
    assert!(parse_index_cmd(&args(&["index", "--manifest", "m.json"])).is_err());
    assert!(parse_index_cmd(&args(&["index", "--manifest"])).is_err());
}

#[test]
fn explicit_now_must_be_a_whole_second_utc_instant() {
    let cutover = parse_now("2026-11-26T09:01:00Z").unwrap();
    assert_eq!(crate::format_iso_utc(cutover), "2026-11-26T09:01:00Z");
    assert!(parse_now("2026-11-26").is_err());
    assert!(parse_now("2026-11-26T09:01:00+01:00").is_err());
    assert!(parse_now("2026-13-40T09:01:00Z").is_err());
}

#[test]
fn written_index_flips_current_exactly_at_the_effective_instant() {
    let dir = std::env::temp_dir().join(format!("flightbag_index_cmd_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let manifest_path = dir.join("manifest.json");
    let entry = |from: &str, until: &str| {
        serde_json::json!({
            "authority": "faa_nasr",
            "effective_date": from,
            "expires_date": until,
            "airac_code": "0000",
            "effective_from_utc": format!("{from}T09:01:00Z"),
            "effective_until_utc": format!("{until}T09:01:00Z"),
            "file": "airspace.pmtiles",
            "url": format!("https://example.invalid/airac-{from}/airspace.pmtiles"),
            "size_bytes": 28_000_000,
            "sha256_hex": "0".repeat(64),
            "upstream_sha256_hex": "1".repeat(64),
            "min_zoom": 2,
            "max_zoom": 10,
            "bounds": [-180.0, -15.0, 180.0, 82.0],
        })
    };
    let rolling = serde_json::json!({
        "schema_version": 1,
        "disclaimer": "test",
        "entries": [entry("2026-10-29", "2026-11-26"), entry("2026-11-26", "2026-12-24")],
    });
    fs::write(&manifest_path, rolling.to_string()).unwrap();

    let current_at = |now: &str| {
        let out_path = dir.join("cycles.json");
        let run = run_index(
            &IndexArgs {
                manifest_path: manifest_path.clone(),
                out_path: out_path.clone(),
                now: Some(now.to_owned()),
                site_max_bytes: DEFAULT_SITE_MAX_BYTES,
            },
            OffsetDateTime::UNIX_EPOCH,
        );
        assert!(run.is_ok());
        let doc: serde_json::Value = serde_json::from_slice(&fs::read(&out_path).unwrap()).unwrap();
        (
            doc["current"]["effective_date"].as_str().map(str::to_owned),
            doc["editions"]["faa_nasr"]["upcoming"]["effective_date"]
                .as_str()
                .map(str::to_owned),
        )
    };
    assert_eq!(
        current_at("2026-11-26T09:00:59Z"),
        (Some("2026-10-29".to_owned()), Some("2026-11-26".to_owned()))
    );
    assert_eq!(
        current_at("2026-11-26T09:01:00Z"),
        (Some("2026-11-26".to_owned()), None)
    );
    fs::remove_dir_all(&dir).ok();
}
