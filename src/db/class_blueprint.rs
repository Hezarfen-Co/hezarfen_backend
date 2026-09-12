//! The `class_blueprint` table: the row keyed by its grade label, the
//! compare-and-set writes the edit and the delete go through, and the reads
//! the sweep and the status screen are built on. The workflows that sequence
//! these — and the row lock that serializes a delete against the attaches
//! made on the blueprint's behalf — live in [`crate::service::class_blueprint`].

use sqlx::postgres::PgConnection;

use crate::constant::CLASS_BLUEPRINT_TABLE;
use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::class_pump;
use crate::db::page::PagedList;
use crate::domain::class_blueprint::{ClassBlueprint, ClassBlueprintId};
use crate::domain::class_group::{ClassGrade, ClassGroupId};
use crate::domain::course::CourseId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// One `class_course` row, projected down to the pair that answers "does this
/// section carry that course". `source` is deliberately not read: a link of any
#[derive(Debug, sqlx::FromRow)]
pub struct Held {
    pub(crate) class: ClassGroupId,
    pub(crate) course: CourseId,
}

/// Prove every course a template is about to name is still there, *inside* the
/// transaction that stores the list.
///
/// The caller's own pre-flight read is not that proof: it is a read of
/// `course` in a request that then writes `class_blueprint`, and two
/// transactions that read and write crossed records interleave freely under
/// READ COMMITTED. `DELETE /courses/{id}` sweeps every template naming the
/// course as part of its cascade, so a delete landing between the read and the
/// write sweeps a row that is not there yet and the list keeps an id nothing
/// points at. A `FOR KEY SHARE` row lock per course is the proof: it is the
/// weakest strength a course `DELETE` cannot take, so the delete waits behind
/// this transaction and finds the list that names the course already stored —
/// and sweeps it.
///
/// One lock per course, the list being at most `MAX_CLASS_COURSES` long —
/// the same pass-per-course the old in-transaction loop made.
async fn courses_alive(tx: &mut PgConnection, courses: &[CourseId]) -> Result<(), AppError> {
    for course in courses {
        let alive = sqlx::query_scalar!(
            r#"SELECT 1 AS "one" FROM course WHERE id = $1 FOR KEY SHARE"#,
            course as _
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !alive {
            return Err(no_such_course());
        }
    }
    Ok(())
}

/// The refusal a locked course read answers when the course is gone: the
/// same `400` the caller's pre-flight read raises, since it is the same fact
/// one instant later.
fn no_such_course() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "course_ids",
        reason: "one of these courses does not exist",
    })
}

/// Write the blueprint's row. A second one for the same grade is a 409 the
/// store itself decides — the grade is the primary key, so the duplicate is
/// seen rather than raced (`23505` on `class_blueprint_pkey`, mapped right
/// here: a duplicate is a decision, never a retry).
pub async fn create(
    db: &Database,
    creator: &UserId,
    grade: ClassGrade,
    courses: Vec<CourseId>,
) -> Result<ClassBlueprint, AppError> {
    let blueprint = ClassBlueprint {
        id: ClassBlueprintId::for_grade(&grade),
        grade: grade.clone(),
        courses: courses.clone(),
        creator: creator.clone(),
    };
    let creator = *creator;
    tx_with_retry(db, false, async move |tx| {
        courses_alive(tx, &courses).await?;
        let inserted = sqlx::query!(
            r#"INSERT INTO class_blueprint (grade, courses, creator)
               VALUES ($1, $2, $3)"#,
            grade as _,
            courses as _,
            creator as _
        )
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => Ok(blueprint.clone()),
            Err(e) if unique_violation(&e) == Some("class_blueprint_pkey") => Err(
                AppError::Conflict("a blueprint already exists for that grade"),
            ),
            Err(e) => Err(e.into()),
        }
    })
    .await
}

pub async fn read(
    db: &Database,
    id: &ClassBlueprintId,
) -> Result<Option<ClassBlueprint>, AppError> {
    let row = sqlx::query!(
        r#"SELECT grade AS "id: ClassBlueprintId", grade AS "grade: ClassGrade",
                  courses AS "courses: Vec<CourseId>", creator AS "creator: UserId"
           FROM class_blueprint WHERE grade = $1"#,
        id as _
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| ClassBlueprint {
        id: row.id,
        grade: row.grade,
        courses: row.courses,
        creator: row.creator,
    }))
}

