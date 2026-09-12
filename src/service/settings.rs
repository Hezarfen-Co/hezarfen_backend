//! The settings edit workflow: snapshot-merge-guard-save, with every name-list
//! guard and the compare-and-set riding ONE transaction, so two managers'
//! patches can neither silently revert each other nor strand a half-applied
//! removal. The singleton's queries live in [`crate::db::settings`].

use crate::constant::CAS_UPDATE_RETRIES;
use crate::database::{Database, tx_with_retry};
use crate::domain::settings::{ExamKindDef, GradeBand, MealSlotDef, Settings};
use crate::error::AppError;

/// The abort marker the edit transaction's closure raises when the guarded
/// save wrote zero rows — the stored row moved under the snapshot this
/// attempt was judged against. It never reaches the wire: [`apply`] maps it
/// to another loop turn right here, and the transaction's rollback already
/// took every retirement back with it. (Same abort-through-`Internal`
/// pattern as `crate::db::field_update`'s markers: a refusal is a decision,
/// so it must not be retried — this one is *un*-decided, which is exactly
/// why the loop re-runs.)
const STALE_SAVE: &str = "settings_save_lost_cas";

/// The stored policy, or the defaults when no row exists yet.
pub async fn load(db: &Database) -> Result<Settings, AppError> {
    crate::db::settings::load(db).await
}

/// Persist the policy only while the stored row still matches `expected` —
/// the compare-and-set. `None` means a concurrent edit landed in between:
/// reload, re-merge, retry.
pub async fn save_if_unchanged(
    db: &Database,
    settings: Settings,
    expected: &Settings,
) -> Result<Option<Settings>, AppError> {
    crate::db::settings::save_if_unchanged(db, settings, expected).await
}

/// A `PATCH /settings` body after wire mapping: only the fields the school
/// sent. Lists stay raw on purpose — validating them is the workflow's own
/// step, in its own order, and keeping a stored meal-slot name skips the
/// URL-charset rule (`MealSlotDef::try_kept`), which only [`apply`],
/// holding the stored list, can decide.
pub struct SettingsPatch {
    pub exam_kinds: Option<Vec<ExamKindInput>>,
    pub attendance_statuses: Option<Vec<String>>,
    pub grade_bands: Option<Vec<GradeBandInput>>,
    pub max_file_bytes: Option<i64>,
    pub chatbot_history_turns: Option<i64>,
    pub max_chatbot_threads: Option<i64>,
    pub max_chatbot_message_len: Option<i64>,
    pub meal_slots: Option<Vec<MealSlotInput>>,
    pub dietary_tags: Option<Vec<String>>,
    /// Absent keeps the knob; `Some(None)` clears it (no cutoff at all).
    pub meal_cancel_cutoff_minutes: Option<Option<i64>>,
}

/// One submitted exam kind, raw.
pub struct ExamKindInput {
    pub name: String,
    pub weight: i64,
}

/// One submitted grade band, raw.
pub struct GradeBandInput {
    pub min: i64,
    pub label: String,
}

/// One submitted meal slot, raw.
pub struct MealSlotInput {
    pub name: String,
    pub serving_minute: Option<i64>,
}

