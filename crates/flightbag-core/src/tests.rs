use alloc::string::String;
use alloc::vec;
use world_core::GeodeticPosition;

use crate::{
    AIRAC_CYCLE_SECONDS, AiracCycle, AirportSurfaceGraph, AirspaceClassification, AirspacePolygon,
    AirspaceVolume, AltitudeDatum, AuthoritySource, ChartBounds, ChartKind,
    ControlledAirspaceClass, ControlledLocalType, EditionManifest, FacilityCode, GeorefChartRef,
    ParkingKind, ParkingSpot, RunwayDesignator, RunwayEndSpec, SourceDigest, SuaKind,
    SurfaceNodeId, VerticalLimit,
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

#[test]
fn airspace_volume_contracts_and_vertical_limits() {
    let shelf = AirspaceVolume {
        airspace_key: String::from("SEA-B-H"),
        ident: String::from("SEA"),
        name: String::from("SEATTLE CLASS B"),
        sector: Some(String::from("AREA H")),
        classification: AirspaceClassification::Controlled {
            class: ControlledAirspaceClass::ClassB,
            local_type: ControlledLocalType::ClassB,
        },
        lower: VerticalLimit::msl_ft(3_000),
        upper: VerticalLimit::msl_ft(10_000),
        hours_of_use: Some(String::from("CONT")),
        controlling_agency: Some(String::from("SEATTLE TRACON")),
        polygons: vec![AirspacePolygon {
            exterior: vec![
                GeodeticPosition::wgs84(47.20, -122.50, 0.0),
                GeodeticPosition::wgs84(47.20, -122.10, 0.0),
                GeodeticPosition::wgs84(47.70, -122.10, 0.0),
                GeodeticPosition::wgs84(47.70, -122.50, 0.0),
            ],
            interiors: vec![],
        }],
    };
    assert!(shelf.validate().is_ok());
    assert_eq!(shelf.classification.class_code(), "B");
    assert_eq!(shelf.lower.datum.renderer_datum(), "msl");
    assert!((shelf.lower.metres() - 914.4).abs() < 1e-6);
    assert!((shelf.upper.metres() - 3_048.0).abs() < 1e-6);

    let sua = AirspaceClassification::SpecialUse(SuaKind::Restricted);
    assert_eq!(sua.category_tag(), "sua");
    assert_eq!(sua.class_code(), "R");
    assert_eq!(VerticalLimit::agl_ft(700).datum, AltitudeDatum::Agl);
    assert_eq!(VerticalLimit::SURFACE.datum.renderer_datum(), "agl");
    assert_eq!(VerticalLimit::flight_level(180).value_ft, 18_000);
}

#[test]
fn cifp_airway_expansion_respects_segment_breaks_and_procedure_seam_dedup() {
    use crate::{
        AirwayRouteClass, AltitudeConstraintKind, CifpAirway, CifpAirwayFix, CifpProcedure,
        CifpProcedureLeg, CifpTransition, CifpWaypointKind, PathTerminator, ProcedureKind,
        ProcedurePhase,
    };

    let fix = |seq: u16, id: &str, brk: bool| CifpAirwayFix {
        seq_num: seq,
        fix_ident: String::from(id),
        icao_region: *b"K6",
        kind: CifpWaypointKind::EnrouteWaypoint,
        latitude_e6: 40_000_000,
        longitude_e6: -74_000_000,
        min_alt_ft: if brk { 0 } else { 3_000 },
        max_alt_ft: 17_500,
        segment_break_after: brk,
    };
    let v3 = CifpAirway {
        route_ident: String::from("V3"),
        route_class: AirwayRouteClass::from_route_ident("V3"),
        fixes: vec![
            fix(10, "MUE", false),
            fix(20, "PARIS", true),
            fix(30, "EYW", false),
            fix(40, "MXE", false),
            fix(50, "SBJ", true),
        ],
    };
    assert_eq!(v3.route_class, AirwayRouteClass::Victor);
    // Cannot bridge across Hawaii (`PARIS` break) to East Coast (`EYW`).
    assert!(v3.expand_inclusive("MUE", "EYW").is_none());
    // Forward and reverse expansion on the East Coast segment works cleanly.
    let fwd = v3.expand_inclusive("EYW", "SBJ").unwrap_or_default();
    assert_eq!(fwd.len(), 3);
    let rev = v3.expand_inclusive("SBJ", "EYW").unwrap_or_default();
    assert_eq!(rev.len(), 3);
    assert_eq!(rev.first().map(|f| f.fix_ident.as_str()), Some("SBJ"));
    assert_eq!(rev.last().map(|f| f.fix_ident.as_str()), Some("EYW"));

    let leg = |seq: u16, id: &str, pt: PathTerminator, missed: bool| CifpProcedureLeg {
        seq_num: seq,
        fix_ident: String::from(id),
        kind: CifpWaypointKind::TerminalWaypoint,
        path_terminator: pt,
        has_coordinates: true,
        latitude_e6: 40_580_000,
        longitude_e6: -74_740_000,
        alt_constraint: AltitudeConstraintKind::AtOrAbove,
        alt1_ft: 2_600,
        alt2_ft: 0,
        speed_limit_kt: 0,
        is_missed_approach: missed,
        is_iaf: false,
        is_faf: false,
        is_mapt: false,
    };
    let r22 = CifpProcedure {
        airport_ident: String::from("N51"),
        kind: ProcedureKind::Approach,
        procedure_ident: String::from("R22"),
        route_type: b'R',
        transitions: vec![
            CifpTransition {
                transition_ident: String::from("PIYIN"),
                phase: ProcedurePhase::EnrouteOrFeederTransition,
                route_type: b'A',
                legs: vec![
                    leg(10, "PIYIN", PathTerminator::IF, false),
                    leg(20, "DODLY", PathTerminator::TF, false),
                ],
            },
            CifpTransition {
                transition_ident: String::new(),
                phase: ProcedurePhase::CommonCore,
                route_type: b'R',
                legs: vec![
                    leg(10, "DODLY", PathTerminator::IF, false),
                    leg(20, "CAVZA", PathTerminator::TF, false),
                    leg(30, "RW22", PathTerminator::TF, false),
                    leg(50, "MOSSE", PathTerminator::DF, true),
                ],
            },
        ],
    };
    let active = r22.assemble_legs(Some("PIYIN"), None, false);
    assert_eq!(active.len(), 4);
    assert_eq!(active.get(1).map(|l| l.fix_ident.as_str()), Some("DODLY"));
    assert_eq!(active.get(3).map(|l| l.fix_ident.as_str()), Some("RW22"));
    let with_missed = r22.assemble_legs(Some("PIYIN"), None, true);
    assert_eq!(with_missed.len(), 5);
}
