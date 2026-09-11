//! Exam workflows: the PATCH re-derive — the merge (set / clear / keep per
//! field) re-judged against a fresh read every retry round, and the
//! mode-freeze, re-draft, and kind gates that guard it — and the delete that
//! collects the image blob keys under the writer lease before the cascade.
//! The queries live in [`crate::db::exam`]; the sitting workflows next door
//! in [`crate::service::exam_attempt`], whose [`EXAM_LOCK`] every path here
//! leases.

use crate::constant::CAS_UPDATE_RETRIES;
use crate::database::Database;
use crate::db::exam;
use crate::domain::answer_image::AnswerImage;
use crate::domain::course::CourseId;
use crate::domain::exam::{
    Exam, ExamAttemptLimit, ExamDescription, ExamDuration, ExamId, ExamKind, ExamMode,
    ExamSchedule, ExamTitle, redraft_error,
};
use crate::domain::exam_result::ExamResult;
use crate::domain::question_image::QuestionImage;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use crate::service::course::require_open;
use crate::service::exam_attempt::{EXAM_LOCK, any_for_exam, course_of};

pub async fn create(
    db: &Database,
    creator: &UserId,
    course: &CourseId,
    title: ExamTitle,
    description: ExamDescription,
    kind: ExamKind,
    schedule: ExamSchedule,
    max_attempts: ExamAttemptLimit,
    allow_rejoin: bool,
    allow_review: bool,
    draft: bool,
) -> Result<Exam, AppError> {
    exam::create(
        db,
        creator,
        course,
        title,
        description,
        kind,
        schedule,
        max_attempts,
        allow_rejoin,
        allow_review,
        draft,
    )
    .await
}

/// The exam row, for callers that only inspect it — the web layer's gates
/// read through here.
pub async fn read(db: &Database, id: &ExamId) -> Result<Option<Exam>, AppError> {
    exam::read(db, id).await
}

pub async fn list_all(db: &Database) -> Result<Vec<Exam>, AppError> {
    exam::list_all(db).await
}

pub async fn list_for_course(db: &Database, course: &CourseId) -> Result<Vec<Exam>, AppError> {
    exam::list_for_course(db, course).await
}

/// Every exam of every course in `courses` (one query) — the catalog as one
/// user sees it.
pub async fn list_for_courses(db: &Database, courses: &[CourseId]) -> Result<Vec<Exam>, AppError> {
    exam::list_for_courses(db, courses).await
}

/// A `PATCH /exams/{id}` request after field validation: every value the
/// request *set* is already validated (title, description, mode, duration,
/// and attempt limit by their newtypes; the kind against the school's
/// current list; newly set window ends by the web layer's no-past grace).
/// What is *kept* — and whether the merged whole is a legal schedule — are
/// the update's own decisions, re-derived against a fresh read every retry
/// round, so they live here and not in the web layer. A `None` field keeps
/// the stored value; the `Option<Option<..>>` schedule fields carry the
/// web DTO's set-or-clear spelling (`Some(None)` = explicit `null`).
pub struct ExamPatch {
    pub title: Option<ExamTitle>,
    pub description: Option<ExamDescription>,
    pub kind: Option<ExamKind>,
    pub mode: Option<Option<ExamMode>>,
    pub starts_at: Option<Option<Timestamp>>,
    pub ends_at: Option<Option<Timestamp>>,
    pub duration_ms: Option<Option<ExamDuration>>,
    pub max_attempts: Option<ExamAttemptLimit>,
    pub allow_rejoin: Option<bool>,
    pub allow_review: Option<bool>,
    pub draft: Option<bool>,
}