/// The whole edit — snapshot, merge, guard, save — as one operation, so the
/// tests can drive it the way two managers do.
///
/// The guards and the save share one transaction, and that is the whole
/// guard: a list edit is a *pair* of writes — every name the edit drops is
/// retired on its reference counter first (which refuses every later
/// claim), and only then is the list itself committed with a
/// compare-and-set against the snapshot the removals were judged from. The
/// old engine could not serialize that pair on its own, so a process-wide
/// lock held it together, and the winner-loses-rollback interaction between
/// two same-snapshot attempts needed a hand-rolled undo list
/// (`Unchanged`-erased which attempt owned a flip, so a no-op could undo a
/// rival). Postgres needs neither: retirement and save commit together or
/// not at all, a stale save rolls its retirements back by aborting, and the
/// row-locked retire switch serializes two attempts dropping the same name.
///
/// Retired before the save, never after: the other order leaves a window in
/// which a mark lands under a kind the settings no longer list. A save that
/// then does not land undoes them — by transaction abort, before the next
/// try, with no undo list to get wrong.
pub async fn apply(db: &Database, patch: &SettingsPatch) -> Result<Settings, AppError> {
    // Merge over a snapshot, then save only while the row still matches it —
    // otherwise a concurrent PATCH of a *different* field would be silently
    // reverted by whichever whole-row write lands second. A refused save
    // reloads and re-merges, so both edits land.
    for _ in 0..CAS_UPDATE_RETRIES {
        let current = load(db).await?;

        let exam_kinds = match &patch.exam_kinds {
            Some(kinds) => kinds
                .iter()
                .map(|kind| ExamKindDef::try_new(&kind.name, kind.weight))
                .collect::<Result<Vec<_>, _>>()?,
            None => current.get_exam_kinds().to_vec(),
        };
        let was_slots = slot_names(&current.get_meal_slots());
        let meal_slots = match &patch.meal_slots {
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
        let attendance_statuses = patch
            .attendance_statuses
            .clone()
            .unwrap_or_else(|| current.get_attendance_statuses().to_vec());
        let grade_bands = match &patch.grade_bands {
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
        params.max_file_bytes = patch.max_file_bytes.unwrap_or(params.max_file_bytes);
        params.chatbot_history_turns = patch
            .chatbot_history_turns
            .unwrap_or(params.chatbot_history_turns);
        params.max_chatbot_threads = patch
            .max_chatbot_threads
            .unwrap_or(params.max_chatbot_threads);
        params.max_chatbot_message_len = patch
            .max_chatbot_message_len
            .unwrap_or(params.max_chatbot_message_len);
        params.meal_slots = meal_slots;
        params.dietary_tags = patch
            .dietary_tags
            .clone()
            .unwrap_or_else(|| current.get_dietary_tags());
        // Double option: absent keeps the knob, `null` clears it (no cutoff).
        params.meal_cancel_cutoff_minutes = patch
            .meal_cancel_cutoff_minutes
            .unwrap_or(params.meal_cancel_cutoff_minutes);

        let settings = Settings::try_new(params)?;

        // Guards + save, one transaction. The removal refusals are the same
        // 409s as ever; a stale save is the `STALE_SAVE` marker, answered by
        // another loop turn below; everything else surfaces.
        let attempted = tx_with_retry(db, false, async |tx| {
            // A name leaves a list by being *retired* on its reference
            // counter — one row-locked switch, which lands only while
            // nothing references the name and refuses every claim from that
            // instant on. A refused removal aborts the transaction, taking
            // any earlier retirement in this same attempt back with it.
            for gone in missing(&was_kinds, &now_kinds) {
                retire_kind(&mut *tx, &gone).await?;
            }
            // Same shape for meal slots: a slot a menu was already published
            // for cannot leave the list — the menu snapshotted the name as
            // text, and a slot no longer offered would leave that menu
            // unreachable from the school's own list.
            for gone in missing(&was_slots, &now_slots) {
                retire_slot(&mut *tx, &gone).await?;
            }
            // A name re-entering a list is back in service: its counter still
            // carries the retirement from the edit that dropped it, and a mark
            // (or a menu) under a kind the school offers again must not be
            // refused. Inside the transaction, "in service" stands only if
            // the save below lands — a stale save aborts and re-retires it.
            for back in missing(&now_kinds, &was_kinds) {
                unretire_kind(&mut *tx, &back).await?;
            }
            for back in missing(&now_slots, &was_slots) {
                unretire_slot(&mut *tx, &back).await?;
            }

            match crate::db::settings::save_if_unchanged(&mut *tx, settings.clone(), &current)
                .await?
            {
                Some(saved) => Ok(saved),
                // The row moved under the snapshot these guards were judged
                // against: the abort puts the names back, and the caller's
                // loop re-merges — or the next attempt would decide against a
                // list nobody asked for.
                None => Err(AppError::Internal(STALE_SAVE.to_string())),
            }
        })
        .await;

        match attempted {
            Ok(saved) => return Ok(saved),
            Err(AppError::Internal(marker)) if marker == STALE_SAVE => continue,
            Err(err) => return Err(err),
        }
    }
    Err(AppError::Conflict(
        "the settings kept changing underneath this update — try again",
    ))
}

/// Retire an exam kind on its reference counter, inside the caller's
/// transaction — the row-locked switch of the `cap` module's retire recipe:
/// the row is read `FOR UPDATE` first, so the read of `retired` and the flip
/// are one unit against a concurrent claim or retire. The flip landing and
/// the flip already standing are both success — the transaction that follows
/// decides whether the retired state stands, so `Unchanged` needs no
/// bookkeeping (the database owns the rollback).
///
/// The name still referenced (`count > 0`) is the caller's removal refusal,
/// byte for byte the old 409.
async fn retire_kind(tx: &mut sqlx::PgConnection, name: &str) -> Result<(), AppError> {
    let was = sqlx::query!(
        r#"SELECT retired AS "retired!: bool" FROM kind_ref WHERE name = $1 FOR UPDATE"#,
        name
    )
    .fetch_optional(tx)
    .await?
    .map(|row| row.retired)
    .unwrap_or(false);
    let landed = sqlx::query!(
        r#"INSERT INTO kind_ref (name, count, retired) VALUES ($1, 0, TRUE)
           ON CONFLICT (name) DO UPDATE SET retired = TRUE
           WHERE kind_ref.count = 0 AND kind_ref.retired IS DISTINCT FROM TRUE
           RETURNING 1 AS "landed!: i64""#,
        name
    )
    .fetch_optional(tx)
    .await?
    .is_some();
    match (landed, was) {
        // Still in use, and not already retired: the removal is refused.
        (false, false) => Err(AppError::ConflictOwned(format!(
            "exams of kind '{name}' are already graded — the kind cannot be removed"
        ))),
        // Flipped, or already retired by the rival that decided the same
        // removal: nothing to do — the transaction decides whether it stands.
        _ => Ok(()),
    }
}

/// [`retire_kind`] against the meal-slot counters.
async fn retire_slot(tx: &mut sqlx::PgConnection, name: &str) -> Result<(), AppError> {
    let was = sqlx::query!(
        r#"SELECT retired AS "retired!: bool" FROM slot_ref WHERE name = $1 FOR UPDATE"#,
        name
    )
    .fetch_optional(tx)
    .await?
    .map(|row| row.retired)
    .unwrap_or(false);
    let landed = sqlx::query!(
        r#"INSERT INTO slot_ref (name, count, retired) VALUES ($1, 0, TRUE)
           ON CONFLICT (name) DO UPDATE SET retired = TRUE
           WHERE slot_ref.count = 0 AND slot_ref.retired IS DISTINCT FROM TRUE
           RETURNING 1 AS "landed!: i64""#,
        name
    )
    .fetch_optional(tx)
    .await?
    .is_some();
    match (landed, was) {
        (false, false) => Err(AppError::ConflictOwned(format!(
            "menus are already published for the '{name}' slot — it cannot be removed"
        ))),
        _ => Ok(()),
    }
}

/// Put an exam kind back in service, inside the caller's transaction. One
/// guarded write: the flip lands only while the name is actually retired, so
/// it is idempotent. Whether the name *stays* in service is the save's
/// decision — a stale save aborts this transaction and the retirement stands
/// again, with no undo list to get wrong.
async fn unretire_kind(tx: &mut sqlx::PgConnection, name: &str) -> Result<(), AppError> {
    sqlx::query!(
        r#"INSERT INTO kind_ref (name, count, retired) VALUES ($1, 0, FALSE)
           ON CONFLICT (name) DO UPDATE SET retired = FALSE
           WHERE kind_ref.retired IS DISTINCT FROM FALSE
           RETURNING 1 AS "landed!: i64""#,
        name
    )
    .fetch_optional(tx)
    .await?;
    Ok(())
}

/// [`unretire_kind`] against the meal-slot counters.
async fn unretire_slot(tx: &mut sqlx::PgConnection, name: &str) -> Result<(), AppError> {
    sqlx::query!(
        r#"INSERT INTO slot_ref (name, count, retired) VALUES ($1, 0, FALSE)
           ON CONFLICT (name) DO UPDATE SET retired = FALSE
           WHERE slot_ref.retired IS DISTINCT FROM FALSE
           RETURNING 1 AS "landed!: i64""#,
        name
    )
    .fetch_optional(tx)
    .await?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;
    use crate::db::settings::save;

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

    /// A `PATCH` body that names nothing: every field keeps the stored value.
    fn a_patch() -> SettingsPatch {
        SettingsPatch {
            exam_kinds: None,
            attendance_statuses: None,
            grade_bands: None,
            max_file_bytes: None,
            chatbot_history_turns: None,
            max_chatbot_threads: None,
            max_chatbot_message_len: None,
            meal_slots: None,
            dietary_tags: None,
            meal_cancel_cutoff_minutes: None,
        }
    }

    /// An `exam_kinds` list as the router builds it (weight 1, like the
    /// harness bodies).
    fn kinds(names: &[&str]) -> Vec<ExamKindInput> {
        names
            .iter()
            .map(|name| ExamKindInput {
                name: (*name).to_string(),
                weight: 1,
            })
            .collect()
    }

    /// A `meal_slots` list as the router builds it (no serving minute).
    fn slots(names: &[&str]) -> Vec<MealSlotInput> {
        names
            .iter()
            .map(|name| MealSlotInput {
                name: (*name).to_string(),
                serving_minute: None,
            })
            .collect()
    }

    /// The lists as the store holds them.
    async fn stored(db: &Database) -> (Vec<String>, Vec<String>) {
        let settings = load(db).await.unwrap();
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
        let without = SettingsPatch {
            exam_kinds: Some(kinds(&["homework", "quiz", "final", "project", "oral"])),
            ..a_patch()
        };

        let (a, b) = tokio::join!(apply(&db, &without), apply(&db, &without));
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
        let without = SettingsPatch {
            exam_kinds: Some(kinds(&["homework", "quiz", "final", "project", "oral"])),
            ..a_patch()
        };

        let held = SETTINGS_LOCK.lock().await;
        let rival = tokio::spawn({
            let db = db.clone();
            async move { apply(&db, &without).await.map(|_| ()) }
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
        let without = SettingsPatch {
            exam_kinds: Some(kinds(&["homework", "quiz", "final", "project", "oral"])),
            ..a_patch()
        };
        apply(&db, &without).await.unwrap();
        let with = SettingsPatch {
            exam_kinds: Some(kinds(&[
                "homework", "quiz", "midterm", "final", "project", "oral",
            ])),
            ..a_patch()
        };

        let (a, b) = tokio::join!(apply(&db, &with), apply(&db, &with));
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
        let without = SettingsPatch {
            meal_slots: Some(slots(&["breakfast", "snack"])),
            ..a_patch()
        };

        let (a, b) = tokio::join!(apply(&db, &without), apply(&db, &without));
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
        save(db, Settings::try_new(params).unwrap()).await.unwrap();
    }

    /// The wedge: re-sending a stored name is the only way to *keep* it, and
    /// dropping it 409s once a menu references it — so validating it under the
    /// younger rule locks the school out of its own `meal_slots` forever. A
    /// stored name is submittable; a new one with the same characters is not.
    #[tokio::test]
    async fn a_stored_slot_name_stays_submittable_but_a_new_one_does_not() {
        let db = init_mem().await.unwrap();
        a_school_with_a_stale_slot(&db).await;

        let edit = SettingsPatch {
            meal_slots: Some(slots(&["a/b", "lunch", "dinner"])),
            ..a_patch()
        };
        apply(&db, &edit).await.unwrap();
        let (_, stored_slots) = stored(&db).await;
        assert_eq!(stored_slots, ["a/b", "lunch", "dinner"]);

        let fresh = SettingsPatch {
            meal_slots: Some(slots(&["a/b", "lunch", "c/d"])),
            ..a_patch()
        };
        assert!(
            apply(&db, &fresh).await.is_err(),
            "a name no school ever stored is still refused"
        );
    }

    /// And an edit that never mentions `meal_slots` still carries the list
    /// over unvalidated — the behaviour the wedge report leaned on.
    #[tokio::test]
    async fn an_unrelated_edit_carries_a_stale_slot_name_over() {
        let db = init_mem().await.unwrap();
        a_school_with_a_stale_slot(&db).await;

        let elsewhere = SettingsPatch {
            max_file_bytes: Some(2048),
            ..a_patch()
        };
        let saved = apply(&db, &elsewhere).await.unwrap();

        assert_eq!(saved.get_max_file_bytes(), 2048);
        assert_eq!(slot_names(&saved.get_meal_slots()), ["a/b", "lunch"]);
    }
}
