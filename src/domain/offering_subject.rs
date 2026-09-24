//! The syllabus topic sets: which of a catalog course's subjects an offering
//! ([`crate::domain::course_offering::CourseOffering`]) selects for its grade,
//! and which one a class section ([`crate::domain::class_course::ClassCourse`])
//! selects for itself. Both are pure membership junctions over `subject` —
//! there is no row type of their own, so this file carries the model's one
//! pure rule and the docs.
//!
//! The rule: a subject may be selected only for an offering of the course the
//! subject belongs to. "Lifted" from the course to the instance level, the
//! same gate guards the class-side selections, because an instance's course
//! is fixed at attach.
//!
//! Resolution is override-or-inherit, never merge, and is **flag-driven**:
//! [`ClassCourse::subjects_inherited`](crate::domain::class_course::ClassCourse::subjects_inherited)
//! `TRUE` = the offering's set applies, `FALSE` = the section's own
//! `class_course_subject` rows are authoritative **including when there are
//! none** (that is how a section clears its syllabus). The flag flips in the
//! same statement that writes or deletes a class-side row
//! ([`crate::db::offering_subject`]); the effective set is
//! [`crate::service::offering_subject::resolved_for_instance`].

use crate::domain::course::CourseId;
use crate::domain::subject::Subject;
use crate::error::ValidationError;

/// The membership precondition: `subject` may be selected for a set whose
/// course is `offering_course` only when it belongs to that course. The same
/// 400 the bare-course tag check (`service::subject::in_course`) has always
/// answered — one condition, one error shape, now enforced at selection time
/// as well as tag time.
pub fn ensure_course_member(
    subject: &Subject,
    offering_course: &CourseId,
) -> Result<(), ValidationError> {
    if subject.get_course() != offering_course {
        return Err(ValidationError::Invalid {
            field: "subject_id",
            reason: "subject belongs to a different course",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::subject::{SubjectDescription, SubjectId, SubjectName};

    fn a_subject(course: CourseId) -> Subject {
        Subject {
            id: SubjectId::generate(),
            course,
            name: SubjectName::try_new("Limits").unwrap(),
            description: SubjectDescription::try_new("").unwrap(),
        }
    }

    /// The precondition refuses a subject of another course, naming the field
    /// with the exact reason the tag-time check uses — drops the guard and
    /// this fails on the `Ok`.
    #[test]
    fn a_foreign_course_subject_is_refused() {
        let subject = a_subject(CourseId::from_key(
            "019732e3-7b00-7000-8000-00000000dead",
        ));
        let offering_course = CourseId::from_key("019732e3-7b00-7000-8000-00000000beef");
        let refused = ensure_course_member(&subject, &offering_course).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "subject_id: subject belongs to a different course"
        );
    }

    /// A subject of the offering's own course passes — negates the guard's
    /// "always refuse" mutation.
    #[test]
    fn an_own_course_subject_passes() {
        let course = CourseId::from_key("019732e3-7b00-7000-8000-00000000dead");
        let subject = a_subject(course.clone());
        ensure_course_member(&subject, &course).unwrap();
    }
}
