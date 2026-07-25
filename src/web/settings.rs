use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::SETTINGS_UPDATE_RETRIES;
use crate::domain::exam_result::ExamResult;
use crate::domain::settings::{ExamKindDef, GradeBand, Settings};
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::exams::EXAM_LOCK;
use super::{CurrentUser, RequireManager};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_settings, update_settings))
}

/// One exam kind the school runs, with its weight in course averages. An exam
/// of this kind counts `weight` times into `Σ(mark×weight) / Σ(weight)`.
#[derive(Serialize, Deserialize, ToSchema)]
struct ExamKindDto {
    /// The `kind` value exams carry, 1–50 characters.
    #[schema(example = "midterm", max_length = 50)]
    name: String,
    /// The kind's weight in the course average, `1`–`100`. Editing it
    /// re-weights every exam of this kind at once.
    #[schema(example = 2, minimum = 1, maximum = 100)]
    weight: i64,
}

/// One grade-display band: marks at or above `min` (and below the next band's
/// `min`) render as `label`.
#[derive(Serialize, Deserialize, ToSchema)]
struct GradeBandDto {
    /// Lowest mark of the band, `0`–`100`. One band must start at `0`.
    #[schema(example = 85, minimum = 0, maximum = 100)]
    min: i64,
    /// What that range shows as, 1–20 characters (`"AA"`, `"5"`, `"pass"`).
    #[schema(example = "AA", max_length = 20)]
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
    /// How many prior turns of a thread the chatbot is given as context.
    #[schema(example = 10)]
    chatbot_history_turns: i64,
    /// How many chatbot threads one user may keep.
    #[schema(example = 50)]
    max_chatbot_threads: i64,
    /// Character limit on one chat message.
    #[schema(example = 4000)]
    max_chatbot_message_len: i64,
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
            chatbot_history_turns: settings.get_chatbot_history_turns(),
            max_chatbot_threads: settings.get_max_chatbot_threads(),
            max_chatbot_message_len: settings.get_max_chatbot_message_len(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct UpdateSettings {
    /// Replaces the whole list when present: 1–20 entries with unique names,
    /// each name 1–50 characters, each weight `1`–`100`.
    #[schema(max_items = 20)]
    exam_kinds: Option<Vec<ExamKindDto>>,
    /// Replaces the whole list when present; must keep `present`, `absent`,
    /// `late`, `excused` (the attendance rate is defined over them). Each entry
    /// is 1–50 characters.
    #[schema(max_items = 20)]
    attendance_statuses: Option<Vec<String>>,
    /// Replaces the whole set when present. `[]` clears the bands (numeric-only
    /// marks); otherwise mins are unique and one band must start at `0`.
    #[schema(max_items = 20)]
    grade_bands: Option<Vec<GradeBandDto>>,
    /// Per-file size limit for note uploads, in bytes:
    /// `1024` (1 KiB) – `26214400` (25 MiB). The ceiling is a server hard cap.
    #[schema(example = 5_242_880, minimum = 1_024, maximum = 26_214_400)]
    max_file_bytes: Option<i64>,
    /// How many prior turns of a thread the chatbot is given as context,
    /// `1`–`50`. Every turn is re-sent on every reply, so this costs tokens.
    #[schema(example = 10, minimum = 1, maximum = 50)]
    chatbot_history_turns: Option<i64>,
    /// How many chatbot threads one user may keep, `1`–`500`. At the cap
    /// the user deletes an old thread before starting a new one.
    #[schema(example = 50, minimum = 1, maximum = 500)]
    max_chatbot_threads: Option<i64>,
    /// Character limit on one chat message, `100`–`8000`. The ceiling is a
    /// server hard cap.
    #[schema(example = 4000, minimum = 100, maximum = 8_000)]
    max_chatbot_message_len: Option<i64>,
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
/// every exam of that kind. For that reason a kind whose exams already carry
/// marks cannot be dropped from the list (409) — those marks would silently
/// re-weight to 1; an unmarked kind leaves freely, and an exam whose kind is
/// gone counts with weight 1 until the kind returns. `max_file_bytes` likewise
/// applies at upload time only — already-stored files keep their size, and the
/// chatbot knobs apply to the next chat request only.
#[utoipa::path(
    patch,
    path = "/",
    tag = "settings",
    security(("session_cookie" = [])),
    request_body = UpdateSettings,
    responses(
        (status = 200, description = "Updated policy", body = SettingsResponse),
        (status = 400, description = "Invalid lists, bands, file limit, or chat limits", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "A removed exam kind still has graded exams, or concurrent edits kept changing the settings mid-save", body = ErrorResponse),
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
    // Writer lease of [`EXAM_LOCK`] while the kind list is being replaced: the
    // no-marks check below and the save are one unit, so a grade (a reader)
    // can't land the first mark of a kind that is being dropped mid-flight.
    let _guard = match req.exam_kinds {
        Some(_) => Some(EXAM_LOCK.write().await),
        None => None,
    };
    for _ in 0..SETTINGS_UPDATE_RETRIES {
        let current = Settings::load(&st.db).await?;

        let exam_kinds = match &req.exam_kinds {
            Some(kinds) => kinds
                .iter()
                .map(|kind| ExamKindDef::try_new(&kind.name, kind.weight))
                .collect::<Result<Vec<_>, _>>()?,
            None => current.get_exam_kinds().to_vec(),
        };
        // A kind that graded exams still count under cannot leave the list —
        // weights are read live, so dropping it would silently re-weight every
        // mark of that kind. Re-checked on every retry: the snapshot it is
        // diffed against is the one the save is conditioned on.
        for gone in current.get_exam_kinds().iter().filter(|kind| {
            !exam_kinds
                .iter()
                .any(|new| new.get_name() == kind.get_name())
        }) {
            if ExamResult::any_for_kind(gone.get_name(), &st.db).await? {
                return Err(AppError::ConflictOwned(format!(
                    "exams of kind '{}' are already graded — the kind cannot be removed",
                    gone.get_name()
                )));
            }
        }
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
        // Merge over the snapshot's resolved values: an omitted field keeps
        // whatever the row (or the default behind an unset field) reads as.
        let mut params = current.params();
        params.exam_kinds = exam_kinds;
        params.attendance_statuses = attendance_statuses;
        params.grade_bands = grade_bands;
        params.max_file_bytes = req.max_file_bytes.unwrap_or(params.max_file_bytes);
        params.chatbot_history_turns = req
            .chatbot_history_turns
            .unwrap_or(params.chatbot_history_turns);
        params.max_chatbot_threads = req
            .max_chatbot_threads
            .unwrap_or(params.max_chatbot_threads);
        params.max_chatbot_message_len = req
            .max_chatbot_message_len
            .unwrap_or(params.max_chatbot_message_len);

        let settings = Settings::try_new(params)?;
        if let Some(saved) = settings.save_if_unchanged(&current, &st.db).await? {
            return Ok(Json(SettingsResponse::new(&saved)));
        }
    }
    Err(AppError::Conflict(
        "the settings kept changing underneath this update — try again",
    ))
}
