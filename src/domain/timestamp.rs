//! Time policy for the whole backend, in one place.
//!
//! - Every instant is a **unix-millisecond `i64`, always UTC** — stored as an
//!   `int`, sent over the wire as a plain number. No timezone is ever stored
//!   or parsed, so the server's `TZ`, the client's locale, and the container's
//!   clock configuration cannot change what a value means.
//! - [`Timestamp::now`] is the **only wall-clock read in the codebase**. The
//!   clippy `disallowed-methods` config (clippy.toml) rejects every other
//!   `now()` source (`chrono::Utc/Local`, `SystemTime`, `time::OffsetDateTime`)
//!   at lint time, so a stray local-time call cannot creep back in.
//! - Durations that only pace things (rate limiter) use the monotonic
//!   `Instant` clock instead, which NTP adjustments cannot move backwards.

use surrealdb::types::SurrealValue;

/// A unix-millisecond instant (UTC by construction). Stored as an `int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, SurrealValue)]
pub struct Timestamp(i64);

pub const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

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
    async fn today_utc_matches_now() {
        let today = Timestamp::today_utc();
        let from_millis = chrono::DateTime::from_timestamp_millis(Timestamp::now().as_millis())
            .unwrap()
            .date_naive();
        // Not flaky: both reads happen within the same test, and a UTC day
        // boundary crossing between them would still satisfy `<=`.
        assert!(today <= from_millis);
    }
}
