//! A homework assignment: a teacher hands it out per course, optionally
//! narrowed to a subset of the enrolled students, and every homework is tagged
//! with one of its course's subjects. Students submit against it
//! ([`crate::domain::homework_submission`]) and a teacher grades a status plus
//! an optional mark ([`crate::domain::homework_result`]).
//!
//! `course`, `created_by`, and `created_at` are fixed at creation (the schema
//! marks them `READONLY`): moving a homework between courses would strand the
//! submissions and grades of students not in the target course. `subject` is
//! deliberately *not* readonly — it is re-taggable through PATCH, validated
//! same-course by the web layer, exactly like an exam question's subject.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_HOMEWORK_DESCRIPTION_LEN, MAX_HOMEWORK_TITLE_LEN};
use crate::database::{Database, HOMEWORK_TABLE};
use crate::domain::course::CourseId;
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkId(RecordId);

impl HomeworkId {
    pub fn generate() -> Self {
        Self(RecordId::new(HOMEWORK_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(HOMEWORK_TABLE, key))
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
pub struct HomeworkTitle(String);

impl HomeworkTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_HOMEWORK_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkDescription(String);

impl HomeworkDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_HOMEWORK_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A homework assignment. `assigned` is the optional student subset: `None`
/// (the column absent) and an empty list both mean "the whole course" — see
/// [`Homework::student_sees`]. Because whole-course homework carries no roster,
/// a student who enrolls later is covered automatically; a subset is a fixed
/// snapshot of the students named at assign (or last PATCH) time.
#[derive(Debug, Clone, SurrealValue)]
pub struct Homework {
    id: HomeworkId,
    course: CourseId,
    subject: SubjectId,
    title: HomeworkTitle,
    description: Option<HomeworkDescription>,
    due_at: Timestamp,
    assigned: Option<Vec<UserId>>,
    created_by: UserId,
    created_at: Timestamp,
}

impl Homework {
    pub fn get_id(&self) -> &HomeworkId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_subject(&self) -> &SubjectId {
        &self.subject
    }

    pub fn get_title(&self) -> &HomeworkTitle {
        &self.title
    }

    pub fn get_description(&self) -> Option<&HomeworkDescription> {
        self.description.as_ref()
    }

    pub fn get_due_at(&self) -> Timestamp {
        self.due_at
    }

    /// The assigned student subset, or `None` for a whole-course homework.
    pub fn get_assigned(&self) -> Option<&[UserId]> {
        self.assigned.as_deref()
    }

    pub fn get_created_by(&self) -> &UserId {
        &self.created_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Whether `user` is in this homework's audience. A whole-course homework
    /// (`assigned` absent or empty) is visible to every enrolled student; a
    /// subset homework only to the students it names. Callers pair this with an
    /// enrollment check — being named is visibility, not enrollment.
    pub fn student_sees(&self, user: &UserId) -> bool {
        match &self.assigned {
            None => true,
            Some(assigned) => assigned.is_empty() || assigned.contains(user),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the sibling entities' create(field, field, ..) shape"
    )]
    pub async fn create(
        course: &CourseId,
        subject: &SubjectId,
        title: HomeworkTitle,
        description: Option<HomeworkDescription>,
        due_at: Timestamp,
        assigned: Option<Vec<UserId>>,
        created_by: &UserId,
        db: &Database,
    ) -> Result<Homework, AppError> {
        let homework = Homework {
            id: HomeworkId::generate(),
            course: course.clone(),
            subject: subject.clone(),
            title,
            description,
            due_at,
            assigned,
            created_by: created_by.clone(),
            created_at: Timestamp::now(),
        };
        let created: Option<Homework> = db.create(homework.id.record()).content(homework).await?;
        created.ok_or_else(|| AppError::Internal("failed to create homework".into()))
    }

    pub async fn read(id: &HomeworkId, db: &Database) -> Result<Option<Homework>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// The course's homework, newest first (ULID ids sort by creation). The web
    /// layer retains only the rows a given student `student_sees`.
    pub async fn list_for_course(
        course: &CourseId,
        db: &Database,
    ) -> Result<Vec<Homework>, AppError> {
        let mut result = db
            .query("SELECT * FROM homework WHERE course = $course ORDER BY id DESC")
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Homework>>(0)?)
    }

    /// Every homework in the system, newest first — the manager+ view of the
    /// cross-course "my homework" list.
    pub async fn list_all(db: &Database) -> Result<Vec<Homework>, AppError> {
        let mut result = db
            .query("SELECT * FROM homework ORDER BY id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<Homework>>(0)?)
    }

    /// Every homework of every course in `courses`, newest first (one query) —
    /// the cross-course list over a caller's visible courses. The web layer
    /// still trims each course's rows to what the caller may see (a student to
    /// the ones they `student_sees`).
    pub async fn list_for_courses(
        courses: &[CourseId],
        db: &Database,
    ) -> Result<Vec<Homework>, AppError> {
        if courses.is_empty() {
            return Ok(Vec::new());
        }
        let records: Vec<RecordId> = courses.iter().map(CourseId::record).collect();
        let mut result = db
            .query("SELECT * FROM homework WHERE course IN $courses ORDER BY id DESC")
            .bind(("courses", records))
            .await?
            .check()?;
        Ok(result.take::<Vec<Homework>>(0)?)
    }

    /// The homework of `course` that `user` is meant to see — whole-course ones
    /// plus any subset that names them — newest first. Backs a student's (or an
    /// observer's) per-course homework report; mirrors [`Homework::student_sees`]
    /// in SurQL so the filter runs in the database.
    pub async fn list_for_user_in_course(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<Homework>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM homework
                 WHERE course = $course
                   AND (assigned = NONE OR assigned = [] OR $usr IN assigned)
                 ORDER BY id DESC",
            )
            .bind(("course", course.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Homework>>(0)?)
    }

    /// Whether any homework still references `subject` — the subject delete
    /// guard's question: a subject with homework can't be deleted until the
    /// homework is re-tagged or removed. Mirrors
    /// [`crate::domain::exam_question::ExamQuestion::any_for_subject`].
    pub async fn any_for_subject(subject: &SubjectId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query("SELECT VALUE id FROM homework WHERE subject = $subject LIMIT 1")
            .bind(("subject", subject.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    /// Re-tag, re-title, re-describe, re-schedule, or re-scope the homework.
    /// Field-scoped, so `course`, `created_by`, and `created_at` are never in
    /// the write at all — no `READONLY` column is re-sent, and nothing the
    /// request didn't name can be carried back from a stale read. The web layer
    /// has already re-checked the new `due_at` against now and the new
    /// `subject` against the course, and refused a narrowing that would orphan
    /// a submission.
    pub async fn update(
        self,
        subject: &SubjectId,
        title: HomeworkTitle,
        description: Option<HomeworkDescription>,
        due_at: Timestamp,
        assigned: Option<Vec<UserId>>,
        db: &Database,
    ) -> Result<Homework, AppError> {
        let assigned = assigned.map(|users| users.iter().map(UserId::record).collect::<Vec<_>>());
        let mut result = db
            .query(
                "UPDATE $id SET subject = $subject, title = $title, description = $description,
                 due_at = $due_at, assigned = $assigned RETURN AFTER",
            )
            .bind(("id", self.id.record()))
            .bind(("subject", subject.record()))
            .bind(("title", title))
            .bind(("description", description))
            .bind(("due_at", due_at))
            .bind(("assigned", assigned))
            .await?
            .check()?;
        result.take::<Vec<Homework>>(0)?.into_iter().next().ok_or(AppError::NotFound)
    }

    /// Delete the homework and cascade its submissions, their files, and its
    /// results — one transaction, so a crash can't orphan a submission under a
    /// vanished homework. The submission file *blobs* are the web layer's to
    /// unlink: it collects their names via
    /// [`crate::domain::homework_file::HomeworkFile::file_keys_for_homework`]
    /// before calling this, and removes them after the rows are gone.
    pub async fn delete(self, db: &Database) -> Result<Homework, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE homework_file WHERE submission IN (SELECT VALUE id FROM homework_submission WHERE homework = $hw);
                 DELETE homework_submission WHERE homework = $hw;
                 DELETE homework_result WHERE homework = $hw;
                 DELETE $hw RETURN BEFORE;
                 COMMIT TRANSACTION;",
            )
            .bind(("hw", self.id.record()))
            .await?
            .check()?;
        // BEGIN is slot 0; the homework's own DELETE is slot 4.
        let deleted: Option<Homework> = result.take::<Vec<Homework>>(4)?.into_iter().next();
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert!(HomeworkTitle::try_new("Read chapter 3").is_ok());
        assert!(HomeworkTitle::try_new("").is_err());
        assert!(HomeworkTitle::try_new("   ").is_err());
        assert!(HomeworkTitle::try_new(&"x".repeat(201)).is_err());
    }

    #[tokio::test]
    async fn description_is_optional_but_bounded() {
        assert!(HomeworkDescription::try_new("").is_ok());
        assert!(HomeworkDescription::try_new(&"x".repeat(2_001)).is_err());
    }

    #[tokio::test]
    async fn student_sees_covers_whole_course_and_named_subsets() {
        let a = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        let b = UserId::from_key("01TESTUSERBBBBBBBBBBBBBBBB");
        let with = |assigned| Homework {
            id: HomeworkId::generate(),
            course: CourseId::from_key("01TESTCOURSEAAAAAAAAAAAAAA"),
            subject: SubjectId::from_key("01TESTSUBJECTAAAAAAAAAAAAA"),
            title: HomeworkTitle::try_new("hw").unwrap(),
            description: None,
            due_at: Timestamp::from_millis(1),
            assigned,
            created_by: a.clone(),
            created_at: Timestamp::from_millis(1),
        };
        // Whole course: absent or empty list means everyone sees it.
        assert!(with(None).student_sees(&a));
        assert!(with(Some(vec![])).student_sees(&b));
        // Subset: only the named students.
        assert!(with(Some(vec![a.clone()])).student_sees(&a));
        assert!(!with(Some(vec![a.clone()])).student_sees(&b));
    }
}
