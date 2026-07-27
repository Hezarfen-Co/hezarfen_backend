//! An academic term — semester, trimester, quarter, whatever this school
//! runs; the structure is just rows, so it needs no code change per school.
//! Courses may link to one term.
//!
//! Terms are calendar structure, not schedules: a school adopting the app
//! mid-year legitimately creates a term that already started, so the no-past
//! rule that guards exams/lessons/events deliberately does not apply here.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{COURSE_COUNT_FIELD, MAX_TERM_NAME_LEN, TERM_TABLE};
use crate::database::Database;
use crate::domain::field_update::FieldUpdate;
use crate::domain::page::PagedList;
use crate::domain::timestamp::{Timestamp, range_error};
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

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
    pub async fn list_all(
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Term>, i64), AppError> {
        PagedList::new("term", "ORDER BY starts_at DESC")
            .run(limit, offset, db)
            .await
    }

    /// Write only the fields the PATCH carried — `None` means the request
    /// omitted it, so the column is left alone rather than re-stated from the
    /// snapshot this struct was read into. All three columns are non-nullable,
    /// so "absent" and "null" both correctly mean "keep".
    pub async fn update(
        self,
        name: Option<TermName>,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<Term, AppError> {
        FieldUpdate::new(self.id.record())
            .set("name", name)
            .set("starts_at", starts_at)
            .set("ends_at", ends_at)
            .ordered("starts_at", "ends_at", range_error())
            .run::<Term>(db)
            .await
    }

    /// Delete the term, but only while no course links it — nothing here
    /// unlinks or cascades. `false` = refused, nothing was written.
    ///
    /// The roster of linking courses is the term's own `course_count`
    /// refcount, claimed by [`crate::domain::course::Course::create`] and
    /// `update` *before* they write a link, so the check and the delete are one
    /// conditional write on one record: a course write racing this either
    /// claims first (and the delete is refused) or finds the row gone (and is
    /// refused itself, with the same 400 the lookup gives). `Err(NotFound)`
    /// keeps the answer a concurrent *delete* used to get.
    pub async fn delete(self, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query(format!(
                "DELETE $term WHERE ({COURSE_COUNT_FIELD} ?? 0) = 0 RETURN BEFORE"
            ))
            .bind(("term", self.id.record()))
            .await?
            .check()?;
        if !result.take::<Vec<Term>>(0)?.is_empty() {
            return Ok(true);
        }
        // Still linked or already gone: the one statement cannot tell those
        // apart, and only the refusal path pays for the read that can.
        match Self::read(&self.id, db).await? {
            Some(_) => Ok(false),
            None => Err(AppError::NotFound),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bite test for the `WHERE` guard that replaced `TERM_LOCK` on the
    /// PATCH path: the handler's pre-flight check is not in play here, so only
    /// the guard can refuse a moved end that inverts the range — and it must
    /// refuse it with the same error, having written nothing.
    #[tokio::test]
    async fn a_moved_end_is_refused_against_the_stored_other_end() {
        let db = crate::database::init_mem().await.unwrap();
        let at = Timestamp::from_millis;
        let term = Term::create(TermName::try_new("2026").unwrap(), at(100), at(200), &db)
            .await
            .unwrap();

        let refused = term
            .clone()
            .update(None, None, Some(at(50)), &db)
            .await
            .expect_err("an end before the stored start must be refused");
        assert!(refused.to_string().contains("at or after starts_at"));
        let stored = Term::read(term.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(
            stored.get_ends_at(),
            at(200),
            "nothing may have been written"
        );

        // A move that keeps the range ordered still lands, guard and all.
        let moved = term.update(None, None, Some(at(300)), &db).await.unwrap();
        assert_eq!(moved.get_ends_at(), at(300));
    }

    #[tokio::test]
    async fn name_is_required_and_bounded() {
        assert!(TermName::try_new("2026 Fall").is_ok());
        assert!(TermName::try_new("").is_err());
        assert!(TermName::try_new("   ").is_err());
        assert!(TermName::try_new(&"x".repeat(101)).is_err());
    }
}