/// Re-derive and save the exam: merge the patch over a fresh read, re-validate
/// the schedule as a unit, judge every business gate against that same row,
/// and land the whole merge with the compare-and-set — retried while the row
/// keeps moving under the snapshot.
///
/// *Reader* lease of [`EXAM_LOCK`] for exactly one pairing: the mode gate
/// below reads `exam_attempt`, and an attempt start takes the *writer*
/// lease, so a first sitting still cannot land between that gate and the
/// write, as it always did.
///
/// It buys nothing against grading, which is a reader too: the re-draft gate
/// is therefore enforced inside the update's own transaction
/// ([`exam::update_if_unchanged`]), where the store decides it. The gate
/// below stays as the pre-flight — same error, one round trip earlier.
/// Concurrent PATCHes of this exam no longer queue behind each other either:
/// the lost update they used to cause is refused by the compare-and-set.
pub async fn update(db: &Database, id: &ExamId, patch: &ExamPatch) -> Result<Exam, AppError> {
    let _guard = EXAM_LOCK.read().await;
    let mut left = CAS_UPDATE_RETRIES;
    loop {
        let current = exam::read(db, id).await?.ok_or(AppError::NotFound)?;
        let course = course_of(&current, db).await?;
        require_open(db, &course).await?;

        let title = patch
            .title
            .clone()
            .unwrap_or_else(|| current.get_title().clone());
        let description = patch
            .description
            .clone()
            .unwrap_or_else(|| current.get_description().clone());
        let kind = patch
            .kind
            .clone()
            .unwrap_or_else(|| current.get_kind().clone());

        // Merge the schedule (set / clear / keep per field), then re-validate it
        // as a unit — a PATCH can't leave a half-schedule behind. Only values this
        // request sets are held to the no-past rule: kept ones may legitimately be
        // past (a running exam's `starts_at`), and rechecking them would block
        // unrelated edits.
        let mode = match &patch.mode {
            Some(update) => update.clone(),
            None => current.get_mode().cloned(),
        };
        let starts_at = match patch.starts_at {
            Some(update) => update,
            None => current.get_starts_at(),
        };
        let ends_at = match patch.ends_at {
            Some(update) => update,
            None => current.get_ends_at(),
        };
        let duration_ms = match patch.duration_ms {
            Some(update) => update,
            None => current.get_duration_ms(),
        };
        let schedule = ExamSchedule::try_new(mode, starts_at, ends_at, duration_ms)?;
        let max_attempts = patch
            .max_attempts
            .unwrap_or_else(|| current.get_max_attempts());
        let allow_rejoin = patch
            .allow_rejoin
            .unwrap_or_else(|| current.get_allow_rejoin());
        let allow_review = patch
            .allow_review
            .unwrap_or_else(|| current.get_allow_review());
        let draft = patch.draft.unwrap_or(current.is_draft());

        // Switching sync <-> async <-> open (or back to unscheduled) would
        // silently rewrite the deadline rules under students who already sat
        // down; extending times, the attempt limit, and the rejoin door are the
        // supported live adjustments instead. Gate read and write share the
        // call-wide reader lease of [`EXAM_LOCK`], so a first attempt can't
        // land in the gap and leave a sat exam's mode flipped under it.
        let mode_changed =
            schedule.get_mode().map(ExamMode::as_str) != current.get_mode().map(ExamMode::as_str);
        if mode_changed && any_for_exam(db, current.get_id()).await? {
            return Err(AppError::Conflict(
                "cannot change the exam mode after attempts have started",
            ));
        }
        // Re-drafting hides the exam — never out from under a student who
        // already sat it or holds a mark on it. Pre-flight only: the write's own
        // transaction re-makes this check and answers with the same error, so a
        // grade landing after this read still cannot leave a mark on a draft.
        if draft && !current.is_draft() {
            let sat = any_for_exam(db, current.get_id()).await?;
            let graded = !ExamResult::list_for_exam(current.get_id(), db)
                .await?
                .is_empty();
            if sat || graded {
                return Err(redraft_error());
            }
        }

        // A graded exam keeps its kind. Moving it re-weights every mark it
        // already carries — the same silent re-weighting the settings' removal
        // guard refuses — and it would strand those marks' references on the
        // kind they were counted under, freeing the kind the exam now claims to
        // be. Marks are counted on the exam row, and the save below *pins* that
        // counter, so a grade landing between this read and the write refuses
        // the save (the loop then re-reads and answers the 409 below).
        if kind.as_str() != current.get_kind().as_str() && current.get_result_count() > 0 {
            return Err(AppError::Conflict(
                "cannot change the kind of an exam that already has marks",
            ));
        }

        if let Some(updated) = exam::update_if_unchanged(
            db,
            current,
            title,
            description,
            kind,
            schedule,
            max_attempts,
            allow_rejoin,
            allow_review,
            draft,
        )
        .await?
        {
            return Ok(updated);
        }
        // The row moved under the snapshot every gate above judged: re-read and
        // re-merge, so both edits land instead of the later reverting the earlier.
        left -= 1;
        if left == 0 {
            return Err(AppError::Conflict(
                "the exam kept changing underneath this update — try again",
            ));
        }
    }
}

/// What [`delete`] did: the blob keys of the image rows the cascade removed —
/// exactly whose files the web layer may unlink.
#[derive(Debug)]
pub struct DeleteOutcome {
    pub image_files: Vec<String>,
    pub answer_image_files: Vec<String>,
}

/// Delete the exam: collect the question/answer image blob keys, then run the
/// cascading delete.
///
/// Writer lease of [`EXAM_LOCK`] across the whole cascade, blob names
/// included — the lease `delete_homework` has always held, and its absence
/// here is what made a sitting able to start inside this delete. Every other
/// child of an exam now writes the exam row in its own transaction, so the
/// store refuses the pair; an attempt cannot, because its claim lands on the
/// *student's* row (`exam_sat_total`) and touches nothing this delete
/// writes. `start_attempt` already takes the writer lease from its exam read
/// through the insert, so this one lease is the whole ordering: a start
/// either finishes before the sweep (which then takes its row) or reads no
/// exam at all and is a 404. Left orphaned, that attempt kept a sitting on
/// the student's lifetime counter and could mint a badge — awards are
/// add-only and never revoked — for an exam that never existed.
///
/// It spans the blob names too: they are collected *before* the rows go, so
/// an image row written after that snapshot would strand its bytes on disk
/// even though the row itself is now refused.
///
/// corner-cut: process-local, so it holds because the deployment runs one
/// process by contract with stop-the-world deploys (two overlapping
/// binaries would
/// reopen it). Closing it in the store means the `cap` shape the counter
/// work already sketched: `claim_and_create` gaining a second record to
/// touch, so the attempt writes the exam key as every other child does.
pub async fn delete(db: &Database, target: &Exam) -> Result<DeleteOutcome, AppError> {
    require_open(db, &course_of(target, db).await?).await?;
    let _guard = EXAM_LOCK.write().await;
    // Rows go first (the delete cascades them), blobs after — a crash in
    // between strands at worst an unreachable blob.
    let images = QuestionImage::list_for_exam(target.get_id(), db).await?;
    let answer_images = AnswerImage::list_for_exam(target.get_id(), db).await?;
    exam::delete(db, target.clone()).await?;
    Ok(DeleteOutcome {
        image_files: images
            .iter()
            .map(|image| image.get_file().to_string())
            .collect(),
        answer_image_files: answer_images
            .iter()
            .map(|image| image.get_file().to_string())
            .collect(),
    })
}
