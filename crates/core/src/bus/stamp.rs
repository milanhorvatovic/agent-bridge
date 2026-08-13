//! The envelope's two clock readings, taken where the sequence number is.
//!
//! The events crate deliberately carries no time dependency — its types are
//! the contract, and a contract does not need a clock — so the formatting
//! lives here, at the one place envelopes are completed. Hand-rolled rather
//! than a workspace time crate: the envelope needs exactly one output shape
//! (RFC 3339, milliseconds, UTC), and twenty lines of date arithmetic are a
//! smaller surface than a calendar library's. The dev-task runner carries
//! its own copy of the same civil-calendar arithmetic by design — it is
//! allowed exactly one dependency and no workspace crates, so it cannot
//! borrow this one; each copy is pinned by its own tests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A [`SystemTime`] as the envelope's `ts` field: RFC 3339 with millisecond
/// resolution, always UTC (`2026-08-13T09:41:00.123Z`).
///
/// A reading before the Unix epoch formats as the epoch itself rather than
/// failing: the field is documented as not an ordering key precisely
/// because wall clocks misbehave, so a publish must not fail on one that
/// does.
pub(crate) fn rfc3339_millis(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let seconds = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let second_of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
    )
}

/// Days since 1970-01-01 to a proleptic-Gregorian (year, month, day).
///
/// Howard Hinnant's `civil_from_days`, restricted to the post-epoch range
/// the caller guarantees, which is why the arithmetic stays unsigned.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let days = days + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let (year_offset, month) = if shifted_month < 10 {
        (0, shifted_month + 3)
    } else {
        (1, shifted_month - 9)
    };
    (year_of_era + era * 400 + year_offset, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64, millis: u32) -> String {
        rfc3339_millis(UNIX_EPOCH + Duration::new(seconds, millis * 1_000_000))
    }

    #[test]
    fn the_epoch_itself() {
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn known_instants_round_trip() {
        // 2024-01-01T00:00:00Z and the 32-bit signed rollover instant,
        // both widely published constants.
        assert_eq!(at(1_704_067_200, 0), "2024-01-01T00:00:00.000Z");
        assert_eq!(at(2_147_483_647, 999), "2038-01-19T03:14:07.999Z");
    }

    #[test]
    fn leap_days_exist_in_leap_years_only() {
        // 2024-02-29T12:00:00Z — a leap day under the every-4 rule.
        assert_eq!(at(1_709_208_000, 0), "2024-02-29T12:00:00.000Z");
        // 2000-02-29T00:00:00Z — a leap day under the every-400 exception.
        assert_eq!(at(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        // 2100 is not a leap year: the day after 2100-02-28 is March 1st.
        // 2100-03-01T00:00:00Z = 4107542400.
        assert_eq!(at(4_107_542_400, 0), "2100-03-01T00:00:00.000Z");
    }

    #[test]
    fn a_pre_epoch_clock_formats_as_the_epoch() {
        let before = UNIX_EPOCH - Duration::from_secs(5);
        assert_eq!(rfc3339_millis(before), "1970-01-01T00:00:00.000Z");
    }
}
