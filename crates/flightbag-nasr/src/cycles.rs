//! 28-day AIRAC cycle resolution and FAA NASR subscription URL formatting.

use flightbag_core::AiracCycle;
use thiserror::Error;
use time::macros::date;
use time::{Date, Duration, Month, OffsetDateTime, UtcOffset};

/// Anchor date of AIRAC cycle `2001` (`2020-01-02`).
pub const AIRAC_EPOCH_DATE: Date = date!(2020 - 01 - 02);

/// Length of one AIRAC cycle in calendar days.
pub const CYCLE_DAYS: i32 = 28;

/// User-supplied cycle selector (`current`, `upcoming`, or explicit `YYYY-MM-DD`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleSpec {
    /// Currently effective 28-day AIRAC cycle at wall-clock UTC (`09:01Z` cutover).
    Current,
    /// Next 28-day AIRAC cycle immediately following [`Self::Current`].
    Upcoming,
    /// Explicit effective date (`YYYY-MM-DD`) on the 28-day AIRAC grid.
    EffectiveDate(Date),
}

impl CycleSpec {
    /// Parses `"current"`, `"upcoming"`, or `"YYYY-MM-DD"`.
    ///
    /// # Errors
    ///
    /// Returns [`NasrCycleError`] if `text` is neither a known keyword nor a valid ISO date.
    pub fn parse(text: &str) -> Result<Self, NasrCycleError> {
        match text.trim() {
            "current" => Ok(Self::Current),
            "upcoming" => Ok(Self::Upcoming),
            other => parse_iso_date(other)
                .map(Self::EffectiveDate)
                .ok_or_else(|| NasrCycleError::InvalidDate(other.to_owned())),
        }
    }
}

/// Resolved 28-day AIRAC cycle metadata and official FAA NASR download URL.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedCycle {
    /// Core `Flightbag` cycle handle.
    #[serde(skip)]
    pub airac: AiracCycle,
    /// Sequential index since `2020-01-02` (`2001` = `0`).
    pub cycle_index: i32,
    /// Four-digit ICAO/FAA `YYCC` cycle code (e.g. `"2611"`).
    pub airac_code: String,
    /// Effective date (`YYYY-MM-DD`).
    pub effective_date: String,
    /// Next cycle's effective date (`YYYY-MM-DD`), i.e. expiration date at `09:01Z`.
    pub expires_date: String,
    /// Inclusive UTC activation timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_from_utc: String,
    /// Exclusive UTC expiration timestamp (`YYYY-MM-DDT09:01:00Z`).
    pub effective_until_utc: String,
    /// Official FAA 28-day NASR subscription archive URL.
    pub nasr_url: String,
    /// Canonical archive filename (`28DaySubscription_Effective_YYYY-MM-DD.zip`).
    pub nasr_filename: String,
}

/// Errors encountered when resolving 28-day AIRAC cycle dates.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum NasrCycleError {
    /// String was not `current`, `upcoming`, or `YYYY-MM-DD`.
    #[error("invalid cycle specifier '{0}'; expected 'current', 'upcoming', or YYYY-MM-DD")]
    InvalidDate(String),
    /// Date precedes `2020-01-02`.
    #[error("date {0} predates AIRAC 2001 epoch (2020-01-02)")]
    PreEpoch(String),
    /// Date does not fall on the 28-day Thursday AIRAC grid.
    #[error("date {actual} is not on the 28-day AIRAC grid (nearest grid date: {nearest})")]
    OffGrid {
        /// Supplied date.
        actual: String,
        /// Governing grid date on or before `actual`.
        nearest: String,
    },
    /// Calendar date range overflowed `time::Date`.
    #[error("calendar date arithmetic overflowed")]
    Overflow,
}

/// Resolves `spec` relative to `now_utc`.
///
/// # Errors
///
/// Returns [`NasrCycleError`] if an explicit date is off-grid or before `2020-01-02`.
pub fn resolve_cycle(
    spec: CycleSpec,
    now_utc: OffsetDateTime,
) -> Result<ResolvedCycle, NasrCycleError> {
    let airac = match spec {
        CycleSpec::Current => AiracCycle::active_at_unix_seconds(now_utc.unix_timestamp()),
        CycleSpec::Upcoming => AiracCycle::active_at_unix_seconds(now_utc.unix_timestamp()).next(),
        CycleSpec::EffectiveDate(date) => airac_from_effective_date(date)?,
    };
    resolved_from_airac(airac)
}

