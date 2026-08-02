use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::SESSION_ATTENDANCE_TABLE;
use crate::database::{Database, transaction_with_retry};
use crate::domain::attendance::AttendanceStatus;
use crate::domain::course::CourseId;
use crate::domain::course_session::{CourseSession, CourseSessionId};
use crate::domain::page::PagedList;
use crate::domain::user::UserId;
use crate::error::AppError;

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

/// One person's roll-call state for one lesson. `course` is denormalized from
/// the session so the per-course attendance report is a single indexed query
/// (`WHERE user = $u`) with no join.
#[derive(Debug, Clone, SurrealValue)]
pub struct SessionAttendance {
    id: SessionAttendanceId,
    session: CourseSessionId,
    course: CourseId,
    user: UserId,
    status: AttendanceStatus,
    marked_by: UserId,
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

    /// Record (or overwrite) `user`'s status for the session. One row per
    /// (session, user), keyed by a deterministic composite id so this is a
    /// single atomic UPSERT — concurrent marks converge on the one row.
    ///
    /// The "session still exists" gate rides in the same transaction as the
    /// mark, the mirror of the cascade in [`CourseSession::delete`]: the
    /// caller's pre-flight read sits four round trips in front of this write,
    /// so a delete landing in that gap used to leave a roll-call row on a
    /// session that was gone — and that row was *unremovable*, its only delete
    /// route 404ing on the vanished session while the attendance report went
    /// on counting it. A deleted session reads NONE, which is falsy, so the
    /// gate is written as an explicit `IS NONE` (see
    /// [`crate::domain::exam_result::ExamResult::grade`]).
    ///
    /// Sound to re-send while the store answers "conflict, retry": the gate
    /// reads the record a delete writes, so the two contend by design, and the
    /// UPSERT cannot legitimately answer "already exists" — its composite id is
    /// bijective with the `session_attendance_session_user` unique tuple, so
    /// the index entry can only point at the row the id already names.
    pub async fn mark(
        session: &CourseSession,
        user: &UserId,
        status: AttendanceStatus,
        marked_by: &UserId,
        db: &Database,
    ) -> Result<SessionAttendance, AppError> {
        let attendance = SessionAttendance {
            id: SessionAttendanceId::composite(session.get_id(), user),
            session: session.get_id().clone(),
            course: session.get_course().clone(),
            user: user.clone(),
            status,
            marked_by: marked_by.clone(),
        };
        let (mut written, mut errors) = transaction_with_retry(
            db,
            "BEGIN TRANSACTION;
                 IF (SELECT VALUE id FROM ONLY $sess) IS NONE { THROW 'session_missing' };
                 LET $after = (UPSERT $id CONTENT $row RETURN AFTER);
                 RETURN $after;
                 COMMIT TRANSACTION;",
            &[
                // `$session` is SurrealDB's own protected variable (the auth
                // session): binding that name errors the whole query.
                ("sess".into(), session.get_id().record().into_value()),
                ("id".into(), attendance.id.record().into_value()),
                ("row".into(), attendance.into_value()),
            ],
            &["session_missing"],
        )
        .await?;
        // An aborted transaction errors every slot and only the THROW's own
        // slot names the marker, so a refusal is read by marker while a lost
        // round was already re-sent — never reported as a 500.
        if errors
            .values()
            .any(|error| error.to_string().contains("session_missing"))
        {
            return Err(AppError::NotFound);
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`,
        // so its slot follows the statement count instead of a hand-kept one.
        let slot = written.num_statements().saturating_sub(2);
        written
            .take::<Vec<SessionAttendance>>(slot)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to mark session attendance".into()))
    }

    pub async fn list_for_session(
        session: &CourseSessionId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<SessionAttendance>, i64), AppError> {
        PagedList::new("session_attendance WHERE session = $s", "ORDER BY id DESC")
            .bind("s", session.record())
            .run(limit, offset, db)
            .await
    }

    /// Every roll-call row ever recorded for `user` — the session half of the
    /// attendance report. Deliberately not filtered by current enrollment:
    /// attendance is a historical record, so unenrolling hides marks (report
    /// semantics) but never absences.
    pub async fn list_for_user(
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<SessionAttendance>, AppError> {
        let mut result = db
            .query("SELECT * FROM session_attendance WHERE user = $usr ORDER BY id DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<SessionAttendance>>(0)?)
    }

    pub async fn remove(
        session: &CourseSessionId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<SessionAttendance>, AppError> {
        let mut result = db
            .query("DELETE session_attendance WHERE session = $s AND user = $usr RETURN BEFORE")
            .bind(("s", session.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<SessionAttendance>>(0)?.into_iter().next())
    }
}
