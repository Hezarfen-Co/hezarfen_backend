//! A homework assignment: a teacher hands it out per course, optionally
//! narrowed to a subset of the enrolled students, and every homework is tagged
//! with one of its course's subjects.
//! Students submit against it ([`crate::domain::homework_submission`]) and a
//! teacher grades a status plus an optional mark
//! ([`crate::domain::homework_result`]). Persistence lives in
//! [`crate::db::homework`], workflows in [`crate::service::homework`].
//!
//! `course`, `created_by`, and `created_at` are fixed at creation (the schema
//! marks them `READONLY`): moving a homework between courses would strand the
//! submissions and grades of students not in the target course. `subject` is
//! deliberately *not* readonly — it is re-taggable through PATCH, validated
//! same-course by the web layer, exactly like an exam question's subject.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{HOMEWORK_TABLE, MAX_HOMEWORK_DESCRIPTION_LEN, MAX_HOMEWORK_TITLE_LEN};
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkId(RecordId);

impl HomeworkId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// homework lists `id DESC` (newest first, [`Homework::list_all`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(HOMEWORK_TABLE, next_ulid().to_string()))
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
    pub(crate) id: HomeworkId,
    pub(crate) course: CourseId,
    pub(crate) subject: SubjectId,
    pub(crate) title: HomeworkTitle,
    pub(crate) description: Option<HomeworkDescription>,
    pub(crate) due_at: Timestamp,
    pub(crate) assigned: Option<Vec<UserId>>,
    pub(crate) created_by: UserId,
    pub(crate) created_at: Timestamp,
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
