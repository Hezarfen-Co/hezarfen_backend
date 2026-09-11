use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{COURSE_TABLE, MAX_COURSE_DESCRIPTION_LEN, MAX_COURSE_TITLE_LEN};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::term::TermId;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_course_kind, validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseId(RecordId);

impl CourseId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// courses list `id DESC` (newest first, [`crate::db::course::list_all`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(COURSE_TABLE, next_ulid().to_string()))
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

/// A validated course kind: `course` (a regular class — ders), `study` (a
/// supervised study session — etüt), or `club` (a student club — kulüp).
/// Purely a label; all kinds behave identically.
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
/// academic term. Comes in three behaviorally identical kinds: `course`,
/// `study` (etüt), and `club` (kulüp). An optional `capacity` caps the roster
/// at enroll time (`None` = unlimited); rows written before the field existed
/// decode as uncapped.
///
/// `creator` owns the course for good — only they (or a manager+) may delete
/// it. `teachers` are the staff a manager assigned to run it: full management
/// rights inside the course, no power to delete it or change the assignment
/// list. Rows written before the field existed decode with nobody assigned.
///
/// Fields are crate-visible: [`crate::db::course`] mints the rows on create
/// and reads the id when updating and cascading a delete.
#[derive(Debug, Clone, SurrealValue)]
pub struct Course {
    pub(crate) id: CourseId,
    pub(crate) creator: UserId,
    #[surreal(default)]
    pub(crate) teachers: Vec<UserId>,
    pub(crate) title: CourseTitle,
    pub(crate) description: CourseDescription,
    pub(crate) kind: CourseKind,
    pub(crate) term: Option<TermId>,
    pub(crate) capacity: Option<i64>,
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

    /// The seat cap enforced at enroll time; `None` = unlimited.
    pub fn get_capacity(&self) -> Option<i64> {
        self.capacity
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    /// The staff assigned to run this course, in assignment order.
    pub fn get_teachers(&self) -> &[UserId] {
        &self.teachers
    }

    /// Whether `user` was assigned to teach this course. Says nothing about
    /// the creator — they own it whether or not they also appear here.
    pub fn is_assigned(&self, user: &UserId) -> bool {
        self.teachers.contains(user)
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
    async fn kind_is_course_study_or_club() {
        assert!(CourseKind::try_new("course").is_ok());
        assert!(CourseKind::try_new("study").is_ok());
        assert!(CourseKind::try_new("club").is_ok());
        assert!(CourseKind::try_new("etut").is_err());
        assert_eq!(CourseKind::course().as_str(), "course");
    }

    /// The database strips `NONE`-valued optional columns, and every course
    /// row written before the capacity field existed has no `capacity` key at
    /// all — both must decode as an uncapped course.
    #[tokio::test]
    async fn course_decodes_without_capacity_key() {
        use surrealdb::types::Value;

        let course = Course {
            id: CourseId::generate(),
            creator: UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T"),
            teachers: Vec::new(),
            title: CourseTitle::try_new("chess").unwrap(),
            description: CourseDescription::try_new("").unwrap(),
            kind: CourseKind::try_new("club").unwrap(),
            term: None,
            capacity: Some(12),
        };
        let Value::Object(mut object) = course.into_value() else {
            panic!("course must encode as an object");
        };
        object.remove("capacity");
        let decoded = Course::from_value(Value::Object(object)).unwrap();
        assert_eq!(decoded.get_capacity(), None);
    }

    /// Every course row written before teacher assignment existed has no
    /// `teachers` key. The boot backfill fills them in, but a row read before
    /// that lands must still decode — as a course nobody was assigned to,
    /// never as a decode error that 500s the catalog.
    #[tokio::test]
    async fn course_decodes_without_teachers_key() {
        use surrealdb::types::Value;

        let assigned = UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8U");
        let course = Course {
            id: CourseId::generate(),
            creator: UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T"),
            teachers: vec![assigned.clone()],
            title: CourseTitle::try_new("chess").unwrap(),
            description: CourseDescription::try_new("").unwrap(),
            kind: CourseKind::try_new("club").unwrap(),
            term: None,
            capacity: None,
        };
        assert!(course.is_assigned(&assigned));

        let Value::Object(mut object) = course.into_value() else {
            panic!("course must encode as an object");
        };
        object.remove("teachers");
        let decoded = Course::from_value(Value::Object(object)).unwrap();
        assert!(decoded.get_teachers().is_empty());
        assert!(!decoded.is_assigned(&assigned));
    }
}
