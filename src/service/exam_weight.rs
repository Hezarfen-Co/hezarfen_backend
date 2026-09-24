//! Exam-weight overrides: the resolution chain and the write doors.
//!
//! **The chain** — [`resolve`] answers "what does an exam of this kind weigh
//! *in this section*": the section's own row (only while its
//! `exam_weights_inherited` flag is `FALSE`), else the grade-level template's
//! row, else the school `settings.exam_kinds` weight for that kind, else
//! [`crate::domain::exam_weight::FALLBACK_WEIGHT`] (1). The floor is what
//! keeps a *retired* kind — settings edits never rewrite history — counting
//! once, exactly as the pre-offering behaviour did.
//!
//! **Own set in force means authoritative, including when empty.** A section
//! with `exam_weights_inherited = FALSE` and no row for a kind resolves that
//! kind to 1 — *not* to the offering's or the settings' weight. That is how a
//! section zeroes out the template's weights without renaming the school's
//! vocabulary; the empty-own-set case is pinned by a test below.
//!
//! Every write validates the kind against `settings.exam_kinds` (the rows may
//! only name kinds the school runs — the routes answer 400
//! `unknown_exam_kind`) and the weight against the same 1..=100 window the
//! settings enforce. The class-side writes flip the flag inside the statement
//! ([`crate::db::exam_weight`]); the reset restores it.

use crate::database::Database;
use crate::db;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::exam_weight::{ExamWeight, FALLBACK_WEIGHT};
use crate::error::{AppError, ValidationError};

/// What an exam of `kind` weighs inside `instance`: the section's own row
/// while its own set is in force (a missing row is a 1, and the chain
/// *stops* there), else the offering's row, else the settings weight, else 1.
pub async fn resolve(
    db: &Database,
    instance: &ClassCourse,
    kind: &str,
) -> Result<i64, AppError> {
    if !instance.exam_weights_inherited() {
        // The section's table is the whole answer — empty included. Falling
        // through to the offering/settings here would silently re-attach the
        // weights a section deliberately cleared.
        return Ok(db::exam_weight::class_weight(db, instance.get_id(), kind)
            .await?
            .map(|row| row.get_weight())
            .unwrap_or(FALLBACK_WEIGHT));
    }
    if let Some(row) = db::exam_weight::offering_weight(db, instance.get_offering(), kind).await? {
        return Ok(row.get_weight());
    }
    let school = super::settings::load(db).await?;
    Ok(school.exam_kind_weight(kind).unwrap_or(FALLBACK_WEIGHT))
}

/// The section's **effective** weight map: the (inherited flag, one resolved
/// entry per kind). With the section's own set in force that map is exactly
/// its rows — even when that is nothing; while it inherits, every settings
/// kind (plus any kind an offering row adds) resolved off the offering →
/// settings → 1 chain. A client renders this verbatim; it never re-implements
/// the chain.
pub async fn resolved_for_class(
    db: &Database,
    instance: &ClassCourse,
) -> Result<(bool, Vec<ExamWeight>), AppError> {
    if !instance.exam_weights_inherited() {
        return Ok((false, db::exam_weight::list_for_class(db, instance.get_id()).await?));
    }
    Ok((true, resolved_inherited(db, instance.get_offering()).await?))
}

/// The template's effective weight map: every settings kind (plus any kind its
/// rows add), each resolved off the offering's row → settings → 1.
pub async fn resolved_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
) -> Result<Vec<ExamWeight>, AppError> {
    resolved_inherited(db, offering).await
}

