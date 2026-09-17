//! The **instance**: one catalog course as taught in one şube — the academic
//! anchor of the K12 model.
//!
//! Everything a class actually runs hangs off this row: its roster, its
//! teachers, its exams, its lessons and its homework. Two şubeler teaching the
//! same course are two instances and share none of it. The weekly hours
//! (`ders_saati`) and the karne policy (`counts_toward_karne`) are the
//! instance's own, so two sections may legitimately differ.
//!
//! The catalog CRUD stays in [`super::courses`]; the exam, session and
//! homework sub-routers live *here* now (they used to hang off `/courses/{id}`)
//! and are split out per module so each still carries its own gate — see
//! [`crate::web::module_gate`].

use std::collections::HashMap;

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::class_course::{ClassCourse, ClassCourseId, DersSaati};
use crate::domain::course_session::SessionTopic;
use crate::domain::enrollment::Enrollment;
use crate::domain::exam::{
    ExamAttemptLimit, ExamDescription, ExamDuration, ExamKind, ExamMode, ExamSchedule, ExamTitle,
};
use crate::domain::homework::HomeworkTitle;
use crate::domain::term::TermId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::service::instance::{can_manage_instance, visible_instances};
use crate::state::AppState;

use super::homework::{description_or_none, resolve_assigned};
use super::{
    CurrentUser, ExamResponse, HomeworkResponse, Page, PageParams, PersonRef, RequireManager,
    RequireTeacher, SessionResponse, check_not_past, check_time_range, paginate, person_map,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(my_instances))
        .routes(routes!(get_instance, update_instance))
        .routes(routes!(assign_teacher))
        .routes(routes!(unassign_teacher))
        .routes(routes!(enroll, list_roster))
        .routes(routes!(unenroll))
}

// The three route pairs below belong to another module, so each is split out
// to carry that module's gate as well as the instance one — see
// `crate::web::module_gate`. They are merged back in `build_router`, so the
// URL space is unchanged.

pub fn exam_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_exam_in_instance, list_instance_exams))
}

pub fn session_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_session_in_instance, list_instance_sessions))
}

pub fn homework_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_homework_in_instance, list_instance_homework))
}

