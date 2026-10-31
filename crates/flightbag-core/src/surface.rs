//! Single-copy airport surface graph contract shared by OTW simulator spawn/taxi,
//! `sokoly.app` maps and `Indicate` airport surface displays.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use world_core::GeodeticPosition;

use crate::FlightbagError;

/// Compact 8-byte ICAO/FAA aerodrome or navaid identifier (compatible with `world_notices::FacilityKey`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FacilityCode([u8; 8]);

impl FacilityCode {
    /// Parses a 1..=8 byte ASCII alphanumeric facility code, uppercasing letters.
    ///
    /// # Errors
    ///
    /// Returns [`FlightbagError::InvalidIdentifier`] if empty, > 8 bytes, or non-alphanumeric.
    pub fn parse(code: &str) -> Result<Self, FlightbagError> {
        let trimmed = code.trim();
        let bytes = trimmed.as_bytes();
        if bytes.is_empty() || bytes.len() > 8 {
            return Err(FlightbagError::InvalidIdentifier);
        }
        let mut buf = [b' '; 8];
        for (dst, src) in buf.iter_mut().zip(bytes.iter()) {
            if !src.is_ascii_alphanumeric() {
                return Err(FlightbagError::InvalidIdentifier);
            }
            *dst = src.to_ascii_uppercase();
        }
        Ok(Self(buf))
    }

    /// Trimmed ASCII string view.
    pub fn as_str(&self) -> &str {
        trimmed_ascii(&self.0)
    }
}

/// Compact 4-byte runway-end designator (for example, `"01"`, `"19R"`, `"36C"`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunwayDesignator([u8; 4]);

impl RunwayDesignator {
    /// Parses a 1..=4 byte ASCII alphanumeric runway-end designator.
    ///
    /// # Errors
    ///
    /// Returns [`FlightbagError::InvalidIdentifier`] if empty, > 4 bytes, or non-alphanumeric.
    pub fn parse(text: &str) -> Result<Self, FlightbagError> {
        let trimmed = text.trim();
        let bytes = trimmed.as_bytes();
        if bytes.is_empty() || bytes.len() > 4 {
            return Err(FlightbagError::InvalidIdentifier);
        }
        let mut buf = [b' '; 4];
        for (dst, src) in buf.iter_mut().zip(bytes.iter()) {
            if !src.is_ascii_alphanumeric() {
                return Err(FlightbagError::InvalidIdentifier);
            }
            *dst = src.to_ascii_uppercase();
        }
        Ok(Self(buf))
    }

    /// Trimmed ASCII string view.
    pub fn as_str(&self) -> &str {
        trimmed_ascii(&self.0)
    }
}

fn trimmed_ascii(bytes: &[u8]) -> &str {
    let mut end = bytes.len();
    while end > 0 {
        if bytes.get(end - 1) == Some(&b' ') {
            end -= 1;
        } else {
            break;
        }
    }
    core::str::from_utf8(bytes.get(..end).unwrap_or(&[])).unwrap_or_default()
}

impl fmt::Debug for FacilityCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FacilityCode({:?})", self.as_str())
    }
}
impl fmt::Display for FacilityCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Debug for RunwayDesignator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RunwayDesignator({:?})", self.as_str())
    }
}
impl fmt::Display for RunwayDesignator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Node identifier inside an [`AirportSurfaceGraph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SurfaceNodeId(pub u32);

/// One runway threshold / departure-arrival end in an [`AirportSurfaceGraph`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunwayEndSpec {
    /// Runway-end designator (e.g. `"01"` or `"19"`).
    pub designator: RunwayDesignator,
    /// Displaced or landing threshold position on WGS-84.
    pub threshold: GeodeticPosition,
    /// True bearing along the runway centerline, radians `[0, 2π)`.
    pub true_bearing_rad: f64,
    /// Usable runway length in meters.
    pub length_m: f64,
    /// Paved/usable runway width in meters.
    pub width_m: f64,
}

/// Taxiway centerline edge linking two surface nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct TaxiEdgeSpec {
    /// Taxiway identifier (e.g. `"A"`, `"B2"`).
    pub name: String,
    /// Start node.
    pub from: SurfaceNodeId,
    /// End node.
    pub to: SurfaceNodeId,
    /// Minimum wingspan/wheelbase width clearance, meters.
    pub width_m: f64,
}

/// Runway hold-short bar node on the taxiway network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HoldShortNode {
    /// Surface graph node ID.
    pub node: SurfaceNodeId,
    /// Geodetic position of the hold-short line center.
    pub position: GeodeticPosition,
    /// Protected runway end.
    pub protects_runway: RunwayDesignator,
}

/// Classification of an apron stand, ramp spot or helipad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkingKind {
    /// Airline terminal gate.
    AirlineGate,
    /// General aviation tie-down or FBO ramp spot.
    GeneralAviationRamp,
    /// Cargo or military hardstand.
    CargoOrMilitary,
    /// Helipad / vertiport pad.
    Helipad,
}

/// Single-copy aircraft spawn / parking stand on an aerodrome surface.
#[derive(Clone, Debug, PartialEq)]
pub struct ParkingSpot {
    /// Gate or stand label (e.g. `"GATE A4"`, `"RAMP 2"`).
    pub label: String,
    /// Stand category.
    pub kind: ParkingKind,
    /// Nosewheel / center position on WGS-84.
    pub position: GeodeticPosition,
    /// Parked nose true heading in radians `[0, 2π)`.
    pub heading_rad: f64,
    /// Connected taxiway graph node, if mapped.
    pub connected_node: Option<SurfaceNodeId>,
}

/// Conflated NASR + CIFP + OSM aerodrome surface graph.
#[derive(Clone, Debug, PartialEq)]
pub struct AirportSurfaceGraph {
    /// Aerodrome identifier.
    pub facility: FacilityCode,
    /// Aerodrome reference point (ARP).
    pub reference_point: GeodeticPosition,
    /// Published runway ends.
    pub runway_ends: Vec<RunwayEndSpec>,
    /// Taxiway network edges.
    pub taxi_edges: Vec<TaxiEdgeSpec>,
    /// Runway hold-short nodes.
    pub hold_short_nodes: Vec<HoldShortNode>,
    /// Gates, stands and helipads.
    pub parking_spots: Vec<ParkingSpot>,
}

impl AirportSurfaceGraph {
    /// Looks up a [`RunwayEndSpec`] by designator.
    pub fn runway_end(&self, designator: RunwayDesignator) -> Option<&RunwayEndSpec> {
        self.runway_ends.iter().find(|r| r.designator == designator)
    }

    /// Looks up a [`ParkingSpot`] by exact stand label.
    pub fn parking_spot(&self, label: &str) -> Option<&ParkingSpot> {
        self.parking_spots.iter().find(|p| p.label == label)
    }
}

/// Lightweight resolver seam allowing weather/NOTAM consumers to resolve a
/// station/facility identifier to its [`GeodeticPosition`] without coupling
/// `world` crates to `Flightbag`.
pub trait StationLocationResolver {
    /// Resolves `facility_code` (such as `"KDOV"`) to its published WGS-84 position.
    fn resolve_facility(&self, facility_code: &str) -> Option<GeodeticPosition>;
}
