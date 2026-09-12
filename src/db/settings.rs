//! The `settings` singleton: one row (`settings`, id `'school'`) holding the
//! school's policy knobs. An absent row reads as the built-in defaults
//! everywhere.

use sqlx::types::Json;

use crate::database::Database;
use crate::domain::settings::{ExamKindDef, GradeBand, MealSlotDef, Settings};
use crate::error::AppError;

pub async fn load(db: &Database) -> Result<Settings, AppError> {
    let settings = sqlx::query_as!(
        Settings,
        r#"SELECT
                  exam_kinds AS "exam_kinds: Json<Vec<ExamKindDef>>",
                  attendance_statuses,
                  grade_bands AS "grade_bands: Json<Vec<GradeBand>>",
                  max_file_bytes,
                  chatbot_history_turns,
                  max_chatbot_message_len,
                  max_chatbot_threads,
                  meal_slots AS "meal_slots: Json<Vec<MealSlotDef>>",
                  dietary_tags,
                  meal_cancel_cutoff_minutes
           FROM settings WHERE id = 'school'"#
    )
    .fetch_optional(db)
    .await?;
    Ok(settings.unwrap_or_else(Settings::defaults))
}

/// Persist the policy (single UPSERT on the fixed singleton id),
/// unconditionally — last write wins. Prefer [`save_if_unchanged`]
/// wherever the new policy was merged from a loaded snapshot.
pub async fn save(db: &Database, settings: Settings) -> Result<Settings, AppError> {
    // whole-row-save-ok: test-only seeding; every production write merges from a loaded snapshot and goes through save_if_unchanged
    let exam_kinds = serde_json::to_value(&settings.exam_kinds.0)
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let grade_bands = serde_json::to_value(&settings.grade_bands.0)
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let meal_slots = settings
        .meal_slots
        .as_ref()
        .map(|slots| serde_json::to_value(&slots.0))
        .transpose()
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let saved = sqlx::query_as!(
        Settings,
        r#"INSERT INTO settings (id, exam_kinds, attendance_statuses, grade_bands,
                                 max_file_bytes, chatbot_history_turns,
                                 max_chatbot_message_len, max_chatbot_threads,
                                 meal_slots, dietary_tags, meal_cancel_cutoff_minutes)
           VALUES ('school', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (id) DO UPDATE SET
               exam_kinds = EXCLUDED.exam_kinds,
               attendance_statuses = EXCLUDED.attendance_statuses,
               grade_bands = EXCLUDED.grade_bands,
               max_file_bytes = EXCLUDED.max_file_bytes,
               chatbot_history_turns = EXCLUDED.chatbot_history_turns,
               max_chatbot_message_len = EXCLUDED.max_chatbot_message_len,
               max_chatbot_threads = EXCLUDED.max_chatbot_threads,
               meal_slots = EXCLUDED.meal_slots,
               dietary_tags = EXCLUDED.dietary_tags,
               meal_cancel_cutoff_minutes = EXCLUDED.meal_cancel_cutoff_minutes
           RETURNING
                     exam_kinds AS "exam_kinds: Json<Vec<ExamKindDef>>",
                     attendance_statuses,
                     grade_bands AS "grade_bands: Json<Vec<GradeBand>>",
                     max_file_bytes,
                     chatbot_history_turns,
                     max_chatbot_message_len,
                     max_chatbot_threads,
                     meal_slots AS "meal_slots: Json<Vec<MealSlotDef>>",
                     dietary_tags,
                     meal_cancel_cutoff_minutes"#,
        exam_kinds,
        settings.attendance_statuses.as_slice(),
        grade_bands,
        settings.max_file_bytes,
        settings.chatbot_history_turns,
        settings.max_chatbot_message_len,
        settings.max_chatbot_threads,
        meal_slots,
        settings.dietary_tags.as_deref(),
        settings.meal_cancel_cutoff_minutes,
    )
    .fetch_one(db)
    .await?;
    Ok(saved)
}

