//! 28-day ICAO / FAA AIRAC cycle arithmetic (`09:01 UTC` activation).

use core::fmt;
use world_core::{DecimalYear, ValidityWindow};

/// Exact duration of one 28-day AIRAC cycle in seconds (`28 * 86_400`).
pub const AIRAC_CYCLE_SECONDS: i64 = 28 * 86_400;

/// Anchor AIRAC cycle `2001` effective instant: `2020-01-02T09:01:00Z` (`1_577_955_660` Unix seconds).
const EPOCH_2001_UNIX_S: i64 = 1_577_955_660;

/// Errors returned when validating `Flightbag` identifiers or manifests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlightbagError {
    /// Identifier contains invalid characters or length.
    InvalidIdentifier,
    /// SHA-256 hex digest was not 64 lowercase hexadecimal ASCII characters.
    InvalidSha256Hex,
    /// Runway or bounding geometry is malformed.
    InvalidGeometry,
}

impl fmt::Display for FlightbagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidIdentifier => "flightbag identifier is empty or invalid ASCII",
            Self::InvalidSha256Hex => "digest must be 64 lowercase hexadecimal ASCII bytes",
            Self::InvalidGeometry => "aerodrome or chart geometry is malformed",
        })
    }
}

impl core::error::Error for FlightbagError {}

/// A 28-day AIRAC cycle identified by its sequential index from cycle `2001` (`index = 0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AiracCycle {
    /// Zero-based 28-day cycle index anchored at `2020-01-02T09:01:00Z`.
    index_from_2001: i32,
}

impl AiracCycle {
    /// Cycle `2001` (`2020-01-02T09:01:00Z`).
    pub const CYCLE_2001: Self = Self { index_from_2001: 0 };

    /// Constructs a cycle from its sequential index relative to `2001`.
    pub const fn from_index(index_from_2001: i32) -> Self {
        Self { index_from_2001 }
    }

    /// Sequential index relative to `2001`.
    pub const fn index(self) -> i32 {
        self.index_from_2001
    }

    /// Returns the AIRAC cycle active at `unix_seconds` (`09:01 UTC` cutover).
    pub fn active_at_unix_seconds(unix_seconds: i64) -> Self {
        let delta = unix_seconds - EPOCH_2001_UNIX_S;
        let idx = delta.div_euclid(AIRAC_CYCLE_SECONDS) as i32;
        Self {
            index_from_2001: idx,
        }
    }

    /// Effective interval `[start_unix_s, end_unix_s)` in UTC Unix seconds.
    pub const fn effective_unix_seconds(self) -> (i64, i64) {
        let start = EPOCH_2001_UNIX_S + (self.index_from_2001 as i64) * AIRAC_CYCLE_SECONDS;
        (start, start + AIRAC_CYCLE_SECONDS)
    }

    /// Inclusive [`ValidityWindow`] in [`DecimalYear`] for cross-checking with `world` models.
    pub fn validity_window(self) -> ValidityWindow {
        let (start, end) = self.effective_unix_seconds();
        ValidityWindow {
            from: DecimalYear::from_unix_seconds(start),
            until: DecimalYear::from_unix_seconds(end - 1),
        }
    }

    /// Immediately following 28-day AIRAC cycle.
    pub const fn next(self) -> Self {
        Self {
            index_from_2001: self.index_from_2001.saturating_add(1),
        }
    }

    /// Immediately preceding 28-day AIRAC cycle.
    pub const fn previous(self) -> Self {
        Self {
            index_from_2001: self.index_from_2001.saturating_sub(1),
        }
    }
}
