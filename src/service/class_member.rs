//! Class-membership workflows: the member-axis add and the two exits — the
//! soft `leave` a route takes and the hard `remove` a transfer rollback needs —
//! with the pump's refusals mapped to this route's answers and a zero-row
//! sweep turned into a 404, plus the roster reads the web layer pages through.
//! The transaction itself lives in [`crate::db::class_pump`]; the table reads
//! in [`crate::db::class_member`].

use crate::constant::{MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use crate::database::Database;
use crate::db::class_member;
use crate::db::class_pump::{self, Axis};
use crate::domain::class_group::ClassGroupId;
use crate::domain::class_member::ClassMember;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Put `user` in `class` and enroll them into every course the class is
/// already attached to, in one transaction.
///
/// A student already enrolled in one of those instances keeps the row they
/// have — no second enrollment and their `source` untouched, so a hand-placed
/// enrollment is never adopted by the class.
pub async fn add(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
    added_by: &UserId,
) -> Result<ClassMember, AppError> {
    // Read off the refusal, never respelled here: this route and a
    // blueprint pump answer one vocabulary. `Made` is the only `None`, and
    // it takes the `Ok` arm below.
    let landed = class_pump::add_member(db, class, user, added_by).await?;
    let code = landed.refusal_code(&Axis::Member).unwrap_or_default();
    match landed {
        class_pump::Attached::Made(saved) => Ok(saved),
        class_pump::Attached::Duplicate => Err(AppError::ConflictCoded {
            code,
            message: "the student is already in this class".into(),
        }),
        class_pump::Attached::Gone => Err(AppError::NotFound),
        // The pivot is the student: they were demoted while this ran, which
        // is the refusal the caller's own read makes a moment earlier and
        // the only one that can arrive after it.
        class_pump::Attached::PivotGone => Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be added to a class",
        })),
        class_pump::Attached::ClassFull => Err(AppError::ConflictCoded {
            code,
            message: format!("this class already holds {MAX_CLASS_MEMBERS} students"),
        }),
        // The other axis: adding one student enrolls them into every
        // attached course, so a class over *that* ceiling cannot take a
        // member however much room its roster has.
        class_pump::Attached::ClassOverloaded => Err(AppError::ConflictCoded {
            code,
            message: format!(
                "this class holds more than {MAX_CLASS_COURSES} courses — \
                 detach some before adding a student"
            ),
        }),
        // Not a ceiling someone can raise, and told apart from one on
        // purpose: no number changes this class's answer, only detaching the
        // link the deleted course left behind.
        class_pump::Attached::CourseGone(course) => Err(AppError::ConflictCoded {
            code,
            message: format!("{course} no longer exists — detach it from this class first"),
        }),
        // Only an attach run on a blueprint's behalf states that claim, and
        // a membership is nobody's but its own.
        class_pump::Attached::SourceGone => Err(AppError::Internal(
            "a class membership has no blueprint to lose".into(),
        )),
    }
}

/// Take `user` out of `class` — the *soft* exit the route takes: the stint's
/// `left_at` is stamped and the row stays as history, while the class's member
/// count comes back so the student no longer counts as a live member. A
/// student who rejoins later gets a fresh live row beside the old one, which
/// is how "left in October, came back in January" is represented.
///
/// A student who was not in the class is a [`AppError::NotFound`], raised here
/// rather than left to each caller to re-derive from a boolean. The
/// enrollments the class pumped for them are released — one row per instance,
/// so a seat they also hold in another section teaching the same course is
/// that section's own row and is not touched (see
/// [`class_pump::leave_member`]).
pub async fn leave(db: &Database, class: &ClassGroupId, user: &UserId) -> Result<(), AppError> {
    let gone = class_pump::leave_member(db, class, user).await?;
    (gone > 0).then_some(()).ok_or(AppError::NotFound)
}

/// Take `user` out of `class` for good — the hard exit, which deletes the
/// stint rather than stamping it. Kept for the şube-transfer rollback path
/// alone, where the stint being undone must leave no trace a later read could
/// mistake for history; every route uses [`leave`].
///
/// The enrollment release is [`leave`]'s: every row the stint owned goes, and
/// no other section's seat is affected (see [`class_pump::remove_member`]).
pub async fn remove(db: &Database, class: &ClassGroupId, user: &UserId) -> Result<(), AppError> {
    let gone = class_pump::remove_member(db, class, user).await?;
    (gone > 0).then_some(()).ok_or(AppError::NotFound)
}

/// The class's roster, newest first, paged — the read behind
/// `GET /classes/{id}/members`.
pub async fn list_for_class(
    db: &Database,
    class: &ClassGroupId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassMember>, i64), AppError> {
    class_member::list_for_class(db, class, limit, offset).await
}

