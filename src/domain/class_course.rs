//! A course attached to a class, and the enrollments that attachment implies.
//!
//! The mirror of [`crate::domain::class_member`] along the pump's other axis:
//! attaching a course enrolls the class's whole roster into it, detaching it
//! sweeps the rows that attach wrote. The attach/detach workflows live in
//! [`crate::service::class_course`]; the row's reads in
//! [`crate::db::class_course`] — this file is the row shape and its
//! composite id.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::CLASS_COURSE_TABLE;
use crate::db::class_pump::link_id;
use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassCourseId(RecordId);

impl ClassCourseId {
    /// The record one (class, course) pair always maps to.
    pub fn composite(class: &ClassGroupId, course: &CourseId) -> Self {
        Self(link_id(CLASS_COURSE_TABLE, class, course.key()))
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

/// One course a class is attached to. `attached_by` is who attached it.
#[derive(Debug, Clone, SurrealValue)]
pub struct ClassCourse {
    pub(crate) id: ClassCourseId,
    pub(crate) class: ClassGroupId,
    pub(crate) course: CourseId,
    pub(crate) attached_by: UserId,
    /// The grade blueprint that placed this attachment, absent when a human
    /// attached the course to this class directly — the mirror of
    /// [`crate::domain::enrollment`]'s `source`, and read the same way: only a
    /// row carrying the key is a blueprint's to take back, so a hand-attached
    /// course survives every blueprint sweep. Rows written before the column
    /// carry no key at all, which is exactly "hand-attached".
    pub(crate) source: Option<ClassBlueprintId>,
    /// When it was attached, and the *only* thing "newest first" can mean here:
    /// the row's id is the (class, course) pair, so ordering by it sorts the
    /// list by the course's own ULID. Optional because rows written before this
    /// column carry no stamp — see the migration note.
    pub(crate) attached_at: Option<Timestamp>,
}

impl ClassCourse {
    pub fn get_id(&self) -> &ClassCourseId {
        &self.id
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
