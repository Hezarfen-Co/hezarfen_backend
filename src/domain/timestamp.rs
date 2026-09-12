//! Time policy for the whole backend, in one place.
//!
//! - Every instant is a **unix-millisecond `i64`, always UTC** — stored as a
//!   `BIGINT`, sent over the wire as a plain number. No timezone is ever
//!   stored or parsed, so the server's `TZ`, the client's locale, and the
//!   container's clock configuration cannot change what a value means.
//! - [`Timestamp::now`] is the **only wall-clock read in the codebase**. The
//!   clippy `disallowed-methods` config (clippy.toml) rejects every other
//!   `now()` source (`chrono::Utc/Local`, `SystemTime`, `time::OffsetDateTime`)
//!   at lint time, so a stray local-time call cannot creep back in.
//! - Durations that only pace things (rate limiter) use the monotonic
//!   `Instant` clock instead, which NTP adjustments cannot move backwards.

use chrono::Datelike;

use crate::constant::MILLIS_PER_DAY;
use crate::error::{AppError, ValidationError};

/// The one spelling of "this range is inverted", so the pre-flight check
/// ([`crate::web::check_time_range`]) and the write-time `WHERE` guard that
/// re-makes it against the stored row answer a client identically.
pub(crate) fn range_error() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "ends_at",
        reason: "must be at or after starts_at",
    })
}

/// A unix-millisecond instant (UTC by construction). Stored as a `BIGINT`.
/// Also serializes as its bare number: the fee plan's installments ride the
/// row as JSON, and a due date must stay a plain millisecond count there too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, sqlx::Type, serde::Serialize, serde::Deserialize)]
#[sqlx(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The sole wall-clock read. Everything else derives from this.
    #[allow(clippy::disallowed_methods)] // the one permitted clock call
    pub fn now() -> Self {
        Self(chrono::Utc::now().timestamp_millis())
    }

    /// `now` shifted by whole days. UTC days are a fixed 86 400 000 ms (no
    /// DST), so plain arithmetic is exact; saturating keeps absurd inputs
    /// from panicking.
    pub fn in_days(days: i64) -> Self {
        Self(
            Self::now()
                .0
                .saturating_add(days.saturating_mul(MILLIS_PER_DAY)),
        )
    }

    pub fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    pub fn as_millis(&self) -> i64 {
        self.0
    }

    pub fn is_past(&self) -> bool {
        self.0 < Self::now().0
    }

    /// Today's calendar date in UTC, derived from [`Timestamp::now`] so the
    /// clock choke point stays single. Calendar-date validation (birth dates)
    /// must leave a one-day grace around this — a client living ahead of UTC
    /// (up to UTC+14) legitimately writes "tomorrow's" date on its wall.
    pub fn today_utc() -> chrono::NaiveDate {
        chrono::DateTime::from_timestamp_millis(Self::now().0)
            .expect("current time is within chrono's representable range")
            .date_naive()
    }

    /// *This* instant's UTC calendar day as a stable integer (days since the
    /// Common Era epoch). The day a stored value can be compared and
    /// incremented as an `int` — `day == stored + 1` is exactly "the next
    /// day", with no string parsing and no month/year arithmetic.
    ///
    /// Takes the instant instead of reading the clock, which is what makes a
    /// day-boundary rule testable without sleeping: [`Timestamp::now`] stays
    /// the single clock read, and the caller derives the day from the same
    /// stamp it already took. Midnight UTC is the boundary, like every other
    /// day calculation here ([`Timestamp::today_utc`], menu dates, the meal
    /// cutoff) — no timezone is stored anywhere, deliberately.
    pub fn day_number(&self) -> i64 {
        chrono::DateTime::from_timestamp_millis(self.0)
            // Reachable only for an instant ±262 000 years from the epoch; the
            // callers derive this from a server clock read.
            .expect("timestamp is within chrono's representable range")
            .date_naive()
            .num_days_from_ce() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn millis_roundtrip() {
        assert_eq!(
            Timestamp::from_millis(1_700_000_000_000).as_millis(),
            1_700_000_000_000
        );
    }

    #[tokio::test]
    async fn ordering() {
        assert!(Timestamp::from_millis(1) < Timestamp::from_millis(2));
    }

    #[tokio::test]
    async fn expiry_direction() {
        assert!(!Timestamp::in_days(1).is_past());
        assert!(Timestamp::in_days(-1).is_past());
    }

    #[tokio::test]
    async fn in_days_is_exact_utc_days() {
        let before = Timestamp::now().as_millis();
        let shifted = Timestamp::in_days(2).as_millis();
        let after = Timestamp::now().as_millis();
        assert!(shifted - before >= 2 * MILLIS_PER_DAY);
        assert!(shifted - after <= 2 * MILLIS_PER_DAY);
    }

    #[tokio::test]
    async fn in_days_saturates_instead_of_panicking() {
        assert_eq!(Timestamp::in_days(i64::MAX).as_millis(), i64::MAX);
    }

    #[tokio::test]
    async fn day_number_is_a_midnight_utc_day_counter() {
        // The unix epoch, pinned: a drifting origin would silently shift every
        // stored streak day.
        let epoch = Timestamp::from_millis(0);
        assert_eq!(epoch.day_number(), 719_163);
        // Same UTC day, different hours — one day number.
        assert_eq!(Timestamp::from_millis(86_399_999).day_number(), 719_163);
        // The next millisecond is the next day, and a whole day is +1.
        assert_eq!(Timestamp::from_millis(86_400_000).day_number(), 719_164);
        assert_eq!(
            Timestamp::from_millis(1_700_000_000_000 + MILLIS_PER_DAY).day_number(),
            Timestamp::from_millis(1_700_000_000_000).day_number() + 1
        );
        // Before the epoch still counts forward.
        assert_eq!(Timestamp::from_millis(-1).day_number(), 719_162);
        // And it agrees with the crate's other day calculation. Not flaky: a
        // day boundary between the two clock reads still leaves `<=`.
        assert!(Timestamp::now().day_number() <= Timestamp::today_utc().num_days_from_ce() as i64);
    }

    #[tokio::test]
    async fn today_utc_matches_now() {
        let today = Timestamp::today_utc();
        let from_millis = chrono::DateTime::from_timestamp_millis(Timestamp::now().as_millis())
            .unwrap()
            .date_naive();
        // Not flaky: both reads happen within the same test, and a UTC day
        // boundary crossing between them would still satisfy `<=`.
        assert!(today <= from_millis);
    }

    /// JSON is part of the storage contract now (embedded installments ride a
    /// JSONB column): the instant must stay a bare millisecond number, never
    /// a string or an object.
    #[test]
    fn json_form_is_the_bare_millisecond_number() {
        assert_eq!(
            serde_json::to_value(Timestamp::from_millis(1_700_000_000_000)).unwrap(),
            serde_json::json!(1_700_000_000_000)
        );
        let back: Timestamp = serde_json::from_value(serde_json::json!(1_700_000_000_000)).unwrap();
        assert_eq!(back.as_millis(), 1_700_000_000_000);
    }
}
