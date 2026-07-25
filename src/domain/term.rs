//! An academic term — semester, trimester, quarter, whatever this school
//! runs; the structure is just rows, so it needs no code change per school.
//! Courses may link to one term.
//!
//! Terms are calendar structure, not schedules: a school adopting the app
//! mid-year legitimately creates a term that already started, so the no-past
//! rule that guards exams/lessons/events deliberately does not apply here.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;
use ulid::Ulid;

use crate::constant::MAX_TERM_NAME_LEN;
use crate::database::{Database, TERM_TABLE};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

/// Serializes the term delete guard (does any course still link it?) against
/// the course writes that assign a term, so a course can't land on a term that
/// is already on its way out. Course writes take it only when they actually
/// link a term.
///
/// Lock order: no path ever holds this and `ENROLL_LOCK` at the same time — the
/// course writes that take this one touch no roster, and the course delete that
/// takes `ENROLL_LOCK` touches no term — so the two cannot deadlock. Should a
/// future path need both, take `ENROLL_LOCK` first.
pub(crate) static TERM_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct TermId(RecordId);

impl TermId {
    pub fn generate() -> Self {
        Self(RecordId::new(TERM_TABLE, Ulid::new().to_string()))
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
    id: TermId,
    name: TermName,
    starts_at: Timestamp,
    ends_at: Timestamp,
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

    pub async fn create(
        name: TermName,
        starts_at: Timestamp,
        ends_at: Timestamp,
        db: &Database,
    ) -> Result<Term, AppError> {
        let term = Term {
            id: TermId::generate(),
            name,
            starts_at,
            ends_at,
        };
        let created: Option<Term> = db.create(term.id.record()).content(term).await?;
        created.ok_or_else(|| AppError::Internal("failed to create term".into()))
    }

    pub async fn read(id: &TermId, db: &Database) -> Result<Option<Term>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every term, newest first — the school calendar is small by nature.
    pub async fn list_all(db: &Database) -> Result<Vec<Term>, AppError> {
        let mut result = db
            .query("SELECT * FROM term ORDER BY starts_at DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<Term>>(0)?)
    }

    /// Field-scoped: the handler reads the term, then awaits validation before
    /// saving with no lock held ([`TERM_LOCK`] guards the delete's link check,
    /// not this), so the save states its own three columns instead of replaying
    /// a whole stale row.
    pub async fn update(
        self,
        name: TermName,
        starts_at: Timestamp,
        ends_at: Timestamp,
        db: &Database,
    ) -> Result<Term, AppError> {
        let mut result = db
            .query(
                "UPDATE $id SET name = $name, starts_at = $starts_at,
                 ends_at = $ends_at RETURN AFTER",
            )
            .bind(("id", self.id.record()))
            .bind(("name", name))
            .bind(("starts_at", starts_at))
            .bind(("ends_at", ends_at))
            .await?
            .check()?;
        result.take::<Vec<Term>>(0)?.into_iter().next().ok_or(AppError::NotFound)
    }

    /// True iff any course still links to this term — the delete guard.
    pub async fn any_course(id: &TermId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query("SELECT VALUE id FROM course WHERE term = $term LIMIT 1")
            .bind(("term", id.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    /// Delete the term. Callers must refuse while [`Term::any_course`] holds —
    /// nothing here unlinks or cascades, so a term is only ever dropped once no
    /// course points at it.
    pub async fn delete(self, db: &Database) -> Result<Term, AppError> {
        let deleted: Option<Term> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
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
