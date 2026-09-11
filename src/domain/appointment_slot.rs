//! A teacher's published availability: "I am free here, book me". This module
//! is the pure row shape: the validated note, the ids, and
//! [`AppointmentSlot::weekly_windows`], the concrete windows a recurring
//! publish unfolds into at write time — each occurrence keeps the same
//! `series` id, so no recurrence rule is ever evaluated at read time and
//! cancelling one week is a plain row delete.
//!
//! The one piece of booking state a slot carries is the `occupied` counter (a
//! [`cap`](crate::db::cap) of one), taken when a booking is made and given
//! back in the same transaction as the reject or cancel that settles it — so
//! the slot frees itself again, and unlike a UNIQUE index it does not keep a
//! dead booking's seat. Stored rather than counted from the
//! [appointment](crate::domain::appointment::Appointment) rows because a
//! conditional write on one row is the only guard a concurrent booking cannot
//! outrun. The reads and writes behind it live in
//! [`crate::db::appointment_slot`]; the publish and delete workflows under
//! [`crate::service::appointment::APPOINTMENT_LOCK`] live in
//! [`crate::service::appointment_slot`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    APPOINTMENT_SLOT_TABLE, MAX_APPOINTMENT_NOTE_LEN, MAX_SLOT_OCCURRENCES, MILLIS_PER_WEEK,
};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

// Slot ids come from [`crate::domain::monotonic_id`]: a recurring publish
// writes its whole expansion inside one millisecond, which random ULID low
// bits would scramble against the `id` tie-break of the `ORDER BY starts_at,
// id` listings.

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AppointmentSlotId(RecordId);

