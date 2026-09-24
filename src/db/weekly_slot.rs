//! The `offering_slot` and `class_course_slot` tables: the reads and writes
//! behind the weekly plan. The row shape lives in
//! [`crate::domain::weekly_slot`]; the workflows (the overlap and cap checks,
//! the offer-side vs class-side doors) in [`crate::service::weekly_slot`].
//!
//! Reads are plain ordered lists (`weekday`, then `starts_at`). The class-side
//! writes are the override doors: every statement that adds or drops a
//! section's own row also flips `class_course.weekly_plan_inherited` to
//! `FALSE`, and the reset flips it back to `TRUE` while deleting the rows —
//! flag and data in one statement, so the two can never be observed
//! disagreeing.
//!
//! Times cross the statement boundary as minutes past midnight: the
//! `::bigint`-bound minute is multiplied into an `interval` and cast to
//! `TIME` on the way in, and the read casts the stored `TIME` back through
//! `EXTRACT(EPOCH ...)` — the same "the cast is the statement's own" shape
//! every `SMALLINT`↔`i64` column here uses.

use sqlx::postgres::PgConnection;

use crate::database::{Database, unique_violation};
use crate::domain::class_course::ClassCourseId;
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::course_session::SessionTopic;
use crate::domain::weekly_slot::{SlotMinute, Weekday, WeeklySlot, WeeklySlotId};
use crate::error::AppError;

/// One offering's slots, weekday first then start time.
pub async fn list_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
) -> Result<Vec<WeeklySlot>, AppError> {
    let mut conn = db.acquire().await?;
    list_for_offering_on(&mut conn, offering).await
}

/// One section's own slots, weekday first then start time — the offering's
/// rows are *not* consulted; that choice is the resolver's, off the flag.
pub async fn list_for_class(
    db: &Database,
    instance: &ClassCourseId,
) -> Result<Vec<WeeklySlot>, AppError> {
    let mut conn = db.acquire().await?;
    list_for_class_on(&mut conn, instance).await
}

pub(crate) async fn list_for_offering_on(
    db: &mut PgConnection,
    offering: &CourseOfferingId,
) -> Result<Vec<WeeklySlot>, AppError> {
    sqlx::query_as!(
        WeeklySlot,
        r#"SELECT id AS "id: WeeklySlotId",
                  weekday AS "weekday: Weekday",
                  (EXTRACT(EPOCH FROM starts_at - TIME '00:00:00') / 60)::bigint AS "starts_at!: SlotMinute",
                  (EXTRACT(EPOCH FROM ends_at - TIME '00:00:00') / 60)::bigint AS "ends_at!: SlotMinute",
                  topic AS "topic?: SessionTopic"
           FROM offering_slot
           WHERE offering = $1
           ORDER BY weekday, starts_at"#,
        offering.uuid(),
    )
    .fetch_all(db)
    .await
    .map_err(Into::into)
}

pub(crate) async fn list_for_class_on(
    db: &mut PgConnection,
    instance: &ClassCourseId,
) -> Result<Vec<WeeklySlot>, AppError> {
    sqlx::query_as!(
        WeeklySlot,
        r#"SELECT id AS "id: WeeklySlotId",
                  weekday AS "weekday: Weekday",
                  (EXTRACT(EPOCH FROM starts_at - TIME '00:00:00') / 60)::bigint AS "starts_at!: SlotMinute",
                  (EXTRACT(EPOCH FROM ends_at - TIME '00:00:00') / 60)::bigint AS "ends_at!: SlotMinute",
                  topic AS "topic?: SessionTopic"
           FROM class_course_slot
           WHERE class_course = $1
           ORDER BY weekday, starts_at"#,
        instance.uuid(),
    )
    .fetch_all(db)
    .await
    .map_err(Into::into)
}

/// Take the owner's row lock inside the caller's transaction, so two adds for
/// one owner cannot both clear the overlap/cap pre-check (READ COMMITTED sees
/// the rival's uncommitted insert as absent). `FOR NO KEY UPDATE`: it
/// serializes the slot writers against each other without blocking the
/// attach pump's `FOR KEY SHARE` reads. `Err(NotFound)` when the owner is
/// gone.
pub(crate) async fn lock_offering_tx(
    tx: &mut PgConnection,
    offering: &CourseOfferingId,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"SELECT id AS "id: CourseOfferingId" FROM course_offering WHERE id = $1
           FOR NO KEY UPDATE"#,
        offering.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)
    .map(|_| ())
}

/// The class-side twin of [`lock_offering_tx`].
pub(crate) async fn lock_class_tx(
    tx: &mut PgConnection,
    instance: &ClassCourseId,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"SELECT id AS "id: ClassCourseId" FROM class_course WHERE id = $1
           FOR NO KEY UPDATE"#,
        instance.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)
    .map(|_| ())
}

