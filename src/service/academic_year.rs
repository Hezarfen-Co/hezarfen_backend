//! Academic-year workflows: the archived-year refusal every write against past
//! structure pays, the link resolver class/term create pass through, and the
//! rollover — the one command that carries a year's şubeler into the next. The
//! queries live in [`crate::db::academic_year`].

use crate::database::Database;
use crate::db::academic_year;
use crate::db::class_course;
use crate::db::class_course_teacher;
use crate::db::class_group;
use crate::db::class_member;
use crate::db::class_pump::{self, Attached};
use crate::domain::academic_year::{
    AcademicYear, AcademicYearId, AcademicYearName, GradePromotion, archived_error,
    rollover_target_not_empty,
};
use crate::domain::class_group::ClassGrade;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The year row, for callers that only inspect it — the web layer's gates
/// read through here.
pub async fn read(db: &Database, id: &AcademicYearId) -> Result<Option<AcademicYear>, AppError> {
    academic_year::read(db, id).await
}

/// Every year, newest first, paged — the calendar is small by nature.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<AcademicYear>, i64), AppError> {
    academic_year::list_all(db, limit, offset).await
}

/// Mint one year. Both ends are required and the range is checked here, where
/// the store's own insert has no `WHERE` to re-check it — the same rule
/// [`AcademicYear::try_new`] states, in the same words.
pub async fn create(
    db: &Database,
    creator: &UserId,
    name: AcademicYearName,
    starts_at: Timestamp,
    ends_at: Timestamp,
    grade_promotions: Vec<GradePromotion>,
) -> Result<AcademicYear, AppError> {
    if ends_at <= starts_at {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "ends_at",
            reason: "must be after starts_at",
        }));
    }
    academic_year::create(db, creator, name, starts_at, ends_at, grade_promotions).await
}

/// Turn an optional request-supplied year id into a validated reference —
/// `None` stays `None`, an unknown id is a `400` naming the field, and an
/// *archived* one is the year's `409`. This is the single spot every new link
/// to a year passes through: a şube create/update and a dönem create alike,
/// so past years take no new structure. The mirror of
/// [`crate::service::term::resolve`], one layer up the calendar.
pub async fn resolve(db: &Database, id: Option<&str>) -> Result<Option<AcademicYearId>, AppError> {
    let Some(id) = id else {
        return Ok(None);
    };
    let resolved = academic_year::read(db, &AcademicYearId::from_key(id))
        .await?
        .ok_or(AppError::Validation(ValidationError::Invalid {
            field: "year_id",
            reason: "academic year does not exist",
        }))?;
    if resolved.is_archived() {
        return Err(archived_error());
    }
    Ok(Some(*resolved.get_id()))
}

/// Refuse when the named year is archived. A missing row is `Ok(())`: a
/// dangling link is not this guard's error, and the caller that cares already
/// answers it (the store's own year-gone refusal).
pub async fn require_open(db: &Database, id: &AcademicYearId) -> Result<(), AppError> {
    match academic_year::read(db, id).await? {
        Some(year) if year.is_archived() => Err(archived_error()),
        _ => Ok(()),
    }
}

/// Refuse when this year row is archived — the read-only rule for past
/// structure, judged against a row the caller already read through [`read`];
/// a missing row is the caller's 404, not this guard's answer.
pub fn require_writable(year: &AcademicYear) -> Result<(), AppError> {
    if year.is_archived() {
        return Err(archived_error());
    }
    Ok(())
}

/// Update an open year; an archived one is refused — past years are read-only.
pub async fn update(
    db: &Database,
    target: AcademicYear,
    name: Option<AcademicYearName>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
    grade_promotions: Option<Vec<GradePromotion>>,
) -> Result<AcademicYear, AppError> {
    require_writable(&target)?;
    academic_year::update(
        db,
        target.get_id(),
        name,
        starts_at,
        ends_at,
        grade_promotions,
    )
    .await
}

/// Close a finished year: stamp `archived_at`, which is what makes the whole
/// year read-only ([`require_open`]/[`require_writable`]). Idempotent — an
/// already-archived year answers with the row it holds, original stamp and
/// all, and a repeat never re-dates it; a missing row is the `404` every
/// other year route answers. One conditional statement does the write
/// ([`academic_year::archive`]), so two archives racing land one stamp: the
/// loser finds `None` and reads the winner's row.
pub async fn archive(db: &Database, id: &AcademicYearId) -> Result<AcademicYear, AppError> {
    let year = read(db, id).await?.ok_or(AppError::NotFound)?;
    if year.is_archived() {
        return Ok(year);
    }
    match academic_year::archive(db, id).await? {
        Some(archived) => Ok(archived),
        // A concurrent archive (or a delete) got there first: the stored row
        // is the answer, and only this no-op path pays for the second read.
        None => read(db, id).await?.ok_or(AppError::NotFound),
    }
}

/// Delete the year while nothing links it — the linked-refusal is the rule's
/// own answer, so the message the web layer used to pick is coded here; a row
/// that is already gone is the store's `404` ([`academic_year::delete`]).
pub async fn delete(db: &Database, target: AcademicYear) -> Result<(), AppError> {
    if academic_year::delete(db, target).await? {
        Ok(())
    } else {
        Err(AppError::Conflict(
            "classes and terms are still linked to this academic year — unlink them first",
        ))
    }
}

