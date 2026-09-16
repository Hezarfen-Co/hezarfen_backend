//! The `settings` singleton: one row (`settings`, id `'school'`) holding the
//! school's policy knobs. An absent row reads as the built-in defaults
//! everywhere.
//!
//! The two vocabulary lists (`attendance_statuses`, `dietary_tags`) are no
//! longer array columns: they live in the child tables
//! `settings_attendance_status` and `settings_dietary_tag`, one row per
//! entry. The junction keeps no order, so every read sorts by entry and the
//! compare-and-set guard below compares row *sets*, not bytes.

use sqlx::types::Json;

use crate::database::{Database, tx_with_retry};
use crate::domain::settings::{ExamKindDef, GradeBand, MealSlotDef, Settings};
use crate::error::AppError;

/// Both sides of every set compare are sorted: the child tables keep no
/// order, and set equality does not want one.
fn sorted(values: &[String]) -> Vec<String> {
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted
}

/// One optional JSONB list as the value a statement binds: the list when the
/// setting carries one, SQL `NULL` when it does not.
fn encode_optional(
    values: &Option<Json<Vec<String>>>,
) -> Result<Option<serde_json::Value>, AppError> {
    values
        .as_ref()
        .map(|list| serde_json::to_value(&list.0))
        .transpose()
        .map_err(|e| AppError::Internal(format!("settings encode: {e}")))
}

/// The one read: row columns plus both vocabularies re-assembled from their
/// child tables. An absent dietary set reads as `None` — the same shape an
/// unset column used to carry.
async fn load_row<'e, E>(db: E) -> Result<Option<Settings>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let settings = sqlx::query_as!(
        Settings,
        r#"SELECT s.exam_kinds AS "exam_kinds: Json<Vec<ExamKindDef>>",
                  COALESCE((SELECT array_agg(t.status ORDER BY t.status)
                            FROM settings_attendance_status t
                            WHERE t.settings = s.id), '{}') AS "attendance_statuses!: Vec<String>",
                  s.grade_bands AS "grade_bands: Json<Vec<GradeBand>>",
                  s.max_file_bytes,
                  s.chatbot_history_turns,
                  s.max_chatbot_message_len,
                  s.max_chatbot_threads,
                  s.meal_slots AS "meal_slots: Json<Vec<MealSlotDef>>",
                  (SELECT array_agg(t.tag ORDER BY t.tag)
                   FROM settings_dietary_tag t
                   WHERE t.settings = s.id) AS dietary_tags,
                  s.meal_cancel_cutoff_minutes,
                  s.branches AS "branches: Json<Vec<String>>",
                  s.excuse_kinds AS "excuse_kinds: Json<Vec<String>>",
                  s.max_excused_absent_days,
                  s.max_unexcused_absent_days,
                  s.timezone
           FROM settings s WHERE s.id = 'school'"#
    )
    .fetch_optional(db)
    .await?;
    Ok(settings)
}

pub async fn load(db: &Database) -> Result<Settings, AppError> {
    let settings = load_row(db).await?;
    Ok(settings.unwrap_or_else(Settings::defaults))
}

