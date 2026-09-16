//! The `course_membership` table: a student's individual membership in a
//! *school-scoped* course (D9) — a kulüp or an etüt, joined directly rather
//! than through a şube's instance.
//!
//! It is the `enrollment` table's twin minus two things: the instance key (a
//! club has no class to belong to) and the roster pump's `source` tag (a
//! membership is placed by a human and swept by no class). The membership
//! counter on the course moves in the same statement as the row, so the
//! catalog's delete guard reads a number its own writes maintain.
//!
//! Whether a given course *may* be joined this way is
//! [`crate::service::enrollment::join_activity`]'s decision: a class-delivered
//! ders ([`crate::domain::course::CourseKind::is_class_delivered`]) is joined
//! through its instance, never here.

use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::course_membership::CourseMembership;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// What [`add`]'s transaction settled on.
enum Verdict {
    /// This pair already holds a row — the idempotent hit.
    Held,
    /// The person being added is no longer a student.
    Unfit,
    /// The row is written.
    Made(CourseMembership),
}

/// Add (idempotently) `user` to one school-scoped course. The table's primary
/// key *is* the (course, user) pair, so concurrent calls converge on one row
/// instead of racing into a 500: the loser reads the winner's row back and
/// returns it, and the counter moves once.
///
/// The role check takes the user row `FOR NO KEY UPDATE` — the same lock a
/// demotion cascade takes before it sweeps memberships — so an add and a
/// demotion serialize in both directions.
pub async fn add(
    db: &Database,
    course: &CourseId,
    user: &UserId,
    added_by: &UserId,
) -> Result<CourseMembership, AppError> {
    if let Some(existing) = read_for_user(db, course, user).await? {
        return Ok(existing);
    }
    let course_id = course.clone();
    let (user_id, added_by_id) = (*user, *added_by);
    let created_at = Timestamp::now();
    let verdict = tx_with_retry(db, false, async move |tx| {
        // The duplicate gate rides ahead of the role check, so "you are
        // already in" still outranks everything.
        let held = sqlx::query!(
            r#"SELECT 1 AS "one" FROM course_membership WHERE course = $1 AND app_user = $2"#,
            course_id.uuid(),
            user_id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        if held.is_some() {
            return Ok(Verdict::Held);
        }
        let role = sqlx::query!(
            r#"SELECT role FROM app_user WHERE id = $1 FOR NO KEY UPDATE"#,
            user_id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        if role.map(|row| row.role).as_deref() != Some(Role::Student.as_str()) {
            return Ok(Verdict::Unfit);
        }
        // The counter and the row in one statement: zero rows means the
        // course is gone — the caller's own read answers that 404 one instant
        // earlier.
        let row = sqlx::query_as!(
            CourseMembership,
            r#"WITH seat AS (
                 UPDATE course
                    SET course_membership_count = course_membership_count + 1
                  WHERE id = $1
                  RETURNING 1)
               INSERT INTO course_membership (course, app_user, added_by, created_at)
               SELECT $1, $2, $3, $4 WHERE EXISTS (SELECT 1 FROM seat)
               RETURNING course AS "course: CourseId", app_user AS "user: UserId",
                         added_by AS "added_by: UserId",
                         created_at AS "created_at: Timestamp""#,
            course_id.uuid(),
            user_id.uuid(),
            added_by_id.uuid(),
            created_at.as_millis(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        match row {
            Some(row) => Ok(Verdict::Made(row)),
            None => {
                let now_held = sqlx::query!(
                    r#"SELECT 1 AS "one" FROM course_membership
                       WHERE course = $1 AND app_user = $2"#,
                    course_id.uuid(),
                    user_id.uuid(),
                )
                .fetch_optional(&mut *tx)
                .await?;
                if now_held.is_some() {
                    return Ok(Verdict::Held);
                }
                Err(AppError::NotFound)
            }
        }
    })
    .await;
    // A rival landing between the gate and the insert is answered off the
    // table's own primary key — same verdict, no second count.
    let verdict = match verdict {
        Err(AppError::Db(err))
            if unique_violation(&err) == Some("course_membership_course_user") =>
        {
            Ok(Verdict::Held)
        }
        other => other,
    };
    match verdict {
        Ok(Verdict::Made(row)) => Ok(row),
        Ok(Verdict::Held) => match read_for_user(db, course, user).await? {
            Some(existing) => Ok(existing),
            None => Err(AppError::Internal("failed to add course membership".into())),
        },
        Ok(Verdict::Unfit) => Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can join a course",
        })),
        Err(err) => Err(err),
    }
}

/// Some(_) iff `user` holds a membership in `course`.
pub async fn read_for_user(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Option<CourseMembership>, AppError> {
    let row = sqlx::query_as!(
        CourseMembership,
        r#"SELECT course AS "course: CourseId", app_user AS "user: UserId",
               added_by AS "added_by: UserId", created_at AS "created_at: Timestamp"
           FROM course_membership WHERE course = $1 AND app_user = $2"#,
        course.uuid(),
        user.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// One course's members, newest first.
pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseMembership>, i64), AppError> {
    PagedList::new(
        "course_membership WHERE course = $1",
        "ORDER BY course DESC, app_user DESC",
    )
    .bind(course.uuid())
    .run::<CourseMembership>(limit, offset, db)
    .await
}

/// Every membership `user` holds, newest first — the "my clubs and etüts"
/// read.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseMembership>, i64), AppError> {
    PagedList::new(
        "course_membership WHERE app_user = $1",
        "ORDER BY course DESC, app_user DESC",
    )
    .bind(user.uuid())
    .run::<CourseMembership>(limit, offset, db)
    .await
}

/// Take the row and give the course's membership counter back in one
/// statement. `None` when the pair held no row.
pub async fn remove(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Option<CourseMembership>, AppError> {
    let gone = sqlx::query_as!(
        CourseMembership,
        r#"WITH gone AS (
             DELETE FROM course_membership
              WHERE course = $1 AND app_user = $2
              RETURNING course, app_user, added_by, created_at),
           counted AS (
             UPDATE course
                SET course_membership_count = GREATEST(
                    course_membership_count - (SELECT count(*) FROM gone), 0)
              WHERE id = $1
              RETURNING 1)
           SELECT course AS "course: CourseId", app_user AS "user: UserId",
               added_by AS "added_by: UserId", created_at AS "created_at: Timestamp"
           FROM gone"#,
        course.uuid(),
        user.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(gone)
}
