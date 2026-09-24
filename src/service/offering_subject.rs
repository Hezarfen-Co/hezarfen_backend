//! Syllabus selection workflows: the offering's topic set (a manager curates
//! the grade's template) and the section's own override of it, plus the two
//! resolvers the read surfaces and the tag-validation rewire ride on. The SQL
//! lives in [`crate::db::offering_subject`]; the one pure rule — a selection
//! names a subject of the offering's own course — in
//! [`crate::domain::offering_subject`].
//!
//! Resolution is override-or-inherit, never merge, and flag-driven:
//! `subjects_inherited` TRUE → the offering's set, FALSE → the section's own
//! rows **even when that set is empty**. Every class-side write door here
//! flips the flag in the same statement as its row change (the CTEs in the
//! db module), so the flag can never describe a stale set; this module never
//! writes `class_course` itself.

use crate::database::Database;
use crate::domain::class_course::ClassCourse;
use crate::domain::course::CourseId;
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::offering_subject::ensure_course_member;
use crate::domain::subject::{Subject, SubjectId};
use crate::error::{AppError, ValidationError};
use crate::{db, service};

/// The topics the grade-level template selects, name-then-id order — the
/// inheritance side of [`resolved_for_instance`].
pub async fn resolved_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
) -> Result<Vec<Subject>, AppError> {
    db::offering_subject::list_for_offering(db, offering).await
}

/// The topics this section teaches, resolved override-or-inherit: its own
/// rows while `subjects_inherited` is FALSE (including an empty set — a
/// section may clear its syllabus), otherwise the offering's set.
pub async fn resolved_for_instance(
    db: &Database,
    instance: &ClassCourse,
) -> Result<Vec<Subject>, AppError> {
    if instance.subjects_inherited() {
        resolved_for_offering(db, instance.get_offering()).await
    } else {
        db::offering_subject::list_for_class_course(db, instance.get_id()).await
    }
}

/// A 400 unless `subject` sits in what the section *teaches* — the resolved
/// set, not the bare course. This is the half of the tag check the
/// exam-question and homework writers now pay in addition to
/// [`service::subject::in_course`](crate::service::subject::in_course):
/// a subject of the right course can still be off the syllabus.
pub async fn ensure_resolved_member(
    db: &Database,
    instance: &ClassCourse,
    subject: &SubjectId,
) -> Result<(), AppError> {
    match db::offering_subject::resolves_for_class_course(db, instance.get_id(), subject).await? {
        Some(true) => Ok(()),
        Some(false) => Err(ValidationError::Invalid {
            field: "subject_id",
            reason: "subject is not in this section's subject set",
        }
        .into()),
        // The section vanished between the handler's read and this probe;
        // that is a 404, not a validation miss.
        None => Err(AppError::NotFound),
    }
}

/// The request's subject as a validated selection candidate: it exists, and
/// it belongs to `course` — the exact two 400s the tag-time check answers,
/// reused so a refused selection and a refused tag read identically.
async fn subject_of_course(
    db: &Database,
    key: &str,
    course: &CourseId,
) -> Result<Subject, AppError> {
    let subject = db::subject::read(db, &SubjectId::from_key(key))
        .await?
        .ok_or(ValidationError::Invalid {
            field: "subject_id",
            reason: "subject does not exist",
        })?;
    ensure_course_member(&subject, course)?;
    Ok(subject)
}

/// Select a topic for the offering. Manager-only at the route. Answers the
/// subject and whether this call added it — re-adding a selection is a no-op
/// success (`added` = false).
pub async fn add_to_offering(
    db: &Database,
    offering: &CourseOfferingId,
    subject_key: &str,
) -> Result<(Subject, bool), AppError> {
    let offering = service::course_offering::read(db, offering)
        .await?
        .ok_or(AppError::NotFound)?;
    let subject = subject_of_course(db, subject_key, offering.get_course()).await?;
    let added = db::offering_subject::add_for_offering(db, offering.get_id(), subject.get_id())
        .await?;
    Ok((subject, added))
}

