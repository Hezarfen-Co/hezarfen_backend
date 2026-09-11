//! Slot publishing and withdrawal: the one-off and weekly publishes, the
//! single and series deletes, and the reads the web layer renders the
//! calendar from. Row reads, listings, the overlap probes, the role-claimed
//! INSERT, and the guarded delete live in [`crate::db::appointment_slot`].
//!
//! Publishing holds [`APPOINTMENT_LOCK`] from the conflict check through the
//! write — the same lock the booking path takes — so a concurrent publish or
//! booking cannot slip a colliding window in between the check and the
//! insert.

use crate::database::Database;
use crate::db::appointment_slot;
use crate::domain::appointment::Appointment;
use crate::domain::appointment_slot::{AppointmentSlot, AppointmentSlotId, SlotNote, SlotSeries};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

use super::appointment::APPOINTMENT_LOCK;

/// The row, for callers that only inspect it — the web layer's gates read
/// through here.
pub async fn read(
    db: &Database,
    id: &AppointmentSlotId,
) -> Result<Option<AppointmentSlot>, AppError> {
    appointment_slot::read(db, id).await
}

/// A teacher's own calendar, earliest first — the web layer's calendar read.
pub async fn list_for_teacher(
    db: &Database,
    teacher: &UserId,
) -> Result<Vec<AppointmentSlot>, AppError> {
    appointment_slot::list_for_teacher(db, teacher).await
}

/// Every slot whose window has not opened yet, earliest first — the bookable
/// calendar a requester browses.
pub async fn list_upcoming(
    db: &Database,
    from: Timestamp,
) -> Result<Vec<AppointmentSlot>, AppError> {
    appointment_slot::list_upcoming(db, from).await
}

/// Every slot of one recurring publish, earliest first.
pub async fn list_for_series(
    db: &Database,
    series: &SlotSeries,
) -> Result<Vec<AppointmentSlot>, AppError> {
    appointment_slot::list_for_series(db, series).await
}