/// Converts an exact 28-day grid `Date` into an [`AiracCycle`].
///
/// # Errors
///
/// Returns [`NasrCycleError`] if `effective` predates `2020-01-02` or is off-grid.
pub fn airac_from_effective_date(effective: Date) -> Result<AiracCycle, NasrCycleError> {
    let elapsed = effective.to_julian_day() - AIRAC_EPOCH_DATE.to_julian_day();
    if elapsed < 0 {
        return Err(NasrCycleError::PreEpoch(format_iso_date(effective)));
    }
    if elapsed % CYCLE_DAYS != 0 {
        let nearest_jd =
            AIRAC_EPOCH_DATE.to_julian_day() + elapsed - elapsed.rem_euclid(CYCLE_DAYS);
        let nearest = Date::from_julian_day(nearest_jd)
            .map(format_iso_date)
            .unwrap_or_default();
        return Err(NasrCycleError::OffGrid {
            actual: format_iso_date(effective),
            nearest,
        });
    }
    Ok(AiracCycle::from_index(elapsed / CYCLE_DAYS))
}

/// Builds complete [`ResolvedCycle`] metadata and FAA NASR URL from an [`AiracCycle`].
///
/// # Errors
///
/// Returns [`NasrCycleError::Overflow`] if the cycle index overflows `time::Date`.
pub fn resolved_from_airac(airac: AiracCycle) -> Result<ResolvedCycle, NasrCycleError> {
    if airac.index() < 0 {
        return Err(NasrCycleError::PreEpoch(format!("index={}", airac.index())));
    }
    let effective_jd = AIRAC_EPOCH_DATE
        .to_julian_day()
        .checked_add(airac.index().saturating_mul(CYCLE_DAYS))
        .ok_or(NasrCycleError::Overflow)?;
    let effective = Date::from_julian_day(effective_jd).map_err(|_| NasrCycleError::Overflow)?;
    let expires = effective
        .checked_add(Duration::days(i64::from(CYCLE_DAYS)))
        .ok_or(NasrCycleError::Overflow)?;

    let effective_date = format_iso_date(effective);
    let expires_date = format_iso_date(expires);
    let airac_code = compute_airac_code(effective)?;
    let nasr_filename = format!("28DaySubscription_Effective_{effective_date}.zip");
    let nasr_url = format!("https://nfdc.faa.gov/webContent/28DaySub/{nasr_filename}");

    Ok(ResolvedCycle {
        airac,
        cycle_index: airac.index(),
        airac_code,
        effective_from_utc: format!("{effective_date}T09:01:00Z"),
        effective_until_utc: format!("{expires_date}T09:01:00Z"),
        effective_date,
        expires_date,
        nasr_url,
        nasr_filename,
    })
}

/// Formats `date` as `YYYY-MM-DD`.
pub fn format_iso_date(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Parses `YYYY-MM-DD` into a [`Date`].
pub fn parse_iso_date(value: &str) -> Option<Date> {
    let mut parts = value.splitn(3, '-');
    let year_s = parts.next()?;
    let month_s = parts.next()?;
    let day_s = parts.next()?;
    if year_s.len() != 4 || month_s.len() != 2 || day_s.len() != 2 {
        return None;
    }
    let year: i32 = year_s.parse().ok()?;
    let month_num: u8 = month_s.parse().ok()?;
    let day: u8 = day_s.parse().ok()?;
    let month = Month::try_from(month_num).ok()?;
    Date::from_calendar_date(year, month, day).ok()
}

/// Returns current wall-clock UTC [`OffsetDateTime`].
pub fn now_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc().to_offset(UtcOffset::UTC)
}

fn compute_airac_code(effective: Date) -> Result<String, NasrCycleError> {
    let jan_1 = Date::from_calendar_date(effective.year(), Month::January, 1)
        .map_err(|_| NasrCycleError::Overflow)?;
    let offset = jan_1.to_julian_day() - AIRAC_EPOCH_DATE.to_julian_day();
    let rem = offset.rem_euclid(CYCLE_DAYS);
    let first_in_year = AIRAC_EPOCH_DATE.to_julian_day()
        + offset.div_euclid(CYCLE_DAYS) * CYCLE_DAYS
        + if rem == 0 { 0 } else { CYCLE_DAYS };
    let ordinal = (effective.to_julian_day() - first_in_year) / CYCLE_DAYS + 1;
    Ok(format!(
        "{:02}{ordinal:02}",
        effective.year().rem_euclid(100)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_2026_10_29_and_upcoming_2026_11_26() {
        let d = parse_iso_date("2026-10-29");
        assert!(d.is_some());
        let Some(date_val) = d else {
            return;
        };
        let resolved = resolve_cycle(
            CycleSpec::EffectiveDate(date_val),
            OffsetDateTime::UNIX_EPOCH,
        );
        assert!(resolved.is_ok());
        let Ok(cur) = resolved else {
            return;
        };
        assert_eq!(cur.effective_date, "2026-10-29");
        assert_eq!(cur.expires_date, "2026-11-26");
        assert_eq!(cur.airac_code, "2611");
        assert_eq!(
            cur.nasr_url,
            "https://nfdc.faa.gov/webContent/28DaySub/28DaySubscription_Effective_2026-10-29.zip"
        );
    }
}
