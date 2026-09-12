use uuid::Uuid;

use crate::constant::MAX_SOLUTION_BODY_LEN;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::PoolQuestionId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

/// Typed solution row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct SolutionId(Uuid);

impl SolutionId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// solutions sort `offered_at ASC, id ASC` and the id *is* the tie-break
    /// ([`crate::db::solution::list_for`]),
    /// and a random low half scrambles rows minted in the same millisecond.
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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Solution {
    pub(crate) id: SolutionId,
    pub(crate) question: PoolQuestionId,
    pub(crate) author: UserId,
    pub(crate) body: SolutionBody,
    pub(crate) offered_at: Timestamp,
    /// The photo's on-disk blob name — a fresh server-generated id every
    /// upload; `None` when the solution carries no image.
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
