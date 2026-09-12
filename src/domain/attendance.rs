use crate::domain::event::EventId;
use crate::domain::user::UserId;
use crate::error::ValidationError;

/// The identity of one (event, user) pair. Not a row column: the table's
/// primary key *is* the pair, which is what makes marking a single atomic
/// UPSERT (`ON CONFLICT (event, app_user)`) with no find-then-insert race,
/// one-row-per-pair by construction. This struct's job is the
/// underscore-joined wire form at the HTTP edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttendanceId {
    pub(crate) event: EventId,
    pub(crate) user: UserId,
}

impl AttendanceId {
    pub fn composite(event: &EventId, user: &UserId) -> Self {
        Self {
            event: event.clone(),
            user: *user,
        }
    }

    /// The underscore-joined wire form (`{event}_{user}`).
    pub fn key(&self) -> String {
        format!("{}_{}", self.event.key(), self.user.key())
    }
}

/// A validated attendance state — one of the school's configured statuses
/// ([`crate::domain::settings::Settings::get_attendance_statuses`]): the core
/// `present | absent | late | excused` plus any the school added. Stored rows
/// keep their status even if the school later edits the list.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct AttendanceStatus(String);

impl AttendanceStatus {
    pub fn try_new(value: &str, allowed: &[String]) -> Result<Self, ValidationError> {
        if !allowed.iter().any(|status| status == value) {
            return Err(ValidationError::Invalid {
                field: "status",
                reason: "is not one of this school's attendance statuses (see GET /settings)",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Attendance {
    pub(crate) event: EventId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) status: AttendanceStatus,
    pub(crate) marked_by: UserId,
}

impl Attendance {
    /// The row's identity, built back from its primary-key columns.
    pub fn get_id(&self) -> AttendanceId {
        AttendanceId::composite(&self.event, &self.user)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_must_be_in_the_allowed_list() {
        let allowed: Vec<String> = crate::domain::settings::Settings::defaults()
            .get_attendance_statuses()
            .to_vec();
        for status in ["present", "absent", "late", "excused"] {
            assert_eq!(
                AttendanceStatus::try_new(status, &allowed)
                    .unwrap()
                    .as_str(),
                status
            );
        }
        assert!(AttendanceStatus::try_new("maybe", &allowed).is_err());
        assert!(AttendanceStatus::try_new("", &allowed).is_err());
        // A school-added status is accepted once it's in the list.
        let extended: Vec<String> = allowed
            .iter()
            .cloned()
            .chain(["online".to_string()])
            .collect();
        assert!(AttendanceStatus::try_new("online", &extended).is_ok());
    }
}
