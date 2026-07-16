use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::database::{Database, REGISTRATION_TABLE};
use crate::domain::event::EventId;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct RegistrationId(RecordId);

impl RegistrationId {
    /// A deterministic id for the (event, user) pair — the same pair always
    /// maps to the same record id, so registering is one atomic UPSERT with
    /// no find-then-insert race and one-row-per-pair by construction (the
    /// enrollment trick). ULID keys are alphanumeric, so `_` is unambiguous.
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
    id: RegistrationId,
    event: EventId,
    user: UserId,
    registered_by: UserId,
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

    /// Register (idempotently) `user` onto `event`, refusing when a `capacity`
    /// cap is set and every seat is taken. The seat count is re-read inside the
    /// transaction and the THROW cancels the UPSERT, so a full event never
    /// over-admits through the check-then-write gap; re-registering someone
    /// already listed never counts against the cap (and returns their row).
    pub async fn register(
        event: &EventId,
        user: &UserId,
        registered_by: &UserId,
        capacity: Option<i64>,
        db: &Database,
    ) -> Result<Registration, AppError> {
        let registration = Registration {
            id: RegistrationId::composite(event, user),
            event: event.clone(),
            user: user.clone(),
            registered_by: registered_by.clone(),
        };
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 LET $already = (SELECT VALUE id FROM registration WHERE event = $ev AND user = $usr);
                 LET $count = (SELECT count() FROM registration WHERE event = $ev GROUP ALL)[0].count ?? 0;
                 IF $cap != NONE AND array::len($already) = 0 AND $count >= $cap { THROW 'event_full' };
                 UPSERT $id CONTENT $registration;
                 COMMIT TRANSACTION;",
            )
            .bind(("ev", event.record()))
            .bind(("usr", user.record()))
            .bind(("cap", capacity))
            .bind(("id", registration.id.record()))
            .bind(("registration", registration))
            .await?;
        // An aborted transaction errors *every* slot, most with a generic
        // "not executed" — only the THROW's own slot names the reason, so scan
        // them all for the marker instead of trusting the first (`check`-style)
        // error.
        let mut errors = result.take_errors();
        if errors
            .values()
            .any(|error| error.to_string().contains("event_full"))
        {
            return Err(AppError::Conflict("the event is full"));
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // Statement slots count BEGIN and the LETs too: the UPSERT is slot 4.
        let saved: Option<Registration> = result.take::<Vec<Registration>>(4)?.into_iter().next();
        saved.ok_or_else(|| AppError::Internal("failed to register user".into()))
    }

    /// Some(_) iff `user` holds a seat on `event` — the audience point check.
    pub async fn read_for_user(
        event: &EventId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<Registration>, AppError> {
        let mut result = db
            .query("SELECT * FROM registration WHERE event = $ev AND user = $usr LIMIT 1")
            .bind(("ev", event.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Registration>>(0)?.into_iter().next())
    }

    pub async fn list_for_event(
        event: &EventId,
        db: &Database,
    ) -> Result<Vec<Registration>, AppError> {
        let mut result = db
            .query("SELECT * FROM registration WHERE event = $ev ORDER BY id DESC")
            .bind(("ev", event.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Registration>>(0)?)
    }

    pub async fn remove(
        event: &EventId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<Registration>, AppError> {
        let mut result = db
            .query("DELETE registration WHERE event = $ev AND user = $usr RETURN BEFORE")
            .bind(("ev", event.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Registration>>(0)?.into_iter().next())
    }
}