/// Publish one slot.
pub async fn create(
    db: &Database,
    teacher: &UserId,
    starts_at: Timestamp,
    ends_at: Timestamp,
    note: Option<SlotNote>,
) -> Result<AppointmentSlot, AppError> {
    AppointmentSlot::check_window(starts_at, ends_at)?;
    // Check-then-insert under the lock so a concurrent publish or booking
    // can't slip a colliding window in between.
    let _guard = APPOINTMENT_LOCK.lock().await;
    if appointment_slot::conflicts_existing(db, teacher, starts_at, ends_at).await? {
        return Err(AppError::ConflictOwned(
            "this time overlaps a slot you have already published".into(),
        ));
    }
    appointment_slot::insert_claimed(
        db,
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
    db: &Database,
    teacher: &UserId,
    starts_at: Timestamp,
    ends_at: Timestamp,
    note: Option<SlotNote>,
    until: Timestamp,
) -> Result<Vec<AppointmentSlot>, AppError> {
    let windows = AppointmentSlot::weekly_windows(starts_at, ends_at, until)?;
    // Validate the whole batch before writing a single row: all-or-nothing,
    // so a mid-series collision never leaves stray weeks behind. Held under
    // the lock from the check through the write, matching `create`.
    let _guard = APPOINTMENT_LOCK.lock().await;
    // (a) against the slots already in the database. `weekly_windows` walks
    // forward, so the first and last occurrence bound every one of them.
    let (first, last) = (windows[0], windows[windows.len() - 1]);
    let published = appointment_slot::windows_in_span(db, teacher, first.0, last.1).await?;
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
    let mut created = appointment_slot::insert_claimed(db, teacher, rows).await?;
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

/// Delete one slot, refusing (409) while a live booking sits on it — the
/// requester is expecting that meeting, so it must be rejected first.
/// Settled bookings (rejected/cancelled) are history of a slot that is
/// going away, so they cascade out with it, like `Event::delete`'s rows.
///
/// The guard is the delete's own `WHERE`, so no booking can land between a
/// check and the row going away.
pub async fn delete(db: &Database, slot: AppointmentSlot) -> Result<AppointmentSlot, AppError> {
    appointment_slot::delete_free(db, std::slice::from_ref(slot.get_id())).await?;
    Ok(slot)
}

/// Delete a whole recurring publish, all-or-nothing: if *any* occurrence
/// still holds a live booking the entire series is refused, so the teacher
/// deals with the person waiting instead of silently keeping a stray week.
pub async fn delete_series(
    db: &Database,
    series: &SlotSeries,
) -> Result<Vec<AppointmentSlot>, AppError> {
    let slots = appointment_slot::list_for_series(db, series).await?;
    if slots.is_empty() {
        return Err(AppError::NotFound);
    }
    let ids: Vec<AppointmentSlotId> = slots.iter().map(|slot| slot.id.clone()).collect();
    appointment_slot::delete_free(db, &ids).await?;
    Ok(slots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{APPOINTMENT_SLOT_TABLE, MILLIS_PER_WEEK, SLOT_OCCUPIED_FIELD};
    use crate::db::cap;

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    #[tokio::test]
    async fn a_publish_shares_one_series_and_orders_its_ids() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slots = publish_weekly(
            &db,
            &teacher,
            at(1_000),
            at(2_000),
            Some(SlotNote::try_new("veli toplantısı").unwrap()),
            at(1_000 + 4 * MILLIS_PER_WEEK),
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

        let listed = list_for_series(&db, &series).await.unwrap();
        assert_eq!(listed.len(), 5);
    }

    #[tokio::test]
    async fn overlapping_publish_is_refused_but_touching_is_allowed() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        create(&db, &teacher, at(1_000), at(2_000), None)
            .await
            .unwrap();

        // Overlaps the existing [1000,2000) → 409.
        assert!(matches!(
            create(&db, &teacher, at(1_500), at(2_500), None).await,
            Err(AppError::ConflictOwned(_))
        ));
        // Touching at the boundary (ends where the next starts) → allowed.
        assert!(
            create(&db, &teacher, at(2_000), at(3_000), None)
                .await
                .is_ok()
        );
        assert!(create(&db, &teacher, at(0), at(1_000), None).await.is_ok());
        // A different teacher sharing the same window is fine.
        let other = UserId::from_key("t2");
        assert!(
            create(&db, &other, at(1_000), at(2_000), None)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn weekly_publish_overlapping_an_existing_slot_writes_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        // A lone slot on the third week of the coming series.
        create(
            &db,
            &teacher,
            at(1_000 + 2 * MILLIS_PER_WEEK),
            at(2_000 + 2 * MILLIS_PER_WEEK),
            None,
        )
        .await
        .unwrap();

        let before = list_for_teacher(&db, &teacher).await.unwrap().len();
        assert!(matches!(
            publish_weekly(
                &db,
                &teacher,
                at(1_000),
                at(2_000),
                None,
                at(1_000 + 4 * MILLIS_PER_WEEK),
            )
            .await,
            Err(AppError::ConflictOwned(_))
        ));
        // All-or-nothing: not one occurrence of the rejected series was written.
        assert_eq!(list_for_teacher(&db, &teacher).await.unwrap().len(), before);
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
        create(&db, &teacher, at(0), at(1_000), None).await.unwrap();
        create(&db, &teacher, at(last + 1_000), at(last + 2_000), None)
            .await
            .unwrap();
        assert_eq!(
            publish_weekly(&db, &teacher, at(1_000), at(2_000), None, at(last))
                .await
                .unwrap()
                .len(),
            5
        );

        // One millisecond into the last occurrence → the whole publish is 409.
        let db = crate::database::init_mem().await.unwrap();
        create(&db, &teacher, at(last + 999), at(last + 3_000), None)
            .await
            .unwrap();
        assert!(matches!(
            publish_weekly(&db, &teacher, at(1_000), at(2_000), None, at(last)).await,
            Err(AppError::ConflictOwned(_))
        ));
        assert_eq!(list_for_teacher(&db, &teacher).await.unwrap().len(), 1);
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
        let slots = publish_weekly(
            &db,
            &teacher,
            at(1_000),
            at(2_000),
            None,
            at(1_000 + 2 * MILLIS_PER_WEEK),
        )
        .await
        .unwrap();
        let series = slots[0].get_series().cloned().unwrap();

        // A free slot goes, and its series is deletable while every week is free.
        assert!(delete(&db, slots[0].clone()).await.is_ok());
        // The seat on the middle week is taken — no booking row, exactly as a
        // racing booking would leave it mid-flight.
        assert!(
            cap::claim(&slots[1].get_id().record(), SLOT_OCCUPIED_FIELD, 1, &db)
                .await
                .unwrap()
        );
        assert!(matches!(
            delete(&db, slots[1].clone()).await,
            Err(AppError::Conflict(
                "the slot has a pending or approved booking"
            ))
        ));
        assert!(matches!(
            delete_series(&db, &series).await,
            Err(AppError::Conflict(_))
        ));
        // Refused, not half-applied: the free third week is still published.
        assert_eq!(list_for_series(&db, &series).await.unwrap().len(), 2);

        // A slot that is simply gone is a 404, not a 409 — the delete tells the
        // two shortfalls apart by what survived it.
        assert!(matches!(
            delete(&db, slots[0].clone()).await,
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
            create(&db, &teacher, at(1_000), at(2_000), None).await,
            Err(AppError::Forbidden(_))
        ));
        assert!(matches!(
            publish_weekly(
                &db,
                &teacher,
                at(1_000),
                at(2_000),
                None,
                at(1_000 + MILLIS_PER_WEEK),
            )
            .await,
            Err(AppError::Forbidden(_))
        ));
        assert!(
            list_for_teacher(&db, &teacher).await.unwrap().is_empty(),
            "a refused publish must write nothing"
        );

        // Promoted, the very same publish lands.
        db.query("UPDATE user:eski SET role = 'teacher'")
            .await
            .unwrap()
            .check()
            .unwrap();
        create(&db, &teacher, at(1_000), at(2_000), None)
            .await
            .unwrap();
        assert_eq!(list_for_teacher(&db, &teacher).await.unwrap().len(), 1);
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
            create(&db, &teacher, at(1_000), at(2_000), None)
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
            let landed = create(
                &db,
                &teacher,
                at(MILLIS_PER_WEEK),
                at(MILLIS_PER_WEEK + 1_000),
                None,
            )
            .await;
            let swept = demoting.await.unwrap();
            assert!(swept.is_ok(), "round {round}: the demotion itself failed");
            if landed.is_ok() {
                published += 1;
            }
            if !list_for_teacher(&db, &teacher).await.unwrap().is_empty() {
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
            publish_weekly(
                &db,
                &teacher,
                at(0),
                at(MILLIS_PER_WEEK + 1),
                None,
                at(MILLIS_PER_WEEK),
            )
            .await,
            Err(AppError::ConflictOwned(_))
        ));
        assert!(list_for_teacher(&db, &teacher).await.unwrap().is_empty());
    }
}
