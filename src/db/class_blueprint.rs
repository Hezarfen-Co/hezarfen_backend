//! The `class_blueprint` table: the row keyed by its grade label, the
//! compare-and-set writes the edit and the delete go through, and the reads
//! the sweep and the status screen are built on. The workflows that sequence
//! these — and the row lock that serializes a delete against the attaches
//! made on the blueprint's behalf — live in [`crate::service::class_blueprint`].

use sqlx::postgres::PgConnection;

use std::collections::HashMap;

use crate::constant::CLASS_BLUEPRINT_TABLE;
use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::class_pump;
use crate::db::page::PagedList;
use crate::domain::class_blueprint::{ClassBlueprint, ClassBlueprintId};
use crate::domain::class_group::{ClassGrade, ClassGroupId};
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The stored columns of one `class_blueprint` row. A template's course list
/// is *not* one of them — it lives across the `blueprint_course` junction and
/// is joined on in a batch after the read ([`with_courses`]), because the
/// paged list here reads whole rows (`SELECT *`), a shape no aggregate column
/// can ride.
#[derive(Debug, sqlx::FromRow)]
struct BlueprintRow {
    /// The surrogate uuid primary key — the key the junction rows reference.
    #[sqlx(rename = "id")]
    key: uuid::Uuid,
    grade: ClassGrade,
    creator: UserId,
}

impl BlueprintRow {
    fn into_blueprint(self, courses: Vec<CourseId>) -> ClassBlueprint {
        ClassBlueprint {
            // The API identity is the grade label, which the key column also
            // stores — the id *is* the grade.
            id: ClassBlueprintId::for_grade(&self.grade),
            grade: self.grade,
            courses,
            creator: self.creator,
        }
    }
}

/// The stored course list of one blueprint, sorted by course. Every read goes
/// through this ordering, which is what makes the compare-and-set below a
/// plain vector comparison: both sides of it are junction reads.
async fn junction_courses<'e, E>(
    executor: E,
    blueprint: uuid::Uuid,
) -> Result<Vec<CourseId>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows = sqlx::query!(
        r#"SELECT course AS "course: CourseId" FROM blueprint_course
           WHERE blueprint = $1 ORDER BY course"#,
        blueprint,
    )
    .fetch_all(executor)
    .await?;
    Ok(rows.into_iter().map(|row| row.course).collect())
}

/// Join the stored course lists onto a batch of blueprint rows, one junction
/// read for the lot.
async fn with_courses(
    db: &Database,
    rows: Vec<BlueprintRow>,
) -> Result<Vec<ClassBlueprint>, AppError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<uuid::Uuid> = rows.iter().map(|row| row.key).collect();
    let links = sqlx::query!(
        r#"SELECT blueprint, course AS "course: CourseId" FROM blueprint_course
           WHERE blueprint = ANY($1) ORDER BY blueprint, course"#,
        &keys,
    )
    .fetch_all(db)
    .await?;
    let mut by_blueprint: HashMap<uuid::Uuid, Vec<CourseId>> = HashMap::new();
    for link in links {
        by_blueprint
            .entry(link.blueprint)
            .or_default()
            .push(link.course);
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            let courses = by_blueprint.remove(&row.key).unwrap_or_default();
            row.into_blueprint(courses)
        })
        .collect())
}

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

