//! `no_std` + `alloc` domain contracts for ARINC 424-18 Coded Instrument Flight Procedures
//! (`FAACIFP18`): enroute airways (`ER`) and airport terminal procedures (`PD` SIDs,
//! `PE` STARs, and `PF` Instrument Approach Procedures).

use alloc::string::String;
use alloc::vec::Vec;

/// ARINC 424-18 navigation entity category referenced by an airway fix or procedure leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum CifpWaypointKind {
    /// Aerodrome reference point (`PA`).
    Airport = 0,
    /// VHF omnidirectional range / VORTAC / DME (`D `).
    VhfNavaid = 1,
    /// Non-directional beacon (`DB`).
    Ndb = 2,
    /// Enroute named RNAV/IFR waypoint (`EA`).
    EnrouteWaypoint = 3,
    /// Terminal RNAV/IFR waypoint (`PC`).
    TerminalWaypoint = 4,
    /// Surveyed runway landing threshold (`PG`, e.g. `RW22`).
    RunwayThreshold = 5,
    /// Unfixed procedural leg (e.g. `VA`, `CA`, `VM` heading/altitude leg).
    Unfixed = 255,
}

impl CifpWaypointKind {
    /// Converts a binary pack tag into a [`CifpWaypointKind`].
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Airport),
            1 => Some(Self::VhfNavaid),
            2 => Some(Self::Ndb),
            3 => Some(Self::EnrouteWaypoint),
            4 => Some(Self::TerminalWaypoint),
            5 => Some(Self::RunwayThreshold),
            255 => Some(Self::Unfixed),
            _ => None,
        }
    }

    /// Canonical single-byte pack tag.
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// Enroute ATS / RNAV airway classification derived from ARINC 424 route designator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum AirwayRouteClass {
    /// Low-altitude VOR airway (`V1`..`V999`).
    Victor = 0,
    /// High-altitude Jet route (`J1`..`J999`).
    Jet = 1,
    /// Low-altitude RNAV Tango route (`T200`..`T999`).
    Tango = 2,
    /// High-altitude RNAV Q-route (`Q1`..`Q999`).
    Quebec = 3,
    /// Oceanic, Alaska, or regional ATS route (`A`, `B`, `G`, `R`, `L`, `M`, `Y`, etc.).
    Other = 4,
}

impl AirwayRouteClass {
    /// Classifies an uppercase airway identifier (`"V3"`, `"J75"`, `"T218"`, `"Q409"`).
    pub fn from_route_ident(ident: &str) -> Self {
        match ident.as_bytes().first().copied() {
            Some(b'V') => Self::Victor,
            Some(b'J') => Self::Jet,
            Some(b'T') => Self::Tango,
            Some(b'Q') => Self::Quebec,
            _ => Self::Other,
        }
    }

    /// Canonical single-byte pack tag.
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// Airport instrument terminal procedure family (`PD`, `PE`, `PF`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ProcedureKind {
    /// Standard Instrument Departure (`PD` / `SID`).
    Sid = 0,
    /// Standard Terminal Arrival Route (`PE` / `STAR`).
    Star = 1,
    /// Instrument Approach Procedure (`PF` / `IAP`, e.g. `ILS`, `RNAV (GPS)`, `VOR`, `LOC`).
    Approach = 2,
}

impl ProcedureKind {
    /// Canonical single-byte pack tag.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Converts a binary pack tag into a [`ProcedureKind`].
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Sid),
            1 => Some(Self::Star),
            2 => Some(Self::Approach),
            _ => None,
        }
    }
}

/// Structural transition phase inside an airport [`CifpProcedure`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ProcedurePhase {
    /// Runway-specific transition (`SID` initial climb from runway, or `STAR` final split to runway).
    RunwayTransition = 0,
    /// Common core trunk (`SID`/`STAR` `"ALL"` body, or `Approach` final + missed approach body).
    CommonCore = 1,
    /// Enroute or feeder transition (`SID` exit fix, `STAR` entry fix, or `Approach` IAF transition).
    EnrouteOrFeederTransition = 2,
}

impl ProcedurePhase {
    /// Canonical single-byte pack tag.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Converts a binary pack tag into a [`ProcedurePhase`].
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::RunwayTransition),
            1 => Some(Self::CommonCore),
            2 => Some(Self::EnrouteOrFeederTransition),
            _ => None,
        }
    }
}