/// The map an inheriting consumer sees: the settings vocabulary, extended by
/// whatever kinds the offering's own rows name (a retired kind with a stored
/// row keeps showing its stored weight), each resolved in memory off one batch
/// read.
async fn resolved_inherited(
    db: &Database,
    offering: &CourseOfferingId,
) -> Result<Vec<ExamWeight>, AppError> {
    let school = super::settings::load(db).await?;
    let rows = db::exam_weight::list_for_offering(db, offering).await?;
    let weight_of = |kind: &str| {
        rows.iter()
            .find(|row| row.get_kind() == kind)
            .map(ExamWeight::get_weight)
            .or_else(|| school.exam_kind_weight(kind))
            .unwrap_or(FALLBACK_WEIGHT)
    };
    let mut kinds: Vec<String> = school
        .get_exam_kinds()
        .iter()
        .map(|def| def.get_name().to_string())
        .collect();
    for row in &rows {
        if !kinds.iter().any(|kind| kind == row.get_kind()) {
            kinds.push(row.get_kind().to_string());
        }
    }
    kinds.sort();
    Ok(kinds
        .into_iter()
        .map(|kind| ExamWeight {
            weight: weight_of(&kind),
            kind,
        })
        .collect())
}

/// The settings check behind every write: the row may only name a kind the
/// school runs. The refusal is a plain 400-shaped
/// [`ValidationError::Invalid`] on `field: "kind"` — the web layer re-renders
/// that shape as the coded `unknown_exam_kind` body.
async fn checked_weight(db: &Database, kind: &str, weight: i64) -> Result<ExamWeight, AppError> {
    let school = super::settings::load(db).await?;
    if !school.get_exam_kinds().iter().any(|def| def.get_name() == kind) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "kind",
            reason: "unknown exam kind (the school's settings.exam_kinds is the vocabulary)",
        }));
    }
    Ok(ExamWeight::try_new(kind, weight)?)
}

/// Upsert one weight on the grade-level template. Manager+ is the route's
/// gate; a gone offering was the handler's 404 one instant earlier (the FK
/// backs the race).
pub async fn set_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
    kind: &str,
    weight: i64,
) -> Result<(), AppError> {
    let weight = checked_weight(db, kind, weight).await?;
    db::exam_weight::upsert_for_offering(db, offering, kind, &weight).await
}

