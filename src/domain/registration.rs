use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::REGISTRATION_TABLE;
use crate::domain::event::EventId;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct RegistrationId(RecordId);

impl RegistrationId {
    /// A deterministic id for the (event, user) pair — the same pair always
    /// maps to the same record id, so one-row-per-pair holds by construction
    /// (the enrollment trick) even if a write ever slipped past the register
    /// lock. ULID keys are alphanumeric, so `_` is unambiguous.
    pub fn composite(event: &EventId, user: &UserId) -> Self {
        Self(RecordId::new(
            REGISTRATION_TABLE,
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

/// A seat on a registration-audience event's signup list. The list *is* the
/// event's roster: whoever holds a row is expected (and markable), everyone
/// else is not. Rows survive audience changes inertly — switching the event
/// away from the registration kind hides them without deleting them.
#[derive(Debug, Clone, SurrealValue)]
pub struct Registration {
    pub(crate) id: RegistrationId,
    pub(crate) event: EventId,
    pub(crate) user: UserId,
    pub(crate) registered_by: UserId,
}

impl Registration {
    pub fn get_id(&self) -> &RegistrationId {
        &self.id
    }

    pub fn get_event(&self) -> &EventId {
        &self.event
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_registered_by(&self) -> &UserId {
        &self.registered_by
    }

    // The demotion sweep — freeing every seat a fall to `parent` would strand,
    // and leaving a list that has already frozen exactly as it stands — is an
    // arm of [`crate::service::user::set_role`], so the seat comes back in
    // the same transaction as the role that invalidated it. The freeze it obeys
    // is [`crate::domain::event::Event::registration_capacity`]'s `Conflict`
    // arm, re-spelled for SurrealQL as
    // [`crate::constant::REGISTRATION_FROZEN_GUARD`] and held to it by
    // `event::tests::the_sql_freeze_guard_matches_the_rust_one`.
}
