use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::SESSION_TABLE;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SessionId(RecordId);

impl SessionId {
    pub fn generate() -> Self {
        Self(RecordId::new(SESSION_TABLE, Ulid::generate().to_string()))
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

/// Fields are crate-visible: [`crate::db::session`] mints the rows on login.
#[derive(Debug, Clone, SurrealValue)]
pub struct Session {
    pub(crate) id: SessionId,
    pub(crate) user: UserId,
    pub(crate) token: SessionToken,
    pub(crate) expires_at: Timestamp,
}

impl Session {
    pub fn token(&self) -> &SessionToken {
        &self.token
    }

    pub fn user(&self) -> &UserId {
        &self.user
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at.is_past()
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
}
