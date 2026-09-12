use crate::domain::event::EventId;
use crate::domain::user::UserId;

/// The (event, user) pair — the table's natural composite primary key. The
/// same pair always maps to the same row, so one-row-per-pair holds by
/// construction (the enrollment trick) even if a write ever slipped past the
/// register lock. UUID strings carry only `-`, so `_` is an unambiguous
/// joiner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationId {
    event: EventId,
    user: UserId,
}

impl RegistrationId {
    pub fn composite(event: &EventId, user: &UserId) -> Self {
        Self {
            event: *event,
            user: *user,
        }
    }

    /// Parse the `{event}_{user}` wire form. A key that parses as no pair
    /// reads as the nil pair, which matches no row — exactly the 404 a
    /// dangling composite key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        let (event, user) = key.rsplit_once('_').unwrap_or(("", ""));
        Self {
            event: EventId::from_key(event),
            user: UserId::from_key(user),
        }
    }

    /// The `{event}_{user}` wire form.
    pub fn key(&self) -> String {
        format!("{}_{}", self.event.key(), self.user.key())
    }

    pub fn event(&self) -> EventId {
        self.event
    }

    pub fn user(&self) -> UserId {
        self.user
    }
}

/// A seat on a registration-audience event's signup list. The list *is* the
/// event's roster: whoever holds a row is expected (and markable), everyone
/// else is not. Rows survive audience changes inertly — switching the event
/// away from the registration kind hides them without deleting them.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Registration {
    pub(crate) event: EventId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) registered_by: UserId,
}

impl Registration {
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
    // arm, re-spelled for the stored guard in
    // [`crate::constant::REGISTRATION_FROZEN_GUARD`] and held to it by
    // `event::tests::the_sql_freeze_guard_matches_the_rust_one`.
}
