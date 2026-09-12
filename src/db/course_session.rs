//! The `course_session` table: reads, a course's timetable listing, the
//! request-scoped field update, and the delete that sweeps roll-call rows in
//! one transaction. Creation stands on the course foreign key, so a lesson
//! can never outlive its course; the web-facing doors and the
//! teacher-resolution rule live in [`crate::service::course_session`].

use crate::constant::COURSE_SESSION_TABLE;
use crate::database::{Database, foreign_key_violation, tx_with_retry};
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
    // The course foreign key is the existence proof the old bump-and-restore
    // "touch" trick faked: a lesson whose course is already gone — or which
    // is deleted while this insert is in flight — is refused here, so a
    // lesson can never outlive its course and 404 through
    // `session_with_course` forever — unreadable, unpatchable, undeletable.
    // `23503` is the parent-gone refusal, the same answer the touch
    // produced.
    let created = sqlx::query_as!(
        CourseSession,
        r#"INSERT INTO course_session (id, course, teacher, topic, starts_at, ends_at)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id AS "id: CourseSessionId", course AS "course: CourseId",
                     teacher AS "teacher: UserId", topic AS "topic: SessionTopic",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp""#,
        session.id.uuid(),
        session.course.uuid(),
        session.teacher.uuid(),
        session.topic.as_str(),
        session.starts_at.as_millis(),
        session.ends_at.map(|at| at.as_millis()),
    )
    .fetch_one(db)
    .await;
    match created {
        Ok(created) => Ok(created),
        Err(err) if foreign_key_violation(&err) => Err(AppError::NotFound),
        Err(err) => Err(err.into()),
    }
}

pub async fn read(db: &Database, id: &CourseSessionId) -> Result<Option<CourseSession>, AppError> {
    let session = sqlx::query_as!(
        CourseSession,
        r#"SELECT id AS "id: CourseSessionId", course AS "course: CourseId",
                  teacher AS "teacher: UserId", topic AS "topic: SessionTopic",
                  starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp"
           FROM course_session WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(session)
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
        "course_session WHERE course = $1",
        "ORDER BY starts_at DESC, id DESC",
    )
    .bind(course.uuid())
    .run::<CourseSession>(limit, offset, db)
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
    FieldUpdate::new(COURSE_SESSION_TABLE, session.id.uuid())
        .set(
            "teacher",
            teacher.map(|teacher| crate::db::page::Param::Uuid(teacher.uuid())),
        )
        .set("topic", topic.map(|topic| topic.as_str().to_owned()))
        .set("starts_at", starts_at.map(|at| at.as_millis()))
        .set(
            "ends_at",
            ends_at.map(|ends_at| crate::db::page::Param::OptI64(ends_at.map(|at| at.as_millis()))),
        )
        .ordered("starts_at", "ends_at", range_error())
        .run::<CourseSession>(db)
        .await
}

/// Delete the session and cascade-remove its roll-call rows, in one
/// transaction: as two unbatched queries a failure between them stranded
/// roll-call rows on a session that was already gone. The other half of
/// that invariant is [`crate::db::session_attendance::mark`], whose
/// session-row lock makes a mark and this delete take turns — a mark
/// committing after this cascade cannot name a session that is gone.
pub async fn delete(db: &Database, session: CourseSession) -> Result<CourseSession, AppError> {
    tx_with_retry(db, false, async move |tx| {
        // Roll-call rows first: the session row's own FK would refuse the
        // delete while they exist. An empty first sweep is fine — the
        // session may simply have had none.
        sqlx::query!(
            r#"DELETE FROM session_attendance WHERE session = $1"#,
            session.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query_as!(
            CourseSession,
            r#"DELETE FROM course_session WHERE id = $1
               RETURNING id AS "id: CourseSessionId", course AS "course: CourseId",
                     teacher AS "teacher: UserId", topic AS "topic: SessionTopic",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp""#,
            session.id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        deleted.ok_or(AppError::NotFound)
    })
    .await
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
