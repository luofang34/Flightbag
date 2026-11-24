use super::*;
use crate::manifest::select_active_entry;

const SITE_MAX_BYTES: u64 = 90_000_000;
const JUST_BEFORE_CUTOVER: &str = "2026-11-26T09:00:59Z";
const AT_CUTOVER: &str = "2026-11-26T09:01:00Z";

fn entry(authority: &str, file: &str, from: &str, until: &str, size_bytes: u64) -> RollingEntry {
    RollingEntry {
        authority: authority.to_owned(),
        effective_date: from.to_owned(),
        expires_date: until.to_owned(),
        airac_code: "0000".to_owned(),
        effective_from_utc: format!("{from}T09:01:00Z"),
        effective_until_utc: format!("{until}T09:01:00Z"),
        file: file.to_owned(),
        url: format!("https://example.invalid/airac-{from}/{file}"),
        size_bytes,
        sha256_hex: "0".repeat(64),
        upstream_sha256_hex: "1".repeat(64),
        min_zoom: 0,
        max_zoom: 10,
        bounds: [-180.0, -90.0, 180.0, 90.0],
    }
}

fn nasr(from: &str, until: &str) -> RollingEntry {
    entry(NASR_AUTHORITY, "airspace.pmtiles", from, until, 28_000_000)
}

fn manifest(entries: Vec<RollingEntry>) -> RollingManifest {
    RollingManifest {
        schema_version: 1,
        disclaimer: PROTOTYPE_DISCLAIMER.to_owned(),
        entries,
    }
}

fn both_posted() -> RollingManifest {
    manifest(vec![
        nasr("2026-10-29", "2026-11-26"),
        nasr("2026-11-26", "2026-12-24"),
    ])
}

fn dates(window: EditionWindow<'_>) -> (Option<&str>, Option<&str>) {
    (
        window.current.map(|e| e.effective_date.as_str()),
        window.upcoming.map(|e| e.effective_date.as_str()),
    )
}

#[test]
fn pre_staged_edition_stays_upcoming_until_its_effective_second() {
    let m = both_posted();
    assert_eq!(
        dates(select_edition_window(
            &m,
            NASR_AUTHORITY,
            JUST_BEFORE_CUTOVER
        )),
        (Some("2026-10-29"), Some("2026-11-26"))
    );
}

#[test]
fn pointer_flips_at_the_effective_second_regardless_of_listing_order() {
    let mut m = both_posted();
    assert_eq!(
        dates(select_edition_window(&m, NASR_AUTHORITY, AT_CUTOVER)),
        (Some("2026-11-26"), None)
    );
    m.entries.reverse();
    assert_eq!(
        dates(select_edition_window(&m, NASR_AUTHORITY, AT_CUTOVER)),
        (Some("2026-11-26"), None)
    );
}

#[test]
fn upcoming_not_posted_leaves_current_untouched_before_cutover() {
    let m = manifest(vec![nasr("2026-10-29", "2026-11-26")]);
    assert_eq!(
        dates(select_edition_window(
            &m,
            NASR_AUTHORITY,
            JUST_BEFORE_CUTOVER
        )),
        (Some("2026-10-29"), None)
    );
}

#[test]
fn expired_edition_is_not_current_when_its_successor_was_never_published() {
    let m = manifest(vec![nasr("2026-10-29", "2026-11-26")]);
    assert_eq!(
        dates(select_edition_window(&m, NASR_AUTHORITY, AT_CUTOVER)),
        (None, None)
    );
    let index = build_cycle_index(&m, AT_CUTOVER, SITE_MAX_BYTES);
    assert_eq!(index.current, None);
    assert!(index.editions.is_empty());
}

#[test]
fn only_a_future_edition_listed_yields_no_current() {
    let m = manifest(vec![nasr("2026-11-26", "2026-12-24")]);
    assert_eq!(
        dates(select_edition_window(
            &m,
            NASR_AUTHORITY,
            JUST_BEFORE_CUTOVER
        )),
        (None, Some("2026-11-26"))
    );
    assert_eq!(
        build_cycle_index(&m, JUST_BEFORE_CUTOVER, SITE_MAX_BYTES).current,
        None
    );
}