/// ARINC 424-18 altitude description (`col 83`) on a procedure leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AltitudeConstraintKind {
    /// No published altitude constraint.
    None = 0,
    /// Cross at exact altitude `alt1_ft` (`blank` / `@`).
    At = 1,
    /// Cross at or above `alt1_ft` (`+`).
    AtOrAbove = 2,
    /// Cross at or below `alt1_ft` (`-`).
    AtOrBelow = 3,
    /// Cross at or below `alt1_ft` and at or above `alt2_ft` (`B`).
    Between = 4,
}

impl AltitudeConstraintKind {
    /// Canonical single-byte pack tag.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Converts a binary pack tag into an [`AltitudeConstraintKind`].
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::None),
            1 => Some(Self::At),
            2 => Some(Self::AtOrAbove),
            3 => Some(Self::AtOrBelow),
            4 => Some(Self::Between),
            _ => None,
        }
    }
}

/// Two-character ARINC 424-18 leg path and terminator code (`cols 48..=49`, e.g. `IF`, `TF`, `DF`, `RF`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathTerminator(pub [u8; 2]);

impl PathTerminator {
    /// Initial Fix (`IF`).
    pub const IF: Self = Self(*b"IF");
    /// Track to a Fix (`TF`).
    pub const TF: Self = Self(*b"TF");
    /// Direct to a Fix (`DF`).
    pub const DF: Self = Self(*b"DF");
    /// Course to a Fix (`CF`).
    pub const CF: Self = Self(*b"CF");
    /// Constant Radius Arc to a Fix (`RF`).
    pub const RF: Self = Self(*b"RF");

    /// Parses a two-character ASCII path terminator (`"TF"`, `"IF"`, `"HM"`, etc.).
    pub fn from_str_2(raw: &str) -> Self {
        let bytes = raw.as_bytes();
        let b0 = bytes.first().copied().unwrap_or(b' ').to_ascii_uppercase();
        let b1 = bytes.get(1).copied().unwrap_or(b' ').to_ascii_uppercase();
        Self([b0, b1])
    }

    /// ASCII representation (`"IF"`, `"TF"`, `"VA"`, etc.).
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).unwrap_or("  ")
    }
}

/// Ordered fix along an enroute airway (`ER` primary record).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CifpAirwayFix {
    /// ARINC 424 sequence number (`cols 26..=29`).
    pub seq_num: u16,
    /// Waypoint or navaid identifier (`"SBJ"`, `"MXE"`, `"VILLS"`).
    pub fix_ident: String,
    /// Two-character ICAO region code (`*b"K6"`, `*b"PA"`).
    pub icao_region: [u8; 2],
    /// Waypoint/navaid category.
    pub kind: CifpWaypointKind,
    /// WGS-84 latitude in micro-degrees (`deg * 1_000_000`).
    pub latitude_e6: i32,
    /// WGS-84 longitude in micro-degrees (`deg * 1_000_000`).
    pub longitude_e6: i32,
    /// Minimum enroute altitude (`MEA`) outbound from this fix in feet (`0` when unspecified).
    pub min_alt_ft: u32,
    /// Maximum authorized altitude (`MAA`) outbound from this fix in feet (`0` when unspecified).
    pub max_alt_ft: u32,
    /// `true` when the airway terminates or breaks into a disconnected regional segment after this fix
    /// (e.g. Hawaii `V3` ending at `PARIS` before East Coast `V3` begins at `EYW`).
    pub segment_break_after: bool,
}

/// Compiled enroute airway (`V`, `J`, `T`, `Q`, or ATS route) with ordered fixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CifpAirway {
    /// Route designator (`"V3"`, `"J75"`, `"T218"`, `"Q409"`).
    pub route_ident: String,
    /// Route altitude/navigation class.
    pub route_class: AirwayRouteClass,
    /// Ordered airway fixes in ARINC sequence order.
    pub fixes: Vec<CifpAirwayFix>,
}

