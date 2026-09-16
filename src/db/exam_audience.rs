//! The `exam_audience` junction: which instances one exam is announced to.
//!
//! Every exam is *owned* by exactly one instance — the `class_course` column
//! on its own row — and [`crate::db::exam::insert_in`] writes that owner's
//! audience row in the same transaction as the exam. The rows beyond it are
//! the **ortak sınav**'s (D2): one exam announced to another şube's instance,
//! sat and graded once, standing in every addressed instance's marks and
//! karne. Every exam read goes through this table
//! ([`crate::db::exam::list_for_class_course`] and the marks reports' joins),
//! so the audience set is the single answer to "which instances does this
//! exam belong to" and the owner column is only the join's tie-break.
//!
//! The owner's row is never removed from here: [`crate::service::exam`]
//! refuses that pair on both routes, and deleting the exam is what ends its
//! audience — [`crate::db::exam::delete`] and the instance sweep each clear
//! the rows in their own transaction. The audience a detach may *not* leave
//! behind is the one pointing at the detaching instance: an exam owned by
//! another instance would block the `class_course` delete with a `23503`, so
//! [`crate::db::course::sweep_instance_subtree`] clears both directions.

use crate::database::{Database, foreign_key_violation};
use crate::domain::class_course::ClassCourseId;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// One audience row, resolved to what a client needs to name it: the
/// instance, the şube it belongs to and the catalog course it teaches.
#[derive(Debug, sqlx::FromRow)]
pub struct Audience {
    class_course: ClassCourseId,
    class: ClassGroupId,
    course: CourseId,
}

impl Audience {
    /// The instance the exam is announced to.
    pub fn get_instance(&self) -> &ClassCourseId {
        &self.class_course
    }

    /// The şube that instance belongs to.
    pub fn get_class(&self) -> &ClassGroupId {
        &self.class
    }

    /// The catalog course that instance teaches — the exam's own, by the
    /// announcement's rule.
    pub fn get_course(&self) -> &CourseId {
        &self.course
    }
}

/// Announce `exam` to `class_course`. An audience the exam already carries is
/// success, not a refusal: the announcement is idempotent, exactly like a
/// teacher assignment ([`crate::db::class_course_teacher::assign`]), and the
/// route answers the resulting set either way.
///
/// Both foreign keys answer a gone parent with `NotFound`: an exam swept by a
/// concurrent detach and an instance that never existed are the same answer
/// to the caller, and neither may read as a `500`.
pub async fn add(
    db: &Database,
    exam: &ExamId,
    class_course: &ClassCourseId,
) -> Result<(), AppError> {
    match sqlx::query!(
        r#"INSERT INTO exam_audience (exam, class_course) VALUES ($1, $2)
           ON CONFLICT ON CONSTRAINT exam_audience_pk DO NOTHING"#,
        exam.uuid(),
        class_course.uuid(),
    )
    .execute(db)
    .await
    {
        Ok(_) => Ok(()),
        Err(e) if foreign_key_violation(&e) => Err(AppError::NotFound),
        Err(e) => Err(e.into()),
    }
}

/// Drop an audience row. `false` when the pair held none, so the web layer
/// can answer 404 instead of pretending it removed one — the shape
/// [`crate::db::class_course_teacher::unassign`] has.
pub async fn remove(
    db: &Database,
    exam: &ExamId,
    class_course: &ClassCourseId,
) -> Result<bool, AppError> {
    let deleted = sqlx::query!(
        r#"DELETE FROM exam_audience WHERE exam = $1 AND class_course = $2"#,
        exam.uuid(),
        class_course.uuid(),
    )
    .execute(db)
    .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Every instance `exam` is announced to, in the order they were announced —
/// uuidv7 is creation order — the read behind `GET /exams/{id}/audience`.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<Audience>, AppError> {
    let rows = sqlx::query_as!(
        Audience,
        r#"SELECT a.class_course AS "class_course: ClassCourseId",
                  c.class AS "class: ClassGroupId",
                  c.course AS "course: CourseId"
           FROM exam_audience a
           JOIN class_course c ON c.id = a.class_course
           WHERE a.exam = $1
           ORDER BY a.class_course"#,
        exam.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The exams addressed to one instance — the reverse read, for a caller that
/// starts from the instance rather than the exam (`exam_audience`'s
/// `class_course` index is the side it walks).
pub async fn list_for_instance(
    db: &Database,
    class_course: &ClassCourseId,
) -> Result<Vec<ExamId>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT exam AS "exam: ExamId" FROM exam_audience
           WHERE class_course = $1 ORDER BY exam"#,
        class_course.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.exam).collect())
}

/// Whether `user` is enrolled in any instance `exam` is addressed to — the
/// owner included, its audience row always standing.
///
/// The enrollment predicate the sitting and grading gates ask once one exam
/// can be announced to several instances (D2): for an exam nobody was
/// announced to this is the owner-only check those gates used to run, row for
/// row, and for an ortak sınav it admits the addressed sections' students —
/// who sit and are graded at their own instance exactly like the owner's.
pub async fn enrolled(db: &Database, exam: &ExamId, user: &UserId) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(
               SELECT 1 FROM exam_audience a
               JOIN enrollment e ON e.class_course = a.class_course
               WHERE a.exam = $1 AND e.app_user = $2) AS "enrolled!""#,
        exam.uuid(),
        user.uuid(),
    )
    .fetch_one(db)
    .await?)
}
