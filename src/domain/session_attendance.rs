use crate::domain::attendance::AttendanceStatus;
use crate::domain::class_course::ClassCourseId;
use crate::domain::course_session::CourseSessionId;
use crate::domain::user::UserId;

/// The (session, user) pair — the table's natural composite primary key.
/// Because the same pair always maps to the same row, marking is a single
/// atomic upsert with no find-then-insert race, and one-row-per-pair holds by
/// construction. UUID strings carry only `-`, so `_` is an unambiguous joiner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttendanceId {
    session: CourseSessionId,
    user: UserId,
}

impl SessionAttendanceId {
    pub fn composite(session: &CourseSessionId, user: &UserId) -> Self {
        Self {
            session: session.clone(),
            user: *user,
        }
    }

    /// Parse the `{session}_{user}` wire form. A key that parses as no pair
    /// reads as the nil pair, which matches no row — exactly the 404 a
    /// dangling composite key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        let (session, user) = key.rsplit_once('_').unwrap_or(("", ""));
        Self {
            session: CourseSessionId::from_key(session),
            user: UserId::from_key(user),
        }
    }

    /// The `{session}_{user}` wire form.
    pub fn key(&self) -> String {
        format!("{}_{}", self.session.key(), self.user.key())
    }

    pub fn session(&self) -> CourseSessionId {
        self.session.clone()
    }

    pub fn user(&self) -> UserId {
        self.user
    }
}

/// Whether a status means the person was *there*, for the `lessons_attended`
/// badge counter. Exactly the rule `GET /attendance/me` already publishes —
/// `StatusCounts::tally` in `src/web/attendance.rs` puts `present` and `late`
/// over the line and leaves `excused` and every school-added status neutral —
/// so a student's badge and their attendance rate never disagree about what
/// attending is.
///
/// Hardcoding the two literals is safe by construction: statuses are the
/// school's to extend, but [`crate::domain::settings`] refuses any write that
/// drops one of the core four, so `present` and `late` can never be renamed
/// away. The SQL below repeats them — they are one rule in two languages, and
/// the transition test is what pins them together.
pub(crate) fn counts_as_attended(status: &AttendanceStatus) -> bool {
    matches!(status.as_str(), "present" | "late")
}

/// One person's roll-call state for one lesson. `class_course` is
/// denormalized from the session so the per-instance attendance report is a
/// single indexed query (`WHERE app_user = $u`) with no join.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionAttendance {
    pub(crate) session: CourseSessionId,
    pub(crate) class_course: ClassCourseId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) status: AttendanceStatus,
    pub(crate) marked_by: UserId,
}

impl SessionAttendance {
    pub fn get_session(&self) -> &CourseSessionId {
        &self.session
    }

    pub fn get_class_course(&self) -> &ClassCourseId {
        &self.class_course
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_status(&self) -> &AttendanceStatus {
        &self.status
    }

    pub fn get_marked_by(&self) -> &UserId {
        &self.marked_by
    }
}
