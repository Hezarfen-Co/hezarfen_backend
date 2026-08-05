use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{REGISTRATION_COUNT_FIELD, REGISTRATION_TABLE};
use crate::database::Database;
use crate::domain::cap;
use crate::domain::event::{Event, EventId};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::AppError;

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
    /// the cap and never rewrites who placed them. The seat is taken by
    /// [`cap::claim_live_and_create`] on the event row — an atomic single-record
    /// conditional write that reads `audience.capacity` off that same row as it
    /// decides, so neither a racing registration nor a concurrent PATCH
    /// *lowering* the capacity can over-admit. Bound as a number instead, the
    /// snapshot below would admit every request already in flight when the
    /// lower cap landed.
    ///
    /// The seat is claimed on the event row, so the same statement says nothing
    /// about the *user* it is for — and a fall to `parent` is the one demotion
    /// whose sweep ([`crate::domain::user::User::set_role`]) can miss a seat and
    /// leave it unfreeable: `unregister` refuses a non-student target and a
    /// parent cannot reach the route at all. So the transaction also claims the
    /// holder's own record ([`cap::role_claim`]), which is the key that
    /// demotion writes: either the sweep sees this seat, or this write sees the
    /// parent and takes it back.
    pub async fn register(
        event: &EventId,
        user: &UserId,
        registered_by: &UserId,
        db: &Database,
    ) -> Result<Registration, AppError> {
        if let Some(existing) = Self::read_for_user(event, user, db).await? {
            return Ok(existing);
        }
        // Read for its refusals only — the audience kind and the closing time.
        // The seat count itself is re-read by the claim.
        Event::read(event, db)
            .await?
            .ok_or(AppError::NotFound)?
            .registration_capacity()?;
        let registration = Registration {
            id: RegistrationId::composite(event, user),
            event: event.clone(),
            user: user.clone(),
            registered_by: registered_by.clone(),
        };
        match cap::claim_live_and_create(
            &event.record(),
            REGISTRATION_COUNT_FIELD,
            // An uncapped registration list stores no `capacity` key at all
            // (SurrealDB drops a `NONE`-valued object key), so the coalesce is
            // what "unlimited" reads as.
            "audience.capacity ?? $num",
            cap::UNLIMITED,
            // Staff hold their own seats, so the bar is not "still a student"
            // but "still someone who can be taken off the list".
            Some((&user.record(), &format!("= '{}'", Role::Parent.as_str()))),
            &registration.id.record(),
            &registration,
            db,
        )
        .await?
        {
            cap::Claimed::Made(created) => Ok(created),
            // A concurrent placement of the same pair got there first: hand its
            // row over, the same no-op the early return above would have made,
            // and with no seat spent either way.
            cap::Claimed::Duplicate => Self::read_for_user(event, user, db)
                .await?
                .ok_or_else(|| AppError::Internal("failed to register user".into())),
            // Full, the event was deleted between the read and the claim, or
            // the holder fell to parent while this ran — the conditional writes
            // match nothing (or throw) either way, and only this path pays for
            // the reads that tell them apart.
            cap::Claimed::Full => match Event::read(event, db).await? {
                None => Err(AppError::NotFound),
                Some(_) => match User::read(user, db).await? {
                    Some(held) if held.get_role() == Role::Parent => Err(AppError::Forbidden(
                        "that account was demoted to parent while this request ran — \
                         only students can be registered for",
                    )),
                    _ => Err(AppError::Conflict("the event is full")),
                },
            },
        }
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
        // The seat comes back in the same transaction as the row that held it.
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 LET $gone = (DELETE registration WHERE event = $ev AND user = $usr RETURN BEFORE);
                 UPDATE $ev SET registration_count = math::max([(registration_count ?? 0) - array::len($gone), 0]);
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            )
            .bind(("ev", event.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Registration>>(3)?.into_iter().next())
    }

    // The demotion sweep — freeing every seat a fall to `parent` would strand,
    // and leaving a list that has already frozen exactly as it stands — is an
    // arm of [`crate::domain::user::User::set_role`], so the seat comes back in
    // the same transaction as the role that invalidated it. The freeze it obeys
    // is [`Event::registration_capacity`]'s `Conflict` arm, re-spelled for
    // SurrealQL as [`crate::constant::REGISTRATION_FROZEN_GUARD`] and held to it
    // by `event::tests::the_sql_freeze_guard_matches_the_rust_one`.
}
