//! The `course_offering` table: the grade-level course template and the reads
//! and writes behind `/offerings`. The row shape lives in
//! [`crate::domain::course_offering`]; the workflows (the class-delivered
//! gate, the duplicate refusal) in [`crate::service::course_offering`].
//!
//! One row per (course, grade_level) — the `UNIQUE` key the attach pump's
//! auto-create races on: [`ensure_tx`] inserts `ON CONFLICT DO NOTHING` and
//! reads the winner's id back, so two attaches landing together share one
//! template whatever the interleaving was.
//!
//! Every content column is nullable by design — `NULL` means *inherit* (from
//! the catalog course, or from the constants), never "missing". The update is
//! field-scoped in the `set_or_clear` shape the other PATCH handlers speak:
//! an absent field keeps its column, an explicit `null` clears back to
//! inherit, a value sets the override.

use sqlx::postgres::PgConnection;

use crate::database::{Database, unique_violation};
use crate::db::page::PagedList;
use crate::domain::class_course::DersSaati;
use crate::domain::course::{CourseDescription, CourseId, CourseTitle};
use crate::domain::course_offering::{CourseOffering, CourseOfferingId};
use crate::domain::grade::GradeLevel;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// One offering as the *runtime* row decode sees it. Identical to
/// [`CourseOffering`] except `default_ders_saati` stays the raw `SMALLINT`
/// (`Option<i16>`): the runtime [`sqlx::FromRow`] path cannot name the
/// `::bigint` cast the static `query_as!` projections use, and
/// [`DersSaati`]'s transparent `i64` would refuse a `SMALLINT` at decode.
/// [`OfferingRow::into_course_offering`] does the two-step conversions the
/// column `CHECK`s already guarantee.
#[derive(Debug, sqlx::FromRow)]
struct OfferingRow {
    id: CourseOfferingId,
    course: CourseId,
    grade_level: GradeLevel,
    title: Option<CourseTitle>,
    description: Option<CourseDescription>,
    default_ders_saati: Option<i16>,
    default_counts_toward_karne: Option<bool>,
    created_by: UserId,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl OfferingRow {
    fn into_course_offering(self) -> CourseOffering {
        CourseOffering {
            id: self.id,
            course: self.course,
            grade_level: self.grade_level,
            title: self.title,
            description: self.description,
            default_ders_saati: self.default_ders_saati.map(|hours| {
                DersSaati::try_new(i64::from(hours))
                    .expect("the column CHECK keeps the weekly hours between 1 and 40")
            }),
            default_counts_toward_karne: self.default_counts_toward_karne,
            created_by: self.created_by,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// One offering by id, or `None` when the id names no row.
pub async fn read(db: &Database, id: &CourseOfferingId) -> Result<Option<CourseOffering>, AppError> {
    let row = sqlx::query_as!(
        CourseOffering,
        r#"SELECT id AS "id: CourseOfferingId",
                  course AS "course: CourseId",
                  grade_level AS "grade_level: GradeLevel",
                  title AS "title?: CourseTitle",
                  description AS "description?: CourseDescription",
                  default_ders_saati::bigint AS "default_ders_saati?: DersSaati",
                  default_counts_toward_karne,
                  created_by AS "created_by: UserId",
                  created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
           FROM course_offering WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Load every offering behind `ids` (one query) — the batch half of the
/// resolved-content reads (`service::instance_resolve::resolved_content`),
/// which map a page of instances onto their templates without an N+1.
pub async fn list_by_ids(
    db: &Database,
    ids: &[CourseOfferingId],
) -> Result<Vec<CourseOffering>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<uuid::Uuid> = ids.iter().map(CourseOfferingId::uuid).collect();
    let rows = sqlx::query_as!(
        CourseOffering,
        r#"SELECT id AS "id: CourseOfferingId",
                  course AS "course: CourseId",
                  grade_level AS "grade_level: GradeLevel",
                  title AS "title?: CourseTitle",
                  description AS "description?: CourseDescription",
                  default_ders_saati::bigint AS "default_ders_saati?: DersSaati",
                  default_counts_toward_karne,
                  created_by AS "created_by: UserId",
                  created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
           FROM course_offering WHERE id = ANY($1)"#,
        &keys,
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The offering a (course, grade_level) pair resolves to, or `None` when no
/// template exists there yet — the read behind "what does this grade teach
/// from" outside a pump transaction.
pub async fn find(
    db: &Database,
    course: &CourseId,
    grade: GradeLevel,
) -> Result<Option<CourseOffering>, AppError> {
    let row = sqlx::query_as!(
        CourseOffering,
        r#"SELECT id AS "id: CourseOfferingId",
                  course AS "course: CourseId",
                  grade_level AS "grade_level: GradeLevel",
                  title AS "title?: CourseTitle",
                  description AS "description?: CourseDescription",
                  default_ders_saati::bigint AS "default_ders_saati?: DersSaati",
                  default_counts_toward_karne,
                  created_by AS "created_by: UserId",
                  created_at AS "created_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
           FROM course_offering WHERE course = $1 AND grade_level = $2"#,
        course.uuid(),
        grade.get(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// The offerings, grade ladder first, paged via `limit`/`offset`; both
/// filters are optional and combine. The page and its count share the same
/// predicate ([`PagedList`]).
pub async fn list(
    db: &Database,
    course: Option<&CourseId>,
    grade: Option<GradeLevel>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseOffering>, i64), AppError> {
    let from_where = match (course, grade) {
        (Some(_), Some(_)) => "course_offering WHERE course = $1 AND grade_level = $2",
        (Some(_), None) => "course_offering WHERE course = $1",
        (None, Some(_)) => "course_offering WHERE grade_level = $1",
        (None, None) => "course_offering",
    };
    let mut page = PagedList::new(from_where, "ORDER BY grade_level, course");
    if let Some(course) = course {
        page = page.bind(course.uuid());
    }
    if let Some(grade) = grade {
        page = page.bind(i64::from(grade.get()));
    }
    let (rows, total) = page.run::<OfferingRow>(limit, offset, db).await?;
    Ok((
        rows.into_iter()
            .map(OfferingRow::into_course_offering)
            .collect(),
        total,
    ))
}

/// Mint the template. The duplicate (course, grade_level) answer is a **409
/// `offering_exists`**, not a store error: a manager creating the same
/// template twice wants to be told it exists, and the attach pump's
/// auto-create would have silently shared the row instead.
#[allow(clippy::too_many_arguments)] // the offering row, spelled field by field
pub async fn create(
    db: &Database,
    course: &CourseId,
    grade: GradeLevel,
    title: Option<CourseTitle>,
    description: Option<CourseDescription>,
    default_ders_saati: Option<DersSaati>,
    default_counts_toward_karne: Option<bool>,
    by: &UserId,
) -> Result<CourseOffering, AppError> {
    let now = Timestamp::now();
    let title = title.map(|title| title.as_str().to_string());
    let description = description.map(|d| d.as_str().to_string());
    // An `i64` bound into a `SMALLINT` column: the assignment cast is the
    // statement's own, and the newtype has already range-checked the value.
    let hours = default_ders_saati.map(DersSaati::as_i64);
    let row = sqlx::query_as!(
        CourseOffering,
        r#"INSERT INTO course_offering
               (id, course, grade_level, title, description,
                default_ders_saati, default_counts_toward_karne,
                created_by, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6::bigint, $7, $8, $9, $9)
           RETURNING id AS "id: CourseOfferingId",
                     course AS "course: CourseId",
                     grade_level AS "grade_level: GradeLevel",
                     title AS "title?: CourseTitle",
                     description AS "description?: CourseDescription",
                     default_ders_saati::bigint AS "default_ders_saati?: DersSaati",
                     default_counts_toward_karne,
                     created_by AS "created_by: UserId",
                     created_at AS "created_at: Timestamp",
                     updated_at AS "updated_at: Timestamp""#,
        CourseOfferingId::generate().uuid(),
        course.uuid(),
        grade.get(),
        title,
        description,
        hours,
        default_counts_toward_karne,
        by.uuid(),
        now.as_millis(),
    )
    .fetch_one(db)
    .await
    .map_err(|e| {
        // The table carries exactly one `UNIQUE` constraint — the (course,
        // grade_level) template key — so *any* unique violation here is the
        // duplicate template, whatever Postgres named it.
        if unique_violation(&e).is_some() {
            AppError::ConflictCoded {
                code: "offering_exists",
                message: "this course already has an offering for this grade".into(),
            }
        } else {
            e.into()
        }
    })?;
    Ok(row)
}

/// Field-scoped PATCH in the `set_or_clear` shape: an absent field keeps its
/// column, `Some(None)` clears back to inherit (`NULL`), `Some(Some(v))` sets
/// the override. `updated_at` moves with every write that changes anything.
/// An empty request writes nothing and reads the row back. `Err(NotFound)`
/// when the offering is gone.
pub async fn update(
    db: &Database,
    id: &CourseOfferingId,
    title: Option<Option<CourseTitle>>,
    description: Option<Option<CourseDescription>>,
    default_ders_saati: Option<Option<DersSaati>>,
    default_counts_toward_karne: Option<Option<bool>>,
) -> Result<CourseOffering, AppError> {
    if title.is_none()
        && description.is_none()
        && default_ders_saati.is_none()
        && default_counts_toward_karne.is_none()
    {
        return read(db, id).await?.ok_or(AppError::NotFound);
    }
    // Present-flag + nullable-value pairs: the flag decides *whether* the
    // column moves, the value decides to what (a typed `NULL` is the inherit
    // state). The weekly hours ride the `::bigint` round-trip cast like every
    // other `DersSaati` read, and the column `CHECK` re-checks the range at
    // the store.
    let title_present = title.is_some();
    let title = title.flatten().map(|t| t.as_str().to_string());
    let description_present = description.is_some();
    let description = description.flatten().map(|d| d.as_str().to_string());
    let hours_present = default_ders_saati.is_some();
    let hours = default_ders_saati.flatten().map(DersSaati::as_i64);
    let counts_present = default_counts_toward_karne.is_some();
    let counts = default_counts_toward_karne.flatten();
    let now = Timestamp::now();
    let row = sqlx::query_as!(
        CourseOffering,
        r#"WITH updated AS (
               UPDATE course_offering SET
                   title = CASE WHEN $2 THEN $3::text ELSE title END,
                   description = CASE WHEN $4 THEN $5::text ELSE description END,
                   default_ders_saati = CASE WHEN $6 THEN
                       COALESCE($7::bigint, default_ders_saati::bigint)::smallint
                       ELSE default_ders_saati END,
                   default_counts_toward_karne = CASE WHEN $8 THEN $9
                       ELSE default_counts_toward_karne END,
                   updated_at = $10
               WHERE id = $1
               RETURNING id, course, grade_level, title, description,
                         default_ders_saati, default_counts_toward_karne,
                         created_by, created_at, updated_at)
           SELECT u.id AS "id: CourseOfferingId",
                  u.course AS "course: CourseId",
                  u.grade_level AS "grade_level: GradeLevel",
                  u.title AS "title?: CourseTitle",
                  u.description AS "description?: CourseDescription",
                  u.default_ders_saati::bigint AS "default_ders_saati?: DersSaati",
                  u.default_counts_toward_karne,
                  u.created_by AS "created_by: UserId",
                  u.created_at AS "created_at: Timestamp",
                  u.updated_at AS "updated_at: Timestamp"
           FROM updated u"#,
        id.uuid(),
        title_present,
        title,
        description_present,
        description,
        hours_present,
        hours,
        counts_present,
        counts,
        now.as_millis(),
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(row)
}

/// The offering's two defaults, straight off its row — `(default_ders_saati,
/// default_counts_toward_karne)`, `None`s *included* (a `NULL` default is the
/// inherit state, and the caller layers its own fallbacks on top). `None` when
/// the id names no offering: the `RESTRICT` foreign key on `class_course`
/// keeps every instance's offering alive, so a missing row is the honest 404
/// the service turns into one, not a case to paper over.
pub async fn defaults(
    db: &Database,
    id: &CourseOfferingId,
) -> Result<Option<(Option<DersSaati>, Option<bool>)>, AppError> {
    let row = sqlx::query!(
        r#"SELECT default_ders_saati::bigint AS "default_ders_saati?: DersSaati",
                  default_counts_toward_karne
           FROM course_offering WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| (row.default_ders_saati, row.default_counts_toward_karne)))
}

/// Delete the template — refused while any instance still teaches from it.
/// The refusal is a **409 `offering_in_use`** (the foreign key's `RESTRICT`
/// is the store's mirror of the same rule); a gone id is the usual 404. One
/// conditional statement is the whole invariant, so a check-then-delete race
/// cannot exist: the `NOT EXISTS` is judged on the row as it is *at the
/// delete*.
pub async fn delete(db: &Database, id: &CourseOfferingId) -> Result<(), AppError> {
    let deleted = sqlx::query!(
        r#"DELETE FROM course_offering
           WHERE id = $1
             AND NOT EXISTS (SELECT 1 FROM class_course cc WHERE cc.offering = $1)"#,
        id.uuid(),
    )
    .execute(db)
    .await?
    .rows_affected();
    if deleted > 0 {
        return Ok(());
    }
    // Zero rows: in use, or already gone. The read tells the two apart.
    let standing = sqlx::query_scalar!(
        r#"SELECT 1 AS "one" FROM course_offering WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?
    .is_some();
    if standing {
        Err(AppError::ConflictCoded {
            code: "offering_in_use",
            message:
                "instances still teach this offering — detach the course from those classes first"
                    .into(),
        })
    } else {
        Err(AppError::NotFound)
    }
}

/// Resolve — or mint empty — the template a (course, grade_level) pair
/// teaches from, inside the caller's transaction. The attach pump's helper:
/// every instance insert needs its offering link, and an untouched grade
/// mints an *empty* template (every content column `NULL`) so it inherits the
/// catalog course until a manager fills it in. `created_by` is the actor
/// whose attach asked.
///
/// The `ON CONFLICT DO NOTHING` + read-back pair is the race-closer: two
/// attaches landing on the same (course, grade_level) together both insert,
/// one wins, and the loser's read-back returns the winner's row — one
/// template per pair, whatever the order was. The table's only `UNIQUE`
/// constraint is that pair, so the bare `DO NOTHING` cannot swallow a
/// different conflict.
pub(crate) async fn ensure_tx(
    tx: &mut PgConnection,
    course: &CourseId,
    grade: GradeLevel,
    by: &UserId,
) -> Result<CourseOfferingId, AppError> {
    let now = Timestamp::now();
    sqlx::query!(
        r#"INSERT INTO course_offering (id, course, grade_level, created_by, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $5)
           ON CONFLICT DO NOTHING"#,
        CourseOfferingId::generate().uuid(),
        course.uuid(),
        grade.get(),
        by.uuid(),
        now.as_millis(),
    )
    .execute(&mut *tx)
    .await?;
    let id = sqlx::query_scalar!(
        r#"SELECT id AS "id: CourseOfferingId" FROM course_offering
           WHERE course = $1 AND grade_level = $2"#,
        course.uuid(),
        grade.get(),
    )
    .fetch_one(&mut *tx)
    .await?;
    Ok(id)
}
