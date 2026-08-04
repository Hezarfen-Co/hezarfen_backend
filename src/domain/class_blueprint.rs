//! A grade's course template: the courses every class section (şube) at one
//! grade carries.
//!
//! A school runs many şube at grade "9" and stocks each of them by hand, one
//! attach at a time. A blueprint is that list said once. It is a layer *above*
//! [`crate::domain::class_pump`], never a replacement for it: applying a
//! blueprint calls the same [`ClassCourse::attach`] a manager's own call does,
//! so the rows it lands are ordinary `class_course` links and ordinary
//! `enrollment` rows, and an elective (seçmeli) placed by hand next to them is
//! still an individual enrollment nothing here can see.
//!
//! Two rules shape everything below.
//!
//! **An edit retro-pumps.** Changing the list reaches every class already at
//! that grade, not just the ones made afterwards. That is an unbounded write
//! loop — one transaction per (class, course), and no ceiling bounds the number
//! of classes — chosen deliberately over a template that only new classes see.
//!
//! **A pump is best-effort.** Each (class, course) pair is one all-or-nothing
//! transaction of the existing pump, and a pair that would breach a limit is
//! *skipped and reported* rather than aborting the other classes' share. So a
//! blueprint edit can leave a partial state — which is the point: one full
//! course must not stop the other eleven sections from being stocked. Every
//! skip is returned, naming the class, the course and the reason.
//!
//! Removal is the mirror, and it is where the provenance tag earns its keep: a
//! `class_course` row this blueprint wrote carries `source`, a row a human
//! attached carries no such key at all, and dropping a course from the
//! blueprint sweeps only the former. The sweep is the pump's own
//! [`crate::domain::class_pump::detach`], so a class losing a course still
//! repairs before it deletes — and it runs one transaction per pair too, for
//! the same reason the pump does.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{CLASS_BLUEPRINT_TABLE, CLASS_COURSE_TABLE, MAX_CLASS_COURSES};
use crate::database::Database;
use crate::domain::class_course::ClassCourse;
use crate::domain::class_group::{ClassGrade, ClassGroup, ClassGroupId};
use crate::domain::class_pump::{Attached, Axis, detach};
use crate::domain::course::CourseId;
use crate::domain::page::PagedList;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassBlueprintId(RecordId);

