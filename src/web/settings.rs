use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::settings::{GradeBand, Settings};
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::{CurrentUser, RequireManager};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_settings, update_settings))
}

/// One grade-display band: marks at or above `min` (and below the next band's
/// `min`) render as `label`.
#[derive(Serialize, Deserialize, ToSchema)]
struct GradeBandDto {
    /// Lowest mark of the band, `0`–`100`. One band must start at `0`.
    #[schema(example = 85)]
    min: i64,
    /// What that range shows as, 1–20 characters (`"AA"`, `"5"`, `"pass"`).
    #[schema(example = "AA")]
    label: String,
}

/// The school's policy: which exam kinds exist, which attendance statuses the
/// roll call accepts, and how numeric marks display as grades.
#[derive(Serialize, ToSchema)]
struct SettingsResponse {
    /// Accepted `kind` values for new exams.
    #[schema(example = json!(["homework", "quiz", "midterm", "final", "project", "oral"]))]
    exam_kinds: Vec<String>,
    /// Accepted `status` values for attendance marking. Always contains the
    /// core four (`present`, `absent`, `late`, `excused`).
    #[schema(example = json!(["present", "absent", "late", "excused"]))]
    attendance_statuses: Vec<String>,
    /// Grade-display bands, highest first. Empty = marks display numeric-only.
    grade_bands: Vec<GradeBandDto>,
}

impl SettingsResponse {
    fn new(settings: &Settings) -> Self {
        Self {
            exam_kinds: settings.get_exam_kinds().to_vec(),
            attendance_statuses: settings.get_attendance_statuses().to_vec(),
            grade_bands: settings
                .get_grade_bands()
                .iter()
                .map(|band| GradeBandDto {
                    min: band.get_min(),
                    label: band.get_label().to_string(),
                })
                .collect(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct UpdateSettings {
    /// Replaces the whole list when present: 1–20 unique entries, each 1–50
    /// characters.
    exam_kinds: Option<Vec<String>>,
    /// Replaces the whole list when present; must keep `present`, `absent`,
    /// `late`, `excused` (the attendance rate is defined over them).
    attendance_statuses: Option<Vec<String>>,
    /// Replaces the whole set when present. `[]` clears the bands (numeric-only
    /// marks); otherwise mins are unique and one band must start at `0`.
    grade_bands: Option<Vec<GradeBandDto>>,
}

/// The school's current policy. Any authenticated user — clients need it to
/// render pickers and grades. Falls back to the built-in defaults until a
/// manager edits it.
#[utoipa::path(
    get,
    path = "/",
    tag = "settings",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The school's policy", body = SettingsResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn get_settings(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
) -> Result<Json<SettingsResponse>, AppError> {
    let settings = Settings::load(&st.db).await?;
    Ok(Json(SettingsResponse::new(&settings)))
}

/// Update the school's policy. Requires manager+. Omitted fields keep their
/// value; a present field replaces its list wholesale. Existing rows are
/// untouched — a removed exam kind or status lives on in old records; only
/// new writes are held to the new lists.
#[utoipa::path(
    patch,
    path = "/",
    tag = "settings",
    security(("session_cookie" = [])),
    request_body = UpdateSettings,
    responses(
        (status = 200, description = "Updated policy", body = SettingsResponse),
        (status = 400, description = "Invalid lists or bands", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
    ),
)]
async fn update_settings(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(req): Json<UpdateSettings>,
) -> Result<Json<SettingsResponse>, AppError> {
    let current = Settings::load(&st.db).await?;

    let exam_kinds = req
        .exam_kinds
        .unwrap_or_else(|| current.get_exam_kinds().to_vec());
    let attendance_statuses = req
        .attendance_statuses
        .unwrap_or_else(|| current.get_attendance_statuses().to_vec());
    let grade_bands = match req.grade_bands {
        Some(bands) => bands
            .into_iter()
            .map(|band| GradeBand::try_new(band.min, &band.label))
            .collect::<Result<Vec<_>, _>>()?,
        None => current.get_grade_bands().to_vec(),
    };

    let settings = Settings::try_new(exam_kinds, attendance_statuses, grade_bands)?;
    let saved = settings.save(&st.db).await?;
    Ok(Json(SettingsResponse::new(&saved)))
}
