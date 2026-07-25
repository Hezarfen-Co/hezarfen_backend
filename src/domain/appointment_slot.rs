//! A teacher's published availability: "I am free here, book me". A slot is
//! pure calendar — it carries no booking state at all. Whether it is taken is
//! *derived* from its [`Appointment`] rows under [`APPOINTMENT_LOCK`], so a
//! rejected or cancelled booking frees the slot again without any flag to
//! reset (and without a UNIQUE index, which would keep a dead booking's seat).
//!
//! A recurring publish is expanded into concrete rows here, at write time,
//! sharing one `series` id — no recurrence rule is ever evaluated at read
//! time. Cancelling one week is then a plain row delete, and the whole series
//! is still addressable through its id.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_APPOINTMENT_NOTE_LEN, MAX_SLOT_OCCURRENCES};
use crate::database::{APPOINTMENT_SLOT_TABLE, Database};
use crate::domain::appointment::{APPOINTMENT_LOCK, Appointment};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::{MILLIS_PER_DAY, Timestamp};
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

/// One week, the only recurrence step this backend expands.
const MILLIS_PER_WEEK: i64 = 7 * MILLIS_PER_DAY;

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

    async fn insert(slot: AppointmentSlot, db: &Database) -> Result<AppointmentSlot, AppError> {
        let created: Option<AppointmentSlot> = db.create(slot.id.record()).content(slot).await?;
        created.ok_or_else(|| AppError::Internal("failed to create appointment slot".into()))
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
        Self::insert(
            AppointmentSlot {
                id: AppointmentSlotId::generate(),
                teacher: teacher.clone(),
                starts_at,
                ends_at,
                note,
                series: None,
                created_at: Timestamp::now(),
            },
            db,
        )
        .await
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
        // One statement, therefore one transaction: SurrealDB rolls the whole
        // `INSERT` back on any error. The row-by-row loop this replaces did not
        // — a database error at week 7 of 10 answered 500 with six stray weeks
        // already published, a half-series nobody asked for. It is also a single
        // round trip, so the lock is now held for two queries whatever the
        // occurrence count, instead of 1 + N (up to 52) sequential ones.
        //
        // No `BEGIN`/`COMMIT` wrapper: those consume result slots in this
        // version, and a lone statement is already atomic — the explicit form
        // would buy nothing but an off-by-one waiting to happen.
        let mut result = db
            .query("INSERT INTO appointment_slot $rows")
            .bind(("rows", rows))
            .await?
            .check()?;
        let mut created = result.take::<Vec<AppointmentSlot>>(0)?;
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

    /// Every slot published from `starts_at` onwards, earliest first — the
    /// bookable calendar a requester browses.
    pub async fn list_upcoming(
        from: Timestamp,
        db: &Database,
    ) -> Result<Vec<AppointmentSlot>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM appointment_slot WHERE ends_at > $from \
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
    /// The occupancy read and the delete run under [`APPOINTMENT_LOCK`], so a
    /// booking cannot land between them and outlive its slot.
    pub async fn delete(self, db: &Database) -> Result<AppointmentSlot, AppError> {
        let _guard = APPOINTMENT_LOCK.lock().await;
        Self::delete_locked(std::slice::from_ref(&self.id), db).await?;
        Ok(self)
    }

    /// Delete a whole recurring publish, all-or-nothing: if *any* occurrence
    /// still holds a live booking the entire series is refused, so the teacher
    /// deals with the person waiting instead of silently keeping a stray week.
    pub async fn delete_series(
        series: &SlotSeries,
        db: &Database,
    ) -> Result<Vec<AppointmentSlot>, AppError> {
        let _guard = APPOINTMENT_LOCK.lock().await;
        let slots = Self::list_for_series(series, db).await?;
        if slots.is_empty() {
            return Err(AppError::NotFound);
        }
        let ids: Vec<AppointmentSlotId> = slots.iter().map(|slot| slot.id.clone()).collect();
        Self::delete_locked(&ids, db).await?;
        Ok(slots)
    }

    /// Shared body of both deletes. Caller must already hold
    /// [`APPOINTMENT_LOCK`]: the live-booking check is only meaningful while
    /// no booking can be written.
    async fn delete_locked(ids: &[AppointmentSlotId], db: &Database) -> Result<(), AppError> {
        for id in ids {
            if Appointment::has_live_booking(id, db).await? {
                return Err(AppError::Conflict(
                    "the slot has a pending or approved booking",
                ));
            }
        }
        let records: Vec<RecordId> = ids.iter().map(|id| id.record()).collect();
        let mut result = db
            .query("DELETE appointment WHERE slot IN $slots; DELETE $slots RETURN BEFORE;")
            .bind(("slots", records))
            .await?
            .check()?;
        if result.take::<Vec<AppointmentSlot>>(1)?.is_empty() {
            return Err(AppError::NotFound);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
