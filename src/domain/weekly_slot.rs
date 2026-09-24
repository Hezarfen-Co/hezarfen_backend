//! The weekly plan slot: one line of a course's timetable template — a
//! weekday and the time-of-day window one lesson hour lands in.
//!
//! Slots live on one of two owners: the grade-level offering
//! (`offering_slot`, the template) or the class×course instance
//! (`class_course_slot`, the per-class override). Which set a reader sees is
//! `class_course.weekly_plan_inherited`'s decision, applied by
//! [`crate::service::weekly_slot::resolved_for_instance`]: `TRUE` follows the
//! offering's rows, `FALSE` makes the section's own rows authoritative
//! *including when they are empty* — that is how a section clears its
//! timetable. Override-or-inherit, never merge.
//!
//! Two of one owner's slots may touch (one may start the minute another
//! ends) but never overlap on the same weekday; the exact duplicate is the
//! store's `UNIQUE (owner, weekday, starts_at)` key. The ordering a resolved
//! list comes back in is `weekday` first, then `starts_at` — Monday through
//! Sunday, each day dawn to dusk.
//!
//! **No scheduler.** Nothing generates dated `course_session` rows from a
//! weekly plan — `course_session` stays the per-lesson occurrence, written by
//! its own routes. This module is template + override + resolution only.
//!
//! The slot bounds live in [`crate::constant`] beside every other published
//! bound, so `GET /limits` and the completeness guard see them.

use crate::constant::{MAX_SLOT_MINUTE, MAX_WEEKLY_SLOTS, MIN_SLOT_MINUTE};
use crate::domain::monotonic_id::next_uuid;
use crate::error::ValidationError;

/// The identity of one slot row. A UUIDv7 minted by the process-wide
/// monotonic generator, like every other row id in the repo.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct WeeklySlotId(uuid::Uuid);

impl WeeklySlotId {
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds that cannot take the
    /// newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> uuid::Uuid {
        self.0
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    /// The bare uuid wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// An ISO weekday: `1` = Monday through `7` = Sunday. The same spell the
/// DDL's `CHECK (weekday BETWEEN 1 AND 7)` enforces at the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct Weekday(i16);

impl Weekday {
    /// The weekday `raw` names, or a validation error when it is off the
    /// ISO scale — the `400` the add routes answer with.
    pub fn new(raw: i16) -> Result<Self, ValidationError> {
        if !(1..=7).contains(&raw) {
            return Err(ValidationError::Invalid {
                field: "weekday",
                reason: "must be 1 (Monday) through 7 (Sunday)",
            });
        }
        Ok(Self(raw))
    }

    /// The integer the wire and the `weekday` column carry.
    pub fn get(self) -> i16 {
        self.0
    }
}

/// A time of day as minutes past midnight — the wire form of the slot's
/// `TIME` columns (`540` = 09:00). The `TIME` cast is the statement's own;
/// this newtype range-checks what the statement binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, sqlx::Type)]
#[sqlx(transparent)]
pub struct SlotMinute(i64);

impl SlotMinute {
    /// The minute `raw` names, or a validation error when it is not a minute
    /// of a real day — the `400` the add routes answer with.
    pub fn new(raw: i64) -> Result<Self, ValidationError> {
        if !(MIN_SLOT_MINUTE..=MAX_SLOT_MINUTE).contains(&raw) {
            return Err(ValidationError::Invalid {
                field: "slot time",
                reason: "must be minutes past midnight, 0 (00:00) through 1439 (23:59)",
            });
        }
        Ok(Self(raw))
    }

    /// The integer the wire carries.
    pub fn get(self) -> i64 {
        self.0
    }
}

/// One weekly plan line: on `weekday`, from `starts_at` to `ends_at`.
///
/// The list a resolver returns is ordered `weekday` first, then `starts_at`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct WeeklySlot {
    pub(crate) id: WeeklySlotId,
    pub(crate) weekday: Weekday,
    pub(crate) starts_at: SlotMinute,
    pub(crate) ends_at: SlotMinute,
}

impl WeeklySlot {
    /// The slot `id` names, or a validation error when the window is not one
    /// (`ends_at` must land strictly after `starts_at` — equal times are an
    /// empty window, inverted ones a typo). The store's
    /// `CHECK (ends_at > starts_at)` is the same rule at rest.
    pub fn new(
        id: WeeklySlotId,
        weekday: Weekday,
        starts_at: SlotMinute,
        ends_at: SlotMinute,
    ) -> Result<Self, ValidationError> {
        if starts_at.get() >= ends_at.get() {
            return Err(ValidationError::Invalid {
                field: "ends_at",
                reason: "must be after starts_at — a slot needs a non-empty window",
            });
        }
        Ok(Self {
            id,
            weekday,
            starts_at,
            ends_at,
        })
    }

    pub fn get_id(&self) -> &WeeklySlotId {
        &self.id
    }

    pub fn get_weekday(&self) -> Weekday {
        self.weekday
    }

