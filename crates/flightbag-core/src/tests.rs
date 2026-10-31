use alloc::string::String;
use alloc::vec;
use world_core::GeodeticPosition;

use crate::{
    AIRAC_CYCLE_SECONDS, AiracCycle, AirportSurfaceGraph, AuthoritySource, ChartBounds, ChartKind,
    EditionManifest, FacilityCode, GeorefChartRef, ParkingKind, ParkingSpot, RunwayDesignator,
    RunwayEndSpec, SourceDigest, SurfaceNodeId,
};

#[test]
fn airac_cycle_28_day_grid_and_validity_window() {
    let c0 = AiracCycle::CYCLE_2001;
    let (start, end) = c0.effective_unix_seconds();
    assert_eq!(end - start, AIRAC_CYCLE_SECONDS);
    assert_eq!(AiracCycle::active_at_unix_seconds(start), c0);
    assert_eq!(AiracCycle::active_at_unix_seconds(start - 1), c0.previous());
    assert_eq!(AiracCycle::active_at_unix_seconds(end), c0.next());

    let win = c0.validity_window();
    assert!(win.contains(win.from));
    assert!(win.contains(win.until));
}

#[test]
fn edition_manifest_and_single_copy_airport_surface_graph() {
    let sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let nasr =
        SourceDigest::new(AuthoritySource::FaaNasr, String::from("2026-10-29"), sha).unwrap();
    let osm = SourceDigest::new(
        AuthoritySource::OpenStreetMap,
        String::from("us-northeast-261028"),
        sha,
    )
    .unwrap();
    let manifest = EditionManifest {
        schema_version: 1,
        edition_id: String::from("flightbag-delaware-2611"),
        airac: AiracCycle::from_index(89),
        sources: vec![nasr, osm],
        signature_ed25519_hex: None,
    };
    assert_eq!(manifest.sources.len(), 2);

    let kdov = FacilityCode::parse("kdov").unwrap();
    assert_eq!(kdov.as_str(), "KDOV");
    let rwy01 = RunwayDesignator::parse("01").unwrap();
    let graph = AirportSurfaceGraph {
        facility: kdov,
        reference_point: GeodeticPosition::wgs84(39.1295, -75.4659, 8.5),
        runway_ends: vec![RunwayEndSpec {
            designator: rwy01,
            threshold: GeodeticPosition::wgs84(39.1170, -75.4680, 8.5),
            true_bearing_rad: 0.12,
            length_m: 2_926.0,
            width_m: 45.7,
        }],
        taxi_edges: vec![],
        hold_short_nodes: vec![],
        parking_spots: vec![ParkingSpot {
            label: String::from("RAMP 1"),
            kind: ParkingKind::GeneralAviationRamp,
            position: GeodeticPosition::wgs84(39.1280, -75.4620, 8.5),
            heading_rad: 1.57,
            connected_node: Some(SurfaceNodeId(1)),
        }],
    };
    assert!(graph.runway_end(rwy01).is_some());
    assert!(graph.parking_spot("RAMP 1").is_some());

    let bounds = ChartBounds {
        south_lat_deg: 39.0,
        west_lon_deg: -75.6,
        north_lat_deg: 39.3,
        east_lon_deg: -75.3,
    };
    bounds.validate().unwrap();
    let chart = GeorefChartRef {
        chart_id: String::from("KDOV-IAP-ILS-01"),
        facility: Some(kdov),
        kind: ChartKind::ApproachPlate,
        title: String::from("ILS OR LOC RWY 01"),
        bounds,
        asset_sha256_hex: String::from(sha),
    };
    assert_eq!(chart.facility, Some(kdov));
}
