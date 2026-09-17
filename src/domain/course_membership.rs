use crate::domain::course::CourseId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

/// The identity of one (course, user) pair. Not a row column: the table's
/// primary key *is* the pair, which makes joining a single atomic UPSERT
/// (`ON CONFLICT (course, app_user)`) with no find-then-insert race. This
/// struct's job is the underscore-joined wire form at the HTTP edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourseMembershipId {
    pub(crate) course: CourseId,
    pub(crate) user: UserId,
}

impl CourseMembershipId {
    pub fn composite(course: &CourseId, user: &UserId) -> Self {
        Self {
            course: course.clone(),
            user: *user,
        }
    }

    /// The underscore-joined wire form (`{course}_{user}`).
    pub fn key(&self) -> String {
        format!("{}_{}", self.course.key(), self.user.key())
    }
}

/// A user's individual membership in a *school-scoped* course (D9): a club or
/// a supervised study a student joins directly. A class-delivered course
/// ([`crate::domain::course::CourseKind::is_class_delivered`]) is joined
/// through its instance instead, never here — that gate lives in the service.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CourseMembership {
    pub(crate) course: CourseId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) added_by: UserId,
    pub(crate) created_at: Timestamp,
}

impl CourseMembership {
    pub fn get_id(&self) -> CourseMembershipId {
        CourseMembershipId::composite(&self.course, &self.user)
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_added_by(&self) -> &UserId {
        &self.added_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }
}
