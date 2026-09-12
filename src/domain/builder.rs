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

use uuid::Uuid;

use crate::domain::monotonic_id::next_uuid;
use crate::domain::session::SessionToken;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{PasswordHash, Username};

pub const BUILDER_TABLE: &str = "builder";
pub const BUILDER_SESSION_TABLE: &str = "builder_session";

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct BuilderId(Uuid);

impl BuilderId {
    /// A plain random UUID deliberately: builder rows are looked up by
    /// username, never listed by id.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(Uuid::parse_str(key).unwrap_or(Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Builder {
    pub(crate) id: BuilderId,
    pub(crate) username: Username,
    pub(crate) password_hash: PasswordHash,
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
}

/// A builder's login session. [`crate::domain::session::Session`]'s shape
/// against the control database, with the token type shared so both cookies
/// are the same 64 hex chars.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BuilderSession {
    pub(crate) builder: BuilderId,
    pub(crate) token: SessionToken,
    pub(crate) expires_at: Timestamp,
}

impl BuilderSession {
    pub fn token(&self) -> &SessionToken {
        &self.token
    }

    pub fn builder(&self) -> &BuilderId {
        &self.builder
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at.is_past()
    }
}
