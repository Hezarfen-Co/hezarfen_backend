//! Class-course workflows: the attach that maps the pump's refusals to this
//! route's answers, the detach that turns a zero-row sweep into a 404, and the
//! instance-scoped gates every route under `/instances` pays — the archived
//! year and the D10 "who may act on this instance" rule. The transactions live
//! in [`crate::db::class_pump`] and [`crate::db::class_course`].

use crate::constant::{MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use crate::database::Database;
use crate::db::class_course;
use crate::db::class_pump::{self, Axis};
use crate::domain::academic_year::AcademicYearId;
use crate::domain::class_course::{ClassCourse, ClassCourseId, DersSaati, OverrideField};
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::{CourseDescription, CourseId, CourseTitle};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// Attach `course` to `class` and enroll the class's whole roster into it,
/// in one transaction.
///
/// Students already in the course keep the rows they have — no second
/// enrollment written, `source` untouched — and the attach is one
/// `class_course` row per (class, course): the instance every exam, session,
/// homework and enrollment under this class's course now keys on. The row
/// teaches from its grade-level offering (auto-created empty by the pump);
/// only a class-delivered ders attaches — a club or etüt is a 400 naming the
/// course.
pub async fn attach(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
    attached_by: &UserId,
) -> Result<ClassCourse, AppError> {
    let landed = class_course::attach_sourced(db, class, course, attached_by, None).await?;
    // Read off the refusal, never respelled here: this route and a
    // blueprint pump answer one vocabulary. `Made` is the only `None`, and
    // it takes the `Ok` arm below.
    let code = landed.refusal_code(&Axis::Course).unwrap_or_default();
    match landed {
        class_pump::Attached::Made(saved) => Ok(saved),
        class_pump::Attached::Duplicate => Err(AppError::ConflictCoded {
            code,
            message: "the course is already on this class".into(),
        }),
        // The class or the course: either end of the link being gone is a
        // 404 on this route, and only a blueprint's skip list needs them
        // told apart.
        class_pump::Attached::Gone | class_pump::Attached::PivotGone => Err(AppError::NotFound),
        class_pump::Attached::ClassFull => Err(AppError::ConflictCoded {
            code,
            message: format!("this class already holds {MAX_CLASS_COURSES} courses"),
        }),
        // The other axis: attaching one course enrolls the whole roster, so
        // a class over *that* ceiling cannot take a course however few it
        // carries. Only a class predating the ceiling can be here.
        class_pump::Attached::ClassOverloaded => Err(AppError::ConflictCoded {
            code,
            message: format!(
                "this class holds more than {MAX_CLASS_MEMBERS} students — \
                 remove some before attaching a course"
            ),
        }),
        // Unreachable on this axis, and kept because the match is
        // exhaustive: the pair loop enrolls the roster into `$pivot` and
        // nothing else, and the pivot claim already proved *that* course
        // alive inside the same transaction. Only a member add walks a
        // class's existing course links, so `linked_course_missing` is that
        // route's alone — which is why this route's `409` does not publish
        // it. Left mapped rather than folded into an `Internal`, so a future
        // axis change is a wrong-looking 409, not a 500.
        class_pump::Attached::CourseGone(course) => Err(AppError::ConflictCoded {
            code,
            message: format!("{course} no longer exists — detach it from this class first"),
        }),
        // This path passes no source, so the claim that answers this is
        // never in the transaction it ran.
        class_pump::Attached::SourceGone => Err(AppError::Internal(
            "a hand attach has no blueprint to lose".into(),
        )),
    }
}

/// Detach `course` from `class` and sweep the enrollments the class pumped
/// into it. A course that was not attached is a [`AppError::NotFound`],
/// raised here rather than left to each caller to re-derive from a boolean.
///
/// The instance is the anchor, so this takes everything the class taught under
/// it — exams, sessions, homework, the roster and the teacher links (see
/// [`class_pump::detach_course`]) — and answers the blob keys of the image and
/// homework-file rows that cascade removed, so the route can unlink those files
/// after the commit (the shape [`crate::service::course::delete`] already has).
/// The rows are gone by the time this returns: a caller that drops the list
/// leaves the bytes on disk with nothing pointing at them.
///
/// An empty list is *not* the refusal — an instance that carried no uploads
/// detaches fine and simply has no files to unlink. Only a pair holding no
/// instance at all is the 404.
pub async fn detach(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
) -> Result<Vec<String>, AppError> {
    class_pump::detach_course(db, class, course, None)
        .await?
        .ok_or(AppError::NotFound)
}

/// The courses a class is attached to, newest first, paged — the read behind
/// `GET /classes/{id}/instances`.
pub async fn list_for_class(
    db: &Database,
    class: &ClassGroupId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassCourse>, i64), AppError> {
    class_course::list_for_class(db, class, limit, offset).await
}

/// One instance, or `None` when the id names no row — the read behind
/// `GET /instances/{id}`.
pub async fn read(db: &Database, id: &ClassCourseId) -> Result<Option<ClassCourse>, AppError> {
    class_course::read(db, id).await
}

/// PATCH one instance's own content overrides: title, description, the
/// weekly hours and the report-card policy. Each `Some(_)` **sets** the
/// override; `None` keeps what the column holds — a PATCH never clears
/// (clearing back to inherit is [`reset_overrides`]' door, `POST
/// /instances/{id}/reset`).
pub async fn update(
    db: &Database,
    id: &ClassCourseId,
    title: Option<CourseTitle>,
    description: Option<CourseDescription>,
    staff: Option<DersSaati>,
    counts: Option<bool>,
) -> Result<ClassCourse, AppError> {
    class_course::update(db, id, title, description, staff, counts).await
}

/// The instance's **effective** weekly hours and report-card policy: its own
/// override, else the offering's default, else the constants (one weekly
/// hour, counted toward the karne). Every read of the now-nullable
/// `ders_saati`/`counts_toward_karne` goes through here — a `NULL` on the
/// instance is *inherit*, never a value, and unwrapping it would present "not
/// decided" as a decision.
///
/// One statement per call (the offering's two defaults off its row, which the
/// instance's `RESTRICT` foreign key keeps alive); the composition with the
/// full content chain (title, description, the set policies) is the Phase 3
/// resolver's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstancePolicy {
    pub ders_saati: DersSaati,
    pub counts_toward_karne: bool,
}

