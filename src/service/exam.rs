//! Exam workflows: the create that resolves the exam's instance and term,
//! the PATCH re-derive — the merge (set / clear / keep per field) re-judged
//! against a fresh read every retry round, and the mode-freeze, re-draft, and
//! kind gates that guard it — and the delete that collects the image blob keys
//! inside the cascade's own transaction. The queries live in
//! [`crate::db::exam`]; the sitting workflows next door in
//! [`crate::service::exam_attempt`].

use crate::constant::CAS_UPDATE_RETRIES;
use crate::database::Database;
use crate::db::exam;
use crate::db::exam_attempt::any_for_exam;
use crate::db::exam_audience;
use crate::db::exam_result;
use crate::domain::class_course::ClassCourseId;
use crate::domain::course::CourseId;
use crate::domain::exam::{
    Exam, ExamAttemptLimit, ExamDescription, ExamDuration, ExamId, ExamKind, ExamMode,
    ExamSchedule, ExamTitle, redraft_error,
};
use crate::domain::term::TermId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};
use crate::service::exam_attempt::require_open;

/// Publish (or draft) an exam on one class×course instance, inside one term.
///
/// Two refusals stand in front of the write. The instance's catalog course
/// must be class-delivered (`kind = course`): a club or supervised study has
/// no exams — D9 keeps the two membership tiers apart, and an exam on a club
/// would be a roster nothing enrolls into. And the instance's *year* must
/// still be open (D8: the term is a grading slice inside the year, so the year
/// is what the exam's structure belongs to; a term archived inside an open
/// year does not close exam creation — the archive is a record, not a wall).
///
/// The `term` is the term the exam is sat in, and it is required; the store's
/// create claims it in the same transaction as the row. It must be one of the
/// *instance's own year's* terms: a term is a grading slice inside a year, so
/// a term from another year is refused here with a `400` on the field rather
/// than filed — the write would succeed in the store and misfile the exam in
/// the report card of a year it is not taught in.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the sibling entities' create(field, field, ..) shape"
)]
pub async fn create(
    db: &Database,
    creator: &UserId,
    class_course: &ClassCourseId,
    term: &TermId,
    title: ExamTitle,
    description: ExamDescription,
    kind: ExamKind,
    schedule: ExamSchedule,
    max_attempts: ExamAttemptLimit,
    allow_rejoin: bool,
    allow_review: bool,
    draft: bool,
) -> Result<Exam, AppError> {
    let Some(instance) = crate::db::class_course::read(db, class_course).await? else {
        return Err(AppError::NotFound);
    };
    let Some(course) = crate::db::course::read(db, instance.get_course()).await? else {
        return Err(AppError::Internal(
            "the instance references a missing course".into(),
        ));
    };
    if !course.get_kind().is_class_delivered() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "course",
            reason: "only a ders can carry exams — a club or etüt is joined, not sat",
        }));
    }
    // The term must be one of the instance's own year's: the year is the
    // scope a report card is computed over (and a term is a grading slice
    // *inside* it), so an exam filed under another year's term would be
    // counted into a report whose terms it does not belong to — invisible to
    // the year it is actually taught in and foreign to the one it names. The
    // instance's year is its section's, read through the one seam that
    // resolves it.
    let Some(term_row) = crate::db::term::read(db, term).await? else {
        return Err(crate::domain::term::gone_error());
    };
    let instance_year = crate::service::class_course::year_of(db, class_course).await?;
    if instance_year != Some(*term_row.get_year()) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "term",
            reason: "the term belongs to another academic year than this class",
        }));
    }
    crate::service::class_course::require_open(db, class_course).await?;
    exam::create(
        db,
        creator,
        class_course,
        term,
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

/// One instance's exams, newest first — the read behind
/// `GET /instances/{id}/exams`.
pub async fn list_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
) -> Result<Vec<Exam>, AppError> {
    exam::list_for_class_course(db, class_course).await
}

/// Every exam of every instance in `instances` (one query) — the report-card
/// and marks reports' cross-instance read, and the list behind a caller's
/// visible instances.
pub async fn list_for_class_course_courses(
    db: &Database,
    instances: &[ClassCourseId],
) -> Result<Vec<Exam>, AppError> {
    exam::list_for_class_course_courses(db, instances).await
}