/// Insert one slot of the offering's template week, inside the caller's
/// transaction. The `(offering, weekday, starts_at)` `UNIQUE` — reachable
/// only by a duplicate insert that raced the caller's pre-check — answers
/// 409 `slot_overlap`, the same refusal the pre-check gives the settled
/// duplicate.
pub(crate) async fn add_for_offering_tx(
    tx: &mut PgConnection,
    slot: &WeeklySlot,
    offering: &CourseOfferingId,
) -> Result<WeeklySlot, AppError> {
    sqlx::query_as!(
        WeeklySlot,
        r#"INSERT INTO offering_slot (id, offering, weekday, starts_at, ends_at, topic)
           VALUES ($1, $2, $3,
                   ($4::bigint * interval '1 minute')::time,
                   ($5::bigint * interval '1 minute')::time,
                   $6)
           RETURNING id AS "id: WeeklySlotId",
                     weekday AS "weekday: Weekday",
                     (EXTRACT(EPOCH FROM starts_at - TIME '00:00:00') / 60)::bigint AS "starts_at!: SlotMinute",
                     (EXTRACT(EPOCH FROM ends_at - TIME '00:00:00') / 60)::bigint AS "ends_at!: SlotMinute",
                     topic AS "topic?: SessionTopic""#,
        slot.get_id().uuid(),
        offering.uuid(),
        slot.get_weekday().get(),
        slot.get_starts_at().get(),
        slot.get_ends_at().get(),
        slot.get_topic().map(|topic| topic.as_str()),
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if unique_violation(&e).is_some() {
            AppError::ConflictCoded {
                code: "slot_overlap",
                message: "a slot already occupies this weekday and start time".into(),
            }
        } else {
            e.into()
        }
    })
}

/// Insert one slot of the section's own week **and flip
/// `weekly_plan_inherited = FALSE` in the same statement** — the data-modifying
/// CTE that makes the row and the override one write. A `flip` that matched no
/// row means the instance vanished mid-transaction: `Err(NotFound)` rolls the
/// insert back.
pub(crate) async fn add_for_class_tx(
    tx: &mut PgConnection,
    slot: &WeeklySlot,
    instance: &ClassCourseId,
) -> Result<WeeklySlot, AppError> {
    sqlx::query_as!(
        WeeklySlot,
        r#"WITH ins AS (
               INSERT INTO class_course_slot (id, class_course, weekday, starts_at, ends_at, topic)
               VALUES ($1, $2, $3,
                       ($4::bigint * interval '1 minute')::time,
                       ($5::bigint * interval '1 minute')::time,
                       $6)
               RETURNING id, weekday, starts_at, ends_at, topic
           ), flip AS (
               UPDATE class_course SET weekly_plan_inherited = FALSE
               WHERE id = $2 AND EXISTS (SELECT 1 FROM ins)
               RETURNING id
           )
           SELECT ins.id AS "id: WeeklySlotId",
                  ins.weekday AS "weekday: Weekday",
                  (EXTRACT(EPOCH FROM ins.starts_at - TIME '00:00:00') / 60)::bigint AS "starts_at!: SlotMinute",
                  (EXTRACT(EPOCH FROM ins.ends_at - TIME '00:00:00') / 60)::bigint AS "ends_at!: SlotMinute",
                  ins.topic AS "topic?: SessionTopic"
           FROM ins
           WHERE EXISTS (SELECT 1 FROM flip)"#,
        slot.get_id().uuid(),
        instance.uuid(),
        slot.get_weekday().get(),
        slot.get_starts_at().get(),
        slot.get_ends_at().get(),
        slot.get_topic().map(|topic| topic.as_str()),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)
}

/// Drop one slot of the offering's template week. A slot of another offering,
/// or one already gone, is `Err(NotFound)` — the id alone never deletes; the
/// owner scoping does.
pub async fn remove_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
    slot: &WeeklySlotId,
) -> Result<(), AppError> {
    let result = sqlx::query!(
        "DELETE FROM offering_slot WHERE id = $1 AND offering = $2",
        slot.uuid(),
        offering.uuid(),
    )
    .execute(db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Drop one slot of the section's own week **and flip the flag to `FALSE` in
/// the same statement** — dropping a row is an override decision even when it
/// empties the set (an empty own set is how a section clears its timetable).
/// `Err(NotFound)` when the id names no row of this instance: the flag stays
/// as it was.
pub async fn remove_for_class(
    db: &Database,
    instance: &ClassCourseId,
    slot: &WeeklySlotId,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"WITH del AS (
               DELETE FROM class_course_slot WHERE id = $1 AND class_course = $2
               RETURNING id
           ), flip AS (
               UPDATE class_course SET weekly_plan_inherited = FALSE
               WHERE id = $2 AND EXISTS (SELECT 1 FROM del)
               RETURNING id
           )
           SELECT id AS "id: ClassCourseId" FROM flip"#,
        slot.uuid(),
        instance.uuid(),
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
    .map(|_| ())
}

/// Reset the section's weekly plan to inherit: delete every one of its own
/// slots and flip `weekly_plan_inherited` back to `TRUE` in the same
/// statement. Idempotent for an already-inherited section (the delete finds
/// nothing, the flip re-asserts `TRUE`); `Err(NotFound)` when the instance
/// itself is gone.
pub async fn reset_for_class(db: &Database, instance: &ClassCourseId) -> Result<(), AppError> {
    sqlx::query!(
        r#"WITH del AS (
               DELETE FROM class_course_slot WHERE class_course = $1
               RETURNING id
           ), flip AS (
               UPDATE class_course SET weekly_plan_inherited = TRUE
               WHERE id = $1
               RETURNING id
           )
           SELECT id AS "id: ClassCourseId" FROM flip"#,
        instance.uuid(),
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
    .map(|_| ())
}
