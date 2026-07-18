use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;

use crate::database::{Database, REGISTRATION_TABLE};
use crate::domain::event::{Event, EventId};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Serializes seat-taking. A `BEGIN…COMMIT` around the count can't do this:
/// SurrealDB transactions don't conflict-check a cross-record `count()`
/// against a concurrent insert (write-skew), so two racing registrations both
/// saw a free seat and a full event over-admitted. The database is embedded —
/// this process is the only writer — so one process-wide lock is sufficient.
// ponytail: global lock, per-event locks (or DB-side serialization) if
// registration ever sees real contention.
static REGISTER_LOCK: Mutex<()> = Mutex::const_new(());

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

    /// Register (idempotently) `user` onto `event`, refusing when a capacity
    /// cap is set and every seat is taken. Someone already listed gets their
    /// existing row back untouched — a true no-op that never counts against
    /// the cap and never rewrites who placed them. The whole check-then-write
    /// runs under [`REGISTER_LOCK`], and the gate + cap are re-derived from a
    /// fresh event read under that lock, so neither a racing registration nor
    /// a concurrent capacity/audience/schedule PATCH can over-admit.
    pub async fn register(
        event: &EventId,
        user: &UserId,
        registered_by: &UserId,
        db: &Database,
    ) -> Result<Registration, AppError> {
        let _guard = REGISTER_LOCK.lock().await;
        if let Some(existing) = Self::read_for_user(event, user, db).await? {
            return Ok(existing);
        }
        let capacity = Event::read(event, db)
            .await?
            .ok_or(AppError::NotFound)?
            .registration_capacity()?;
        if let Some(capacity) = capacity
            && Self::list_for_event(event, db).await?.len() as i64 >= capacity
        {
            return Err(AppError::Conflict("the event is full"));
        }
        let registration = Registration {
            id: RegistrationId::composite(event, user),
            event: event.clone(),
            user: user.clone(),
            registered_by: registered_by.clone(),
        };
        let created: Option<Registration> = db
            .create(registration.id.record())
            .content(registration)
            .await?;
        created.ok_or_else(|| AppError::Internal("failed to register user".into()))
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
