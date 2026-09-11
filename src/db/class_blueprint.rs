//! The `class_blueprint` table: the row keyed by its grade label, the
//! compare-and-set writes the edit and the delete go through, and the reads
//! the sweep and the status screen are built on. The workflows that sequence
//! these under [`crate::service::class_blueprint::BLUEPRINT_LOCK`] live in
//! [`crate::service::class_blueprint`].

use surrealdb::types::{RecordId, SurrealValue};

use crate::constant::{CLASS_BLUEPRINT_TABLE, CLASS_COURSE_TABLE, ENROLLMENT_COUNT_FIELD};
use crate::database::{Database, transaction_with_retry};
use crate::db::page::PagedList;
use crate::domain::class_blueprint::{ClassBlueprint, ClassBlueprintId};
use crate::domain::class_group::{ClassGrade, ClassGroupId};
use crate::domain::class_pump::{Axis, detach};
use crate::domain::course::CourseId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The `THROW` marker a template write aborts with when one of the courses it
/// names is gone — the same answer the caller's own pre-flight read gives, made
/// inside the transaction that stores the list.
const DEAD_MARK: &str = "blueprint_no_course";

/// One `class_course` row, projected down to the pair that answers "does this
/// section carry that course". `source` is deliberately not read: a link of any
/// provenance satisfies the template.
#[derive(SurrealValue)]
pub struct Held {
    pub(crate) class: ClassGroupId,
    pub(crate) course: CourseId,
}

/// The statements that prove every course a template is about to name is
/// still there, *inside* the transaction that stores the list.
///
/// The caller's own pre-flight read is not that proof: it is a read of
/// `course` in a request that then writes `class_blueprint`, and SurrealDB
/// does not conflict-check reads (write skew). `DELETE /courses/{id}` sweeps
/// every template naming the course as part of its cascade, so a delete
/// landing between the read and the write sweeps a row that is not there yet
/// and the list keeps an id nothing points at. [`prune`] only fires
/// while walking a section, so at a grade carrying none the id is permanent.
///
/// The claim is [`crate::domain::class_pump::Axis::pivot_claim`]'s exactly:
/// the counter is *moved* — bumped, gated, restored to the value this
/// transaction found, `NONE` included — because an `UPDATE` leaving the
/// document unchanged is elided by 3.2.3 and enters no write set. Moving it
/// puts this write on the record the course's delete guard writes.
fn courses_alive() -> String {
    format!(
        "FOR $course IN ($courses ?? []) {{
             LET $was = (SELECT VALUE {ENROLLMENT_COUNT_FIELD} FROM ONLY $course);
             LET $alive = (UPDATE $course SET {ENROLLMENT_COUNT_FIELD} = \
                 ({ENROLLMENT_COUNT_FIELD} ?? 0) + 1 RETURN VALUE id);
             IF array::len($alive) = 0 {{ THROW '{DEAD_MARK}' }};
             UPDATE $course SET {ENROLLMENT_COUNT_FIELD} = $was;
         }}"
    )
}

/// The refusal a [`DEAD_MARK`] abort is: the same `400` the caller's
/// pre-flight read raises, since it is the same fact one instant later.
fn no_such_course() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "course_ids",
        reason: "one of these courses does not exist",
    })
}

/// Write the blueprint. A second one for the same grade is a 409 the store
/// itself decides — the grade is the record key, so the duplicate is seen
/// rather than raced.
///
/// Not [`transaction_with_retry`]: the `CREATE` here can legitimately answer
/// "already exists", which that loop cannot tell from a lost round and would
/// re-send until the tries ran out — turning this 409 into a 500. A round
/// genuinely lost to a concurrent course delete is therefore an error the
/// caller retries, with nothing written.
pub async fn create(
    db: &Database,
    creator: &UserId,
    grade: ClassGrade,
    courses: Vec<CourseId>,
) -> Result<ClassBlueprint, AppError> {
    let blueprint = ClassBlueprint {
        id: ClassBlueprintId::for_grade(&grade),
        grade,
        courses,
        creator: creator.clone(),
    };
    let named: Vec<RecordId> = blueprint.courses.iter().map(CourseId::record).collect();
    let mut result = db
        .query(format!(
            "BEGIN TRANSACTION;
             {};
             CREATE $id CONTENT $row;
             COMMIT TRANSACTION;",
            courses_alive()
        ))
        .bind(("id", blueprint.id.record()))
        .bind(("row", blueprint))
        .bind(("courses", named))
        .await?;
    let mut errors = result.take_errors();
    if errors
        .values()
        .any(|error| error.to_string().contains(DEAD_MARK))
    {
        return Err(no_such_course());
    }
    if errors.values().any(surrealdb::Error::is_already_exists) {
        return Err(AppError::Conflict(
            "a blueprint already exists for that grade",
        ));
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN and the FOR: the CREATE is slot 2.
    result
        .take::<Vec<ClassBlueprint>>(2)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to create blueprint".into()))
}

pub async fn read(
    db: &Database,
    id: &ClassBlueprintId,
) -> Result<Option<ClassBlueprint>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Every blueprint, by grade label — the id *is* the label, so this is the
/// only ordering that means anything here.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassBlueprint>, i64), AppError> {
    PagedList::new(CLASS_BLUEPRINT_TABLE, "ORDER BY id ASC")
        .run(limit, offset, db)
        .await
}

