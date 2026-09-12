use uuid::Uuid;

use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

/// Typed work-entry row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct WorkEntryId(Uuid);

impl WorkEntryId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// the log sorts `check_in DESC, id DESC` and the id breaks the tie between
    /// two stints checked in at the same instant.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    /// Parse a wire key. A key that parses as no UUID — a malformed path
    /// segment — reads as the nil id, which matches no row: exactly the 404 a
    /// dangling record key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        Self(Uuid::parse_str(key).unwrap_or(Uuid::nil()))
    }

    /// The hyphenated wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// One work stint of a staff member: server-stamped `check_in`, and
/// `check_out` once closed. The wall clock is read server-side only — a
/// client can never supply its own instants (managers correct closed entries
/// through an explicit endpoint instead).
///
/// "At most one open stint per staff member" is no longer carried by a
/// deterministic key: it is a partial unique index on the table
/// (`work_entry_open`) over rows whose `check_out` is NULL, so the database
/// itself refuses a second check-in.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkEntry {
    pub(crate) id: WorkEntryId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) check_in: Timestamp,
    pub(crate) check_out: Option<Timestamp>,
}

impl WorkEntry {
    pub fn get_id(&self) -> &WorkEntryId {
        &self.id
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_check_in(&self) -> Timestamp {
        self.check_in
    }

    pub fn get_check_out(&self) -> Option<Timestamp> {
        self.check_out
    }

    pub fn is_open(&self) -> bool {
        self.check_out.is_none()
    }
}
