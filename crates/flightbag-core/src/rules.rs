//! Standing Special Flight Rules Areas (SFRAs) and georeferenced chart metadata.

use alloc::string::String;
use alloc::vec::Vec;
use world_core::GeodeticPosition;

use crate::{FacilityCode, FlightbagError};

/// Category of standing (non-moving) Special Flight Rules Area.
///
/// Temporary, event-driven restrictions (VIP, wildfire, stadium, launch TFRs)
/// belong in `world-notices`, whereas standing 14 CFR Part 93 / AIP special
/// rules live here in `Flightbag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SfraKind {
    /// Washington, DC Metropolitan Special Flight Rules Area.
    DcSfra,
    /// Washington, DC Flight Restricted Zone.
    DcFrz,
    /// Grand Canyon National Park SFRA.
    GrandCanyon,
    /// New York Class B / Hudson & East River SFRA corridor.
    HudsonRiverCorridor,
    /// Los Angeles Special Flight Rules Area.
    LosAngelesSfra,
    /// Other standing Part 93 / national AIP special traffic rule area.
    StandingSpecialRule,
}

/// Standing Special Flight Rules Area published in an aeronautical cycle.
#[derive(Clone, Debug, PartialEq)]
pub struct SpecialFlightRuleArea {
    /// Canonical airspace key (compatible with `world_notices::AirspaceKey`).
    pub designator: String,
    /// SFRA classification.
    pub kind: SfraKind,
    /// Closed lateral boundary polygon on WGS-84.
    pub boundary: Vec<GeodeticPosition>,
    /// Lower altitude above mean sea level, meters.
    pub floor_msl_m: f64,
    /// Upper altitude above mean sea level, meters.
    pub ceiling_msl_m: f64,
    /// Standing CTAF/guard/ATC frequency in kHz, if required.
    pub required_frequency_khz: Option<u32>,
    /// Discrete beacon / squawk code required for VFR transit, if applicable.
    pub discrete_squawk_octal: Option<u16>,
}

/// Kind of georeferenced aeronautical chart or terminal procedure plate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChartKind {
    /// Instrument Approach Procedure (IAP) plate.
    ApproachPlate,
    /// Airport Diagram (APD).
    AirportDiagram,
    /// Standard Instrument Departure (SID).
    Sid,
    /// Standard Terminal Arrival (STAR).
    Star,
    /// IFR Enroute Low Altitude chart.
    EnrouteLow,
    /// IFR Enroute High Altitude chart.
    EnrouteHigh,
    /// VFR Sectional or Terminal Area Chart.
    VfrSectional,
}

/// Geodetic bounding box of a georeferenced chart asset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChartBounds {
    /// Southernmost latitude, degrees.
    pub south_lat_deg: f64,
    /// Westernmost longitude, degrees.
    pub west_lon_deg: f64,
    /// Northernmost latitude, degrees.
    pub north_lat_deg: f64,
    /// Easternmost longitude, degrees.
    pub east_lon_deg: f64,
}

impl ChartBounds {
    /// Validates latitude/longitude ordering and finiteness.
    ///
    /// # Errors
    ///
    /// Returns [`FlightbagError::InvalidGeometry`] when inverted or out of range.
    pub fn validate(&self) -> Result<(), FlightbagError> {
        let valid_lats = (-90.0..=90.0).contains(&self.south_lat_deg)
            && (-90.0..=90.0).contains(&self.north_lat_deg)
            && self.south_lat_deg < self.north_lat_deg;
        let valid_lons = (-180.0..=180.0).contains(&self.west_lon_deg)
            && (-180.0..=180.0).contains(&self.east_lon_deg)
            && self.west_lon_deg < self.east_lon_deg;
        if valid_lats && valid_lons {
            Ok(())
        } else {
            Err(FlightbagError::InvalidGeometry)
        }
    }
}

/// Metadata referencing a georeferenced chart or terminal plate in an edition.
#[derive(Clone, Debug, PartialEq)]
pub struct GeorefChartRef {
    /// Chart identifier within the edition.
    pub chart_id: String,
    /// Associated aerodrome, if terminal-specific.
    pub facility: Option<FacilityCode>,
    /// Chart category.
    pub kind: ChartKind,
    /// Display title (e.g. `"ILS OR LOC RWY 01"`).
    pub title: String,
    /// Geodetic footprint for moving-map overlay draping.
    pub bounds: ChartBounds,
    /// Lowercase 64-character SHA-256 hex digest of the chart asset.
    pub asset_sha256_hex: String,
}