/// Write the course list only while the stored row still carries the list
/// this caller read: two managers editing the same grade cannot have one's
/// list silently pump the other's diff. `None` means the conditional write
/// matched nothing — the row is gone, or its list moved since the caller read
/// it, and only the caller can tell those apart (the workflow re-reads and
/// answers a `409` or a `404`).
///
/// A template naming a course that is gone aborts with [`DEAD_MARK`] and is
/// reported as the caller's own pre-flight `400`.
pub async fn set_courses_if_unchanged(
    db: &Database,
    id: &ClassBlueprintId,
    held: Vec<CourseId>,
    wanted: Vec<CourseId>,
) -> Result<Option<ClassBlueprint>, AppError> {
    let named: Vec<RecordId> = wanted.iter().map(CourseId::record).collect();
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
             {};
             UPDATE $id SET courses = $wanted WHERE courses = $held RETURN AFTER;
             COMMIT TRANSACTION;",
            courses_alive()
        ),
        &[
            ("id".into(), id.record().into_value()),
            ("wanted".into(), wanted.into_value()),
            ("held".into(), held.into_value()),
            ("courses".into(), named.into_value()),
        ],
        &[DEAD_MARK],
    )
    .await?;
    if errors
        .values()
        .any(|error| error.to_string().contains(DEAD_MARK))
    {
        return Err(no_such_course());
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN and the FOR: the UPDATE is slot 2.
    Ok(result.take::<Vec<ClassBlueprint>>(2)?.into_iter().next())
}

/// Delete the row, but only while it still carries the list this caller read
/// — the edit race answers the caller, not a silent delete of somebody
/// else's additions. `false` means the conditional delete matched nothing:
/// the row is gone, or its list moved since the caller read it, and only the
/// caller can tell those apart.
pub async fn delete_if_unchanged(
    db: &Database,
    id: &ClassBlueprintId,
    held: Vec<CourseId>,
) -> Result<bool, AppError> {
    let mut result = db
        .query("DELETE $id WHERE courses = $held RETURN BEFORE")
        .bind(("id", id.record()))
        .bind(("held", held))
        .await?
        .check()?;
    Ok(!result.take::<Vec<ClassBlueprint>>(0)?.is_empty())
}

/// Every `class_course` row held by any of `classes`, projected down to the
/// pair — the read [`crate::service::class_blueprint::status`] diffs the
/// template against.
pub async fn held_links(db: &Database, classes: Vec<RecordId>) -> Result<Vec<Held>, AppError> {
    let mut result = db
        .query(format!(
            "SELECT class, course FROM {CLASS_COURSE_TABLE} WHERE class IN $classes"
        ))
        .bind(("classes", classes))
        .await?
        .check()?;
    Ok(result.take::<Vec<Held>>(0)?)
}

