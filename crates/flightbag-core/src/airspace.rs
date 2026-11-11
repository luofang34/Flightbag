//! Cycle-dated 3D Controlled Airspace (`Class B..E`) and Special Use Airspace (`SUA`) contracts.

use alloc::string::String;
use alloc::vec::Vec;
use world_core::GeodeticPosition;

use crate::FlightbagError;

/// Exact SI conversion factor from international feet to metres.
pub const METRES_PER_FOOT: f64 = 0.3048;

/// ICAO / FAA controlled airspace letter class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlledAirspaceClass {
    /// Class B terminal radar service area around primary hub airports.
    ClassB,
    /// Class C terminal radar airspace with inner core and outer shelf.
    ClassC,
    /// Class D towered airport surface cylinder/keyhole.
    ClassD,
    /// Class E controlled airspace (surface, extension, transition, or enroute).
    ClassE,
}

impl ControlledAirspaceClass {
    /// Canonical single-letter code (`"B"`, `"C"`, `"D"`, `"E"`).
    pub const fn code(self) -> &'static str {
        match self {
            Self::ClassB => "B",
            Self::ClassC => "C",
            Self::ClassD => "D",
            Self::ClassE => "E",
        }
    }
}

/// Specific FAA NASR controlled airspace sub-type used for VFR vs IFR chart portrayal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlledLocalType {
    /// Class B airspace shelf or surface core (`CLASS_B`).
    ClassB,
    /// Class C airspace shelf or surface core (`CLASS_C`).
    ClassC,
    /// Class D surface airspace (`CLASS_D`).
    ClassD,
    /// Class E surface area designated for an airport (`CLASS_E2`).
    ClassE2Surface,
    /// Class E surface area arrival extension (`CLASS_E3`).
    ClassE3Extension,
    /// Class E surface area airway/approach extension (`CLASS_E4`).
    ClassE4Extension,
    /// Class E transition area starting at `700 ft` or `1,200 ft` AGL (`CLASS_E5`).
    ClassE5Transition,
    /// Class E enroute domestic airspace (`CLASS_E6`).
    ClassE6Enroute,
    /// Offshore or other controlled airspace designation.
    ClassEOther,
}

impl ControlledLocalType {
    /// Canonical stable string tag emitted into vector tiles and manifests.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::ClassB => "CLASS_B",
            Self::ClassC => "CLASS_C",
            Self::ClassD => "CLASS_D",
            Self::ClassE2Surface => "CLASS_E2",
            Self::ClassE3Extension => "CLASS_E3",
            Self::ClassE4Extension => "CLASS_E4",
            Self::ClassE5Transition => "CLASS_E5",
            Self::ClassE6Enroute => "CLASS_E6",
            Self::ClassEOther => "CLASS_E_OTHER",
        }
    }
}

/// FAA Special Use Airspace (SUA) category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SuaKind {
    /// Prohibited Area (`P-`), flight prohibited without explicit authorization.
    Prohibited,
    /// Restricted Area (`R-`), unusual/invisible hazards such as artillery or missiles.
    Restricted,
    /// Warning Area (`W-`), hazardous activity over international/coastal waters.
    Warning,
    /// Military Operations Area (`MOA`), separating IFR traffic from military training.
    Moa,
    /// Alert Area (`A-`), high volume of pilot training or unusual aerial activity.
    Alert,
    /// National Security Area (`NSA`), requested voluntary avoidance or NOTAM restriction.
    NationalSecurityArea,
}

impl SuaKind {
    /// Short chart class code (`"P"`, `"R"`, `"W"`, `"MOA"`, `"A"`, `"NSA"`).
    pub const fn code(self) -> &'static str {
        match self {
            Self::Prohibited => "P",
            Self::Restricted => "R",
            Self::Warning => "W",
            Self::Moa => "MOA",
            Self::Alert => "A",
            Self::NationalSecurityArea => "NSA",
        }
    }

    /// Canonical descriptive tag (`"PROHIBITED"`, `"RESTRICTED"`, etc.).
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Prohibited => "PROHIBITED",
            Self::Restricted => "RESTRICTED",
            Self::Warning => "WARNING",
            Self::Moa => "MOA",
            Self::Alert => "ALERT",
            Self::NationalSecurityArea => "NSA",
        }
    }
}

