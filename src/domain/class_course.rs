//! A course as taught in one class: the academic *instance*.
//!
//! The anchor of the K12 model. A course in the school catalog is a template;
//! attaching it to a class section mints one of these rows, and exams, sessions,
//! roll-call, homework and enrollments all key on the instance — so two
//! sections teaching the same course have separate rosters, exams, timetables
//! and teachers. The row carries the weekly hours (`ders_saati`, the report-card
//! weight) and whether it counts toward the report card.
//!
//! The attach/detach workflows live in [`crate::service::class_course`]; the
//! row's reads in [`crate::db::class_course`] — this file is the row shape and
//! its id.

use crate::constant::{MAX_DERS_SAATI, MIN_DERS_SAATI};
use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;

/// The identity of one class×course instance. A UUIDv7 minted by the
/// process-wide monotonic generator, so a class's instances list in creation
/// order.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ClassCourseId(uuid::Uuid);

impl ClassCourseId {
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

    /// The bare uuid wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// Weekly lesson hours of an instance: the weight the instance carries in the
/// year's report-card average, and the count a timetable grid would lay out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct DersSaati(i64);

impl DersSaati {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        if !(MIN_DERS_SAATI..=MAX_DERS_SAATI).contains(&value) {
            return Err(ValidationError::Invalid {
                field: "ders_saati",
                reason: "must be between 1 and 40 weekly hours",
            });
        }
        Ok(Self(value))
    }

    pub fn as_i64(&self) -> i64 {
        self.0
    }
}

/// One course taught in one class. `attached_by` is who attached it, `source`
/// the blueprint that placed it (absent for a hand attach). `ders_saati` and
/// `counts_toward_karne` are the instance's own policy — what two sections
/// teaching the same catalog course may legitimately differ on.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClassCourse {
    pub(crate) id: ClassCourseId,
    pub(crate) class: ClassGroupId,
    pub(crate) course: CourseId,
    pub(crate) attached_by: UserId,
    /// The grade blueprint that placed this attachment, absent when a human
    /// attached the course to this class directly — read the same way as
    /// [`crate::domain::enrollment`]'s `source`: only a row carrying the key
    /// is a blueprint's to take back, so a hand-attached course survives every
    /// blueprint sweep.
    pub(crate) source: Option<ClassBlueprintId>,
    pub(crate) ders_saati: DersSaati,
    pub(crate) counts_toward_karne: bool,
    pub(crate) enrollment_count: i64,
    pub(crate) attached_at: Timestamp,
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

    pub fn get_ders_saati(&self) -> DersSaati {
        self.ders_saati
    }

    pub fn counts_toward_karne(&self) -> bool {
        self.counts_toward_karne
    }

    pub fn get_enrollment_count(&self) -> i64 {
        self.enrollment_count
    }

    pub fn get_attached_at(&self) -> Timestamp {
        self.attached_at
    }
}