/// Every exam of every catalog course in `courses` (one query) — the catalog
/// as one user sees it, across the instances those courses are taught in.
pub async fn list_for_courses(db: &Database, courses: &[CourseId]) -> Result<Vec<Exam>, AppError> {
    exam::list_for_courses(db, courses).await
}

// ---- audience: the shared exam (D2) ---------------------------------------

/// The exam, or the `404` a gone id deserves — and the caller's right to act
/// on the exam's **owner** instance, or the `403` D10 answers
/// ([`crate::service::class_course::ensure_instance_teacher`]: manager+, an
/// assigned teacher, or the section's homeroom teacher).
///
/// The audience routes never move the exam, they announce it, so the gate is
/// the owner's and not the target's: the section that runs the exam decides
/// who else it is addressed to, and a teacher of the receiving section has no
/// say in the announcement.
async fn managed_exam(db: &Database, user: &User, id: &ExamId) -> Result<Exam, AppError> {
    let exam = exam::read(db, id).await?.ok_or(AppError::NotFound)?;
    crate::service::class_course::ensure_instance_teacher(db, user, exam.get_class_course())
        .await?;
    Ok(exam)
}

/// Announce `exam` to another instance — the shared-exam write (D2).
///
/// The exam stays owned by the instance it was created on; an audience row is
/// what makes a second (third, …) class section sit the same sitting and carry the
/// mark in its own marks and report card ([`crate::db::exam_result`] reads
/// through `exam_audience`). Three rules keep the announcement from filing
/// academic work where it cannot be graded:
///
/// - the target must teach the **same catalog course** — a mark on an algebra
///   exam standing in a geometry instance's report is a subject that report
///   never taught;
/// - both instances must sit under the **same academic year** — a report card
///   is computed over one year, so an announcement across years would file the
///   mark into a year the exam is not taught in (the rule [`create`] already
///   applies to the term);
/// - the target may not be the owner itself: the owner's audience row always
///   exists, and the owner cannot be withdrawn (see [`remove_audience`]) —
///   deleting the exam is what ends it.
///
/// The target's year must still be open ([`crate::service::class_course::require_open`]):
/// announcing into an archived year is a write into a read-only year like
/// every other one. A repeat announcement is success, not a conflict — the
/// store treats the existing row as a no-op — and the answer is the audience
/// the exam holds after the call.
///
/// Every refusal is coded: `404` for a gone exam or target instance, `403`
/// for a caller with no right over the owner, `400` for the three shape
/// rules, and the archived year's `409`.
pub async fn add_audience(
    db: &Database,
    user: &User,
    id: &ExamId,
    target: &ClassCourseId,
) -> Result<Vec<exam_audience::Audience>, AppError> {
    let exam = managed_exam(db, user, id).await?;
    let owner = crate::db::class_course::read(db, exam.get_class_course())
        .await?
        // A foreign key names the owner, so only a concurrent detach — which
        // sweeps the exam in the same transaction — could have taken it;
        // either way the exam is gone, and 404 is that answer.
        .ok_or(AppError::NotFound)?;
    let target_row = crate::db::class_course::read(db, target)
        .await?
        .ok_or(AppError::NotFound)?;
    if target_row.get_id() == owner.get_id() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "instance",
            reason: "this instance owns the exam — an audience is another instance, and \
                     deleting the exam is what ends the owner's",
        }));
    }
    if target_row.get_course() != owner.get_course() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "instance",
            reason: "the instance teaches another course than the exam's",
        }));
    }
    let owner_year = crate::service::class_course::year_of(db, owner.get_id()).await?;
    if owner_year != crate::service::class_course::year_of(db, target).await? {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "instance",
            reason: "the instance belongs to another academic year than the exam's",
        }));
    }
    crate::service::class_course::require_open(db, target).await?;
    exam_audience::add(db, id, target).await?;
    list_audience(db, id).await
}

