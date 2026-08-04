//! A course attached to a class, and the enrollments that attachment implies.
//!
//! The mirror of [`crate::domain::class_member`] along the pump's other axis:
//! attaching a course enrolls the class's whole roster into it, detaching it
//! sweeps the rows that attach wrote.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{CLASS_COURSE_TABLE, MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use crate::database::Database;
use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_group::ClassGroupId;
use crate::domain::class_pump::{Attached, Axis, attach, detach, link_id};
use crate::domain::course::CourseId;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassCourseId(RecordId);

impl ClassCourseId {
    /// The record one (class, course) pair always maps to.
    pub fn composite(class: &ClassGroupId, course: &CourseId) -> Self {
        Self(link_id(CLASS_COURSE_TABLE, class, course.key()))
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

/// One course a class is attached to. `attached_by` is who attached it.
#[derive(Debug, Clone, SurrealValue)]
pub struct ClassCourse {
    id: ClassCourseId,
    class: ClassGroupId,
    course: CourseId,
    attached_by: UserId,
    /// The grade blueprint that placed this attachment, absent when a human
    /// attached the course to this class directly — the mirror of
    /// [`crate::domain::enrollment`]'s `source`, and read the same way: only a
    /// row carrying the key is a blueprint's to take back, so a hand-attached
    /// course survives every blueprint sweep. Rows written before the column
    /// carry no key at all, which is exactly "hand-attached".
    source: Option<ClassBlueprintId>,
    /// When it was attached, and the *only* thing "newest first" can mean here:
    /// the row's id is the (class, course) pair, so ordering by it sorts the
    /// list by the course's own ULID. Optional because rows written before this
    /// column carry no stamp — see the migration note.
    attached_at: Option<Timestamp>,
}

impl ClassCourse {
    pub fn get_id(&self) -> &ClassCourseId {
        &self.id
    }

    pub fn get_class(&self) -> &ClassGroupId {
        &self.class
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_attached_by(&self) -> &UserId {
        &self.attached_by
    }

    /// The blueprint that placed this attachment, or `None` for a hand attach.
    pub fn get_source(&self) -> Option<&ClassBlueprintId> {
        self.source.as_ref()
    }

    /// Attach `course` to `class` and enroll the class's whole roster into it,
    /// in one transaction.
    ///
    /// Students already in the course keep the rows they have — no seat
    /// charged, `source` untouched — and a roster that does not fit refuses the
    /// whole attach rather than filling the course to its cap and stopping.
    pub async fn attach(
        class: &ClassGroupId,
        course: &CourseId,
        attached_by: &UserId,
        db: &Database,
    ) -> Result<ClassCourse, AppError> {
        let landed = Self::attach_sourced(class, course, attached_by, None, db).await?;
        // Read off the refusal, never respelled here: this route and a
        // blueprint pump answer one vocabulary. `Made` is the only `None`, and
        // it takes the `Ok` arm below.
        let code = landed.refusal_code().unwrap_or_default();
        match landed {
            Attached::Made(saved) => Ok(saved),
            Attached::Duplicate => Err(AppError::ConflictCoded {
                code,
                message: "the course is already on this class".into(),
            }),
            // The class or the course: either end of the link being gone is a
            // 404 on this route, and only a blueprint's skip list needs them
            // told apart.
            Attached::Gone | Attached::PivotGone => Err(AppError::NotFound),
            Attached::ClassFull => Err(AppError::ConflictCoded {
                code,
                message: format!("this class already holds {MAX_CLASS_COURSES} courses"),
            }),
            // The other axis: attaching one course enrolls the whole roster, so
            // a class over *that* ceiling cannot take a course however few it
            // carries. Only a class predating the ceiling can be here.
            Attached::ClassOverloaded => Err(AppError::ConflictCoded {
                code,
                message: format!(
                    "this class holds more than {MAX_CLASS_MEMBERS} students — \
                     remove some before attaching a course"
                ),
            }),
            Attached::Full(full) => Err(AppError::ConflictCoded {
                code,
                message: format!("{full} cannot hold the whole class"),
            }),
            // Another of the class's links points at a deleted course. This
            // axis claims its own pivot, so it is never *this* course.
            Attached::CourseGone(course) => Err(AppError::ConflictCoded {
                code,
                message: format!("{course} no longer exists — detach it from this class first"),
            }),
            // This path passes no source, so the claim that answers this is
            // never in the transaction it ran.
            Attached::SourceGone => Err(AppError::Internal(
                "a hand attach has no blueprint to lose".into(),
            )),
        }
    }

    /// The attach itself, with the refusals left *unmapped*.
    ///
    /// A hand attach ([`ClassCourse::attach`]) turns each of them into the
    /// error the route answers with, because one call is one course and a
    /// refusal is that call's whole answer. A blueprint pump cannot: it runs
    /// one of these per (class, course), and a course that does not fit one
    /// section must not abort the other eleven — so it needs to *read* the
    /// refusal and carry on ([`crate::domain::class_blueprint`]).
    ///
    /// `source` is the provenance tag, and it is written by the same statement
    /// that writes the link, so no attachment can exist without the answer to
    /// "may a blueprint take this back". It is also *claimed* in that
    /// transaction ([`Attached::SourceGone`]): a blueprint deleted while this
    /// pump ran has already swept by that tag, so a row landing afterwards
    /// would carry a name nothing can reach.
    pub(crate) async fn attach_sourced(
        class: &ClassGroupId,
        course: &CourseId,
        attached_by: &UserId,
        source: Option<&ClassBlueprintId>,
        db: &Database,
    ) -> Result<Attached<ClassCourse>, AppError> {
        let link = ClassCourse {
            id: ClassCourseId::composite(class, course),
            class: class.clone(),
            course: course.clone(),
            attached_by: attached_by.clone(),
            source: source.cloned(),
            attached_at: Some(Timestamp::now()),
        };
        attach(
            class,
            Axis::Course,
            &link.id.record(),
            &link,
            course.record(),
            attached_by.record(),
            source.map(ClassBlueprintId::record),
            db,
        )
        .await
    }

    /// Detach `course` from `class` and sweep the enrollments the class pumped
    /// into it. A course that was not attached is a [`AppError::NotFound`],
    /// raised here rather than left to each caller to re-derive from a boolean.
    ///
    /// A student another attached class still claims keeps their row, re-tagged
    /// to that class (see [`crate::domain::class_pump::detach`]).
    pub async fn detach(
        class: &ClassGroupId,
        course: &CourseId,
        db: &Database,
    ) -> Result<(), AppError> {
        let gone = detach(
            "$link",
            Axis::Course,
            &[(
                "link".into(),
                ClassCourseId::composite(class, course)
                    .record()
                    .into_value(),
            )],
            db,
        )
        .await?;
        (gone > 0).then_some(()).ok_or(AppError::NotFound)
    }

    /// The courses a class is attached to, newest first — by when they were
    /// attached, not by the course's own id, which is what the composite record
    /// id sorts on. A row older than the column carries no stamp at all, and
    /// NONE sorts last under DESC — the honest place for a row of unknown age.
    pub async fn list_for_class(
        class: &ClassGroupId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ClassCourse>, i64), AppError> {
        PagedList::new(
            format!("{CLASS_COURSE_TABLE} WHERE class = $class"),
            "ORDER BY attached_at DESC, id DESC",
        )
        .bind("class", class.record())
        .run(limit, offset, db)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::class_member::ClassMember;
    use crate::domain::class_member::tests::{a_class, a_course, counter, rows, source_of};
    use crate::domain::enrollment::Enrollment;

    /// Attaching seeds the course from the roster the class already holds.
    #[tokio::test]
    async fn an_attach_enrolls_the_whole_roster() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        let students = [UserId::from_key("a"), UserId::from_key("b")];
        for student in &students {
            ClassMember::add(&class, student, &manager, &db)
                .await
                .unwrap();
        }

        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();
        for student in &students {
            assert_eq!(
                source_of(&algebra, student, &db).await,
                Some(Some(class.clone()))
            );
        }
        assert_eq!(counter("enrollment_count", algebra.record(), &db).await, 2);
        assert_eq!(counter("class_course_count", class.record(), &db).await, 1);

        let again = ClassCourse::attach(&class, &algebra, &manager, &db).await;
        assert!(
            matches!(again, Err(AppError::ConflictCoded { code, .. }) if code == "duplicate"),
            "a second attach is a 409 coded `duplicate`: {again:?}"
        );
    }

    /// A course with room for only part of the roster takes none of it: zero
    /// enrollments, the counter unmoved, and no attachment row.
    #[tokio::test]
    async fn a_roster_that_does_not_fit_attaches_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let class = a_class("9-A", &db).await;
        let tight = a_course("algebra", Some(2), &db).await;
        for student in ["a", "b", "c"] {
            ClassMember::add(&class, &UserId::from_key(student), &manager, &db)
                .await
                .unwrap();
        }

        let refused = ClassCourse::attach(&class, &tight, &manager, &db).await;
        assert!(
            matches!(refused, Err(AppError::ConflictCoded { code, ref message })
                if code == "course_full" && message.contains(tight.key())),
            "the refusal must be coded `course_full` and name the course: {refused:?}"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM enrollment", &db).await,
            0,
            "the two seats that did fit must be given back with the third"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM class_course", &db).await,
            0,
            "…and no attachment row may survive"
        );
        assert_eq!(counter("enrollment_count", tight.record(), &db).await, 0);
        assert_eq!(counter("class_course_count", class.record(), &db).await, 0);
    }

    /// A hand-placed row is skipped on the way in and left standing on the way
    /// out — the class never owned it.
    #[tokio::test]
    async fn a_hand_placed_row_survives_the_detach() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        Enrollment::enroll(&algebra, &student, &manager, &db)
            .await
            .unwrap();
        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();

        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(None),
            "an attach may not adopt a hand-placed row"
        );
        assert_eq!(
            counter("enrollment_count", algebra.record(), &db).await,
            1,
            "…nor charge a seat for it"
        );

        ClassCourse::detach(&class, &algebra, &db).await.unwrap();
        assert_eq!(source_of(&algebra, &student, &db).await, Some(None));
        assert_eq!(counter("enrollment_count", algebra.record(), &db).await, 1);
        let again = ClassCourse::detach(&class, &algebra, &db).await;
        assert!(
            matches!(again, Err(AppError::NotFound)),
            "a second detach is a 404: {again:?}"
        );
    }

