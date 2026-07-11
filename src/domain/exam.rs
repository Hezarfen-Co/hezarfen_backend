use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_EXAM_DESCRIPTION_LEN, MAX_EXAM_TITLE_LEN};
use crate::database::{Database, EXAM_TABLE};
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_exam_kind, validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamId(RecordId);

impl ExamId {
    pub fn generate() -> Self {
        Self(RecordId::new(EXAM_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(EXAM_TABLE, key))
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
pub struct ExamTitle(String);

impl ExamTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_EXAM_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamDescription(String);

impl ExamDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_EXAM_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated exam kind: `homework` | `quiz`. An exam is never a graded course
/// mark — only one of these two assessment forms.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamKind(String);

impl ExamKind {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_exam_kind(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Exam {
    id: ExamId,
    creator: UserId,
    title: ExamTitle,
    description: ExamDescription,
    kind: ExamKind,
}

impl Exam {
    pub fn get_id(&self) -> &ExamId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_title(&self) -> &ExamTitle {
        &self.title
    }

    pub fn get_description(&self) -> &ExamDescription {
        &self.description
    }

    pub fn get_kind(&self) -> &ExamKind {
        &self.kind
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    pub async fn create(
        creator: &UserId,
        title: ExamTitle,
        description: ExamDescription,
        kind: ExamKind,
        db: &Database,
    ) -> Result<Exam, AppError> {
        let exam = Exam {
            id: ExamId::generate(),
            creator: creator.clone(),
            title,
            description,
            kind,
        };
        let created: Option<Exam> = db.create(exam.id.record()).content(exam).await?;
        created.ok_or_else(|| AppError::Internal("failed to create exam".into()))
    }

    pub async fn read(id: &ExamId, db: &Database) -> Result<Option<Exam>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_all(db: &Database) -> Result<Vec<Exam>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam ORDER BY id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<Exam>>(0)?)
    }

    pub async fn update(
        mut self,
        title: ExamTitle,
        description: ExamDescription,
        kind: ExamKind,
        db: &Database,
    ) -> Result<Exam, AppError> {
        self.title = title;
        self.description = description;
        self.kind = kind;
        let updated: Option<Exam> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Delete the exam and cascade-remove its result rows.
    pub async fn delete(self, db: &Database) -> Result<Exam, AppError> {
        db.query("DELETE exam_result WHERE exam = $ex")
            .bind(("ex", self.id.record()))
            .await?
            .check()?;
        let deleted: Option<Exam> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert!(ExamTitle::try_new("midterm").is_ok());
        assert!(ExamTitle::try_new("").is_err());
        assert!(ExamTitle::try_new("   ").is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(ExamDescription::try_new("").is_ok());
    }

    #[tokio::test]
    async fn kind_must_be_known() {
        for kind in ["homework", "quiz"] {
            assert_eq!(ExamKind::try_new(kind).unwrap().as_str(), kind);
        }
        assert!(ExamKind::try_new("final").is_err());
        assert!(ExamKind::try_new("").is_err());
    }
}