/// Top-level classification of a static NASR airspace volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AirspaceClassification {
    /// Controlled airspace (`Class B`, `C`, `D`, `E`).
    Controlled {
        /// Primary airspace class letter.
        class: ControlledAirspaceClass,
        /// Specific NASR sub-classification (`CLASS_E2`, `CLASS_E5`, etc.).
        local_type: ControlledLocalType,
    },
    /// Special Use Airspace (`P`, `R`, `W`, `MOA`, `A`, `NSA`).
    SpecialUse(SuaKind),
}

impl AirspaceClassification {
    /// Broad category tag (`"controlled"` or `"sua"`).
    pub const fn category_tag(self) -> &'static str {
        match self {
            Self::Controlled { .. } => "controlled",
            Self::SpecialUse(_) => "sua",
        }
    }

    /// Primary class code (`"B"`, `"C"`, `"D"`, `"E"`, `"P"`, `"R"`, `"W"`, `"MOA"`, `"A"`, `"NSA"`).
    pub const fn class_code(self) -> &'static str {
        match self {
            Self::Controlled { class, .. } => class.code(),
            Self::SpecialUse(sua) => sua.code(),
        }
    }

    /// Detailed type tag (`"CLASS_B"`, `"CLASS_E2"`, `"CLASS_E5"`, `"RESTRICTED"`, `"MOA"`, etc.).
    pub const fn local_type_tag(self) -> &'static str {
        match self {
            Self::Controlled { local_type, .. } => local_type.tag(),
            Self::SpecialUse(sua) => sua.tag(),
        }
    }
}

/// Vertical reference datum for an airspace floor or ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AltitudeDatum {
    /// Ground or water surface (`0 ft AGL`).
    Surface,
    /// Height above local terrain surface (`AGL`).
    Agl,
    /// Altitude above mean sea level (`MSL`).
    Msl,
    /// Standard pressure altitude flight level (`FL`, `1013.25 hPa` / `29.92 inHg`).
    FlightLevel,
    /// Unbounded upper limit (`UNL`).
    Unlimited,
}

impl AltitudeDatum {
    /// Short datum tag (`"sfc"`, `"agl"`, `"msl"`, `"fl"`, `"unl"`).
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Surface => "sfc",
            Self::Agl => "agl",
            Self::Msl => "msl",
            Self::FlightLevel => "fl",
            Self::Unlimited => "unl",
        }
    }

    /// Renderer vertical-datum token (`"agl"` for terrain-relative limits, `"msl"` otherwise).
    pub const fn renderer_datum(self) -> &'static str {
        match self {
            Self::Surface | Self::Agl => "agl",
            Self::Msl | Self::FlightLevel | Self::Unlimited => "msl",
        }
    }
}

/// Floor or ceiling altitude of an airspace volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VerticalLimit {
    /// Vertical reference datum.
    pub datum: AltitudeDatum,
    /// Altitude value in feet (`0` for [`AltitudeDatum::Surface`], `fl * 100` for flight levels).
    pub value_ft: i32,
}

impl VerticalLimit {
    /// Surface (`SFC`, `0 ft AGL`).
    pub const SURFACE: Self = Self {
        datum: AltitudeDatum::Surface,
        value_ft: 0,
    };

    /// Constructs a limit in feet above ground level (`AGL`).
    pub const fn agl_ft(feet: i32) -> Self {
        if feet <= 0 {
            Self::SURFACE
        } else {
            Self {
                datum: AltitudeDatum::Agl,
                value_ft: feet,
            }
        }
    }

    /// Constructs a limit in feet above mean sea level (`MSL`).
    pub const fn msl_ft(feet: i32) -> Self {
        Self {
            datum: AltitudeDatum::Msl,
            value_ft: feet,
        }
    }

    /// Constructs a standard-pressure flight level (`FL180` -> `fl = 180`, `value_ft = 18_000`).
    pub const fn flight_level(fl: u16) -> Self {
        Self {
            datum: AltitudeDatum::FlightLevel,
            value_ft: (fl as i32) * 100,
        }
    }

