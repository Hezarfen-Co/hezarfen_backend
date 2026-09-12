use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::user::UserId;

/// The identity of one (course, user) pair. Not a row column: the table's
/// primary key *is* the pair, which is what makes enrolling a single atomic
/// UPSERT (`ON CONFLICT (course, app_user)`) with no find-then-insert race
/// and one-row-per-pair by construction. This struct's job is the
/// underscore-joined wire form at the HTTP edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentId {
    pub(crate) course: CourseId,
    pub(crate) user: UserId,
}

impl EnrollmentId {
    pub fn composite(course: &CourseId, user: &UserId) -> Self {
        Self {
            course: course.clone(),
            user: user.clone(),
        }
    }

    /// The underscore-joined wire form (`{course}_{user}`).
    pub fn key(&self) -> String {
        format!("{}_{}", self.course.key(), self.user.key())
    }
}

/// A user's membership in a course. Grading requires it; removing it hides the
/// user's marks from the report but never deletes result rows.
///
/// `source` names the class ([`crate::domain::class_group::ClassGroup`]) that
/// pumped this row, and is NULL when a human placed the student directly —
/// every row placed by hand. Absence *is* the meaning, so nothing backfills
/// it: a class sweep may only take back the rows it wrote.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Enrollment {
    pub(crate) course: CourseId,
    pub(crate) user: UserId,
    pub(crate) enrolled_by: UserId,
    pub(crate) source: Option<ClassGroupId>,
}

impl Enrollment {
    /// The row's identity, built back from its primary-key columns.
    pub fn get_id(&self) -> EnrollmentId {
        EnrollmentId::composite(&self.course, &self.user)
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_enrolled_by(&self) -> &UserId {
        &self.enrolled_by
    }

    /// The class that pumped this row, or `None` for a hand-placed one.
    pub fn get_source(&self) -> Option<&ClassGroupId> {
        self.source.as_ref()
    }
}
