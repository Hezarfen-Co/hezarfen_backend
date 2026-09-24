//! A course as taught in one class: the academic *instance*.
//!
//! The anchor of the K12 model. A course in the school catalog is a template;
//! attaching it to a class section mints one of these rows, and exams, sessions,
//! roll-call, homework and enrollments all key on the instance — so two
//! sections teaching the same course have separate rosters, exams, timetables
//! and teachers.
//!
//! The row teaches **from its offering** — the
//! [`crate::domain::course_offering::CourseOffering`] template for the
//! course × the class's grade level, resolved (and auto-created empty) by the
//! attach pump. Every content field on the instance is an *override*: `title`,
//! `description`, `ders_saati` and `counts_toward_karne` are nullable, and a
//! `NULL` means inherit — from the offering, and through it from the catalog
//! row / the constants (1 weekly hour, counted toward the karne). The three
//! `*_inherited` flags carry the same rule for the set-valued policies
//! (subjects, exam weights, weekly plan), which cannot be `NULL`: `TRUE` =
//! follow the offering's set, `FALSE` = this section's own table is
//! authoritative, including when it is empty. Override-or-inherit, never
//! merge.
//!
//! The attach/detach workflows live in [`crate::service::class_course`] (which
//! also owns [`crate::service::class_course::resolve_policy`], the effective
//! `ders_saati`/`counts_toward_karne` a read should use); the row's reads in
//! [`crate::db::class_course`] — this file is the row shape and its id.

use crate::constant::{MAX_DERS_SAATI, MIN_DERS_SAATI};
use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::{CourseDescription, CourseId, CourseTitle};
use crate::domain::course_offering::CourseOfferingId;
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
/// As a *column* it is the instance's own override — `NULL` inherits from the
/// offering's `default_ders_saati`, and the offering's `NULL` means the
/// constant floor ([`crate::constant::MIN_DERS_SAATI`]).
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

    pub fn as_i64(self) -> i64 {
        self.0
    }
}

/// A named instance override the reset door can clear back to inherit. The
/// scalars go `NULL` (the offering's default — then the catalog row or the
/// constant — applies again); the three set flags flip back to `TRUE` (the
/// offering's set is authoritative again). The wire spellings live with the
/// reset route's parser in [`crate::service::class_course`]; this enum is the
/// closed set both sides answer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideField {
    /// The section's own title.
    Title,
    /// The section's own description.
    Description,
    /// The section's own weekly hours.
    DersSaati,
    /// The section's own report-card policy.
    CountsTowardKarne,
    /// The section's own subject set (the flag; set-row deletion rides the
    /// subject lane's reset).
    Subjects,
    /// The section's own exam-kind weights (the flag).
    ExamWeights,
    /// The section's own weekly plan (the flag).
    WeeklyPlan,
}

/// One course taught in one class. `attached_by` is who attached it, `source`
/// the blueprint that placed it (absent for a hand attach). The content
/// fields are the instance's own *overrides* — `None` inherits from the
/// offering ([`Self::get_offering`]), which is what two sections teaching the
/// same catalog course at the same grade usually want to differ from only
/// deliberately.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClassCourse {
    pub(crate) id: ClassCourseId,
    pub(crate) class: ClassGroupId,
    pub(crate) course: CourseId,
    /// The grade-level template this instance teaches from — always set: the
    /// attach pump resolves it (auto-creating the empty template when the
    /// grade is untouched) in the same transaction as the insert.
    pub(crate) offering: CourseOfferingId,
    pub(crate) attached_by: UserId,
    /// The grade blueprint that placed this attachment, absent when a human
    /// attached the course to this class directly — read the same way as
    /// [`crate::domain::enrollment`]'s `source`: only a row carrying the key
    /// is a blueprint's to take back, so a hand-attached course survives every
    /// blueprint sweep.
    pub(crate) source: Option<ClassBlueprintId>,
    pub(crate) title: Option<CourseTitle>,
    pub(crate) description: Option<CourseDescription>,
    pub(crate) ders_saati: Option<DersSaati>,
    pub(crate) counts_toward_karne: Option<bool>,
    pub(crate) subjects_inherited: bool,
    pub(crate) exam_weights_inherited: bool,
    pub(crate) weekly_plan_inherited: bool,
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

    /// The grade-level template this instance teaches from. The effective
    /// values the instance inherits ride
    /// [`crate::service::class_course::resolve_policy`].
    pub fn get_offering(&self) -> &CourseOfferingId {
        &self.offering
    }

    pub fn get_attached_by(&self) -> &UserId {
        &self.attached_by
    }

    /// The blueprint that placed this attachment, or `None` for a hand attach.
    pub fn get_source(&self) -> Option<&ClassBlueprintId> {
        self.source.as_ref()
    }

    /// The instance's own title override, `None` = inherit.
    pub fn get_title(&self) -> Option<&CourseTitle> {
        self.title.as_ref()
    }

    /// The instance's own description override, `None` = inherit.
    pub fn get_description(&self) -> Option<&CourseDescription> {
        self.description.as_ref()
    }

    /// The instance's own weekly-hours override, `None` = inherit (the
    /// offering's default, else 1). The *effective* value — what a read
    /// should present — is [`crate::service::class_course::resolve_policy`]'s
    /// answer, not this one.
    pub fn get_ders_saati(&self) -> Option<DersSaati> {
        self.ders_saati
    }

    /// The instance's own report-card policy override, `None` = inherit (the
    /// offering's default, else counted).
    pub fn counts_toward_karne(&self) -> Option<bool> {
        self.counts_toward_karne
    }

    /// Whether this section follows the offering's subject set (`TRUE`) or
    /// keeps its own `class_course_subject` table authoritative (`FALSE`,
    /// including when that table is empty — that is how a section clears its
    /// syllabus).
    pub fn subjects_inherited(&self) -> bool {
        self.subjects_inherited
    }

    /// The same override switch for the section's exam-kind weights.
    pub fn exam_weights_inherited(&self) -> bool {
        self.exam_weights_inherited
    }

    /// The same override switch for the section's weekly plan.
    pub fn weekly_plan_inherited(&self) -> bool {
        self.weekly_plan_inherited
    }

    pub fn get_enrollment_count(&self) -> i64 {
        self.enrollment_count
    }

    pub fn get_attached_at(&self) -> Timestamp {
        self.attached_at
    }
}
