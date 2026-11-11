//! Core `no_std` + `alloc` contracts for published, cycle-dated aeronautical
//! editions (`Flightbag`).
//!
//! `Flightbag` owns the **non-moving** aeronautical baseline shared across
//! real flight, EFB/C2 (`sokoly.app`), cockpit displays (`Indicate`) and
//! closed-loop simulation (`Simulate`):
//! - [`AiracCycle`] and [`EditionManifest`]: 28-day ICAO/FAA AIRAC effective
//!   windows (`09:01Z` activation) and multi-authority SHA-256 digests.
//! - [`AirportSurfaceGraph`]: single-copy runway thresholds, taxiway edges,
//!   hold-short nodes and parking/gate spots.
//! - [`SpecialFlightRuleArea`] and [`GeorefChartRef`]: standing special flight
//!   rules and georeferenced terminal/enroute charts.

#![no_std]

extern crate alloc;

mod airspace;
mod cycle;
mod edition;
mod rules;
mod surface;

#[cfg(test)]
mod tests;

pub use airspace::{
    AirspaceClassification, AirspacePolygon, AirspaceVolume, AltitudeDatum,
    ControlledAirspaceClass, ControlledLocalType, METRES_PER_FOOT, SuaKind, VerticalLimit,
};
pub use cycle::{AIRAC_CYCLE_SECONDS, AiracCycle, FlightbagError};
pub use edition::{AuthoritySource, EditionManifest, SourceDigest};
pub use rules::{ChartBounds, ChartKind, GeorefChartRef, SfraKind, SpecialFlightRuleArea};
pub use surface::{
    AirportSurfaceGraph, FacilityCode, HoldShortNode, ParkingKind, ParkingSpot, RunwayDesignator,
    RunwayEndSpec, StationLocationResolver, SurfaceNodeId, TaxiEdgeSpec,
};
