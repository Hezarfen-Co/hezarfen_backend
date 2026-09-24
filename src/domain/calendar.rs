//! The school's day math, in one place: zone offset, the calendar day an
//! instant falls on, the instant a day starts at, the ISO weekday, and day
//! ranges.
//!
//! Everything here is fixed-offset arithmetic — the allow-list in
//! [`crate::constant::TIMEZONES`] names zones whose offset is constant
//! (Turkey has run on UTC+3 with no DST since 2016), so the backend buckets
//! days without carrying a timezone database. Consumers read the school's
//! zone off its settings
//! ([`Settings::get_timezone`](crate::domain::settings::Settings::get_timezone))
//! and pass the offset down, because "a day" is a calendar day at the school,
//! not a UTC one.

use chrono::Datelike;

/// The school's day boundary as a fixed offset in minutes.
///
/// An unrecognised name falls back to [`crate::constant::DEFAULT_TIMEZONE`]'s
/// offset, which is what an unset setting gets anyway.
pub fn zone_offset_minutes(timezone: &str) -> i32 {
    match timezone {
        "UTC" => 0,
        _ => 3 * 60,
    }
}

/// The largest shift [`zoned_day`] can ever apply: the biggest offset any
/// entry of [`crate::constant::TIMEZONES`] produces. A caller that must not
/// panic (the materialize route's raw `i64` instants) uses it for a checked
/// pre-flight that mirrors the unchecked arithmetic below.
pub const MAX_ZONE_OFFSET_MINUTES: i32 = 3 * 60;

/// The calendar day an instant falls on, in the school's zone.
pub fn zoned_day(millis: i64, offset_minutes: i32) -> chrono::NaiveDate {
    let shifted = millis + i64::from(offset_minutes) * 60_000;
    chrono::DateTime::from_timestamp_millis(shifted)
        .expect("timestamps are within chrono's representable range")
        .date_naive()
}

/// The instant `day` begins at, in the school's zone: midnight local time as
/// a UTC millisecond count. The inverse of [`zoned_day`] at day granularity —
/// a round trip through both lands on the same date.
pub fn day_start_millis(day: chrono::NaiveDate, offset_minutes: i32) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc()
        .timestamp_millis()
        - i64::from(offset_minutes) * 60_000
}

/// The ISO weekday of `day`: `1` = Monday through `7` = Sunday, the same
/// scale the weekly plan's `weekday` column carries.
pub fn iso_weekday(day: chrono::NaiveDate) -> i16 {
    day.weekday().num_days_from_monday() as i16 + 1
}

/// How many days `from..=to` spans, endpoints included. An inverted range
/// (`to` before `from`) answers a negative count; the generator works over
/// [`days_between`] instead, whose answer is simply empty there.
pub fn days_inclusive(from: chrono::NaiveDate, to: chrono::NaiveDate) -> i64 {
    (to - from).num_days() + 1
}

/// Every calendar day from `from` through `to`, endpoints included. An
/// inverted range names no day at all.
///
/// Walked one `succ_opt` at a time rather than collected from `from..=to`:
/// `RangeInclusive<NaiveDate>` is only an iterator through chrono's private
/// `Step`, so the range form does not compile here. The walk stops at the
/// last representable date instead of panicking.
pub fn days_between(from: chrono::NaiveDate, to: chrono::NaiveDate) -> Vec<chrono::NaiveDate> {
    let mut days = Vec::new();
    let mut day = from;
    while day <= to {
        days.push(day);
        match day.succ_opt() {
            Some(next) => day = next,
            None => break,
        }
    }
    days
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::timestamp::Timestamp;

    #[test]
    fn the_zone_offset_follows_the_allow_list() {
        assert_eq!(zone_offset_minutes("UTC"), 0);
        assert_eq!(zone_offset_minutes("Europe/Istanbul"), 3 * 60);
    }

    /// The day bucket is the school's day, not UTC's: 22:30 UTC in summer
    /// Istanbul is already tomorrow. This is the whole reason the timezone is
    /// read off the settings.
    #[test]
    fn the_day_bucket_follows_the_school_zone() {
        let at = Timestamp::from_millis(1_767_216_600_000); // 2025-12-31T21:30Z
        assert_eq!(
            zoned_day(at.as_millis(), zone_offset_minutes("UTC")).to_string(),
            "2025-12-31"
        );
        assert_eq!(
            zoned_day(at.as_millis(), zone_offset_minutes("Europe/Istanbul")).to_string(),
            "2026-01-01"
        );
    }

    #[test]
    fn iso_weekday_is_the_monday_one_scale() {
        // 2026-09-28 is a Monday, 2026-10-04 the Sunday that ends it.
        let monday = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        let sunday = chrono::NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        assert_eq!(iso_weekday(monday), 1);
        assert_eq!(iso_weekday(sunday), 7);
    }

    #[test]
    fn day_ranges_are_inclusive() {
        let monday = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        let next_monday = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        // A single day spans one day.
        assert_eq!(days_inclusive(monday, monday), 1);
        assert_eq!(days_between(monday, monday), vec![monday]);
        // Monday through the next Monday is eight days, endpoints named.
        assert_eq!(days_inclusive(monday, next_monday), 8);
        let days = days_between(monday, next_monday);
        assert_eq!(days.len(), 8);
        assert_eq!(days[0], monday);
        assert_eq!(days[7], next_monday);
        // An inverted range names no day.
        assert!(days_between(next_monday, monday).is_empty());
    }

    #[test]
    fn day_start_round_trips_through_zoned_day() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        for offset in [0, 3 * 60] {
            let start = day_start_millis(day, offset);
            assert_eq!(zoned_day(start, offset), day);
            // Midnight local: the day before, this instant is still evening.
            assert_eq!(zoned_day(start - 1, offset), day.pred_opt().unwrap());
        }
    }
}