/// Every blueprint, by grade label — the id *is* the label, so this is the
/// only ordering that means anything here.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassBlueprint>, i64), AppError> {
    PagedList::new(CLASS_BLUEPRINT_TABLE, "ORDER BY grade ASC")
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
/// A template naming a course that is gone refuses inside the transaction
/// with the pre-flight `400` ([`courses_alive`]).
pub async fn set_courses_if_unchanged(
    db: &Database,
    id: &ClassBlueprintId,
    held: Vec<CourseId>,
    wanted: Vec<CourseId>,
) -> Result<Option<ClassBlueprint>, AppError> {
    let id = id.clone();
    tx_with_retry(db, false, async move |tx| {
        courses_alive(tx, &wanted).await?;
        let updated = sqlx::query!(
            r#"UPDATE class_blueprint SET courses = $2
               WHERE grade = $1 AND courses = $3
               RETURNING grade AS "grade: ClassGrade", creator AS "creator: UserId""#,
            id as _,
            wanted as _,
            held as _
        )
        .fetch_optional(&mut *tx)
        .await?;
        Ok(updated.map(|row| ClassBlueprint {
            id: id.clone(),
            grade: row.grade,
            courses: wanted.clone(),
            creator: row.creator,
        }))
    })
    .await
}

/// Delete the row, but only while it still carries the list this caller read
/// — the edit race answers the caller, not a silent delete of somebody
/// else's additions. `false` means the conditional delete matched nothing:
/// the row is gone, or its list moved since the caller read it, and only the
/// caller can tell those apart.
///
/// The `DELETE` is also the whole of the serialization against the attaches
/// made on this blueprint's behalf: a sourced attach holds a `FOR KEY SHARE`
/// on this row across its transaction ([`class_pump::attach_course`]), so its
/// row lands before this delete's sweep runs or not at all — the invariant the
/// old process-wide write lease held, now on the row itself.
pub async fn delete_if_unchanged(
    db: &Database,
    id: &ClassBlueprintId,
    held: Vec<CourseId>,
) -> Result<bool, AppError> {
    let deleted = sqlx::query!(
        r#"DELETE FROM class_blueprint WHERE grade = $1 AND courses = $2"#,
        id as _,
        held as _
    )
    .execute(db)
    .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Every `class_course` row held by any of `classes`, projected down to the
/// pair — the read [`crate::service::class_blueprint::status`] diffs the
/// template against.
pub async fn held_links(db: &Database, classes: Vec<ClassGroupId>) -> Result<Vec<Held>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT class AS "class: ClassGroupId", course AS "course: CourseId"
           FROM class_course WHERE class = ANY($1)"#,
        classes as _
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Held {
            class: row.class,
            course: row.course,
        })
        .collect())
}

/// Drop a course that no longer exists out of a blueprint's list.
///
/// The delete does this itself now: the course's own cascade sweeps
/// `class_blueprint` in the same transaction that takes the course's
/// `class_course` links, so a template stops naming a course the instant that
/// course goes. This used to be the *only* thing that could remove such an id,
/// and that was the bug: it fires only while walking a section, so a grade
/// with no sections could never reach it, and the repair every doc surface
/// pointed at (`PATCH` the list back as it stands) is a `400` for naming a
/// course that does not exist. A permanent dangling id, and no call that could
/// clear it.
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
pub async fn prune(
    db: &Database,
    id: &ClassBlueprintId,
    course: &CourseId,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"UPDATE class_blueprint SET courses = array_remove(courses, $2) WHERE grade = $1"#,
        id as _,
        course as _
    )
    .execute(db)
    .await?;
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
/// `source` is the whole filter, so a row without the tag — a hand attach —
/// is never matched, and a class that acquired the same course by hand keeps
/// it. The rows are read rather than derived from the classes at this grade:
/// a class whose grade was edited after the pump still carries this
/// blueprint's attachments, and only the tag can find it.
pub async fn sourced_links(
    db: &Database,
    id: &ClassBlueprintId,
    keep: &[CourseId],
) -> Result<Vec<Held>, AppError> {
    let keep: Vec<CourseId> = keep.to_vec();
    let rows = sqlx::query!(
        r#"SELECT class AS "class: ClassGroupId", course AS "course: CourseId"
           FROM class_course
           WHERE source = $1 AND course <> ALL($2)"#,
        id as _,
        keep as _
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Held {
            class: row.class,
            course: row.course,
        })
        .collect())
}

/// Detach the link rows [`sourced_links`] named and sweep the enrollments
/// they pumped. The sweep underneath is the pump's own, so a student a
/// second class still claims is re-tagged rather than unenrolled.
///
/// Each detach re-asserts the tag: the link's identity is the (class, course)
/// pair and says nothing about provenance, so a row a *new* blueprint at the
/// same grade attached while this ran would otherwise be swept by the old
/// one's tail.
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
    links: Vec<Held>,
) -> Result<(), AppError> {
    for link in links {
        class_pump::detach_course(db, &link.class, &link.course, Some(id)).await?;
    }
    Ok(())
}