    /// Constructs an unlimited ceiling capped at `nominal_cap_ft` (typically `60_000 ft`) for 3D rendering.
    pub const fn unlimited(nominal_cap_ft: i32) -> Self {
        Self {
            datum: AltitudeDatum::Unlimited,
            value_ft: nominal_cap_ft,
        }
    }

    /// Geometric height/altitude in SI metres (`value_ft * 0.3048`).
    pub fn metres(self) -> f64 {
        f64::from(self.value_ft) * METRES_PER_FOOT
    }
}

/// Single polygon component (exterior boundary plus optional interior cutout holes) on WGS-84.
#[derive(Clone, Debug, PartialEq)]
pub struct AirspacePolygon {
    /// Outer boundary vertices (`lat_deg`, `lon_deg`), at least 3 vertices.
    pub exterior: Vec<GeodeticPosition>,
    /// Optional interior cutout rings (holes).
    pub interiors: Vec<Vec<GeodeticPosition>>,
}

impl AirspacePolygon {
    /// Validates ring vertex counts and coordinate finiteness/ranges.
    ///
    /// # Errors
    ///
    /// Returns [`FlightbagError::InvalidGeometry`] if any ring has fewer than 3 points or invalid coordinates.
    pub fn validate(&self) -> Result<(), FlightbagError> {
        validate_ring(&self.exterior)?;
        for hole in &self.interiors {
            validate_ring(hole)?;
        }
        Ok(())
    }
}

fn validate_ring(ring: &[GeodeticPosition]) -> Result<(), FlightbagError> {
    if ring.len() < 3 {
        return Err(FlightbagError::InvalidGeometry);
    }
    for pos in ring {
        let valid_lat = pos.latitude_deg.is_finite() && (-90.0..=90.0).contains(&pos.latitude_deg);
        let valid_lon =
            pos.longitude_deg.is_finite() && (-180.0..=180.0).contains(&pos.longitude_deg);
        if !valid_lat || !valid_lon {
            return Err(FlightbagError::InvalidGeometry);
        }
    }
    Ok(())
}

/// A 3D airspace volume (single shelf, cylinder, or SUA sector) published in an AIRAC cycle.
#[derive(Clone, Debug, PartialEq)]
pub struct AirspaceVolume {
    /// Canonical key linking static cycle geometry to live notices (`world_notices::AirspaceKey`).
    pub airspace_key: String,
    /// Primary facility or SUA identifier (e.g. `"SEA"`, `"R-6701"`, `"OLYMPIC A"`).
    pub ident: String,
    /// Official descriptive airspace name (e.g. `"SEATTLE CLASS B"`, `"R-6701 FORT LEWIS, WA"`).
    pub name: String,
    /// Optional sub-sector designator within a multi-shelf airspace (e.g. `"AREA A"`).
    pub sector: Option<String>,
    /// Controlled or Special Use Airspace classification.
    pub classification: AirspaceClassification,
    /// Floor limit.
    pub lower: VerticalLimit,
    /// Ceiling limit.
    pub upper: VerticalLimit,
    /// Published effective hours or schedule summary (e.g. `"CONT"`, `"BY NOTAM"`).
    pub hours_of_use: Option<String>,
    /// Controlling ATC facility or agency, when published.
    pub controlling_agency: Option<String>,
    /// Lateral WGS-84 polygon components (already exclusion-cut and shelf-normalized).
    pub polygons: Vec<AirspacePolygon>,
}

impl AirspaceVolume {
    /// Validates non-empty identifiers, vertical ordering, and polygon geometry.
    ///
    /// # Errors
    ///
    /// Returns [`FlightbagError`] if identifiers or lateral/vertical bounds are malformed.
    pub fn validate(&self) -> Result<(), FlightbagError> {
        if self.airspace_key.trim().is_empty() || self.name.trim().is_empty() {
            return Err(FlightbagError::InvalidIdentifier);
        }
        if self.polygons.is_empty() {
            return Err(FlightbagError::InvalidGeometry);
        }
        if self.lower.datum == self.upper.datum && self.upper.value_ft < self.lower.value_ft {
            return Err(FlightbagError::InvalidGeometry);
        }
        for polygon in &self.polygons {
            polygon.validate()?;
        }
        Ok(())
    }
}
