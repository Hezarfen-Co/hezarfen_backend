//! An academic term — semester, trimester, quarter, whatever this school
//! runs; the structure is just rows, so it needs no code change per school.
//! Courses may link to one term.
//!
//! Terms are calendar structure, not schedules: a school adopting the app
//! mid-year legitimately creates a term that already started, so the no-past
//! rule that guards exams/lessons/events deliberately does not apply here.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_TERM_NAME_LEN, TERM_TABLE};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct TermId(RecordId);

impl TermId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// terms sort `starts_at DESC, id DESC` and the id breaks the tie between
    /// two terms starting at the same instant,
    /// and a random low half sorts arbitrarily inside one millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(TERM_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(TERM_TABLE, key))
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

/// The answer every link to a term that is not there gets — the claim is a
/// conditional write on the term row, so a term a delete already removed
/// matches nothing and the caller says exactly what the link resolver's
/// pre-flight lookup ([`crate::service::term::resolve`]) would have.
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
) -> (Option<RecordId>, Option<RecordId>) {
    match patch {
        None => (None, None),
        Some(next) if next.as_ref() == current => (None, None),
        Some(next) => (
            next.as_ref().map(TermId::record),
            current.map(TermId::record),
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
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
#[derive(Debug, Clone, SurrealValue)]
pub struct Term {
    pub(crate) id: TermId,
    pub(crate) name: TermName,
    pub(crate) starts_at: Timestamp,
    pub(crate) ends_at: Timestamp,
    /// When a manager archived this term; `None` = open. `#[surreal(default)]`
    /// for the same reason `BankQuestion::subject` has one: rows written before
    /// the column existed still decode, as open terms.
    #[surreal(default)]
    pub(crate) archived_at: Option<Timestamp>,
}

impl Term {
    pub fn get_id(&self) -> &TermId {
        &self.id
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
