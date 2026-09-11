//! The `course_session` table: reads, a course's timetable listing, the
//! request-scoped field update, and the delete that sweeps roll-call rows in
//! one transaction. Creation writes the course row through
//! [`cap::touch_and_create`] so a lesson can never outlive its course; the
//! web-facing doors and the teacher-resolution rule live in
//! [`crate::service::course_session`].

use surrealdb::types::SurrealValue;

use crate::constant::ENROLLMENT_COUNT_FIELD;
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::course_session::{CourseSession, CourseSessionId, SessionTopic};
use crate::domain::timestamp::{Timestamp, range_error};
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(
    db: &Database,
    course: &CourseId,
    teacher: &UserId,
    topic: SessionTopic,
    starts_at: Timestamp,
    ends_at: Option<Timestamp>,
) -> Result<CourseSession, AppError> {
    let session = CourseSession {
        id: CourseSessionId::generate(),
        course: course.clone(),
        teacher: teacher.clone(),
        topic,
        starts_at,
        ends_at,
    };
    // The course row is *written* (bumped and put back), not read, so this
    // collides with `Course::delete`'s cascade: a lesson that outlives its
    // course 404s through `session_with_course` forever — unreadable,
    // unpatchable, undeletable. See [`cap::touch_and_create`].
    cap::touch_and_create(
        &course.record(),
        ENROLLMENT_COUNT_FIELD,
        &session.id.record(),
        &session,
        db,
    )
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn read(db: &Database, id: &CourseSessionId) -> Result<Option<CourseSession>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// A course's sessions, most recent lesson first. Ordered by `starts_at`
/// (not id): a timetable is read by when the lesson happens, not by when
/// the row was created.
pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseSession>, i64), AppError> {
    PagedList::new(
        "course_session WHERE course = $course",
        "ORDER BY starts_at DESC, id DESC",
    )
    .bind("course", course.record())
    .run(limit, offset, db)
    .await
}

/// Request-scoped: the handler holds nothing across its read and this
/// write — the range check rides in the `WHERE` — so a field the request
/// omitted (`None`) is not written at all. Re-sending the snapshot's value
/// instead would revert a concurrent PATCH of that field — scoping the
/// `SET` alone does not stop that, the values have to come from the
/// request. `ends_at` is nullable, so it takes the outer/inner
/// `Option<Option<_>>`: `None` = omitted (keep), `Some(None)` = clear.
pub async fn update(
    db: &Database,
    session: CourseSession,
    teacher: Option<UserId>,
    topic: Option<SessionTopic>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Option<Timestamp>>,
) -> Result<CourseSession, AppError> {
    FieldUpdate::new(session.id.record())
        .set("teacher", teacher.map(|teacher| teacher.record()))
        .set("topic", topic)
        .set("starts_at", starts_at)
        .set("ends_at", ends_at)
        .ordered("starts_at", "ends_at", range_error())
        .run::<CourseSession>(db)
        .await
}

/// Delete the session and cascade-remove its roll-call rows, in one
/// transaction: as two unbatched queries a failure between them stranded
/// roll-call rows on a session that was already gone. The other half of
/// that invariant is [`crate::db::session_attendance::mark`],
/// which proves the session still exists inside its own write — a
/// transaction here cannot stop a mark that commits *after* this one.
pub async fn delete(db: &Database, session: CourseSession) -> Result<CourseSession, AppError> {
    let (mut result, mut errors) = transaction_with_retry(
        db,
        "BEGIN TRANSACTION;
             DELETE session_attendance WHERE session = $s;
             LET $before = (DELETE $s RETURN BEFORE);
             RETURN $before;
             COMMIT TRANSACTION;",
        &[("s".into(), session.id.record().into_value())],
        // No THROW of its own — an unconditional cascade, so the only
        // error worth telling apart is a lost round (see
        // [`crate::db::exam::delete`]).
        &[],
    )
    .await?;
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Read through the trailing `RETURN`, never a hand-counted slot.
    let slot = result.num_statements().saturating_sub(2);
    let deleted: Option<CourseSession> =
        result.take::<Vec<CourseSession>>(slot)?.into_iter().next();
    deleted.ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::COURSE_SESSION_TABLE;

    /// A lesson must not outlive the course it is a lesson of: an orphan 404s
    /// forever through `session_with_course` — unreadable, unpatchable,
    /// undeletable.
    ///
    /// [`create`] therefore *writes* the course row rather than
    /// reading it ([`cap::touch_and_create`]); the harness and the window it
    /// races in are documented on
    /// [`crate::db::course::assert_no_child_outlives_a_course_delete`].
    /// Mutation-tested: with the bare `db.create` this shipped with, all four
    /// rounds orphan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_session_never_outlives_its_course() {
        fn make(course: CourseId, db: Database) -> tokio::task::JoinHandle<Result<(), AppError>> {
            tokio::spawn(async move {
                create(
                    &db,
                    &course,
                    &UserId::generate(),
                    SessionTopic::try_new("limits").unwrap(),
                    Timestamp::from_millis(1),
                    None,
                )
                .await
                .map(|_| ())
            })
        }
        crate::db::course::assert_no_child_outlives_a_course_delete(
            "session_orphan_race",
            COURSE_SESSION_TABLE,
            make,
        )
        .await;
    }
}
