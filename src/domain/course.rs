use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_COURSE_DESCRIPTION_LEN, MAX_COURSE_TITLE_LEN};
use crate::database::{COURSE_TABLE, Database};
use crate::domain::term::TermId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_course_kind, validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseId(RecordId);

impl CourseId {
    pub fn generate() -> Self {
        Self(RecordId::new(COURSE_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(COURSE_TABLE, key))
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
pub struct CourseTitle(String);

impl CourseTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_COURSE_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseDescription(String);

impl CourseDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_COURSE_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated course kind: `course` (a regular class — ders) or `study` (a
/// supervised study session — etüt). Purely a label; both kinds behave
/// identically.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseKind(String);

impl CourseKind {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_course_kind(value)?;
        Ok(Self(value.to_string()))
    }

    /// The classic kind — what every course is unless said otherwise.
    pub fn course() -> Self {
        Self("course".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A course: the unit exams and enrollments hang off. Marks are computed per
/// course, each exam weighted by its kind's settings weight. May belong to an
/// academic term. Comes in two behaviorally identical kinds: `course` and
/// `study` (etüt).
#[derive(Debug, Clone, SurrealValue)]
pub struct Course {
    id: CourseId,
    creator: UserId,
    title: CourseTitle,
    description: CourseDescription,
    kind: CourseKind,
    term: Option<TermId>,
}

impl Course {
    pub fn get_id(&self) -> &CourseId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_title(&self) -> &CourseTitle {
        &self.title
    }

    pub fn get_description(&self) -> &CourseDescription {
        &self.description
    }

    pub fn get_kind(&self) -> &CourseKind {
        &self.kind
    }

    pub fn get_term(&self) -> Option<&TermId> {
        self.term.as_ref()
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    pub async fn create(
        creator: &UserId,
        title: CourseTitle,
        description: CourseDescription,
        kind: CourseKind,
        term: Option<TermId>,
        db: &Database,
    ) -> Result<Course, AppError> {
        let course = Course {
            id: CourseId::generate(),
            creator: creator.clone(),
            title,
            description,
            kind,
            term,
        };
        let created: Option<Course> = db.create(course.id.record()).content(course).await?;
        created.ok_or_else(|| AppError::Internal("failed to create course".into()))
    }

    pub async fn read(id: &CourseId, db: &Database) -> Result<Option<Course>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_all(db: &Database) -> Result<Vec<Course>, AppError> {
        let mut result = db
            .query("SELECT * FROM course ORDER BY id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<Course>>(0)?)
    }

    /// The courses `user` is enrolled in — the spine of `/courses/me` and the
    /// marks report.
    pub async fn list_enrolled(user: &UserId, db: &Database) -> Result<Vec<Course>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM course
                 WHERE id IN (SELECT VALUE course FROM enrollment WHERE user = $usr)
                 ORDER BY id DESC",
            )
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Course>>(0)?)
    }

    /// The courses `user` created — a teacher's slice of the catalog.
    pub async fn list_created(user: &UserId, db: &Database) -> Result<Vec<Course>, AppError> {
        let mut result = db
            .query("SELECT * FROM course WHERE creator = $usr ORDER BY id DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Course>>(0)?)
    }

    /// Load every course behind `ids` (one query) — the join half of the
    /// attendance report's per-course blocks.
    pub async fn list_by_ids(ids: &[CourseId], db: &Database) -> Result<Vec<Course>, AppError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let records: Vec<RecordId> = ids.iter().map(CourseId::record).collect();
        let mut result = db
            .query("SELECT * FROM course WHERE id IN $ids")
            .bind(("ids", records))
            .await?
            .check()?;
        Ok(result.take::<Vec<Course>>(0)?)
    }

    pub async fn update(
        mut self,
        title: CourseTitle,
        description: CourseDescription,
        kind: CourseKind,
        term: Option<TermId>,
        db: &Database,
    ) -> Result<Course, AppError> {
        self.title = title;
        self.description = description;
        self.kind = kind;
        self.term = term;
        let updated: Option<Course> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Delete the course and cascade-remove everything inside it: results and
    /// attempts of its exams, its enrollments, its sessions with their roll
    /// call, its subjects, and the exams themselves. The children go in one
    /// transaction so a crash can't leave an exam pointing at a deleted course.
    pub async fn delete(self, db: &Database) -> Result<Course, AppError> {
        db.query(
            "BEGIN TRANSACTION;
             DELETE exam_result WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE exam_attempt WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE exam_answer WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE exam_question WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
             DELETE session_attendance WHERE course = $course;
             DELETE course_session WHERE course = $course;
             DELETE enrollment WHERE course = $course;
             DELETE subject WHERE course = $course;
             DELETE exam WHERE course = $course;
             COMMIT TRANSACTION;",
        )
        .bind(("course", self.id.record()))
        .await?
        .check()?;
        let deleted: Option<Course> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert!(CourseTitle::try_new("algebra").is_ok());
        assert!(CourseTitle::try_new("").is_err());
        assert!(CourseTitle::try_new("   ").is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(CourseDescription::try_new("").is_ok());
    }

    #[tokio::test]
    async fn kind_is_course_or_study() {
        assert!(CourseKind::try_new("course").is_ok());
        assert!(CourseKind::try_new("study").is_ok());
        assert!(CourseKind::try_new("etut").is_err());
        assert_eq!(CourseKind::course().as_str(), "course");
    }
}
