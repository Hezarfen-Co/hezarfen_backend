//! Homework workflows: the cascading delete and the shared student-side wall
//! ([`gate_own_submission`]) every submission path walks. Row reads and
//! listings pass through to [`crate::db::homework`]; the submission, file,
//! and grade workflows live in the sibling `service::homework_*` modules.
//!
//! There is no homework subsystem lock any more: the writes that used to
//! serialize on one — the PATCH's orphan guard, the delete cascade, grading,
//! and every student-side write — now contend on the *homework row itself*
//! (`SELECT … FOR UPDATE` inside each transaction), so the ordering they
//! need survives a multi-process deployment. See
//! [`crate::db::homework::update`] and
//! [`crate::db::homework_result::grade`].

use crate::database::Database;
use crate::db::homework;
use crate::domain::class_course::ClassCourseId;
use crate::domain::homework::{Homework, HomeworkDescription, HomeworkId, HomeworkTitle};
use crate::domain::role::Role;
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::AppError;

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the sibling entities' create(field, field, ..) shape"
)]
pub async fn create(
    db: &Database,
    class_course: &ClassCourseId,
    subject: &SubjectId,
    title: HomeworkTitle,
    description: Option<HomeworkDescription>,
    due_at: Timestamp,
    assigned: Option<Vec<UserId>>,
    created_by: &UserId,
) -> Result<Homework, AppError> {
    homework::create(
        db,
        class_course,
        subject,
        title,
        description,
        due_at,
        assigned,
        created_by,
    )
    .await
}

/// The homework row, for callers that only inspect it — the web layer's gates
/// read through here.
pub async fn read(db: &Database, id: &HomeworkId) -> Result<Option<Homework>, AppError> {
    homework::read(db, id).await
}

pub async fn list_all(db: &Database) -> Result<Vec<Homework>, AppError> {
    homework::list_all(db).await
}

/// The homework of one class×course instance, newest first — the read behind
/// `GET /instances/{id}/homework`.
pub async fn list_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
) -> Result<Vec<Homework>, AppError> {
    homework::list_for_class_course(db, class_course).await
}

/// Every homework of every instance in `instances` (one query) — the read
/// behind a caller's visible courses.
pub async fn list_for_class_courses(
    db: &Database,
    instances: &[ClassCourseId],
) -> Result<Vec<Homework>, AppError> {
    homework::list_for_class_courses(db, instances).await
}

/// The homework of one instance that `user` is meant to see — the per-instance
/// block of a homework report.
pub async fn list_for_user_in_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    user: &UserId,
) -> Result<Vec<Homework>, AppError> {
    homework::list_for_user_in_course(db, class_course, user).await
}

/// Re-scope (or re-tag, re-title, re-describe, re-schedule) the homework.
///
/// The audience-narrowing orphan guard and the stale-re-tag refusal are one
/// transaction with the write in [`homework::update`], under the homework
/// row's lock — a submission (which locks the same row before writing)
/// cannot land between the check and the narrowing it would have refused.
pub async fn update(
    db: &Database,
    homework: Homework,
    subject: Option<SubjectId>,
    title: Option<HomeworkTitle>,
    description: Option<Option<HomeworkDescription>>,
    due_at: Option<Timestamp>,
    assigned: Option<Option<Vec<UserId>>>,
) -> Result<Homework, AppError> {
    homework::update(db, homework, subject, title, description, due_at, assigned).await
}

/// Delete the homework: run the cascading delete (submissions, their files,
/// results, then the row — one transaction, children first) and return the
/// submission-file blob keys, which the transaction collected before the
/// wipes so no file added mid-delete can strand its blob. The web layer
/// unlinks the blobs once the rows are gone; a crash in between strands at
/// worst an unreachable file.
pub async fn delete(db: &Database, homework: Homework) -> Result<Vec<String>, AppError> {
    let (_, blob_keys) = homework::delete(db, homework).await?;
    Ok(blob_keys)
}

/// A 403 unless `user` is enrolled in the instance the homework belongs to —
/// the homework twin of the exam sitting wall
/// ([`crate::service::exam_attempt::ensure_enrolled`], which is `Exam`-shaped).
/// Submitting is course content, so leaving the instance closes it; re-checked
/// on every submission and file write, so an unenrollment mid-task bites the
/// next.
async fn ensure_enrolled(
    class_course: &ClassCourseId,
    user: &UserId,
    db: &Database,
) -> Result<(), AppError> {
    if crate::service::enrollment::read_for_user(db, class_course, user)
        .await?
        .is_none()
    {
        return Err(AppError::Forbidden(
            "you are not enrolled in this homework's course",
        ));
    }
    Ok(())
}

/// Read a homework and clear `user` to act on their own submission to it — the
/// shared wall of the submission, file, and (student side of the) download
/// paths. Three gates, in order:
///
/// 1. Exact `Student` on the *live* role. Teachers assign homework, they never
///    hand it in; the `RequireStudent` extractor's ≥Student would wave a
///    promoted teacher through, so this checks the role `CurrentUser` read for
///    this very request — the same reasoning as [`crate::service::exam_attempt::ensure_student`].
/// 2. Current enrollment in the course.
/// 3. The audience check: a student a subset homework does not name gets a 404,
///    never a 403, so a subset assignment never leaks to those left out (the
///    no-leak idiom an unseen exam draft uses).
pub async fn gate_own_submission(
    id: &str,
    user: &User,
    db: &Database,
) -> Result<Homework, AppError> {
    let homework = homework::read(db, &HomeworkId::from_key(id))
        .await?
        .ok_or(AppError::NotFound)?;
    if user.get_role() != Role::Student {
        return Err(AppError::Forbidden(
            "only students have homework submissions",
        ));
    }
    ensure_enrolled(homework.get_class_course(), user.get_id(), db).await?;
    if !homework.student_sees(user.get_id()) {
        return Err(AppError::NotFound);
    }
    Ok(homework)
}

/// The year wall of the student side: an archived year makes past structure
/// read-only. Deliberately *not* inside [`gate_own_submission`] — that gate
/// also fronts the download read, and an archived year is still browsable. So
/// every student *write* calls this right after the gate, which keeps the
/// order that matters: a student the homework never named is refused by the
/// audience check with a 404 and never learns the homework exists.
///
/// The gate keys on the homework's instance → its class section → that
/// section's year (D8): a term archived inside an open year does not close
/// homework, the year does.
pub async fn require_open_instance(homework: &Homework, db: &Database) -> Result<(), AppError> {
    crate::service::class_course::require_open(db, homework.get_class_course()).await
}

/// Bring one user's badge awards up to date after a write moved their counters
/// — the student's after a submission, the grader's after a grade. Never fails
/// the request it follows: a badge is a decoration on top
/// of the work, and losing one to a transient database error is not worth
/// refusing a hand-in over — the next counter move re-runs this and heals it.
pub(crate) async fn award_badges(user: &UserId, db: &Database) {
    if let Err(err) = crate::db::badge::sync(db, user).await {
        tracing::warn!("failed to sync badges for {}: {err}", user.key());
    }
}
