//! Weekly-plan workflows: the two resolution arms (the flag decides whose
//! rows a reader sees), the guarded adds (overlap and cap, judged inside the
//! owner's row lock so two racers cannot both clear the pre-check), and the
//! override doors (add/remove/reset on the section's own set, each flipping
//! `weekly_plan_inherited` in the same statement — see
//! [`crate::db::weekly_slot`]).
//!
//! **Not a scheduler.** Nothing runs on its own: this module is template +
//! override + resolution only, and the explicit materialize door
//! ([`crate::service::course_session::materialize`]) is the one caller that
//! turns a resolution into dated lessons.

use crate::database::{Database, tx_with_retry};
use crate::db::weekly_slot as slot_db;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::course_session::SessionTopic;
use crate::domain::weekly_slot::{
    SlotAddError, SlotMinute, Weekday, WeeklySlot, WeeklySlotId, check_add,
};
use crate::error::AppError;

/// The weekly plan a read should present for `instance`: the offering's
/// slots while the section follows the template
/// (`weekly_plan_inherited`), its own rows once it overrode — **including
/// when that own set is empty**, which is how a section clears its
/// timetable. Weekday first, then start time; the empty plan is a valid
/// answer, never an error.
pub async fn resolved_for_instance(
    db: &Database,
    instance: &ClassCourse,
) -> Result<Vec<WeeklySlot>, AppError> {
    if instance.weekly_plan_inherited() {
        resolved_for_offering(db, instance.get_offering()).await
    } else {
        resolved_own(db, instance.get_id()).await
    }
}

/// The offering's template week — the arm `resolved_for_instance` follows
/// while the section inherits, and the direct read the offering-side route
/// serves. Weekday first, then start time.
pub async fn resolved_for_offering(
    db: &Database,
    offering_id: &CourseOfferingId,
) -> Result<Vec<WeeklySlot>, AppError> {
    slot_db::list_for_offering(db, offering_id).await
}

/// The section's own rows, flag-independent — the resolver's override arm.
async fn resolved_own(
    db: &Database,
    instance: &ClassCourseId,
) -> Result<Vec<WeeklySlot>, AppError> {
    slot_db::list_for_class(db, instance).await
}

/// Add one slot to the offering's template week. Manager-gated at the route;
/// refused with 409 `slot_overlap` when a template slot already occupies an
/// intersecting window on that weekday (the exact duplicate overlaps too),
/// 409 `slot_cap` when the week already holds [`MAX_WEEKLY_SLOTS`] slots,
/// and 400 for a malformed weekday, minute, or empty window. `topic` is the
/// optional lesson topic the materializer copies onto generated sessions —
/// content, never an overlap input.
pub async fn add_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
    weekday: Weekday,
    starts_at: SlotMinute,
    ends_at: SlotMinute,
    topic: Option<SessionTopic>,
) -> Result<WeeklySlot, AppError> {
    let slot = WeeklySlot::new(WeeklySlotId::generate(), weekday, starts_at, ends_at, topic)?;
    let owner = offering.clone();
    tx_with_retry(db, false, async move |tx| {
        // The row lock serializes the pre-check and the insert: under READ
        // COMMITTED a rival's uncommitted slot would otherwise be invisible.
        slot_db::lock_offering_tx(tx, &owner).await?;
        let existing = slot_db::list_for_offering_on(&mut *tx, &owner).await?;
        check_add(&existing, &slot).map_err(slot_add_conflict)?;
        slot_db::add_for_offering_tx(tx, &slot, &owner).await
    })
    .await
}

/// Add one slot to the section's own week — the override door. Writing any
/// row flips `weekly_plan_inherited` to `FALSE` in the same statement, so the
/// section's own set becomes authoritative from this write on. Same refusals
/// as [`add_for_offering`], plus 404 when the instance is gone.
pub async fn add_for_class(
    db: &Database,
    instance: &ClassCourseId,
    weekday: Weekday,
    starts_at: SlotMinute,
    ends_at: SlotMinute,
    topic: Option<SessionTopic>,
) -> Result<WeeklySlot, AppError> {
    let slot = WeeklySlot::new(WeeklySlotId::generate(), weekday, starts_at, ends_at, topic)?;
    let owner = instance.clone();
    tx_with_retry(db, false, async move |tx| {
        slot_db::lock_class_tx(tx, &owner).await?;
        let existing = slot_db::list_for_class_on(&mut *tx, &owner).await?;
        check_add(&existing, &slot).map_err(slot_add_conflict)?;
        slot_db::add_for_class_tx(tx, &slot, &owner).await
    })
    .await
}