impl AppointmentSlotId {
    pub fn generate() -> Self {
        Self(RecordId::new(
            APPOINTMENT_SLOT_TABLE,
            next_ulid().to_string(),
        ))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(APPOINTMENT_SLOT_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// The id shared by every occurrence one recurring publish created. A plain
/// ULID string (not a record id): it names a group, never a row.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SlotSeries(String);

impl SlotSeries {
    pub fn generate() -> Self {
        Self(next_ulid().to_string())
    }

    pub fn from_key(key: &str) -> Self {
        Self(key.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SlotNote(String);

impl SlotNote {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("note", value, MAX_APPOINTMENT_NOTE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The half-open window `[starts_at, ends_at)` a slot offers. Half-open is the
/// whole point: back-to-back slots (10:00–10:30, 10:30–11:00) touch without
/// overlapping, which is exactly how a teacher's hour is carved up.
#[derive(Debug, Clone, SurrealValue)]
pub struct AppointmentSlot {
    pub(crate) id: AppointmentSlotId,
    pub(crate) teacher: UserId,
    pub(crate) starts_at: Timestamp,
    pub(crate) ends_at: Timestamp,
    pub(crate) note: Option<SlotNote>,
    pub(crate) series: Option<SlotSeries>,
    pub(crate) created_at: Timestamp,
}

impl AppointmentSlot {
    pub fn get_id(&self) -> &AppointmentSlotId {
        &self.id
    }

    pub fn get_teacher(&self) -> &UserId {
        &self.teacher
    }

    pub fn get_starts_at(&self) -> Timestamp {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Timestamp {
        self.ends_at
    }

    pub fn get_note(&self) -> Option<&SlotNote> {
        self.note.as_ref()
    }

    pub fn get_series(&self) -> Option<&SlotSeries> {
        self.series.as_ref()
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// A slot must cover a real span of time — an empty or inverted window
    /// could never be booked, and would break the touching-is-not-overlapping
    /// rule the conflict guard rests on.
    pub(crate) fn check_window(
        starts_at: Timestamp,
        ends_at: Timestamp,
    ) -> Result<(), ValidationError> {
        if starts_at.as_millis() >= ends_at.as_millis() {
            return Err(ValidationError::Invalid {
                field: "ends_at",
                reason: "must be after starts_at",
            });
        }
        Ok(())
    }

    /// The concrete windows a weekly publish expands into: the first one, then
    /// the same time each following week, up to and including `until`.
    /// Pure — no clock, no database — so the caller can size the write before
    /// making it. Refuses more than [`MAX_SLOT_OCCURRENCES`] occurrences.
    pub fn weekly_windows(
        starts_at: Timestamp,
        ends_at: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<(Timestamp, Timestamp)>, AppError> {
        Self::check_window(starts_at, ends_at)?;
        if until.as_millis() < starts_at.as_millis() {
            return Err(ValidationError::Invalid {
                field: "until",
                reason: "must not be before the first slot",
            }
            .into());
        }
        let count = (until.as_millis() - starts_at.as_millis()) / MILLIS_PER_WEEK + 1;
        if count > MAX_SLOT_OCCURRENCES as i64 {
            return Err(ValidationError::TooLong {
                field: "until",
                max: MAX_SLOT_OCCURRENCES,
                got: count as usize,
            }
            .into());
        }
        // Checked, not plain, arithmetic: `ends_at` is a caller-supplied i64 and
        // a window near `i64::MAX` overflows on the very first shift — which in
        // release wraps the end *below* the start, and an inverted window can
        // never overlap anything, silently disabling the double-booking guard.
        // Same spirit as `Timestamp::in_days`, but refusing instead of
        // saturating: a saturated end would still invert.
        let shifted = |base: Timestamp, week: i64| -> Option<Timestamp> {
            week.checked_mul(MILLIS_PER_WEEK)
                .and_then(|shift| base.as_millis().checked_add(shift))
                .map(Timestamp::from_millis)
        };
        (0..count)
            .map(|week| {
                let (starts_at, ends_at) = shifted(starts_at, week)
                    .zip(shifted(ends_at, week))
                    .ok_or(ValidationError::Invalid {
                        field: "ends_at",
                        reason: "is too far ahead to repeat weekly",
                    })?;
                Self::check_window(starts_at, ends_at)?;
                Ok((starts_at, ends_at))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{
        MAX_APPOINTMENT_NOTE_LEN, MAX_SLOT_OCCURRENCES, MILLIS_PER_DAY, MILLIS_PER_WEEK,
    };

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    #[tokio::test]
    async fn note_is_optional_and_capped() {
        assert!(SlotNote::try_new("").is_ok());
        assert!(SlotNote::try_new(&"x".repeat(MAX_APPOINTMENT_NOTE_LEN)).is_ok());
        assert!(SlotNote::try_new(&"x".repeat(MAX_APPOINTMENT_NOTE_LEN + 1)).is_err());
    }

    #[tokio::test]
    async fn window_must_be_a_real_span() {
        assert!(AppointmentSlot::check_window(at(10), at(20)).is_ok());
        assert!(AppointmentSlot::check_window(at(20), at(20)).is_err());
        assert!(AppointmentSlot::check_window(at(21), at(20)).is_err());
    }

    #[tokio::test]
    async fn weekly_expansion_counts_whole_weeks_inclusive() {
        let start = at(0);
        let end = at(MILLIS_PER_DAY / 24);
        // `until` on the third occurrence's own start — that week is included.
        let windows = AppointmentSlot::weekly_windows(start, end, at(2 * MILLIS_PER_WEEK)).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[2].0.as_millis(), 2 * MILLIS_PER_WEEK);
        assert_eq!(
            windows[2].1.as_millis(),
            2 * MILLIS_PER_WEEK + MILLIS_PER_DAY / 24
        );
        // One millisecond short of the next week does not add an occurrence.
        assert_eq!(
            AppointmentSlot::weekly_windows(start, end, at(3 * MILLIS_PER_WEEK - 1))
                .unwrap()
                .len(),
            3
        );
        // A single-shot week is still one occurrence.
        assert_eq!(
            AppointmentSlot::weekly_windows(start, end, start)
                .unwrap()
                .len(),
            1
        );
        assert!(AppointmentSlot::weekly_windows(at(MILLIS_PER_WEEK), end, at(0)).is_err());
    }

    #[tokio::test]
    async fn weekly_expansion_refuses_more_than_the_cap() {
        let start = at(0);
        let end = at(1_000);
        let last_ok = at((MAX_SLOT_OCCURRENCES as i64 - 1) * MILLIS_PER_WEEK);
        assert_eq!(
            AppointmentSlot::weekly_windows(start, end, last_ok)
                .unwrap()
                .len(),
            MAX_SLOT_OCCURRENCES
        );
        assert!(matches!(
            AppointmentSlot::weekly_windows(start, end, at(last_ok.as_millis() + MILLIS_PER_WEEK)),
            Err(AppError::Validation(ValidationError::TooLong { .. }))
        ));
    }

    /// A window ending near `i64::MAX` overflows on the first weekly shift.
    /// Unchecked, release builds wrapped the end *below* the start and stored
    /// inverted windows, which `Appointment::overlaps` can never flag — the
    /// double-booking guard would have gone quietly blind.
    #[tokio::test]
    async fn a_shift_that_would_overflow_is_refused() {
        let now = Timestamp::now();
        assert!(matches!(
            AppointmentSlot::weekly_windows(
                now,
                at(i64::MAX),
                at(now.as_millis() + 51 * MILLIS_PER_WEEK),
            ),
            Err(AppError::Validation(ValidationError::Invalid {
                field: "ends_at",
                ..
            }))
        ));
        // The un-shifted first occurrence alone is still fine.
        assert_eq!(
            AppointmentSlot::weekly_windows(now, at(i64::MAX), now)
                .unwrap()
                .len(),
            1
        );
    }
}
