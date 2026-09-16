//! Academic years (eğitim yılı) — the top of the calendar.
//!
//! A year is the container a şube and its dönemler belong to, and the one that
//! carries the sınıf-geçme policy (`grade_promotions`). A grade absent from that
//! policy is not carried over by [`rollover`](crate::service::academic_year::rollover)
//! — that is how graduation is expressed — so the command is explicit and
//! idempotent rather than automatic at a date.
//!
//! A year with structure still on it cannot be deleted (`409`), and an archived
//! year is read-only: no new şube, no new dönem, no new exam inside it.

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::academic_year::{AcademicYear, AcademicYearName, GradePromotion};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::state::AppState;

use super::{Page, PageParams, RequireManager, RequireTeacher, check_time_range};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_year, list_years))
        .routes(routes!(get_year, update_year, delete_year))
        .routes(routes!(rollover))
}

#[derive(Deserialize, ToSchema)]
struct CreateYear {
    #[schema(example = "2026-2027", max_length = 100)]
    name: String,
    /// Year start, UTC unix-milliseconds. May lie in the past — a school
    /// adopting the app mid-year backfills its calendar legitimately.
    #[schema(example = 1_780_000_000_000_i64)]
    starts_at: i64,
    /// Year end, UTC unix-milliseconds; must be after `starts_at`.
    #[schema(example = 1_810_000_000_000_i64)]
    ends_at: i64,
    /// The sınıf-geçme policy: each pair maps a grade label to the label its
    /// students move to at `POST /academic-years/{id}/rollover`. A grade the
    /// list does not name is not rolled over — it graduates. Omit for a year
    /// whose rollover is not decided yet.
    grade_promotions: Option<Vec<PromotionBody>>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateYear {
    #[schema(max_length = 100)]
    name: Option<String>,
    starts_at: Option<i64>,
    ends_at: Option<i64>,
    /// The whole new policy — a *set*, not a delta: a grade missing from it is
    /// no longer promoted at the next rollover.
    grade_promotions: Option<Vec<PromotionBody>>,
}

/// One sınıf-geçme pair off the wire.
#[derive(Deserialize, Serialize, ToSchema)]
struct PromotionBody {
    #[schema(max_length = 20, example = "5")]
    from_grade: String,
    #[schema(max_length = 20, example = "6")]
    to_grade: String,
}

impl PromotionBody {
    fn parse(&self) -> Result<GradePromotion, AppError> {
        Ok(GradePromotion::try_new(&self.from_grade, &self.to_grade)?)
    }
}

#[derive(Serialize, ToSchema)]
struct YearResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    id: String,
    #[schema(example = "2026-2027")]
    name: String,
    /// Year start, UTC unix-milliseconds.
    starts_at: i64,
    /// Year end, UTC unix-milliseconds.
    ends_at: i64,
    /// Archived at, UTC unix-millis; null while the year is open.
    archived_at: Option<i64>,
    /// Who created it.
    creator: String,
    grade_promotions: Vec<PromotionBody>,
    /// How many şubeler sit in this year.
    #[schema(example = 12)]
    class_count: i64,
    /// How many dönemler it holds.
    #[schema(example = 2)]
    term_count: i64,
}

impl YearResponse {
    fn new(year: &AcademicYear) -> Self {
        Self {
            id: year.get_id().key().to_string(),
            name: year.get_name().as_str().to_string(),
            starts_at: year.get_starts_at().as_millis(),
            ends_at: year.get_ends_at().as_millis(),
            archived_at: year.get_archived_at().map(|at| at.as_millis()),
            creator: year.get_creator().key().to_string(),
            grade_promotions: year
                .get_grade_promotions()
                .iter()
                .map(|promo| PromotionBody {
                    from_grade: promo.get_from_grade().to_string(),
                    to_grade: promo.get_to_grade().to_string(),
                })
                .collect(),
            class_count: year.get_class_count(),
            term_count: year.get_term_count(),
        }
    }
}

/// What one rollover carried: how many şubeler were planted in the target
/// year, how many live students came with them, and the grades left behind
/// because the year names no promotion for them — the graduating ones.
#[derive(Serialize, ToSchema)]
struct RolloverResponse {
    /// The year that received the şubeler.
    year: String,
    #[schema(example = 12)]
    classes: usize,
    #[schema(example = 312)]
    students: usize,
    /// The grade labels that stayed behind (graduation), in the order they
    /// were met.
    #[schema(example = json!(["8", "12"]))]
    graduated: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
struct Rollover {
    /// The year the şubeler come *from* — usually the one that just ended.
    /// Must be a different, existing year; the target must still be empty.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    from_year: String,
}

/// The year a path id names, or a 404.
async fn year_or_404(id: &str, db: &crate::database::Database) -> Result<AcademicYear, AppError> {
    service::academic_year::read(
        db,
        &crate::domain::academic_year::AcademicYearId::from_key(id),
    )
    .await?
    .ok_or(AppError::NotFound)
}

/// The promotion list off the wire, each pair validated against the grade
/// bound.
fn promotions(bodies: Option<Vec<PromotionBody>>) -> Result<Option<Vec<GradePromotion>>, AppError> {
    bodies
        .map(|bodies| bodies.iter().map(PromotionBody::parse).collect())
        .transpose()
}

/// The merged range check for a PATCH: an omitted end keeps the stored one.
fn check_range(
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
    year: &AcademicYear,
) -> Result<(), AppError> {
    let starts = starts_at.unwrap_or_else(|| year.get_starts_at());
    let ends = ends_at.unwrap_or_else(|| year.get_ends_at());
    if ends <= starts {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "ends_at",
            reason: "must be after starts_at",
        }));
    }
    Ok(())
}

