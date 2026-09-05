use crate::web::tenant_state::State;
use axum::Json;
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
use crate::domain::settings::{ExamKindDef, GradeBand, MealSlotDef, SETTINGS_LOCK, Settings};
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
    /// freely and applies live, to menus already published for it too. Names
    /// must not contain `/ \ ? # %` (a menu's id carries the name into a URL) —
    /// except a name the school already stores, which stays submittable so a
    /// list written before that rule can still be edited.
    #[schema(max_items = 20)]
    meal_slots: Option<Vec<MealSlotDto>>,
    /// Replaces the whole list when present: at most 20 unique entries, each
    /// 1–50 characters. `[]` means the school tracks no dietary tags.
    #[schema(max_items = 20)]
    dietary_tags: Option<Vec<String>>,
    /// Minutes before a slot's `serving_minute` at which booking *and*
    /// cancelling close, `0`–`10080` (one week). It binds only the slots that
    /// carry a `serving_minute`: with no serving hour there is no instant to
    /// count back from, so that slot's menus close at no deadline until the
    /// hour is set. Omit to keep the current value; send `null` for
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
/// (409), because the menu snapshotted its name. Slot names must not contain
/// `/ \ ? # %` (400) — a menu's id carries the name into a URL — but a name
/// already on the school's stored list is exempt, so a list written before that
/// rule can still be edited around it.
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
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_settings(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(req): Json<UpdateSettings>,
) -> Result<Json<SettingsResponse>, AppError> {
    Ok(Json(SettingsResponse::new(&apply(&req, &st.db).await?)))
}