/// Replace the attendance vocabulary wholesale. Callers run inside a
/// transaction (or inside the single CAS statement), so the delete and the
/// insert cannot be observed apart.
async fn replace_statuses(
    conn: &mut sqlx::PgConnection,
    statuses: &[String],
) -> Result<(), AppError> {
    sqlx::query!("DELETE FROM settings_attendance_status WHERE settings = 'school'")
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "INSERT INTO settings_attendance_status (settings, status)
         SELECT 'school', t.status FROM unnest($1::text[]) AS t(status)",
        statuses,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Replace the dietary vocabulary wholesale. `None` and `Some(empty)` both
/// write zero rows — "explicitly empty" and "never set" are the same state
/// on the child tables.
async fn replace_tags(
    conn: &mut sqlx::PgConnection,
    tags: Option<&[String]>,
) -> Result<(), AppError> {
    sqlx::query!("DELETE FROM settings_dietary_tag WHERE settings = 'school'")
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "INSERT INTO settings_dietary_tag (settings, tag)
         SELECT 'school', t.tag FROM unnest($1::text[]) AS t(tag)",
        tags,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Persist the policy (single UPSERT on the fixed singleton id plus a
/// wholesale rewrite of both vocabularies, one transaction), unconditionally
/// — last write wins. Prefer [`save_if_unchanged`] wherever the new policy
/// was merged from a loaded snapshot.
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
    // The two JSONB vocabularies ride the row like `meal_slots`: an unset
    // list stays NULL rather than being coerced to `[]`, because a whole-row
    // save that wrote `[]` for a field it never carried would erase the
    // distinction an unset column carries.
    let branches = encode_optional(&settings.branches)?;
    let excuse_kinds = encode_optional(&settings.excuse_kinds)?;
    let attendance = sorted(&settings.attendance_statuses);
    let dietary = settings.dietary_tags.clone();
    tx_with_retry(db, true, async move |tx| {
        sqlx::query!(
            r#"INSERT INTO settings (id, exam_kinds, grade_bands, max_file_bytes,
                                     chatbot_history_turns, max_chatbot_message_len,
                                     max_chatbot_threads, meal_slots,
                                     meal_cancel_cutoff_minutes, branches, excuse_kinds,
                                     max_excused_absent_days, max_unexcused_absent_days,
                                     timezone)
               VALUES ('school', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
               ON CONFLICT (id) DO UPDATE SET
                   exam_kinds = EXCLUDED.exam_kinds,
                   grade_bands = EXCLUDED.grade_bands,
                   max_file_bytes = EXCLUDED.max_file_bytes,
                   chatbot_history_turns = EXCLUDED.chatbot_history_turns,
                   max_chatbot_message_len = EXCLUDED.max_chatbot_message_len,
                   max_chatbot_threads = EXCLUDED.max_chatbot_threads,
                   meal_slots = EXCLUDED.meal_slots,
                   meal_cancel_cutoff_minutes = EXCLUDED.meal_cancel_cutoff_minutes,
                   branches = EXCLUDED.branches,
                   excuse_kinds = EXCLUDED.excuse_kinds,
                   max_excused_absent_days = EXCLUDED.max_excused_absent_days,
                   max_unexcused_absent_days = EXCLUDED.max_unexcused_absent_days,
                   timezone = EXCLUDED.timezone"#,
            exam_kinds,
            grade_bands,
            settings.max_file_bytes,
            settings.chatbot_history_turns,
            settings.max_chatbot_message_len,
            settings.max_chatbot_threads,
            meal_slots,
            settings.meal_cancel_cutoff_minutes,
            branches,
            excuse_kinds,
            settings.max_excused_absent_days,
            settings.max_unexcused_absent_days,
            settings.timezone,
        )
        .execute(&mut *tx)
        .await?;
        replace_statuses(&mut *tx, &attendance).await?;
        replace_tags(&mut *tx, dietary.as_deref()).await?;
        load_row(&mut *tx).await?.ok_or(AppError::NotFound)
    })
    .await
}

/// Persist the policy only while the stored row still matches `expected`
/// — the snapshot the caller merged omitted fields from. `None` means a
/// concurrent edit landed in between and nothing was written: reload,
/// re-merge, retry. Without this compare-and-set, two managers patching
/// *different* fields silently revert each other (both merge from the
/// same snapshot; the later whole-row write restores its stale copy of
/// the other's field).
///
/// ONE guarded statement, generic over the executor so the workflow can run
/// the same guarded save inside its own transaction (a stale save must roll
/// its retirements back, which only an abort can do). The `INSERT` arm
/// materializes the defaults-as-loaded state when no row exists yet (`load`
/// reported the defaults, so the defaults are what the caller merged over —
/// nothing to compare), and the `ON CONFLICT DO UPDATE … WHERE` arm applies
/// `settings` only if the stored state (still) equals `expected`: row
/// columns each compared with `IS NOT DISTINCT FROM`, and the two
/// vocabularies compared as sorted aggregates over their child tables — the
/// row *sets*, since the junctions keep no order. The settings row's own
/// lock is the serialization point: a waiter for the same row re-runs the
/// whole guard against the winner's committed state, and its child-table
/// subqueries then read the winner's committed vocabulary.
///
/// The vocabulary rewrites are CTEs gated on the row write having produced a
/// row (`new_row`), and each insert waits for its delete through the
/// delete's RETURNING count — data-modifying CTEs share one snapshot, so
/// without that chain the insert could race its own delete and drop rows.
/// The returned row's vocabulary comes from the inserts' RETURNING, not a
/// re-read: the statement cannot see its own writes to the child tables.
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
    let branches = encode_optional(&settings.branches)?;
    let excuse_kinds = encode_optional(&settings.excuse_kinds)?;
    let expected_branches = encode_optional(&expected.branches)?;
    let expected_excuse_kinds = encode_optional(&expected.excuse_kinds)?;
    let attendance = sorted(&settings.attendance_statuses);
    let expected_attendance = sorted(&expected.attendance_statuses);
    // An empty `Some` must compare equal to the stored NULL an empty set
    // reads as, so it is normalized away on the expected side.
    let expected_dietary = expected
        .dietary_tags
        .as_ref()
        .filter(|tags| !tags.is_empty())
        .map(|tags| tags.as_slice());
    let saved = sqlx::query_as!(
        Settings,
        r#"WITH new_row AS (
               INSERT INTO settings (id, exam_kinds, grade_bands, max_file_bytes,
                                     chatbot_history_turns, max_chatbot_message_len,
                                     max_chatbot_threads, meal_slots,
                                     meal_cancel_cutoff_minutes, branches, excuse_kinds,
                                     max_excused_absent_days, max_unexcused_absent_days,
                                     timezone)
               VALUES ('school', $1, $3, $4, $5, $6, $7, $8, $10, $21, $22, $23, $24, $25)
               ON CONFLICT (id) DO UPDATE SET
                   exam_kinds = $1,
                   grade_bands = $3,
                   max_file_bytes = $4,
                   chatbot_history_turns = $5,
                   max_chatbot_message_len = $6,
                   max_chatbot_threads = $7,
                   meal_slots = $8,
                   meal_cancel_cutoff_minutes = $10,
                   branches = $21,
                   excuse_kinds = $22,
                   max_excused_absent_days = $23,
                   max_unexcused_absent_days = $24,
                   timezone = $25
               WHERE settings.exam_kinds                 IS NOT DISTINCT FROM $13
                 AND settings.grade_bands                IS NOT DISTINCT FROM $14
                 AND settings.max_file_bytes             IS NOT DISTINCT FROM $15
                 AND settings.chatbot_history_turns      IS NOT DISTINCT FROM $16
                 AND settings.max_chatbot_message_len    IS NOT DISTINCT FROM $17
                 AND settings.max_chatbot_threads        IS NOT DISTINCT FROM $18
                 AND settings.meal_slots                 IS NOT DISTINCT FROM $19
                 AND settings.meal_cancel_cutoff_minutes IS NOT DISTINCT FROM $20
                 AND settings.branches                   IS NOT DISTINCT FROM $26
                 AND settings.excuse_kinds               IS NOT DISTINCT FROM $27
                 AND settings.max_excused_absent_days    IS NOT DISTINCT FROM $28
                 AND settings.max_unexcused_absent_days  IS NOT DISTINCT FROM $29
                 AND settings.timezone                   IS NOT DISTINCT FROM $30
                 AND COALESCE((SELECT array_agg(s.status ORDER BY s.status)
                               FROM settings_attendance_status s
                               WHERE s.settings = settings.id), '{}')
                     IS NOT DISTINCT FROM $11::text[]
                 AND (SELECT array_agg(t.tag ORDER BY t.tag)
                      FROM settings_dietary_tag t
                      WHERE t.settings = settings.id)
                     IS NOT DISTINCT FROM $12::text[]
               RETURNING exam_kinds, grade_bands, max_file_bytes,
                         chatbot_history_turns, max_chatbot_message_len,
                         max_chatbot_threads, meal_slots, meal_cancel_cutoff_minutes,
                         branches, excuse_kinds, max_excused_absent_days,
                         max_unexcused_absent_days, timezone),
           status_del AS (
               DELETE FROM settings_attendance_status
               WHERE settings = 'school' AND EXISTS (SELECT 1 FROM new_row)
               RETURNING 1),
           status_ins AS (
               INSERT INTO settings_attendance_status (settings, status)
               SELECT 'school', t.status FROM unnest($2::text[]) AS t(status)
                    CROSS JOIN (SELECT count(*) AS deleted FROM status_del) d
               WHERE EXISTS (SELECT 1 FROM new_row)
               RETURNING status),
           tag_del AS (
               DELETE FROM settings_dietary_tag
               WHERE settings = 'school' AND EXISTS (SELECT 1 FROM new_row)
               RETURNING 1),
           tag_ins AS (
               INSERT INTO settings_dietary_tag (settings, tag)
               SELECT 'school', t.tag FROM unnest($9::text[]) AS t(tag)
                    CROSS JOIN (SELECT count(*) AS deleted FROM tag_del) d
               WHERE EXISTS (SELECT 1 FROM new_row)
               RETURNING tag)
           SELECT r.exam_kinds AS "exam_kinds: Json<Vec<ExamKindDef>>",
                  COALESCE((SELECT array_agg(status ORDER BY status)
                            FROM status_ins), '{}') AS "attendance_statuses!: Vec<String>",
                  r.grade_bands AS "grade_bands: Json<Vec<GradeBand>>",
                  r.max_file_bytes,
                  r.chatbot_history_turns,
                  r.max_chatbot_message_len,
                  r.max_chatbot_threads,
                  r.meal_slots AS "meal_slots: Json<Vec<MealSlotDef>>",
                  (SELECT array_agg(tag ORDER BY tag) FROM tag_ins) AS dietary_tags,
                  r.meal_cancel_cutoff_minutes,
                  r.branches AS "branches: Json<Vec<String>>",
                  r.excuse_kinds AS "excuse_kinds: Json<Vec<String>>",
                  r.max_excused_absent_days,
                  r.max_unexcused_absent_days,
                  r.timezone
           FROM new_row r"#,
        exam_kinds,
        &attendance,
        grade_bands,
        settings.max_file_bytes,
        settings.chatbot_history_turns,
        settings.max_chatbot_message_len,
        settings.max_chatbot_threads,
        meal_slots,
        settings.dietary_tags.as_deref(),
        settings.meal_cancel_cutoff_minutes,
        &expected_attendance,
        expected_dietary,
        expected_exam_kinds,
        expected_grade_bands,
        expected.max_file_bytes,
        expected.chatbot_history_turns,
        expected.max_chatbot_message_len,
        expected.max_chatbot_threads,
        expected_meal_slots,
        expected.meal_cancel_cutoff_minutes,
        branches,
        excuse_kinds,
        settings.max_excused_absent_days,
        settings.max_unexcused_absent_days,
        settings.timezone,
        expected_branches,
        expected_excuse_kinds,
        expected.max_excused_absent_days,
        expected.max_unexcused_absent_days,
        expected.timezone,
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
