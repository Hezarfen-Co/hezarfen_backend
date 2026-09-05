use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{SESSION_DURATION_DAYS, SESSION_TABLE};
use crate::database::Database;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SessionId(RecordId);

impl SessionId {
    pub fn generate() -> Self {
        Self(RecordId::new(SESSION_TABLE, Ulid::new().to_string()))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    #[allow(dead_code)]
    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// A random, opaque session token (64 hex chars).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SessionToken(String);

impl SessionToken {
    pub fn generate() -> Result<Self, AppError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| AppError::Internal(format!("rng: {e}")))?;
        Ok(Self(hex::encode(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Session {
    id: SessionId,
    user: UserId,
    token: SessionToken,
    expires_at: Timestamp,
}

impl Session {
    pub async fn create(user: &UserId, db: &Database) -> Result<Session, AppError> {
        let session = Session {
            id: SessionId::generate(),
            user: user.clone(),
            token: SessionToken::generate()?,
            expires_at: Timestamp::in_days(SESSION_DURATION_DAYS),
        };
        let created: Option<Session> = db.create(session.id.record()).content(session).await?;
        created.ok_or_else(|| AppError::Internal("failed to create session".into()))
    }

    pub fn token(&self) -> &SessionToken {
        &self.token
    }

    pub fn user(&self) -> &UserId {
        &self.user
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at.is_past()
    }

    pub async fn find_by_token(token: &str, db: &Database) -> Result<Option<Session>, AppError> {
        let mut result = db
            .query("SELECT * FROM session WHERE token = $tok LIMIT 1")
            .bind(("tok", token.to_string()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Session>>(0)?.into_iter().next())
    }

    pub async fn delete_by_token(token: &str, db: &Database) -> Result<(), AppError> {
        db.query("DELETE session WHERE token = $tok")
            .bind(("tok", token.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    /// Revoke every session a user holds. The other half of a password reset:
    /// the new credential means nothing while a cookie minted under the old one
    /// still authenticates.
    pub async fn delete_by_user(user: &UserId, db: &Database) -> Result<(), AppError> {
        db.query("DELETE session WHERE user = $usr")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(())
    }

    /// Delete every session whose expiry is in the past, returning how many were
    /// removed. Expired sessions are already rejected at auth, but nothing else
    /// deletes their rows — without this sweep the `session` table grows forever.
    pub async fn purge_expired(db: &Database) -> Result<u64, AppError> {
        let now = Timestamp::now().as_millis();
        let mut result = db
            .query("DELETE session WHERE expires_at < $now RETURN BEFORE")
            .bind(("now", now))
            .await?
            .check()?;
        Ok(result.take::<Vec<Session>>(0)?.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_is_64_hex_and_unique() {
        let token = SessionToken::generate().unwrap();
        assert_eq!(token.as_str().len(), 64);
        assert!(token.as_str().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(SessionToken::generate().unwrap().as_str(), token.as_str());
    }

    #[tokio::test]
    async fn purge_expired_removes_only_past_sessions() {
        let db = crate::database::init_mem().await.unwrap();
        db.query(
            "CREATE session SET user = type::record('user', 'u'), token = 'past', expires_at = 1;
             CREATE session SET user = type::record('user', 'u'), token = 'future', expires_at = 99999999999999;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        assert_eq!(Session::purge_expired(&db).await.unwrap(), 1);
        assert!(Session::find_by_token("past", &db).await.unwrap().is_none());
        assert!(
            Session::find_by_token("future", &db)
                .await
                .unwrap()
                .is_some()
        );
    }
}
