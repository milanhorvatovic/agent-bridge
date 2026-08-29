//! A dependency-free RFC 3339 UTC clock reading.
//!
//! The runtime's lockfile records when an instance started, and that
//! `started_at` needs a `date-time` string. A wall-clock reading is exactly
//! right for it — it correlates with the world outside the process and never
//! orders anything (that is `seq`'s job) — so the civil-calendar conversion
//! here turns the Unix epoch into the string the schema names without pulling
//! in a date library.

/// The current instant as an RFC 3339 UTC timestamp with millisecond
/// resolution, e.g. `2026-05-16T08:00:00.123Z`.
#[must_use]
pub fn rfc3339_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(now.as_secs(), now.subsec_millis())
}

/// The last instant RFC 3339 can spell: its `date-fullyear` is exactly four
/// digits, so an epoch beyond `9999-12-31T23:59:59Z` has no valid rendering.
const MAX_RFC3339_SECONDS: u64 = 253_402_300_799;

/// Format seconds-since-epoch and a millisecond remainder as RFC 3339 UTC.
/// Split out so the calendar arithmetic is testable against known instants.
fn format_rfc3339(epoch_secs: u64, millis: u32) -> String {
    // A clock set past the year 9999 would otherwise render a five-or-more
    // digit year — not an RFC 3339 date-time, which this function promises.
    // Clamp to the last representable instant rather than emit an invalid one.
    if epoch_secs > MAX_RFC3339_SECONDS {
        return "9999-12-31T23:59:59.999Z".to_owned();
    }
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        secs_of_day % 3600 / 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert a count of days since the Unix epoch into a civil `(year, month,
/// day)`, by Howard Hinnant's `civil_from_days` — the standard branch-free
/// conversion, valid across the whole representable range and correct on leap
/// years without a table.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_formatter_matches_known_instants() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        // 2026-05-16T08:00:00.123Z — the design's own example instant.
        assert_eq!(
            format_rfc3339(1_778_918_400, 123),
            "2026-05-16T08:00:00.123Z"
        );
    }

    #[test]
    fn an_epoch_past_the_year_9999_clamps_to_the_last_valid_instant() {
        // Beyond the four-digit-year boundary the calendar arithmetic would
        // spell a five-digit year; the formatter clamps instead of emitting a
        // string that is no longer RFC 3339. The last valid second is fine.
        assert_eq!(
            format_rfc3339(MAX_RFC3339_SECONDS, 0),
            "9999-12-31T23:59:59.000Z"
        );
        assert_eq!(
            format_rfc3339(MAX_RFC3339_SECONDS + 1, 500),
            "9999-12-31T23:59:59.999Z"
        );
        assert_eq!(format_rfc3339(u64::MAX, 0), "9999-12-31T23:59:59.999Z");
    }

    #[test]
    fn now_is_well_formed_and_after_the_epoch() {
        let now = rfc3339_now();
        assert!(now.ends_with('Z') && now.len() == "1970-01-01T00:00:00.000Z".len());
        assert!(now.as_str() > "2020-01-01T00:00:00.000Z");
    }
}