/// Write the blueprint's row — and its `blueprint_course` rows, in the same
/// transaction, under the same per-course proof ([`courses_alive`]). A second
/// blueprint for the same grade is a 409 the store itself decides — the grade
/// carries a UNIQUE constraint, so the duplicate is seen rather than raced
/// (`23505` on `class_blueprint_grade_key`, mapped right here: a duplicate is
/// a decision, never a retry). The surrogate `id` is minted here and never
/// read back: the grade label stays the only identity the API speaks.
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
        creator: *creator,
    };
    let creator = *creator;
    let id = next_uuid();
    tx_with_retry(db, false, async move |tx| {
        courses_alive(tx, &courses).await?;
        let inserted = sqlx::query!(
            r#"INSERT INTO class_blueprint (id, grade, creator)
               VALUES ($1, $2, $3)"#,
            id,
            grade as _,
            creator as _
        )
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {
                sqlx::query!(
                    r#"INSERT INTO blueprint_course (blueprint, course)
                       SELECT $1, x FROM unnest($2::uuid[]) AS t(x)
                       ON CONFLICT DO NOTHING"#,
                    id,
                    courses as _,
                )
                .execute(&mut *tx)
                .await?;
                Ok(blueprint.clone())
            }
            Err(e) if unique_violation(&e) == Some("class_blueprint_grade_key") => Err(
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
    let row = sqlx::query_as!(
        BlueprintRow,
        r#"SELECT id AS "key: uuid::Uuid", grade AS "grade: ClassGrade",
                  creator AS "creator: UserId"
           FROM class_blueprint WHERE grade = $1"#,
        id as _
    )
    .fetch_optional(db)
    .await?;
    match row {
        Some(row) => {
            let courses = junction_courses(db, row.key).await?;
            Ok(Some(row.into_blueprint(courses)))
        }
        None => Ok(None),
    }
}

/// Every blueprint, by grade label — the id *is* the label, so this is the
/// only ordering that means anything here.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassBlueprint>, i64), AppError> {
    let (rows, total) = PagedList::new(CLASS_BLUEPRINT_TABLE, "ORDER BY grade ASC")
        .run::<BlueprintRow>(limit, offset, db)
        .await?;
    Ok((with_courses(db, rows).await?, total))
}

/// Write the course list only while the stored rows still answer to the list
/// this caller read: two managers editing the same grade cannot have one's
/// list silently pump the other's diff. `None` means the claim matched
/// nothing — the row is gone, or its list moved since the caller read it, and
/// only the caller can tell those apart (the workflow re-reads and answers a
/// `409` or a `404`).
///
/// The write is a claim (`FOR UPDATE` on the row), a comparison (the stored
/// `blueprint_course` rows, read under that lock, against `held` — both
/// sorted the same way, so the comparison is a plain vector equality), then
/// the diff: rows `wanted` dropped are deleted, rows it added are inserted.
/// All of it rides one transaction, and the row lock it holds is the same
/// strength the blueprint delete takes, so a pump's `FOR KEY SHARE` claim on
/// this row waits out the whole write exactly as it waits out a delete.
///
/// A template naming a course that is gone refuses inside the transaction
/// with the pre-flight `400` ([`courses_alive`]).
pub async fn set_courses_if_unchanged(
    db: &Database,
    id: &ClassBlueprintId,
    held: Vec<CourseId>,
    wanted: Vec<CourseId>,
) -> Result<Option<ClassBlueprint>, AppError> {
    let grade = id.clone();
    let mut held = held;
    held.sort_by_key(|course| course.uuid());
    let keys: Vec<uuid::Uuid> = wanted.iter().map(CourseId::uuid).collect();
    tx_with_retry(db, false, async move |tx| {
        courses_alive(tx, &wanted).await?;
        let row = sqlx::query!(
            r#"SELECT id AS "key: uuid::Uuid", grade AS "grade: ClassGrade",
                      creator AS "creator: UserId"
               FROM class_blueprint WHERE grade = $1 FOR UPDATE"#,
            grade.key(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored = junction_courses(&mut *tx, row.key).await?;
        if stored != held {
            return Ok(None);
        }
        // The diff, from the list this caller stored: rows it dropped go,
        // rows it added come (a duplicate in `wanted` is one row — the
        // workflow deduplicates, and the key does the rest).
        sqlx::query!(
            r#"DELETE FROM blueprint_course
               WHERE blueprint = $1 AND NOT (course = ANY($2))"#,
            row.key,
            &keys,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"INSERT INTO blueprint_course (blueprint, course)
               SELECT $1, x FROM unnest($2::uuid[]) AS t(x)
               ON CONFLICT DO NOTHING"#,
            row.key,
            &keys,
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(ClassBlueprint {
            id: ClassBlueprintId::for_grade(&row.grade),
            grade: row.grade,
            courses: wanted.clone(),
            creator: row.creator,
        }))
    })
    .await
}

/// Delete the row — but only while its stored list is still the one this
/// caller read; the edit race answers the caller, not a silent delete of
/// somebody else's additions. `false` means the claim matched nothing: the
/// row is gone, or its list moved since the caller read it, and only the
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
    let grade = id.clone();
    tx_with_retry(db, false, async move |tx| {
        delete_claimed(&mut *tx, &grade, &held).await
    })
    .await
}

/// [`delete_if_unchanged`] inside the caller's transaction — the blueprint
/// delete rides the row lock its claim took, so the sourced links are swept
/// while the window the claim opened is still closed. The claim and the
/// comparison re-run here: in the delete workflow's own transaction the
/// [`held_courses_for_update`] claim has already settled both, so they are a
/// formality; the standalone path above gets its whole CAS from this one
/// body.
pub(crate) async fn delete_if_unchanged_in(
    tx: &mut sqlx::PgConnection,
    id: &ClassBlueprintId,
    held: &[CourseId],
) -> Result<bool, AppError> {
    delete_claimed(tx, id, held).await
}

/// The delete's core: claim the row, compare the stored junction rows with
/// `held`, and take the junction rows before the blueprint row they
/// reference — the `NO ACTION` foreign key is checked the moment the final
/// `DELETE` runs.
async fn delete_claimed(
    tx: &mut PgConnection,
    id: &ClassBlueprintId,
    held: &[CourseId],
) -> Result<bool, AppError> {
    let row = sqlx::query!(
        r#"SELECT id AS "key: uuid::Uuid" FROM class_blueprint
           WHERE grade = $1 FOR UPDATE"#,
        id as _,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let mut held = held.to_vec();
    held.sort_by_key(|course| course.uuid());
    let stored = junction_courses(&mut *tx, row.key).await?;
    if stored != held {
        return Ok(false);
    }
    sqlx::query!(
        r#"DELETE FROM blueprint_course WHERE blueprint = $1"#,
        row.key
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(r#"DELETE FROM class_blueprint WHERE id = $1"#, row.key)
        .execute(&mut *tx)
        .await?;
    Ok(true)
}

/// The stored course list of one blueprint, read `FOR UPDATE` — the claim
/// the delete holds across its sweep, so a sourced attach (whose own
/// transaction takes `FOR KEY SHARE` on this row before inserting) either
/// commits before the claim and is swept, or waits past it and finds no
/// row. `None` when the blueprint is gone.
pub(crate) async fn held_courses_for_update(
    tx: &mut sqlx::PgConnection,
    id: &ClassBlueprintId,
) -> Result<Option<Vec<CourseId>>, AppError> {
    let row = sqlx::query!(
        r#"SELECT id AS "key: uuid::Uuid" FROM class_blueprint
           WHERE grade = $1 FOR UPDATE"#,
        id as _,
    )
    .fetch_optional(&mut *tx)
    .await?;
    match row {
        Some(row) => Ok(Some(junction_courses(&mut *tx, row.key).await?)),
        None => Ok(None),
    }
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
/// The delete does this itself now — twice over: the course's own cascade
/// sweeps `blueprint_course` (the junction row is a foreign key now, so a
/// template physically cannot keep naming a course whose row is gone) in the
/// same transaction that takes the course's `class_course` links. This used
/// to be the *only* thing that could remove such an id, and that was the bug:
/// it fires only while walking a section, so a grade with no sections could
/// never reach it, and the repair every doc surface pointed at (`PATCH` the
/// list back as it stands) is a `400` for naming a course that does not
/// exist. A permanent dangling id, and no call that could clear it.
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
        r#"DELETE FROM blueprint_course
           WHERE course = $2
             AND blueprint = (SELECT id FROM class_blueprint WHERE grade = $1)"#,
        id as _,
        course as _,
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
///
/// The stored tag is the blueprint's surrogate uuid, not the grade label the
/// API speaks — the query translates the label to it.
pub async fn sourced_links(
    db: &Database,
    id: &ClassBlueprintId,
    keep: &[CourseId],
) -> Result<Vec<Held>, AppError> {
    let keep: Vec<CourseId> = keep.to_vec();
    let rows = sqlx::query!(
        r#"SELECT class AS "class: ClassGroupId", course AS "course: CourseId"
           FROM class_course
           WHERE source = (SELECT id FROM class_blueprint WHERE grade = $1)
             AND course <> ALL($2)"#,
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
///
/// Answers the **blob keys** every detach's cascade removed — the
/// question/answer images and homework files of each swept instance subtree —
/// for the caller's route to unlink after the commits. Each detach commits its
/// own transaction, so the keys of the rows already swept come back even when
/// a later link refuses ([`crate::db::class_pump::detach_course`] answers
/// `None` for a pair that held no instance, which contributes nothing); a
/// failure before a link's detach returns the Err and drops the keys gathered
/// so far, which is the same accepted window
/// [`crate::service::course::delete`] documents: at worst an unreachable blob.
pub async fn drop_links(
    db: &Database,
    id: &ClassBlueprintId,
    links: Vec<Held>,
) -> Result<Vec<String>, AppError> {
    let mut blobs = Vec::new();
    for link in links {
        if let Some(keys) =
            class_pump::detach_course(db, &link.class, &link.course, Some(id)).await?
        {
            blobs.extend(keys);
        }
    }
    Ok(blobs)
}