pub async fn resolve_policy(
    db: &Database,
    instance: &ClassCourse,
) -> Result<InstancePolicy, AppError> {
    let (default_hours, default_counts) =
        crate::db::course_offering::defaults(db, instance.get_offering())
            .await?
            .ok_or(AppError::NotFound)?;
    Ok(InstancePolicy {
        ders_saati: instance.get_ders_saati().or(default_hours).unwrap_or_else(
            || {
                DersSaati::try_new(crate::constant::MIN_DERS_SAATI)
                    .expect("the floor is a valid weekly-hours count")
            },
        ),
        counts_toward_karne: instance
            .counts_toward_karne()
            .or(default_counts)
            .unwrap_or(true),
    })
}

/// Clear the named overrides back to **inherit** — the reset behind `POST
/// /instances/{id}/reset`. The scalars go `NULL` (the offering's default,
/// then the catalog row or the constant, applies again); the three set flags
/// flip back to `TRUE` (the offering's set is authoritative again — deleting
/// the section's own set rows rides the child-table lanes' reset doors).
///
/// An empty list is a 400: a reset naming nothing would otherwise read as a
/// success that did nothing.
pub async fn reset_overrides(
    db: &Database,
    id: &ClassCourseId,
    fields: &[OverrideField],
) -> Result<ClassCourse, AppError> {
    if fields.is_empty() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "fields",
            reason: "name at least one override to reset",
        }));
    }
    class_course::clear_overrides(db, id, fields).await
}

/// Parse the reset route's field names. The set-valued policies are named by
/// their *set* (`subjects`, `exam_weights`, `weekly_plan`), not by the flag
/// column — resetting one flips the flag back to inherit.
pub fn parse_reset_field(name: &str) -> Result<OverrideField, AppError> {
    match name {
        "title" => Ok(OverrideField::Title),
        "description" => Ok(OverrideField::Description),
        "ders_saati" => Ok(OverrideField::DersSaati),
        "counts_toward_karne" => Ok(OverrideField::CountsTowardKarne),
        "subjects" => Ok(OverrideField::Subjects),
        "exam_weights" => Ok(OverrideField::ExamWeights),
        "weekly_plan" => Ok(OverrideField::WeeklyPlan),
        other => Err(ValidationError::Unknown {
            field: "fields",
            value: other.to_string(),
        }
        .into()),
    }
}

