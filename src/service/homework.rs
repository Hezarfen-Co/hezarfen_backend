//! Homework workflows: the PATCH re-scope with its orphan guard, the
//! cascading delete, and the shared student-side wall
//! ([`gate_own_submission`]) every submission path walks. Row reads and
//! listings pass through to [`crate::db::homework`]; the submission, file,
//! and grade workflows live in the sibling `service::homework_*` modules,
//! whose every locked path leases [`HOMEWORK_LOCK`].

use crate::database::Database;
use crate::db::homework;
use crate::db::homework_file;
use crate::db::homework_result;
use crate::db::homework_submission;
use crate::domain::course::CourseId;
use crate::domain::homework::{Homework, HomeworkDescription, HomeworkId, HomeworkTitle};
use crate::domain::homework_result::HomeworkResult;
use crate::domain::homework_submission::HomeworkSubmission;
use crate::domain::role::Role;
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::AppError;

/// Serializes the homework subsystem's cross-record check-then-writes, which
/// `BEGIN…COMMIT` cannot (write skew) — the same reasoning as
/// [`crate::service::exam_attempt::EXAM_LOCK`]. Every lease below is held across the
/// database write it guards, so within this one process it does order a full
/// round trip — but the freeze no longer *rests* on that: a graded submission
/// used to stay unedited only because the "no grade yet" read and the write it
/// licensed sat under one lease. That rule now lives in the database too —
/// grading stamps [`crate::constant::SUBMISSION_GRADED_FIELD`] on the
/// submission row and every student-side write carries `graded_by_result =
/// NONE` as its own condition. The one case the stamp cannot cover — grading
/// work with no submission row yet — falls back to the lease pair, so a write
/// lease must never stop spanning its own database call (see
/// [`crate::db::homework_result::grade`]).
///
/// What still leases it, honestly:
/// - Write: the homework PATCH's orphan guard ([`update`]), the
///   homework-delete cascade ([`delete`]), and grade/ungrade — whose
///   freeze rule (the stamp landing on a submission that may be written in the
///   same instant) is the one thing here still resting on the two leases being
///   mutually exclusive. The *existence* half has left: a grade now moves a
///   value on the homework row inside its own transaction
///   ([`crate::db::homework_result::grade`]), so a
///   concurrent delete refuses it rather than being read around.
/// - Read: the student's submission/file writes, which no longer gate the
///   freeze but still must not land under a PATCH re-scoping the audience out
///   from under them.
///
/// The subject rule has left: creating a homework and re-tagging one move the
/// subject's reference counter, and the subject delete is refused while that
/// counter is non-zero ([`crate::domain::subject::Subject::delete`]), so
/// neither the create (in `web::courses`) nor the outside writer the subject
/// delete used to take is on this list any more.
///
/// Lock order, where both are taken: `HOMEWORK_LOCK` before the counter lock in
/// [`crate::db::cap`], never the reverse.
// corner-cut: global RwLock, shard per-homework if write latency ever matters.
pub(crate) static HOMEWORK_LOCK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the sibling entities' create(field, field, ..) shape"
)]
pub async fn create(
    db: &Database,
    course: &CourseId,
    subject: &SubjectId,
    title: HomeworkTitle,
    description: Option<HomeworkDescription>,
    due_at: Timestamp,
    assigned: Option<Vec<UserId>>,
    created_by: &UserId,
) -> Result<Homework, AppError> {
    homework::create(
        db,
        course,
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

pub async fn list_for_course(db: &Database, course: &CourseId) -> Result<Vec<Homework>, AppError> {
    homework::list_for_course(db, course).await
}

/// Every homework of every course in `courses` (one query) — the catalog as
/// one user sees it.
pub async fn list_for_courses(
    db: &Database,
    courses: &[CourseId],
) -> Result<Vec<Homework>, AppError> {
    homework::list_for_courses(db, courses).await
}

/// The homework of `course` that `user` is meant to see — the per-course
/// block of a homework report.
pub async fn list_for_user_in_course(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Vec<Homework>, AppError> {
    homework::list_for_user_in_course(db, course, user).await
}

/// Re-scope (or re-tag, re-title, re-describe, re-schedule) the homework.
///
/// Writer lease of [`HOMEWORK_LOCK`]: `ensure_no_orphans` below reads the
/// live submissions and results, and the row write depends on what it saw —
/// without the lease a submission (a reader) could land between the check
/// and the write, orphaned by the narrowing that just missed it. The subject
/// re-tag no longer needs it — it moves the two subjects' reference counters
/// inside [`homework::update`].
///
/// The orphan guard runs on exactly the requests that re-scope the audience.
/// An absent `assigned` writes nothing, so the stored subset is untouched and
/// no narrowing can happen behind the guard's back — which the old "carry the
/// snapshot back" branch could do, re-narrowing over a concurrent widening.
pub async fn update(
    db: &Database,
    homework: Homework,
    subject: Option<SubjectId>,
    title: Option<HomeworkTitle>,
    description: Option<Option<HomeworkDescription>>,
    due_at: Option<Timestamp>,
    assigned: Option<Option<Vec<UserId>>>,
) -> Result<Homework, AppError> {
    let _guard = HOMEWORK_LOCK.write().await;
    if let Some(resolved) = assigned.as_ref() {
        ensure_no_orphans(&homework, resolved.as_deref(), db).await?;
    }
    homework::update(db, homework, subject, title, description, due_at, assigned).await
}

/// Refuse (409) a PATCH that would narrow `homework`'s audience so a student
/// who already submitted or was graded falls outside it — their work would be
/// stranded. `new_assigned` is the proposed subset (`None` = whole course, in
/// which case no one can be orphaned). The blocking students are named in the
/// message so the teacher knows whose work to clear (or whom to keep assigned)
/// first.
async fn ensure_no_orphans(
    homework: &Homework,
    new_assigned: Option<&[UserId]>,
    db: &Database,
) -> Result<(), AppError> {
    // Whole-course covers everyone — no narrowing, no orphans.
    let Some(subset) = new_assigned else {
        return Ok(());
    };
    let submissions = homework_submission::list_for_homework(db, homework.get_id()).await?;
    let results = homework_result::list_for_homework(db, homework.get_id()).await?;
    let mut blocked: Vec<String> = Vec::new();
    for user in submissions
        .iter()
        .map(HomeworkSubmission::get_user)
        .chain(results.iter().map(HomeworkResult::get_user))
    {
        let key = user.key().to_string();
        if !subset.contains(user) && !blocked.contains(&key) {
            blocked.push(key);
        }
    }
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(AppError::ConflictOwned(format!(
            "narrowing the assigned list would orphan existing work by {} student(s): {}",
            blocked.len(),
            blocked.join(", ")
        )))
    }
}

/// Delete the homework: collect the submission-file blob keys, then run the
/// cascading delete under [`HOMEWORK_LOCK`]'s write lease
/// so no submission can land under the homework mid-delete; the blob names are
/// collected before the rows are wiped (the cascade is one transaction, children
/// first) and removed after, so a crash in between strands at worst an
/// unreachable file.
pub async fn delete(db: &Database, homework: Homework) -> Result<Vec<String>, AppError> {
    let _guard = HOMEWORK_LOCK.write().await;
    let blob_keys = homework_file::file_keys_for_homework(db, homework.get_id()).await?;
    homework::delete(db, homework).await?;
    Ok(blob_keys)
}

/// A 403 unless `user` is enrolled in `course` — the homework twin of the exam
/// sitting wall ([`crate::service::exam_attempt::ensure_enrolled`], which is `Exam`-shaped).
/// Submitting is course content, so leaving the course closes it; re-checked on
/// every submission and file write, so an unenrollment mid-task bites the next.
async fn ensure_enrolled(course: &CourseId, user: &UserId, db: &Database) -> Result<(), AppError> {
    if crate::service::enrollment::read_for_user(db, course, user)
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
    ensure_enrolled(homework.get_course(), user.get_id(), db).await?;
    if !homework.student_sees(user.get_id()) {
        return Err(AppError::NotFound);
    }
    Ok(homework)
}

/// The term wall of the student side: an archived term makes past years
/// read-only. Deliberately *not* inside [`gate_own_submission`] — that gate
/// also fronts the download read, and an archived year is still browsable. So
/// every student *write* calls this right after the gate, which keeps the
/// order that matters: a student the homework never named is refused by the
/// audience check with a 404 and never learns the homework exists. A course
/// that vanished under us is the gates' own business, not this one's.
pub async fn require_open_term(homework: &Homework, db: &Database) -> Result<(), AppError> {
    if let Some(course) = crate::service::course::read(db, homework.get_course()).await? {
        crate::service::course::require_open(db, &course).await?;
    }
    Ok(())
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
