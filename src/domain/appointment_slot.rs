//! A teacher's published availability: "I am free here, book me". A slot is
//! nearly pure calendar: the one piece of booking state it carries is the
//! `occupied` counter (a [`cap`](crate::db::cap) of one), taken when a
//! booking is made and given back in the same transaction as the reject or
//! cancel that settles it — so the slot frees itself again, and unlike a UNIQUE
//! index it does not keep a dead booking's seat. Stored rather than counted
//! from the [`Appointment`] rows because a conditional write on one row is the
//! only guard a concurrent booking cannot outrun.
//!
//! A recurring publish is expanded into concrete rows here, at write time,
//! sharing one `series` id — no recurrence rule is ever evaluated at read
//! time. Cancelling one week is then a plain row delete, and the whole series
//! is still addressable through its id.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    APPOINTMENT_SLOT_TABLE, MAX_APPOINTMENT_NOTE_LEN, MAX_SLOT_OCCURRENCES, MILLIS_PER_WEEK, ROLES,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::appointment::{APPOINTMENT_LOCK, Appointment};
use crate::db::cap;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

/// The publisher fell below `teacher` while the publish was in flight.
const UNFIT_MARK: &str = "slot_not_staff";

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
    id: AppointmentSlotId,
    teacher: UserId,
    starts_at: Timestamp,
    ends_at: Timestamp,
    note: Option<SlotNote>,
    series: Option<SlotSeries>,
    created_at: Timestamp,
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
    fn check_window(starts_at: Timestamp, ends_at: Timestamp) -> Result<(), ValidationError> {
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

    /// Write a whole publish — one occurrence or fifty-two — while the publisher
    /// still *holds* a teaching role.
    ///
    /// Every row here names the teacher, so the write touches no key a demotion
    /// touches: [`crate::domain::user::User::set_role`] sweeps the slots its own
    /// snapshot can see, and a slot landing after that snapshot would survive it
    /// under a role that may not publish — reachable by no route afterwards,
    /// since the calendar hides a demoted teacher's slots and the deletes are
    /// `teacher`-gated. So the teacher's own record is claimed beside the insert
    /// ([`cap::role_claim`]), which is the key the demotion writes: either the
    /// sweep sees these slots, or this write sees the new role and publishes
    /// nothing.
    ///
    /// The bar is read off the hierarchy rather than spelled out, so a new role
    /// cannot drift out of it.
    ///
    /// Admissible for [`transaction_with_retry`]: the claim is `SELECT`/`UPDATE`
    /// only, and the `INSERT` carries freshly minted ULIDs on a table with no
    /// `UNIQUE` index, so no rival can make it answer "already exists".
    async fn insert_claimed(
        teacher: &UserId,
        rows: Vec<AppointmentSlot>,
        db: &Database,
    ) -> Result<Vec<AppointmentSlot>, AppError> {
        let staff = ROLES
            .iter()
            .filter(|role| role.at_least(Role::Teacher))
            .map(|role| format!("'{}'", role.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let held = cap::role_claim("teacher", &format!("NOT IN [{staff}]"), UNFIT_MARK);
        let sql = format!(
            "BEGIN TRANSACTION;\n{};\nINSERT INTO {APPOINTMENT_SLOT_TABLE} $rows;\n\
             COMMIT TRANSACTION;",
            held.join(";\n")
        );
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &sql,
            &[
                ("teacher".into(), teacher.record().into_value()),
                ("rows".into(), rows.into_value()),
            ],
            &[UNFIT_MARK],
        )
        .await?;
        // Demoted while this ran — the same refusal `RequireTeacher` makes a
        // moment earlier, and the only one that can arrive after it.
        if errors
            .values()
            .any(|error| error.to_string().contains(UNFIT_MARK))
        {
            return Err(AppError::Forbidden(
                "that account no longer holds a teaching role",
            ));
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // BEGIN plus the claim's own statements — counted, not tallied by hand,
        // so a statement added there cannot read back the wrong result.
        Ok(result.take::<Vec<AppointmentSlot>>(held.len() + 1)?)
    }

    /// Does the teacher already have a published slot whose window collides with
    /// `[starts_at, ends_at)`? Half-open, so a slot ending exactly where the new
    /// one starts is *not* a conflict — that is how a teacher's hour is carved
    /// into back-to-back slots. Caller must hold [`APPOINTMENT_LOCK`] for the
    /// answer to still be true by the time the insert lands.
    async fn conflicts_existing(
        teacher: &UserId,
        starts_at: Timestamp,
        ends_at: Timestamp,
        db: &Database,
    ) -> Result<bool, AppError> {
        let mut result = db
            .query(
                "SELECT VALUE id FROM appointment_slot \
                 WHERE teacher = $teacher AND starts_at < $ends AND ends_at > $starts \
                 LIMIT 1",
            )
            .bind(("teacher", teacher.record()))
            .bind(("starts", starts_at.as_millis()))
            .bind(("ends", ends_at.as_millis()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    /// Every window this teacher already published that intersects the envelope
    /// `[from, to)`. One read for a whole batch: a stored row that overlaps *any*
    /// window in the batch necessarily overlaps the envelope spanning them all,
    /// so filtering the envelope and comparing in memory is exactly the
    /// per-window [`conflicts_existing`](Self::conflicts_existing) check,
    /// hoisted out of the loop — which is what keeps [`APPOINTMENT_LOCK`] down
    /// to two round trips instead of one per occurrence.
    async fn windows_in_span(
        teacher: &UserId,
        from: Timestamp,
        to: Timestamp,
        db: &Database,
    ) -> Result<Vec<(Timestamp, Timestamp)>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM appointment_slot \
                 WHERE teacher = $teacher AND starts_at < $to AND ends_at > $from",
            )
            .bind(("teacher", teacher.record()))
            .bind(("from", from.as_millis()))
            .bind(("to", to.as_millis()))
            .await?
            .check()?;
        Ok(result
            .take::<Vec<AppointmentSlot>>(0)?
            .into_iter()
            .map(|slot| (slot.starts_at, slot.ends_at))
            .collect())
    }

    /// Publish one slot.
    pub async fn create(
        teacher: &UserId,
        starts_at: Timestamp,
        ends_at: Timestamp,
        note: Option<SlotNote>,
        db: &Database,
    ) -> Result<AppointmentSlot, AppError> {
        Self::check_window(starts_at, ends_at)?;
        // Check-then-insert under the lock so a concurrent publish or booking
        // can't slip a colliding window in between.
        let _guard = APPOINTMENT_LOCK.lock().await;
        if Self::conflicts_existing(teacher, starts_at, ends_at, db).await? {
            return Err(AppError::ConflictOwned(
                "this time overlaps a slot you have already published".into(),
            ));
        }
        Self::insert_claimed(
            teacher,
            vec![AppointmentSlot {
                id: AppointmentSlotId::generate(),
                teacher: teacher.clone(),
                starts_at,
                ends_at,
                note,
                series: None,
                created_at: Timestamp::now(),
            }],
            db,
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to create appointment slot".into()))
    }

    /// Publish the same weekly window repeatedly, up to `until`. Every row
    /// carries the same [`SlotSeries`], so the whole publish stays addressable
    /// (and deletable) as one thing while each occurrence remains an ordinary,
    /// independently bookable and independently cancellable slot.
    pub async fn publish_weekly(
        teacher: &UserId,
        starts_at: Timestamp,
        ends_at: Timestamp,
        note: Option<SlotNote>,
        until: Timestamp,
        db: &Database,
    ) -> Result<Vec<AppointmentSlot>, AppError> {
        let windows = Self::weekly_windows(starts_at, ends_at, until)?;
        // Validate the whole batch before writing a single row: all-or-nothing,
        // so a mid-series collision never leaves stray weeks behind. Held under
        // the lock from the check through the write, matching `create`.
        let _guard = APPOINTMENT_LOCK.lock().await;
        // (a) against the slots already in the database. `weekly_windows` walks
        // forward, so the first and last occurrence bound every one of them.
        let (first, last) = (windows[0], windows[windows.len() - 1]);
        let published = Self::windows_in_span(teacher, first.0, last.1, db).await?;
        for (i, &(w_start, w_end)) in windows.iter().enumerate() {
            if published
                .iter()
                .any(|&(p_start, p_end)| Appointment::overlaps(w_start, w_end, p_start, p_end))
            {
                return Err(AppError::ConflictOwned(
                    "a repeated slot overlaps one you have already published".into(),
                ));
            }
            // (b) against every earlier occurrence in this same batch — catches
            // a duration longer than the weekly step overlapping itself.
            if windows[..i]
                .iter()
                .any(|&(o_start, o_end)| Appointment::overlaps(w_start, w_end, o_start, o_end))
            {
                return Err(AppError::ConflictOwned(
                    "the repeated slots overlap each other".into(),
                ));
            }
        }
        let series = SlotSeries::generate();
        let now = Timestamp::now();
        let rows: Vec<AppointmentSlot> = windows
            .iter()
            .map(|&(starts_at, ends_at)| AppointmentSlot {
                id: AppointmentSlotId::generate(),
                teacher: teacher.clone(),
                starts_at,
                ends_at,
                note: note.clone(),
                series: Some(series.clone()),
                created_at: now,
            })
            .collect();
        // One `INSERT`, therefore all-or-nothing: SurrealDB rolls the whole
        // publish back on any error. The row-by-row loop this replaces did not
        // — a database error at week 7 of 10 answered 500 with six stray weeks
        // already published, a half-series nobody asked for. It is also a single
        // round trip, so the lock is held for two queries whatever the
        // occurrence count, instead of 1 + N (up to 52) sequential ones.
        let mut created = Self::insert_claimed(teacher, rows, db).await?;
        if created.len() != windows.len() {
            return Err(AppError::Internal(format!(
                "published {} of {} appointment slots",
                created.len(),
                windows.len()
            )));
        }
        // `INSERT` makes no promise about the order it echoes rows back in, and
        // the response is rendered as the published calendar.
        created.sort_by_key(|slot| slot.starts_at.as_millis());
        Ok(created)
    }

    pub async fn read(
        id: &AppointmentSlotId,
        db: &Database,
    ) -> Result<Option<AppointmentSlot>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// A teacher's own calendar, earliest first.
    pub async fn list_for_teacher(
        teacher: &UserId,
        db: &Database,
    ) -> Result<Vec<AppointmentSlot>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM appointment_slot WHERE teacher = $teacher \
                 ORDER BY starts_at ASC, id ASC",
            )
            .bind(("teacher", teacher.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<AppointmentSlot>>(0)?)
    }

    /// Every slot whose window has not opened yet, earliest first — the bookable
    /// calendar a requester browses.
    ///
    /// The bound is `starts_at`, not `ends_at`, so that this window is exactly
    /// the one [`Appointment::book`] accepts: a slot already underway is
    /// unbookable (`cancel` could never undo the booking), and offering it would
    /// be a calendar entry that can only answer `409`. The publish-time 60s skew
    /// grace deliberately does not apply here either — `book` does not grant it,
    /// and a slot published a moment after its own start is unbookable from
    /// birth, so it belongs on nobody's calendar.
    pub async fn list_upcoming(
        from: Timestamp,
        db: &Database,
    ) -> Result<Vec<AppointmentSlot>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM appointment_slot WHERE starts_at > $from \
                 ORDER BY starts_at ASC, id ASC",
            )
            .bind(("from", from.as_millis()))
            .await?
            .check()?;
        Ok(result.take::<Vec<AppointmentSlot>>(0)?)
    }

    pub async fn list_for_series(
        series: &SlotSeries,
        db: &Database,
    ) -> Result<Vec<AppointmentSlot>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM appointment_slot WHERE series = $series \
                 ORDER BY starts_at ASC, id ASC",
            )
            .bind(("series", series.as_str().to_string()))
            .await?
            .check()?;
        Ok(result.take::<Vec<AppointmentSlot>>(0)?)
    }

    /// Delete one slot, refusing (409) while a live booking sits on it — the
    /// requester is expecting that meeting, so it must be rejected first.
    /// Settled bookings (rejected/cancelled) are history of a slot that is
    /// going away, so they cascade out with it, like [`Event::delete`]'s rows.
    ///
    /// The guard is the delete's own `WHERE`, so no booking can land between a
    /// check and the row going away.
    pub async fn delete(self, db: &Database) -> Result<AppointmentSlot, AppError> {
        Self::delete_free(std::slice::from_ref(&self.id), db).await?;
        Ok(self)
    }

    /// Delete a whole recurring publish, all-or-nothing: if *any* occurrence
    /// still holds a live booking the entire series is refused, so the teacher
    /// deals with the person waiting instead of silently keeping a stray week.
    pub async fn delete_series(
        series: &SlotSeries,
        db: &Database,
    ) -> Result<Vec<AppointmentSlot>, AppError> {
        let slots = Self::list_for_series(series, db).await?;
        if slots.is_empty() {
            return Err(AppError::NotFound);
        }
        let ids: Vec<AppointmentSlotId> = slots.iter().map(|slot| slot.id.clone()).collect();
        Self::delete_free(&ids, db).await?;
        Ok(slots)
    }

    /// Shared body of both deletes: drop every named slot, but only while its
    /// `occupied` counter says nobody is waiting on it — the delete's own
    /// `WHERE` is the guard, which is what makes it hold against a booking
    /// landing in the middle of it (a separate read-then-delete could not).
    ///
    /// All-or-nothing across the whole list: if fewer rows go than were named,
    /// the transaction is thrown away, so a series never loses its free weeks
    /// and keeps the booked one. Whether that shortfall was a live booking or a
    /// slot that no longer exists is read off what survived — the occupied rows
    /// are still there, a vanished one is not — which is the 409/404 the caller
    /// used to get from a separate check.
    async fn delete_free(ids: &[AppointmentSlotId], db: &Database) -> Result<(), AppError> {
        let records: Vec<RecordId> = ids.iter().map(|id| id.record()).collect();
        let (_, mut errors) = transaction_with_retry(
            db,
            "BEGIN TRANSACTION;
                 LET $gone = (DELETE $slots WHERE (occupied ?? 0) = 0 RETURN BEFORE);
                 IF array::len($gone) != array::len($slots) {
                     THROW IF array::len((SELECT VALUE id FROM appointment_slot
                         WHERE id IN $slots)) > 0 { 'slot_occupied' } ELSE { 'slot_missing' }
                 };
                 DELETE appointment WHERE slot IN $slots;
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            &[("slots".into(), records.into_value())],
            &["slot_occupied", "slot_missing"],
        )
        .await?;
        // An aborted transaction errors every slot; only the THROW's own slot
        // names the marker (the `Exam::update` treatment), and a round lost to
        // a booking landing on `occupied` mid-flight is re-sent rather than
        // reported (see [`transaction_with_retry`]).
        let thrown = |marker: &str| {
            errors
                .values()
                .any(|error| error.to_string().contains(marker))
        };
        if thrown("slot_occupied") {
            return Err(AppError::Conflict(
                "the slot has a pending or approved booking",
            ));
        }
        if thrown("slot_missing") {
            return Err(AppError::NotFound);
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{MILLIS_PER_DAY, SLOT_OCCUPIED_FIELD};
    use crate::db::cap;

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

    /// Every occurrence of one publish must carry the same series id, and ids
    /// minted back-to-back must stay in write order (the `ORDER BY` tie-break).
    #[tokio::test]
    async fn a_publish_shares_one_series_and_orders_its_ids() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slots = AppointmentSlot::publish_weekly(
            &teacher,
            at(1_000),
            at(2_000),
            Some(SlotNote::try_new("veli toplantısı").unwrap()),
            at(1_000 + 4 * MILLIS_PER_WEEK),
            &db,
        )
        .await
        .unwrap();

        assert_eq!(slots.len(), 5);
        let series = slots[0].get_series().cloned().unwrap();
        assert!(slots.iter().all(|slot| slot.get_series() == Some(&series)));
        assert!(
            slots
                .windows(2)
                .all(|pair| pair[0].get_id().key() < pair[1].get_id().key())
        );

        let listed = AppointmentSlot::list_for_series(&series, &db)
            .await
            .unwrap();
        assert_eq!(listed.len(), 5);
    }

    #[tokio::test]
    async fn overlapping_publish_is_refused_but_touching_is_allowed() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        AppointmentSlot::create(&teacher, at(1_000), at(2_000), None, &db)
            .await
            .unwrap();

        // Overlaps the existing [1000,2000) → 409.
        assert!(matches!(
            AppointmentSlot::create(&teacher, at(1_500), at(2_500), None, &db).await,
            Err(AppError::ConflictOwned(_))
        ));
        // Touching at the boundary (ends where the next starts) → allowed.
        assert!(
            AppointmentSlot::create(&teacher, at(2_000), at(3_000), None, &db)
                .await
                .is_ok()
        );
        assert!(
            AppointmentSlot::create(&teacher, at(0), at(1_000), None, &db)
                .await
                .is_ok()
        );
        // A different teacher sharing the same window is fine.
        let other = UserId::from_key("t2");
        assert!(
            AppointmentSlot::create(&other, at(1_000), at(2_000), None, &db)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn weekly_publish_overlapping_an_existing_slot_writes_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        // A lone slot on the third week of the coming series.
        AppointmentSlot::create(
            &teacher,
            at(1_000 + 2 * MILLIS_PER_WEEK),
            at(2_000 + 2 * MILLIS_PER_WEEK),
            None,
            &db,
        )
        .await
        .unwrap();

        let before = AppointmentSlot::list_for_teacher(&teacher, &db)
            .await
            .unwrap()
            .len();
        assert!(matches!(
            AppointmentSlot::publish_weekly(
                &teacher,
                at(1_000),
                at(2_000),
                None,
                at(1_000 + 4 * MILLIS_PER_WEEK),
                &db,
            )
            .await,
            Err(AppError::ConflictOwned(_))
        ));
        // All-or-nothing: not one occurrence of the rejected series was written.
        assert_eq!(
            AppointmentSlot::list_for_teacher(&teacher, &db)
                .await
                .unwrap()
                .len(),
            before
        );
    }

    /// The existing-slot check is one envelope-wide read instead of a query per
    /// occurrence, so the edges of that envelope are what could go wrong: a
    /// clash on the *last* week must still be caught, and a slot merely touching
    /// the batch's boundaries must still be allowed.
    #[tokio::test]
    async fn the_batch_wide_conflict_read_still_sees_the_last_week_and_lets_touching_through() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let last = 1_000 + 4 * MILLIS_PER_WEEK;
        // Touching both ends of the envelope: ends where the first occurrence
        // starts, and starts where the last one ends.
        AppointmentSlot::create(&teacher, at(0), at(1_000), None, &db)
            .await
            .unwrap();
        AppointmentSlot::create(&teacher, at(last + 1_000), at(last + 2_000), None, &db)
            .await
            .unwrap();
        assert_eq!(
            AppointmentSlot::publish_weekly(&teacher, at(1_000), at(2_000), None, at(last), &db)
                .await
                .unwrap()
                .len(),
            5
        );

        // One millisecond into the last occurrence → the whole publish is 409.
        let db = crate::database::init_mem().await.unwrap();
        AppointmentSlot::create(&teacher, at(last + 999), at(last + 3_000), None, &db)
            .await
            .unwrap();
        assert!(matches!(
            AppointmentSlot::publish_weekly(&teacher, at(1_000), at(2_000), None, at(last), &db)
                .await,
            Err(AppError::ConflictOwned(_))
        ));
        assert_eq!(
            AppointmentSlot::list_for_teacher(&teacher, &db)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The delete-vs-book race: a booking that lands between this delete's read
    /// and its write must still keep the slot alive, so the guard has to be the
    /// delete's own `WHERE` — here driven by taking the seat the way that
    /// booking would, with no booking row to read.
    ///
    /// A series is all-or-nothing: one taken week refuses the whole publish,
    /// and the free weeks must survive the refusal.
    #[tokio::test]
    async fn a_taken_slot_is_never_deleted_and_takes_its_series_with_it() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slots = AppointmentSlot::publish_weekly(
            &teacher,
            at(1_000),
            at(2_000),
            None,
            at(1_000 + 2 * MILLIS_PER_WEEK),
            &db,
        )
        .await
        .unwrap();
        let series = slots[0].get_series().cloned().unwrap();

        // A free slot goes, and its series is deletable while every week is free.
        assert!(slots[0].clone().delete(&db).await.is_ok());
        // The seat on the middle week is taken — no booking row, exactly as a
        // racing booking would leave it mid-flight.
        assert!(
            cap::claim(&slots[1].get_id().record(), SLOT_OCCUPIED_FIELD, 1, &db)
                .await
                .unwrap()
        );
        assert!(matches!(
            slots[1].clone().delete(&db).await,
            Err(AppError::Conflict(
                "the slot has a pending or approved booking"
            ))
        ));
        assert!(matches!(
            AppointmentSlot::delete_series(&series, &db).await,
            Err(AppError::Conflict(_))
        ));
        // Refused, not half-applied: the free third week is still published.
        assert_eq!(
            AppointmentSlot::list_for_series(&series, &db)
                .await
                .unwrap()
                .len(),
            2
        );

        // A slot that is simply gone is a 404, not a 409 — the delete tells the
        // two shortfalls apart by what survived it.
        assert!(matches!(
            slots[0].clone().delete(&db).await,
            Err(AppError::NotFound)
        ));
    }

    /// The claim on the publisher's own record, asked exactly the way the race
    /// asks it: a publish whose `RequireTeacher` snapshot predates a demotion
    /// must not land, because the sweep in that demotion has already chosen the
    /// slots it will take and would leave this one behind — under a role that
    /// can neither list nor delete it.
    ///
    /// Both halves matter: a row that is *not there* claims nothing (every other
    /// test here publishes for a teacher with no user row at all), and a live
    /// teacher+ passes through untouched.
    #[tokio::test]
    async fn a_publish_by_someone_who_lost_the_role_is_refused() {
        let db = crate::database::init_mem().await.unwrap();
        // Written as a row rather than through `User` — the password hasher is
        // private to that module, and the only column this asks about is `role`.
        db.query("CREATE user:eski SET username = 'eski', password_hash = 'x', role = 'student'")
            .await
            .unwrap()
            .check()
            .unwrap();

        // The row says `student`, so the claim refuses — one-off and weekly
        // alike, since both go through the same write.
        let teacher = UserId::from_key("eski");
        assert!(matches!(
            AppointmentSlot::create(&teacher, at(1_000), at(2_000), None, &db).await,
            Err(AppError::Forbidden(_))
        ));
        assert!(matches!(
            AppointmentSlot::publish_weekly(
                &teacher,
                at(1_000),
                at(2_000),
                None,
                at(1_000 + MILLIS_PER_WEEK),
                &db,
            )
            .await,
            Err(AppError::Forbidden(_))
        ));
        assert!(
            AppointmentSlot::list_for_teacher(&teacher, &db)
                .await
                .unwrap()
                .is_empty(),
            "a refused publish must write nothing"
        );

        // Promoted, the very same publish lands.
        db.query("UPDATE user:eski SET role = 'teacher'")
            .await
            .unwrap()
            .check()
            .unwrap();
        AppointmentSlot::create(&teacher, at(1_000), at(2_000), None, &db)
            .await
            .unwrap();
        assert_eq!(
            AppointmentSlot::list_for_teacher(&teacher, &db)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The other half of that claim: the publish that arrives *while* the
    /// demotion is running. Its sweep has already chosen the slots it will take
    /// ([`crate::domain::user::User::set_role`] reads them into `$slots` before
    /// it deletes), so a row landing after that read shares no key with anything
    /// the demotion writes and survives it — a slot owned by someone who can
    /// neither list nor delete it, forever. The claim on the user record is what
    /// puts the two transactions on one key.
    ///
    /// Real server, and `#[ignore]`d for it, like every other race here: the
    /// subject *is* the store's conflict detection, which `init_mem`'s embedded
    /// engine does not have — it commits both sides and answers `Ok` to each, so
    /// this passes there on broken code.
    ///
    /// The window is opened by the schema rather than by a lucky interleaving: a
    /// `DEFINE EVENT` on the sweep's own `DELETE` holds the demotion open inside
    /// its transaction, well past the read that chose `$slots`, so the publish
    /// lands in the middle of it every round.
    ///
    /// Stored state is the verdict, not the return value: whether the publish is
    /// refused outright or swept along with the rest of the calendar, no slot may
    /// outlive the role.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_publish_landing_inside_a_demotion_never_outlives_the_role() {
        use crate::domain::role::Role;
        use crate::domain::user::User;

        let (db, _serialized) = crate::database::init_test_server("slot_demotion_race").await;
        let (mut raced, mut published, mut stranded) = (0, 0, 0);
        for round in 0..4 {
            let key = format!("t{round}");
            let teacher = UserId::from_key(&key);
            db.query(format!(
                "CREATE user:{key} SET username = '{key}', password_hash = 'x', \
                 role = '{}';",
                Role::Teacher.as_str()
            ))
            .await
            .unwrap()
            .check()
            .unwrap();
            // A slot for the sweep to delete — that delete is the seam.
            AppointmentSlot::create(&teacher, at(1_000), at(2_000), None, &db)
                .await
                .unwrap();
            db.query(format!(
                "DEFINE EVENT OVERWRITE hold_the_sweep ON TABLE {APPOINTMENT_SLOT_TABLE} \
                 WHEN $event = 'DELETE' THEN {{ IF $before.teacher = \
                 type::record('user', '{key}') {{ SLEEP 300ms }} }};"
            ))
            .await
            .unwrap()
            .check()
            .unwrap();

            let demoting = {
                let db = db.clone();
                let target = teacher.clone();
                tokio::spawn(async move {
                    User::read(&target, &db)
                        .await
                        .unwrap()
                        .unwrap()
                        .set_role(Role::Student, &db)
                        .await
                })
            };
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if !demoting.is_finished() {
                raced += 1;
            }
            // The publish a `RequireTeacher` snapshot taken a moment ago allows.
            let landed = AppointmentSlot::create(
                &teacher,
                at(MILLIS_PER_WEEK),
                at(MILLIS_PER_WEEK + 1_000),
                None,
                &db,
            )
            .await;
            let swept = demoting.await.unwrap();
            assert!(swept.is_ok(), "round {round}: the demotion itself failed");
            if landed.is_ok() {
                published += 1;
            }
            if !AppointmentSlot::list_for_teacher(&teacher, &db)
                .await
                .unwrap()
                .is_empty()
            {
                stranded += 1;
            }
        }
        eprintln!(
            "a publish racing a demotion: {raced}/4 rounds landed inside it, \
             {published} publishes committed"
        );
        assert!(raced > 0, "no round ever reached the race");
        assert_eq!(
            stranded, 0,
            "a slot survived under a role that can neither list nor delete it"
        );
    }

    #[tokio::test]
    async fn weekly_publish_that_self_overlaps_is_refused() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        // A window longer than the weekly step collides with the next week.
        assert!(matches!(
            AppointmentSlot::publish_weekly(
                &teacher,
                at(0),
                at(MILLIS_PER_WEEK + 1),
                None,
                at(MILLIS_PER_WEEK),
                &db,
            )
            .await,
            Err(AppError::ConflictOwned(_))
        ));
        assert!(
            AppointmentSlot::list_for_teacher(&teacher, &db)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
