//! A student's place in a class, and the enrollments that place implies.
//!
//! Both writes here are one call into [`crate::domain::class_pump`]: this file
//! only names the axis, the counter and the composite id, because adding a
//! member and attaching a course to the class are the same pump seen from its
//! two ends.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{CLASS_MEMBER_TABLE, MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::class_group::ClassGroupId;
use crate::domain::class_pump::{Attached, Axis, attach, detach, link_id};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassMemberId(RecordId);

impl ClassMemberId {
    /// The record one (class, user) pair always maps to.
    pub fn composite(class: &ClassGroupId, user: &UserId) -> Self {
        Self(link_id(CLASS_MEMBER_TABLE, class, user.key()))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// One student in one class. `added_by` is who put them there.
#[derive(Debug, Clone, SurrealValue)]
pub struct ClassMember {
    id: ClassMemberId,
    class: ClassGroupId,
    user: UserId,
    added_by: UserId,
    /// When they were added, and the *only* thing "newest first" can mean here:
    /// the row's id is the (class, user) pair, so ordering by it sorts the
    /// roster by the student's account ULID. Optional because rows written
    /// before this column carry no stamp — see the migration note.
    added_at: Option<Timestamp>,
}

impl ClassMember {
    pub fn get_id(&self) -> &ClassMemberId {
        &self.id
    }

    pub fn get_class(&self) -> &ClassGroupId {
        &self.class
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_added_by(&self) -> &UserId {
        &self.added_by
    }

    /// Put `user` in `class` and enroll them into every course the class is
    /// already attached to, in one transaction.
    ///
    /// A student already enrolled in one of those courses keeps the row they
    /// have — no seat is charged and their `source` is untouched, so a
    /// hand-placed enrollment is never adopted by the class — and a course with
    /// no free seat refuses the *whole* join rather than half of it.
    pub async fn add(
        class: &ClassGroupId,
        user: &UserId,
        added_by: &UserId,
        db: &Database,
    ) -> Result<ClassMember, AppError> {
        let member = ClassMember {
            id: ClassMemberId::composite(class, user),
            class: class.clone(),
            user: user.clone(),
            added_by: added_by.clone(),
            added_at: Some(Timestamp::now()),
        };
        let landed = attach(
            class,
            Axis::Member,
            (&member.id.record(), &member),
            user.record(),
            added_by.record(),
            None,
            db,
        )
        .await?;
        // Read off the refusal, never respelled here: this route and a
        // blueprint pump answer one vocabulary. `Made` is the only `None`, and
        // it takes the `Ok` arm below.
        let code = landed.refusal_code(&Axis::Member).unwrap_or_default();
        match landed {
            Attached::Made(saved) => Ok(saved),
            Attached::Duplicate => Err(AppError::ConflictCoded {
                code,
                message: "the student is already in this class".into(),
            }),
            Attached::Gone => Err(AppError::NotFound),
            // The pivot is the student: they were demoted while this ran, which
            // is the refusal the caller's own read makes a moment earlier and
            // the only one that can arrive after it.
            Attached::PivotGone => Err(AppError::Validation(ValidationError::Invalid {
                field: "user_id",
                reason: "only students can be added to a class",
            })),
            Attached::ClassFull => Err(AppError::ConflictCoded {
                code,
                message: format!("this class already holds {MAX_CLASS_MEMBERS} students"),
            }),
            // The other axis: adding one student enrolls them into every
            // attached course, so a class over *that* ceiling cannot take a
            // member however much room its roster has.
            Attached::ClassOverloaded => Err(AppError::ConflictCoded {
                code,
                message: format!(
                    "this class holds more than {MAX_CLASS_COURSES} courses — \
                     detach some before adding a student"
                ),
            }),
            Attached::Full(course) => Err(AppError::ConflictCoded {
                code,
                message: format!("{course} is full, so the class cannot take this student"),
            }),
            // Not a capacity problem, and told apart from one on purpose: no
            // number anyone can raise unblocks this class, only detaching the
            // link the deleted course left behind.
            Attached::CourseGone(course) => Err(AppError::ConflictCoded {
                code,
                message: format!("{course} no longer exists — detach it from this class first"),
            }),
            // Only an attach run on a blueprint's behalf states that claim, and
            // a membership is nobody's but its own.
            Attached::SourceGone => Err(AppError::Internal(
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
    /// class instead of deleted (see [`crate::domain::class_pump::detach`]).
    pub async fn remove(
        class: &ClassGroupId,
        user: &UserId,
        db: &Database,
    ) -> Result<(), AppError> {
        let gone = detach(
            "$link",
            Axis::Member,
            &[(
                "link".into(),
                ClassMemberId::composite(class, user).record().into_value(),
            )],
            db,
        )
        .await?;
        (gone > 0).then_some(()).ok_or(AppError::NotFound)
    }

    /// The class's roster, newest first — by when the student was added, not by
    /// their account id, which is what the composite record id sorts on.
    /// A row older than the column carries no stamp at all, and NONE sorts last
    /// under DESC — which is the honest place for a row of unknown age.
    pub async fn list_for_class(
        class: &ClassGroupId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ClassMember>, i64), AppError> {
        PagedList::new(
            format!("{CLASS_MEMBER_TABLE} WHERE class = $class"),
            "ORDER BY added_at DESC, id DESC",
        )
        .bind("class", class.record())
        .run(limit, offset, db)
        .await
    }

    /// The classes one student belongs to, newest membership first — the read
    /// behind "which class section (şube) am I in". Same ordering story as
    /// [`ClassMember::list_for_class`], along the other axis of the same index.
    pub async fn list_for_user(
        user: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ClassMember>, i64), AppError> {
        PagedList::new(
            format!("{CLASS_MEMBER_TABLE} WHERE user = $usr"),
            "ORDER BY added_at DESC, id DESC",
        )
        .bind("usr", user.record())
        .run(limit, offset, db)
        .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::class_course::ClassCourse;
    use crate::domain::class_group::{ClassGroup, ClassName};
    use crate::domain::course::{CourseDescription, CourseId, CourseKind, CourseTitle};

    pub(crate) async fn a_class(name: &str, db: &Database) -> ClassGroupId {
        ClassGroup::create(
            &UserId::from_key("manager"),
            ClassName::try_new(name).unwrap(),
            None,
            None,
            None,
            db,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    pub(crate) async fn a_course(title: &str, capacity: Option<i64>, db: &Database) -> CourseId {
        crate::db::course::create(
            db,
            &UserId::from_key("manager"),
            CourseTitle::try_new(title).unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            None,
            capacity,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    /// A counter, re-read out of the store — never off a return value, which
    /// the in-memory engine forges wins on (see [`crate::db::cap`]).
    pub(crate) async fn counter(field: &str, of: RecordId, db: &Database) -> i64 {
        let mut result = db
            .query(format!("SELECT VALUE {field} ?? 0 FROM $of"))
            .bind(("of", of))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<i64>>(0)
            .unwrap()
            .first()
            .copied()
            .unwrap()
    }

    /// How many rows `sql` selects ids for.
    pub(crate) async fn rows(sql: &str, db: &Database) -> usize {
        let mut result = db.query(sql).await.unwrap().check().unwrap();
        result.take::<Vec<RecordId>>(0).unwrap().len()
    }

    /// Whether `id` names a live row.
    pub(crate) async fn exists(id: RecordId, db: &Database) -> bool {
        let mut result = db
            .query("SELECT VALUE id FROM $id")
            .bind(("id", id))
            .await
            .unwrap()
            .check()
            .unwrap();
        !result.take::<Vec<RecordId>>(0).unwrap().is_empty()
    }

    /// The class that wrote an enrollment, or `None` for a hand-placed row.
    pub(crate) async fn source_of(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> Option<Option<ClassGroupId>> {
        crate::db::enrollment::read_for_user(db, course, user)
            .await
            .unwrap()
            .map(|row| row.get_source().cloned())
    }

    #[tokio::test]
    async fn a_member_is_enrolled_into_every_attached_course() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        let physics = a_course("physics", None, &db).await;
        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();
        ClassCourse::attach(&class, &physics, &manager, &db)
            .await
            .unwrap();

        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();

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
        use crate::domain::enrollment::EnrollmentId;

        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();
        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();

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
        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();
        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();

        let again = ClassMember::add(&class, &student, &manager, &db).await;
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
        ClassCourse::attach(&class, &roomy, &manager, &db)
            .await
            .unwrap();
        ClassCourse::attach(&class, &full, &manager, &db)
            .await
            .unwrap();

        // The whole record id, not just the key: a caller reading the 409 must
        // not have to guess which table the name came from.
        let named = format!("{}:{}", crate::constant::COURSE_TABLE, full.key());
        let refused = ClassMember::add(&class, &student, &manager, &db).await;
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
        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();

        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();
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

        ClassMember::remove(&class, &student, &db).await.unwrap();
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
        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();
        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();

        ClassMember::remove(&class, &student, &db).await.unwrap();
        assert_eq!(source_of(&algebra, &student, &db).await, None);
        assert_eq!(
            counter("enrollment_count", algebra.record(), &db).await,
            0,
            "the seat must come back with the row that held it"
        );
        assert_eq!(counter("class_member_count", class.record(), &db).await, 0);
        let again = ClassMember::remove(&class, &student, &db).await;
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
        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();
        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();

        // `Course::delete` refuses while the roster is occupied, so the wipe is
        // spelled the way the cascade does it, minus the guard.
        db.query("DELETE enrollment WHERE course = $course; DELETE class_course WHERE course = $course; DELETE $course;")
            .bind(("course", algebra.record()))
            .await
            .unwrap()
            .check()
            .unwrap();

        ClassMember::remove(&class, &student, &db)
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
        ClassCourse::attach(&first, &algebra, &manager, &db)
            .await
            .unwrap();
        ClassMember::add(&first, &student, &manager, &db)
            .await
            .unwrap();
        ClassMember::add(&second, &student, &manager, &db)
            .await
            .unwrap();

        crate::service::user::set_role(
            &db,
            account.get_id(),
            crate::domain::role::Role::Teacher,
        )
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
            crate::domain::class_group::ClassGroup::read(&second, &db)
                .await
                .unwrap()
                .unwrap()
                .delete(&db)
                .await
                .unwrap(),
            "a class holding neither members nor courses must delete"
        );
    }
}