    /// Detaching with nobody else claiming the rows deletes them and gives the
    /// seats back.
    #[tokio::test]
    async fn a_detach_sweeps_the_rows_it_pumped() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", None, &db).await;
        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();
        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();

        ClassCourse::detach(&class, &algebra, &db).await.unwrap();
        assert_eq!(source_of(&algebra, &student, &db).await, None);
        assert_eq!(counter("enrollment_count", algebra.record(), &db).await, 0);
        assert_eq!(counter("class_course_count", class.record(), &db).await, 0);
        assert_eq!(
            rows("SELECT VALUE id FROM class_member", &db).await,
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
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let algebra = a_course("algebra", None, &db).await;
        let first = a_class("9-A", &db).await;
        let second = a_class("club", &db).await;
        for class in [&first, &second] {
            ClassMember::add(class, &student, &manager, &db)
                .await
                .unwrap();
            ClassCourse::attach(class, &algebra, &manager, &db)
                .await
                .unwrap();
        }
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(Some(first.clone())),
            "the second attach must skip the row the first wrote"
        );
        assert_eq!(
            counter("enrollment_count", algebra.record(), &db).await,
            1,
            "…and pay no second seat for it"
        );

        ClassCourse::detach(&first, &algebra, &db).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(Some(second.clone())),
            "the row must be handed to the class that still claims it"
        );
        assert_eq!(
            counter("enrollment_count", algebra.record(), &db).await,
            1,
            "a repair is not a release"
        );

        ClassCourse::detach(&second, &algebra, &db).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            None,
            "the last claimant leaving takes the row with it"
        );
        assert_eq!(counter("enrollment_count", algebra.record(), &db).await, 0);
    }

    /// The same repair along the member axis: removing the student from the
    /// class that owns the row hands it to the other class they are in.
    #[tokio::test]
    async fn a_member_removal_hands_a_shared_row_over_too() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("manager");
        let student = UserId::from_key("student");
        let algebra = a_course("algebra", None, &db).await;
        let first = a_class("9-A", &db).await;
        let second = a_class("club", &db).await;
        for class in [&first, &second] {
            ClassCourse::attach(class, &algebra, &manager, &db)
                .await
                .unwrap();
            ClassMember::add(class, &student, &manager, &db)
                .await
                .unwrap();
        }

        ClassMember::remove(&first, &student, &db).await.unwrap();
        assert_eq!(
            source_of(&algebra, &student, &db).await,
            Some(Some(second.clone())),
            "the student is still in the second class, so the row stays"
        );
        assert_eq!(counter("enrollment_count", algebra.record(), &db).await, 1);

        ClassMember::remove(&second, &student, &db).await.unwrap();
        assert_eq!(source_of(&algebra, &student, &db).await, None);
        assert_eq!(counter("enrollment_count", algebra.record(), &db).await, 0);
    }
}
