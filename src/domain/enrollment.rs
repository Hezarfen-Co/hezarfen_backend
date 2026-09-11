use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::ENROLLMENT_TABLE;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EnrollmentId(RecordId);

impl EnrollmentId {
    /// A deterministic id for the (course, user) pair. The same pair always
    /// maps to the same record id, so enrolling is a single atomic UPSERT with
    /// no find-then-insert race and one-row-per-pair by construction. ULID keys
    /// are alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(course: &CourseId, user: &UserId) -> Self {
        Self(RecordId::new(
            ENROLLMENT_TABLE,
            format!("{}_{}", course.key(), user.key()),
        ))
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

/// A user's membership in a course. Grading requires it; removing it hides the
/// user's marks from the report but never deletes result rows.
///
/// `source` names the class ([`crate::domain::class_group::ClassGroup`]) that
/// pumped this row, and is absent when a human placed the student directly —
/// every row written before classes existed, and every row placed by hand since.
/// Absence *is* the meaning, so nothing backfills it: a class sweep may only take
/// back the rows it wrote.
#[derive(Debug, Clone, SurrealValue)]
pub struct Enrollment {
    pub(crate) id: EnrollmentId,
    pub(crate) course: CourseId,
    pub(crate) user: UserId,
    pub(crate) enrolled_by: UserId,
    pub(crate) source: Option<ClassGroupId>,
}

impl Enrollment {
    pub fn get_id(&self) -> &EnrollmentId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_enrolled_by(&self) -> &UserId {
        &self.enrolled_by
    }

    /// The class that pumped this row, or `None` for a hand-placed one.
    pub fn get_source(&self) -> Option<&ClassGroupId> {
        self.source.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The database strips `NONE`-valued optional columns, and every enrollment
    /// row written before classes existed has no `source` key at all — both must
    /// decode as a hand-placed row, never as a decode error that 500s a roster.
    #[tokio::test]
    async fn enrollment_decodes_without_source_key() {
        use surrealdb::types::Value;

        let class = crate::domain::class_group::ClassGroupId::from_key("9a");
        let course = CourseId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T");
        let student = UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8U");
        let enrollment = Enrollment {
            id: EnrollmentId::composite(&course, &student),
            course,
            user: student,
            enrolled_by: UserId::from_key("mgr"),
            source: Some(class.clone()),
        };
        assert_eq!(enrollment.get_source(), Some(&class));

        let Value::Object(mut object) = enrollment.into_value() else {
            panic!("an enrollment must encode as an object");
        };
        object.remove("source");
        let decoded = Enrollment::from_value(Value::Object(object)).unwrap();
        assert_eq!(decoded.get_source(), None);
    }
}