/// The academic year this instance sits under, if any — the seam every
/// instance-scoped write reads the archive gate off (instance → section →
/// year).
pub async fn year_of(
    db: &Database,
    instance: &ClassCourseId,
) -> Result<Option<AcademicYearId>, AppError> {
    let Some(class) = class_course::class_of(db, instance).await? else {
        return Ok(None);
    };
    Ok(crate::db::class_group::read(db, &class)
        .await?
        .and_then(|class| class.get_year().copied()))
}

/// Refuse the write when the instance's year is archived — past years are
/// read-only. A pre-flight guard, accepted race (see README concurrency
/// model): a year archived after this read still lets the write through.
///
/// A class section with no year, or one whose year row is gone, passes: a
/// dangling link is not this guard's error, and the caller that cares answers
/// it.
pub async fn require_open(db: &Database, instance: &ClassCourseId) -> Result<(), AppError> {
    if let Some(year) = year_of(db, instance).await? {
        crate::service::academic_year::require_open(db, &year).await?;
    }
    Ok(())
}

/// Assign `target` to teach this instance, on `by`'s orders.
///
/// The target must already hold the `teacher` role or higher — assignment
/// hands out the rights every instance-scoped gate re-checks against that bar,
/// so assigning anyone below it would write a row that can never be used —
/// and the caller `by` must hold `manager`+: staffing is the office's call,
/// stated here so it holds for every caller of this workflow and not only for
/// the route that remembers to extract it. Assignment is idempotent, and a
/// past year is read-only: staffing an archived year's instance is refused
/// like every other write against it.
pub async fn assign_teacher(
    db: &Database,
    instance: &ClassCourseId,
    target: &UserId,
    by: &UserId,
) -> Result<(), AppError> {
    let Some(actor) = crate::db::user::read(db, by).await? else {
        return Err(AppError::Unauthorized);
    };
    if !actor.get_role().at_least(Role::Manager) {
        return Err(AppError::Forbidden(
            "assigning a teacher is the office's call — manager role or higher",
        ));
    }
    let Some(target_user) = crate::db::user::read(db, target).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };
    if !target_user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user must hold the teacher role or higher",
        }));
    }
    require_open(db, instance).await?;
    crate::db::class_course_teacher::assign(db, instance, target).await?;
    Ok(())
}

/// Drop `target` from this instance's teachers: read-only in a past year, like
/// every other write against archived structure. `false` when they were not
/// assigned to it, so the web layer can answer 404 instead of pretending it
/// removed someone.
pub async fn unassign_teacher(
    db: &Database,
    instance: &ClassCourseId,
    target: &UserId,
) -> Result<bool, AppError> {
    require_open(db, instance).await?;
    crate::db::class_course_teacher::unassign(db, instance, target).await
}