/// Drop a topic from the offering's selection. A subject the offering never
/// selected is a 404.
pub async fn remove_from_offering(
    db: &Database,
    offering: &CourseOfferingId,
    subject_key: &str,
) -> Result<(), AppError> {
    let offering = service::course_offering::read(db, offering)
        .await?
        .ok_or(AppError::NotFound)?;
    let dropped =
        db::offering_subject::remove_for_offering(db, offering.get_id(), &SubjectId::from_key(subject_key))
            .await?;
    if !dropped {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Select a topic for the section's own set — the override: the CTE in the
/// db module flips `subjects_inherited` to FALSE in the same statement, so
/// from this call on the section's own table (not the offering's) is
/// authoritative. Idempotent: re-adding is a no-op success (`added` = false).
pub async fn add_to_instance(
    db: &Database,
    instance: &ClassCourse,
    subject_key: &str,
) -> Result<(Subject, bool), AppError> {
    let subject = subject_of_course(db, subject_key, instance.get_course()).await?;
    let (live, added) =
        db::offering_subject::add_for_class_course(db, instance.get_id(), subject.get_id())
            .await?;
    if !live {
        // The section was detached between the handler's gate and this write.
        return Err(AppError::NotFound);
    }
    Ok((subject, added))
}

/// Drop a topic from the section's own set (flagging the set as its own in
/// the same statement, when a row actually went). A subject the section
/// never selected is a 404 — including while inheriting, where it must not
/// silently start an override.
pub async fn remove_from_instance(
    db: &Database,
    instance: &ClassCourse,
    subject_key: &str,
) -> Result<(), AppError> {
    let (live, dropped) = db::offering_subject::remove_for_class_course(
        db,
        instance.get_id(),
        &SubjectId::from_key(subject_key),
    )
    .await?;
    if !live || !dropped {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// The section's reset door: sweep its own rows and restore inheritance —
/// the flag goes back to TRUE in the same statement that sweeps. Answers the
/// number of rows the sweep removed (0 is a fine answer: clearing an empty
/// own set is exactly the request that must re-inherit).
pub async fn reset_instance(db: &Database, instance: &ClassCourse) -> Result<i64, AppError> {
    let (swept, live) = db::offering_subject::reset_for_class_course(db, instance.get_id()).await?;
    if !live {
        return Err(AppError::NotFound);
    }
    Ok(swept)
}

/// Parse a path segment as a subject key — the nil-uuid trick the repo's
/// other id keys use: a malformed segment matches no row and reads as a 404.
/// Kept next to the doors so callers need no domain imports for it.
pub fn subject_key(key: &str) -> SubjectId {
    SubjectId::from_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_test_db;
    use crate::db::course::a_test_instance;
    use crate::db::subject as subject_rows;
    use crate::domain::subject::{SubjectDescription, SubjectName};

    async fn a_topic(db: &Database, course: &CourseId, name: &str) -> Subject {
        subject_rows::create(
            db,
            course,
            SubjectName::try_new(name).unwrap(),
            SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap()
    }

    /// A live instance row for a freshly attached course: inheriting by
    /// default, offering auto-minted for the same course.
    async fn an_instance(db: &Database) -> (ClassCourse, CourseId) {
        let (instance, course) = a_test_instance(db).await;
        let row = service::class_course::read(db, &instance)
            .await
            .unwrap()
            .unwrap();
        (row, course)
    }

    fn names(subjects: &[Subject]) -> Vec<&str> {
        subjects
            .iter()
            .map(|s| s.get_name().as_str())
            .collect::<Vec<_>>()
    }

    /// Inheritance: with the flag at its attach-time TRUE and no own rows,
    /// the section resolves to exactly the offering's set, in name order —
    /// Alpha before Zeta although Zeta was created first, so dropping the
    /// ORDER BY fails this.
    #[tokio::test]
    async fn an_inheriting_section_resolves_the_offerings_set_in_name_order() {
        let (db, _leases) = init_test_db().await;
        let (instance, course) = an_instance(&db).await;
        let zeta = a_topic(&db, &course, "Zeta").await;
        let alpha = a_topic(&db, &course, "Alpha").await;
        for subject in [&zeta, &alpha] {
            db::offering_subject::add_for_offering(
                &db,
                instance.get_offering(),
                subject.get_id(),
            )
            .await
            .unwrap();
        }

        assert!(instance.subjects_inherited());
        let resolved = resolved_for_instance(&db, &instance).await.unwrap();
        assert_eq!(names(&resolved), vec!["Alpha", "Zeta"]);
        // The offering-side resolver agrees — one chain, two doors.
        let offering = resolved_for_offering(&db, instance.get_offering())
            .await
            .unwrap();
        assert_eq!(names(&offering), vec!["Alpha", "Zeta"]);
    }

    /// Own-set-wins, including when empty: one add flips the flag and the
    /// section resolves to its own table; after removing the only own row the
    /// flag is still FALSE and the resolved set is *empty* — the offering's
    /// topics must NOT bleed back in, or "clear the syllabus" would be
    /// impossible.
    #[tokio::test]
    async fn an_owned_set_wins_even_when_it_is_empty() {
        let (db, _leases) = init_test_db().await;
        let (instance, course) = an_instance(&db).await;
        let offering_topic = a_topic(&db, &course, "Offering-only").await;
        db::offering_subject::add_for_offering(
            &db,
            instance.get_offering(),
            offering_topic.get_id(),
        )
        .await
        .unwrap();
        let own = a_topic(&db, &course, "Own").await;

        let (_, added) = add_to_instance(&db, &instance, &own.get_id().key())
            .await
            .unwrap();
        assert!(added);

        let owned = service::class_course::read(&db, instance.get_id())
            .await
            .unwrap()
            .unwrap();
        assert!(!owned.subjects_inherited(), "the add must flip the flag");
        assert_eq!(
            names(&resolved_for_instance(&db, &owned).await.unwrap()),
            vec!["Own"]
        );

        remove_from_instance(&db, &owned, &own.get_id().key())
            .await
            .unwrap();
        let cleared = service::class_course::read(&db, instance.get_id())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !cleared.subjects_inherited(),
            "a plain row delete must not restore inheritance"
        );
        assert!(resolved_for_instance(&db, &cleared).await.unwrap().is_empty());
    }

    /// The membership precondition at both doors: a subject of *another*
    /// course is a 400 with the tag-check's exact reason, writes no row, and
    /// leaves the flag alone. Break `ensure_course_member` (or wire a door
    /// past it) and the asserts on the error and the row counts fail.
    #[tokio::test]
    async fn a_foreign_course_subject_is_refused_at_both_doors() {
        let (db, _leases) = init_test_db().await;
        let (instance, _course) = an_instance(&db).await;
        let (foreign_instance, foreign_course) = an_instance(&db).await;
        let stranger = a_topic(&db, &foreign_course, "Stranger").await;

        for door in [
            add_to_offering(&db, instance.get_offering(), &stranger.get_id().key()).await,
            add_to_instance(&db, &instance, &stranger.get_id().key()).await,
        ] {
            let err = door.unwrap_err();
            assert!(
                matches!(err, AppError::Validation(_)),
                "expected the 400 validation refusal, got {err:?}"
            );
            assert_eq!(
                err.to_string(),
                "subject_id: subject belongs to a different course"
            );
        }

        // Nothing landed anywhere, and the section still inherits.
        assert!(resolved_for_offering(&db, instance.get_offering())
            .await
            .unwrap()
            .is_empty());
        assert!(resolved_for_offering(&db, foreign_instance.get_offering())
            .await
            .unwrap()
            .is_empty());
        assert!(db::offering_subject::list_for_class_course(&db, instance.get_id())
            .await
            .unwrap()
            .is_empty());
        assert!(instance.subjects_inherited());
    }

    /// The reset door: after an own selection, reset sweeps the rows and
    /// flips the flag back to TRUE in the same statement — the offering's set
    /// applies again, and a second reset stays green (idempotent).
    #[tokio::test]
    async fn a_reset_restores_inheritance() {
        let (db, _leases) = init_test_db().await;
        let (instance, course) = an_instance(&db).await;
        let offering_topic = a_topic(&db, &course, "Template").await;
        db::offering_subject::add_for_offering(
            &db,
            instance.get_offering(),
            offering_topic.get_id(),
        )
        .await
        .unwrap();
        let own = a_topic(&db, &course, "Own").await;
        add_to_instance(&db, &instance, &own.get_id().key())
            .await
            .unwrap();

        let swept = reset_instance(&db, &instance).await.unwrap();
        assert_eq!(swept, 1, "the one own row must be gone");

        let restored = service::class_course::read(&db, instance.get_id())
            .await
            .unwrap()
            .unwrap();
        assert!(restored.subjects_inherited());
        assert_eq!(
            names(&resolved_for_instance(&db, &restored).await.unwrap()),
            vec!["Template"]
        );

        // Resetting a section that now holds nothing is still a clean 200-ish
        // success: the door is about the flag as much as the rows.
        assert_eq!(reset_instance(&db, &restored).await.unwrap(), 0);
    }

    /// Idempotent add: the second POST of the same selection succeeds without
    /// a second row — and the flag stays FALSE across both. A duplicate row
    /// (or a 409 on the repeat) fails the count/asserts.
    #[tokio::test]
    async fn re_adding_a_selection_is_a_no_op_success() {
        let (db, _leases) = init_test_db().await;
        let (instance, course) = an_instance(&db).await;
        let topic = a_topic(&db, &course, "Twice").await;

        let (first_subject, first_added) =
            add_to_instance(&db, &instance, &topic.get_id().key()).await.unwrap();
        assert!(first_added);
        let (_, second_added) =
            add_to_instance(&db, &instance, &topic.get_id().key()).await.unwrap();
        assert!(!second_added, "the repeat must be a no-op, not a second row");

        assert_eq!(first_subject.get_id(), topic.get_id());
        let own = db::offering_subject::list_for_class_course(&db, instance.get_id())
            .await
            .unwrap();
        assert_eq!(names(&own), vec!["Twice"]);

        // The offering door idempts the same way.
        let (_, offering_again) =
            add_to_offering(&db, instance.get_offering(), &topic.get_id().key())
                .await
                .unwrap();
        assert!(offering_again);
        let (_, offering_repeat) =
            add_to_offering(&db, instance.get_offering(), &topic.get_id().key())
                .await
                .unwrap();
        assert!(!offering_repeat);
        assert_eq!(
            names(
                &resolved_for_offering(&db, instance.get_offering())
                    .await
                    .unwrap()
            ),
            vec!["Twice"]
        );
    }

    /// The rewire's core: a same-course subject OFF the resolved set is
    /// refused by the tag gate, and accepted once the section selects it —
    /// while the plain course check (`service::subject::in_course`) would
    /// pass both. Whatever makes `ensure_resolved_member` a bare-course
    /// check again fails one of the two asserts.
    #[tokio::test]
    async fn the_tag_gate_checks_the_resolved_set_not_the_bare_course() {
        let (db, _leases) = init_test_db().await;
        let (instance, course) = an_instance(&db).await;
        let selected = a_topic(&db, &course, "Selected").await;
        let unselected = a_topic(&db, &course, "Unselected").await;
        db::offering_subject::add_for_offering(
            &db,
            instance.get_offering(),
            selected.get_id(),
        )
        .await
        .unwrap();

        // Inheriting: the offering's set is the syllabus.
        ensure_resolved_member(&db, &instance, selected.get_id())
            .await
            .unwrap();
        let err = ensure_resolved_member(&db, &instance, unselected.get_id())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "subject_id: subject is not in this section's subject set"
        );

        // Same subject, once the section owns it: accepted. The owned set is
        // authoritative *instead of* the offering's, so `selected` — in the
        // course, on the offering — is now OFF the syllabus.
        add_to_instance(&db, &instance, &unselected.get_id().key())
            .await
            .unwrap();
        ensure_resolved_member(&db, &instance, unselected.get_id())
            .await
            .unwrap();
        let err = ensure_resolved_member(&db, &instance, selected.get_id())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "subject_id: subject is not in this section's subject set"
        );
    }
}
