use uuid::Uuid;

use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Typed session row id. A UUIDv7 minted by the process-wide monotonic
/// generator. (The table is `user_session` in PostgreSQL — the Rust type
/// keeps its name.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    /// The hyphenated wire form.
    #[allow(dead_code)]
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// A random, opaque session token (64 hex chars).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Session {
    #[sqlx(rename = "app_user")]
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
