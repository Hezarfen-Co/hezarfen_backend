//! The `course_session` table: reads, an instance's timetable listing, the
//! request-scoped field update, and the delete that sweeps roll-call rows in
//! one transaction. Creation stands on the instance foreign key, so a lesson
//! can never outlive the class×course instance it is a lesson of; the
//! web-facing doors and the teacher-resolution rule live in
//! [`crate::service::course_session`].

use crate::constant::COURSE_SESSION_TABLE;
use crate::database::{Database, foreign_key_violation, tx_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::class_course::ClassCourseId;
use crate::domain::course_session::{CourseSession, CourseSessionId, SessionTopic};
use crate::domain::timestamp::{Timestamp, range_error};
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(
    db: &Database,
    class_course: &ClassCourseId,
    teacher: &UserId,
    topic: SessionTopic,
    starts_at: Timestamp,
    ends_at: Option<Timestamp>,
) -> Result<CourseSession, AppError> {
    let session = CourseSession {
        id: CourseSessionId::generate(),
        class_course: class_course.clone(),
        teacher: *teacher,
        topic,
        starts_at,
        ends_at,
    };
    // The instance foreign key is the existence proof the old
    // bump-and-restore "touch" trick faked: a lesson whose instance is
    // already gone — or which is detached while this insert is in flight — is
    // refused here, so a lesson can never outlive the instance and 404
    // through the session reads forever — unreadable, unpatchable,
    // undeletable. `23503` is the parent-gone refusal, the same answer the
    // touch produced.
    let created = sqlx::query_as!(
        CourseSession,
        r#"INSERT INTO course_session (id, class_course, teacher, topic, starts_at, ends_at)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id AS "id: CourseSessionId",
                     class_course AS "class_course: ClassCourseId",
                     teacher AS "teacher: UserId", topic AS "topic: SessionTopic",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp""#,
        session.id.uuid(),
        session.class_course.uuid(),
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
        r#"SELECT id AS "id: CourseSessionId",
                  class_course AS "class_course: ClassCourseId",
                  teacher AS "teacher: UserId", topic AS "topic: SessionTopic",
                  starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp"
           FROM course_session WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(session)
}

/// An instance's sessions, most recent lesson first. Ordered by `starts_at`
/// (not id): a timetable is read by when the lesson happens, not by when
/// the row was created.
pub async fn list_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseSession>, i64), AppError> {
    PagedList::new(
        "course_session WHERE class_course = $1",
        "ORDER BY starts_at DESC, id DESC",
    )
    .bind(class_course.uuid())
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
    // Cascade mode: a roll-call row written *after* the sweep's snapshot (a
    // mark racing the delete) still references the session when the session
    // row goes, and that 23503 means "a racing writer is mid-flight" — the
    // re-send sweeps the settled rows and succeeds.
    tx_with_retry(db, true, async move |tx| {
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
               RETURNING id AS "id: CourseSessionId",
                     class_course AS "class_course: ClassCourseId",
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
    async fn a_session_never_outlives_its_course() {
        use crate::domain::course::CourseId;
        use crate::domain::user::UserId;

        fn make(course: CourseId, db: Database) -> tokio::task::JoinHandle<Result<(), AppError>> {
            tokio::spawn(async move {
                // The creator is a foreign key now: a real `app_user` row.
                let creator = UserId::generate();
                sqlx::query(
                    "INSERT INTO app_user (id, username, created_at, role) \
                     VALUES ($1, $2, 0, 'teacher')",
                )
                .bind(creator.uuid())
                .bind(format!("session-race-{}", &creator.key()[30..]))
                .execute(&db)
                .await
                .unwrap();
                // The lesson hangs off the instance now, so the round has to
                // attach one — that claim is exactly what the racing delete
                // contends on, and a course already gone refuses right here.
                let class = crate::db::class_group::create(
                    &db,
                    &creator,
                    crate::domain::class_group::ClassName::try_new("9-A").unwrap(),
                    crate::domain::grade::GradeLevel::new(9).unwrap(),
                    None,
                    None,
                )
                .await?
                .get_id()
                .clone();
                let instance = crate::service::class_course::attach(&db, &class, &course, &creator)
                    .await?
                    .get_id()
                    .clone();
                create(
                    &db,
                    &instance,
                    &creator,
                    SessionTopic::try_new("limits").unwrap(),
                    Timestamp::from_millis(1),
                    None,
                )
                .await
                .map(|_| ())
            })
        }
        crate::db::course::assert_no_child_outlives_a_course_delete(
            "course_session",
            "SELECT count(*) FROM course_session s
               JOIN class_course cc ON cc.id = s.class_course
              WHERE cc.course = $1",
            make,
        )
        .await;
    }
}
