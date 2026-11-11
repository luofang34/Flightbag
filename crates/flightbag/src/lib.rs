//! Umbrella crate for `Flightbag`: cycle-dated, cryptographically
//! fingerprinted non-moving aeronautical data shared across real flight,
//! EFB/C2 (`sokoly.app`), cockpit displays (`Indicate`) and simulation
//! (`Simulate`).

#![no_std]

pub use flightbag_core as core;
pub use flightbag_core::{
    AIRAC_CYCLE_SECONDS, AiracCycle, AirportSurfaceGraph, AirspaceClassification, AirspacePolygon,
    AirspaceVolume, AltitudeDatum, AuthoritySource, ChartBounds, ChartKind,
    ControlledAirspaceClass, ControlledLocalType, EditionManifest, FacilityCode, FlightbagError,
    GeorefChartRef, HoldShortNode, METRES_PER_FOOT, ParkingKind, ParkingSpot, RunwayDesignator,
    RunwayEndSpec, SfraKind, SourceDigest, SpecialFlightRuleArea, StationLocationResolver, SuaKind,
    SurfaceNodeId, TaxiEdgeSpec, VerticalLimit,
};
