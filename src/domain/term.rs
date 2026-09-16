use sqlx::Type;
use uuid::Uuid;

use crate::constant::MAX_TERM_NAME_LEN;
use crate::domain::academic_year::AcademicYearId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

/// Typed term row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct TermId(Uuid);

impl TermId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// terms sort `starts_at DESC, id DESC` and the id breaks the tie between
    /// two terms starting at the same instant,
    /// and a random low half sorts arbitrarily inside one millisecond.
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
pub fn gone_error() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "term_id",
        reason: "term does not exist",
    })
}

/// The refusal every write against archived structure gets, coded so a client
/// can tell it from the other 409s on the same route.
pub fn archived_error() -> AppError {
    AppError::ConflictCoded {
        code: "term_archived",
        message: "this term is archived — past years are read-only".into(),
    }
}

/// What a PATCH's `term` field owes the refcounts: the term to claim, and the
/// one to give back. Covers all three moves — set (none→some), move
/// (some→other) and clear (some→none) — and moves nothing for a PATCH that
/// omitted the field or re-stated the link it already had.
pub fn ref_move(
    current: Option<&TermId>,
    patch: &Option<Option<TermId>>,
) -> (Option<TermId>, Option<TermId>) {
    match patch {
        None => (None, None),
        Some(next) if next.as_ref() == current => (None, None),
        Some(next) => (*next, current.copied()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct TermName(String);

impl TermName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_TERM_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One term on the school's academic calendar. Both ends are required — a
/// term is a date range by definition.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Term {
    pub(crate) id: TermId,
    /// The academic year this dönem is a grading slice of.
    pub(crate) year: AcademicYearId,
    pub(crate) name: TermName,
    pub(crate) starts_at: Timestamp,
    pub(crate) ends_at: Timestamp,
    /// When a manager archived this term; `None` = open. The column is
    /// nullable, so a term that predates archiving reads exactly like an
    /// open one.
    pub(crate) archived_at: Option<Timestamp>,
}

impl Term {
    pub fn get_id(&self) -> &TermId {
        &self.id
    }

    pub fn get_year(&self) -> &AcademicYearId {
        &self.year
    }

    pub fn get_name(&self) -> &TermName {
        &self.name
    }

    pub fn get_starts_at(&self) -> Timestamp {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Timestamp {
        self.ends_at
    }

    pub fn get_archived_at(&self) -> Option<Timestamp> {
        self.archived_at
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn name_is_required_and_bounded() {
        assert!(TermName::try_new("2026 Fall").is_ok());
        assert!(TermName::try_new("").is_err());
        assert!(TermName::try_new("   ").is_err());
        assert!(TermName::try_new(&"x".repeat(101)).is_err());
    }
}
