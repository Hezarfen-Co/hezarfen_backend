//! A solution offered on an approved pool question (see
//! [`crate::domain::pool_question`]). Anyone in the school — student or staff
//! — may offer one; rows list oldest first, reading as a discussion thread.
//! Solutions die with their question (the question delete cascades here).
//! Like a question, a solution may carry one photo (`image_*` metadata on the
//! row, bytes on disk under a server-generated ULID) — but unlike a question
//! it has no moderation state, so its author may edit the body and swap the
//! photo at any time; there is nothing to freeze. The queries live in
//! [`crate::db::solution`], the funnels in [`crate::service::solution`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_SOLUTION_BODY_LEN, SOLUTION_TABLE};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::PoolQuestionId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SolutionId(RecordId);

impl SolutionId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// solutions sort `offered_at ASC, id ASC` and the id *is* the tie-break
    /// ([`crate::db::solution::list_for`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(SOLUTION_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(SOLUTION_TABLE, key))
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
pub struct SolutionBody(String);

impl SolutionBody {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("body", value, MAX_SOLUTION_BODY_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Solution {
    pub(crate) id: SolutionId,
    pub(crate) question: PoolQuestionId,
    pub(crate) author: UserId,
    pub(crate) body: SolutionBody,
    pub(crate) offered_at: Timestamp,
    /// The photo's on-disk blob name — a fresh ULID every upload; `None` when
    /// the solution carries no image.
    pub(crate) image_file: Option<String>,
    pub(crate) image_content_type: Option<FileContentType>,
    pub(crate) image_size: Option<i64>,
}

impl Solution {
    pub fn new(question: &PoolQuestionId, author: &UserId, body: SolutionBody) -> Self {
        Self {
            id: SolutionId::generate(),
            question: question.clone(),
            author: author.clone(),
            body,
            offered_at: Timestamp::now(),
            image_file: None,
            image_content_type: None,
            image_size: None,
        }
    }

    pub fn get_id(&self) -> &SolutionId {
        &self.id
    }

    pub fn get_question(&self) -> &PoolQuestionId {
        &self.question
    }

    pub fn get_author(&self) -> &UserId {
        &self.author
    }

    pub fn get_body(&self) -> &SolutionBody {
        &self.body
    }

    pub fn get_offered_at(&self) -> Timestamp {
        self.offered_at
    }

    pub fn get_image_file(&self) -> Option<&str> {
        self.image_file.as_deref()
    }

    pub fn get_image_content_type(&self) -> Option<&FileContentType> {
        self.image_content_type.as_ref()
    }

    pub fn get_image_size(&self) -> Option<i64> {
        self.image_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_rules() {
        assert!(SolutionBody::try_new("").is_err());
        assert!(SolutionBody::try_new("   ").is_err());
        assert!(SolutionBody::try_new(&"x".repeat(10_001)).is_err());
        assert_eq!(
            SolutionBody::try_new("Kısmi integrasyon.")
                .unwrap()
                .as_str(),
            "Kısmi integrasyon."
        );
    }
}
