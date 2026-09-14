//! A grade's course template: the courses every class section (şube) at one
//! grade carries.
//!
//! A school runs many şube at grade "9" and stocks each of them by hand, one
//! attach at a time. A blueprint is that list said once. It is a layer *above*
//! [`crate::db::class_pump`], never a replacement for it: applying a
//! blueprint calls the same [`crate::service::class_course::attach`] a manager's own call does,
//! so the rows it lands are ordinary `class_course` links and ordinary
//! `enrollment` rows, and an elective (seçmeli) placed by hand next to them is
//! still an individual enrollment nothing here can see.
//!
//! This module is the pure shape: the id (the grade label *is* the primary
//! key, so "one blueprint per grade" holds by construction), the row, the
//! pump's report types ([`Pumped`], [`Skip`], [`SectionStatus`]) and the
//! shared skip vocabulary ([`skip_reason`]).
//!
//! The behavior lives one layer down: the edit-retro-pump, the apply/sweep
//! engine in [`crate::service::class_blueprint`], the queries in
//! [`crate::db::class_blueprint`].

use crate::constant::MAX_CLASS_COURSES;
use crate::db::class_pump::{Attached, Axis};
use crate::domain::class_course::ClassCourse;
use crate::domain::class_group::{ClassGrade, ClassGroupId};
use crate::domain::course::CourseId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// A grade label as the table's `TEXT` primary key — one blueprint per grade
/// by construction, like the settings singleton's `'school'` row: the key is
/// a name, not a minted entity id.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ClassBlueprintId(String);

impl ClassBlueprintId {
    /// The key one grade label always maps to.
    pub fn for_grade(grade: &ClassGrade) -> Self {
        Self(grade.as_str().to_string())
    }

    pub fn from_key(key: &str) -> Self {
        Self(key.to_string())
    }

    pub fn key(&self) -> &str {
        &self.0
    }
}

