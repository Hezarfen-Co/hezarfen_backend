use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_EXAM_DESCRIPTION_LEN, MAX_EXAM_TITLE_LEN};
use crate::database::{Database, EXAM_TABLE};
use crate::domain::course::CourseId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_exam_kind, validate_optional, validate_required, validate_weight};

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

/// A validated exam kind: `homework` | `quiz` | `midterm` | `final` |
/// `project` | `oral`. Informational metadata only — `weight` drives the
/// course average.
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

/// A validated exam weight, held to `[MIN_EXAM_WEIGHT, MAX_EXAM_WEIGHT]`. The
/// exam counts `weight` times into its course's average.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
pub struct ExamWeight(i64);

impl ExamWeight {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        validate_weight(value)?;
        Ok(Self(value))
    }

    pub fn as_i64(&self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Exam {
    id: ExamId,
    creator: UserId,
    course: CourseId,
    title: ExamTitle,
    description: ExamDescription,
    kind: ExamKind,
    weight: ExamWeight,
}

impl Exam {
    pub fn get_id(&self) -> &ExamId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
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

    pub fn get_weight(&self) -> ExamWeight {
        self.weight
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    pub async fn create(
        creator: &UserId,
        course: &CourseId,
        title: ExamTitle,
        description: ExamDescription,
        kind: ExamKind,
        weight: ExamWeight,
        db: &Database,
    ) -> Result<Exam, AppError> {
        let exam = Exam {
            id: ExamId::generate(),
            creator: creator.clone(),
            course: course.clone(),
            title,
            description,
            kind,
            weight,
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

    pub async fn list_for_course(course: &CourseId, db: &Database) -> Result<Vec<Exam>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam WHERE course = $course ORDER BY id DESC")
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Exam>>(0)?)
    }

    // `course` is deliberately not updatable — moving an exam between courses
    // would strand results of students not enrolled in the target course.
    pub async fn update(
        mut self,
        title: ExamTitle,
        description: ExamDescription,
        kind: ExamKind,
        weight: ExamWeight,
        db: &Database,
    ) -> Result<Exam, AppError> {
        self.title = title;
        self.description = description;
        self.kind = kind;
        self.weight = weight;
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
        for kind in ["homework", "quiz", "midterm", "final", "project", "oral"] {
            assert_eq!(ExamKind::try_new(kind).unwrap().as_str(), kind);
        }
        assert!(ExamKind::try_new("essay").is_err());
        assert!(ExamKind::try_new("").is_err());
    }

    #[tokio::test]
    async fn weight_range_is_enforced() {
        assert_eq!(ExamWeight::try_new(1).unwrap().as_i64(), 1);
        assert_eq!(ExamWeight::try_new(100).unwrap().as_i64(), 100);
        assert!(ExamWeight::try_new(0).is_err());
        assert!(ExamWeight::try_new(101).is_err());
    }
}