impl CifpAirway {
    /// Expands the contiguous airway segment from `entry_ident` to `exit_ident` (inclusive of
    /// intermediate fixes and `exit_ident`, excluding `entry_ident` if already in the flight plan,
    /// or returning the full inclusive slice `[entry..=exit]`).
    ///
    /// Returns the inclusive ordered fix slice `[entry..=exit]` in direction of flight (forward or
    /// reverse), or `None` if either fix is absent or separated by a [`CifpAirwayFix::segment_break_after`].
    pub fn expand_inclusive(
        &self,
        entry_ident: &str,
        exit_ident: &str,
    ) -> Option<Vec<&CifpAirwayFix>> {
        if entry_ident.is_empty() || exit_ident.is_empty() || entry_ident == exit_ident {
            return None;
        }
        // Partition into contiguous connected sub-chains so duplicate designators (e.g. Hawaii vs East Coast V3)
        // never bridge across an unchartable break.
        let mut seg_start = 0_usize;
        while seg_start < self.fixes.len() {
            let mut seg_end = seg_start;
            while let Some(fx) = self.fixes.get(seg_end) {
                if fx.segment_break_after || seg_end + 1 >= self.fixes.len() {
                    break;
                }
                seg_end += 1;
            }
            if let Some(sub) = self.fixes.get(seg_start..=seg_end) {
                let entry_pos = sub.iter().position(|f| f.fix_ident == entry_ident);
                let exit_pos = sub.iter().position(|f| f.fix_ident == exit_ident);
                if let (Some(i0), Some(i1)) = (entry_pos, exit_pos)
                    && i0 != i1
                {
                    if i0 < i1 {
                        if let Some(slice) = sub.get(i0..=i1) {
                            return Some(slice.iter().collect());
                        }
                    } else if let Some(slice) = sub.get(i1..=i0) {
                        return Some(slice.iter().rev().collect());
                    }
                }
            }
            seg_start = seg_end + 1;
        }
        None
    }
}

/// Single ARINC 424-18 leg inside a [`CifpTransition`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CifpProcedureLeg {
    /// ARINC 424 leg sequence (`cols 27..=29`).
    pub seq_num: u16,
    /// Terminating fix identifier (`""` on heading/altitude-terminated legs such as `VA` or `CA`).
    pub fix_ident: String,
    /// Terminating fix category.
    pub kind: CifpWaypointKind,
    /// Two-letter ARINC 424 path and terminator (`IF`, `TF`, `DF`, `CF`, `RF`, `VA`, `CA`, `HM`).
    pub path_terminator: PathTerminator,
    /// `true` when `latitude_e6` and `longitude_e6` carry resolved WGS-84 coordinates.
    pub has_coordinates: bool,
    /// WGS-84 latitude in micro-degrees (`0` when `!has_coordinates`).
    pub latitude_e6: i32,
    /// WGS-84 longitude in micro-degrees (`0` when `!has_coordinates`).
    pub longitude_e6: i32,
    /// Altitude constraint qualifier.
    pub alt_constraint: AltitudeConstraintKind,
    /// Primary constraint altitude in feet (`FL200` normalized to `20_000`).
    pub alt1_ft: u32,
    /// Secondary (lower `"Between"`) altitude in feet.
    pub alt2_ft: u32,
    /// Maximum IAS speed constraint in knots (`0` when unrestricted).
    pub speed_limit_kt: u16,
    /// `true` for missed-approach legs at or after the Missed Approach Point (`MAPt`).
    pub is_missed_approach: bool,
    /// `true` when flagged as an Initial Approach Fix (`IAF`).
    pub is_iaf: bool,
    /// `true` when flagged as the Final Approach Fix (`FAF`).
    pub is_faf: bool,
    /// `true` when flagged as the Missed Approach Point (`MAPt`).
    pub is_mapt: bool,
}

/// Named runway, common-core, or enroute/IAF transition inside a [`CifpProcedure`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CifpTransition {
    /// Transition identifier (`"RW22B"`, `"BIGGY"`, `"PIYIN"`, `"ALL"`, or `""` for unnamed core).
    pub transition_ident: String,
    /// Structural transition phase (`RunwayTransition`, `CommonCore`, or `EnrouteOrFeederTransition`).
    pub phase: ProcedurePhase,
    /// Raw ARINC 424 route type byte (`col 20`).
    pub route_type: u8,
    /// Ordered procedure legs.
    pub legs: Vec<CifpProcedureLeg>,
}

