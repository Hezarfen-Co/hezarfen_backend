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

use crate::domain::session::SessionToken;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{PasswordHash, Username};

pub const BUILDER_TABLE: &str = "builder";
pub const BUILDER_SESSION_TABLE: &str = "builder_session";

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BuilderId(RecordId);

impl BuilderId {
    pub fn generate() -> Self {
        Self(RecordId::new(BUILDER_TABLE, Ulid::generate().to_string()))
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
#[derive(Debug, Clone, SurrealValue)]
pub struct BuilderSession {
    pub(crate) id: RecordId,
    pub(crate) builder: BuilderId,
    pub(crate) token: SessionToken,
    pub(crate) created_at: Timestamp,
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
