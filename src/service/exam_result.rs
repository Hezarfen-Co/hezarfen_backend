//! Exam-mark workflows: the grading gate chain, ungrading behind the
//! archived-year gate, and the read doors the web layer takes. The mark's
//! own claim-riding transaction lives in [`crate::db::exam_result`].

use crate::database::Database;
use crate::db;
use crate::domain::class_course::ClassCourseId;
use crate::domain::exam::{Exam, ExamId};
use crate::domain::exam_result::{ExamResult, Mark};
use crate::domain::role::Role;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::service::exam_attempt::{read_latest_for_user, require_open};

/// Grade a student's current sitting: every wall the handler used to run
/// (draft, retired kind, self-grade, target existence/role/enrollment), then
/// the mark's own transaction.
///
/// The draft gate below is the pre-flight; the mark's own transaction
/// re-makes it on the locked exam row ([`db::exam_result::grade`]), so a
/// re-draft landing after this read cannot leave a mark on a hidden exam —
/// the store decides, which is what the old reader lease of `EXAM_LOCK`
/// used to stand in for.
///
/// The kind gate is the settings-list twin of the counter's retired bit:
/// `kind_ref`'s bit is what actually refuses the mark inside
/// [`db::exam_result::grade`], and it is a *different record* from the
/// list — a settings PATCH moves both, so anything that leaves them
/// disagreeing (a rolled-back retirement, a hand edit) would otherwise
/// reopen grading under a kind nobody lists, which is also a mark that
/// averages at weight 1 forever. Both gates, same answer; this one is a
/// read, the counter's is the one that survives a race.
///
/// `mark` arrives raw because its range check is deliberately sequenced
/// *after* the draft and kind gates — validation order is observable.
///
/// Both sides of the grade moved a counter — the grader's `marks_given`,
/// the student's `high_mark` — so both are brought up to date. A badge is a
/// decoration on top of the mark: losing one to a transient database error
/// must never fail the grading, and the next counter move heals it.
pub async fn grade(
    db: &Database,
    exam_id: &ExamId,
    grader: &UserId,
    target: &UserId,
    mark: i64,
) -> Result<ExamResult, AppError> {
    // Exam must exist.
    let exam = crate::service::exam::read(db, exam_id)
        .await?
        .ok_or(AppError::NotFound)?;
    require_open(db, &exam).await?;
    // Pre-flight: `db::exam_result::grade` re-makes this check inside the
    // mark's own transaction, so a re-draft landing after this read cannot
    // leave a mark on a hidden exam.
    if exam.is_draft() {
        return Err(crate::domain::exam_result::draft_error());
    }
    let kind = exam.get_kind().as_str();
    if !crate::service::settings::load(db)
        .await?
        .get_exam_kinds()
        .iter()
        .any(|offered| offered.get_name() == kind)
    {
        return Err(crate::domain::exam_result::retired_kind_error(kind));
    }

    let mark = Mark::try_new(mark)?;

    // Grading never targets oneself — no grader, whatever their role, may
    // write their own mark.
    if target == grader {
        return Err(AppError::Forbidden("grading yourself is not allowed"));
    }

    // Target user must exist.
    let Some(target_user) = crate::service::user::read(db, target).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };

    // Only students carry marks — the grade system is theirs alone. A stale
    // enrollment left behind by a promotion can't reopen grading for staff.
    if target_user.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be graded",
        }));
    }

    // ... and be enrolled in an instance the exam is addressed to — the
    // owner's or an announced sibling's (a shared exam, D2): an addressed
    // section's student is graded exactly like the owner's.
    if !crate::service::exam::enrolled_anywhere(db, &exam, target).await? {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user is not enrolled in this course",
        }));
    }

    // The mark lands on the student's current sitting; the latest seq is the
    // grade-of-record. An offline-graded exam has no sitting — grade its base
    // seq (1).
    let seq = read_latest_for_user(db, exam_id, target)
        .await?
        .map_or(1, |a| a.get_seq());
    let result = db::exam_result::grade(db, exam_id, target, seq, mark, grader, kind).await?;
    for person in [grader, target] {
        if let Err(err) = crate::db::badge::sync(db, person).await {
            tracing::warn!("failed to sync badges for {}: {err}", person.key());
        }
    }
    Ok(result)
}

/// Remove a student's results from an exam — every sitting's marks at once,
/// behind the archived-term gate. `NotFound` mapping stays with the caller.
pub async fn remove(
    db: &Database,
    exam: &Exam,
    target: &UserId,
) -> Result<Option<ExamResult>, AppError> {
    require_open(db, exam).await?;
    db::exam_result::remove(db, exam.get_id(), target, exam.get_kind().as_str()).await
}

/// A single user's result for an exam, if graded.
pub async fn read_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Option<ExamResult>, AppError> {
    db::exam_result::read_for_user(db, exam, user).await
}

/// An exam's graded results, deduped to the latest mark per (exam, user).
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<ExamResult>, AppError> {
    db::exam_result::list_for_exam(db, exam).await
}

/// Every sitting's mark for one (exam, user) pair, oldest first.
pub async fn list_all_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<ExamResult>, AppError> {
    db::exam_result::list_all_for_exam_user(db, exam, user).await
}

/// The user's graded results restricted to one instance's exams — the raw
/// rows behind the per-instance block of the marks report.
pub async fn list_for_user_in_course(
    db: &Database,
    class_course: &ClassCourseId,
    user: &UserId,
) -> Result<Vec<ExamResult>, AppError> {
    db::exam_result::list_for_user_in_course(db, class_course, user).await
}