/// Airport terminal instrument procedure (`SID`, `STAR`, or `Approach`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CifpProcedure {
    /// Host airport identifier (`"KEWR"`, `"N51"`, `"KHSV"`).
    pub airport_ident: String,
    /// Procedure family (`Sid`, `Star`, `Approach`).
    pub kind: ProcedureKind,
    /// Published ARINC procedure identifier (`"PORTT4"`, `"PHLBO4"`, `"R22"`, `"H04RZ"`).
    pub procedure_ident: String,
    /// Representative ARINC 424 route type byte (`b'R'` RNAV, `b'I'` ILS, `b'H'` RNP, etc.).
    pub route_type: u8,
    /// Ordered transitions belonging to this procedure.
    pub transitions: Vec<CifpTransition>,
}

impl CifpProcedure {
    /// Assembles a deduplicated lateral leg sequence for the selected transition(s).
    ///
    /// - For a `Sid`: `RunwayTransition` (`primary_transition`) -> `CommonCore` -> `EnrouteOrFeederTransition` (`secondary_transition`).
    /// - For a `Star`: `EnrouteOrFeederTransition` (`primary_transition`) -> `CommonCore` -> `RunwayTransition` (`secondary_transition`).
    /// - For an `Approach`: `EnrouteOrFeederTransition` (`primary_transition`) -> `CommonCore` (filtering missed approach unless `include_missed`).
    pub fn assemble_legs(
        &self,
        primary_transition: Option<&str>,
        secondary_transition: Option<&str>,
        include_missed: bool,
    ) -> Vec<&CifpProcedureLeg> {
        let mut chosen: Vec<&CifpTransition> = Vec::new();
        match self.kind {
            ProcedureKind::Sid => {
                if let Some(rwy_t) = primary_transition.and_then(|id| {
                    self.transitions.iter().find(|t| {
                        t.phase == ProcedurePhase::RunwayTransition && t.transition_ident == id
                    })
                }) {
                    chosen.push(rwy_t);
                }
                for core_t in self
                    .transitions
                    .iter()
                    .filter(|t| t.phase == ProcedurePhase::CommonCore)
                {
                    chosen.push(core_t);
                }
                if let Some(enr_t) = secondary_transition.and_then(|id| {
                    self.transitions.iter().find(|t| {
                        t.phase == ProcedurePhase::EnrouteOrFeederTransition
                            && t.transition_ident == id
                    })
                }) {
                    chosen.push(enr_t);
                }
            }
            ProcedureKind::Star => {
                if let Some(enr_t) = primary_transition.and_then(|id| {
                    self.transitions.iter().find(|t| {
                        t.phase == ProcedurePhase::EnrouteOrFeederTransition
                            && t.transition_ident == id
                    })
                }) {
                    chosen.push(enr_t);
                }
                for core_t in self
                    .transitions
                    .iter()
                    .filter(|t| t.phase == ProcedurePhase::CommonCore)
                {
                    chosen.push(core_t);
                }
                if let Some(rwy_t) = secondary_transition.and_then(|id| {
                    self.transitions.iter().find(|t| {
                        t.phase == ProcedurePhase::RunwayTransition && t.transition_ident == id
                    })
                }) {
                    chosen.push(rwy_t);
                }
            }
            ProcedureKind::Approach => {
                if let Some(iaf_t) = primary_transition.and_then(|id| {
                    self.transitions.iter().find(|t| {
                        t.phase == ProcedurePhase::EnrouteOrFeederTransition
                            && t.transition_ident == id
                    })
                }) {
                    chosen.push(iaf_t);
                }
                for core_t in self
                    .transitions
                    .iter()
                    .filter(|t| t.phase == ProcedurePhase::CommonCore)
                {
                    chosen.push(core_t);
                }
            }
        }

        let mut out: Vec<&CifpProcedureLeg> = Vec::new();
        for tr in chosen {
            for leg in &tr.legs {
                if !include_missed && leg.is_missed_approach {
                    continue;
                }
                // Deduplicate seam fixes where transition ends at X (`TF X`) and next phase begins `IF X`.
                let is_seam_dup = !leg.fix_ident.is_empty()
                    && out
                        .last()
                        .is_some_and(|prev| prev.fix_ident == leg.fix_ident);
                if !is_seam_dup {
                    out.push(leg);
                }
            }
        }
        out
    }
}