/// What one [`rollover`] carried: how many şubeler were planted in the target
/// year, how many live students came with them, and the grades that stayed
/// behind because the year lists no promotion for them (graduation).
#[derive(Debug, Clone)]
pub struct RolloverReport {
    pub classes: usize,
    pub students: usize,
    pub graduated: Vec<String>,
}

/// Carry `from`'s şubeler into `target`, the explicit year-boundary command.
///
/// A grade with **no** promotion entry is not rolled over — that is how
/// graduation is expressed — and a şube carrying no grade at all is not
/// either: there is nothing to promote it on. Each rolled şube is created
/// afresh in the target year with the same name and the mapped grade, then
/// gets copies of everything that belongs to the *structure* of the year: its
/// instances (same courses, same `ders_saati`, same karne policy, `source`
/// left NULL so no blueprint sweep can take a copied link back), each
/// instance's assigned teachers, and every **live** member — copied through
/// the pump, which is also what enrolls them into the new instances, and
/// tagged `source_class_group` with the şube they were copied out of (the
/// provenance §4.9 asks for: the new roster still names the old stint's şube).
/// The old year is untouched: it stays the record of what was taught.
///
/// Refusals: the target year must be empty (a second rollover into it is a
/// `409`, which is also what makes the command idempotent — a re-run is
/// refused, never duplicated), archived (read-only), and distinct from the
/// source. The emptiness check is a pre-flight read, an accepted race in the
/// README's sense: two rollovers racing the same empty target can each plant
/// şubeler; the class writes themselves serialize on the year row, so the
/// counters stay honest either way.
///
/// Each şube is carried over on its own: a failure mid-run leaves the
/// şubeler already planted in place, and a re-run is refused by the emptiness
/// guard — the report says what landed, and the operator resolves the rest by
/// hand. One transaction per write, never one across the run: the store owns
/// concurrency, and a run-wide transaction would hold the year row for the
/// length of the whole school.
pub async fn rollover(
    db: &Database,
    target: &AcademicYearId,
    from: &AcademicYearId,
    by: &UserId,
) -> Result<RolloverReport, AppError> {
    let target_year = read(db, target).await?.ok_or(AppError::NotFound)?;
    require_writable(&target_year)?;
    if target == from {
        return Err(AppError::Conflict(
            "a year cannot roll over into itself — name the year that follows",
        ));
    }
    if target_year.get_class_count() != 0 {
        return Err(rollover_target_not_empty());
    }
    let from_year = read(db, from).await?.ok_or(AppError::NotFound)?;

    let mut report = RolloverReport {
        classes: 0,
        students: 0,
        graduated: Vec::new(),
    };
    for class in class_group::list_for_year(db, from).await? {
        let Some(grade) = class.get_grade() else {
            continue;
        };
        let Some(mapped) = from_year.promotion_for(grade.as_str()) else {
            // No promotion entry: this grade graduated. Reported once per
            // label, however many şubeler carry it.
            let label = grade.as_str().to_string();
            if !report.graduated.contains(&label) {
                report.graduated.push(label);
            }
            continue;
        };
        let next = ClassGrade::try_new(mapped)?;
        let planted = class_group::create(
            db,
            by,
            class.get_name().clone(),
            Some(next),
            Some(*target),
            class.get_teacher().copied(),
        )
        .await?;
        let planted_id = planted.get_id().clone();

        // The structure first, so the member copy below enrolls the students
        // into every instance the new şube carries; the pump's member axis is
        // what pays those enrollments.
        for instance in
            class_course::list_for_class_ids(db, std::slice::from_ref(class.get_id())).await?
        {
            let landed =
                class_course::attach_sourced(db, &planted_id, instance.get_course(), by, None)
                    .await?;
            let Attached::Made(copied) = landed else {
                // A fresh şube cannot hold the course already, and its caps
                // cannot be full: anything else here is the store disagreeing
                // with the read one statement earlier, which is a 500 and not
                // a partial rollover.
                return Err(AppError::Internal(format!(
                    "rollover: attaching {} to the new class was refused",
                    instance.get_course().key()
                )));
            };
            class_course::update(
                db,
                copied.get_id(),
                Some(instance.get_ders_saati()),
                Some(instance.counts_toward_karne()),
            )
            .await?;
            for teacher in class_course_teacher::list_for_instance(db, instance.get_id()).await? {
                class_course_teacher::assign(db, copied.get_id(), &teacher).await?;
            }
        }

        for member in class_member::list_for_class(db, class.get_id(), None, 0)
            .await?
            .0
        {
            // The stint carries where the student came from: the copy names the
            // şube it was copied out of, so last year's roster is still legible
            // from the new one (and the tag is written in the same statement as
            // the stint — see `add_member_sourced`).
            match class_pump::add_member_sourced(
                db,
                &planted_id,
                member.get_user(),
                by,
                Some(class.get_id()),
            )
            .await?
            {
                Attached::Made(_) => report.students += 1,
                // A student the pump refuses (demoted out of `student` since
                // the stint was read, or gone) is not carried over: the new
                // year's roster is written under the same role rule the route
                // is.
                Attached::PivotGone | Attached::Gone => continue,
                _ => {
                    return Err(AppError::Internal(format!(
                        "rollover: adding {} to the new class was refused",
                        member.get_user().key()
                    )));
                }
            }
        }
        report.classes += 1;
    }
    Ok(report)
}