#[derive(Deserialize, ToSchema)]
struct UpdateInstance {
    /// Weekly lesson hours: the instance's weight in the year's karne average.
    /// Omit to keep the stored value; at least 1 and at most 40.
    #[schema(minimum = 1, maximum = 40, example = 5)]
    ders_saati: Option<i64>,
    /// Whether this instance's marks count into the karne. Omit to keep the
    /// stored value.
    counts_toward_karne: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
struct AssignTeacher {
    /// The staff member to put in charge of this instance. Must hold the
    /// `teacher` role or higher.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    user_id: String,
}

#[derive(Deserialize, ToSchema)]
struct EnrollUser {
    /// The user to enroll.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    user_id: String,
}

#[derive(Deserialize, ToSchema)]
struct CreateExamInCourse {
    #[schema(max_length = 200, example = "Midterm")]
    title: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// The assessment form — one of the school's exam kinds (`GET /settings`;
    /// defaults: `yazili`, `sozlu`, `uygulama`). The kind's
    /// settings-configured weight decides how heavily the exam counts into the
    /// instance's average.
    #[schema(max_length = 50, example = "yazili")]
    kind: String,
    /// The dönem the exam is sat in (`GET /terms`). Required, and it must be a
    /// dönem of this instance's şube's academic year — that is the slice the
    /// exam's marks are counted into.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    term: String,
    /// `sync` (one fixed window for everyone), `async` (each student starts
    /// inside the window and gets `duration_ms`), or `open` (no window — sit
    /// anytime). Omit for an offline-graded exam that cannot be sat.
    #[schema(example = "sync")]
    mode: Option<String>,
    /// Window open, UTC unix-milliseconds (`sync`/`async`). Must not be in the
    /// past.
    #[schema(example = 1_900_000_000_000_i64)]
    starts_at: Option<i64>,
    /// Window close, UTC unix-milliseconds (`sync`/`async`). Must not precede
    /// `starts_at`.
    #[schema(example = 1_900_000_360_000_i64)]
    ends_at: Option<i64>,
    /// Per-attempt time budget in milliseconds. Required for `async`,
    /// optional for `open`; must fit inside the window when there is one.
    #[schema(minimum = 60_000, maximum = 86_400_000, example = 3_600_000_i64)]
    duration_ms: Option<i64>,
    /// How many attempts each student gets. Defaults to 1; `0` means
    /// unlimited.
    #[schema(minimum = 0, maximum = 100, example = 1)]
    max_attempts: Option<i64>,
    /// Whether a student who left the exam room may come back in and keep
    /// answering. Defaults to `true`.
    allow_rejoin: Option<bool>,
    /// Whether students may review their graded attempt once results are out.
    /// Defaults to `false`.
    allow_review: Option<bool>,
    /// Keep the exam private while it is still being written. Defaults to
    /// `false` (published).
    draft: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
struct CreateHomework {
    #[schema(max_length = 200, example = "Read chapter 3 and answer Q1-Q5")]
    title: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// The course subject this homework belongs to
    /// (`GET /courses/{id}/subjects`). Required — every homework is tagged with
    /// one of its course's subjects.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    subject_id: String,
    /// When the homework is due, UTC unix-milliseconds. Required; must not be
    /// in the past. Late submissions are still accepted, just flagged late.
    #[schema(example = 1_900_000_000_000_i64)]
    due_at: i64,
    /// The students this homework is for: a list of enrolled student ids. Omit,
    /// send `null`, or send `[]` to assign the whole enrolled roster (whoever
    /// is enrolled when they submit); a subset caps at 200 named students.
    #[schema(max_items = 200)]
    assigned: Option<Vec<String>>,
}

#[derive(Deserialize, ToSchema)]
struct CreateSessionInCourse {
    /// What the lesson covers. Optional.
    #[schema(max_length = 200, example = "Limits and continuity")]
    topic: Option<String>,
    /// Who teaches the session. Defaults to the caller; must hold the
    /// `teacher` role or higher.
    teacher_id: Option<String>,
    /// Lesson start, UTC unix-milliseconds. Must not be in the past.
    #[schema(example = 1_900_000_000_000_i64)]
    starts_at: i64,
    /// Lesson end, UTC unix-milliseconds. Optional (open-ended); must not be
    /// in the past.
    ends_at: Option<i64>,
}

/// Public shape of one instance: the course as this şube teaches it, with the
/// policy that is the instance's own.
#[derive(Serialize, ToSchema)]
pub struct InstanceResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    pub id: String,
    /// The şube (class section) that teaches it.
    pub class: String,
    /// The catalog course being taught (`GET /courses/{id}`) — its title lives
    /// there, shared by every instance of it.
    pub course: String,
    /// Weekly lesson hours; the instance's weight in the year's karne average.
    #[schema(example = 5)]
    pub ders_saati: i64,
    /// Whether this instance's marks count toward the karne.
    pub counts_toward_karne: bool,
    /// How many students are enrolled right now.
    #[schema(example = 28)]
    pub enrollment_count: i64,
    /// The staff assigned to run this instance — separate from the şube's
    /// homeroom teacher, who may also act here (see `GET /instances/{id}`).
    pub teachers: Vec<PersonRef>,
}

impl InstanceResponse {
    fn new(
        instance: &ClassCourse,
        teachers: &[UserId],
        people: &HashMap<String, PersonRef>,
    ) -> Self {
        Self {
            id: instance.get_id().key().to_string(),
            class: instance.get_class().key().to_string(),
            course: instance.get_course().key().to_string(),
            ders_saati: instance.get_ders_saati().as_i64(),
            counts_toward_karne: instance.counts_toward_karne(),
            enrollment_count: instance.get_enrollment_count(),
            teachers: teachers
                .iter()
                .map(|teacher| PersonRef::resolve(people, teacher))
                .collect(),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct EnrollmentResponse {
    id: String,
    /// The instance this roster row is under (`GET /instances/{id}`).
    class_course: String,
    /// The enrolled student.
    user: PersonRef,
    /// Who enrolled them.
    enrolled_by: PersonRef,
    /// The şube this row was pumped by, or `null` when a human placed it
    /// directly. A row with a şube on it is *swept* when that şube drops the
    /// student or detaches the course; a `null` one is nobody's to take back.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    source: Option<String>,
}

impl EnrollmentResponse {
    fn new(enrollment: &Enrollment, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: enrollment.get_id().key().to_string(),
            class_course: enrollment.get_class_course().key().to_string(),
            user: PersonRef::resolve(people, enrollment.get_user()),
            enrolled_by: PersonRef::resolve(people, enrollment.get_enrolled_by()),
            source: enrollment.get_source().map(|class| class.key().to_string()),
        }
    }
}

/// The instance a path id names, or a 404 — every route under `/instances/{id}`
/// gates on it, so a missing instance never reads as an empty roster.
async fn instance_or_404(key: &ClassCourseId, db: &Database) -> Result<ClassCourse, AppError> {
    service::class_course::read(db, key)
        .await?
        .ok_or(AppError::NotFound)
}

/// Who may read inside an instance: anyone who can manage it, plus its
/// enrolled students. Other teachers and unenrolled students see nothing.
pub(crate) async fn can_view_instance(
    db: &Database,
    instance: &ClassCourseId,
    user: &User,
) -> Result<bool, AppError> {
    if can_manage_instance(db, instance, user).await? {
        return Ok(true);
    }
    Ok(
        service::enrollment::read_for_user(db, instance, user.get_id())
            .await?
            .is_some(),
    )
}

/// Join the people a page of instances names (their assigned teachers) in one
/// batch read — the lookup shrinks with the window, never the whole table.
pub(crate) async fn instance_people(
    rows: &[(ClassCourse, Vec<UserId>)],
    db: &Database,
) -> Result<HashMap<String, PersonRef>, AppError> {
    person_map(
        rows.iter()
            .flat_map(|(_, teachers)| teachers.iter().cloned()),
        db,
    )
    .await
}

/// The instances the caller may act in or see, paged via `?limit=&offset=`
/// (omit `limit` for all of them), newest first; returns a
/// `{items, total, limit, offset}` envelope. The route a student reads to find
/// the courses their section is being taught, and a teacher the ones they run.
///
/// The set is exactly
/// [`crate::service::instance::visible_instances`]'s — the caller's şubeler (a
/// student's live membership, a homeroom teacher's) unioned with the instances
/// they were assigned to teach, deduped by instance. One rule for what the
/// caller may see, so this list and every instance-scoped gate can never
/// disagree about which instances are theirs.
#[utoipa::path(
    get,
    path = "/me",
    tag = "instances",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's instances (all of them when unpaged)", body = Page<InstanceResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_instances(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<InstanceResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Both reads the caller's identity feeds (`class_member` and the teach /
    // homeroom assignments) are `visible_instances`'. Its order is the two
    // arms' own — each newest first — so the union is sorted back into one
    // newest-first order: `?limit=&offset=` pages over a total order, and a
    // stable one is what keeps a page from handing the same row out twice.
    let mut visible = visible_instances(&user, &st.db).await?;
    visible.sort_by(|(a, _), (b, _)| {
        b.get_attached_at()
            .cmp(&a.get_attached_at())
            .then_with(|| b.get_id().uuid().cmp(&a.get_id().uuid()))
    });
    let total = visible.len() as i64;
    let window: Vec<ClassCourse> = paginate(&visible, limit, offset)
        .iter()
        .map(|(instance, _)| instance.clone())
        .collect();
    let with_teachers = crate::db::class_course_teacher::into_instances(&st.db, window).await?;
    let people = instance_people(&with_teachers, &st.db).await?;
    let items = with_teachers
        .iter()
        .map(|(instance, teachers)| InstanceResponse::new(instance, teachers, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch one instance by id. Visible to its enrolled students, its assigned
/// teachers, its şube's homeroom teacher, and managers/admins.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    responses(
        (status = 200, description = "The instance", body = InstanceResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, and not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_instance(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<InstanceResponse>, AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    if !can_view_instance(&st.db, instance.get_id(), &user).await? {
        return Err(AppError::Forbidden(
            "only this instance's enrolled students, its teachers, its class's homeroom teacher, or a manager/admin can view it",
        ));
    }
    let teachers =
        crate::db::class_course_teacher::list_for_instance(&st.db, instance.get_id()).await?;
    let people = person_map(teachers.iter().cloned(), &st.db).await?;
    Ok(Json(InstanceResponse::new(&instance, &teachers, &people)))
}

/// Update one instance's own policy. Requires teacher+ and a right over this
/// instance: manager+, one of its assigned teachers, or its şube's homeroom
/// teacher. Omitted fields keep their value; both are non-clearable.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = UpdateInstance,
    responses(
        (status = 200, description = "Updated instance", body = InstanceResponse),
        (status = 400, description = "Invalid ders_saati", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_instance(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateInstance>,
) -> Result<Json<InstanceResponse>, AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::class_course::require_open(&st.db, instance.get_id()).await?;

    let staff = req.ders_saati.map(DersSaati::try_new).transpose()?;
    let updated =
        service::class_course::update(&st.db, instance.get_id(), staff, req.counts_toward_karne)
            .await?;
    let teachers =
        crate::db::class_course_teacher::list_for_instance(&st.db, updated.get_id()).await?;
    let people = person_map(teachers.iter().cloned(), &st.db).await?;
    Ok(Json(InstanceResponse::new(&updated, &teachers, &people)))
}

/// Assign a teacher to this instance (idempotent). Manager+ only — staffing is
/// the office's call. The assignee must already hold the `teacher` role or
/// higher; the assignment gives them full management of the instance (exams,
/// sessions, homework, roster, grading) but they keep no catalog rights over
/// the course itself.
#[utoipa::path(
    post,
    path = "/{id}/teachers",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = AssignTeacher,
    responses(
        (status = 200, description = "Assigned (or already assigned)", body = InstanceResponse),
        (status = 400, description = "Unknown user, or user is not a teacher or higher", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "That user was demoted below teacher while the request ran — the assignment was undone; or this instance's academic year is archived", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn assign_teacher(
    State(st): State<AppState>,
    RequireManager(manager): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<AssignTeacher>,
) -> Result<Json<InstanceResponse>, AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    let target = UserId::from_key(&req.user_id);
    service::class_course::assign_teacher(&st.db, instance.get_id(), &target, manager.get_id())
        .await?;
    // The row is written; a demotion that raced the gate inside the assignment
    // swept the list before this row was in it (see [`super::undo_if_demoted`]).
    super::undo_if_demoted(&target, &st.db).await?;
    let teachers =
        crate::db::class_course_teacher::list_for_instance(&st.db, instance.get_id()).await?;
    let people = person_map(teachers.iter().cloned(), &st.db).await?;
    Ok(Json(InstanceResponse::new(&instance, &teachers, &people)))
}

/// Unassign a teacher from this instance. Manager+ only. The instance, its
/// exams, sessions, and roster are untouched — the teacher just loses their
/// management rights over it. A user who was never assigned is a 404.
#[utoipa::path(
    delete,
    path = "/{id}/teachers/{user}",
    tag = "instances",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Instance id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Unassigned"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Instance not found, or that user was not assigned to it", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn unassign_teacher(
    State(st): State<AppState>,
    RequireManager(_manager): RequireManager,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    let removed = service::class_course::unassign_teacher(
        &st.db,
        instance.get_id(),
        &UserId::from_key(&target),
    )
    .await?;
    if !removed {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- enrollments ----------------------------------------------------------

/// Enroll a student into this instance (idempotent upsert). Requires teacher+
/// and a right over the instance. Only students can be enrolled — enrollment is
/// student membership, and it gates sitting exams, being graded, and the
/// roster. The row is hand-placed (`source` `null`), so no şube sweep can take
/// it back.
#[utoipa::path(
    post,
    path = "/{id}/enrollments",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = EnrollUser,
    responses(
        (status = 200, description = "Enrolled (or already enrolled)", body = EnrollmentResponse),
        (status = 400, description = "Unknown user, or user is not a student", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn enroll(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<EnrollUser>,
) -> Result<Json<EnrollmentResponse>, AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::class_course::require_open(&st.db, instance.get_id()).await?;

    let target = UserId::from_key(&req.user_id);
    let enrollment =
        service::enrollment::enroll(&st.db, instance.get_id(), &target, user.get_id()).await?;
    let people = person_map([target, *user.get_id()], &st.db).await?;
    Ok(Json(EnrollmentResponse::new(&enrollment, &people)))
}

/// List this instance's roster, paged via `?limit=&offset=` (omit `limit` for
/// the whole roster). Requires teacher+ and a right over the instance —
/// students see their own instances via `GET /instances/me`. Returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/enrollments",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id"), PageParams),
    responses(
        (status = 200, description = "A page of enrollments (the whole roster when unpaged)", body = Page<EnrollmentResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
    ),
)]
async fn list_roster(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<EnrollmentResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // The instance must exist — a missing one is a 404, not an empty roster.
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    let (rows, total) =
        service::enrollment::list_for_class_course(&st.db, instance.get_id(), limit, offset)
            .await?;
    // Join people onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        rows.iter()
            .flat_map(|e| [*e.get_user(), *e.get_enrolled_by()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|e| EnrollmentResponse::new(e, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Unenroll a student from this instance. Requires teacher+ and a right over
/// the instance. Existing exam results are kept (they disappear from the
/// student's marks report until re-enrolled).
#[utoipa::path(
    delete,
    path = "/{id}/enrollments/{user}",
    tag = "instances",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Instance id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Unenrolled"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn unenroll(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::class_course::require_open(&st.db, instance.get_id()).await?;
    service::enrollment::unenroll(&st.db, instance.get_id(), &UserId::from_key(&target)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- exams in an instance ---------------------------------------------------

/// Create an exam inside one instance. Requires teacher+ and a right over the
/// instance; the exam's marks count into the instance's average with its
/// kind's weight (`GET /settings`) and into the dönem's karne named by `term`.
/// Omit `mode` for an offline-graded exam nobody can sit; `sync`/`async` take
/// a window (async also `duration_ms`), `open` is sittable anytime with an
/// optional per-attempt `duration_ms`. `max_attempts` (default 1, `0` =
/// unlimited) meters retakes and `allow_rejoin` (default `true`) is the
/// exam-room door — both stay editable while the exam runs. Send `draft: true`
/// to keep the exam private while it's still being written: only the
/// instance's managers see it, and sitting and grading are blocked until it's
/// published (`PATCH` `draft: false`). Only a class-delivered course (`kind`
/// `course`) carries exams.
#[utoipa::path(
    post,
    path = "/{id}/exams",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = CreateExamInCourse,
    responses(
        (status = 201, description = "Exam created", body = ExamResponse),
        (status = 400, description = "Invalid fields, kind, attempt limit, schedule, an unknown or foreign dönem, or a club/etüt instance", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_exam_in_instance(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateExamInCourse>,
) -> Result<(StatusCode, Json<ExamResponse>), AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;

    let title = ExamTitle::try_new(&req.title)?;
    let description = ExamDescription::try_new(&req.description.unwrap_or_default())?;
    let school = service::settings::load(&st.db).await?;
    let kind = ExamKind::try_new(&req.kind, school.get_exam_kinds())?;
    // The dönem is resolved *without* the archive gate: a dönem archived inside
    // an open year is a record, not a wall (the year is what
    // [`service::exam::create`] refuses). An unknown id is this route's 400.
    let Some(term) = service::term::read(&st.db, &TermId::from_key(&req.term)).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "term",
            reason: "term does not exist",
        }));
    };
    let starts_at = req.starts_at.map(Timestamp::from_millis);
    let ends_at = req.ends_at.map(Timestamp::from_millis);
    check_not_past("starts_at", starts_at)?;
    check_not_past("ends_at", ends_at)?;
    let schedule = ExamSchedule::try_new(
        req.mode.as_deref().map(ExamMode::try_new).transpose()?,
        starts_at,
        ends_at,
        req.duration_ms.map(ExamDuration::try_new).transpose()?,
    )?;
    let max_attempts = match req.max_attempts {
        Some(limit) => ExamAttemptLimit::try_new(limit)?,
        None => ExamAttemptLimit::single(),
    };
    let exam = crate::service::exam::create(
        &st.db,
        user.get_id(),
        instance.get_id(),
        term.get_id(),
        title,
        description,
        kind,
        schedule,
        max_attempts,
        req.allow_rejoin.unwrap_or(true),
        req.allow_review.unwrap_or(false),
        req.draft.unwrap_or(false),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(ExamResponse::new(&exam))))
}

/// List one instance's exams, paged via `?limit=&offset=` (omit `limit` for
/// all of them). Visible to the instance's enrolled students, its teachers,
/// its şube's homeroom teacher, and managers/admins — but drafts appear only
/// to the instance's managers. Returns a `{items, total, limit, offset}`
/// envelope.
#[utoipa::path(
    get,
    path = "/{id}/exams",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id"), PageParams),
    responses(
        (status = 200, description = "A page of the instance's exams (all of them when unpaged)", body = Page<ExamResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not this instance's teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
    ),
)]
async fn list_instance_exams(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ExamResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // The instance must exist — a missing one is a 404, not an empty exam list.
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    if !can_view_instance(&st.db, instance.get_id(), &user).await? {
        return Err(AppError::Forbidden(
            "only this instance's enrolled students, its teachers, its class's homeroom teacher, or a manager/admin can view its exams",
        ));
    }
    let mut exams = crate::service::exam::list_for_class_course(&st.db, instance.get_id()).await?;
    // Drafts are the managers' workbench — enrolled students don't see them.
    if !can_manage_instance(&st.db, instance.get_id(), &user).await? {
        exams.retain(|exam| !exam.is_draft());
    }
    let total = exams.len() as i64;
    // Paged in the web layer: the draft filter above is per-row Rust.
    let items = paginate(&exams, limit, offset)
        .iter()
        .map(ExamResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

// ---- homework in an instance -------------------------------------------------

/// Assign homework inside one instance. Requires teacher+ and a right over the
/// instance. The homework is tagged with one of the catalog course's subjects
/// and given a future `due_at`; `assigned` optionally narrows it to a subset of
/// the enrolled students (omit or empty = the whole roster).
#[utoipa::path(
    post,
    path = "/{id}/homework",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = CreateHomework,
    responses(
        (status = 201, description = "Homework created", body = HomeworkResponse),
        (status = 400, description = "Invalid fields, a due date in the past, an unknown subject (or one from another course), or an assigned student not enrolled / over the cap", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_homework_in_instance(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateHomework>,
) -> Result<(StatusCode, Json<HomeworkResponse>), AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::class_course::require_open(&st.db, instance.get_id()).await?;

    let title = HomeworkTitle::try_new(&req.title)?;
    let description = match req.description.as_deref() {
        Some(text) => description_or_none(text)?,
        None => None,
    };
    let due_at = Timestamp::from_millis(req.due_at);
    check_not_past("due_at", Some(due_at))?;
    // No lease: the create takes the subject's reference counter in the same
    // breath as the row, and the subject delete is refused while that counter
    // is non-zero — so the check below is only a pre-flight for the message.
    let subject =
        service::subject::in_course(&st.db, &req.subject_id, instance.get_course()).await?;
    let assigned = resolve_assigned(req.assigned, instance.get_id(), &st.db).await?;
    let homework = service::homework::create(
        &st.db,
        instance.get_id(),
        &subject,
        title,
        description,
        due_at,
        assigned,
        user.get_id(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(HomeworkResponse::new(&homework))))
}

/// List one instance's homework, newest first, paged via `?limit=&offset=`
/// (omit `limit` for all of it). Visible to the instance's enrolled students,
/// its teachers, and managers/admins — but a student sees only the homework
/// they are assigned (whole-roster ones plus subsets that name them, each with
/// its `assigned` narrowed to themselves). Returns a `{items, total, limit,
/// offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/homework",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id"), PageParams),
    responses(
        (status = 200, description = "A page of the instance's homework (all of it when unpaged)", body = Page<HomeworkResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not this instance's teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
    ),
)]
async fn list_instance_homework(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<HomeworkResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // The instance must exist — a missing one is a 404, not an empty list.
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    if !can_view_instance(&st.db, instance.get_id(), &user).await? {
        return Err(AppError::Forbidden(
            "only this instance's enrolled students, its teachers, its class's homeroom teacher, or a manager/admin can view its homework",
        ));
    }
    let mut homework = service::homework::list_for_class_course(&st.db, instance.get_id()).await?;
    // A student sees only the homework they are assigned; managers see all.
    let manages = can_manage_instance(&st.db, instance.get_id(), &user).await?;
    if !manages {
        homework.retain(|hw| hw.student_sees(user.get_id()));
    }
    let total = homework.len() as i64;
    // Paged in the web layer: the audience filter above is per-row Rust. A
    // subset roster goes out whole only to a manager of the instance; a student
    // sees themselves in it and no one else.
    let items = paginate(&homework, limit, offset)
        .iter()
        .map(|hw| {
            if manages {
                HomeworkResponse::new(hw)
            } else {
                HomeworkResponse::for_viewer(hw, user.get_id())
            }
        })
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

// ---- sessions in an instance -------------------------------------------------

/// Create a lesson session inside one instance. Requires teacher+ and a right
/// over the instance. The session's teacher defaults to the caller.
#[utoipa::path(
    post,
    path = "/{id}/sessions",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = CreateSessionInCourse,
    responses(
        (status = 201, description = "Session created", body = SessionResponse),
        (status = 400, description = "Invalid fields, time range, times in the past, or teacher", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_session_in_instance(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateSessionInCourse>,
) -> Result<(StatusCode, Json<SessionResponse>), AppError> {
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::class_course::require_open(&st.db, instance.get_id()).await?;

    let topic = SessionTopic::try_new(&req.topic.unwrap_or_default())?;
    let teacher =
        service::course_session::resolve_session_teacher(req.teacher_id.as_deref(), &user, &st.db)
            .await?;
    let starts_at = Timestamp::from_millis(req.starts_at);
    let ends_at = req.ends_at.map(Timestamp::from_millis);
    check_not_past("starts_at", Some(starts_at))?;
    check_not_past("ends_at", ends_at)?;
    check_time_range(Some(starts_at), ends_at)?;

    let session = service::course_session::create(
        &st.db,
        instance.get_id(),
        teacher.get_id(),
        topic,
        starts_at,
        ends_at,
    )
    .await?;
    let people = PersonRef::map_of(&[&teacher]);
    Ok((
        StatusCode::CREATED,
        Json(SessionResponse::new(&session, &people)),
    ))
}

/// List one instance's lesson sessions, most recent first, paged via
/// `?limit=&offset=` (omit `limit` for all of them). Visible to the instance's
/// enrolled students, its teachers, and managers/admins. Returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/sessions",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id"), PageParams),
    responses(
        (status = 200, description = "A page of the instance's sessions (all of them when unpaged)", body = Page<SessionResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not this instance's teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
    ),
)]
async fn list_instance_sessions(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<SessionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // The instance must exist — a missing one is a 404, not an empty list.
    let instance = instance_or_404(&ClassCourseId::from_key(&id), &st.db).await?;
    if !can_view_instance(&st.db, instance.get_id(), &user).await? {
        return Err(AppError::Forbidden(
            "only this instance's enrolled students, its teachers, its class's homeroom teacher, or a manager/admin can view its sessions",
        ));
    }
    let (rows, total) =
        service::course_session::list_for_class_course(&st.db, instance.get_id(), limit, offset)
            .await?;
    // Join teachers onto the page alone — the lookup shrinks with the window.
    let people = person_map(rows.iter().map(|s| *s.get_teacher()), &st.db).await?;
    let items = rows
        .iter()
        .map(|s| SessionResponse::new(s, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::class_group::ClassGroupId;

    /// The read path behind `GET /instances/me` is `class_member` → the şubeler
    /// → their instances. A student enrolled by a şube sees exactly that
    /// şube's instances, and a second şube teaching the same course is a
    /// different row.
    #[tokio::test]
    async fn a_member_reads_their_own_sections_instances() {
        let (db, _leases) = crate::database::init_test_db().await;
        let office = crate::db::class_member::tests::fixture_user(&db, "instances-office").await;
        let student = crate::db::class_member::tests::fixture_user(&db, "instances-student").await;
        let course = crate::db::class_member::tests::a_course("Matematik", &db).await;
        let a = crate::db::class_member::tests::a_class("5-A", &db).await;
        let b = crate::db::class_member::tests::a_class("5-B", &db).await;
        service::class_course::attach(&db, &a, &course, &office)
            .await
            .unwrap();
        service::class_course::attach(&db, &b, &course, &office)
            .await
            .unwrap();
        service::class_member::add(&db, &a, &student, &office)
            .await
            .unwrap();

        let (members, _) = crate::db::class_member::list_for_user(&db, &student, None, 0)
            .await
            .unwrap();
        let classes: Vec<ClassGroupId> = members
            .iter()
            .map(|member| member.get_class().clone())
            .collect();
        let rows = crate::db::class_course::list_for_class_ids(&db, &classes)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "the student reads 5-B's instance too");
        assert_eq!(rows[0].get_class(), &a);
    }

    /// A homeroom teacher is **not** a member of the section they run, so the
    /// member read alone left their own instances out of
    /// [`visible_instances`] — while [`can_manage_instance`] let them act on
    /// every one of them. The homeroom column is the second, separate way a
    /// teacher reaches a şube, and it has to be unioned in.
    #[tokio::test]
    async fn a_homeroom_teacher_sees_their_sections_instances() {
        use crate::domain::class_group::ClassName;

        let (db, _leases) = crate::database::init_test_db().await;
        let office = crate::db::class_member::tests::fixture_user(&db, "visible-office").await;
        let teacher = crate::db::class_member::tests::fixture_user(&db, "visible-teacher").await;
        sqlx::query("UPDATE app_user SET role = 'teacher' WHERE id = $1")
            .bind(teacher.uuid())
            .execute(&db)
            .await
            .unwrap();
        let course = crate::db::class_member::tests::a_course("Matematik", &db).await;
        let class = service::class_group::create(
            &db,
            &office,
            ClassName::try_new("5-A").unwrap(),
            None,
            None,
            Some(teacher),
        )
        .await
        .unwrap();
        service::class_course::attach(&db, class.get_id(), &course, &office)
            .await
            .unwrap();

        let homeroom_teacher = crate::service::user::read(&db, &teacher)
            .await
            .unwrap()
            .unwrap();
        let visible = visible_instances(&homeroom_teacher, &db).await.unwrap();
        assert_eq!(
            visible.len(),
            1,
            "the section the teacher runs is invisible to them"
        );
        assert!(visible[0].1, "and they run it");
        assert_eq!(visible[0].0.get_class(), class.get_id());
    }

    /// `GET /instances/me` serves [`visible_instances`]' set, so the second arm
    /// of the union — the instances a teacher was *assigned* to teach — has to
    /// surface there even when they are neither enrolled in nor homeroom of
    /// the şube. Before the union the route was the member read alone, and a
    /// teacher's assigned instances were invisible to them.
    #[tokio::test]
    async fn an_assigned_teacher_reads_the_instances_they_teach_once() {
        let (db, _leases) = crate::database::init_test_db().await;
        let office = crate::db::class_member::tests::fixture_user(&db, "union-office").await;
        let teacher = crate::db::class_member::tests::fixture_user(&db, "union-teacher").await;
        sqlx::query("UPDATE app_user SET role = 'teacher' WHERE id = $1")
            .bind(teacher.uuid())
            .execute(&db)
            .await
            .unwrap();
        let stranger = crate::db::class_member::tests::fixture_user(&db, "union-stranger").await;
        let course = crate::db::class_member::tests::a_course("Matematik", &db).await;
        let a = crate::db::class_member::tests::a_class("5-A", &db).await;
        let b = crate::db::class_member::tests::a_class("5-B", &db).await;
        let first = service::class_course::attach(&db, &a, &course, &office)
            .await
            .unwrap();
        service::class_course::attach(&db, &b, &course, &office)
            .await
            .unwrap();
        // The office staffing call, minus its role gate: what this test is
        // about is the read, not who may write the row.
        crate::db::class_course_teacher::assign(&db, first.get_id(), &teacher)
            .await
            .unwrap();

        let teacher_user = crate::service::user::read(&db, &teacher)
            .await
            .unwrap()
            .unwrap();
        let visible = visible_instances(&teacher_user, &db).await.unwrap();
        assert_eq!(visible.len(), 1, "the instance they teach, exactly once");
        assert_eq!(visible[0].0.get_id(), first.get_id());
        assert!(visible[0].1, "and they run it");

        // A caller tied to neither section reads nothing — the union is not a
        // door to every instance.
        let stranger_user = crate::service::user::read(&db, &stranger)
            .await
            .unwrap()
            .unwrap();
        assert!(
            visible_instances(&stranger_user, &db)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The two arms meet on a homeroom teacher who is also assigned to the
    /// same instance: one instance, one row. The union is a set — a duplicate
    /// would show the same instance twice on `GET /instances/me` and count it
    /// twice in the page's `total`.
    #[tokio::test]
    async fn a_homeroom_teacher_assigned_to_the_same_instance_reads_it_once() {
        use crate::domain::class_group::ClassName;

        let (db, _leases) = crate::database::init_test_db().await;
        let office = crate::db::class_member::tests::fixture_user(&db, "dedupe-office").await;
        let teacher = crate::db::class_member::tests::fixture_user(&db, "dedupe-teacher").await;
        sqlx::query("UPDATE app_user SET role = 'teacher' WHERE id = $1")
            .bind(teacher.uuid())
            .execute(&db)
            .await
            .unwrap();
        let course = crate::db::class_member::tests::a_course("Matematik", &db).await;
        let class = service::class_group::create(
            &db,
            &office,
            ClassName::try_new("5-A").unwrap(),
            None,
            None,
            Some(teacher),
        )
        .await
        .unwrap();
        let instance = service::class_course::attach(&db, class.get_id(), &course, &office)
            .await
            .unwrap();
        crate::db::class_course_teacher::assign(&db, instance.get_id(), &teacher)
            .await
            .unwrap();

        let teacher_user = crate::service::user::read(&db, &teacher)
            .await
            .unwrap()
            .unwrap();
        let visible = visible_instances(&teacher_user, &db).await.unwrap();
        assert_eq!(
            visible.len(),
            1,
            "homeroom and assignment are one instance, not two rows"
        );
        assert_eq!(visible[0].0.get_id(), instance.get_id());
    }
}