/// The classes one student belongs to, newest membership first, paged — the
/// read behind `GET /classes/me` and `GET /classes/user/{user}`.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassMember>, i64), AppError> {
    class_member::list_for_user(db, user, limit, offset).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::class_member::is_live_member;
    use crate::db::class_member::tests::{
        a_class, a_course, counter, enrollment_exists, rows, source_of,
    };
    use crate::service::class_course;

    /// Adding a member enrolls them into every instance the class already
    /// carries, each row tagged with the class, and the class's own member
    /// counter ticks once.
    #[tokio::test]
    async fn a_member_is_enrolled_into_every_attached_instance() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let physics = a_course("physics", &db).await;
        let algebra_instance = class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        let physics_instance = class_course::attach(&db, &class, &physics, &manager)
            .await
            .unwrap();

        add(&db, &class, &student, &manager).await.unwrap();

        for instance in [&algebra_instance, &physics_instance] {
            assert_eq!(
                source_of(instance.get_id(), &student, &db).await,
                Some(Some(class.clone())),
                "every attached instance must hold a row tagged with the class"
            );
            assert_eq!(
                counter("enrollment_count", instance.get_id().uuid(), &db).await,
                1
            );
        }
        assert_eq!(counter("class_member_count", class.uuid(), &db).await, 1);
    }

    /// The pumped row keys the same `(instance, user)` pair a hand enroll
    /// writes, or the two paths would key one pair two ways and only the
    /// primary key (a 500) would notice.
    #[tokio::test]
    async fn a_pumped_row_uses_the_hand_enrolls_pair() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        assert!(
            enrollment_exists(instance.get_id(), &student, &db).await,
            "the pump must key the pair the way enrollment does"
        );
    }

    /// Re-adding is a 409, and it must cost nothing: no second counter tick.
    #[tokio::test]
    async fn a_second_add_is_refused_and_writes_nothing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        let again = add(&db, &class, &student, &manager).await;
        assert!(
            matches!(again, Err(AppError::ConflictCoded { code, .. }) if code == "duplicate"),
            "a second add is a 409 coded `duplicate`: {again:?}"
        );
        assert_eq!(
            counter("class_member_count", class.uuid(), &db).await,
            1,
            "a refused add may not tick the counter"
        );
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            1
        );
    }

    /// A student already enrolled by hand keeps their own row: the pump skips
    /// it, and leaving the class leaves it standing.
    #[tokio::test]
    async fn a_hand_placed_enrollment_is_skipped_and_survives_the_exit() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        crate::db::enrollment::enroll(&db, instance.get_id(), &student, &manager, None)
            .await
            .unwrap();

        add(&db, &class, &student, &manager).await.unwrap();
        assert_eq!(
            source_of(instance.get_id(), &student, &db).await,
            Some(None),
            "the class must not adopt a hand-placed row"
        );
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            1,
            "the skipped pair may not be counted twice"
        );

        leave(&db, &class, &student).await.unwrap();
        assert_eq!(
            source_of(instance.get_id(), &student, &db).await,
            Some(None),
            "the sweep may only take back the rows the class wrote"
        );
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            1
        );
    }

    /// The soft exit: the stint is stamped, not deleted, and a rejoin writes a
    /// fresh live row beside it — the shape "left in October, came back in
    /// January" needs.
    #[tokio::test]
    async fn a_leave_stamps_the_stint_and_a_rejoin_writes_a_second_row() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        leave(&db, &class, &student).await.unwrap();
        assert_eq!(
            rows("class_member", &db).await,
            1,
            "the stint is history, not a deletion"
        );
        assert!(
            !is_live_member(&db, &class, &student).await.unwrap(),
            "…and it no longer counts as a live member"
        );
        assert_eq!(counter("class_member_count", class.uuid(), &db).await, 0);

        // The rejoin is a fresh row: the partial unique index only guards the
        // live stint, so the pair may hold one live row again.
        add(&db, &class, &student, &manager).await.unwrap();
        assert_eq!(rows("class_member", &db).await, 2);
        assert!(is_live_member(&db, &class, &student).await.unwrap());
        assert_eq!(counter("class_member_count", class.uuid(), &db).await, 1);
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            1,
            "the rejoin enrols the pair once, not twice"
        );
    }

    /// The hard exit deletes the stint and sweeps the rows the class pumped —
    /// the transfer-rollback path.
    #[tokio::test]
    async fn a_remove_deletes_the_stint_and_the_rows_the_class_pumped() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        remove(&db, &class, &student).await.unwrap();
        assert_eq!(rows("class_member", &db).await, 0);
        assert_eq!(source_of(instance.get_id(), &student, &db).await, None);
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            0,
            "the count must come back with the row that held it"
        );
        assert_eq!(counter("class_member_count", class.uuid(), &db).await, 0);
        let again = remove(&db, &class, &student).await;
        assert!(
            matches!(again, Err(AppError::NotFound)),
            "a second removal is a 404, not a second sweep: {again:?}"
        );
    }

    /// A member whose class no longer carries the course still leaves cleanly:
    /// the membership row and the pumped enrollment are independent facts.
    #[tokio::test]
    async fn a_sweep_tolerates_an_instance_the_class_already_dropped() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        // The detach takes the instance and the rows it pumped; the stint
        // survives it, so the exit has nothing left to sweep.
        class_course::detach(&db, &class, &algebra).await.unwrap();
        leave(&db, &class, &student)
            .await
            .expect("the membership must still be removable with its enrollment gone");
        assert_eq!(counter("class_member_count", class.uuid(), &db).await, 0);
    }

    /// The role-change sweep, which is one transaction with the role write
    /// itself: the memberships go with their counters, the enrollment rows go
    /// with their instance counts, and each count comes back exactly once —
    /// the double decrement the old two-call split existed to avoid.
    #[tokio::test]
    async fn the_role_sweep_takes_memberships_and_enrollments_together() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        // A real row, because the sweep now rides on the role write.
        let account = crate::db::user::create(
            &db,
            crate::domain::user::Username::try_new("ogrenci").unwrap(),
            None,
        )
        .await
        .unwrap();
        let student = *account.get_id();
        let first = a_class("9-A", &db).await;
        let second = a_class("club", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = class_course::attach(&db, &first, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &first, &student, &manager).await.unwrap();
        add(&db, &second, &student, &manager).await.unwrap();

        crate::service::user::set_role(&db, account.get_id(), crate::domain::role::Role::Teacher)
            .await
            .unwrap();
        assert_eq!(
            rows("class_member", &db).await,
            2,
            "the stints stay as history — a soft leave, not a deletion"
        );
        for class in [&first, &second] {
            assert!(
                !is_live_member(&db, class, &student).await.unwrap(),
                "no live stint may survive the sweep"
            );
            assert_eq!(
                counter("class_member_count", class.uuid(), &db).await,
                0,
                "every class must get its member count back"
            );
        }
        assert_eq!(
            source_of(instance.get_id(), &student, &db).await,
            None,
            "the enrollment goes in the same transaction as the membership"
        );
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            0,
            "…and its count comes back once, not twice"
        );
        // Which is the point of folding them: the sweep is one transaction, so
        // no enrollment row is left tagged with a class whose live roster is
        // already empty — the shape that stranded rows nothing could reach.
        assert_eq!(
            counter("class_course_count", first.uuid(), &db).await,
            1,
            "the attachment itself is untouched by a role sweep"
        );
    }

    /// D4: a member exit **releases** the rows the leaving section wrote —
    /// there is no heir to hand one over to. Two şubeler teaching the same
    /// catalog course hold the student in their *own* instance, each with its
    /// own row tagged with its own class, so a release may only take the
    /// leaving section's row and count back; the rival's row stands exactly as
    /// written. The old rule re-tagged the row with a rival section instead
    /// (keeping it on the juncture of a class that no longer held the student),
    /// which is what this pins gone.
    #[tokio::test]
    async fn a_member_exit_releases_its_own_row_and_leaves_the_rivals_standing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let first = a_class("5-A", &db).await;
        let second = a_class("5-B", &db).await;
        let algebra = a_course("algebra", &db).await;
        let first_instance = class_course::attach(&db, &first, &algebra, &manager)
            .await
            .unwrap();
        let second_instance = class_course::attach(&db, &second, &algebra, &manager)
            .await
            .unwrap();

        // Both sections hold the student, so both instances carry a row — each
        // tagged with the section that pumped it.
        for class in [&first, &second] {
            add(&db, class, &student, &manager).await.unwrap();
        }
        assert_eq!(
            source_of(first_instance.get_id(), &student, &db).await,
            Some(Some(first.clone()))
        );
        assert_eq!(
            source_of(second_instance.get_id(), &student, &db).await,
            Some(Some(second.clone()))
        );

        // 5-A lets the student go: its own row goes and its own count comes
        // back. 5-B's row is a row *it* wrote, on *its* instance, and stands.
        leave(&db, &first, &student).await.unwrap();
        assert_eq!(
            source_of(first_instance.get_id(), &student, &db).await,
            None,
            "the leaving section's row must be released"
        );
        assert_eq!(
            counter("enrollment_count", first_instance.get_id().uuid(), &db).await,
            0,
            "…and its roster count comes back with it"
        );
        assert_eq!(
            source_of(second_instance.get_id(), &student, &db).await,
            Some(Some(second.clone())),
            "the rival section's own row and its tag must stand"
        );
        assert_eq!(
            counter("enrollment_count", second_instance.get_id().uuid(), &db).await,
            1,
            "…and so must the rival's roster count"
        );
        assert_eq!(counter("class_member_count", first.uuid(), &db).await, 0);
        assert_eq!(counter("class_member_count", second.uuid(), &db).await, 1);
    }
}
