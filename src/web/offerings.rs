//! The course **offerings**: the grade-level templates (`course_offering`)
//! every class×course instance inherits from — one row per
//! (course × grade_level) carrying the title, description and the
//! weekly-hours / report-card defaults for that grade, with the per-class
//! overrides living on the instances themselves
//! ([`crate::web::instances`]).
//!
//! Reads are open to any signed-in session; writes are the office's
//! (`manager`+), the same bar as the catalog itself. Every write door keeps
//! the override-or-inherit rule visible: a `PATCH` field sent as `null`
//! clears back to inherit, and the delete refuses with `offering_in_use`
//! while any instance still teaches from the template.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::class_course::DersSaati;
use crate::domain::course::{CourseDescription, CourseId, CourseTitle};
use crate::domain::course_offering::{CourseOffering, CourseOfferingId};
use crate::domain::grade::GradeLevel;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::state::AppState;
use crate::web::tenant_state::State;

use super::{CurrentUser, Page, PageParams, PersonRef, RequireManager, person_map, set_or_clear};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_offering, list_offerings))
        .routes(routes!(get_offering, update_offering, delete_offering))
}

#[derive(Deserialize, ToSchema)]
struct CreateOffering {
    /// The catalog course this template teaches from (`POST /courses`).
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    course: String,
    /// The ladder rung: `0` is anaokulu, `1..=12` the school years.
    #[schema(minimum = 0, maximum = 12, example = 9)]
    grade_level: i16,
    /// The grade's own title; omit to inherit the catalog course's.
    #[schema(max_length = 200)]
    title: Option<String>,
    /// The grade's own description; omit to inherit the catalog's.
    #[schema(max_length = 2_000)]
    description: Option<String>,
    /// The grade's default weekly hours; omit for the constant 1.
    #[schema(minimum = 1, maximum = 40, example = 5)]
    default_ders_saati: Option<i64>,
    /// The grade's default report-card policy; omit for counted (`true`).
    default_counts_toward_karne: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateOffering {
    /// Omit to keep; send `null` to clear (inherit the catalog course's
    /// title); send a value to set the grade's own.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>, max_length = 200)]
    title: Option<Option<String>>,
    /// Omit to keep; `null` to clear (inherit the catalog's); a value to set.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>, max_length = 2_000)]
    description: Option<Option<String>>,
    /// Omit to keep; `null` to clear (inherit the constant 1); 1..=40 to set.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>, minimum = 1, maximum = 40)]
    default_ders_saati: Option<Option<i64>>,
    /// Omit to keep; `null` to clear (inherit counted); a value to set.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<bool>)]
    default_counts_toward_karne: Option<Option<bool>>,
}

/// The list filters, both optional and combinable.
#[derive(Deserialize, IntoParams)]
struct OfferingFilter {
    /// Only this course's offerings.
    #[param(example = "019732e3-7b00-7000-8000-00000000dead")]
    course: Option<String>,
    /// Only this ladder rung (`0` = anaokulu).
    #[param(minimum = 0, maximum = 12)]
    grade_level: Option<i16>,
}

impl OfferingFilter {
    /// Validate + canonicalize: a malformed course key is a 400, not an empty
    /// page — the caller asked for something, the key just does not name it.
    fn resolve(&self) -> Result<(Option<CourseId>, Option<GradeLevel>), AppError> {
        let course = match &self.course {
            Some(key) => {
                let id = CourseId::from_key(key);
                if id.uuid().is_nil() {
                    return Err(ValidationError::Invalid {
                        field: "course",
                        reason: "must be a hyphenated uuid",
                    }
                    .into());
                }
                Some(id)
            }
            None => None,
        };
        let grade = self.grade_level.map(GradeLevel::new).transpose()?;
        Ok((course, grade))
    }
}

/// Public shape of one offering: the grade-level template, with `null`
/// meaning *inherit* on every content field. These are the **raw template
/// values** — this row IS the grade's template, so a manager reads and edits
/// exactly what is stored here. The per-instance **resolved** view (override
/// → offering → catalog/constant, no nulls, with the source flags) is what
/// `GET /instances/{id}` serves.
#[derive(Serialize, ToSchema)]
pub struct OfferingResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    pub id: String,
    /// The catalog course taught from this template (`GET /courses/{id}`).
    pub course: String,
    /// The ladder rung: `0` = anaokulu, `1..=12` the school years.
    pub grade_level: i16,
    /// The grade's own title; `null` = the catalog course's title.
    pub title: Option<String>,
    /// The grade's own description; `null` = the catalog course's.
    pub description: Option<String>,
    /// The grade's default weekly hours; `null` = the constant 1.
    pub default_ders_saati: Option<i64>,
    /// The grade's default report-card policy; `null` = counted (`true`).
    pub default_counts_toward_karne: Option<bool>,
    /// Who minted the template (deliberately, or by first attaching the
    /// course to a class at this grade).
    pub created_by: PersonRef,
    /// Mint instant, UTC unix-milliseconds.
    pub created_at: i64,
    /// Last content edit, UTC unix-milliseconds.
    pub updated_at: i64,
}

impl OfferingResponse {
    fn new(offering: &CourseOffering, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: offering.get_id().key(),
            course: offering.get_course().key(),
            grade_level: offering.get_grade_level().get(),
            title: offering.get_title().map(|t| t.as_str().to_string()),
            description: offering.get_description().map(|d| d.as_str().to_string()),
            default_ders_saati: offering.get_default_ders_saati().map(DersSaati::as_i64),
            default_counts_toward_karne: offering.get_default_counts_toward_karne(),
            created_by: PersonRef::resolve(people, offering.get_created_by()),
            created_at: offering.get_created_at().as_millis(),
            updated_at: offering.get_updated_at().as_millis(),
        }
    }
}

