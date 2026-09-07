//! The deployment operator's account, in the control database.
//!
//! A builder creates, suspends and drops schools; it is **not** a user of any
//! school and holds no role there. The two principals are kept apart by
//! construction: a builder row lives in a different database and its cookie
//! carries a different prefix (`builder.<token>` against `<slug>.<token>`), so
//! neither extractor can be fed the other's cookie.
//!
//! The credential newtypes are [`crate::domain::user`]'s — same rules, same
//! argon2 discipline, deliberately not a second copy that could drift.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::SESSION_DURATION_DAYS;
use crate::database::Database;
use crate::domain::session::SessionToken;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{Password, PasswordHash, Username};
use crate::error::AppError;

pub const BUILDER_TABLE: &str = "builder";
pub const BUILDER_SESSION_TABLE: &str = "builder_session";

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BuilderId(RecordId);

impl BuilderId {
    pub fn generate() -> Self {
        Self(RecordId::new(BUILDER_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(BUILDER_TABLE, key))
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

#[derive(Debug, Clone, SurrealValue)]
pub struct Builder {
    id: BuilderId,
    username: Username,
    password_hash: PasswordHash,
}

impl Builder {
    pub fn get_id(&self) -> &BuilderId {
        &self.id
    }

    pub fn get_username(&self) -> &Username {
        &self.username
    }

    pub fn get_password_hash(&self) -> &PasswordHash {
        &self.password_hash
    }

    pub async fn find_by_username(
        username: &str,
        control: &Database,
    ) -> Result<Option<Builder>, AppError> {
        let mut result = control
            .query("SELECT * FROM builder WHERE username = $u LIMIT 1")
            .bind(("u", username.trim().to_string()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Builder>>(0)?.into_iter().next())
    }

    pub async fn read(id: &BuilderId, control: &Database) -> Result<Option<Builder>, AppError> {
        Ok(control.select(id.record()).await?)
    }

    /// Idempotent startup seed. An existing row is left exactly as it is — the
    /// seed never rewrites a credential, so rotating `BUILDER_PASSWORD` in the
    /// environment does not silently re-key a live account (delete the row to
    /// re-seed). There is no role to promote here, so this cannot have
    /// `ensure_admin`'s create-then-promote hole.
    pub async fn ensure(
        username: Username,
        password: Password,
        control: &Database,
    ) -> Result<(), AppError> {
        if Self::find_by_username(username.as_str(), control)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let builder = Builder {
            id: BuilderId::generate(),
            password_hash: password.hash_async().await?,
            username,
        };
        let created: Option<Builder> = control
            .create(builder.id.record())
            .content(builder.clone())
            .await?;
        created.ok_or_else(|| AppError::Internal("failed to create the builder account".into()))?;
        // The account name stays out of the event: exported log records must
        // name no person (see `telemetry`).
        tracing::info!("seeded the builder account named by BUILDER_USERNAME");
        Ok(())
    }
}

/// A builder's login session. [`crate::domain::session::Session`]'s shape
/// against the control database, with the token type shared so both cookies
/// are the same 64 hex chars.
#[derive(Debug, Clone, SurrealValue)]
pub struct BuilderSession {
    id: RecordId,
    builder: BuilderId,
    token: SessionToken,
    created_at: Timestamp,
    expires_at: Timestamp,
}

impl BuilderSession {
    pub async fn create(
        builder: &BuilderId,
        control: &Database,
    ) -> Result<BuilderSession, AppError> {
        let session = BuilderSession {
            id: RecordId::new(BUILDER_SESSION_TABLE, Ulid::new().to_string()),
            builder: builder.clone(),
            token: SessionToken::generate()?,
            created_at: Timestamp::now(),
            expires_at: Timestamp::in_days(SESSION_DURATION_DAYS),
        };
        let created: Option<BuilderSession> =
            control.create(session.id.clone()).content(session).await?;
        created.ok_or_else(|| AppError::Internal("failed to create the builder session".into()))
    }

    pub fn token(&self) -> &SessionToken {
        &self.token
    }

    pub fn builder(&self) -> &BuilderId {
        &self.builder
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at.is_past()
    }

    pub async fn find_by_token(
        token: &str,
        control: &Database,
    ) -> Result<Option<BuilderSession>, AppError> {
        let mut result = control
            .query("SELECT * FROM builder_session WHERE token = $tok LIMIT 1")
            .bind(("tok", token.to_string()))
            .await?
            .check()?;
        Ok(result.take::<Vec<BuilderSession>>(0)?.into_iter().next())
    }

    pub async fn delete_by_token(token: &str, control: &Database) -> Result<(), AppError> {
        control
            .query("DELETE builder_session WHERE token = $tok")
            .bind(("tok", token.to_string()))
            .await?
            .check()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::Tenants;

    #[tokio::test]
    async fn the_seed_is_idempotent_and_never_rewrites_a_credential() {
        let tenants = Tenants::new_mem().await.unwrap();
        let control = tenants.control();
        let name = Username::try_new("builder1").unwrap();
        Builder::ensure(name.clone(), Password::try_new("secret1").unwrap(), control)
            .await
            .unwrap();
        let first = Builder::find_by_username("builder1", control)
            .await
            .unwrap()
            .expect("seeded");

        // A second boot with a *different* password must leave the row alone:
        // an environment edit is not a password reset.
        Builder::ensure(name, Password::try_new("secret2").unwrap(), control)
            .await
            .unwrap();
        let again = Builder::find_by_username("builder1", control)
            .await
            .unwrap()
            .expect("still there");
        assert_eq!(again.get_id(), first.get_id());
        assert_eq!(
            again.get_password_hash().as_str(),
            first.get_password_hash().as_str()
        );
    }

    #[tokio::test]
    async fn a_builder_session_round_trips_and_deletes() {
        let tenants = Tenants::new_mem().await.unwrap();
        let control = tenants.control();
        Builder::ensure(
            Username::try_new("builder1").unwrap(),
            Password::try_new("secret1").unwrap(),
            control,
        )
        .await
        .unwrap();
        let builder = Builder::find_by_username("builder1", control)
            .await
            .unwrap()
            .unwrap();

        let session = BuilderSession::create(builder.get_id(), control)
            .await
            .unwrap();
        let token = session.token().as_str().to_string();
        let found = BuilderSession::find_by_token(&token, control)
            .await
            .unwrap()
            .expect("found by token");
        assert_eq!(found.builder(), builder.get_id());
        assert!(!found.is_expired());

        BuilderSession::delete_by_token(&token, control)
            .await
            .unwrap();
        assert!(
            BuilderSession::find_by_token(&token, control)
                .await
                .unwrap()
                .is_none()
        );
    }
}