/// Drop one slot of the offering's template week. A slot of another offering
/// (or an already-gone id) is a 404.
pub async fn remove_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
    slot: &WeeklySlotId,
) -> Result<(), AppError> {
    slot_db::remove_for_offering(db, offering, slot).await
}

/// Drop one slot of the section's own week — the override door. The flag
/// flips to `FALSE` in the same statement as the delete, so dropping the
/// last row leaves the section with an authoritative *empty* plan rather
/// than quietly re-inheriting. A 404 (unknown id, or another instance's
/// slot) leaves the flag untouched.
pub async fn remove_for_class(
    db: &Database,
    instance: &ClassCourseId,
    slot: &WeeklySlotId,
) -> Result<(), AppError> {
    slot_db::remove_for_class(db, instance, slot).await
}

/// Reset the section's weekly plan to inherit: delete its own slots and flip
/// the flag back to `TRUE` in one statement. After this
/// [`resolved_for_instance`] serves the offering's rows again. Idempotent for
/// a section that never overrode; 404 when the instance is gone.
pub async fn reset_for_class(db: &Database, instance: &ClassCourseId) -> Result<(), AppError> {
    slot_db::reset_for_class(db, instance).await
}

/// Map the pure add-time refusals onto the wire's coded 409s.
fn slot_add_conflict(error: SlotAddError) -> AppError {
    match error {
        SlotAddError::Overlap => AppError::ConflictCoded {
            code: "slot_overlap",
            message: "another slot already occupies an intersecting time on this weekday".into(),
        },
        SlotAddError::Full => AppError::ConflictCoded {
            code: "slot_cap",
            message: format!(
                "this weekly plan already holds the maximum of {} slots",
                crate::constant::MAX_WEEKLY_SLOTS
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::class_member::tests::{a_class, a_course, fixture_user};

    /// The instance row, re-read from the store — the flag assertions judge
    /// the stored column, not a return value.
    async fn instance_row(db: &Database, id: &ClassCourseId) -> ClassCourse {
        crate::service::class_course::read(db, id)
            .await
            .unwrap()
            .unwrap()
    }

    fn day(raw: i16) -> Weekday {
        Weekday::new(raw).unwrap()
    }

    fn mins(raw: i64) -> SlotMinute {
        SlotMinute::new(raw).unwrap()
    }

    /// An attached instance: its auto-minted offering id is the template the
    /// inheritance arm reads.
    async fn an_instance(db: &Database, name: &str) -> ClassCourse {
        let manager = fixture_user(db, "manager").await;
        let class = a_class(name, db).await;
        let course = a_course("Matematik", db).await;
        crate::service::class_course::attach(db, &class, &course, &manager)
            .await
            .unwrap()
    }

    fn coded(result: &Result<WeeklySlot, AppError>) -> String {
        match result {
            Err(AppError::ConflictCoded { code, .. }) => code.to_string(),
            other => panic!("expected a coded 409, got {other:?}"),
        }
    }

    /// While the section inherits, its resolved plan is the offering's —
    /// weekday first, then start time, whatever order the slots were added
    /// in. The flag stays TRUE; nothing on the section's side was written.
    #[tokio::test]
    async fn an_inheriting_section_resolves_the_offerings_slots_in_order() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db, "5-A").await;
        let offering = instance.get_offering().clone();

        add_for_offering(&db, &offering, day(2), mins(600), mins(660), None)
            .await
            .unwrap();
        add_for_offering(&db, &offering, day(1), mins(600), mins(660), None)
            .await
            .unwrap();
        add_for_offering(&db, &offering, day(1), mins(480), mins(540), None)
            .await
            .unwrap();

        assert!(
            instance_row(&db, instance.get_id())
                .await
                .weekly_plan_inherited()
        );
        let resolved = resolved_for_instance(&db, &instance).await.unwrap();
        let starts: Vec<i64> = resolved.iter().map(|s| s.get_starts_at().get()).collect();
        let weekdays: Vec<i16> = resolved.iter().map(|s| s.get_weekday().get()).collect();
        assert_eq!(weekdays, vec![1, 1, 2], "weekday orders first");
        assert_eq!(starts, vec![480, 600, 600], "start time breaks the tie");
    }

    /// The override arm: once the section wrote a slot of its own, its own
    /// set is authoritative — and **stays** authoritative when empty, which
    /// is exactly how a section clears its timetable instead of silently
    /// re-inheriting the template.
    #[tokio::test]
    async fn an_own_set_wins_including_when_it_is_empty() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db, "5-B").await;
        let offering = instance.get_offering().clone();
        add_for_offering(&db, &offering, day(1), mins(480), mins(540), None)
            .await
            .unwrap();

        add_for_class(&db, instance.get_id(), day(3), mins(540), mins(600), None)
            .await
            .unwrap();
        // The write flipped the flag: re-read so the resolver follows the
        // override arm instead of the stale inheriting snapshot.
        let instance = instance_row(&db, instance.get_id()).await;
        assert!(!instance.weekly_plan_inherited());
        let resolved = resolved_for_instance(&db, &instance).await.unwrap();
        assert_eq!(resolved.len(), 1, "the own row, not the offering's");

        // Dropping the last own row empties the authoritative set; the
        // offering's slot stays hidden behind the still-FALSE flag.
        let own_id = resolved[0].get_id().clone();
        remove_for_class(&db, instance.get_id(), &own_id)
            .await
            .unwrap();
        assert!(
            !instance_row(&db, instance.get_id())
                .await
                .weekly_plan_inherited()
        );
        assert!(
            resolved_for_instance(&db, &instance)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Intersecting windows on one weekday are refused — partial, exact
    /// duplicate, and containment — while touching endpoints and other
    /// weekdays pass. Malformed bodies are 400s.
    #[tokio::test]
    async fn an_overlapping_slot_is_refused() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db, "5-C").await;
        add_for_class(&db, instance.get_id(), day(1), mins(540), mins(600), None)
            .await
            .unwrap();

        for (starts, ends, why) in [
            (540, 600, "the exact duplicate"),
            (570, 630, "the partial overlap"),
            (530, 610, "the containing window"),
            (530, 630, "the wider window"),
        ] {
            let attempt =
                add_for_class(&db, instance.get_id(), day(1), mins(starts), mins(ends), None).await;
            assert_eq!(
                coded(&attempt),
                "slot_overlap",
                "{why} must answer 409 slot_overlap"
            );
        }

        // Touching endpoints and a different weekday are not overlaps.
        add_for_class(&db, instance.get_id(), day(1), mins(600), mins(660), None)
            .await
            .unwrap();
        add_for_class(&db, instance.get_id(), day(2), mins(540), mins(600), None)
            .await
            .unwrap();
        assert_eq!(
            slot_db::list_for_class(&db, instance.get_id())
                .await
                .unwrap()
                .len(),
            3
        );

        // Malformed bodies are validation 400s, before any owner query runs.
        assert!(Weekday::new(0).is_err());
        assert!(SlotMinute::new(-1).is_err());
        let empty_window =
            add_for_class(&db, instance.get_id(), day(1), mins(600), mins(600), None).await;
        assert!(
            matches!(empty_window, Err(AppError::Validation(_))),
            "{empty_window:?}"
        );
        let inverted = add_for_class(&db, instance.get_id(), day(1), mins(1439), mins(600), None).await;
        assert!(
            matches!(inverted, Err(AppError::Validation(_))),
            "{inverted:?}"
        );
    }

    /// The cap bounds one owner's plan; the refusal names the cap, and the
    /// stored set never exceeds the bound.
    #[tokio::test]
    async fn the_slot_cap_holds() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db, "5-D").await;

        // Forty non-overlapping one-minute windows: weekday (i%7)+1, minute
        // 480+(i/7) — five or six per weekday, all touching, none crossing.
        for i in 0..crate::constant::MAX_WEEKLY_SLOTS {
            let i = i as i64;
            add_for_class(&db, instance.get_id(), day(((i % 7) + 1) as i16), mins(480 + i / 7), mins(481 + i / 7), None)
            .await
            .unwrap();
        }
        assert_eq!(
            slot_db::list_for_class(&db, instance.get_id())
                .await
                .unwrap()
                .len(),
            crate::constant::MAX_WEEKLY_SLOTS
        );

        // Room on Sunday's timeline, but the plan is full.
        let one_more = add_for_class(&db, instance.get_id(), day(7), mins(700), mins(760), None).await;
        assert_eq!(coded(&one_more), "slot_cap");
        assert_eq!(
            slot_db::list_for_class(&db, instance.get_id())
                .await
                .unwrap()
                .len(),
            crate::constant::MAX_WEEKLY_SLOTS
        );
    }

    /// The reset door: the own slots go, the flag returns to TRUE, and the
    /// offering's rows are what the section resolves again.
    #[tokio::test]
    async fn reset_restores_inheritance() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db, "5-E").await;
        let offering = instance.get_offering().clone();
        add_for_offering(&db, &offering, day(1), mins(480), mins(540), None)
            .await
            .unwrap();
        add_for_class(&db, instance.get_id(), day(2), mins(480), mins(540), None)
            .await
            .unwrap();
        assert!(
            !instance_row(&db, instance.get_id())
                .await
                .weekly_plan_inherited()
        );

        reset_for_class(&db, instance.get_id()).await.unwrap();
        let after = instance_row(&db, instance.get_id()).await;
        assert!(after.weekly_plan_inherited());
        let resolved = resolved_for_instance(&db, &after).await.unwrap();
        assert_eq!(resolved.len(), 1, "the offering's slot serves again");
        assert_eq!(resolved[0].get_weekday(), day(1));

        // Idempotent: resetting an inheriting section is a no-op success.
        reset_for_class(&db, instance.get_id()).await.unwrap();
        assert!(
            instance_row(&db, instance.get_id())
                .await
                .weekly_plan_inherited()
        );
    }

    /// Removing an unknown slot — or another instance's slot — is a 404 that
    /// leaves both sections alone. An own removal keeps the rest of the set.
    #[tokio::test]
    async fn removing_a_slot_scopes_to_its_owner() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db, "5-F").await;
        let other = an_instance(&db, "5-G").await;

        add_for_class(&db, instance.get_id(), day(1), mins(480), mins(540), None)
            .await
            .unwrap();
        add_for_class(&db, instance.get_id(), day(2), mins(480), mins(540), None)
            .await
            .unwrap();
        let own = slot_db::list_for_class(&db, instance.get_id())
            .await
            .unwrap();

        // Another section's instance id is a 404, and the flag/id stand.
        let stranger = remove_for_class(&db, other.get_id(), own[0].get_id()).await;
        assert!(matches!(stranger, Err(AppError::NotFound)), "{stranger:?}");
        // An unknown slot id is a 404 too.
        let missing = remove_for_class(&db, instance.get_id(), &WeeklySlotId::generate()).await;
        assert!(matches!(missing, Err(AppError::NotFound)), "{missing:?}");
        assert_eq!(
            slot_db::list_for_class(&db, instance.get_id())
                .await
                .unwrap()
                .len(),
            2
        );

        remove_for_class(&db, instance.get_id(), own[0].get_id())
            .await
            .unwrap();
        let rest = slot_db::list_for_class(&db, instance.get_id())
            .await
            .unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].get_weekday(), day(2));
        assert!(
            !instance_row(&db, instance.get_id())
                .await
                .weekly_plan_inherited()
        );
    }

    /// The offering-side doors mirror the class side minus the flag: overlap
    /// refused, remove scoped to the offering, resolve ordering holds.
    #[tokio::test]
    async fn the_offering_side_adds_removes_and_resolves() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db, "5-H").await;
        let offering = instance.get_offering().clone();

        add_for_offering(&db, &offering, day(1), mins(540), mins(600), None)
            .await
            .unwrap();
        let clash = add_for_offering(&db, &offering, day(1), mins(570), mins(630), None).await;
        assert_eq!(coded(&clash), "slot_overlap");

        let slots = slot_db::list_for_offering(&db, &offering).await.unwrap();
        assert_eq!(slots.len(), 1);
        let unknown = remove_for_offering(&db, &offering, &WeeklySlotId::generate()).await;
        assert!(matches!(unknown, Err(AppError::NotFound)), "{unknown:?}");

        // Another offering's slot id is not deletable through this one.
        let other_offering = an_instance(&db, "5-I").await.get_offering().clone();
        let foreign = remove_for_offering(&db, &other_offering, slots[0].get_id()).await;
        assert!(matches!(foreign, Err(AppError::NotFound)), "{foreign:?}");

        remove_for_offering(&db, &offering, slots[0].get_id())
            .await
            .unwrap();
        assert!(
            resolved_for_offering(&db, &offering)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