/// Drop a course that no longer exists out of a blueprint's list.
///
/// The delete does this itself now:
/// [`crate::db::course::delete`] sweeps `class_blueprint` in
/// the same cascade that takes the course's `class_course` links, so a
/// template stops naming a course the instant that course goes. This used
/// to be the *only* thing that could remove such an id, and that was the
/// bug: it fires only while walking a section, so a grade with no sections
/// could never reach it, and the repair every doc surface pointed at
/// (`PATCH` the list back as it stands) is a `400` for naming a course that
/// does not exist. A permanent dangling id, and no call that could clear
/// it.
///
/// What is left for this to do is the **window** the sweep cannot cover: a
/// pump walks a snapshot of the list it read, so a course deleted after
/// that read is still attempted, and the delete's sweep has already run
/// past a row it will not visit again. The skip is then reported once for
/// the run that found it (later classes in the same
/// [`crate::service::class_blueprint::pump`] skip it by its dead-course set
/// rather than re-attempting and re-pruning it), and this write is a no-op
/// against a list the cascade already trimmed. It also clears ids written
/// before that cascade existed.
///
/// Not a compare-and-set, unlike
/// [`crate::service::class_blueprint::set_courses`]: "a course that does
/// not exist is not in this list" holds for every version of the list, so
/// there is nothing a concurrent edit could make this write wrong about,
/// and re-running it changes nothing. No sweep follows it either — the
/// attachments it would sweep are exactly the ones the course's own delete
/// already took.
///
/// No [`crate::service::class_blueprint::BLUEPRINT_LOCK`] lease either: an
/// `UPDATE` of a record that is gone writes nothing, so this cannot
/// resurrect a blueprint a delete took while the pump around it was
/// running.
pub async fn prune(
    db: &Database,
    id: &ClassBlueprintId,
    course: &CourseId,
) -> Result<(), AppError> {
    db.query("UPDATE $id SET courses = array::complement(courses ?? [], [$course])")
        .bind(("id", id.record()))
        .bind(("course", course.record()))
        .await?
        .check()?;
    Ok(())
}

/// A blueprint's attachments that `keep` does not justify: every
/// `class_course` row carrying its tag for a course outside that list. The
/// delete passes `&[]` and so takes all of them.
///
/// Read off the *stored* list rather than a diff against a handle, which is
/// what makes a repeated call a repair rather than a no-op: whatever a
/// half-finished sweep left behind is exactly what the next one finds.
///
/// `source` is the whole filter, so a row without the key — a hand attach —
/// is never matched, and a class that acquired the same course by hand keeps
/// it. The rows are read rather than derived from the classes at this grade:
/// a class whose grade was edited after the pump still carries this
/// blueprint's attachments, and only the tag can find it.
pub async fn sourced_links(
    db: &Database,
    id: &ClassBlueprintId,
    keep: &[CourseId],
) -> Result<Vec<RecordId>, AppError> {
    let named: Vec<RecordId> = keep.iter().map(CourseId::record).collect();
    let mut found = db
        .query(format!(
            "SELECT VALUE id FROM {CLASS_COURSE_TABLE} \
             WHERE source = $blueprint AND course NOT IN $keep"
        ))
        .bind(("blueprint", id.record()))
        .bind(("keep", named))
        .await?
        .check()?;
    Ok(found.take::<Vec<RecordId>>(0)?)
}

/// Detach the link rows [`sourced_links`] named and sweep the enrollments
/// they pumped. The sweep underneath is the pump's own, so a student a
/// second class still claims is re-tagged rather than unenrolled.
///
/// Each detach re-asserts the tag, because this loop deliberately runs
/// without the lease: the link's record id is the (class, course) pair and
/// says nothing about provenance, so a row a *new* blueprint at the same
/// grade attached while this ran would otherwise be swept by the old one's
/// tail.
///
/// One transaction per *link row*, which is the module note's "one
/// transaction per (class, course)" and the mirror of the pump's
/// best-effort rule. One statement per course would be shorter, but it puts
/// every section at the grade into a single unbounded transaction: its write
/// loop is one enrollment sweep per (class, member) with no ceiling over the
/// class count, and a failure anywhere in it rolls back the whole grade —
/// so one contended section keeps the other eleven attached to a course the
/// template no longer holds. Per row, a failure leaves the pairs already
/// detached detached, each with its counter released and its enrollments
/// swept, and the rest exactly as they were: a partial removal the next call
/// finishes.
pub async fn drop_links(
    db: &Database,
    id: &ClassBlueprintId,
    links: Vec<RecordId>,
) -> Result<(), AppError> {
    for link in links {
        detach(
            "$link WHERE source = $blueprint",
            Axis::Course,
            &[
                ("link".into(), link.into_value()),
                ("blueprint".into(), id.record().into_value()),
            ],
            db,
        )
        .await?;
    }
    Ok(())
}
