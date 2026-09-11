//! Class-membership workflows: the member-axis add and remove — the pump's
//! refusals mapped to this route's answers, and a zero-row sweep turned into
//! a 404 — plus the roster reads the web layer pages through. The transaction
//! itself lives in [`crate::db::class_pump`]; the table reads in
//! [`crate::db::class_member`].

use surrealdb::types::SurrealValue;

use crate::constant::{MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use crate::database::Database;
use crate::db::class_member;
use crate::db::class_pump::{self, Axis};
use crate::domain::class_group::ClassGroupId;
use crate::domain::class_member::{ClassMember, ClassMemberId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Put `user` in `class` and enroll them into every course the class is
/// already attached to, in one transaction.
///
/// A student already enrolled in one of those courses keeps the row they
/// have — no seat is charged and their `source` is untouched, so a
/// hand-placed enrollment is never adopted by the class — and a course with
/// no free seat refuses the *whole* join rather than half of it.
pub async fn add(
    db: &Database,
    class: &ClassGroupId,
    user: &UserId,
    added_by: &UserId,
) -> Result<ClassMember, AppError> {
    let member = ClassMember {
        id: ClassMemberId::composite(class, user),
        class: class.clone(),
        user: user.clone(),
        added_by: added_by.clone(),
        added_at: Some(Timestamp::now()),
    };
    let landed = class_pump::attach(
        db,
        class,
        Axis::Member,
        (&member.id.record(), &member),
        user.record(),
        added_by.record(),
        None,
    )
    .await?;
    // Read off the refusal, never respelled here: this route and a
    // blueprint pump answer one vocabulary. `Made` is the only `None`, and
    // it takes the `Ok` arm below.
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
        class_pump::Attached::Full(course) => Err(AppError::ConflictCoded {
            code,
            message: format!("{course} is full, so the class cannot take this student"),
        }),
        // Not a capacity problem, and told apart from one on purpose: no
        // number anyone can raise unblocks this class, only detaching the
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

/// Take `user` out of `class` and sweep the enrollments the class pumped
/// for them. A student who was not in the class is a
/// [`AppError::NotFound`], raised here rather than left to each caller to
/// re-derive from a boolean.
///
/// An enrollment another attached class still claims is re-tagged to that
/// class instead of deleted (see [`crate::db::class_pump::detach`]).
pub async fn remove(db: &Database, class: &ClassGroupId, user: &UserId) -> Result<(), AppError> {
    let gone = class_pump::detach(
        db,
        "$link",
        Axis::Member,
        &[(
            "link".into(),
            ClassMemberId::composite(class, user).record().into_value(),
        )],
    )
    .await?;
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
    use crate::db::class_member::tests::{a_class, a_course, counter, exists, rows, source_of};
    use crate::domain::enrollment::EnrollmentId;
    use crate::service::class_course;

    /// Adding a member enrolls them into every course the class already
    /// carries, each row tagged with the class, and the class's own member
    /// counter ticks once.
    #[tokio::test]
    async fn a_member_is_enrolled_into_every_attached_course() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        let physics = a_course("physics", None, &db).await;
        class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        class_course::attach(&db, &class, &physics, &manager)
            .await
            .unwrap();

        add(&db, &class, &student, &manager).await.unwrap();

        for course in [&algebra, &physics] {
            assert_eq!(
                source_of(course, &student, &db).await,
                Some(Some(class.clone())),
                "every attached course must hold a row tagged with the class"
            );
            assert_eq!(counter("enrollment_count", course.record(), &db).await, 1);
        }
        assert_eq!(counter("class_member_count", class.record(), &db).await, 1);
    }

    /// The pumped row must carry the *same* composite id a hand enroll would
    /// mint, or the two paths would key one pair two ways and only the unique
    /// index (a 500) would notice.
    #[tokio::test]
    async fn a_pumped_row_uses_the_hand_enrolls_composite_id() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        assert!(
            exists(EnrollmentId::composite(&algebra, &student).record(), &db).await,
            "the pump must key the pair the way Enrollment::composite does"
        );
    }

    /// Re-adding is a 409, and it must cost nothing: no second counter tick and
    /// no second sweep of anybody's enrollment.
    #[tokio::test]
    async fn a_second_add_is_refused_and_writes_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        let again = add(&db, &class, &student, &manager).await;
        assert!(
            matches!(again, Err(AppError::ConflictCoded { code, .. }) if code == "duplicate"),
            "a second add is a 409 coded `duplicate`: {again:?}"
        );
        assert_eq!(
            counter("class_member_count", class.record(), &db).await,
            1,
            "a refused add may not tick the counter"
        );
        assert_eq!(counter("enrollment_count", algebra.record(), &db).await, 1);
    }

    /// Adding into a class whose courses have no room refuses the *whole* join:
    /// the roomy course must not keep a seat the full one denied.
    #[tokio::test]
    async fn a_full_course_refuses_the_whole_join() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let roomy = a_course("algebra", None, &db).await;
        let full = a_course("physics", Some(0), &db).await;
        class_course::attach(&db, &class, &roomy, &manager)
            .await
            .unwrap();
        class_course::attach(&db, &class, &full, &manager)
            .await
            .unwrap();

        // The whole record id, not just the key: a caller reading the 409 must
        // not have to guess which table the name came from.
        let named = format!("{}:{}", crate::constant::COURSE_TABLE, full.key());
        let refused = add(&db, &class, &student, &manager).await;
        assert!(
            matches!(refused, Err(AppError::ConflictCoded { code, ref message })
                if code == "course_full" && message.contains(&named)),
            "the refusal must be coded `course_full` and name the course that had no seat \
             as {named}: {refused:?}"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM enrollment", &db).await,
            0,
            "not one seat may survive the refusal"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM class_member", &db).await,
            0,
            "…nor the membership row itself"
        );
        assert_eq!(
            counter("enrollment_count", roomy.record(), &db).await,
            0,
            "…nor the roomy course's counter"
        );
        assert_eq!(counter("class_member_count", class.record(), &db).await, 0);
    }

    /// A student already enrolled by hand keeps their own row: no seat is
    /// charged for them, the row's `source` stays empty, and removing them from
    /// the class leaves it standing.
    #[tokio::test]
    async fn a_hand_placed_enrollment_is_skipped_and_survives_removal() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        crate::db::enrollment::enroll(&db, &algebra, &student, &manager)
            .await
            .unwrap();
        class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();

        add(&db, &class, &student, &manager).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(None),
            "the class must not adopt a hand-placed row"
        );
        assert_eq!(
            counter("enrollment_count", algebra.record(), &db).await,
            1,
            "the skipped pair may not be charged a second seat"
        );

        remove(&db, &class, &student).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(None),
            "the sweep may only take back the rows the class wrote"
        );
        assert_eq!(counter("enrollment_count", algebra.record(), &db).await, 1);
    }

    /// Removal with nobody else claiming the row: the enrollment goes and its
    /// seat comes back.
    #[tokio::test]
    async fn removal_deletes_the_rows_the_class_pumped() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        remove(&db, &class, &student).await.unwrap();
        assert_eq!(source_of(&algebra, &student, &db).await, None);
        assert_eq!(
            counter("enrollment_count", algebra.record(), &db).await,
            0,
            "the seat must come back with the row that held it"
        );
        assert_eq!(counter("class_member_count", class.record(), &db).await, 0);
        let again = remove(&db, &class, &student).await;
        assert!(
            matches!(again, Err(AppError::NotFound)),
            "a second removal is a 404, not a second sweep: {again:?}"
        );
    }

    /// A member who is not in any attached course still removes cleanly, and a
    /// course deleted out from under the class leaves nothing to sweep — the
    /// membership row and the pumped enrollment are independent facts.
    #[tokio::test]
    async fn a_sweep_tolerates_rows_a_course_delete_already_took() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &class, &student, &manager).await.unwrap();

        // `Course::delete` refuses while the roster is occupied, so the wipe is
        // spelled the way the cascade does it, minus the guard.
        db.query("DELETE enrollment WHERE course = $course; DELETE class_course WHERE course = $course; DELETE $course;")
            .bind(("course", algebra.record()))
            .await
            .unwrap()
            .check()
            .unwrap();

        remove(&db, &class, &student)
            .await
            .expect("the membership must still be removable with its enrollment gone");
        assert_eq!(counter("class_member_count", class.record(), &db).await, 0);
    }

    /// The role-change sweep, which is one transaction with the role write
    /// itself: the memberships go with their counters, the enrollment rows go
    /// with their seats, and each seat comes back exactly once — the double
    /// decrement the old two-call split existed to avoid.
    #[tokio::test]
    async fn the_role_sweep_takes_memberships_and_enrollments_together() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        // A real row, because the sweep now rides on the role write.
        let hash = crate::domain::user::Password::try_new("secret1")
            .unwrap()
            .hash_async()
            .await
            .unwrap();
        let account = crate::db::user::create(
            &db,
            crate::domain::user::Username::try_new("ogrenci").unwrap(),
            hash,
        )
        .await
        .unwrap();
        let student = account.get_id().clone();
        let first = a_class("9-A", &db).await;
        let second = a_class("club", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        class_course::attach(&db, &first, &algebra, &manager)
            .await
            .unwrap();
        add(&db, &first, &student, &manager).await.unwrap();
        add(&db, &second, &student, &manager).await.unwrap();

        crate::service::user::set_role(&db, account.get_id(), crate::domain::role::Role::Teacher)
            .await
            .unwrap();
        assert_eq!(rows("SELECT VALUE id FROM class_member", &db).await, 0);
        for class in [&first, &second] {
            assert_eq!(
                counter("class_member_count", class.record(), &db).await,
                0,
                "every class must get its member count back"
            );
        }
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            None,
            "the enrollment goes in the same transaction as the membership"
        );
        assert_eq!(
            counter("enrollment_count", algebra.record(), &db).await,
            0,
            "…and its seat comes back once, not twice"
        );
        // Which is the point of folding them: the class the student was in is
        // free of them entirely — no membership, no count, and no enrollment
        // row left tagged with a class that is about to be deletable.
        assert!(
            crate::db::class_group::delete(
                &db,
                crate::db::class_group::read(&db, &second)
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .await
            .unwrap(),
            "a class holding neither members nor courses must delete"
        );
    }
}