impl ClassBlueprintId {
    /// The record one grade label always maps to. The label is the key, so
    /// "one blueprint per grade" holds by construction rather than by a
    /// find-then-insert that two managers can race — the shape
    /// [`crate::domain::menu::MenuId::for_slot`] uses.
    pub fn for_grade(grade: &ClassGrade) -> Self {
        Self(RecordId::new(CLASS_BLUEPRINT_TABLE, grade.as_str()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(CLASS_BLUEPRINT_TABLE, key))
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

/// One grade's template. `grade` is stored beside the key it *is*, so a read
/// never has to parse a record id back into a domain value.
#[derive(Debug, Clone, SurrealValue)]
pub struct ClassBlueprint {
    id: ClassBlueprintId,
    grade: ClassGrade,
    courses: Vec<CourseId>,
    creator: UserId,
}

/// One (class, course) pair a pump refused, and why. Precise enough to act on:
/// the class is named as well as identified, because a manager reading "12
/// skipped" needs to know *which* section is short a course.
#[derive(Debug, Clone)]
pub struct Skip {
    pub class: ClassGroupId,
    pub class_name: String,
    pub course: CourseId,
    pub reason: &'static str,
}

/// Why this attach did not land, or `None` when it did. A duplicate is not a
/// skip: the course is already on the class, which is exactly what the
/// blueprint asks for, and re-running a pump must therefore report nothing.
///
/// A machine code, not a sentence: the client owns the wording and this API
/// only says *which* refusal it was, the shape every other enum here has
/// (roles, course kinds, the badge catalog). The set is closed and documented
/// on `SkipResponse.reason`.
///
/// Each code names the record that actually failed. The pump's two "gone"
/// answers are a *class* delete ([`Attached::Gone`] → `class_deleted`, the
/// class counter's claim matching nothing on a row a re-read no longer finds)
/// and a *course* delete ([`Attached::PivotGone`] → `course_deleted`, the
/// course's own claim matching nothing) — and telling a manager the class
/// vanished when the course did sends them to look at a section that is
/// standing right there. `linked_course_missing` is a third: *another* course
/// already attached to this class no longer exists, and it must be detached
/// before this attach can be retried.
fn skip_reason(landed: &Attached<ClassCourse>) -> Option<&'static str> {
    match landed {
        Attached::Made(_) | Attached::Duplicate => None,
        Attached::Gone => Some("class_deleted"),
        Attached::PivotGone => Some("course_deleted"),
        Attached::ClassFull => Some("class_at_course_ceiling"),
        Attached::ClassOverloaded => Some("class_roster_too_large"),
        Attached::Full(_) => Some("course_full"),
        Attached::CourseGone(_) => Some("linked_course_missing"),
    }
}

impl ClassBlueprint {
    pub fn get_id(&self) -> &ClassBlueprintId {
        &self.id
    }

    pub fn get_grade(&self) -> &ClassGrade {
        &self.grade
    }

    pub fn get_courses(&self) -> &[CourseId] {
        &self.courses
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    /// The grade a blueprint may be keyed on. Non-empty, because the label is
    /// the record id and there is no blueprint for "no grade"; and free of the
    /// characters that would make that id unaddressable as a URL path segment,
    /// the second gate [`crate::domain::menu::MenuSlot`] carries for the same
    /// reason. Grades were never validated for this, so a *class* may already
    /// carry a label refused here — it simply cannot have a blueprint until it
    /// is renamed, which is a 400 the caller can read rather than a route
    /// nobody can reach.
    pub fn grade_key(value: &str) -> Result<ClassGrade, AppError> {
        if value.is_empty() {
            return Err(AppError::Validation(ValidationError::Empty("grade")));
        }
        if value
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%'))
        {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "grade",
                reason: "must not contain / \\ ? # or %",
            }));
        }
        Ok(ClassGrade::try_new(value)?)
    }

