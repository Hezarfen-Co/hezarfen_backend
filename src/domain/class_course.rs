//! A course attached to a class, and the enrollments that attachment implies.
//!
//! The mirror of [`crate::domain::class_member`] along the pump's other axis:
//! attaching a course enrolls the class's whole roster into it, detaching it
//! sweeps the rows that attach wrote. The attach/detach workflows live in
//! [`crate::service::class_course`]; the row's reads in
//! [`crate::db::class_course`] — this file is the row shape and its
//! composite id.

use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::user::UserId;

/// The identity of one (class, course) pair. Not a row column: the table's
/// primary key *is* the pair, and this struct's job is the underscore-joined
/// wire form at the HTTP edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassCourseId {
    pub(crate) class: ClassGroupId,
    pub(crate) course: CourseId,
}

impl ClassCourseId {
    /// The one id a (class, course) pair can have.
    pub fn composite(class: &ClassGroupId, course: &CourseId) -> Self {
        Self {
            class: class.clone(),
            course: course.clone(),
        }
    }

    /// The underscore-joined wire form (`{class}_{course}`).
    pub fn key(&self) -> String {
        format!("{}_{}", self.class.key(), self.course.key())
    }
}

/// One course a class is attached to. `attached_by` is who attached it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClassCourse {
    pub(crate) class: ClassGroupId,
    pub(crate) course: CourseId,
    pub(crate) attached_by: UserId,
    /// The grade blueprint that placed this attachment, absent when a human
    /// attached the course to this class directly — the mirror of
    /// [`crate::domain::enrollment`]'s `source`, and read the same way: only a
    /// row carrying the key is a blueprint's to take back, so a hand-attached
    /// course survives every blueprint sweep.
    pub(crate) source: Option<ClassBlueprintId>,
}

impl ClassCourse {
    /// The row's identity, built back from its primary-key columns.
    pub fn get_id(&self) -> ClassCourseId {
        ClassCourseId::composite(&self.class, &self.course)
    }

    pub fn get_class(&self) -> &ClassGroupId {
        &self.class
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_attached_by(&self) -> &UserId {
        &self.attached_by
    }

    /// The blueprint that placed this attachment, or `None` for a hand attach.
    pub fn get_source(&self) -> Option<&ClassBlueprintId> {
        self.source.as_ref()
    }
}
