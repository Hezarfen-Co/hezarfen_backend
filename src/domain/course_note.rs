use uuid::Uuid;
use crate::constant::{MAX_NOTE_CONTENT_LEN, MAX_NOTE_TITLE_LEN};
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct CourseNoteId(uuid::Uuid);

impl CourseNoteId {
    /// Minted from the process-wide monotonic generator, not a plain random
    /// UUID: a course's notes list `id DESC` (newest first,
    /// [`crate::db::course_note::list_for_course`]), and a random low half
    /// scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct CourseNoteTitle(String);

impl CourseNoteTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_NOTE_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct CourseNoteContent(String);

impl CourseNoteContent {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("content", value, MAX_NOTE_CONTENT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CourseNote {
    pub(crate) id: CourseNoteId,
    pub(crate) course: CourseId,
    pub(crate) author: UserId,
    pub(crate) title: CourseNoteTitle,
    pub(crate) content: CourseNoteContent,
}

impl CourseNote {
    pub fn get_id(&self) -> &CourseNoteId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_author(&self) -> &UserId {
        &self.author
    }

    pub fn get_title(&self) -> &CourseNoteTitle {
        &self.title
    }

    pub fn get_content(&self) -> &CourseNoteContent {
        &self.content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert_eq!(CourseNoteTitle::try_new("hi").unwrap().as_str(), "hi");
        assert!(CourseNoteTitle::try_new("  ").is_err());
    }

    #[tokio::test]
    async fn content_is_optional() {
        assert_eq!(CourseNoteContent::try_new("").unwrap().as_str(), "");
        assert_eq!(CourseNoteContent::try_new("body").unwrap().as_str(), "body");
    }
}
