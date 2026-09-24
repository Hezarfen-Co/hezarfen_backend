//! The holiday calendar: one named, school-wide non-teaching range. Both ends
//! are *instants* (unix millis), exactly like `term`, `event`, `exam` and
//! `academic_year` — the materializer compares instants, so no separate day
//! type exists.

use uuid::Uuid;

use crate::constant::{HOLIDAY_KINDS, MAX_HOLIDAY_NAME_LEN};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

/// Typed holiday row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct HolidayId(Uuid);

impl HolidayId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// holidays sort `starts_at DESC, id DESC` and the id breaks the tie
    /// between two holidays starting at the same instant.
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

/// The holiday's display name ("29 Ekim Cumhuriyet Bayramı").
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct HolidayName(String);

impl HolidayName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_HOLIDAY_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why the school is closed, from a closed list: `resmi` (official), `dini`
/// (religious), `idari` (administrative), `ara` (a break inside the year).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct HolidayKind(String);

impl HolidayKind {
    /// The accepted spellings — the wire form of the DDL's `CHECK`.
    pub const ALL: [&str; 4] = HOLIDAY_KINDS;

    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        if !Self::ALL.contains(&value) {
            return Err(ValidationError::Unknown {
                field: "kind",
                value: value.to_owned(),
            });
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One school-wide non-teaching range: from `starts_at` through `ends_at`
/// (inclusive instants), named, and stamped by the manager who declared it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Holiday {
    pub(crate) id: HolidayId,
    pub(crate) name: HolidayName,
    pub(crate) starts_at: Timestamp,
    pub(crate) ends_at: Timestamp,
    pub(crate) kind: HolidayKind,
    pub(crate) creator: UserId,
    pub(crate) created_at: Timestamp,
}

impl Holiday {
    pub fn get_id(&self) -> &HolidayId {
        &self.id
    }

    pub fn get_name(&self) -> &HolidayName {
        &self.name
    }

    pub fn get_starts_at(&self) -> Timestamp {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Timestamp {
        self.ends_at
    }

    pub fn get_kind(&self) -> &HolidayKind {
        &self.kind
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Whether this holiday reaches into `[from, to]`. Touching counts: a
    /// holiday ending exactly when the range begins (or beginning exactly
    /// when it ends) still blocks the day it touches.
    pub fn overlaps(&self, from: Timestamp, to: Timestamp) -> bool {
        self.ends_at >= from && self.starts_at <= to
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::user::UserId;

    fn a_holiday(starts_at: i64, ends_at: i64) -> Holiday {
        Holiday {
            id: HolidayId::generate(),
            name: HolidayName::try_new("29 Ekim").unwrap(),
            starts_at: Timestamp::from_millis(starts_at),
            ends_at: Timestamp::from_millis(ends_at),
            kind: HolidayKind::try_new("resmi").unwrap(),
            creator: UserId::generate(),
            created_at: Timestamp::now(),
        }
    }

    #[test]
    fn name_is_required_and_bounded() {
        assert!(HolidayName::try_new("29 Ekim").is_ok());
        // Blank (whitespace-only) names are refused like an empty one.
        assert!(HolidayName::try_new("   ").is_err());
        assert!(HolidayName::try_new("").is_err());
        assert!(HolidayName::try_new(&"x".repeat(MAX_HOLIDAY_NAME_LEN + 1)).is_err());
        assert!(HolidayName::try_new(&"x".repeat(MAX_HOLIDAY_NAME_LEN)).is_ok());
    }

    #[test]
    fn kind_is_a_closed_list() {
        for kind in HolidayKind::ALL {
            assert!(HolidayKind::try_new(kind).is_ok());
        }
        assert!(matches!(
            HolidayKind::try_new("x"),
            Err(ValidationError::Unknown { field: "kind", .. })
        ));
    }

    #[test]
    fn overlaps_at_both_touching_edges() {
        // The holiday spans [100, 200].
        let holiday = a_holiday(100, 200);
        // Touching on either edge is still a block.
        assert!(holiday.overlaps(Timestamp::from_millis(200), Timestamp::from_millis(300)));
        assert!(holiday.overlaps(Timestamp::from_millis(0), Timestamp::from_millis(100)));
        // One millisecond past either edge is not.
        assert!(!holiday.overlaps(Timestamp::from_millis(201), Timestamp::from_millis(300)));
        assert!(!holiday.overlaps(Timestamp::from_millis(0), Timestamp::from_millis(99)));
        // And a range inside the holiday is, of course.
        assert!(holiday.overlaps(Timestamp::from_millis(150), Timestamp::from_millis(160)));
    }
}