    pub fn get_starts_at(&self) -> SlotMinute {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> SlotMinute {
        self.ends_at
    }

    /// Whether this and `other` cannot both stand in one owner's plan: the
    /// same weekday with intersecting windows. Touching endpoints are fine —
    /// a lesson may start the minute another ends.
    pub fn overlaps(&self, other: &WeeklySlot) -> bool {
        self.weekday == other.weekday
            && self.starts_at.get() < other.ends_at.get()
            && other.starts_at.get() < self.ends_at.get()
    }
}

/// Why [`check_add`] refused a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotAddError {
    /// A slot of the same owner already overlaps the candidate on its
    /// weekday. The exact duplicate overlaps too, so it answers here.
    Overlap,
    /// The owner's plan already holds [`MAX_WEEKLY_SLOTS`] slots.
    Full,
}

/// The add-time invariant, judged against the owner's current rows: overlap
/// first (it names the concrete collision the caller must move), then the
/// plan-size cap. A pure rule so the refusal order is one the tests can pin.
pub fn check_add(existing: &[WeeklySlot], candidate: &WeeklySlot) -> Result<(), SlotAddError> {
    if existing.iter().any(|slot| slot.overlaps(candidate)) {
        return Err(SlotAddError::Overlap);
    }
    if existing.len() >= MAX_WEEKLY_SLOTS {
        return Err(SlotAddError::Full);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(raw: i16) -> Weekday {
        Weekday::new(raw).unwrap()
    }

    fn mins(raw: i64) -> SlotMinute {
        SlotMinute::new(raw).unwrap()
    }

    fn slot(weekday: i16, starts: i64, ends: i64) -> WeeklySlot {
        WeeklySlot::new(
            WeeklySlotId::generate(),
            day(weekday),
            mins(starts),
            mins(ends),
        )
        .unwrap()
    }

    #[test]
    fn weekdays_are_the_iso_scale() {
        assert!(Weekday::new(1).is_ok());
        assert!(Weekday::new(7).is_ok());
        assert!(Weekday::new(0).is_err());
        assert!(Weekday::new(8).is_err());
    }

    #[test]
    fn slot_minutes_are_one_real_day() {
        assert!(SlotMinute::new(0).is_ok());
        assert!(SlotMinute::new(MAX_SLOT_MINUTE).is_ok());
        assert!(SlotMinute::new(-1).is_err());
        assert!(SlotMinute::new(MAX_SLOT_MINUTE + 1).is_err());
    }

    #[test]
    fn a_slot_window_must_be_non_empty_and_forward() {
        // Zero-length and inverted windows are refused.
        assert!(WeeklySlot::new(WeeklySlotId::generate(), day(1), mins(600), mins(600)).is_err());
        assert!(WeeklySlot::new(WeeklySlotId::generate(), day(1), mins(600), mins(540)).is_err());
        // A one-minute window stands.
        assert!(WeeklySlot::new(WeeklySlotId::generate(), day(1), mins(600), mins(601)).is_ok());
    }

    #[test]
    fn overlap_is_same_weekday_and_intersecting() {
        let monday_morning = slot(1, 540, 600);
        // Same window: an exact duplicate overlaps.
        assert!(monday_morning.overlaps(&slot(1, 540, 600)));
        // Partial and containing windows overlap.
        assert!(monday_morning.overlaps(&slot(1, 570, 630)));
        assert!(monday_morning.overlaps(&slot(1, 530, 610)));
        assert!(monday_morning.overlaps(&slot(1, 530, 630)));
        // Touching endpoints do not: one lesson may start when another ends.
        assert!(!monday_morning.overlaps(&slot(1, 600, 660)));
        assert!(!monday_morning.overlaps(&slot(1, 480, 540)));
        // The same window on another weekday is a different line of the week.
        assert!(!monday_morning.overlaps(&slot(2, 540, 600)));
    }

    #[test]
    fn check_add_refuses_overlap_before_the_cap() {
        let existing: Vec<WeeklySlot> = (0..MAX_WEEKLY_SLOTS)
            .map(|i| {
                slot(
                    ((i % 7) + 1) as i16,
                    480 + (i as i64 / 7),
                    481 + (i as i64 / 7),
                )
            })
            .collect();
        let colliding = slot(1, 480, 482);
        // Every plan below the cap still refuses the overlap first.
        let short: Vec<WeeklySlot> = existing[..MAX_WEEKLY_SLOTS - 1].to_vec();
        assert_eq!(check_add(&short, &colliding), Err(SlotAddError::Overlap));
        // A full plan refuses an overlap as an overlap, not as a full plan.
        assert_eq!(check_add(&existing, &colliding), Err(SlotAddError::Overlap));
    }

    #[test]
    fn check_add_refuses_a_full_plan_with_room_for_nothing_more() {
        let existing: Vec<WeeklySlot> = (0..MAX_WEEKLY_SLOTS)
            .map(|i| {
                slot(
                    ((i % 7) + 1) as i16,
                    480 + (i as i64 / 7),
                    481 + (i as i64 / 7),
                )
            })
            .collect();
        // Touching, non-colliding, on a weekday with room: still refused —
        // the cap, not the overlap, is the reason. (Sunday holds five of the
        // forty, the last ending at 485; this one starts exactly there.)
        let free_slot = slot(7, 485, 486);
        assert_eq!(check_add(&existing, &free_slot), Err(SlotAddError::Full));
        // Below the cap the same candidate is accepted.
        let short: Vec<WeeklySlot> = existing[..MAX_WEEKLY_SLOTS - 1].to_vec();
        assert_eq!(check_add(&short, &free_slot), Ok(()));
    }
}
