use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::database::{ATTENDANCE_TABLE, Database};
use crate::domain::event::EventId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_status;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AttendanceId(RecordId);

impl AttendanceId {
    /// A deterministic id for the (event, user) pair. Because the same pair
    /// always maps to the same record id, marking is a single atomic UPSERT with
    /// no find-then-insert race, and one-row-per-pair holds by construction.
    /// ULID keys are alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(event: &EventId, user: &UserId) -> Self {
        Self(RecordId::new(
            ATTENDANCE_TABLE,
            format!("{}_{}", event.key(), user.key()),
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

/// A validated attendance state: present | absent | late | excused.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AttendanceStatus(String);

impl AttendanceStatus {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_status(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Attendance {
    id: AttendanceId,
    event: EventId,
    user: UserId,
    status: AttendanceStatus,
    marked_by: UserId,
}

impl Attendance {
    pub fn get_id(&self) -> &AttendanceId {
        &self.id
    }

    pub fn get_event(&self) -> &EventId {
        &self.event
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

    /// Record (or overwrite) `user`'s status for `event`. One row per (event,
    /// user), keyed by a deterministic composite id so this is a single atomic
    /// UPSERT — concurrent marks for the same pair can no longer both insert and
    /// collide on the unique index (a 500); they converge on the one row.
    pub async fn mark(
        event: &EventId,
        user: &UserId,
        status: AttendanceStatus,
        marked_by: &UserId,
        db: &Database,
    ) -> Result<Attendance, AppError> {
        let attendance = Attendance {
            id: AttendanceId::composite(event, user),
            event: event.clone(),
            user: user.clone(),
            status,
            marked_by: marked_by.clone(),
        };
        let saved: Option<Attendance> = db
            .upsert(attendance.id.record())
            .content(attendance)
            .await?;
        saved.ok_or_else(|| AppError::Internal("failed to mark attendance".into()))
    }

    pub async fn list_for_event(
        event: &EventId,
        db: &Database,
    ) -> Result<Vec<Attendance>, AppError> {
        let mut result = db
            .query("SELECT * FROM attendance WHERE event = $ev ORDER BY id DESC")
            .bind(("ev", event.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Attendance>>(0)?)
    }

    /// Every event-attendance row recorded for `user` — the events half of the
    /// attendance report.
    pub async fn list_for_user(user: &UserId, db: &Database) -> Result<Vec<Attendance>, AppError> {
        let mut result = db
            .query("SELECT * FROM attendance WHERE user = $usr ORDER BY id DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Attendance>>(0)?)
    }

    pub async fn remove(
        event: &EventId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<Attendance>, AppError> {
        let mut result = db
            .query("DELETE attendance WHERE event = $ev AND user = $usr RETURN BEFORE")
            .bind(("ev", event.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Attendance>>(0)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_must_be_known() {
        for status in ["present", "absent", "late", "excused"] {
            assert_eq!(AttendanceStatus::try_new(status).unwrap().as_str(), status);
        }
        assert!(AttendanceStatus::try_new("maybe").is_err());
        assert!(AttendanceStatus::try_new("").is_err());
    }
}