/// The whole edit — snapshot, merge, retire, save — as one operation, so the
/// tests can drive it the way two managers do. Split out of the handler for
/// nothing else.
async fn apply(req: &UpdateSettings, db: &Database) -> Result<Settings, AppError> {
    // One settings edit at a time: the removals below are decided against the
    // snapshot this loop loads and written *before* the save that justifies
    // them, and no per-name bookkeeping can make that pair survive a rival's
    // pair interleaving with it (see `SETTINGS_LOCK`).
    let _guard = SETTINGS_LOCK.lock().await;
    // Merge over a snapshot, then save only while the row still matches it —
    // otherwise a concurrent PATCH of a *different* field would be silently
    // reverted by whichever whole-row write lands second. A refused save
    // reloads and re-merges, so both edits land.
    for _ in 0..CAS_UPDATE_RETRIES {
        let current = Settings::load(db).await?;

        let exam_kinds = match &req.exam_kinds {
            Some(kinds) => kinds
                .iter()
                .map(|kind| ExamKindDef::try_new(&kind.name, kind.weight))
                .collect::<Result<Vec<_>, _>>()?,
            None => current.get_exam_kinds().to_vec(),
        };
        let was_slots = slot_names(&current.get_meal_slots());
        let meal_slots = match &req.meal_slots {
            Some(slots) => slots
                .iter()
                .map(|slot| {
                    // A name the row already carries is validated as a kept one:
                    // the charset rule is younger than the stored lists, and
                    // re-validating one under it would leave a school unable to
                    // edit the rest of its slots at all (`MealSlotDef::try_kept`).
                    if was_slots.iter().any(|kept| kept == slot.name.trim()) {
                        MealSlotDef::try_kept(&slot.name, slot.serving_minute)
                    } else {
                        MealSlotDef::try_new(&slot.name, slot.serving_minute)
                    }
                })
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
        // process-wide lock that was released around the round trip between them.
        //
        // Retired before the save, never after: the other order leaves a window
        // in which a mark lands under a kind the settings no longer list. A save
        // that then does not land undoes them (`restore`) before the next try.
        //
        // Only a write that actually *flipped* a bit is recorded for that undo.
        // Retirement is idempotent, so two PATCHes dropping the same kind are
        // both told "retired" — and the one whose save then loses the row's CAS
        // used to un-retire the winner's kind, leaving it off the list with its
        // counter in service: gradable again, its count then non-zero, and so
        // impossible to remove ever after.
        let mut undo: Vec<Undo> = Vec::new();
        let mut refused = None;
        for gone in missing(&was_kinds, &now_kinds) {
            let counter = kind_ref(&gone);
            match cap::retire_name(&counter, db).await? {
                cap::Switched::Flipped => undo.push(Undo::Retired(counter)),
                cap::Switched::Unchanged => {}
                cap::Switched::InUse => {
                    refused = Some(AppError::ConflictOwned(format!(
                        "exams of kind '{gone}' are already graded — the kind cannot be removed"
                    )));
                    break;
                }
            }
        }
        // Same shape for meal slots: a slot a menu was already published for
        // cannot leave the list — the menu snapshotted the name as text, and a
        // slot no longer offered would leave that menu unreachable from the
        // school's own list.
        if refused.is_none() {
            for gone in missing(&was_slots, &now_slots) {
                let counter = slot_ref(&gone);
                match cap::retire_name(&counter, db).await? {
                    cap::Switched::Flipped => undo.push(Undo::Retired(counter)),
                    cap::Switched::Unchanged => {}
                    cap::Switched::InUse => {
                        refused = Some(AppError::ConflictOwned(format!(
                            "menus are already published for the '{gone}' slot — it cannot be removed"
                        )));
                        break;
                    }
                }
            }
        }
        if let Some(refused) = refused {
            restore(&undo, db).await?;
            return Err(refused);
        }
        // A name re-entering a list is back in service: its counter still
        // carries the retirement from the edit that dropped it, and a mark (or
        // a menu) under a kind the school offers again must not be refused.
        // Recorded for the same undo as the retirements — a re-add whose save
        // does not land leaves a name off the list that grades happily.
        for back in missing(&now_kinds, &was_kinds) {
            if let cap::Switched::Flipped = cap::unretire_name(&kind_ref(&back), db).await? {
                undo.push(Undo::Unretired(kind_ref(&back)));
            }
        }
        for back in missing(&now_slots, &was_slots) {
            if let cap::Switched::Flipped = cap::unretire_name(&slot_ref(&back), db).await? {
                undo.push(Undo::Unretired(slot_ref(&back)));
            }
        }

        match settings.save_if_unchanged(&current, db).await {
            Ok(Some(saved)) => return Ok(saved),
            // The row moved under the snapshot these guards were judged against:
            // put the names back and re-merge, or the next attempt would decide
            // against a list nobody asked for.
            Ok(None) => restore(&undo, db).await?,
            Err(err) => {
                restore(&undo, db).await?;
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

/// One `retired` bit this attempt moved, and which way — what `restore` needs
/// to put it back. Only bits this attempt actually *flipped* are listed: a
/// write that changed nothing has nothing to take back, and taking it back
/// would undo the concurrent edit that really did move it.
enum Undo {
    /// Retired for a removal; put it back in service.
    Retired(RecordId),
    /// Put back in service for a re-add; retire it again.
    Unretired(RecordId),
}

/// Undo every bit this attempt moved: the edit that justified them did not
/// land, so the lists the school offers are still the ones it started with.
async fn restore(undo: &[Undo], db: &Database) -> Result<(), AppError> {
    for step in undo {
        match step {
            Undo::Retired(name) => {
                cap::unretire_name(name, db).await?;
            }
            // A re-retirement can be refused: a mark (or a menu) landed under
            // the name during the window it was in service. Refusing to undo is
            // then the honest answer — the reference is real, and retiring over
            // it would make the row's kind ungradable while nothing lists it.
            // The name is left in service, matching the list this attempt is
            // about to re-read.
            Undo::Unretired(name) => {
                cap::retire_name(name, db).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;

    /// The `retired` bit as the store holds it, `None` when no counter row was
    /// ever written — read back, never inferred from a return value.
    async fn bit(db: &Database, counter: RecordId) -> Option<bool> {
        let mut result = db
            .query("SELECT VALUE retired FROM $id")
            .bind(("id", counter))
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take::<Vec<bool>>(0).unwrap().first().copied()
    }

    /// Two managers drop the same exam kind at once; one of them loses the
    /// row's compare-and-set and rolls its attempt back. The winner's
    /// retirement must survive that rollback — undone, the kind is off the list
    /// with a counter that still grades, and the first mark to land makes it
    /// unremovable for good.
    ///
    /// The loser's half is what runs here: a retirement that changed nothing
    /// records no undo, so `restore` has nothing to take back.
    #[tokio::test]
    async fn the_loser_of_a_settings_race_cannot_un_retire_the_winner_s_kind() {
        let db = init_mem().await.unwrap();
        let counter = kind_ref("midterm");

        // The winner's PATCH retires the kind.
        assert!(matches!(
            cap::retire_name(&counter, &db).await.unwrap(),
            cap::Switched::Flipped
        ));

        // The loser's PATCH decides the same removal against the same snapshot,
        // then its save is refused.
        let mut undo = Vec::new();
        if let cap::Switched::Flipped = cap::retire_name(&counter, &db).await.unwrap() {
            undo.push(Undo::Retired(counter.clone()));
        }
        assert!(undo.is_empty(), "a no-op retirement records no undo");
        restore(&undo, &db).await.unwrap();

        assert_eq!(
            bit(&db, counter).await,
            Some(true),
            "the winner's retirement must outlive the loser's rollback"
        );
    }

    /// The mirror hole: a re-*added* name is put back in service before the
    /// save, so a save that does not land owes a re-retirement. Without it the
    /// name is off the stored list with a counter that grades happily.
    #[tokio::test]
    async fn a_rollback_re_retires_a_name_this_attempt_put_back_in_service() {
        let db = init_mem().await.unwrap();
        let counter = slot_ref("lunch");
        cap::retire_name(&counter, &db).await.unwrap();

        // The attempt re-adds the slot, then its save is refused.
        let mut undo = Vec::new();
        if let cap::Switched::Flipped = cap::unretire_name(&counter, &db).await.unwrap() {
            undo.push(Undo::Unretired(counter.clone()));
        }
        assert_eq!(undo.len(), 1, "a real un-retirement is undoable");
        restore(&undo, &db).await.unwrap();

        assert_eq!(
            bit(&db, counter).await,
            Some(true),
            "a re-add that never landed must leave the slot retired"
        );
    }

    /// ...unless a menu was published in the window the slot was back in
    /// service. The reference is real, so the re-retirement is refused and the
    /// slot stays usable — the honest end of the same rule, not a silent
    /// overwrite of a live reference.
    #[tokio::test]
    async fn a_rollback_leaves_a_name_that_gained_a_reference_in_service() {
        let db = init_mem().await.unwrap();
        let counter = slot_ref("lunch");
        cap::retire_name(&counter, &db).await.unwrap();
        cap::unretire_name(&counter, &db).await.unwrap();
        db.query("UPDATE $id SET count = 1")
            .bind(("id", counter.clone()))
            .await
            .unwrap()
            .check()
            .unwrap();

        restore(&[Undo::Unretired(counter.clone())], &db)
            .await
            .unwrap();

        assert_eq!(bit(&db, counter).await, Some(false));
    }

    // --- the whole edit, driven the way two managers drive it -------------

    /// A `PATCH` body, built the way the router builds one.
    fn patch(body: serde_json::Value) -> UpdateSettings {
        serde_json::from_value(body).unwrap()
    }

    /// A `meal_slots`/`exam_kinds` list as the DTO carries it.
    fn kinds(names: &[&str]) -> serde_json::Value {
        names
            .iter()
            .map(|name| serde_json::json!({ "name": name, "weight": 1 }))
            .collect()
    }

    fn slots(names: &[&str]) -> serde_json::Value {
        names
            .iter()
            .map(|name| serde_json::json!({ "name": name, "serving_minute": null }))
            .collect()
    }

    /// The lists as the store holds them.
    async fn stored(db: &Database) -> (Vec<String>, Vec<String>) {
        let settings = Settings::load(db).await.unwrap();
        (
            kind_names(settings.get_exam_kinds()),
            slot_names(&settings.get_meal_slots()),
        )
    }

    /// The whole invariant, over the reachable half of the race: two managers
    /// drop the same exam kind at once, both edits go through the real path,
    /// and when both have returned the counter's `retired` bit must agree with
    /// the list the store actually holds.
    ///
    /// One of the two decides the removal, retires the kind and saves; the
    /// other cannot even see the old list, because the whole snapshot-retire-
    /// save pair is serialized (`SETTINGS_LOCK`). Interleaved, the second
    /// attempt would be told "already retired", record no undo, win the
    /// compare-and-set, and leave the first attempt un-retiring the kind it had
    /// legitimately removed — off the list and gradable.
    #[tokio::test]
    async fn two_edits_dropping_the_same_kind_leave_it_retired() {
        let db = init_mem().await.unwrap();
        let without = patch(serde_json::json!({
            "exam_kinds": kinds(&["homework", "quiz", "final", "project", "oral"]),
        }));

        let (a, b) = tokio::join!(apply(&without, &db), apply(&without, &db));
        a.unwrap();
        b.unwrap();

        let (kinds, _) = stored(&db).await;
        assert!(
            !kinds.contains(&"midterm".to_string()),
            "dropped from the list"
        );
        assert_eq!(
            bit(&db, kind_ref("midterm")).await,
            Some(true),
            "a kind the stored list no longer offers must not grade"
        );
    }

    /// The property those two outcomes rest on, asserted on its own because a
    /// `join!` cannot be made to schedule the losing order: the edit is a pair
    /// of writes — retire the dropped names, then commit the list — and no
    /// rival's pair may run between them.
    ///
    /// Retirement is idempotent, so an interleaved rival is told "already
    /// retired", records no rollback, wins the compare-and-set, and leaves the
    /// attempt that really flipped the bit un-retiring a kind the stored list
    /// no longer offers: gradable again, and unremovable for good once a mark
    /// lands. Holding the lock stands in for a rival mid-pair here — nothing
    /// the other edit does may be visible until the pair completes.
    #[tokio::test]
    async fn a_rival_edit_cannot_run_between_a_retirement_and_its_save() {
        let db = init_mem().await.unwrap();
        let without = patch(serde_json::json!({
            "exam_kinds": kinds(&["homework", "quiz", "final", "project", "oral"]),
        }));

        let held = SETTINGS_LOCK.lock().await;
        let rival = tokio::spawn({
            let db = db.clone();
            async move { apply(&without, &db).await.map(|_| ()) }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(
            bit(&db, kind_ref("midterm")).await,
            None,
            "a rival's pair must not have started, let alone half-landed"
        );
        assert!(stored(&db).await.0.contains(&"midterm".to_string()));

        drop(held);
        rival.await.unwrap().unwrap();
        assert_eq!(bit(&db, kind_ref("midterm")).await, Some(true));
        assert!(!stored(&db).await.0.contains(&"midterm".to_string()));
    }

    /// The mirror: two managers put the same kind *back* at once. The attempt
    /// that loses must not re-retire a name the winner's stored list carries —
    /// a listed kind that refuses every grade.
    #[tokio::test]
    async fn two_edits_re_adding_the_same_kind_leave_it_in_service() {
        let db = init_mem().await.unwrap();
        let without = patch(serde_json::json!({
            "exam_kinds": kinds(&["homework", "quiz", "final", "project", "oral"]),
        }));
        apply(&without, &db).await.unwrap();
        let with = patch(serde_json::json!({
            "exam_kinds": kinds(&["homework", "quiz", "midterm", "final", "project", "oral"]),
        }));

        let (a, b) = tokio::join!(apply(&with, &db), apply(&with, &db));
        a.unwrap();
        b.unwrap();

        let (kinds, _) = stored(&db).await;
        assert!(kinds.contains(&"midterm".to_string()), "back on the list");
        assert_eq!(
            bit(&db, kind_ref("midterm")).await,
            Some(false),
            "a kind the stored list offers must grade"
        );
    }

    /// The same pair over meal slots, which retire on their own counters.
    #[tokio::test]
    async fn two_edits_dropping_the_same_slot_leave_it_retired() {
        let db = init_mem().await.unwrap();
        let without = patch(serde_json::json!({
            "meal_slots": slots(&["breakfast", "snack"]),
        }));

        let (a, b) = tokio::join!(apply(&without, &db), apply(&without, &db));
        a.unwrap();
        b.unwrap();

        let (_, stored_slots) = stored(&db).await;
        assert!(!stored_slots.contains(&"lunch".to_string()));
        assert_eq!(bit(&db, slot_ref("lunch")).await, Some(true));
    }

    // --- the stale slot name that wedged the whole list -------------------

    /// A school that stored `a/b` before the URL-safety rule existed.
    async fn a_school_with_a_stale_slot(db: &Database) {
        let mut params = Settings::defaults().params();
        params.meal_slots = vec![
            MealSlotDef::try_kept("a/b", None).unwrap(),
            MealSlotDef::try_new("lunch", None).unwrap(),
        ];
        Settings::try_new(params).unwrap().save(db).await.unwrap();
    }

    /// The wedge: re-sending a stored name is the only way to *keep* it, and
    /// dropping it 409s once a menu references it — so validating it under the
    /// younger rule locks the school out of its own `meal_slots` forever. A
    /// stored name is submittable; a new one with the same characters is not.
    #[tokio::test]
    async fn a_stored_slot_name_stays_submittable_but_a_new_one_does_not() {
        let db = init_mem().await.unwrap();
        a_school_with_a_stale_slot(&db).await;

        let edit = patch(serde_json::json!({
            "meal_slots": slots(&["a/b", "lunch", "dinner"]),
        }));
        apply(&edit, &db).await.unwrap();
        let (_, stored_slots) = stored(&db).await;
        assert_eq!(stored_slots, ["a/b", "lunch", "dinner"]);

        let fresh = patch(serde_json::json!({
            "meal_slots": slots(&["a/b", "lunch", "c/d"]),
        }));
        assert!(
            apply(&fresh, &db).await.is_err(),
            "a name no school ever stored is still refused"
        );
    }

    /// And an edit that never mentions `meal_slots` still carries the list
    /// over unvalidated — the behaviour the wedge report leaned on.
    #[tokio::test]
    async fn an_unrelated_edit_carries_a_stale_slot_name_over() {
        let db = init_mem().await.unwrap();
        a_school_with_a_stale_slot(&db).await;

        let elsewhere = patch(serde_json::json!({ "max_file_bytes": 2048 }));
        let saved = apply(&elsewhere, &db).await.unwrap();

        assert_eq!(saved.get_max_file_bytes(), 2048);
        assert_eq!(slot_names(&saved.get_meal_slots()), ["a/b", "lunch"]);
    }
}