/// Withdraw `exam` from one instance's audience. The same **gate** as
/// [`add_audience`] (management rights over the exam's owner instance), and
/// none of its shape rules re-run: the row was validated when it was
/// announced, and taking it back is cleanup — a stale announcement (an
/// instance whose section moved years, say) can always be withdrawn.
///
/// One pair is the exception, and it is the route's whole safety property:
/// the **owner's** row is not withdrawable. Removing it would leave an exam
/// belonging to no instance — every audience read is the `exam_audience`
/// join, so the exam, its marks and its attempts would vanish from every
/// report while their rows stood — which is why the owner pair is a `400`
/// here and not a delete.
///
/// A pair that holds no audience row is the `404`
/// [`crate::db::exam_audience::remove`]'s boolean carries — never a silent
/// success. The exam's own year must still be open, like every other write
/// against the exam's structure ([`update`] and [`delete`]): withdrawing from
/// an archived year's shared exam is a `409`, not a silent edit of a past
/// year's record. The answer is the audience the exam holds after the call.
pub async fn remove_audience(
    db: &Database,
    user: &User,
    id: &ExamId,
    target: &ClassCourseId,
) -> Result<Vec<exam_audience::Audience>, AppError> {
    let exam = managed_exam(db, user, id).await?;
    if exam.get_class_course() == target {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "instance",
            reason: "the owner instance is not an audience — an exam cannot lose the \
                     instance it belongs to; delete the exam instead",
        }));
    }
    require_open(db, &exam).await?;
    if !exam_audience::remove(db, id, target).await? {
        return Err(AppError::NotFound);
    }
    list_audience(db, id).await
}

/// The audience of one exam: the instances it is announced to, its owner
/// included — the read behind `GET /exams/{id}/audience`. Read-only, so the
/// web layer gates it with the exam's own view rule.
pub async fn list_audience(
    db: &Database,
    id: &ExamId,
) -> Result<Vec<exam_audience::Audience>, AppError> {
    exam_audience::list_for_exam(db, id).await
}

/// Whether `user` is enrolled in any instance `exam` is addressed to — its
/// owner or an announced sibling (D2).
///
/// This is the enrollment predicate every *student* door at an exam asks
/// (sitting, answering, grading), and announcing an exam is what moves it:
/// the addressed sections' students sit and are graded at their own instance.
/// An exam nobody was announced to answers exactly what the owner-only check
/// these doors used to run answered.
pub async fn enrolled_anywhere(
    db: &Database,
    exam: &Exam,
    user: &UserId,
) -> Result<bool, AppError> {
    exam_audience::enrolled(db, exam.get_id(), user).await
}

