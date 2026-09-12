use sqlx::Type;
use uuid::Uuid;

use crate::constant::{MAX_SUBJECT_DESCRIPTION_LEN, MAX_SUBJECT_NAME_LEN};
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_uuid;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

/// Typed subject row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct SubjectId(Uuid);

impl SubjectId {
    /// Minted from the process-wide monotonic generator, not a random v4: the
    /// id *is* the curriculum's order ([`crate::db::subject::list_for_course`]
    /// sorts `id ASC`), and a random low half scrambles a burst of saves that
    /// lands inside one millisecond.
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
#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct SubjectName(String);

impl SubjectName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_SUBJECT_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct SubjectDescription(String);

impl SubjectDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_SUBJECT_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A subject: one topic of a course's curriculum. Every exam question links to
/// a subject of its exam's course, so results can later be read per topic. The
/// course link is fixed at creation — a subject is course content, and moving
/// it would strand the questions tagged with it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Subject {
    pub(crate) id: SubjectId,
    pub(crate) course: CourseId,
    pub(crate) name: SubjectName,
    pub(crate) description: SubjectDescription,
}

impl Subject {
    pub fn get_id(&self) -> &SubjectId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_name(&self) -> &SubjectName {
        &self.name
    }

    pub fn get_description(&self) -> &SubjectDescription {
        &self.description
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A teacher entering a curriculum gets it back in the order they typed it:
    /// `list_for_course` sorts `id ASC`, so the ids minted inside one
    /// millisecond have to sort in mint order. Revert `generate` to a random
    /// v4 and this fails — the low bits are redrawn per id, so a same-tick
    /// burst comes out shuffled.
    #[tokio::test]
    async fn ids_sort_in_creation_order() {
        let ids: Vec<String> = (0..500).map(|_| SubjectId::generate().key()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[tokio::test]
    async fn name_is_required() {
        assert!(SubjectName::try_new("Limits").is_ok());
        assert!(SubjectName::try_new("").is_err());
        assert!(SubjectName::try_new("   ").is_err());
        assert!(SubjectName::try_new(&"x".repeat(201)).is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(SubjectDescription::try_new("").is_ok());
        assert!(SubjectDescription::try_new(&"x".repeat(2_001)).is_err());
    }
}