/// Drop one weight row from the template; a pair with no row is a 404.
pub async fn remove_from_offering(
    db: &Database,
    offering: &CourseOfferingId,
    kind: &str,
) -> Result<(), AppError> {
    if !db::exam_weight::delete_from_offering(db, offering, kind).await? {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Upsert one weight on the section and take its set own — the flag flips in
/// the same statement, so the write is never half an override. A gone instance
/// is a 404.
pub async fn set_for_class(
    db: &Database,
    instance: &ClassCourseId,
    kind: &str,
    weight: i64,
) -> Result<(), AppError> {
    let weight = checked_weight(db, kind, weight).await?;
    db::exam_weight::upsert_for_class(db, instance, kind, &weight).await
}

/// Drop one weight row from the section — still taking the set own (a section
/// that deletes its last row has *decided* on an empty set, and an empty own
/// set resolves 1). No row for the kind, or a gone instance, is a 404.
pub async fn remove_from_class(
    db: &Database,
    instance: &ClassCourseId,
    kind: &str,
) -> Result<(), AppError> {
    if !db::exam_weight::delete_from_class(db, instance, kind).await? {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Restore inheritance: delete every section weight row and set the flag back
/// to `TRUE`, in one statement. Idempotent; a gone instance is a 404.
pub async fn reset_class(db: &Database, instance: &ClassCourseId) -> Result<(), AppError> {
    db::exam_weight::reset_class(db, instance).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::class_member::tests::{a_class, a_course};
    use crate::error::ValidationError;

    /// A manager account and one attached instance — the attach auto-mints
    /// the grade-level offering the chain reads. The school's vocabulary is
    /// seeded to what the chain tests assume: `final` weighs 3 in the
    /// settings, `yazili` 1, and `oral` stays unknown so the refusal has a
    /// name.
    async fn an_instance(db: &Database) -> ClassCourse {
        let mut params = crate::domain::settings::Settings::defaults().params();
        params.exam_kinds = vec![
            crate::domain::settings::ExamKindDef::try_new("final", 3).unwrap(),
            crate::domain::settings::ExamKindDef::try_new("yazili", 1).unwrap(),
        ];
        crate::db::settings::save(db, crate::domain::settings::Settings::try_new(params).unwrap())
            .await
            .unwrap();
        let manager = crate::db::class_member::tests::fixture_user(db, "weights-manager").await;
        let class = a_class("9-A", db).await;
        let algebra = a_course("algebra-weights", db).await;
        crate::service::class_course::attach(db, &class, &algebra, &manager)
            .await
            .unwrap()
    }

    async fn offering_id(db: &Database, instance: &ClassCourse) -> CourseOfferingId {
        crate::service::class_course::read(db, instance.get_id())
            .await
            .unwrap()
            .unwrap()
            .get_offering()
            .clone()
    }

    /// The full chain on the default vocabulary (`final` weighs 3 in the
    /// settings): settings → offering row → class row → reset, plus the
    /// retired-kind floor. Each step *changes* the answer, so every
    /// assertion can fail.
    #[tokio::test]
    async fn the_chain_walks_class_then_offering_then_settings_then_one() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db).await;
        let offering = offering_id(&db, &instance).await;

        // Inheriting, nothing overridden anywhere: the settings weight.
        assert_eq!(resolve(&db, &instance, "final").await.unwrap(), 3);

        // The template overrides the settings…
        set_for_offering(&db, &offering, "final", 5).await.unwrap();
        assert_eq!(resolve(&db, &instance, "final").await.unwrap(), 5);

        // …and the section overrides the template.
        set_for_class(&db, instance.get_id(), "final", 7).await.unwrap();
        let own7 = instance_of(&db, instance.get_id()).await;
        assert!(
            !own7.exam_weights_inherited(),
            "the class-side write took the set own in the same statement"
        );
        assert_eq!(resolve(&db, &own7, "final").await.unwrap(), 7);

        // Deleting the section's row takes the set own — and the chain
        // STOPS at the class: 1, not the offering's 5, not the settings' 3.
        // This is the empty-own-set contract: an own set in force is
        // authoritative including when it is empty.
        remove_from_class(&db, instance.get_id(), "final").await.unwrap();
        let own = instance_of(&db, instance.get_id()).await;
        assert!(!own.exam_weights_inherited());
        assert!(db::exam_weight::list_for_class(&db, instance.get_id())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            resolve(&db, &own, "final").await.unwrap(),
            FALLBACK_WEIGHT,
            "an empty own set resolves 1 — the offering and settings weights stay out"
        );
        // A kind the settings don't even list, under the same own set in
        // force: 1 as well — the chain never left the class table.
        assert_eq!(
            resolve(&db, &own, "oral").await.unwrap(),
            FALLBACK_WEIGHT,
            "an unknown kind under an own set in force counts once"
        );

        // The reset restores inheritance: the offering's 5 applies again.
        reset_class(&db, instance.get_id()).await.unwrap();
        let reset = instance_of(&db, instance.get_id()).await;
        assert!(reset.exam_weights_inherited());
        assert_eq!(resolve(&db, &reset, "final").await.unwrap(), 5);

        // A kind the school does not list at all (no row anywhere) counts once.
        assert_eq!(resolve(&db, &reset, "oral").await.unwrap(), FALLBACK_WEIGHT);
    }

    /// While the section inherits, its effective map is the whole resolved
    /// vocabulary; the moment its own set is in force, the map is exactly its
    /// rows — nothing invented, nothing carried over.
    #[tokio::test]
    async fn the_resolved_map_switches_wholesale_with_the_flag() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db).await;
        let offering = offering_id(&db, &instance).await;
        set_for_offering(&db, &offering, "final", 5).await.unwrap();

        let (inherited, map) = resolved_for_class(&db, &instance).await.unwrap();
        assert!(inherited);
        let weight_of = |kind: &str| {
            map.iter()
                .find(|row| row.get_kind() == kind)
                .map(ExamWeight::get_weight)
                .unwrap_or_else(|| panic!("no `{kind}` in the resolved map"))
        };
        assert_eq!(weight_of("final"), 5, "the offering row, not the settings 3");
        assert_eq!(weight_of("yazili"), 1, "the settings weight shines through");

        set_for_class(&db, instance.get_id(), "yazili", 4).await.unwrap();
        let own = instance_of(&db, instance.get_id()).await;
        let (inherited, map) = resolved_for_class(&db, &own).await.unwrap();
        assert!(!inherited);
        assert_eq!(map.len(), 1, "the own set is the map — no inherited kinds");
        assert_eq!(map[0].get_kind(), "yazili");
        assert_eq!(map[0].get_weight(), 4);
    }

    /// Writes refuse a kind the school does not run — before the weight is
    /// even looked at (the settings vocabulary check is the first 400).
    #[tokio::test]
    async fn writes_refuse_an_unknown_kind_before_the_weight() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db).await;
        let offering = offering_id(&db, &instance).await;

        for written in [
            set_for_offering(&db, &offering, "oral", 2).await,
            set_for_class(&db, instance.get_id(), "oral", 2).await,
            // Both wrong at once: the *kind* is what the error names.
            set_for_class(&db, instance.get_id(), "oral", 500).await,
        ] {
            let err = written.unwrap_err();
            assert!(
                matches!(
                    &err,
                    AppError::Validation(ValidationError::Invalid { field: "kind", .. })
                ),
                "an unknown kind is refused on `kind`: {err:?}"
            );
        }

        // A weight outside 1..=100 on a *known* kind is the bounds refusal.
        let err = set_for_class(&db, instance.get_id(), "final", 500)
            .await
            .unwrap_err();
        assert!(matches!(
            &err,
            AppError::Validation(ValidationError::Invalid { field: "weight", .. })
        ));
    }

    /// Deleting a row nobody wrote is a 404 and — per the flag-guarded
    /// delete — flips nothing: a refused delete must not take the set own.
    #[tokio::test]
    async fn a_delete_with_no_row_is_a_404_that_moves_nothing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db).await;
        let offering = offering_id(&db, &instance).await;

        assert!(matches!(
            remove_from_class(&db, instance.get_id(), "final").await,
            Err(AppError::NotFound)
        ));
        assert!(matches!(
            remove_from_offering(&db, &offering, "final").await,
            Err(AppError::NotFound)
        ));
        let own = instance_of(&db, instance.get_id()).await;
        assert!(
            own.exam_weights_inherited(),
            "the refused delete left inheritance in place"
        );
        assert_eq!(resolve(&db, &own, "final").await.unwrap(), 3);
    }

    /// The reset deletes every row and inherits again in one statement.
    #[tokio::test]
    async fn reset_drops_the_whole_own_set_and_inherits_again() {
        let (db, _leases) = crate::database::init_test_db().await;
        let instance = an_instance(&db).await;
        set_for_class(&db, instance.get_id(), "yazili", 4).await.unwrap();
        set_for_class(&db, instance.get_id(), "final", 6).await.unwrap();

        reset_class(&db, instance.get_id()).await.unwrap();
        let own = instance_of(&db, instance.get_id()).await;
        assert!(own.exam_weights_inherited());
        assert!(db::exam_weight::list_for_class(&db, instance.get_id())
            .await
            .unwrap()
            .is_empty());
        // Settings weights apply again (yazili defaults to 1; final to 3).
        assert_eq!(resolve(&db, &own, "final").await.unwrap(), 3);

        // A second reset is a no-op success, and a gone instance is a 404.
        reset_class(&db, instance.get_id()).await.unwrap();
        let ghost = ClassCourseId::from_key("019732e3-7b00-7000-8000-00000000e1a1");
        assert!(matches!(reset_class(&db, &ghost).await, Err(AppError::NotFound)));
    }

    async fn instance_of(db: &Database, id: &ClassCourseId) -> ClassCourse {
        crate::service::class_course::read(db, id)
            .await
            .unwrap()
            .unwrap()
    }
}
