use crate::constant::{MAX_COURSE_DESCRIPTION_LEN, MAX_COURSE_TITLE_LEN};
use crate::domain::monotonic_id::next_uuid;
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
/// Only `course` is class-delivered (a şube attaches it as an instance);
/// `study`/`club` are school-scoped and joined individually.
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

    /// Whether a şube attaches this kind as an instance. Only a regular ders
    /// (`course`) is class-delivered; clubs and etüt are school-scoped and
    /// joined individually ([`crate::domain::course_membership`]).
    pub fn is_class_delivered(&self) -> bool {
        self.0 == "course"
    }
}

/// A course: a school-level catalog entry. It is a *template* — the academic
/// work (roster, exams, timetable) lives on the instances a class attaches
/// ([`crate::domain::class_course::ClassCourse`]), so this row carries only
/// what every instance shares: title, description, kind.
///
/// `creator` is who minted it; the catalog is Manager+-owned, so there is no
/// per-teacher ownership. The two counters are the delete guard: a course may
/// only be dropped when no class attaches it and no one holds an individual
/// membership.
///
/// Fields are crate-visible: [`crate::db::course`] mints the rows on create
/// and reads the id when updating and cascading a delete.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Course {
    pub(crate) id: CourseId,
    pub(crate) creator: UserId,
    pub(crate) title: CourseTitle,
    pub(crate) description: CourseDescription,
    pub(crate) kind: CourseKind,
    pub(crate) class_course_count: i64,
    pub(crate) course_membership_count: i64,
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

    /// How many class instances attach this catalog course.
    pub fn get_class_course_count(&self) -> i64 {
        self.class_course_count
    }

    /// How many users hold an individual (club/etüt) membership.
    pub fn get_course_membership_count(&self) -> i64 {
        self.course_membership_count
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
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
        assert!(CourseKind::course().is_class_delivered());
        assert!(!CourseKind::try_new("club").unwrap().is_class_delivered());
        assert!(!CourseKind::try_new("study").unwrap().is_class_delivered());
    }
}
