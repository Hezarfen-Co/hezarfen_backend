//! The settings edit workflow: one edit at a time under [`SETTINGS_LOCK`],
//! a snapshot-merge-retire-save pair whose halves cannot interleave, and the
//! compare-and-set retry that keeps two managers' patches from silently
//! reverting each other. The singleton's queries live in
//! [`crate::db::settings`].

use surrealdb::types::RecordId;
use tokio::sync::Mutex;

use crate::constant::CAS_UPDATE_RETRIES;
use crate::database::Database;
use crate::db::cap;
use crate::domain::exam_result::kind_ref;
use crate::domain::menu::slot_ref;
use crate::domain::settings::{ExamKindDef, GradeBand, MealSlotDef, Settings};
use crate::error::AppError;

/// One settings edit at a time, over the whole process.
///
/// A list edit is a **pair** of writes, and the pair is the guard: every name
/// the edit drops is retired on its reference counter first (which refuses
/// every later claim), and only then is the list itself committed with a
/// compare-and-set against the snapshot the removals were judged from. Neither
/// write can be made to cover the other. Both retirement and un-retirement are
/// *idempotent*, so a rival that decided the same removal against the same
/// snapshot is told "already retired" and records no rollback — and when it is
/// that rival's save that wins the compare-and-set, the attempt which really
/// flipped the bit rolls it back, leaving the name off the stored list with a
/// counter reading "in service": gradable again, and unremovable for good once
/// a mark lands. No per-name bit can close that, because `Unchanged` has
/// erased which attempt owns the flip.
///
/// So the pair is serialized instead. Every writer of the singleton goes
/// through `PATCH /settings`, and the deployment runs one process by contract
/// (stop-the-world upgrades), so process-wide is deployment-wide here — the
/// same argument [`crate::db::cap`]'s own lock makes for `retire_name`'s
/// two statements, one level up. The compare-and-set stays: it is what keeps a
/// crashed or rolled-back attempt from writing a list nobody merged.
///
/// **Lock order:** `SETTINGS_LOCK` → `cap`'s `CLAIM_LOCK`, never the reverse —
/// the retirements are taken while this is held.
pub(crate) static SETTINGS_LOCK: Mutex<()> = Mutex::const_new(());

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
/// URL-charset rule (`MealSlotDef::try_kept`), which only [`apply`], holding
/// the lock and the stored list, can decide.
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

/// The whole edit — snapshot, merge, retire, save — as one operation, so the
/// tests can drive it the way two managers do.
pub async fn apply(db: &Database, patch: &SettingsPatch) -> Result<Settings, AppError> {
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

        // The removal guards. A name leaves a list by being *retired* on its
        // reference counter — one conditional write on one record, which lands
        // only while nothing references the name and refuses every claim from
        // that instant on. That is the whole guard: the check and the removal
        // used to be a cross-table count and a save held together by a
        // process-wide lock that was released around the round trip between
        // them.
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

        match save_if_unchanged(db, settings, &current).await {
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
