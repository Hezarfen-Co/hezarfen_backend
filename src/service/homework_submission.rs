//! Homework submission workflows: the student's submit (the graded gate and
//! the audience interlock around the guarded upsert) and the withdrawal that
//! gives the badge counters back. The row writes live in
//! [`crate::db::homework_submission`]; the freeze itself is a condition of
//! those writes, and the homework row's lock is what orders them against
//! grading — there is no subsystem lock here any more.

use crate::database::Database;
use crate::db::homework_file;
use crate::db::homework_result;
use crate::db::homework_submission;
use crate::domain::homework::Homework;
use crate::domain::homework_file::HomeworkFile;
use crate::domain::homework_submission::{HomeworkSubmission, SubmissionText};
use crate::domain::user::User;
use crate::error::AppError;

/// What [`submit`] landed: whether the (homework, user) row existed before
/// (the web layer's 200-vs-201), the homework it answered, and the row.
pub struct Submitted {
    pub existed: bool,
    pub homework: Homework,
    pub submission: HomeworkSubmission,
}

/// Submit (or re-submit) the caller's own work for a homework. The graded
/// read below answers the common case — graded minutes ago, and the student
/// who never submitted has no row to carry the freeze; the upsert's own
/// `graded_by_result IS NULL` condition is what holds when the grade lands
/// *while* this request runs (grade and write serialize on the homework
/// row's lock).
///
/// `None` from [`homework_submission::upsert`] is that freeze biting; both
/// refusals return the same 409.
pub async fn submit(
    db: &Database,
    user: &User,
    id: &str,
    text: Option<SubmissionText>,
) -> Result<Submitted, AppError> {
    let homework = crate::service::homework::gate_own_submission(id, user, db).await?;
    crate::service::homework::require_open_instance(&homework, db).await?;
    const GRADED: AppError = AppError::Conflict(
        "this homework has been graded — ask the teacher to remove the grade before editing your submission",
    );
    if homework_result::read_for(db, homework.get_id(), user.get_id())
        .await?
        .is_some()
    {
        return Err(GRADED);
    }
    // `existed` comes back from the transaction itself, so the 201-vs-200
    // answer and the badge rule are exact even against a rival first
    // hand-in.
    let Some((submission, existed)) =
        homework_submission::upsert(db, &homework, user.get_id(), text, false).await?
    else {
        return Err(GRADED);
    };
    // Only a first hand-in moved a counter, so only a first hand-in can have
    // earned anything — an edit re-runs nothing.
    if !existed {
        crate::service::homework::award_badges(user.get_id(), db).await;
    }
    Ok(Submitted {
        existed,
        homework,
        submission,
    })
}

/// Withdraw the caller's own submission — its text, its file rows, and their
/// blobs. Returns the file rows (the blobs are the web layer's to unlink:
/// collected before the wipe, unlinked after). Refused (409) once the work
/// is graded — the first read answers the common case, and the delete's own
/// freeze condition (the stamp on the row) holds when a grade lands since
/// the read above, refusing the write rather than wiping graded work.
pub async fn delete(db: &Database, user: &User, id: &str) -> Result<Vec<HomeworkFile>, AppError> {
    let homework = crate::service::homework::gate_own_submission(id, user, db).await?;
    crate::service::homework::require_open_instance(&homework, db).await?;
    const GRADED: AppError = AppError::Conflict(
        "this homework has been graded — ask the teacher to remove the grade before deleting your submission",
    );
    if homework_result::read_for(db, homework.get_id(), user.get_id())
        .await?
        .is_some()
    {
        return Err(GRADED);
    }
    let submission = homework_submission::read_for(db, homework.get_id(), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    // Collect blob names before the cascade (the delete wipes the file
    // rows in the same transaction), then unlink after the rows are gone.
    let files = homework_file::list_for_submission(db, submission.get_id()).await?;
    if homework_submission::delete(db, submission).await?.is_none() {
        return Err(GRADED);
    }
    // The counters just came down; a badge already earned stays earned (`sync`
    // only ever adds), so this is here to keep the award rows in step with the
    // *next* submission rather than to take anything back.
    crate::service::homework::award_badges(user.get_id(), db).await;
    Ok(files)
}

/// `user`'s submission to `homework`, if they have one — the web layer's
/// read paths go through here.
pub async fn read_for(
    db: &Database,
    homework: &crate::domain::homework::HomeworkId,
    user: &crate::domain::user::UserId,
) -> Result<Option<HomeworkSubmission>, AppError> {
    homework_submission::read_for(db, homework, user).await
}

/// Re-stamp a submission's `updated_at` to now — a file add/delete moves the
/// submission's "last touched" clock (the late flag) without touching the
/// text.
pub async fn touch(
    db: &Database,
    id: &crate::domain::homework_submission::HomeworkSubmissionId,
) -> Result<HomeworkSubmission, AppError> {
    homework_submission::touch(db, id).await
}

/// Every submission to `homework` — the roster's raw rows.
pub async fn list_for_homework(
    db: &Database,
    homework: &crate::domain::homework::HomeworkId,
) -> Result<Vec<HomeworkSubmission>, AppError> {
    homework_submission::list_for_homework(db, homework).await
}
