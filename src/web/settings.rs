use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use surrealdb::types::RecordId;

use crate::constant::CAS_UPDATE_RETRIES;
use crate::database::Database;
use crate::domain::cap;
use crate::domain::exam_result::kind_ref;
use crate::domain::menu::slot_ref;
use crate::domain::settings::{ExamKindDef, GradeBand, MealSlotDef, Settings};
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::{CurrentUser, RequireManager, set_or_clear};

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

/// One meal slot the school serves. A published menu snapshots this name, so
/// renaming or retiring a slot never rewrites a past menu.
#[derive(Serialize, Deserialize, ToSchema)]
struct MealSlotDto {
    /// The `slot` value menus carry, 1–50 characters.
    #[schema(example = "lunch", max_length = 50)]
    name: String,
    /// When the slot is served, as minutes past midnight **UTC** on the menu's
    /// date (`0`–`1439`; `720` = 12:00 UTC). `meal_cancel_cutoff_minutes`
    /// counts back from this instant.
    ///
    /// **The clock is UTC, not local time** — this API stores no school
    /// timezone, so a UTC+3 school enters `540` (09:00 UTC) for a meal served
    /// at noon locally. `null` = unset, and then the cutoff counts back from
    /// midnight UTC of the menu's date, the pre-serving-time behaviour.
    #[schema(example = 720, minimum = 0, maximum = 1_439)]
    serving_minute: Option<i64>,
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
    /// Meal slots menus may be published for, each with the UTC minute it is
    /// served at (`null` = unset → the cutoff counts back from midnight UTC).
    /// Empty = the school runs no meal program.
    #[schema(example = json!([
        {"name": "breakfast", "serving_minute": 300},
        {"name": "lunch", "serving_minute": 720},
        {"name": "snack", "serving_minute": null},
    ]))]
    meal_slots: Vec<MealSlotDto>,
    /// Dietary tags a dish and a student's profile may carry.
    #[schema(example = json!(["vegetarian", "vegan", "gluten_free"]))]
    dietary_tags: Vec<String>,
    /// How many minutes before a slot's `serving_minute` booking and
    /// cancelling close. `null` = no cutoff.
    #[schema(example = 120)]
    meal_cancel_cutoff_minutes: Option<i64>,
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
            meal_slots: settings
                .get_meal_slots()
                .iter()
                .map(|slot| MealSlotDto {
                    name: slot.get_name().to_string(),
                    serving_minute: slot.get_serving_minute(),
                })
                .collect(),
            dietary_tags: settings.get_dietary_tags(),
            meal_cancel_cutoff_minutes: settings.get_meal_cancel_cutoff_minutes(),
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
    /// Replaces the whole list when present: at most 20 entries with unique
    /// names, each 1–50 characters, each with an optional `serving_minute`
    /// (minutes past midnight **UTC**, `0`–`1439`) the booking cutoff counts
    /// back from. `[]` switches the meal program off. A slot a menu was already
    /// published for cannot be removed (409); its serving time may be edited
    /// freely and applies live, to menus already published for it too.
    #[schema(max_items = 20)]
    meal_slots: Option<Vec<MealSlotDto>>,
    /// Replaces the whole list when present: at most 20 unique entries, each
    /// 1–50 characters. `[]` means the school tracks no dietary tags.
    #[schema(max_items = 20)]
    dietary_tags: Option<Vec<String>>,
    /// Minutes before a slot's `serving_minute` at which booking *and*
    /// cancelling close, `0`–`10080` (one week). A slot with no serving time
    /// is measured from midnight UTC of the menu's date instead. Omit to keep the current value; send `null` for
    /// no cutoff at all.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>, example = 120, minimum = 0, maximum = 10_080)]
    meal_cancel_cutoff_minutes: Option<Option<i64>>,
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
/// gone counts with weight 1 but cannot be graded (409) until the kind
/// returns — the other end of the same rule. `max_file_bytes` likewise
/// applies at upload time only — already-stored files keep their size, and the
/// chatbot knobs apply to the next chat request only. `meal_slots` follows the
/// exam-kind rule: a slot a menu was already published for cannot be dropped
/// (409), because the menu snapshotted its name.
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
        (status = 409, description = "A removed exam kind still has graded exams, a removed meal slot still has published menus, or concurrent edits kept changing the settings mid-save", body = ErrorResponse),
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
    for _ in 0..CAS_UPDATE_RETRIES {
        let current = Settings::load(&st.db).await?;

        let exam_kinds = match &req.exam_kinds {
            Some(kinds) => kinds
                .iter()
                .map(|kind| ExamKindDef::try_new(&kind.name, kind.weight))
                .collect::<Result<Vec<_>, _>>()?,
            None => current.get_exam_kinds().to_vec(),
        };
        let meal_slots = match &req.meal_slots {
            Some(slots) => slots
                .iter()
                .map(|slot| MealSlotDef::try_new(&slot.name, slot.serving_minute))
                .collect::<Result<Vec<_>, _>>()?,
            None => current.get_meal_slots(),
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
        // The name lists as they stand and as they would stand, for the removal
        // guards below — a list edit is judged by which names it drops.
        let was_kinds = kind_names(current.get_exam_kinds());
        let now_kinds = kind_names(&exam_kinds);
        let was_slots = slot_names(&current.get_meal_slots());
        let now_slots = slot_names(&meal_slots);

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
        params.meal_slots = meal_slots;
        params.dietary_tags = req
            .dietary_tags
            .clone()
            .unwrap_or_else(|| current.get_dietary_tags());
        // Double option: absent keeps the knob, `null` clears it (no cutoff).
        params.meal_cancel_cutoff_minutes = req
            .meal_cancel_cutoff_minutes
            .unwrap_or(params.meal_cancel_cutoff_minutes);

        let settings = Settings::try_new(params)?;

        // The removal guards. A name leaves a list by being *retired* on its
        // reference counter — one conditional write on one record, which lands
        // only while nothing references the name and refuses every claim from
        // that instant on. That is the whole guard: the check and the removal
        // used to be a cross-table count and a save held together by a
        // process-wide lock, which the second replica walked straight through.
        //
        // Retired before the save, never after: the other order leaves a window
        // in which a mark lands under a kind the settings no longer list. A save
        // that then does not land undoes them (`restore`) before the next try.
        let mut retired: Vec<RecordId> = Vec::new();
        let mut refused = None;
        for gone in missing(&was_kinds, &now_kinds) {
            let counter = kind_ref(&gone);
            if !cap::retire(&counter, &st.db).await? {
                refused = Some(AppError::ConflictOwned(format!(
                    "exams of kind '{gone}' are already graded — the kind cannot be removed"
                )));
                break;
            }
            retired.push(counter);
        }
        // Same shape for meal slots: a slot a menu was already published for
        // cannot leave the list — the menu snapshotted the name as text, and a
        // slot no longer offered would leave that menu unreachable from the
        // school's own list.
        if refused.is_none() {
            for gone in missing(&was_slots, &now_slots) {
                let counter = slot_ref(&gone);
                if !cap::retire(&counter, &st.db).await? {
                    refused = Some(AppError::ConflictOwned(format!(
                        "menus are already published for the '{gone}' slot — it cannot be removed"
                    )));
                    break;
                }
                retired.push(counter);
            }
        }
        if let Some(refused) = refused {
            restore(&retired, &st.db).await?;
            return Err(refused);
        }
        // A name re-entering a list is back in service: its counter still
        // carries the retirement from the edit that dropped it, and a mark (or
        // a menu) under a kind the school offers again must not be refused.
        for back in missing(&now_kinds, &was_kinds) {
            cap::unretire(&kind_ref(&back), &st.db).await?;
        }
        for back in missing(&now_slots, &was_slots) {
            cap::unretire(&slot_ref(&back), &st.db).await?;
        }

        match settings.save_if_unchanged(&current, &st.db).await {
            Ok(Some(saved)) => return Ok(Json(SettingsResponse::new(&saved))),
            // The row moved under the snapshot these guards were judged against:
            // put the names back and re-merge, or the next attempt would decide
            // against a list nobody asked for.
            Ok(None) => restore(&retired, &st.db).await?,
            Err(err) => {
                restore(&retired, &st.db).await?;
                return Err(err);
            }
        }
    }
    Err(AppError::Conflict(
        "the settings kept changing underneath this update — try again",
    ))
}

fn kind_names(kinds: &[ExamKindDef]) -> Vec<String> {
    kinds
        .iter()
        .map(|kind| kind.get_name().to_string())
        .collect()
}

fn slot_names(slots: &[MealSlotDef]) -> Vec<String> {
    slots
        .iter()
        .map(|slot| slot.get_name().to_string())
        .collect()
}

/// The names `before` carries that `after` does not — a list edit's removals,
/// or its additions with the arguments swapped.
fn missing(before: &[String], after: &[String]) -> Vec<String> {
    before
        .iter()
        .filter(|name| !after.contains(name))
        .cloned()
        .collect()
}

/// Put back every name this attempt retired: the edit that would have removed
/// them did not land, so they are still names the school offers.
async fn restore(names: &[RecordId], db: &Database) -> Result<(), AppError> {
    for name in names {
        cap::unretire(name, db).await?;
    }
    Ok(())
}