/// Create an academic year. Requires manager+. Past dates are allowed — years
/// are calendar structure, not schedules. `grade_promotions` is the
/// sınıf-geçme policy the rollover applies.
#[utoipa::path(
    post,
    path = "/",
    tag = "academic-years",
    security(("session_cookie" = [])),
    request_body = CreateYear,
    responses(
        (status = 201, description = "Academic year created", body = YearResponse),
        (status = 400, description = "Invalid name, range, or grade labels", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "A year with that name already exists", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_year(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Json(req): Json<CreateYear>,
) -> Result<(StatusCode, Json<YearResponse>), AppError> {
    let name = AcademicYearName::try_new(&req.name)?;
    let starts_at = Timestamp::from_millis(req.starts_at);
    let ends_at = Timestamp::from_millis(req.ends_at);
    check_time_range(Some(starts_at), Some(ends_at))?;
    let year = service::academic_year::create(
        &st.db,
        user.get_id(),
        name,
        starts_at,
        ends_at,
        promotions(req.grade_promotions)?.unwrap_or_default(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(YearResponse::new(&year))))
}

/// List every academic year, newest first. Requires teacher+. Paged via
/// `?limit=&offset=` (omit `limit` for the full list); returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "academic-years",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of academic years (the full list when unpaged)", body = Page<YearResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn list_years(
    State(st): State<AppState>,
    RequireTeacher(_user): RequireTeacher,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<YearResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (years, total) = service::academic_year::list_all(&st.db, limit, offset).await?;
    let items = years.iter().map(YearResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single academic year by id. Requires teacher+.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "academic-years",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Academic year id")),
    responses(
        (status = 200, description = "The academic year", body = YearResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_year(
    State(st): State<AppState>,
    RequireTeacher(_user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<YearResponse>, AppError> {
    Ok(Json(YearResponse::new(&year_or_404(&id, &st.db).await?)))
}

/// Update an academic year. Requires manager+. Omitted fields keep their
/// value; `grade_promotions` is replaced as a whole when sent. An archived
/// year is read-only.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "academic-years",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Academic year id")),
    request_body = UpdateYear,
    responses(
        (status = 200, description = "Updated academic year", body = YearResponse),
        (status = 400, description = "Invalid name, range, or grade labels", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The year is archived — past years are read-only, a rename collided with an existing year, or the year changed under concurrent edits (re-read and retry)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_year(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateYear>,
) -> Result<Json<YearResponse>, AppError> {
    let year = year_or_404(&id, &st.db).await?;
    service::academic_year::require_writable(&year)?;
    let name = req
        .name
        .as_deref()
        .map(AcademicYearName::try_new)
        .transpose()?;
    let starts_at = req.starts_at.map(Timestamp::from_millis);
    let ends_at = req.ends_at.map(Timestamp::from_millis);
    check_range(starts_at, ends_at, &year)?;
    let updated = service::academic_year::update(
        &st.db,
        year,
        name,
        starts_at,
        ends_at,
        promotions(req.grade_promotions)?,
    )
    .await?;
    Ok(Json(YearResponse::new(&updated)))
}

/// Delete an academic year. Requires manager+. Refused with a 409 while any
/// şube or dönem still links it — move or delete them first. An archived year
/// must be re-opened (`PATCH` is not enough: there is no unarchive here, so
/// the row is only deletable while open) like every other past-structure
/// write.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "academic-years",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Academic year id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Classes or terms are still linked to this year, or the year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_year(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let year = year_or_404(&id, &st.db).await?;
    service::academic_year::require_writable(&year)?;
    service::academic_year::delete(&st.db, year).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Carry another year's şubeler into this one. Requires manager+. Each şube of
/// `from_year` whose grade the target year promotes is planted afresh here —
/// same name, mapped grade, and copies of its instances (`ders_saati`, karne
/// policy, teachers) and of every live member, who are also enrolled into the
/// new instances. A grade with no promotion entry stays behind: that is
/// graduation, and `graduated` names it. The target must be empty (a second
/// rollover into it is a 409, which is what makes the command idempotent),
/// open, and different from the source.
#[utoipa::path(
    post,
    path = "/{id}/rollover",
    tag = "academic-years",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Target academic year id")),
    request_body = Rollover,
    responses(
        (status = 200, description = "What the rollover carried", body = RolloverResponse),
        (status = 400, description = "Unknown from_year", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Target year not found", body = ErrorResponse),
        (status = 409, description = "The target year already holds classes, is archived, or is the source year itself", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn rollover(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<Rollover>,
) -> Result<Json<RolloverResponse>, AppError> {
    let target = year_or_404(&id, &st.db).await?;
    let from = year_or_404(&req.from_year, &st.db).await?;
    let report =
        service::academic_year::rollover(&st.db, target.get_id(), from.get_id(), user.get_id())
            .await?;
    Ok(Json(RolloverResponse {
        year: target.get_id().key(),
        classes: report.classes,
        students: report.students,
        graduated: report.graduated,
    }))
}
