//! The grade-level course **template**: one `course_offering` row per
//! (course × grade_level), the content every section at that grade teaches
//! from — title, description, the weekly-hours and report-card defaults.
//!
//! A course stays the school-wide catalog row and the identity of the ders;
//! the offering is the grade axis' slice of it. Every
//! [`crate::domain::class_course::ClassCourse`] instance points at exactly one
//! offering (its `offering` link) and may override any content field on the
//! instance row; **unset = inherit** from the offering, and the offering's
//! unset = fall back to the catalog row / the constants (1 weekly hour,
//! counted toward the karne). Override-or-inherit, never merge.
//!
//! The CRUD workflows live in [`crate::service::course_offering`], the SQL in
//! [`crate::db::course_offering`]; this file is the row shape and its id.

use crate::domain::class_course::DersSaati;
use crate::domain::course::{CourseDescription, CourseId, CourseTitle};
use crate::domain::grade::GradeLevel;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

/// The identity of one course×grade offering. A UUIDv7 minted by the
/// process-wide monotonic generator, like every other row id in the repo.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct CourseOfferingId(uuid::Uuid);

impl CourseOfferingId {
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

/// One course as one grade teaches it — the template a class section's
/// instance inherits from. Every content field is nullable: a `NULL` is not
/// "no value" but "inherit from the catalog course" (`title`, `description`)
/// or "use the constant" (`default_ders_saati` → 1,
/// `default_counts_toward_karne` → `TRUE`). An offering minted by the attach
/// pump starts with all of them unset, so an untouched grade teaches exactly
/// what the catalog row says.
///
/// `created_by` is who minted the row — the manager that created it on
/// purpose, or the actor whose attach auto-created it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CourseOffering {
    pub(crate) id: CourseOfferingId,
    pub(crate) course: CourseId,
    pub(crate) grade_level: GradeLevel,
    pub(crate) title: Option<CourseTitle>,
    pub(crate) description: Option<CourseDescription>,
    pub(crate) default_ders_saati: Option<DersSaati>,
    pub(crate) default_counts_toward_karne: Option<bool>,
    pub(crate) created_by: UserId,
    pub(crate) created_at: Timestamp,
    pub(crate) updated_at: Timestamp,
}

impl CourseOffering {
    pub fn get_id(&self) -> &CourseOfferingId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_grade_level(&self) -> GradeLevel {
        self.grade_level
    }

    /// The offering's own title override, `None` = the catalog course's title.
    pub fn get_title(&self) -> Option<&CourseTitle> {
        self.title.as_ref()
    }

    /// The offering's own description override, `None` = the catalog's.
    pub fn get_description(&self) -> Option<&CourseDescription> {
        self.description.as_ref()
    }

    /// The grade's default weekly hours, `None` = the constant floor (1).
    pub fn get_default_ders_saati(&self) -> Option<DersSaati> {
        self.default_ders_saati
    }

    /// The grade's default report-card policy, `None` = counted (`TRUE`).
    pub fn get_default_counts_toward_karne(&self) -> Option<bool> {
        self.default_counts_toward_karne
    }

    pub fn get_created_by(&self) -> &UserId {
        &self.created_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    pub fn get_updated_at(&self) -> Timestamp {
        self.updated_at
    }
}