#[test]
fn sync_selection_never_returns_an_edition_ahead_of_its_effective_time() {
    let future_only = manifest(vec![nasr("2026-11-26", "2026-12-24")]);
    assert_eq!(
        select_active_entry(&future_only, NASR_AUTHORITY, JUST_BEFORE_CUTOVER),
        None
    );

    let m = both_posted();
    let before = select_active_entry(&m, NASR_AUTHORITY, JUST_BEFORE_CUTOVER);
    assert_eq!(
        before.map(|e| e.effective_date.as_str()),
        Some("2026-10-29")
    );
    let after = select_active_entry(&m, NASR_AUTHORITY, AT_CUTOVER);
    assert_eq!(after.map(|e| e.effective_date.as_str()), Some("2026-11-26"));

    let expired_only = manifest(vec![nasr("2026-10-29", "2026-11-26")]);
    let stale = select_active_entry(&expired_only, NASR_AUTHORITY, AT_CUTOVER);
    assert_eq!(stale.map(|e| e.effective_date.as_str()), Some("2026-10-29"));
}

#[test]
fn authorities_flip_independently_on_their_own_cycle_length() {
    let m = manifest(vec![
        nasr("2026-10-29", "2026-11-26"),
        nasr("2026-11-26", "2026-12-24"),
        entry(
            "faa_vfr_terminal",
            "vfr_terminal.pmtiles",
            "2026-10-29",
            "2026-12-24",
            185_000_000,
        ),
    ]);
    let index = build_cycle_index(&m, AT_CUTOVER, SITE_MAX_BYTES);
    let chart = &index.editions["faa_vfr_terminal"];
    assert_eq!(
        chart.current.as_ref().map(|e| e.effective_date.as_str()),
        Some("2026-10-29")
    );
    let nasr_now = &index.editions[NASR_AUTHORITY];
    assert_eq!(
        nasr_now.current.as_ref().map(|e| e.effective_date.as_str()),
        Some("2026-11-26")
    );
}

#[test]
fn index_mirrors_only_artifacts_that_fit_the_static_site() {
    let m = manifest(vec![
        nasr("2026-10-29", "2026-11-26"),
        entry(
            "faa_vfr_sectional",
            "vfr_sectional.pmtiles",
            "2026-10-29",
            "2026-12-24",
            1_235_326_444,
        ),
    ]);
    let index = build_cycle_index(&m, JUST_BEFORE_CUTOVER, SITE_MAX_BYTES);
    let small = index.editions[NASR_AUTHORITY].current.clone().unwrap();
    assert_eq!(
        small.site_path.as_deref(),
        Some("cycles/2026-10-29/airspace.pmtiles")
    );
    let large = index.editions["faa_vfr_sectional"].current.clone().unwrap();
    assert_eq!(large.site_path, None);
    assert_eq!(
        large.release_url,
        "https://example.invalid/airac-2026-10-29/vfr_sectional.pmtiles"
    );
}

#[test]
fn index_is_identical_for_two_instants_inside_the_same_window() {
    let m = both_posted();
    let early = build_cycle_index(&m, "2026-11-20T03:23:00Z", SITE_MAX_BYTES);
    let late = build_cycle_index(&m, JUST_BEFORE_CUTOVER, SITE_MAX_BYTES);
    assert_eq!(early, late);
    let alias = early.current.unwrap();
    assert_eq!(alias.effective_date, "2026-10-29");
    assert_eq!(alias.effective_until_utc, AT_CUTOVER);
    assert_eq!(
        alias.airspace_pmtiles_url,
        "cycles/current/airspace.pmtiles"
    );
}

#[test]
fn instant_shape_check_rejects_offsets_and_missing_seconds() {
    assert!(is_iso_utc_instant(AT_CUTOVER));
    assert!(!is_iso_utc_instant("2026-11-26T09:01Z"));
    assert!(!is_iso_utc_instant("2026-11-26T09:01:00+00:00"));
    assert!(!is_iso_utc_instant("2026-11-26 09:01:00Z"));
}
