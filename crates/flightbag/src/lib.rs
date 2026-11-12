//! Umbrella crate for `Flightbag`: cycle-dated, cryptographically
//! fingerprinted non-moving aeronautical data shared across experimental
//! EFB/C2 prototypes (`sokoly.app`), cockpit display research (`Indicate`),
//! 3D/2D cartography (`maplibre-rs`), and simulation (`Simulate`).
//!
//! > **Warning — Experimental Prototype (Not for Navigation or Flight):**
//! > This crate and its generated datasets are unverified research prototypes
//! > and must never be used for real-world navigation, flight planning, or
//! > operational flight decisions.

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