    /// The course list a blueprint may hold: deduplicated, and no longer than
    /// one class may carry — a blueprint above that ceiling would guarantee a
    /// skip on every class it ever reached.
    fn course_list(courses: Vec<CourseId>) -> Result<Vec<CourseId>, AppError> {
        let mut list: Vec<CourseId> = Vec::new();
        for course in courses {
            if !list.contains(&course) {
                list.push(course);
            }
        }
        if list.len() as i64 > MAX_CLASS_COURSES {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "course_ids",
                reason: "a blueprint cannot hold more courses than a class may carry \
                         (max_class_courses)",
            }));
        }
        Ok(list)
    }

    /// Write the blueprint. A second one for the same grade is a 409 the store
    /// itself decides — the grade is the record key, so the duplicate is seen
    /// rather than raced.
    pub async fn create(
        creator: &UserId,
        grade: ClassGrade,
        courses: Vec<CourseId>,
        db: &Database,
    ) -> Result<ClassBlueprint, AppError> {
        let blueprint = ClassBlueprint {
            id: ClassBlueprintId::for_grade(&grade),
            grade,
            courses: Self::course_list(courses)?,
            creator: creator.clone(),
        };
        match db
            .create::<Option<ClassBlueprint>>(blueprint.id.record())
            .content(blueprint)
            .await
        {
            Ok(Some(created)) => Ok(created),
            Ok(None) => Err(AppError::Internal("failed to create blueprint".into())),
            Err(error) if error.is_already_exists() => Err(AppError::Conflict(
                "a blueprint already exists for that grade",
            )),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn read(
        id: &ClassBlueprintId,
        db: &Database,
    ) -> Result<Option<ClassBlueprint>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every blueprint, by grade label — the id *is* the label, so this is the
    /// only ordering that means anything here.
    pub async fn list_all(
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ClassBlueprint>, i64), AppError> {
        PagedList::new(CLASS_BLUEPRINT_TABLE, "ORDER BY id ASC")
            .run(limit, offset, db)
            .await
    }

    /// Replace the course list, then reconcile every class at this grade with
    /// it: the courses this blueprint no longer holds are detached from the
    /// classes *it* attached them to, and the ones it holds are pumped into
    /// every class that fits.
    ///
    /// The write is a compare-and-set on the list this caller read, so two
    /// managers editing the same grade cannot have one's list silently pump the
    /// other's diff — the loser is a 409 and re-reads.
    ///
    /// Removals run first: a course leaving frees a place under the per-class
    /// course ceiling that the same edit's additions can then use.
    pub async fn set_courses(
        self,
        courses: Vec<CourseId>,
        by: &UserId,
        db: &Database,
    ) -> Result<(ClassBlueprint, Vec<Skip>), AppError> {
        let wanted = Self::course_list(courses)?;
        let dropped: Vec<CourseId> = self
            .courses
            .iter()
            .filter(|course| !wanted.contains(course))
            .cloned()
            .collect();
        let mut result = db
            .query("UPDATE $id SET courses = $wanted WHERE courses = $held RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("wanted", wanted))
            .bind(("held", self.courses.clone()))
            .await?
            .check()?;
        let Some(saved) = result.take::<Vec<ClassBlueprint>>(0)?.into_iter().next() else {
            // The conditional write matched nothing: the row is gone, or its
            // list moved since this caller read it. Only this path pays for the
            // read that tells those apart.
            return match Self::read(&self.id, db).await? {
                Some(_) => Err(AppError::Conflict(
                    "this blueprint changed since you read it — re-read and retry",
                )),
                None => Err(AppError::NotFound),
            };
        };
        saved.drop_courses(&dropped, db).await?;
        let skipped = saved.pump(by, db).await?;
        Ok((saved, skipped))
    }

    /// Delete the blueprint, taking every attachment it made with it. Courses a
    /// human attached to those classes by hand carry no `source` and are left
    /// exactly where they are — the same cut a single removal makes, applied to
    /// the whole list, because a template that is gone owns nothing.
    pub async fn delete(self, db: &Database) -> Result<(), AppError> {
        self.drop_courses(&self.courses.clone(), db).await?;
        let gone: Option<ClassBlueprint> = db.delete(self.id.record()).await?;
        gone.map(|_| ()).ok_or(AppError::NotFound)
    }

    /// Attach every course in this blueprint to `class`, skipping — never
    /// aborting on — the ones that do not fit. A course already on the class is
    /// a no-op, whoever attached it: this is what makes a pump repeatable, and
    /// what stops it re-tagging a hand-attached course as its own.
    pub async fn apply_to(
        &self,
        class: &ClassGroup,
        by: &UserId,
        db: &Database,
    ) -> Result<Vec<Skip>, AppError> {
        let mut skipped = Vec::new();
        for course in &self.courses {
            let landed =
                ClassCourse::attach_sourced(class.get_id(), course, by, Some(&self.id), db).await?;
            if matches!(landed, Attached::PivotGone) {
                self.prune(course, db).await?;
            }
            if let Some(reason) = skip_reason(&landed) {
                skipped.push(Skip {
                    class: class.get_id().clone(),
                    class_name: class.get_name().as_str().to_string(),
                    course: course.clone(),
                    reason,
                });
            }
        }
        Ok(skipped)
    }

    /// [`Self::apply_to`] every class at this blueprint's grade. The write loop
    /// is one transaction per (class, course) and bounded by neither — see the
    /// module note.
    pub async fn pump(&self, by: &UserId, db: &Database) -> Result<Vec<Skip>, AppError> {
        let mut skipped = Vec::new();
        for class in ClassGroup::list_for_grade(&self.grade, db).await? {
            skipped.extend(self.apply_to(&class, by, db).await?);
        }
        Ok(skipped)
    }

    /// Drop a course that no longer exists out of this blueprint's list.
    ///
    /// [`crate::domain::course::Course::delete`] takes the `class_course` links
    /// a course had, but nothing it can reach names the blueprints holding its
    /// id — so a deleted course stays in the list and every future pump refuses
    /// it again, on every class, forever. A skip a manager cannot act on is
    /// noise, so the pump that *finds* the dangling id also removes it, and the
    /// removal is reported once (the skip) rather than every time.
    ///
    /// Not a compare-and-set, unlike [`Self::set_courses`]: "a course that does
    /// not exist is not in this list" holds for every version of the list, so
    /// there is nothing a concurrent edit could make this write wrong about,
    /// and re-running it changes nothing. No sweep follows it either — the
    /// attachments it would sweep are exactly the ones the course's own delete
    /// already took.
    async fn prune(&self, course: &CourseId, db: &Database) -> Result<(), AppError> {
        db.query("UPDATE $id SET courses = array::complement(courses ?? [], [$course])")
            .bind(("id", self.id.record()))
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(())
    }

    /// Detach `courses` from every class *this blueprint* attached them to, and
    /// sweep the enrollments those attachments pumped.
    ///
    /// `source` is the whole filter, so a row without the key — a hand attach —
    /// is never matched, and a class that acquired the same course by hand
    /// keeps it. The sweep underneath is the pump's own, so a student a second
    /// class still claims is re-tagged rather than unenrolled.
    ///
    /// One transaction per *link row*, which is the module note's "one
    /// transaction per (class, course)" and the mirror of the pump's
    /// best-effort rule. One statement per course would be shorter, but it puts
    /// every section at the grade into a single unbounded transaction: its write
    /// loop is one enrollment sweep per (class, member) with no ceiling over the
    /// class count, and a failure anywhere in it rolls back the whole grade —
    /// so one contended section keeps the other eleven attached to a course the
    /// template no longer holds. Per row, a failure leaves the pairs already
    /// detached detached, each with its counter released and its enrollments
    /// swept, and the rest exactly as they were: a partial removal, which is the
    /// same partial state a pump is allowed to leave.
    ///
    /// The rows are read first rather than derived from the classes at this
    /// grade: a class whose grade was edited after the pump still carries this
    /// blueprint's attachments, and only the `source` tag can find it.
    async fn drop_courses(&self, courses: &[CourseId], db: &Database) -> Result<(), AppError> {
        for course in courses {
            let mut found = db
                .query(format!(
                    "SELECT VALUE id FROM {CLASS_COURSE_TABLE} \
                     WHERE course = $course AND source = $blueprint"
                ))
                .bind(("course", course.record()))
                .bind(("blueprint", self.id.record()))
                .await?
                .check()?;
            for link in found.take::<Vec<RecordId>>(0)? {
                detach(
                    "$link",
                    Axis::Course,
                    &[("link".into(), link.into_value())],
                    db,
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::class_member::tests::{a_class, a_course};

    /// A blueprint holding `courses`, at grade "9".
    async fn a_blueprint(courses: Vec<CourseId>, db: &Database) -> ClassBlueprint {
        ClassBlueprint::create(
            &UserId::from_key("manager"),
            ClassBlueprint::grade_key("9").unwrap(),
            courses,
            db,
        )
        .await
        .unwrap()
    }

    /// Every skip names the record that actually failed.
    ///
    /// The three refusals a manager can meet come out of two different claims
    /// in one transaction, and they used to be reported by one string that
    /// guessed: a deleted *course* was announced as a deleted *class*, on a
    /// section standing right there. Each is provoked from the state that
    /// really produces it, not from the enum.
    #[tokio::test]
    async fn a_skip_names_what_actually_failed() {
        let manager = UserId::from_key("manager");

        // The course is deleted out from under the pump: the pivot claim
        // matches nothing.
        let db = crate::database::init_mem().await.unwrap();
        let course = a_course("algebra", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course.clone()], &db).await;
        db.query("DELETE $c")
            .bind(("c", course.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let skipped = blueprint.apply_to(&class, &manager, &db).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "course_deleted",
            "the course went, not the class"
        );
        // …and the dangling id is taken out of the list, so the skip is
        // reported once instead of on every pump forever.
        assert!(
            ClassBlueprint::read(blueprint.get_id(), &db)
                .await
                .unwrap()
                .unwrap()
                .get_courses()
                .is_empty(),
            "a course that no longer exists is pruned from the blueprint"
        );

        // The class is deleted out from under the pump: the counter claim
        // matches nothing and the read that follows finds no row.
        let db = crate::database::init_mem().await.unwrap();
        let course = a_course("algebra", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course.clone()], &db).await;
        db.query("DELETE $c")
            .bind(("c", class.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let skipped = blueprint.apply_to(&class, &manager, &db).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "class_deleted",
            "the class went, not the course"
        );
        assert_eq!(
            ClassBlueprint::read(blueprint.get_id(), &db)
                .await
                .unwrap()
                .unwrap()
                .get_courses(),
            &[course],
            "a live course is not pruned because a class vanished"
        );

        // The class stands at its own ceiling: the same claim matches nothing,
        // but the row is there — and that is the one a manager can act on.
        let db = crate::database::init_mem().await.unwrap();
        let course = a_course("algebra", None, &db).await;
        let class = ClassGroup::read(&a_class("9-A", &db).await, &db)
            .await
            .unwrap()
            .unwrap();
        let blueprint = a_blueprint(vec![course], &db).await;
        db.query(format!(
            "UPDATE $c SET {} = $cap",
            crate::constant::CLASS_COURSE_COUNT_FIELD
        ))
        .bind(("c", class.get_id().record()))
        .bind(("cap", MAX_CLASS_COURSES))
        .await
        .unwrap()
        .check()
        .unwrap();
        let skipped = blueprint.apply_to(&class, &manager, &db).await.unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(
            skipped[0].reason, "class_at_course_ceiling",
            "a full class is not a deleted one"
        );
    }

    #[test]
    fn a_grade_key_must_be_addressable() {
        assert!(ClassBlueprint::grade_key("").is_err());
        assert!(ClassBlueprint::grade_key("9/A").is_err());
        assert!(ClassBlueprint::grade_key("9%A").is_err());
        assert_eq!(
            ClassBlueprint::grade_key("9-A")
                .unwrap()
                .as_str()
                .to_string(),
            "9-A"
        );
        assert!(ClassBlueprint::grade_key(&"x".repeat(1000)).is_err());
    }

    /// The list is a *set*: a caller sending the same course twice must not
    /// make the pump attach it twice (the second is a duplicate anyway) nor
    /// spend two of the ceiling's places on one course.
    #[test]
    fn a_course_list_is_deduplicated_and_bounded() {
        let algebra = CourseId::from_key("algebra");
        let held = ClassBlueprint::course_list(vec![algebra.clone(), algebra.clone()]).unwrap();
        assert_eq!(held, vec![algebra]);

        let too_many: Vec<CourseId> = (0..=MAX_CLASS_COURSES)
            .map(|n| CourseId::from_key(&n.to_string()))
            .collect();
        assert!(ClassBlueprint::course_list(too_many).is_err());
    }

    /// A blueprint pumps best-effort, so every refusal the pump can answer must
    /// map to a reported skip rather than fall through as a success — and the
    /// two "nothing to do" answers must map to no skip at all.
    ///
    /// The codes are pinned exactly, not merely as "some": they are a published
    /// vocabulary a bilingual client branches and translates on, so a reworded
    /// one is a silent contract break. A new [`Attached`] variant fails the
    /// match arm above, and this pins what the existing ones say.
    #[test]
    fn every_refusal_the_pump_can_answer_is_a_skip() {
        // `Made` shares `Duplicate`'s arm, and building one needs a live link
        // row — the arm itself is the only thing to check there.
        assert!(skip_reason(&Attached::Duplicate).is_none());
        for (refusal, code) in [
            (Attached::Gone, "class_deleted"),
            (Attached::PivotGone, "course_deleted"),
            (Attached::ClassFull, "class_at_course_ceiling"),
            (Attached::ClassOverloaded, "class_roster_too_large"),
            (Attached::Full("course:algebra".into()), "course_full"),
            (
                Attached::CourseGone("course:algebra".into()),
                "linked_course_missing",
            ),
        ] {
            assert_eq!(
                skip_reason(&refusal),
                Some(code),
                "{refusal:?} must be reported as its own code, not swallowed or reworded"
            );
        }
    }
}