async fn offering_or_404(key: &str, db: &Database) -> Result<CourseOffering, AppError> {
    service::course_offering::read(db, &CourseOfferingId::from_key(key))
        .await?
        .ok_or(AppError::NotFound)
}

/// Create the grade-level template for a course. Manager+. A course that is
/// not a class-delivered ders has no grade axis and is refused (a club or
/// etüt is joined individually, never attached); a template that already
/// exists for the (course, grade) pair is a 409 `offering_exists`.
#[utoipa::path(
    post,
    path = "/",
    tag = "offerings",
    security(("session_cookie" = [])),
    request_body = CreateOffering,
    responses(
        (status = 201, description = "Offering created", body = OfferingResponse),
        (status = 400, description = "Invalid fields, or the course is not a class-delivered ders (kind `course`)", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 409, description = "The course already has an offering for this grade — the body carries the machine code `offering_exists`", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_offering(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Json(req): Json<CreateOffering>,
) -> Result<(StatusCode, Json<OfferingResponse>), AppError> {
    let course = CourseId::from_key(&req.course);
    if course.uuid().is_nil() {
        return Err(ValidationError::Invalid {
            field: "course",
            reason: "must be a hyphenated uuid",
        }
        .into());
    }
    let grade = GradeLevel::new(req.grade_level)?;
    let title = match &req.title {
        Some(title) => Some(CourseTitle::try_new(title)?),
        None => None,
    };
    let description = match &req.description {
        Some(description) => Some(CourseDescription::try_new(description)?),
        None => None,
    };
    let staff = req.default_ders_saati.map(DersSaati::try_new).transpose()?;
    let offering = service::course_offering::create(
        &st.db,
        user.get_id(),
        &course,
        grade,
        title,
        description,
        staff,
        req.default_counts_toward_karne,
    )
    .await?;
    let people = PersonRef::map_of(&[&user]);
    Ok((StatusCode::CREATED, Json(OfferingResponse::new(&offering, &people))))
}

/// List the offerings, paged via `?limit=&offset=` (omit `limit` for all of
/// them); optionally narrowed by `?course=` and/or `?grade_level=`. Returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(PageParams, OfferingFilter),
    responses(
        (status = 200, description = "A page of offerings (all of them when unpaged)", body = Page<OfferingResponse>),
        (status = 400, description = "Invalid limit, offset, course key, or grade_level", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_offerings(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Query(page): Query<PageParams>,
    Query(filter): Query<OfferingFilter>,
) -> Result<Json<Page<OfferingResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (course, grade) = filter.resolve()?;
    let (rows, total) =
        service::course_offering::list(&st.db, course.as_ref(), grade, limit, offset).await?;
    let people = person_map(rows.iter().map(|o| *o.get_created_by()), &st.db).await?;
    let items = rows
        .iter()
        .map(|offering| OfferingResponse::new(offering, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch one offering by id. Any signed-in session. The body is the raw
/// template row (nullable = inherit) — not the per-instance resolved view
/// `GET /instances/{id}` serves.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    responses(
        (status = 200, description = "The offering's raw template values (null = inherit); the per-class resolved view lives on GET /instances/{id}", body = OfferingResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_offering(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<OfferingResponse>, AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    let people = person_map(std::iter::once(*offering.get_created_by()), &st.db).await?;
    Ok(Json(OfferingResponse::new(&offering, &people)))
}

/// Field-scoped edit. Manager+. Omitted fields keep their value; an explicit
/// `null` clears the override back to inherit (the catalog course's title or
/// description, the constant 1 weekly hour, counted toward the karne); a
/// value sets the grade's own. Instances that carry no override of their own
/// follow the change immediately — that is what inherit means.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    request_body = UpdateOffering,
    responses(
        (status = 200, description = "Updated offering", body = OfferingResponse),
        (status = 400, description = "Invalid field values", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type"),
    ),
)]
async fn update_offering(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateOffering>,
) -> Result<Json<OfferingResponse>, AppError> {
    // Tri-state per field: absent = keep, null = clear to inherit, value = set.
    let title = match req.title {
        None => None,
        Some(inner) => Some(match inner {
            None => None,
            Some(text) => Some(CourseTitle::try_new(&text)?),
        }),
    };
    let description = match req.description {
        None => None,
        Some(inner) => Some(match inner {
            None => None,
            Some(text) => Some(CourseDescription::try_new(&text)?),
        }),
    };
    let staff = match req.default_ders_saati {
        None => None,
        Some(inner) => Some(match inner {
            None => None,
            Some(hours) => Some(DersSaati::try_new(hours)?),
        }),
    };
    let offering = service::course_offering::update(
        &st.db,
        &CourseOfferingId::from_key(&id),
        title,
        description,
        staff,
        req.default_counts_toward_karne,
    )
    .await?;
    let people = person_map(std::iter::once(*offering.get_created_by()), &st.db).await?;
    Ok(Json(OfferingResponse::new(&offering, &people)))
}

/// Delete the template. Manager+. Refused with 409 `offering_in_use` while
/// any class×course instance still teaches from it — detach the course from
/// those classes first. Deleting a template no section uses erases only the
/// grade's overrides; the instances keep existing (their next attach
/// re-mints an empty template).
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "An instance still teaches from this offering — the body carries the machine code `offering_in_use`", body = ErrorResponse),
    ),
)]
async fn delete_offering(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    service::course_offering::delete(&st.db, &CourseOfferingId::from_key(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
