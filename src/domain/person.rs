//! Pure person shapes: the typed id, the control-plane account row, the
//! school memberships read off the registry join, and the `person.` login
//! session. Persistence lives in [`crate::db::person`]; the register-side
//! create-or-load in [`crate::service::person`].
//!
//! A person is the global account behind a school's [`crate::domain::user`]
//! rows: one username + password in the control database, any number of
//! memberships. Role and profile stay per-school — a person has neither
//! here, only the credential and where they belong.

use uuid::Uuid;

use crate::domain::monotonic_id::next_uuid;
use crate::domain::session::SessionToken;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{PasswordHash, Username};
use crate::tenant::{SchoolStatus, Slug};

/// Typed person row id (control database). A UUIDv7 minted by the
/// process-wide monotonic generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct PersonId(Uuid);

impl PersonId {
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

/// The control-plane account row. Fields are crate-visible:
/// [`crate::db::person`] mints rows, exactly like the other split domains.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Person {
    pub(crate) id: PersonId,
    pub(crate) username: Username,
    pub(crate) password_hash: PasswordHash,
}

impl Person {
    pub fn get_id(&self) -> &PersonId {
        &self.id
    }

    pub fn get_username(&self) -> &Username {
        &self.username
    }

    pub fn get_password_hash(&self) -> &PasswordHash {
        &self.password_hash
    }
}

/// One school a person belongs to, joined off the control registry.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Membership {
    pub(crate) slug: Slug,
    pub(crate) name: String,
    pub(crate) status: SchoolStatus,
}

impl Membership {
    pub fn slug(&self) -> &Slug {
        &self.slug
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// A suspended school is not offered at login and cannot be selected.
    pub fn is_active(&self) -> bool {
        self.status == SchoolStatus::Active
    }
}

/// A control-plane login session — the row behind a `person.<token>` cookie.
/// It names no school: it is exactly the not-yet-chosen state, and
/// `POST /auth/school` exchanges it for a school session.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PersonSession {
    pub(crate) person: PersonId,
    pub(crate) token: SessionToken,
    pub(crate) expires_at: Timestamp,
}

impl PersonSession {
    pub fn token(&self) -> &SessionToken {
        &self.token
    }

    pub fn person(&self) -> &PersonId {
        &self.person
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at.is_past()
    }
}
