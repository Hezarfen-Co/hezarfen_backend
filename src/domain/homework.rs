//! A homework assignment: a teacher hands it out per course, optionally
//! narrowed to a subset of the enrolled students, and every homework is tagged
//! with one of its course's subjects.
//! Students submit against it ([`crate::domain::homework_submission`]) and a
//! teacher grades a status plus an optional mark
//! ([`crate::domain::homework_result`]). Persistence lives in
//! [`crate::db::homework`], workflows in [`crate::service::homework`].
//!
//! `course`, `created_by`, and `created_at` are fixed at creation (READONLY
//! app discipline): moving a homework between courses would strand the
//! submissions and grades of students not in the target course. `subject` is
//! deliberately *not* fixed — it is re-taggable through PATCH, validated
//! same-course by the web layer, exactly like an exam question's subject.

use crate::constant::{MAX_HOMEWORK_DESCRIPTION_LEN, MAX_HOMEWORK_TITLE_LEN};
use crate::domain::class_course::ClassCourseId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct HomeworkId(uuid::Uuid);

impl HomeworkId {
    /// Minted from the process-wide monotonic generator, not a plain random
    /// UUID: homework lists `id DESC` (newest first, [`HomeworkId`]-ordered
    /// listings), and a random low half scrambles rows minted in the same
    /// millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> uuid::Uuid {
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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

/// A homework assignment. `assigned` is the optional student subset,
/// assembled from the `homework_assignment` child rows (`None` = no rows =
/// the whole course — see [`Homework::student_sees`]). Because whole-course
/// homework carries no roster, a student who enrolls later is covered
/// automatically; a subset is a fixed snapshot of the students named at
/// assign (or last PATCH) time.
///
/// The `subject` tag names a topic of the homework's course — and, since the
/// course-template system, one the instance actually *teaches*: its resolved
/// subject set (override-or-inherit), validated service-side
/// ([`crate::service::subject::in_instance`] at tag time,
/// [`crate::service::offering_subject::ensure_resolved_member`] on create),
/// because the check needs the instance's override state and so cannot live
/// as a pure rule on this row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Homework {
    pub(crate) id: HomeworkId,
    pub(crate) class_course: ClassCourseId,
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

    pub fn get_class_course(&self) -> &ClassCourseId {
        &self.class_course
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
    /// (`assigned` NULL or empty) is visible to every enrolled student; a
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

    const A: &str = "0198f1a2-3b4c-7d5e-8f90-aaaa2b3c4d5e";
    const B: &str = "0198f1a2-3b4c-7d5e-8f90-bbbb3c4d5e6f";

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
        let a = UserId::from_key(A);
        let b = UserId::from_key(B);
        let with = |assigned| Homework {
            id: HomeworkId::generate(),
            class_course: ClassCourseId::from_key("0198f1a2-3b4c-7d5e-8f90-cccc3c4d5e6f"),
            subject: SubjectId::from_key("0198f1a2-3b4c-7d5e-8f90-dddd4c4d5e6f"),
            title: HomeworkTitle::try_new("hw").unwrap(),
            description: None,
            due_at: Timestamp::from_millis(1),
            assigned,
            created_by: a,
            created_at: Timestamp::from_millis(1),
        };
        // Whole course: NULL or empty list means everyone sees it.
        assert!(with(None).student_sees(&a));
        assert!(with(Some(vec![])).student_sees(&b));
        // Subset: only the named students.
        assert!(with(Some(vec![a])).student_sees(&a));
        assert!(!with(Some(vec![a])).student_sees(&b));
    }
}
