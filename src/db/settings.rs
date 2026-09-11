//! The `settings` singleton: one row (`settings:school`) holding the school's
//! policy knobs. An absent row reads as the built-in defaults everywhere.

use crate::database::Database;
use crate::domain::settings::Settings;
use crate::error::AppError;

/// The stored policy, or the defaults when no row exists yet.
pub async fn load(db: &Database) -> Result<Settings, AppError> {
    let found: Option<Settings> = db.select(Settings::record_id()).await?;
    Ok(found.unwrap_or_else(Settings::defaults))
}

/// Persist the policy (single UPSERT on the fixed singleton id),
/// unconditionally — last write wins. Prefer [`save_if_unchanged`]
/// wherever the new policy was merged from a loaded snapshot.
pub async fn save(db: &Database, settings: Settings) -> Result<Settings, AppError> {
    // whole-row-save-ok: test-only seeding; every production write merges from a loaded snapshot and goes through save_if_unchanged
    let saved: Option<Settings> = db.upsert(Settings::record_id()).content(settings).await?;
    saved.ok_or_else(|| AppError::Internal("failed to save settings".into()))
}

/// Persist the policy only while the stored row still matches `expected`
/// — the snapshot the caller merged omitted fields from. `None` means a
/// concurrent edit landed in between and nothing was written: reload,
/// re-merge, retry. Without this compare-and-set, two managers patching
/// *different* fields silently revert each other (both merge from the
/// same snapshot; the later whole-row write restores its stale copy of
/// the other's field).
///
/// One transaction: the seed insert materializes the defaults-as-loaded
/// state when no row exists yet (`load` reported the defaults, so the
/// defaults are what the caller merged over), then the guarded update
/// applies `settings` only if the row (still) equals `expected`.
///
/// **`meal_slots` is compared through a projection, and it has to be.**
/// SurrealDB *drops* an object key whose value is `NONE` on write, while
/// the `SurrealValue` derive always emits `serving_minute: NONE` for a
/// slot without one — so `{name: 'lunch'} = {name: 'lunch', serving_minute:
/// NONE}` is **false** and a plain equality guard would never match again
/// for any school with a serving-time-less slot (which is every school
/// until it sets one): every `PATCH /settings` would 409 forever. Rebuilding
/// both sides as full objects normalizes the shapes. The NONE-ness of the
/// column itself is compared separately, so "never set" stays distinguishable
/// from "explicitly empty". Top-level optional columns need none of this:
/// a missing field reads as `NONE`, and `NONE = NONE` holds.
pub async fn save_if_unchanged(
    db: &Database,
    settings: Settings,
    expected: &Settings,
) -> Result<Option<Settings>, AppError> {
    let mut result = db
        .query(
            "BEGIN TRANSACTION;
             INSERT IGNORE INTO settings $expected;
             UPDATE $id CONTENT $new
                 WHERE exam_kinds = $ek
                   AND attendance_statuses = $st
                   AND grade_bands = $gb
                   AND max_file_bytes = $mf
                   AND chatbot_history_turns = $ct
                   AND max_chatbot_threads = $cc
                   AND max_chatbot_message_len = $cl
                   AND (meal_slots = NONE) = $ms_unset
                   AND (meal_slots ?? []).map(|$s| {
                           name: $s.name,
                           serving_minute: $s.serving_minute
                       }) = ($ms ?? [])
                   AND dietary_tags = $dt
                   AND meal_cancel_cutoff_minutes = $mc;
             COMMIT TRANSACTION;",
        )
        .bind(("expected", expected.clone()))
        .bind(("id", Settings::record_id()))
        .bind(("new", settings))
        .bind(("ek", expected.exam_kinds.clone()))
        .bind(("st", expected.attendance_statuses.clone()))
        .bind(("gb", expected.grade_bands.clone()))
        .bind(("mf", expected.max_file_bytes))
        .bind(("ct", expected.chatbot_history_turns))
        .bind(("cc", expected.max_chatbot_threads))
        .bind(("cl", expected.max_chatbot_message_len))
        .bind(("ms_unset", expected.meal_slots.is_none()))
        .bind(("ms", expected.meal_slots.clone()))
        .bind(("dt", expected.dietary_tags.clone()))
        .bind(("mc", expected.meal_cancel_cutoff_minutes))
        .await?
        .check()?;
    // Statement slots count BEGIN too: the guarded UPDATE is slot 2. An
    // empty slot means the row no longer matched `expected`.
    Ok(result.take::<Vec<Settings>>(2)?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{
        DEFAULT_CHATBOT_HISTORY_TURNS, DEFAULT_MAX_CHATBOT_MESSAGE_LEN,
        DEFAULT_MAX_CHATBOT_THREADS, DEFAULT_MAX_FILE_BYTES,
    };
    use crate::domain::settings::{ExamKindDef, GradeBand, SettingsParams};

    fn kinds(list: &[&str]) -> Vec<ExamKindDef> {
        list.iter()
            .map(|s| ExamKindDef::try_new(s, 1).unwrap())
            .collect()
    }

    fn names(settings: &Settings) -> Vec<&str> {
        settings
            .get_exam_kinds()
            .iter()
            .map(ExamKindDef::get_name)
            .collect()
    }

    /// The defaults as tweakable params — every site below overrides only the
    /// fields it is testing (`..params()`).
    fn params() -> SettingsParams {
        Settings::defaults().params()
    }

    fn bands(list: &[(i64, &str)]) -> Vec<GradeBand> {
        list.iter()
            .map(|(min, label)| GradeBand::try_new(*min, label).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_row_predating_the_optional_knobs_reads_the_defaults() {
        let db = crate::database::init_mem().await.unwrap();
        // The defaults carry no explicit knobs, so this writes a row without
        // those fields — exactly what a volume from before them looks like.
        save(&db, Settings::defaults()).await.unwrap();
        let loaded = load(&db).await.unwrap();
        assert_eq!(loaded.get_max_file_bytes(), DEFAULT_MAX_FILE_BYTES);
        assert_eq!(
            loaded.get_chatbot_history_turns(),
            DEFAULT_CHATBOT_HISTORY_TURNS
        );
        assert_eq!(
            loaded.get_max_chatbot_threads(),
            DEFAULT_MAX_CHATBOT_THREADS
        );
        assert_eq!(
            loaded.get_max_chatbot_message_len(),
            DEFAULT_MAX_CHATBOT_MESSAGE_LEN
        );
        // And a snapshot of that old row still passes the compare-and-set.
        let saved = save_if_unchanged(
            &db,
            Settings::try_new(SettingsParams {
                max_file_bytes: 4096,
                ..loaded.params()
            })
            .unwrap(),
            &loaded,
        )
        .await
        .unwrap()
        .expect("a merge over an old-shape row applies");
        assert_eq!(saved.get_max_file_bytes(), 4096);
    }

    #[tokio::test]
    async fn load_save_roundtrip_on_the_singleton() {
        let db = crate::database::init_mem().await.unwrap();
        // No row yet → the defaults, not an error.
        let loaded = load(&db).await.unwrap();
        assert_eq!(
            loaded.get_exam_kinds(),
            Settings::defaults().get_exam_kinds()
        );
        // Save a custom policy and read it back — bands (nested objects under
        // a FLEXIBLE field) must survive the trip.
        save(
            &db,
            Settings::try_new(SettingsParams {
                exam_kinds: kinds(&["lab"]),
                grade_bands: bands(&[(0, "F"), (50, "P")]),
                max_file_bytes: 2048,
                chatbot_history_turns: 3,
                ..params()
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let loaded = load(&db).await.unwrap();
        assert_eq!(names(&loaded), ["lab"]);
        assert_eq!(loaded.get_grade_bands().len(), 2);
        assert_eq!(loaded.grade_label(60.0), Some("P"));
        assert_eq!(loaded.get_max_file_bytes(), 2048);
        assert_eq!(loaded.get_chatbot_history_turns(), 3);
        // A second save lands on the same singleton row, not a new one.
        save(
            &db,
            Settings::try_new(SettingsParams {
                exam_kinds: kinds(&["quiz"]),
                ..params()
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let mut result = db.query("SELECT * FROM settings").await.unwrap();
        let rows: Vec<Settings> = result.take(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(names(&rows[0]), ["quiz"]);
    }

    #[tokio::test]
    async fn a_stale_snapshot_cannot_revert_a_newer_policy() {
        let db = crate::database::init_mem().await.unwrap();

        // Editor A snapshots the policy (the defaults — no row yet)...
        let stale = load(&db).await.unwrap();
        // ...then editor B lands a new exam-kind list first.
        save(
            &db,
            Settings::try_new(SettingsParams {
                exam_kinds: kinds(&["lab"]),
                ..params()
            })
            .unwrap(),
        )
        .await
        .unwrap();

        // A's merge over the stale snapshot (kinds kept "as loaded", bands
        // changed) — exactly what a concurrent PATCH /settings computes —
        // must be refused, not applied.
        let refused = save_if_unchanged(
            &db,
            Settings::try_new(SettingsParams {
                grade_bands: bands(&[(0, "F"), (50, "P")]),
                ..stale.params()
            })
            .unwrap(),
            &stale,
        )
        .await
        .unwrap();
        assert!(refused.is_none(), "a stale snapshot's save must not apply");

        // B's edit must survive A's stale write attempt.
        let after = load(&db).await.unwrap();
        assert_eq!(
            names(&after),
            ["lab"],
            "a concurrent editor's exam kinds must not be silently reverted"
        );
        assert!(after.get_grade_bands().is_empty());

        // A's retry — reload, re-merge, save again — lands both edits.
        let fresh = load(&db).await.unwrap();
        let saved = save_if_unchanged(
            &db,
            Settings::try_new(SettingsParams {
                grade_bands: bands(&[(0, "F"), (50, "P")]),
                ..fresh.params()
            })
            .unwrap(),
            &fresh,
        )
        .await
        .unwrap()
        .expect("a merge over the current row applies");
        assert_eq!(names(&saved), ["lab"]);
        assert_eq!(saved.get_grade_bands().len(), 2);
    }

    #[tokio::test]
    async fn save_if_unchanged_seeds_the_first_row() {
        let db = crate::database::init_mem().await.unwrap();
        // No row yet: `load` reports the defaults, and a save conditioned on
        // that snapshot must apply (seeding the singleton on the way).
        let current = load(&db).await.unwrap();
        let saved = save_if_unchanged(
            &db,
            Settings::try_new(SettingsParams {
                exam_kinds: kinds(&["lab"]),
                ..current.params()
            })
            .unwrap(),
            &current,
        )
        .await
        .unwrap()
        .expect("the first save applies");
        assert_eq!(names(&saved), ["lab"]);
        let mut result = db.query("SELECT * FROM settings").await.unwrap();
        let rows: Vec<Settings> = result.take(0).unwrap();
        assert_eq!(rows.len(), 1, "still one singleton row");
    }
}
