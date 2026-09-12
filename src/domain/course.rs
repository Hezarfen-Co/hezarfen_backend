use crate::constant::{MAX_COURSE_DESCRIPTION_LEN, MAX_COURSE_TITLE_LEN};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::term::TermId;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_course_kind, validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct CourseId(uuid::Uuid);

impl CourseId {
    /// Minted from the process-wide monotonic generator, not a plain random
    /// UUID: courses list `id DESC` (newest first,
    /// [`crate::db::course::list_all`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
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

    /// The inner uuid, for binding the id through the runtime-checked
    /// builders ([`crate::db::page::Param`]) whose values are raw uuids.
    pub(crate) fn uuid(&self) -> uuid::Uuid {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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
/// at enroll time (`NULL` = unlimited).
///
/// `creator` owns the course for good — only they (or a manager+) may delete
/// it. `teachers` are the staff a manager assigned to run it: full management
/// rights inside the course, no power to delete it or change the assignment
/// list.
///
/// Fields are crate-visible: [`crate::db::course`] mints the rows on create
/// and reads the id when updating and cascading a delete.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Course {
    pub(crate) id: CourseId,
    pub(crate) creator: UserId,
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
}
