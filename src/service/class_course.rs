//! Class-course workflows: the attach that maps the pump's refusals to this
//! route's answers, and the detach that turns a zero-row sweep into a 404.
//! The transactions live in [`crate::db::class_pump`] and
//! [`crate::db::class_course`].

use crate::constant::{MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use crate::database::Database;
use crate::db::class_course;
use crate::db::class_pump::{self, Axis};
use crate::domain::class_course::ClassCourse;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Attach `course` to `class` and enroll the class's whole roster into it,
/// in one transaction.
///
/// Students already in the course keep the rows they have — no seat
/// charged, `source` untouched — and a roster that does not fit refuses the
/// whole attach rather than filling the course to its cap and stopping.
pub async fn attach(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
    attached_by: &UserId,
) -> Result<ClassCourse, AppError> {
    let landed = class_course::attach_sourced(db, class, course, attached_by, None).await?;
    // Read off the refusal, never respelled here: this route and a
    // blueprint pump answer one vocabulary. `Made` is the only `None`, and
    // it takes the `Ok` arm below.
    let code = landed.refusal_code(&Axis::Course).unwrap_or_default();
    match landed {
        class_pump::Attached::Made(saved) => Ok(saved),
        class_pump::Attached::Duplicate => Err(AppError::ConflictCoded {
            code,
            message: "the course is already on this class".into(),
        }),
        // The class or the course: either end of the link being gone is a
        // 404 on this route, and only a blueprint's skip list needs them
        // told apart.
        class_pump::Attached::Gone | class_pump::Attached::PivotGone => Err(AppError::NotFound),
        class_pump::Attached::ClassFull => Err(AppError::ConflictCoded {
            code,
            message: format!("this class already holds {MAX_CLASS_COURSES} courses"),
        }),
        // The other axis: attaching one course enrolls the whole roster, so
        // a class over *that* ceiling cannot take a course however few it
        // carries. Only a class predating the ceiling can be here.
        class_pump::Attached::ClassOverloaded => Err(AppError::ConflictCoded {
            code,
            message: format!(
                "this class holds more than {MAX_CLASS_MEMBERS} students — \
                 remove some before attaching a course"
            ),
        }),
        class_pump::Attached::Full(full) => Err(AppError::ConflictCoded {
            code,
            message: format!("{full} cannot hold the whole class"),
        }),
        // Unreachable on this axis, and kept because the match is
        // exhaustive: the pair loop enrolls the roster into `$pivot` and
        // nothing else, and the pivot claim already proved *that* course
        // alive inside the same transaction. Only a member add walks a
        // class's existing course links, so `linked_course_missing` is that
        // route's alone — which is why this route's `409` does not publish
        // it. Left mapped rather than folded into an `Internal`, so a future
        // axis change is a wrong-looking 409, not a 500.
        class_pump::Attached::CourseGone(course) => Err(AppError::ConflictCoded {
            code,
            message: format!("{course} no longer exists — detach it from this class first"),
        }),
        // This path passes no source, so the claim that answers this is
        // never in the transaction it ran.
        class_pump::Attached::SourceGone => Err(AppError::Internal(
            "a hand attach has no blueprint to lose".into(),
        )),
    }
}

/// Detach `course` from `class` and sweep the enrollments the class pumped
/// into it. A course that was not attached is a [`AppError::NotFound`],
/// raised here rather than left to each caller to re-derive from a boolean.
///
/// to that class (see [`class_pump::detach_course`]).
pub async fn detach(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
) -> Result<(), AppError> {
    let gone = class_pump::detach_course(db, class, course, None).await?;
    (gone > 0).then_some(()).ok_or(AppError::NotFound)
}

/// The courses a class is attached to, newest first, paged — the read behind
/// `GET /classes/{id}/courses`.
pub async fn list_for_class(
    db: &Database,
    class: &ClassGroupId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassCourse>, i64), AppError> {
    class_course::list_for_class(db, class, limit, offset).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::class_member::tests::{
        a_class, a_course, counter, course_exists, link_exists, rows, source_of,
    };
    use crate::service::class_member;

    /// Attaching seeds the course from the roster the class already holds.
    #[tokio::test]
    async fn an_attach_enrolls_the_whole_roster() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        let students = [
            crate::db::class_member::tests::fixture_user(&db, "a").await,
            crate::db::class_member::tests::fixture_user(&db, "b").await,
        ];
        for student in &students {
            class_member::add(&db, &class, student, &manager)
                .await
                .unwrap();
        }

        attach(&db, &class, &algebra, &manager).await.unwrap();
        for student in &students {
            assert_eq!(
                source_of(&algebra, student, &db).await,
                Some(Some(class.clone()))
            );
        }
        assert_eq!(counter("enrollment_count", algebra.uuid(), &db).await, 2);
        assert_eq!(counter("class_course_count", class.uuid(), &db).await, 1);

        let again = attach(&db, &class, &algebra, &manager).await;
        assert!(
            matches!(again, Err(AppError::ConflictCoded { code, .. }) if code == "duplicate"),
            "a second attach is a 409 coded `duplicate`: {again:?}"
        );
    }

    /// A course with room for only part of the roster takes none of it: zero
    /// enrollments, the counter unmoved, and no attachment row.
    #[tokio::test]
    async fn a_roster_that_does_not_fit_attaches_nothing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let class = a_class("9-A", &db).await;
        let tight = a_course("algebra", Some(2), &db).await;
        for name in ["a", "b", "c"] {
            let student = crate::db::class_member::tests::fixture_user(&db, name).await;
            class_member::add(&db, &class, &student, &manager).await.unwrap();
        }

        let refused = attach(&db, &class, &tight, &manager).await;
        assert!(
            matches!(refused, Err(AppError::ConflictCoded { code, ref message })
                if code == "course_full" && message.contains(tight.key().as_str())),
            "the refusal must be coded `course_full` and name the course: {refused:?}"
        );
        assert_eq!(
            rows("enrollment", &db).await,
            0,
            "the two seats that did fit must be given back with the third"
        );
        assert_eq!(
            rows("class_course", &db).await,
            0,
            "…and no attachment row may survive"
        );
        assert_eq!(counter("enrollment_count", tight.uuid(), &db).await, 0);
        assert_eq!(counter("class_course_count", class.uuid(), &db).await, 0);
    }

    /// A hand-placed row is skipped on the way in and left standing on the way
    /// out — the class never owned it.
    #[tokio::test]
    async fn a_hand_placed_row_survives_the_detach() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        crate::db::enrollment::enroll(&db, &algebra, &student, &manager)
            .await
            .unwrap();
        class_member::add(&db, &class, &student, &manager)
            .await
            .unwrap();

        attach(&db, &class, &algebra, &manager).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(None),
            "an attach may not adopt a hand-placed row"
        );
        assert_eq!(
            counter("enrollment_count", algebra.uuid(), &db).await,
            1,
            "…nor charge a seat for it"
        );

        detach(&db, &class, &algebra).await.unwrap();
        assert_eq!(source_of(&algebra, &student, &db).await, Some(None));
        assert_eq!(counter("enrollment_count", algebra.uuid(), &db).await, 1);
        let again = detach(&db, &class, &algebra).await;
        assert!(
            matches!(again, Err(AppError::NotFound)),
            "a second detach is a 404: {again:?}"
        );
    }

    /// Detaching with nobody else claiming the rows deletes them and gives the
    /// seats back.
    #[tokio::test]
    async fn a_detach_sweeps_the_rows_it_pumped() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        class_member::add(&db, &class, &student, &manager)
            .await
            .unwrap();
        attach(&db, &class, &algebra, &manager).await.unwrap();

        detach(&db, &class, &algebra).await.unwrap();
        assert_eq!(source_of(&algebra, &student, &db).await, None);
        assert_eq!(counter("enrollment_count", algebra.uuid(), &db).await, 0);
        assert_eq!(counter("class_course_count", class.uuid(), &db).await, 0);
        assert_eq!(
            rows("class_member", &db).await,
            1,
            "the two axes are independent: dropping the course may not drop \
             the student out of the class as well"
        );
    }

    /// Two classes, one course, one student in both: the second attach skipped
    /// the row the first wrote, so the row names only the first class — and
    /// detaching *that* class must hand the row to the second rather than
    /// unenroll a student the second is still responsible for. Detaching the
    /// second then finds nobody left and takes the seat back.
    #[tokio::test]
    async fn a_detach_hands_a_shared_row_to_the_class_that_still_claims_it() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = UserId::from_key("student");
        let algebra = a_course("algebra", None, &db).await;
        let first = a_class("9-A", &db).await;
        let second = a_class("club", &db).await;
        for class in [&first, &second] {
            class_member::add(&db, class, &student, &manager)
                .await
                .unwrap();
            attach(&db, class, &algebra, &manager).await.unwrap();
        }
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(Some(first.clone())),
            "the second attach must skip the row the first wrote"
        );
        assert_eq!(
            counter("enrollment_count", algebra.uuid(), &db).await,
            1,
            "…and pay no second seat for it"
        );

        detach(&db, &first, &algebra).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(Some(second.clone())),
            "the row must be handed to the class that still claims it"
        );
        assert_eq!(
            counter("enrollment_count", algebra.uuid(), &db).await,
            1,
            "a repair is not a release"
        );

        detach(&db, &second, &algebra).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            None,
            "the last claimant leaving takes the row with it"
        );
        assert_eq!(counter("enrollment_count", algebra.uuid(), &db).await, 0);
    }

    /// The same repair along the member axis: removing the student from the
    /// class that owns the row hands it to the other class they are in.
    #[tokio::test]
    async fn a_member_removal_hands_a_shared_row_over_too() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = UserId::from_key("student");
        let algebra = a_course("algebra", None, &db).await;
        let first = a_class("9-A", &db).await;
        let second = a_class("club", &db).await;
        for class in [&first, &second] {
            attach(&db, class, &algebra, &manager).await.unwrap();
            class_member::add(&db, class, &student, &manager)
                .await
                .unwrap();
        }

        class_member::remove(&db, &first, &student).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(Some(second.clone())),
            "the student is still in the second class, so the row stays"
        );
        assert_eq!(counter("enrollment_count", algebra.uuid(), &db).await, 1);

        class_member::remove(&db, &second, &student).await.unwrap();
        assert_eq!(source_of(&algebra, &student, &db).await, None);
        assert_eq!(counter("enrollment_count", algebra.uuid(), &db).await, 0);
    }

    /// The restore half of the pivot claim: it bumps the course's counter to
    /// prove the row is there and must put it back *exactly* as it found it —
    /// absent, which the boot backfill keys on (`WHERE enrollment_count =
    /// NONE`). An empty roster is the case that has no seat write to hide a
    /// leftover bump behind.
    #[tokio::test]
    async fn the_pivot_claim_gives_the_courses_counter_back_untouched() {
        let (db, _leases) = crate::database::init_test_db().await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        let absent = "SELECT VALUE id FROM course WHERE enrollment_count = NONE";
        assert_eq!(
            rows(absent, &db).await,
            1,
            "a fresh course carries no count"
        );

        attach(&db, &class, &algebra, &UserId::from_key("manager"))
            .await
            .unwrap();
        assert_eq!(
            rows(absent, &db).await,
            1,
            "the claim's bump must be restored to absent, not to 0"
        );
    }

    /// The `Menu::delete` defect on the class layer: a `class_course` row must
    /// not outlive the course it names. The attach's proof that the course is
    /// still there is a *write* on the course row
    /// ([`crate::db::class_pump::Axis::pivot_claim`]), landing it on the
    /// very key `Course::delete`'s guard writes — but only because that write
    /// now *moves* the counter. The `count = count` this shipped with left the
    /// document unchanged, which SurrealDB 3.2.3 elides: it entered no write
    /// set, collided with nothing, and both callers were told OK while the link
    /// stayed pointing at a deleted course.
    ///
    /// The roster is empty deliberately — that is the only shape where nothing
    /// else in the transaction touches the course row, so the claim is the
    /// whole guard. One child per round, for the reason the menu twin
    /// documents: a second writer makes the delete lose and re-send, and the
    /// re-sent sweep clears the evidence.
    ///
    /// The window is opened by the schema, not by a lucky interleaving: a
    /// `DEFINE EVENT` on `course` fires inside the delete's own transaction the
    /// instant the row goes, so the `SLEEP` lands between the delete and its
    /// `DELETE class_course WHERE course = $course` sweep every time.
    ///
    /// Real server, and `#[ignore]`d for it: the subject *is* the store's
    /// conflict detection, which `init_mem`'s embedded engine does not have —
    /// it commits both and answers `Ok` to each, so this passes there on broken
    /// code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_class_course_link_never_outlives_the_course() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let (mut orphans, mut swept, mut miscounted) = (0, 0, 0);
        for round in 0..4 {
            let class = a_class(&format!("9-{round}"), &db).await;
            let algebra = a_course(&format!("algebra{round}"), None, &db).await;
            let course = crate::db::course::read(&db, &algebra)
                .await
                .unwrap()
                .unwrap();

            // The old engine needed a schema event to hold the delete's
            // window open; Postgres puts the racing delete and attach on the
            // same rows, so a barrier start covers every interleaving — the
            // link row may never outlive the course in any of them.
            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, gate, course) = (db.clone(), gate.clone(), course.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    crate::db::course::delete(&db, course).await
                })
            };
            let child = {
                let (db, class, algebra, manager, gate) =
                    (db.clone(), class.clone(), algebra.clone(), manager.clone(), gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    attach(&db, &class, &algebra, &manager).await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            // A 404 for the attach, or a refusal for the delete, is a correct
            // answer — the only defect is stored state.
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced attach must be answered, not 500: {child:?}"
            );

            // Stored state is the whole verdict; a return value is not evidence.
            if !course_exists(&algebra, &db).await {
                swept += 1;
                if link_exists(&class, &algebra, &db).await {
                    orphans += 1;
                }
                miscounted += counter("class_course_count", class.uuid(), &db).await;
            } else if matches!(drop_it, Ok(true)) {
                panic!("round {round}: the delete reported success but the course is still there");
            }
        }
        eprintln!("Course::delete raced by an attach: {swept}/4 rounds deleted the course");
        assert!(
            swept > 0,
            "no round ever deleted the course, so the race never actually ran"
        );
        assert_eq!(orphans, 0, "a class_course link outlived its course");
        assert_eq!(miscounted, 0, "a class counts a course that is gone");
    }
}
