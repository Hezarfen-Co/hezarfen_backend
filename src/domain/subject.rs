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

    /// Several subjects in one query — the bulk half of a list endpoint that
    /// names each row's subject (a read per row would be an N+1).
    pub async fn list_by_ids(ids: &[&SubjectId], db: &Database) -> Result<Vec<Subject>, AppError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let records: Vec<RecordId> = ids.iter().map(|id| id.record()).collect();
        let mut result = db
            .query("SELECT * FROM subject WHERE id IN $ids")
            .bind(("ids", records))
            .await?
            .check()?;
        Ok(result.take::<Vec<Subject>>(0)?)
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
        // Field-scoped: the handler holds no lock across its read and this
        // write, so a whole-row save would revert a concurrent edit of the
        // other field.
        let mut result = db
            .query("UPDATE $id SET name = $name, description = $description RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("name", name))
            .bind(("description", description))
            .await?
            .check()?;
        result.take::<Vec<Subject>>(0)?.into_iter().next().ok_or(AppError::NotFound)
    }

    /// Delete the subject and clear it off every bank template that carried it
    /// as origin metadata — one transaction, so a template can't be left
    /// pointing at a subject that no longer exists.
    ///
    /// Exam questions and homework are *not* cascaded: their `subject` is a
    /// required field the web layer refuses to orphan (both still block the
    /// delete with a 409). The bank's is optional metadata, and blocking on it
    /// was a dead end — only the template's owner may re-tag it, so a manager
    /// could never clear their own 409, and a private template raising it
    /// leaked its existence.
    pub async fn delete(self, db: &Database) -> Result<Subject, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 UPDATE bank_question SET subject = NONE WHERE subject = $sub;
                 LET $before = (DELETE $sub RETURN BEFORE);
                 RETURN $before;
                 COMMIT TRANSACTION;",
            )
            .bind(("sub", self.id.record()))
            .await?
            .check()?;
        // Read through the trailing `RETURN`, not a counted slot — see
        // [`crate::domain::exam::Exam::delete`].
        let slot = result.num_statements().saturating_sub(2);
        let deleted: Option<Subject> = result.take::<Vec<Subject>>(slot)?.into_iter().next();
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