/// Persist the policy only while the stored row still matches `expected`
/// — the snapshot the caller merged omitted fields from. `None` means a
/// concurrent edit landed in between and nothing was written: reload,
/// re-merge, retry. Without this compare-and-set, two managers patching
/// *different* fields silently revert each other (both merge from the
/// same snapshot; the later whole-row write restores its stale copy of
/// the other's field).
///
/// ONE guarded statement, where the old engine needed a seed insert and a
/// separate guarded update: the `INSERT` arm materializes the defaults-as-
/// loaded state when no row exists yet (`load` reported the defaults, so
/// the defaults are what the caller merged over — nothing to compare), and
/// the `ON CONFLICT DO UPDATE … WHERE` arm applies `settings` only if the
/// row (still) equals `expected`, every column compared with
/// `IS NOT DISTINCT FROM` so "never set" (`NULL`) stays distinct from
/// "explicitly empty" and an unset column matches an unset expectation.
///
/// The old `meal_slots` projection dance was an artifact of the old engine
/// *dropping* object keys whose value was `NONE` on write, which made a
/// plain equality guard never match a slot without a serving time. Both
/// sides of this comparison are written by the same Rust serialization,
/// which round-trips through JSONB unchanged — the shapes cannot diverge,
/// so the columns are compared whole.
///
/// Generic over the executor so the workflow can run the same guarded save
/// inside its own transaction (a stale save must roll its retirements back,
/// which only an abort can do).
pub async fn save_if_unchanged<'e, E>(
    db: E,
    settings: Settings,
    expected: &Settings,
) -> Result<Option<Settings>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let exam_kinds = serde_json::to_value(&settings.exam_kinds.0)
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let grade_bands = serde_json::to_value(&settings.grade_bands.0)
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let meal_slots = settings
        .meal_slots
        .as_ref()
        .map(|slots| serde_json::to_value(&slots.0))
        .transpose()
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let expected_exam_kinds = serde_json::to_value(&expected.exam_kinds.0)
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let expected_grade_bands = serde_json::to_value(&expected.grade_bands.0)
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let expected_meal_slots = expected
        .meal_slots
        .as_ref()
        .map(|slots| serde_json::to_value(&slots.0))
        .transpose()
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))?;
    let saved = sqlx::query_as!(
        Settings,
        r#"INSERT INTO settings (id, exam_kinds, attendance_statuses, grade_bands,
                                 max_file_bytes, chatbot_history_turns,
                                 max_chatbot_message_len, max_chatbot_threads,
                                 meal_slots, dietary_tags, meal_cancel_cutoff_minutes)
           VALUES ('school', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (id) DO UPDATE SET
               exam_kinds = $1,
               attendance_statuses = $2,
               grade_bands = $3,
               max_file_bytes = $4,
               chatbot_history_turns = $5,
               max_chatbot_message_len = $6,
               max_chatbot_threads = $7,
               meal_slots = $8,
               dietary_tags = $9,
               meal_cancel_cutoff_minutes = $10
           WHERE settings.exam_kinds                 IS NOT DISTINCT FROM $11
             AND settings.attendance_statuses        IS NOT DISTINCT FROM $12
             AND settings.grade_bands                IS NOT DISTINCT FROM $13
             AND settings.max_file_bytes             IS NOT DISTINCT FROM $14
             AND settings.chatbot_history_turns      IS NOT DISTINCT FROM $15
             AND settings.max_chatbot_message_len    IS NOT DISTINCT FROM $16
             AND settings.max_chatbot_threads        IS NOT DISTINCT FROM $17
             AND settings.meal_slots                 IS NOT DISTINCT FROM $18
             AND settings.dietary_tags               IS NOT DISTINCT FROM $19
             AND settings.meal_cancel_cutoff_minutes IS NOT DISTINCT FROM $20
           RETURNING
                     exam_kinds AS "exam_kinds: Json<Vec<ExamKindDef>>",
                     attendance_statuses,
                     grade_bands AS "grade_bands: Json<Vec<GradeBand>>",
                     max_file_bytes,
                     chatbot_history_turns,
                     max_chatbot_message_len,
                     max_chatbot_threads,
                     meal_slots AS "meal_slots: Json<Vec<MealSlotDef>>",
                     dietary_tags,
                     meal_cancel_cutoff_minutes"#,
        exam_kinds,
        settings.attendance_statuses.as_slice(),
        grade_bands,
        settings.max_file_bytes,
        settings.chatbot_history_turns,
        settings.max_chatbot_message_len,
        settings.max_chatbot_threads,
        meal_slots,
        settings.dietary_tags.as_deref(),
        settings.meal_cancel_cutoff_minutes,
        expected_exam_kinds,
        expected.attendance_statuses.as_slice(),
        expected_grade_bands,
        expected.max_file_bytes,
        expected.chatbot_history_turns,
        expected.max_chatbot_message_len,
        expected.max_chatbot_threads,
        expected_meal_slots,
        expected.dietary_tags.as_deref(),
        expected.meal_cancel_cutoff_minutes,
    )
    .fetch_optional(db)
    .await?;
    // An empty result means the row no longer matched `expected`.
    Ok(saved)
}

#[cfg(test)]
mod tests {
    // Ported in wave 3: they need a live school database.
}