/// A 403 unless `user` may act on this instance — D10, the one gate every
/// instance-scoped route shares in place of a `class_course_teacher` lookup of
/// its own. It passes when the user is `manager`+ (the office), when they are
/// one of the instance's assigned teachers, or when they are the section's
/// homeroom teacher — the three ways a person legitimately runs a section's
/// course.
///
/// A gone instance is a `404`, never a 403: the route the user asked about
/// does not exist.
pub async fn ensure_instance_teacher(
    db: &Database,
    user: &User,
    instance: &ClassCourseId,
) -> Result<(), AppError> {
    if user.get_role().at_least(Role::Manager) {
        return Ok(());
    }
    let Some(class) = class_course::class_of(db, instance).await? else {
        return Err(AppError::NotFound);
    };
    // Both ways in below are an *assignment*, and an assignment is history:
    // the caller's live role decides, exactly as it does on the catalog path
    // ([`crate::web::courses::can_manage_course`]). The demotion cascade
    // sweeps the assignment table and the homeroom column only when it runs
    // with the role write, so a flip that skipped it would otherwise leave a
    // student running the section they were stripped of.
    if user.get_role().at_least(Role::Teacher)
        && (crate::db::class_course_teacher::list_for_instance(db, instance)
            .await?
            .contains(user.get_id())
            || crate::db::class_group::read(db, &class)
                .await?
                .and_then(|class| class.get_teacher().copied())
                .as_ref()
                == Some(user.get_id()))
    {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "only this instance's teachers, its class's homeroom teacher, or a manager may act here",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::MIN_DERS_SAATI;
    use crate::db::class_group;
    use crate::db::class_member::tests::{
        a_class, a_course, counter, course_exists, link_exists, rows, source_of,
    };
    use crate::domain::user::Username;
    use crate::service::class_member;

    /// The stored user row, for the gates that judge a live role.
    async fn user_of(db: &Database, id: &UserId) -> User {
        crate::db::user::read(db, id).await.unwrap().unwrap()
    }

    /// A real account at a role — the D10 gates judge the live row, so a
    /// fixture id would not do.
    async fn staff(db: &Database, username: &str, role: Role) -> UserId {
        let account = crate::db::user::create(db, Username::try_new(username).unwrap(), None)
            .await
            .unwrap();
        crate::service::user::set_role(db, account.get_id(), role)
            .await
            .unwrap();
        *account.get_id()
    }

    /// Attaching mints the instance, seeds it from the roster the class already
    /// holds, and refuses a second attach of the same pair.
    #[tokio::test]
    async fn an_attach_enrolls_the_whole_roster_into_the_new_instance() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let students = [
            crate::db::class_member::tests::fixture_user(&db, "a").await,
            crate::db::class_member::tests::fixture_user(&db, "b").await,
        ];
        for student in &students {
            class_member::add(&db, &class, student, &manager)
                .await
                .unwrap();
        }

        let instance = attach(&db, &class, &algebra, &manager).await.unwrap();
        for student in &students {
            assert_eq!(
                source_of(instance.get_id(), student, &db).await,
                Some(Some(class.clone()))
            );
        }
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            2
        );
        assert_eq!(counter("class_course_count", class.uuid(), &db).await, 1);

        let again = attach(&db, &class, &algebra, &manager).await;
        assert!(
            matches!(again, Err(AppError::ConflictCoded { code, .. }) if code == "duplicate"),
            "a second attach is a 409 coded `duplicate`: {again:?}"
        );
    }

    /// Two class sections teaching one catalog course are two instances — that
    /// is the whole remodel — and each keeps its own roster.
    #[tokio::test]
    async fn two_classes_teaching_one_course_keep_their_own_rosters() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let algebra = a_course("algebra", &db).await;
        let fifth_a = a_class("5-A", &db).await;
        let fifth_b = a_class("5-B", &db).await;

        let first = attach(&db, &fifth_a, &algebra, &manager).await.unwrap();
        let second = attach(&db, &fifth_b, &algebra, &manager).await.unwrap();
        assert_ne!(
            first.get_id(),
            second.get_id(),
            "one catalog course, one instance per şube"
        );

        class_member::add(&db, &fifth_a, &student, &manager)
            .await
            .unwrap();
        assert_eq!(
            source_of(first.get_id(), &student, &db).await,
            Some(Some(fifth_a.clone())),
            "5-A's student lands in 5-A's instance"
        );
        assert_eq!(
            source_of(second.get_id(), &student, &db).await,
            None,
            "…and nowhere near 5-B's"
        );

        // Detaching one class section takes its instance and its roster; the
        // other section's instance and roster stand.
        detach(&db, &fifth_a, &algebra).await.unwrap();
        assert_eq!(source_of(first.get_id(), &student, &db).await, None);
        assert_eq!(
            source_of(second.get_id(), &student, &db).await,
            None,
            "5-B's instance was never enrolled either"
        );
        assert_eq!(
            counter("class_course_count", fifth_b.uuid(), &db).await,
            1,
            "the other şube keeps its instance"
        );
        assert_eq!(
            counter("class_member_count", fifth_a.uuid(), &db).await,
            1,
            "the two axes are independent: dropping the course keeps the roster"
        );
    }

    /// A hand-placed row is never adopted: the pump skips it, and a member
    /// exit leaves it standing because it carries no class `source`. The
    /// detach is the other story: it takes the whole instance subtree, and a
    /// hand row lives on that instance like any other.
    #[tokio::test]
    async fn a_hand_placed_row_is_never_adopted_by_the_class() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = attach(&db, &class, &algebra, &manager).await.unwrap();
        crate::db::enrollment::enroll(&db, instance.get_id(), &student, &manager, None)
            .await
            .unwrap();
        class_member::add(&db, &class, &student, &manager)
            .await
            .unwrap();

        assert_eq!(
            source_of(instance.get_id(), &student, &db).await,
            Some(None),
            "the pump may not adopt a hand-placed row"
        );
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            1,
            "…nor count it twice"
        );

        class_member::leave(&db, &class, &student).await.unwrap();
        assert_eq!(
            source_of(instance.get_id(), &student, &db).await,
            Some(None),
            "the member sweep may only take back the rows the class wrote"
        );
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            1
        );

        detach(&db, &class, &algebra).await.unwrap();
        assert_eq!(
            source_of(instance.get_id(), &student, &db).await,
            None,
            "the detach takes the instance and every row on it"
        );
        let again = detach(&db, &class, &algebra).await;
        assert!(
            matches!(again, Err(AppError::NotFound)),
            "a second detach is a 404: {again:?}"
        );
    }

    /// Detaching sweeps the rows the class pumped, and only those: the
    /// membership row is a separate axis and stays.
    #[tokio::test]
    async fn a_detach_sweeps_the_rows_it_pumped() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "student").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        class_member::add(&db, &class, &student, &manager)
            .await
            .unwrap();
        let instance = attach(&db, &class, &algebra, &manager).await.unwrap();

        detach(&db, &class, &algebra).await.unwrap();
        assert_eq!(source_of(instance.get_id(), &student, &db).await, None);
        assert_eq!(rows("enrollment", &db).await, 0);
        assert_eq!(counter("class_course_count", class.uuid(), &db).await, 0);
        assert_eq!(
            rows("class_member", &db).await,
            1,
            "the two axes are independent: dropping the course may not drop \
             the student out of the class as well"
        );
    }

    /// The attach of an empty class leaves the new instance's counter at zero —
    /// nothing to enroll, nothing charged.
    #[tokio::test]
    async fn the_attach_of_an_empty_roster_charges_no_seat() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = attach(&db, &class, &algebra, &manager).await.unwrap();
        assert_eq!(
            counter("enrollment_count", instance.get_id().uuid(), &db).await,
            0,
            "a fresh instance's roster counter is zero"
        );
    }

    /// PATCHing the instance writes only what the request carried, the reset
    /// door clears a named override back to inherit, and the resolver layers
    /// override → offering default → constant.
    #[tokio::test]
    async fn an_update_writes_only_the_fields_it_carried() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;
        let class = a_class("9-A", &db).await;
        let algebra = a_course("algebra", &db).await;
        let instance = attach(&db, &class, &algebra, &manager).await.unwrap();
        // A fresh instance carries no overrides: the resolver falls through
        // the (empty, auto-created) offering to the constants.
        let fresh = read(&db, instance.get_id()).await.unwrap().unwrap();
        assert_eq!(fresh.get_ders_saati(), None);
        let fresh_policy = resolve_policy(&db, &fresh).await.unwrap();
        assert_eq!(fresh_policy.ders_saati.as_i64(), MIN_DERS_SAATI);
        assert!(fresh_policy.counts_toward_karne);

        let updated = update(
            &db,
            instance.get_id(),
            Some(CourseTitle::try_new("9-A Matematik").unwrap()),
            None,
            Some(DersSaati::try_new(5).unwrap()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            updated.get_title().map(CourseTitle::as_str),
            Some("9-A Matematik")
        );
        assert_eq!(updated.get_ders_saati().map(DersSaati::as_i64), Some(5));
        assert_eq!(
            updated.counts_toward_karne(),
            None,
            "the omitted field keeps its stored value"
        );
        // The effective hours follow the override, not the constant.
        let policy = resolve_policy(&db, &updated).await.unwrap();
        assert_eq!(policy.ders_saati.as_i64(), 5);

        let again = update(
            &db,
            instance.get_id(),
            None,
            None,
            None,
            Some(false),
        )
        .await
        .unwrap();
        assert_eq!(again.counts_toward_karne(), Some(false));
        assert_eq!(
            again.get_ders_saati().map(DersSaati::as_i64),
            Some(5),
            "…and the other direction holds too"
        );

        // Reset: the named overrides go back to inherit, the unnamed (the
        // title) stay.
        let cleared = reset_overrides(
            &db,
            instance.get_id(),
            &[OverrideField::DersSaati, OverrideField::CountsTowardKarne],
        )
        .await
        .unwrap();
        assert_eq!(cleared.get_ders_saati(), None);
        assert_eq!(cleared.counts_toward_karne(), None);
        assert_eq!(
            cleared.get_title().map(CourseTitle::as_str),
            Some("9-A Matematik"),
            "the reset only touches the fields it names"
        );
        let cleared_policy = resolve_policy(&db, &cleared).await.unwrap();
        assert_eq!(cleared_policy.ders_saati.as_i64(), MIN_DERS_SAATI);
        assert!(cleared_policy.counts_toward_karne);
        assert!(
            reset_overrides(&db, instance.get_id(), &[]).await.is_err(),
            "a reset naming nothing is a 400, not a no-op success"
        );
    }

    /// D10: the instance's own teacher, the section's homeroom teacher and a
    /// manager may act on it; another instance's teacher and a plain student
    /// may not. Assignment itself is the office's call and the assignee must
    /// hold the teacher bar.
    #[tokio::test]
    async fn the_instance_gate_admits_its_teachers_homeroom_and_managers() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = staff(&db, "mudur", Role::Manager).await;
        let assigned = staff(&db, "ayse", Role::Teacher).await;
        let outsider = staff(&db, "mehmet", Role::Teacher).await;
        let homeroom = staff(&db, "zeynep", Role::Teacher).await;
        let student = crate::db::class_member::tests::fixture_user(&db, "ogrenci").await;
        let algebra = a_course("algebra", &db).await;
        let class = class_group::create(
            &db,
            &manager,
            crate::domain::class_group::ClassName::try_new("9-A").unwrap(),
            crate::domain::grade::GradeLevel::new(9).unwrap(),
            None,
            Some(homeroom),
        )
        .await
        .unwrap();
        let instance = attach(&db, &class.get_id().clone(), &algebra, &manager)
            .await
            .unwrap();

        assign_teacher(&db, instance.get_id(), &assigned, &manager)
            .await
            .unwrap();
        // Idempotent, like every other assignment.
        assign_teacher(&db, instance.get_id(), &assigned, &manager)
            .await
            .unwrap();

        assert!(
            ensure_instance_teacher(&db, &user_of(&db, &manager).await, instance.get_id())
                .await
                .is_ok()
        );
        assert!(
            ensure_instance_teacher(&db, &user_of(&db, &assigned).await, instance.get_id())
                .await
                .is_ok()
        );
        assert!(
            ensure_instance_teacher(&db, &user_of(&db, &homeroom).await, instance.get_id())
                .await
                .is_ok()
        );
        assert!(
            ensure_instance_teacher(&db, &user_of(&db, &outsider).await, instance.get_id())
                .await
                .is_err(),
            "a teacher of another instance is not this one's"
        );
        assert!(
            ensure_instance_teacher(&db, &user_of(&db, &student).await, instance.get_id())
                .await
                .is_err()
        );

        // A student cannot be made an instance's teacher, and a non-manager
        // cannot assign one.
        let refused = assign_teacher(&db, instance.get_id(), &student, &manager).await;
        assert!(
            matches!(refused, Err(AppError::Validation(_))),
            "a student is not a teacher: {refused:?}"
        );
        let refused = assign_teacher(&db, instance.get_id(), &assigned, &outsider).await;
        assert!(
            matches!(refused, Err(AppError::Forbidden(_))),
            "staffing is the office's call: {refused:?}"
        );
    }

    /// The `Menu::delete` defect on the class layer: a `class_course` row must
    /// not outlive the course it names. The attach's proof that the course is
    /// still there lands on the very row `Course::delete`'s guard locks, so
    /// whatever the interleaving, a link row and a deleted course can never
    /// both stand.
    ///
    /// Both orders are forced. Delete-first awaits the delete, then the
    /// attach: the course is gone and no link may appear. Attach-first awaits
    /// the attach, then the delete: a course a class still teaches must
    /// refuse, not 500, and the course stays. A 2ms head start does not force
    /// that under pool contention. Overlapped rounds may all lose the delete;
    /// that is not a failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_class_course_link_never_outlives_the_course() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;

        // Delete-first: nothing may attach to a course that is already gone.
        {
            let class = a_class("9-df", &db).await;
            let algebra = a_course("algebra-df", &db).await;
            let course = crate::db::course::read(&db, &algebra)
                .await
                .unwrap()
                .unwrap();
            let dropped = crate::db::course::delete(&db, course).await;
            assert!(
                matches!(dropped, Ok(true)),
                "delete-first: an empty course must delete, not {dropped:?}"
            );
            assert!(
                !course_exists(&algebra, &db).await,
                "delete-first: the course is still there"
            );
            let child = attach(&db, &class, &algebra, &manager).await;
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "delete-first: an attach must be answered, not 500: {child:?}"
            );
            assert!(
                !link_exists(&class, &algebra, &db).await,
                "a class_course link outlived its course"
            );
            assert_eq!(
                counter("class_course_count", class.uuid(), &db).await,
                0,
                "a class counts a course that is gone"
            );
        }

        // Attach-first: the guard must refuse. Parent stays; that is the
        // verdict, not a missed sweep.
        {
            let class = a_class("9-wf", &db).await;
            let algebra = a_course("algebra-wf", &db).await;
            let course = crate::db::course::read(&db, &algebra)
                .await
                .unwrap()
                .unwrap();
            let child = attach(&db, &class, &algebra, &manager).await;
            assert!(
                child.is_ok(),
                "attach-first: the attach must land before the delete: {child:?}"
            );
            let dropped = crate::db::course::delete(&db, course).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "attach-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                matches!(dropped, Ok(false)),
                "attach-first: a course a class still teaches must refuse, not {dropped:?}"
            );
            assert!(
                course_exists(&algebra, &db).await,
                "attach-first: the refused delete removed the course"
            );
            assert!(
                link_exists(&class, &algebra, &db).await,
                "attach-first: the landed link is gone"
            );
        }

        // Overlapped rounds. Delete may lose every one of them.
        let mut swept = 0;
        for round in 0..4 {
            let class = a_class(&format!("9-{round}"), &db).await;
            let algebra = a_course(&format!("algebra{round}"), &db).await;
            let course = crate::db::course::read(&db, &algebra)
                .await
                .unwrap()
                .unwrap();

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, gate, course) = (db.clone(), gate.clone(), course);
                tokio::spawn(async move {
                    gate.wait().await;
                    crate::db::course::delete(&db, course).await
                })
            };
            let child = {
                let (db, class, algebra, manager, gate) =
                    (db.clone(), class.clone(), algebra.clone(), manager, gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    attach(&db, &class, &algebra, &manager).await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced attach must be answered, not 500: {child:?}"
            );
            assert!(
                !matches!(drop_it, Err(AppError::Db(_))),
                "round {round}: a raced delete must be answered, not 500: {drop_it:?}"
            );

            if !course_exists(&algebra, &db).await {
                swept += 1;
                assert!(
                    !link_exists(&class, &algebra, &db).await,
                    "round {round}: a class_course link outlived its course"
                );
                assert_eq!(
                    counter("class_course_count", class.uuid(), &db).await,
                    0,
                    "round {round}: a class counts a course that is gone"
                );
            } else if matches!(drop_it, Ok(true)) {
                panic!("round {round}: the delete reported success but the course is still there");
            }
        }
        eprintln!(
            "Course::delete raced by an attach: {swept}/4 concurrent rounds deleted the course"
        );
    }
}
