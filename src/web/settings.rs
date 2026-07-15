use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::SETTINGS_UPDATE_RETRIES;
use crate::domain::settings::{ExamKindDef, GradeBand, Settings};
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::{CurrentUser, RequireManager};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_settings, update_settings))
}

/// One exam kind the school runs, with its weight in course averages. An exam
/// of this kind counts `weight` times into `Σ(mark×weight) / Σ(weight)`.
#[derive(Serialize, Deserialize, ToSchema)]
struct ExamKindDto {
    /// The `kind` value exams carry, 1–50 characters.
    #[schema(example = "midterm")]
    name: String,
    /// The kind's weight in the course average, `1`–`100`. Editing it
    /// re-weights every exam of this kind at once.
    #[schema(example = 2)]
    weight: i64,
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

/// The school's policy: which exam kinds exist (and their weights in course
/// averages), which attendance statuses the roll call accepts, and how
/// numeric marks display as grades.
#[derive(Serialize, ToSchema)]
struct SettingsResponse {
    /// Accepted `kind` values for new exams, each with its weight in the
    /// course average.
    #[schema(example = json!([
        {"name": "midterm", "weight": 2},
        {"name": "final", "weight": 3},
        {"name": "oral", "weight": 1},
    ]))]
    exam_kinds: Vec<ExamKindDto>,
    /// Accepted `status` values for attendance marking. Always contains the
    /// core four (`present`, `absent`, `late`, `excused`).
    #[schema(example = json!(["present", "absent", "late", "excused"]))]
    attendance_statuses: Vec<String>,
    /// Grade-display bands, highest first. Empty = marks display numeric-only.
    grade_bands: Vec<GradeBandDto>,
    /// Per-file size limit for note uploads, in bytes.
    #[schema(example = 5_242_880)]
    max_file_bytes: i64,
}

impl SettingsResponse {
    fn new(settings: &Settings) -> Self {
        Self {
            exam_kinds: settings
                .get_exam_kinds()
                .iter()
                .map(|kind| ExamKindDto {
                    name: kind.get_name().to_string(),
                    weight: kind.get_weight(),
                })
                .collect(),
            attendance_statuses: settings.get_attendance_statuses().to_vec(),
            grade_bands: settings
                .get_grade_bands()
                .iter()
                .map(|band| GradeBandDto {
                    min: band.get_min(),
                    label: band.get_label().to_string(),
                })
                .collect(),
            max_file_bytes: settings.get_max_file_bytes(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct UpdateSettings {
    /// Replaces the whole list when present: 1–20 entries with unique names,
    /// each name 1–50 characters, each weight `1`–`100`.
    exam_kinds: Option<Vec<ExamKindDto>>,
    /// Replaces the whole list when present; must keep `present`, `absent`,
    /// `late`, `excused` (the attendance rate is defined over them).
    attendance_statuses: Option<Vec<String>>,
    /// Replaces the whole set when present. `[]` clears the bands (numeric-only
    /// marks); otherwise mins are unique and one band must start at `0`.
    grade_bands: Option<Vec<GradeBandDto>>,
    /// Per-file size limit for note uploads, in bytes:
    /// `1024` (1 KiB) – `26214400` (25 MiB). The ceiling is a server hard cap.
    #[schema(example = 5_242_880)]
    max_file_bytes: Option<i64>,
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
/// new writes are held to the new lists. Kind weights, though, apply live:
/// mark reports read them at request time, so editing a weight re-weights
/// every exam of that kind, and an exam whose kind was removed from the list
/// counts with weight 1 until the kind returns. `max_file_bytes` likewise
/// applies at upload time only — already-stored files keep their size.
#[utoipa::path(
    patch,
    path = "/",
    tag = "settings",
    security(("session_cookie" = [])),
    request_body = UpdateSettings,
    responses(
        (status = 200, description = "Updated policy", body = SettingsResponse),
        (status = 400, description = "Invalid lists, bands, or file limit", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "Concurrent edits kept changing the settings mid-save", body = ErrorResponse),
    ),
)]
async fn update_settings(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(req): Json<UpdateSettings>,
) -> Result<Json<SettingsResponse>, AppError> {
    // Merge over a snapshot, then save only while the row still matches it —
    // otherwise a concurrent PATCH of a *different* field would be silently
    // reverted by whichever whole-row write lands second. A refused save
    // reloads and re-merges, so both edits land.
    for _ in 0..SETTINGS_UPDATE_RETRIES {
        let current = Settings::load(&st.db).await?;

        let exam_kinds = match &req.exam_kinds {
            Some(kinds) => kinds
                .iter()
                .map(|kind| ExamKindDef::try_new(&kind.name, kind.weight))
                .collect::<Result<Vec<_>, _>>()?,
            None => current.get_exam_kinds().to_vec(),
        };
        let attendance_statuses = req
            .attendance_statuses
            .clone()
            .unwrap_or_else(|| current.get_attendance_statuses().to_vec());
        let grade_bands = match &req.grade_bands {
            Some(bands) => bands
                .iter()
                .map(|band| GradeBand::try_new(band.min, &band.label))
                .collect::<Result<Vec<_>, _>>()?,
            None => current.get_grade_bands().to_vec(),
        };
        let max_file_bytes = req
            .max_file_bytes
            .unwrap_or_else(|| current.get_max_file_bytes());

        let settings =
            Settings::try_new(exam_kinds, attendance_statuses, grade_bands, max_file_bytes)?;
        if let Some(saved) = settings.save_if_unchanged(&current, &st.db).await? {
            return Ok(Json(SettingsResponse::new(&saved)));
        }
    }
    Err(AppError::Conflict(
        "the settings kept changing underneath this update — try again",
    ))
}