/// One grade's template. The id is the grade label itself — the key one
/// grade always maps to — stored here beside the `grade` field so a read
/// never has to parse a record id back into a domain value. `courses` is
/// not a column of the row: it is the `blueprint_course` junction, joined
/// on by the db layer (sorted by course, which is the order every CAS
/// comparison relies on).
#[derive(Debug, Clone)]
pub struct ClassBlueprint {
    pub(crate) id: ClassBlueprintId,
    pub(crate) grade: ClassGrade,
    pub(crate) courses: Vec<CourseId>,
    pub(crate) creator: UserId,
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

/// What a pump did: how many sections it reached, and the pairs it refused.
///
/// `matched` exists because an empty `skipped` is not success on its own. A
/// grade label is free text ([`ClassGrade`]) and
/// [`crate::db::class_group::list_for_grade`]
/// matches it exactly, so a blueprint keyed `"9 "` reaches none of the sections
/// keyed `"9"` — and with nothing to skip it answers exactly like a pump that
/// stocked every one of them. The count is the only thing that tells those
/// apart.
///
/// A struct rather than a pair with
/// [`crate::service::class_blueprint::set_courses`]'s: three
/// unlabelled fields off one call is a shape a caller has to remember.
#[derive(Debug)]
pub struct Pumped {
    /// The sections this pump **reached** — not the ones the grade holds. The
    /// two differ when the run ended early (`blueprint_deleted`), and what
    /// happened is the actionable one.
    pub matched: i64,
    pub skipped: Vec<Skip>,
}

/// One section's distance from its grade's template: the courses it does not
/// carry. Named as well as identified, like [`Skip`], and for the same reason.
#[derive(Debug, Clone)]
pub struct SectionStatus {
    pub class: ClassGroupId,
    pub class_name: String,
    pub missing: Vec<CourseId>,
}

/// Why this attach did not land, or `None` when it did — the shared vocabulary
/// of [`Attached::refusal_code`], which is where the codes and the set they
/// close over are documented.
///
/// One divergence, and it lives here because only a pump has it: a duplicate is
/// not a skip. The course is already on the class, which is exactly what the
/// blueprint asks for, so re-running a pump must report nothing — while a *hand*
/// attach's duplicate is a genuine refusal (`duplicate`), because that call
/// asked for the row and did not get it.
///
/// Always the course axis: a pump attaches courses to sections, so the two
/// ceiling codes are read on that side ([`Attached::refusal_code`] takes the
/// axis because they differ per axis).
pub(crate) fn skip_reason(landed: &Attached<ClassCourse>) -> Option<&'static str> {
    match landed {
        Attached::Duplicate => None,
        other => other.refusal_code(&Axis::Course),
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
    /// the primary key and there is no blueprint for "no grade"; and free of
    /// the characters that would make that key unaddressable as a URL path
    /// segment, the second gate [`crate::domain::menu::MenuSlot`] carries for
    /// the same reason. Grades were never validated for this, so a *class* may
    /// already carry a label refused here — it simply cannot have a blueprint
    /// until it is renamed, which is a 400 the caller can read rather than a
    /// route nobody can reach.
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
    pub(crate) fn course_list(courses: Vec<CourseId>) -> Result<Vec<CourseId>, AppError> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let algebra = CourseId::from_key("0198f1a2-3b4c-7d5e-8f90-1a2b3c4d5e6f");
        let held = ClassBlueprint::course_list(vec![algebra.clone(), algebra.clone()]).unwrap();
        assert_eq!(held, vec![algebra]);

        // One over the ceiling is refused; each key must be a distinct
        // *parseable* UUID, or the parser would collapse them all to nil.
        let too_many: Vec<CourseId> = (0..=MAX_CLASS_COURSES)
            .map(|n| CourseId::from_key(&format!("0198f1a2-3b4c-7d5e-8f90-{n:012x}")))
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
    ///
    /// Each one is also checked against [`Attached::refusal_code`], the manual
    /// routes' half of the same vocabulary: `duplicate` is the *only* place the
    /// two are allowed to differ, so a second spelling of any other code cannot
    /// drift in on either side.
    #[test]
    fn every_refusal_the_pump_can_answer_is_a_skip() {
        // `Made` is the other `None` there, and building one needs a live link
        // row — the arm itself is the only thing to check for it.
        assert!(skip_reason(&Attached::Duplicate).is_none());
        assert_eq!(
            Attached::<ClassCourse>::Duplicate.refusal_code(&Axis::Course),
            Some("duplicate"),
            "a hand attach's duplicate is a refusal even though a pump's is not"
        );
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
            (Attached::SourceGone, "blueprint_deleted"),
        ] {
            assert_eq!(
                skip_reason(&refusal),
                Some(code),
                "{refusal:?} must be reported as its own code, not swallowed or reworded"
            );
            assert_eq!(
                refusal.refusal_code(&Axis::Course),
                skip_reason(&refusal),
                "{refusal:?} must read the same on a manual 409 as in a pump's skip list"
            );
        }
    }

    /// The two ceiling refusals mean the *opposite* ceiling on the two axes —
    /// `ClassFull` is the axis being attached, `ClassOverloaded` the other one —
    /// so the member add's pair must be the member add's own, and a code that
    /// reads the same on both axes is the bug this pins: the roster being full
    /// was published as `class_at_course_ceiling`.
    #[test]
    fn each_axis_names_the_ceiling_it_actually_hit() {
        for (refusal, course_axis, member_axis) in [
            (
                Attached::<ClassCourse>::ClassFull,
                "class_at_course_ceiling",
                "class_at_roster_ceiling",
            ),
            (
                Attached::ClassOverloaded,
                "class_roster_too_large",
                "class_course_list_too_large",
            ),
        ] {
            assert_eq!(refusal.refusal_code(&Axis::Course), Some(course_axis));
            assert_eq!(refusal.refusal_code(&Axis::Member), Some(member_axis));
        }
        // Every other code is axis-free, and must stay that way: they name a
        // record, not a ceiling.
        for refusal in [
            Attached::<ClassCourse>::Duplicate,
            Attached::Gone,
            Attached::PivotGone,
            Attached::Full("course:algebra".into()),
            Attached::CourseGone("course:algebra".into()),
            Attached::SourceGone,
        ] {
            assert_eq!(
                refusal.refusal_code(&Axis::Course),
                refusal.refusal_code(&Axis::Member),
                "{refusal:?} names a record, so it must read the same on both axes"
            );
        }
    }
}
