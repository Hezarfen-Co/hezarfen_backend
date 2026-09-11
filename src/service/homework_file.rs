//! Homework file workflows: the attachment of one file to the caller's
//! submission (the graded gate plus the auto-created seat it hangs off) and
//! the pass-through reads the download paths scope through. The blob bytes
//! and their ordering (blob before row on upload, row before blob on delete)
//! stay the web layer's — it owns `files_path` — but every lease holder is
//! [`HOMEWORK_LOCK`](crate::service::homework::HOMEWORK_LOCK)'s reader side.
//! The row writes live in [`crate::db::homework_file`].

use crate::database::Database;
use crate::db::homework_file;
use crate::db::homework_submission;
use crate::domain::homework::Homework;
use crate::domain::homework_file::{HomeworkFile, HomeworkFileId};
use crate::domain::homework_submission::HomeworkSubmission;
use crate::domain::homework_submission::HomeworkSubmissionId;
use crate::domain::user::User;
use crate::error::AppError;

/// The submission the file will hang off, created if absent — a photo-only
/// homework never types text, so an upload auto-creates an empty submission
/// to carry the file (an existing one's text is preserved). A photo-only
/// hand-in is a hand-in: it creates the row, so it moves the counters, so it
/// earns badges exactly as a text submit does. Refused (409) when a grade
/// has frozen the homework.
pub async fn ensure_can_attach(
    db: &Database,
    user: &User,
    homework: &Homework,
) -> Result<HomeworkSubmission, AppError> {
    const GRADED: AppError = AppError::Conflict(
        "this homework has been graded — ask the teacher to remove the grade before adding files",
    );
    // The common-case gate; the freeze itself rides on the writes below.
    if crate::db::homework_result::read_for(db, homework.get_id(), user.get_id())
        .await?
        .is_some()
    {
        return Err(GRADED);
    }
    match homework_submission::read_for(db, homework.get_id(), user.get_id()).await? {
        Some(existing) => Ok(existing),
        None => {
            let created = homework_submission::upsert(db, homework, user.get_id(), None)
                .await?
                .ok_or(GRADED)?;
            crate::service::homework::award_badges(user.get_id(), db).await;
            Ok(created)
        }
    }
}

/// Persist the assembled row — the seat claim and the freeze in one
/// conditional write (see [`homework_file::insert`]). `Ok(None)` is the
/// freeze; the web layer keeps its own wording and unlinks the blob it wrote.
pub async fn insert(db: &Database, file: HomeworkFile) -> Result<Option<HomeworkFile>, AppError> {
    homework_file::insert(db, file).await
}

/// Delete one file row and give its seat back in the same transaction.
/// `Ok(None)` is the freeze biting (the caller answers 409); `Err(NotFound)`
/// means the row had already vanished.
pub async fn delete(db: &Database, file: HomeworkFile) -> Result<Option<HomeworkFile>, AppError> {
    homework_file::delete(db, file).await
}

/// Read a file's row only if it belongs to `submission` — the student's
/// download path, behind the submission gate.
pub async fn read_for(
    db: &Database,
    id: &HomeworkFileId,
    submission: &HomeworkSubmissionId,
) -> Result<Option<HomeworkFile>, AppError> {
    homework_file::read_for(db, id, submission).await
}

/// Read a file by id only if it hangs off a submission to `homework` — the
/// grader's download scoping.
pub async fn read_in_homework(
    db: &Database,
    id: &HomeworkFileId,
    homework: &crate::domain::homework::HomeworkId,
) -> Result<Option<HomeworkFile>, AppError> {
    homework_file::read_in_homework(db, id, homework).await
}

/// All of `submission`'s files, newest first.
pub async fn list_for_submission(
    db: &Database,
    submission: &HomeworkSubmissionId,
) -> Result<Vec<HomeworkFile>, AppError> {
    homework_file::list_for_submission(db, submission).await
}
