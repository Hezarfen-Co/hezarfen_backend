use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_SUBJECT_DESCRIPTION_LEN, MAX_SUBJECT_NAME_LEN, SUBJECT_TABLE};
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_ulid;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SubjectId(RecordId);

impl SubjectId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`: the
    /// id *is* the curriculum's order ([`crate::db::subject::list_for_course`]
    /// sorts `id ASC`), and a random low half scrambles a burst of saves that
    /// lands inside one millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(SUBJECT_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(SUBJECT_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
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
#[derive(Debug, Clone, SurrealValue)]
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
    /// millisecond have to sort in mint order. Revert `generate` to
    /// `Ulid::new()` and this fails — the low 80 bits are redrawn per id, so a
    /// same-tick burst comes out shuffled.
    #[tokio::test]
    async fn ids_sort_in_creation_order() {
        let ids: Vec<String> = (0..500)
            .map(|_| SubjectId::generate().key().to_string())
            .collect();
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
