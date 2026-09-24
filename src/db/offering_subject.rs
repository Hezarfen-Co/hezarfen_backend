//! The syllabus selection junctions: `offering_subject` (what the grade-level
//! template teaches) and `class_course_subject` (what one section teaches for
//! itself), over the subject rows of [`crate::db::subject`]. The row types are
//! the subjects themselves — a selection is only membership — so every read
//! here answers `Vec<Subject>` in the resolved listing order: **by subject
//! name, then subject id** (deterministic under any insert order).
//!
//! The class-side writes are the override machinery: each is **one
//! data-modifying-CTE statement** that flips `class_course.subjects_inherited`
//! in the same breath it inserts, deletes or sweeps the section's rows — the
//! flag can never drift from the table it governs, and this module never
//! touches `class_course` outside that flag. The reset is the same shape in
//! reverse: sweep the rows, flip the flag back to TRUE, one statement.

use crate::database::Database;
use crate::domain::class_course::ClassCourseId;
use crate::domain::course::CourseId;
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::subject::{Subject, SubjectDescription, SubjectId, SubjectName};
use crate::error::AppError;

/// Map the junctions' foreign-key refusals to the caller's 404: an offering,
/// a section or a subject deleted between the service's up-front read and
/// this write cannot half-land (the same mapping
/// [`crate::db::subject::create`] uses for its course key).
fn gone(err: sqlx::Error) -> AppError {
    if crate::database::foreign_key_violation(&err) {
        AppError::NotFound
    } else {
        err.into()
    }
}

/// The offering's selected topics, name-then-id order.
pub async fn list_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
) -> Result<Vec<Subject>, AppError> {
    let rows = sqlx::query_as!(
        Subject,
        r#"SELECT s.id AS "id: SubjectId",
                  s.course AS "course: CourseId",
                  s.name AS "name: SubjectName",
                  s.description AS "description: SubjectDescription"
           FROM offering_subject os
           JOIN subject s ON s.id = os.subject
           WHERE os.offering = $1
           ORDER BY s.name ASC, s.id ASC"#,
        offering.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The section's own selected topics — authoritative only while
/// `subjects_inherited` is FALSE, and empty-list-is-a-set then — name-then-id
/// order.
pub async fn list_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
) -> Result<Vec<Subject>, AppError> {
    let rows = sqlx::query_as!(
        Subject,
        r#"SELECT s.id AS "id: SubjectId",
                  s.course AS "course: CourseId",
                  s.name AS "name: SubjectName",
                  s.description AS "description: SubjectDescription"
           FROM class_course_subject ccs
           JOIN subject s ON s.id = ccs.subject
           WHERE ccs.class_course = $1
           ORDER BY s.name ASC, s.id ASC"#,
        class_course.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Add one topic to the offering's selection. Idempotent: re-adding a
/// selection the offering already holds writes nothing and answers `false`
/// (`true` = newly added), matching the junction idiom.
pub async fn add_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
    subject: &SubjectId,
) -> Result<bool, AppError> {
    let added = sqlx::query!(
        r#"INSERT INTO offering_subject (offering, subject)
           VALUES ($1, $2)
           ON CONFLICT DO NOTHING"#,
        offering.uuid(),
        subject.uuid(),
    )
    .execute(db)
    .await
    .map_err(gone)?;
    Ok(added.rows_affected() > 0)
}

/// Drop one topic from the offering's selection. `false` when it was not
/// selected — an honest nothing-changed, which the service answers as a 404.
pub async fn remove_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
    subject: &SubjectId,
) -> Result<bool, AppError> {
    let dropped = sqlx::query!(
        r#"DELETE FROM offering_subject
           WHERE offering = $1 AND subject = $2"#,
        offering.uuid(),
        subject.uuid(),
    )
    .execute(db)
    .await?;
    Ok(dropped.rows_affected() > 0)
}

/// Add one topic to the section's own set, flipping `subjects_inherited` to
/// FALSE **in the same statement** — the insert and the override switch are
/// one data-modifying-CTE unit, so a reader can never see a class-owned row
/// under a still-inheriting flag. Idempotent like the offering side: the
/// second add of one topic writes no row (`added` = false) but the intent —
/// "this section owns its set" — still stands, so the flip is unconditional
/// on a live instance. `live` = false means the instance vanished mid-flight;
/// the service answers that 404.
pub async fn add_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    subject: &SubjectId,
) -> Result<(bool, bool), AppError> {
    let row = sqlx::query!(
        r#"WITH flipped AS (
               UPDATE class_course
               SET subjects_inherited = FALSE
               WHERE id = $1
               RETURNING id
           ), inserted AS (
               INSERT INTO class_course_subject (class_course, subject)
               SELECT $1, $2
               WHERE EXISTS (SELECT 1 FROM flipped)
               ON CONFLICT DO NOTHING
               RETURNING subject
           )
           SELECT EXISTS (SELECT 1 FROM flipped) AS "live!",
                  EXISTS (SELECT 1 FROM inserted) AS "added!""#,
        class_course.uuid(),
        subject.uuid(),
    )
    .fetch_one(db)
    .await
    .map_err(gone)?;
    Ok((row.live, row.added))
}

