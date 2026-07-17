use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_SUBJECT_DESCRIPTION_LEN, MAX_SUBJECT_NAME_LEN};
use crate::database::{Database, SUBJECT_TABLE};
use crate::domain::course::CourseId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SubjectId(RecordId);

impl SubjectId {
    pub fn generate() -> Self {
        Self(RecordId::new(SUBJECT_TABLE, Ulid::new().to_string()))
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
    id: SubjectId,
    course: CourseId,
    name: SubjectName,
    description: SubjectDescription,
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

    pub async fn create(
        course: &CourseId,
        name: SubjectName,
        description: SubjectDescription,
        db: &Database,
    ) -> Result<Subject, AppError> {
        let subject = Subject {
            id: SubjectId::generate(),
            course: course.clone(),
            name,
            description,
        };
        let created: Option<Subject> = db.create(subject.id.record()).content(subject).await?;
        created.ok_or_else(|| AppError::Internal("failed to create subject".into()))
    }

    pub async fn read(id: &SubjectId, db: &Database) -> Result<Option<Subject>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// The course's subjects in curriculum order (ULID ids sort by creation).
    pub async fn list_for_course(
        course: &CourseId,
        db: &Database,
    ) -> Result<Vec<Subject>, AppError> {
        let mut result = db
            .query("SELECT * FROM subject WHERE course = $course ORDER BY id ASC")
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Subject>>(0)?)
    }

    pub async fn update(
        mut self,
        name: SubjectName,
        description: SubjectDescription,
        db: &Database,
    ) -> Result<Subject, AppError> {
        self.name = name;
        self.description = description;
        let updated: Option<Subject> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    pub async fn delete(self, db: &Database) -> Result<Subject, AppError> {
        let deleted: Option<Subject> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
