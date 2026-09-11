use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::SESSION_ATTENDANCE_TABLE;
use crate::domain::attendance::AttendanceStatus;
use crate::domain::course::CourseId;
use crate::domain::course_session::CourseSessionId;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SessionAttendanceId(RecordId);

impl SessionAttendanceId {
    /// A deterministic id for the (session, user) pair. Because the same pair
    /// always maps to the same record id, marking is a single atomic UPSERT with
    /// no find-then-insert race, and one-row-per-pair holds by construction.
    /// ULID keys are alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(session: &CourseSessionId, user: &UserId) -> Self {
        Self(RecordId::new(
            SESSION_ATTENDANCE_TABLE,
            format!("{}_{}", session.key(), user.key()),
        ))
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

/// One person's roll-call state for one lesson. `course` is denormalized from
/// the session so the per-course attendance report is a single indexed query
/// (`WHERE user = $u`) with no join.
#[derive(Debug, Clone, SurrealValue)]
pub struct SessionAttendance {
    pub(crate) id: SessionAttendanceId,
    pub(crate) session: CourseSessionId,
    pub(crate) course: CourseId,
    pub(crate) user: UserId,
    pub(crate) status: AttendanceStatus,
    pub(crate) marked_by: UserId,
}

impl SessionAttendance {
    pub fn get_id(&self) -> &SessionAttendanceId {
        &self.id
    }

    pub fn get_session(&self) -> &CourseSessionId {
        &self.session
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
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