/// Drop one topic from the section's own set, flipping `subjects_inherited`
/// to FALSE in the same statement — but **only when a row actually went**:
/// a delete that finds nothing flips nothing, so the service can answer 404
/// for a missing selection without a stray side effect on the flag. (`live`
/// = false: the instance is gone mid-flight.)
pub async fn remove_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    subject: &SubjectId,
) -> Result<(bool, bool), AppError> {
    let row = sqlx::query!(
        r#"WITH dropped AS (
               DELETE FROM class_course_subject
               WHERE class_course = $1 AND subject = $2
               RETURNING subject
           ), flipped AS (
               UPDATE class_course
               SET subjects_inherited = FALSE
               WHERE id = $1 AND EXISTS (SELECT 1 FROM dropped)
               RETURNING id
           )
           SELECT EXISTS (SELECT 1 FROM flipped) AS "live!",
                  EXISTS (SELECT 1 FROM dropped) AS "dropped!""#,
        class_course.uuid(),
        subject.uuid(),
    )
    .fetch_one(db)
    .await?;
    Ok((row.live, row.dropped))
}

/// The reset door: sweep every class-owned row and flip
/// `subjects_inherited` back to TRUE — one statement, so the set and its
/// switch agree at every instant. The flip is unconditional on a live
/// instance: clearing an already-empty own set is exactly the request that
/// must restore inheritance. Answers the swept row count; `live` = false
/// means the instance vanished mid-flight.
pub async fn reset_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
) -> Result<(i64, bool), AppError> {
    let row = sqlx::query!(
        r#"WITH swept AS (
               DELETE FROM class_course_subject
               WHERE class_course = $1
               RETURNING subject
           ), restored AS (
               UPDATE class_course
               SET subjects_inherited = TRUE
               WHERE id = $1
               RETURNING id
           )
           SELECT (SELECT count(*) FROM swept) AS "swept!: i64",
                  EXISTS (SELECT 1 FROM restored) AS "live!""#,
        class_course.uuid(),
    )
    .fetch_one(db)
    .await?;
    Ok((row.swept, row.live))
}

/// The resolved-membership probe behind the exam-question/homework tag rewire:
/// does `subject` sit in what this section *teaches* — its own rows while
/// `subjects_inherited` is FALSE, else the offering's set? One statement, so
/// the flag and the rows it reads agree as of one snapshot. `None` = the
/// instance is gone.
pub async fn resolves_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    subject: &SubjectId,
) -> Result<Option<bool>, AppError> {
    let row = sqlx::query!(
        r#"SELECT CASE WHEN cc.subjects_inherited
                   THEN EXISTS (SELECT 1
                                FROM offering_subject os
                                WHERE os.offering = cc.offering AND os.subject = $2)
                   ELSE EXISTS (SELECT 1
                                FROM class_course_subject ccs
                                WHERE ccs.class_course = cc.id AND ccs.subject = $2)
               END AS "member!"
           FROM class_course cc
           WHERE cc.id = $1"#,
        class_course.uuid(),
        subject.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| row.member))
}