/// A `403` unless [`enrolled_anywhere`] — the refusal the sit paths answer,
/// kept byte-for-byte from the owner-only check it replaces.
pub async fn ensure_enrolled_anywhere(
    db: &Database,
    exam: &Exam,
    user: &UserId,
) -> Result<(), AppError> {
    if enrolled_anywhere(db, exam, user).await? {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "you are not enrolled in this exam's course",
    ))
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
/// The mode gate below reads `exam_attempt`, and the sitting create's
/// transaction re-judges the sittable gates on the locked exam row
/// ([`crate::db::exam_attempt::guard_start`]), so a first sitting still
/// cannot land on an exam whose mode this PATCH is flipping — the store
/// decides, not a process lock.
///
/// It buys nothing against grading, which is a reader too: the re-draft gate
/// is therefore enforced inside the update's own transaction
/// ([`exam::update_if_unchanged`]), where the store decides it. The gate
/// below stays as the pre-flight — same error, one round trip earlier.
/// Concurrent PATCHes of this exam no longer queue behind each other either:
/// the lost update they used to cause is refused by the compare-and-set.
pub async fn update(db: &Database, id: &ExamId, patch: &ExamPatch) -> Result<Exam, AppError> {
    let mut left = CAS_UPDATE_RETRIES;
    loop {
        let current = exam::read(db, id).await?.ok_or(AppError::NotFound)?;
        require_open(db, &current).await?;

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
        // supported live adjustments instead. The sitting create's own
        // transaction re-judges the mode gate on the locked row, so a first
        // attempt can't land in the gap and leave a sat exam's mode flipped
        // under it.
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
            let graded = !exam_result::list_for_exam(db, current.get_id())
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
        // be. Marks are counted on the exam row, and the save below pins
        // that counter — reading it inside its own transaction — so a grade
        // landing between this read and the write refuses the save (the
        // loop then re-reads and answers the 409 below).
        if kind.as_str() != current.get_kind().as_str()
            && exam::result_count(db, current.get_id()).await? > 0
        {
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
/// The store replaced the writer lease this delete used to hold across the
/// whole cascade, blob names included: the delete's transaction takes the
/// exam row `FOR UPDATE` first, and every child writer that matters locks
/// the same row first (a sitting create's guard, the freeze gate, an answer
/// save ahead of its upsert). So a start or a save either finishes before
/// the sweep — which then takes its row too — or finds no exam and is a
/// `404`. Left orphaned, an attempt kept a sitting on the student's
/// lifetime counter and could mint a badge — awards are add-only and never
/// revoked — for an exam that never existed.
///
/// It spans the blob names too: they are collected *inside* the deleting
/// transaction, under the lock, so an image row written after an
/// out-of-transaction snapshot cannot strand its bytes on disk even though
/// the row itself is now refused.
pub async fn delete(db: &Database, target: &Exam) -> Result<DeleteOutcome, AppError> {
    require_open(db, target).await?;
    // Rows go first (the delete cascades them), blobs after — a crash in
    // between strands at worst an unreachable blob.
    let deleted = exam::delete(db, target.clone()).await?;
    Ok(DeleteOutcome {
        image_files: deleted.question_image_files,
        answer_image_files: deleted.answer_image_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::academic_year::AcademicYearId;
    use crate::domain::class_course::ClassCourse;
    use crate::domain::class_group::ClassName;
    use crate::domain::role::Role;
    use crate::domain::settings::Settings;
    use crate::domain::user::Username;

    /// A real account at a role — the announcement gate judges the live row,
    /// so a raw fixture id would not do.
    async fn staff(db: &Database, username: &str, role: Role) -> User {
        let account = crate::db::user::create(db, Username::try_new(username).unwrap(), None)
            .await
            .unwrap();
        crate::service::user::set_role(db, account.get_id(), role)
            .await
            .unwrap();
        crate::db::user::read(db, account.get_id())
            .await
            .unwrap()
            .unwrap()
    }

    /// An instance of `course` inside a fresh class section of `year` — one
    /// side of an announcement.
    async fn instance_in(
        db: &Database,
        manager: &UserId,
        name: &str,
        course: &CourseId,
        year: Option<&AcademicYearId>,
    ) -> ClassCourse {
        let class = crate::db::class_group::create(
            db,
            manager,
            ClassName::try_new(name).unwrap(),
            None,
            year.cloned(),
            None,
        )
        .await
        .unwrap();
        crate::service::class_course::attach(db, class.get_id(), course, manager)
            .await
            .unwrap()
    }

    /// A published, unscheduled exam on `instance` — the parent an audience
    /// row hangs off.
    async fn exam_on(db: &Database, instance: &ClassCourse) -> Exam {
        let creator = crate::db::class_member::tests::fixture_user(db, "audience-author").await;
        let term = crate::db::term::a_test_term(db).await;
        let allowed = Settings::defaults().get_exam_kinds().to_vec();
        crate::db::exam::create(
            db,
            &creator,
            instance.get_id(),
            &term,
            ExamTitle::try_new("1. Yazılı").unwrap(),
            ExamDescription::try_new("").unwrap(),
            ExamKind::try_new("yazili", &allowed).unwrap(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
        )
        .await
        .unwrap()
    }

    /// The announcement's shape rules: same catalog course, same academic
    /// year, another instance — each violation a coded `400`, the pair that
    /// satisfies all three a row the audience read (and so the marks reports)
    /// sees, and a repeat a no-op rather than a conflict.
    #[tokio::test]
    async fn an_announcement_needs_the_same_course_and_year() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = staff(&db, "audience-mudur", Role::Manager).await;
        let algebra = crate::db::class_member::tests::a_course("algebra", &db).await;
        let geometry = crate::db::class_member::tests::a_course("geometry", &db).await;
        let year = crate::db::academic_year::a_test_year(&db).await;
        let other_year = crate::db::academic_year::a_test_year(&db).await;

        let owner = instance_in(&db, manager.get_id(), "5-A", &algebra, Some(&year)).await;
        let sibling = instance_in(&db, manager.get_id(), "5-B", &algebra, Some(&year)).await;
        let wrong_course = instance_in(&db, manager.get_id(), "5-C", &geometry, Some(&year)).await;
        let wrong_year =
            instance_in(&db, manager.get_id(), "6-A", &algebra, Some(&other_year)).await;
        let exam = exam_on(&db, &owner).await;

        let audience = add_audience(&db, &manager, exam.get_id(), sibling.get_id())
            .await
            .unwrap();
        assert_eq!(audience.len(), 2, "owner + announcement");
        assert_eq!(
            audience[0].get_instance(),
            owner.get_id(),
            "the owner stands first — announcement order is creation order"
        );
        assert_eq!(audience[1].get_instance(), sibling.get_id());
        assert_eq!(audience[1].get_class(), sibling.get_class());
        assert_eq!(audience[1].get_course(), &algebra);
        // The sibling's own exam list carries it — the join the marks reports
        // and the instance page read.
        assert_eq!(
            list_for_class_course(&db, sibling.get_id())
                .await
                .unwrap()
                .len(),
            1,
            "the announcement is one exam, addressed here"
        );

        for (bad, why) in [
            (&wrong_course, "another course"),
            (&wrong_year, "another year"),
        ] {
            let refused = add_audience(&db, &manager, exam.get_id(), bad.get_id()).await;
            assert!(
                matches!(refused, Err(AppError::Validation(_))),
                "{why} must be a coded 400: {refused:?}"
            );
        }
        let owned = add_audience(&db, &manager, exam.get_id(), owner.get_id()).await;
        assert!(
            matches!(owned, Err(AppError::Validation(_))),
            "the owner is not an audience: {owned:?}"
        );
        let ghost = ClassCourseId::generate();
        assert!(
            matches!(
                add_audience(&db, &manager, exam.get_id(), &ghost).await,
                Err(AppError::NotFound)
            ),
            "a minted instance id names nothing"
        );
        assert!(matches!(
            add_audience(&db, &manager, &ExamId::generate(), sibling.get_id()).await,
            Err(AppError::NotFound)
        ));

        add_audience(&db, &manager, exam.get_id(), sibling.get_id())
            .await
            .unwrap();
        assert_eq!(
            exam_audience::list_for_exam(&db, exam.get_id())
                .await
                .unwrap()
                .len(),
            2,
            "a repeat announcement is a no-op, not a second row"
        );
    }

    /// The gate is the **owner** instance's: its assigned teacher and a
    /// manager+ may announce and withdraw; the target instance's own teacher
    /// may not — running the receiving section is not running the exam — and
    /// a student may not either.
    #[tokio::test]
    async fn an_announcement_is_gated_on_the_owner_instance() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = staff(&db, "audience-mudur-2", Role::Manager).await;
        let owner_teacher = staff(&db, "audience-ogretmen-a", Role::Teacher).await;
        let target_teacher = staff(&db, "audience-ogretmen-b", Role::Teacher).await;
        let student = staff(&db, "audience-ogrenci", Role::Student).await;
        let algebra = crate::db::class_member::tests::a_course("algebra", &db).await;
        let owner = instance_in(&db, manager.get_id(), "7-A", &algebra, None).await;
        let target = instance_in(&db, manager.get_id(), "7-B", &algebra, None).await;
        crate::service::class_course::assign_teacher(
            &db,
            owner.get_id(),
            owner_teacher.get_id(),
            manager.get_id(),
        )
        .await
        .unwrap();
        crate::service::class_course::assign_teacher(
            &db,
            target.get_id(),
            target_teacher.get_id(),
            manager.get_id(),
        )
        .await
        .unwrap();
        let exam = exam_on(&db, &owner).await;

        assert!(
            add_audience(&db, &owner_teacher, exam.get_id(), target.get_id())
                .await
                .is_ok(),
            "an assigned teacher of the owner instance announces"
        );
        assert!(
            remove_audience(&db, &owner_teacher, exam.get_id(), target.get_id())
                .await
                .is_ok(),
            "…and withdraws"
        );
        for (user, who) in [
            (&target_teacher, "the target's teacher"),
            (&student, "a student"),
        ] {
            let refused = add_audience(&db, user, exam.get_id(), target.get_id()).await;
            assert!(
                matches!(refused, Err(AppError::Forbidden(_))),
                "{who} must be a 403: {refused:?}"
            );
        }
    }

    /// Withdrawal: the owner's own pair is a coded `400` (an exam cannot lose
    /// the instance it belongs to), a pair holding no row is a `404`, and the
    /// row the instance's read joins on is gone afterwards.
    #[tokio::test]
    async fn withdrawing_an_audience_leaves_the_owner_alone() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = staff(&db, "audience-mudur-3", Role::Manager).await;
        let algebra = crate::db::class_member::tests::a_course("algebra", &db).await;
        let owner = instance_in(&db, manager.get_id(), "8-A", &algebra, None).await;
        let target = instance_in(&db, manager.get_id(), "8-B", &algebra, None).await;
        let exam = exam_on(&db, &owner).await;
        add_audience(&db, &manager, exam.get_id(), target.get_id())
            .await
            .unwrap();

        let audience = remove_audience(&db, &manager, exam.get_id(), target.get_id())
            .await
            .unwrap();
        assert_eq!(audience.len(), 1);
        assert_eq!(audience[0].get_instance(), owner.get_id());
        assert!(
            list_for_class_course(&db, target.get_id())
                .await
                .unwrap()
                .is_empty(),
            "the withdrawn instance no longer carries the exam"
        );

        assert!(
            matches!(
                remove_audience(&db, &manager, exam.get_id(), target.get_id()).await,
                Err(AppError::NotFound)
            ),
            "a pair holding no row is a 404, never a silent success"
        );
        let owner_pair = remove_audience(&db, &manager, exam.get_id(), owner.get_id()).await;
        assert!(
            matches!(owner_pair, Err(AppError::Validation(_))),
            "the owner pair is refused: {owner_pair:?}"
        );
        assert_eq!(
            exam_audience::list_for_exam(&db, exam.get_id())
                .await
                .unwrap()
                .len(),
            1,
            "the owner's row stands"
        );
    }

    /// The archived-year posture of both routes: announcing into an archived
    /// year and withdrawing from one are the coded `409` every other write
    /// against the year answers, and neither writes — an announcement row
    /// predating the archive is no loophole for more of them.
    #[tokio::test]
    async fn an_archived_year_refuses_the_announcement_and_the_withdrawal() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = staff(&db, "audience-mudur-4", Role::Manager).await;
        let algebra = crate::db::class_member::tests::a_course("algebra", &db).await;
        let year = crate::db::academic_year::a_test_year(&db).await;
        let owner = instance_in(&db, manager.get_id(), "9-A", &algebra, Some(&year)).await;
        let announced = instance_in(&db, manager.get_id(), "9-B", &algebra, Some(&year)).await;
        let later = instance_in(&db, manager.get_id(), "9-C", &algebra, Some(&year)).await;
        let exam = exam_on(&db, &owner).await;
        add_audience(&db, &manager, exam.get_id(), announced.get_id())
            .await
            .unwrap();

        crate::service::academic_year::archive(&db, &year)
            .await
            .unwrap();

        for (result, route) in [
            (
                add_audience(&db, &manager, exam.get_id(), later.get_id()).await,
                "the announcement",
            ),
            (
                remove_audience(&db, &manager, exam.get_id(), announced.get_id()).await,
                "the withdrawal",
            ),
        ] {
            assert!(
                matches!(
                    result,
                    Err(AppError::ConflictCoded { code, .. }) if code == "academic_year_archived"
                ),
                "{route} into an archived year must be the coded 409: {result:?}"
            );
        }
        assert_eq!(
            exam_audience::list_for_exam(&db, exam.get_id())
                .await
                .unwrap()
                .len(),
            2,
            "neither refusal wrote"
        );
    }
}
