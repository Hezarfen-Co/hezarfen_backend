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
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
use crate::domain::course_session::SessionTopic;
use crate::domain::enrollment::Enrollment;
use crate::domain::exam::{
    ExamAttemptLimit, ExamDescription, ExamDuration, ExamKind, ExamMode, ExamSchedule, ExamTitle,
};
use crate::domain::homework::{Homework, HomeworkTitle};
use crate::domain::role::Role;
use crate::domain::subject::{SubjectDescription, SubjectName};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::state::AppState;

use super::homework::{description_or_none, resolve_assigned};
use super::{
    CourseResponse, CurrentUser, ExamResponse, HomeworkResponse, Page, PageParams, PersonRef,
    RequireManager, RequireTeacher, SessionResponse, SubjectResponse, check_not_past,
    check_time_range, course_people, paginate, person_map, remove_blob, set_or_clear,
    undo_if_demoted,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_course, list_courses))
        .routes(routes!(my_courses))
        .routes(routes!(get_course, update_course, delete_course))
        .routes(routes!(assign_teacher))
        .routes(routes!(unassign_teacher))
        .routes(routes!(enroll, list_roster))
        .routes(routes!(unenroll))
}

// The four route pairs below are mounted under `/courses` but *belong* to
// another module, so each is split out to carry that module's gate as well as
// the course one — see `crate::web::module_gate`. They are merged back in
// `build_router`, so the URL space is unchanged.

pub fn exam_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_exam_in_course, list_course_exams))
}

pub fn session_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_session_in_course, list_course_sessions))
}

pub fn subject_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_subject_in_course, list_course_subjects))
}

pub fn homework_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_homework_in_course, list_course_homework))
}

#[derive(Deserialize, ToSchema)]
struct CreateCourse {
    #[schema(max_length = 200, example = "Algebra")]
    title: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// `course` (a regular class — the default), `study` (a supervised study
    /// session — etüt), or `club` (a student club — kulüp). Behaviorally
    /// identical; a label for the UI.
    #[schema(example = "course")]
    kind: Option<String>,
    /// The academic term this course belongs to (`GET /terms`). Optional.
    term_id: Option<String>,
    /// Seat cap enforced when enrolling, at least 1. Omit for unlimited.
    #[schema(example = 12)]
    capacity: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateCourse {
    #[schema(max_length = 200)]
    title: Option<String>,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// `course`, `study` (etüt), or `club` (kulüp). Omit to keep the current
    /// kind.
    kind: Option<String>,
    /// Omit to keep the current term, send `null` to unlink, or send a term
    /// id to (re)assign.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    term_id: Option<Option<String>>,
    /// Omit to keep the current cap, send `null` to lift it, or send a value
    /// (at least 1) to (re)cap. Lowering below the current roster keeps the
    /// roster — only new enrolls are refused.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>, example = 12)]
    capacity: Option<Option<i64>>,
}

#[derive(Deserialize, ToSchema)]
struct EnrollUser {
    /// The user to enroll.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    user_id: String,
}

#[derive(Deserialize, ToSchema)]
struct AssignTeacher {
    /// The staff member to put in charge of the course. Must hold the
    /// `teacher` role or higher.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    user_id: String,
}

#[derive(Deserialize, ToSchema)]
struct CreateExamInCourse {
    #[schema(max_length = 200, example = "Midterm")]
    title: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// The assessment form — one of the school's exam kinds (`GET /settings`;
    /// defaults: `homework`, `quiz`, `midterm`, `final`, `project`, `oral`).
    /// The kind's settings-configured weight decides how heavily the exam
    /// counts into the course average.
    #[schema(max_length = 50, example = "midterm")]
    kind: String,
    /// `sync` (one fixed window for everyone), `async` (each student starts
    /// inside the window and gets `duration_ms`), or `open` (no window — sit
    /// anytime). Omit for an offline-graded exam that cannot be sat.
    #[schema(example = "sync")]
    mode: Option<String>,
    /// Window open, UTC unix-milliseconds. Required for `sync`/`async`,
    /// forbidden for `open`; must not be in the past.
    #[schema(example = 1_900_000_000_000_i64)]
    starts_at: Option<i64>,
    /// Window close, UTC unix-milliseconds. Required for `sync`/`async`,
    /// forbidden for `open`; must not be in the past.
    ends_at: Option<i64>,
    /// Per-attempt time budget, milliseconds — required for `async`, optional
    /// for `open` (omit for unlimited time), forbidden for `sync`.
    #[schema(minimum = 60_000, maximum = 86_400_000, example = 5_400_000_i64)]
    duration_ms: Option<i64>,
    /// How many attempts each student gets, `1`–`100`, or `0` for unlimited.
    /// Defaults to `1` — the classic single sitting.
    #[schema(minimum = 0, maximum = 100, example = 1)]
    max_attempts: Option<i64>,
    /// Whether a student who left the exam room may come back in and keep
    /// answering. Defaults to `true`; editable live while the exam runs.
    allow_rejoin: Option<bool>,
    /// Whether students may review their graded attempt once results are out.
    /// Defaults to `false`; editable live.
    allow_review: Option<bool>,
    /// Save as a work-in-progress draft: visible only to the course's
    /// managers, not sittable, not gradable, until published via
    /// `PATCH /exams/{id}` with `draft: false`. Defaults to `false`.
    draft: Option<bool>,
}

#[derive(Serialize, ToSchema)]
struct EnrollmentResponse {
    id: String,
    course: String,
    /// The enrolled student.
    user: PersonRef,
    /// Who enrolled them.
    enrolled_by: PersonRef,
    /// The class (`GET /classes/{id}`) this row was pumped by, or `null` when a
    /// human placed it directly. A row with a class on it is *swept* when that
    /// class drops the student or detaches the course; a `null` one is nobody's
    /// to take back. Without it no client could tell which of its roster rows a
    /// class change is about to remove.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    source: Option<String>,
}

impl EnrollmentResponse {
    fn new(enrollment: &Enrollment, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: enrollment.get_id().key().to_string(),
            course: enrollment.get_course().key().to_string(),
            user: PersonRef::resolve(people, enrollment.get_user()),
            enrolled_by: PersonRef::resolve(people, enrollment.get_enrolled_by()),
            source: enrollment.get_source().map(|class| class.key().to_string()),
        }
    }
}

/// Who may write inside a specific course (edit it, enroll, add exams,
/// sessions, subjects, grade): its creator, a teacher a manager assigned to
/// it, or anyone `manager` and above — and in every case only while the
/// caller is *still* `teacher` or above.
///
/// The `teacher` floor is enforced here rather than left to the callers: half
/// of them extract `CurrentUser`, not `RequireTeacher`, so a creator demoted
/// to `student` or `parent` used to keep course-management rights forever (the
/// `creator` column is a historical fact and is never swept, unlike the
/// assignment list).
///
/// Deleting the course and changing its teacher list sit *above* this bar —
/// see [`owns_course`].
pub(crate) fn can_manage_course(course: &Course, user: &User) -> bool {
    user.get_role().at_least(Role::Teacher)
        && (course.is_creator(user.get_id())
            || course.is_assigned(user.get_id())
            || user.get_role().at_least(Role::Manager))
}

/// Who may destroy a course: its creator, or anyone `manager` and above. An
/// assigned teacher runs the course but does not own it — they cannot delete
/// it out from under the person who made it.
///
/// Carries the same live-`teacher` floor as [`can_manage_course`], and for the
/// same reason: a demoted creator owns nothing.
fn owns_course(course: &Course, user: &User) -> bool {
    user.get_role().at_least(Role::Teacher)
        && (course.is_creator(user.get_id()) || user.get_role().at_least(Role::Manager))
}

/// Who may read inside a specific course (its details, exams, sessions):
/// anyone who can manage it, plus its enrolled users. Other teachers and
/// unenrolled students see nothing.
pub(crate) async fn can_view_course(
    course: &Course,
    user: &User,
    db: &Database,
) -> Result<bool, AppError> {
    if can_manage_course(course, user) {
        return Ok(true);
    }
    Ok(
        service::enrollment::read_for_user(db, course.get_id(), user.get_id())
            .await?
            .is_some(),
    )
}

/// The catalog as one user sees it: every course for manager+, otherwise the
/// courses they created or were assigned to plus the ones they're enrolled in,
/// newest first.
///
/// The created/assigned half carries the same live-`teacher` floor as
/// [`can_manage_course`], and for the same reason: `creator` is a historical
/// column no demotion sweeps, so without it a demoted creator kept seeing the
/// course — and, through the `/exams` and `/homework` catalogs that build on
/// this list, its published exams and homework. Below `teacher` a course is
/// visible only the way it is to any other student: by enrollment.
pub(crate) async fn visible_courses(user: &User, db: &Database) -> Result<Vec<Course>, AppError> {
    if user.get_role().at_least(Role::Manager) {
        return service::course::list_all(db).await;
    }
    let mut courses = if user.get_role().at_least(Role::Teacher) {
        service::course::list_for_teacher(db, user.get_id()).await?
    } else {
        Vec::new()
    };
    for course in service::course::list_enrolled(db, user.get_id(), None, 0)
        .await?
        .0
    {
        if !courses
            .iter()
            .any(|known| known.get_id() == course.get_id())
        {
            courses.push(course);
        }
    }
    // Both sources come newest-first; re-sort so the merged list is too.
    courses.sort_by(|a, b| b.get_id().key().cmp(a.get_id().key()));
    Ok(courses)
}

// ---- courses ------------------------------------------------------------

/// A seat cap, when given, must be positive — `null`/omitted means unlimited.
fn check_capacity(capacity: Option<i64>) -> Result<(), AppError> {
    if capacity.is_some_and(|capacity| capacity < 1) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "capacity",
            reason: "capacity must be at least 1",
        }));
    }
    Ok(())
}

/// Create a course owned by the current user. Requires the `teacher` role or
/// higher. `kind` picks the flavor — `course` (a regular class, the default),
/// `study` (a supervised study session — etüt), or `club` (a student club —
/// kulüp); all behave identically. `capacity` caps the roster at enroll time
/// (omit for unlimited).
#[utoipa::path(
    post,
    path = "/",
    tag = "courses",
    security(("session_cookie" = [])),
    request_body = CreateCourse,
    responses(
        (status = 201, description = "Course created", body = CourseResponse),
        (status = 400, description = "Invalid fields, kind, or capacity", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 409, description = "The named term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Json(req): Json<CreateCourse>,
) -> Result<(StatusCode, Json<CourseResponse>), AppError> {
    let title = CourseTitle::try_new(&req.title)?;
    let description = CourseDescription::try_new(&req.description.unwrap_or_default())?;
    let kind = match req.kind {
        Some(ref kind) => CourseKind::try_new(kind)?,
        None => CourseKind::course(),
    };
    // Pre-flight only: the create itself claims a reference on the term before
    // it writes the link, and a term deleted in between fails that claim with
    // this very error — so an unknown id reads the same whichever side wins.
    let term = service::term::resolve(&st.db, req.term_id.as_deref()).await?;
    check_capacity(req.capacity)?;
    let course = service::course::create(
        &st.db,
        user.get_id(),
        title,
        description,
        kind,
        term,
        req.capacity,
    )
    .await?;
    // The creator is the caller — already loaded, no extra lookup.
    let people = PersonRef::map_of(&[&user]);
    Ok((
        StatusCode::CREATED,
        Json(CourseResponse::new(&course, &people)),
    ))
}

/// List the courses visible to the caller: every course for manager+,
/// otherwise the courses they created plus the ones they're enrolled in. Paged
/// via `?limit=&offset=` (omit `limit` for the full list); returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "courses",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's visible courses (all of them when unpaged)", body = Page<CourseResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_courses(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<CourseResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let courses = visible_courses(&user, &st.db).await?;
    let total = courses.len() as i64;
    // Paged in the web layer: the visible set is a Rust union of two lists.
    let window = paginate(&courses, limit, offset);
    // Join creators onto the page alone — the lookup shrinks with the window.
    let people = person_map(window.iter().flat_map(course_people), &st.db).await?;
    let items = window
        .iter()
        .map(|course| CourseResponse::new(course, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// The courses the current user is enrolled in, paged via `?limit=&offset=`
/// (omit `limit` for all of them); returns a `{items, total, limit, offset}`
/// envelope.
#[utoipa::path(
    get,
    path = "/me",
    tag = "courses",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's enrolled courses (all of them when unpaged)", body = Page<CourseResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_courses(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<CourseResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (courses, total) =
        service::course::list_enrolled(&st.db, user.get_id(), limit, offset).await?;
    let people = person_map(courses.iter().flat_map(course_people), &st.db).await?;
    let items = courses
        .iter()
        .map(|course| CourseResponse::new(course, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single course by id. Visible to its enrolled users, and to its
/// creator, its assigned teachers and managers/admins while those accounts are
/// still `teacher`+ — a demoted creator sees it only if they are enrolled, like
/// any other student (see [`can_manage_course`]).
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 200, description = "The course", body = CourseResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, and not a still-`teacher`+ course creator, assigned teacher, or manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_course(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
        ));
    }
    let people = person_map(course_people(&course), &st.db).await?;
    Ok(Json(CourseResponse::new(&course, &people)))
}

/// Update a course. Requires teacher+ and course management rights — its
/// creator, a teacher assigned to it, or a manager/admin. Omitted fields keep
/// their value.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = UpdateCourse,
    responses(
        (status = 200, description = "Updated course", body = CourseResponse),
        (status = 400, description = "Invalid fields, kind, or capacity", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The term this update moves the course off changed since the caller read it (nothing was written, re-read and retry), or this course's term (or the named one) is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateCourse>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit this course",
        ));
    }
    service::course::require_open(&st.db, &course).await?;

    // Only what the request actually carried is validated and written — an
    // omitted field stays `None` so the save never re-sends this snapshot's
    // value over a concurrent PATCH of that field.
    let title = req.title.as_deref().map(CourseTitle::try_new).transpose()?;
    let description = req
        .description
        .as_deref()
        .map(CourseDescription::try_new)
        .transpose()?;
    let kind = req.kind.as_deref().map(CourseKind::try_new).transpose()?;
    // Both columns are nullable, so both stay clearable: omitted is `None`
    // (keep), an explicit `null` is `Some(None)` (write `NONE`).
    let term = match req.term_id {
        // Explicit `null` clears the link; a value must name a real term.
        Some(ref update) => Some(service::term::resolve(&st.db, update.as_deref()).await?),
        None => None,
    };
    // Explicit `null` lifts the cap; a value must be positive.
    check_capacity(req.capacity.flatten())?;
    let capacity = req.capacity;

    let updated =
        service::course::update(&st.db, course, title, description, kind, term, capacity).await?;
    let people = person_map(course_people(&updated), &st.db).await?;
    Ok(Json(CourseResponse::new(&updated, &people)))
}

/// Delete a course. Requires teacher+; only its creator or a manager/admin may
/// delete it — an assigned teacher runs the course but does not own it.
/// Refused with a 409 while anyone is still enrolled — empty the roster first,
/// so a course that carries students is never dropped by accident. Once empty,
/// it cascades the course's exams (with their results, questions, answers, and
/// question images), its homework (with submissions, submission files, and
/// grades), its sessions and roll call, and its subjects. It also detaches the
/// course from every class that carried it and strikes its id out of every
/// class blueprint that named it — a template holding a course nothing can
/// resolve is a stocking run that skips it and a `PATCH` that refuses the very
/// list the template already holds.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Students are still enrolled in this course, or this course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !owns_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can delete this course",
        ));
    }
    // The workflow — archived-term gate, EXAM_LOCK and HOMEWORK_LOCK writer
    // leases, blob-key collection, cascade — is [`service::course::delete`]'s.
    // Blob unlinking stays here because only the web layer knows `files_path`.
    let outcome = service::course::delete(&st.db, &course).await?;
    if !outcome.deleted {
        return Err(AppError::Conflict(
            "students are still enrolled in this course — remove them first",
        ));
    }
    for file in outcome
        .image_files
        .iter()
        .chain(&outcome.answer_image_files)
        .chain(&outcome.homework_files)
        .chain(&outcome.course_note_files)
    {
        remove_blob(&st.files_path, file).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- assigned teachers ----------------------------------------------------

/// Assign a teacher to a course (idempotent). Manager+ only — staffing is the
/// office's call, so a course's own creator cannot hand management rights to
/// their peers. The assignee must already hold the `teacher` role or higher;
/// assigning gives them full management of the course (exams, sessions,
/// subjects, roster, grading) but not the power to delete it or change this
/// list. The course's assigned teachers are returned on every course response.
#[utoipa::path(
    post,
    path = "/{id}/teachers",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = AssignTeacher,
    responses(
        (status = 200, description = "Assigned (or already assigned)", body = CourseResponse),
        (status = 400, description = "Unknown user, or user is not a teacher or higher", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 409, description = "That user was demoted below teacher while the request ran — the assignment was undone; or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn assign_teacher(
    State(st): State<AppState>,
    RequireManager(_manager): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<AssignTeacher>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let target = UserId::from_key(&req.user_id);
    let updated = service::course::assign_teacher(&st.db, &course, &target).await?;
    // The row is written; a demotion that raced the bar above swept the list
    // before this assignment was in it, and nothing re-sweeps (see
    // [`super::undo_if_demoted`]).
    undo_if_demoted(&target, &st.db).await?;
    let people = person_map(course_people(&updated), &st.db).await?;
    Ok(Json(CourseResponse::new(&updated, &people)))
}

/// Unassign a teacher from a course. Manager+ only. The course itself, its
/// exams, sessions, and roster are untouched — the teacher just loses their
/// management rights over it. A user who was never assigned is a 404.
#[utoipa::path(
    delete,
    path = "/{id}/teachers/{user}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Course id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Unassigned"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Course not found, or that user was not assigned to it", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn unassign_teacher(
    State(st): State<AppState>,
    RequireManager(_manager): RequireManager,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let removed =
        service::course::unassign_teacher(&st.db, &course, &UserId::from_key(&target)).await?;
    if removed.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- enrollments ----------------------------------------------------------

/// Enroll a user into a course (idempotent upsert). Requires teacher+ and
/// course management rights. Only students can be enrolled — enrollment is
/// student membership, and it gates sitting exams, being graded, and the class
/// roster, all student-only. A course with a `capacity` refuses new members
/// once the roster is full (someone already enrolled is returned as-is).
/// Enrolling a student a class pumped in takes the row *off* that class —
/// `source` comes back `null` — so a later class sweep can no longer undo a
/// placement made by hand, the mirror of a manual unenroll winning permanently.
#[utoipa::path(
    post,
    path = "/{id}/enrollments",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = EnrollUser,
    responses(
        (status = 200, description = "Enrolled (or already enrolled)", body = EnrollmentResponse),
        (status = 400, description = "Unknown user, or user is not a student", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 409, description = "The course is full, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn enroll(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<EnrollUser>,
) -> Result<Json<EnrollmentResponse>, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can enroll users",
        ));
    }
    service::course::require_open(&st.db, &course).await?;

    let target = UserId::from_key(&req.user_id);
    let enrollment =
        service::enrollment::enroll(&st.db, course.get_id(), &target, user.get_id()).await?;
    let people = person_map([target, user.get_id().clone()], &st.db).await?;
    Ok(Json(EnrollmentResponse::new(&enrollment, &people)))
}

/// List a course's roster, paged via `?limit=&offset=` (omit `limit` for the
/// whole roster). Requires teacher+ and course management rights — students see
/// their own courses via `GET /courses/me`. Returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/enrollments",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id"), PageParams),
    responses(
        (status = 200, description = "A page of enrollments (the whole roster when unpaged)", body = Page<EnrollmentResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_roster(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<EnrollmentResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Course must exist — a missing course is a 404, not an empty roster.
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can list the roster",
        ));
    }
    let (rows, total) =
        service::enrollment::list_for_course(&st.db, course.get_id(), limit, offset).await?;
    // Join people onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        rows.iter()
            .flat_map(|e| [e.get_user().clone(), e.get_enrolled_by().clone()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|e| EnrollmentResponse::new(e, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Unenroll a user from a course. Requires teacher+ and course management
/// rights. Existing exam results are kept (they disappear from the user's marks
/// report until re-enrolled).
#[utoipa::path(
    delete,
    path = "/{id}/enrollments/{user}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Course id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Unenrolled"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn unenroll(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can unenroll users",
        ));
    }
    service::course::require_open(&st.db, &course).await?;
    service::enrollment::unenroll(&st.db, course.get_id(), &UserId::from_key(&target)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- exams in a course ----------------------------------------------------

/// Create an exam inside a course. Requires teacher+ and course management
/// rights; the exam's marks count into the course average with its kind's
/// weight (`GET /settings`). Omit `mode` for an offline-graded exam nobody
/// can sit; `sync`/`async` take a window (async also `duration_ms`), `open`
/// is sittable anytime with an optional per-attempt `duration_ms`.
/// `max_attempts` (default 1, `0` = unlimited) meters retakes and
/// `allow_rejoin` (default `true`) is the exam-room door — both stay editable
/// while the exam runs. Send `draft: true` to keep the exam private while
/// it's still being written: only the course's managers see it, and sitting
/// and grading are blocked until it's published (`PATCH` `draft: false`).
#[utoipa::path(
    post,
    path = "/{id}/exams",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = CreateExamInCourse,
    responses(
        (status = 201, description = "Exam created", body = ExamResponse),
        (status = 400, description = "Invalid fields, kind, attempt limit, or schedule (malformed window, duration exceeding the window, or times in the past)", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_exam_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateExamInCourse>,
) -> Result<(StatusCode, Json<ExamResponse>), AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can add exams to this course",
        ));
    }
    service::course::require_open(&st.db, &course).await?;

    let title = ExamTitle::try_new(&req.title)?;
    let description = ExamDescription::try_new(&req.description.unwrap_or_default())?;
    let school = service::settings::load(&st.db).await?;
    let kind = ExamKind::try_new(&req.kind, school.get_exam_kinds())?;
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
        course.get_id(),
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

/// List a course's exams, paged via `?limit=&offset=` (omit `limit` for all of
/// them). Visible to the course's enrolled users, its creator, and
/// managers/admins — but drafts appear only to the course's managers.
/// Returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/exams",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id"), PageParams),
    responses(
        (status = 200, description = "A page of the course's exams (all of them when unpaged)", body = Page<ExamResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not the course creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_course_exams(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ExamResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Course must exist — a missing course is a 404, not an empty exam list.
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
        ));
    }
    let mut exams = crate::service::exam::list_for_course(&st.db, course.get_id()).await?;
    // Drafts are the managers' workbench — enrolled students don't see them.
    if !can_manage_course(&course, &user) {
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

// ---- subjects in a course ---------------------------------------------------
// The course's curriculum topics. Every exam question links to one, so the
// list doubles as the tag picker when authoring questions.

#[derive(Deserialize, ToSchema)]
struct CreateSubject {
    #[schema(max_length = 200, example = "Limits and continuity")]
    name: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
}

/// Create a subject inside a course. Requires teacher+ and course management
/// rights. Subjects are the course's curriculum topics — every exam question
/// must be tagged with one of its course's subjects.
#[utoipa::path(
    post,
    path = "/{id}/subjects",
    tag = "subjects",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = CreateSubject,
    responses(
        (status = 201, description = "Subject created", body = SubjectResponse),
        (status = 400, description = "Invalid name or description", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_subject_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateSubject>,
) -> Result<(StatusCode, Json<SubjectResponse>), AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can add subjects to this course",
        ));
    }
    service::course::require_open(&st.db, &course).await?;

    let name = SubjectName::try_new(&req.name)?;
    let description = SubjectDescription::try_new(&req.description.unwrap_or_default())?;
    let subject = service::subject::create(&st.db, course.get_id(), name, description).await?;
    Ok((StatusCode::CREATED, Json(SubjectResponse::new(&subject))))
}

/// List a course's subjects in creation order, paged via `?limit=&offset=`
/// (omit `limit` for all of them). Visible to the course's enrolled users, its
/// creator, and managers/admins. Returns a `{items, total, limit, offset}`
/// envelope.
#[utoipa::path(
    get,
    path = "/{id}/subjects",
    tag = "subjects",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id"), PageParams),
    responses(
        (status = 200, description = "A page of the course's subjects (all of them when unpaged)", body = Page<SubjectResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not the course creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_course_subjects(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<SubjectResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Course must exist — a missing course is a 404, not an empty list.
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
        ));
    }
    let (subjects, total) =
        service::subject::list_for_course(&st.db, course.get_id(), limit, offset).await?;
    let items = subjects.iter().map(SubjectResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

// ---- homework in a course --------------------------------------------------
// A teacher assigns homework per course, tagged with one of the course's
// subjects and due at a future time. `assigned` optionally narrows it to a
// subset of the enrolled students; omit it for the whole course.

#[derive(Deserialize, ToSchema)]
struct CreateHomework {
    #[schema(max_length = 200, example = "Read chapter 3 and answer Q1-Q5")]
    title: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// The course subject this homework belongs to
    /// (`GET /courses/{id}/subjects`). Required — every homework is tagged with
    /// one of its course's subjects.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    subject_id: String,
    /// When the homework is due, UTC unix-milliseconds. Required; must not be
    /// in the past. Late submissions are still accepted, just flagged late.
    #[schema(example = 1_900_000_000_000_i64)]
    due_at: i64,
    /// The students this homework is for: a list of enrolled student ids. Omit,
    /// send `null`, or send `[]` to assign the whole enrolled course (whoever is
    /// enrolled when they submit); a subset caps at 200 named students.
    #[schema(max_items = 200)]
    assigned: Option<Vec<String>>,
}

/// Assign a homework inside a course. Requires teacher+ and course management
/// rights. The homework is tagged with one of the course's subjects and given a
/// future `due_at`; `assigned` optionally narrows it to a subset of the enrolled
/// students (omit or empty = the whole course).
#[utoipa::path(
    post,
    path = "/{id}/homework",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = CreateHomework,
    responses(
        (status = 201, description = "Homework created", body = HomeworkResponse),
        (status = 400, description = "Invalid fields, a due date in the past, an unknown subject (or one from another course), or an assigned student not enrolled / over the cap", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_homework_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateHomework>,
) -> Result<(StatusCode, Json<HomeworkResponse>), AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can add homework to this course",
        ));
    }
    service::course::require_open(&st.db, &course).await?;

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
    let subject = service::subject::in_course(&st.db, &req.subject_id, course.get_id()).await?;
    let assigned = resolve_assigned(req.assigned, course.get_id(), &st.db).await?;
    let homework = Homework::create(
        course.get_id(),
        &subject,
        title,
        description,
        due_at,
        assigned,
        user.get_id(),
        &st.db,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(HomeworkResponse::new(&homework))))
}

/// List a course's homework, newest first, paged via `?limit=&offset=` (omit
/// `limit` for all of it). Visible to the course's enrolled users, its creator,
/// its assigned teachers, and managers/admins — but a student sees only the
/// homework they are assigned (whole-course ones plus subsets that name them,
/// each with its `assigned` narrowed to themselves).
/// Returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/homework",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id"), PageParams),
    responses(
        (status = 200, description = "A page of the course's homework (all of it when unpaged)", body = Page<HomeworkResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not the course creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_course_homework(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<HomeworkResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Course must exist — a missing course is a 404, not an empty homework list.
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
        ));
    }
    let mut homework = Homework::list_for_course(course.get_id(), &st.db).await?;
    // A student sees only the homework they are assigned; managers see all.
    let manages = can_manage_course(&course, &user);
    if !manages {
        homework.retain(|hw| hw.student_sees(user.get_id()));
    }
    let total = homework.len() as i64;
    // Paged in the web layer: the audience filter above is per-row Rust. A
    // subset roster goes out whole only to a manager of the course; a student
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

// ---- sessions in a course --------------------------------------------------

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

/// Create a lesson session inside a course. Requires teacher+ and course
/// management rights. The session's teacher defaults to the caller.
#[utoipa::path(
    post,
    path = "/{id}/sessions",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = CreateSessionInCourse,
    responses(
        (status = 201, description = "Session created", body = SessionResponse),
        (status = 400, description = "Invalid fields, time range, times in the past, or teacher", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_session_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateSessionInCourse>,
) -> Result<(StatusCode, Json<SessionResponse>), AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can add sessions to this course",
        ));
    }
    service::course::require_open(&st.db, &course).await?;

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
        course.get_id(),
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

/// List a course's lesson sessions, most recent first, paged via
/// `?limit=&offset=` (omit `limit` for all of them). Visible to the course's
/// enrolled users, its creator, and managers/admins. Returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/sessions",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id"), PageParams),
    responses(
        (status = 200, description = "A page of the course's sessions (all of them when unpaged)", body = Page<SessionResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not the course creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_course_sessions(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<SessionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Course must exist — a missing course is a 404, not an empty list.
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
        ));
    }
    let (rows, total) =
        service::course_session::list_for_course(&st.db, course.get_id(), limit, offset).await?;
    // Join teachers onto the page alone — the lookup shrinks with the window.
    let people = person_map(rows.iter().map(|s| s.get_teacher().clone()), &st.db).await?;
    let items = rows
        .iter()
        .map(|s| SessionResponse::new(s, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;
    use crate::domain::user::{Password, Username};

    /// A user at `role`, minted through the real create path.
    async fn user(username: &str, role: Role, db: &Database) -> User {
        let hash = Password::try_new("secret1")
            .unwrap()
            .hash_async()
            .await
            .unwrap();
        let user = crate::service::user::create(db, Username::try_new(username).unwrap(), hash)
            .await
            .unwrap();
        crate::service::user::set_role(db, user.get_id(), role)
            .await
            .unwrap()
            .0
    }

    /// A course `creator` made, with nobody assigned.
    async fn course(creator: &User, db: &Database) -> Course {
        service::course::create(
            db,
            creator.get_id(),
            CourseTitle::try_new("Matematik").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::try_new("course").unwrap(),
            None,
            None,
        )
        .await
        .unwrap()
    }

    /// The leak: `creator` is a historical column that demotion never sweeps,
    /// so the grant itself has to re-read the live role on every call.
    #[tokio::test]
    async fn demoted_creator_loses_management_and_ownership() {
        let db = init_mem().await.unwrap();
        let creator = user("teacher", Role::Teacher, &db).await;
        let course = course(&creator, &db).await;
        assert!(can_manage_course(&course, &creator));
        assert!(owns_course(&course, &creator));

        for role in [Role::Student, Role::Parent] {
            let demoted = crate::service::user::set_role(&db, creator.get_id(), role)
                .await
                .unwrap()
                .0;
            assert!(
                !can_manage_course(&course, &demoted),
                "{role:?} creator still manages the course"
            );
            assert!(
                !owns_course(&course, &demoted),
                "{role:?} creator still owns the course"
            );
        }
    }

    /// The same floor on the assignment list — the demotion sweep clears it,
    /// but the grant must not depend on that sweep having run.
    #[tokio::test]
    async fn demoted_assigned_teacher_loses_management() {
        let db = init_mem().await.unwrap();
        let creator = user("creator", Role::Teacher, &db).await;
        let assigned = user("assigned", Role::Teacher, &db).await;
        let course =
            service::course::assign_teacher(&db, &course(&creator, &db).await, assigned.get_id())
                .await
                .unwrap();
        // Still teacher+: untouched by the floor.
        assert!(can_manage_course(&course, &assigned));
        // ...but never an owner, assigned or not.
        assert!(!owns_course(&course, &assigned));

        let demoted = crate::service::user::set_role(&db, assigned.get_id(), Role::Student)
            .await
            .unwrap()
            .0;
        assert!(!can_manage_course(&course, &demoted));
    }

    /// A manager may schedule a lesson on a teacher's behalf, and the session
    /// belongs to the teacher they named — not to the caller. Everything hung
    /// off that column (who may take the roll call, and who the roll call
    /// credits) follows it, so a handler that stored the caller instead would
    /// hand the office staff somebody else's lesson.
    #[tokio::test]
    async fn a_manager_scheduling_a_lesson_names_the_teacher_not_themselves() {
        let db = init_mem().await.unwrap();
        let manager = user("manager", Role::Manager, &db).await;
        let teacher = user("teacher", Role::Teacher, &db).await;
        let course = course(&manager, &db).await;
        let st = AppState {
            db: db.clone(),
            tenants: crate::database::init_mem_tenants().await.unwrap(),
            files_path: std::env::temp_dir(),
            cookie_secure: false,
            rate_limit: crate::rate_limit::RateLimitConfig::unlimited(),
            chatbot_limit: Default::default(),
            exam_presence: Default::default(),
            board_hub: Default::default(),
            db_up: Default::default(),
            ai: None,
            metrics: crate::telemetry::Metrics::noop(),
        };

        let (status, Json(session)) = create_session_in_course(
            State(st),
            RequireTeacher(manager.clone()),
            Path(course.get_id().key().to_string()),
            Json(CreateSessionInCourse {
                topic: None,
                teacher_id: Some(teacher.get_id().key().to_string()),
                starts_at: Timestamp::now().as_millis() + 60_000,
                ends_at: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        // Read back off the stored row, which is what `SessionResponse` names.
        assert_eq!(session.teacher.id, teacher.get_id().key());
        assert_ne!(
            session.teacher.id,
            manager.get_id().key(),
            "the caller took the lesson instead of the teacher they named"
        );
    }

    /// No course is left orphaned by the floor: manager+ reaches a course whose
    /// creator was demoted and which has no assigned teachers.
    #[tokio::test]
    async fn manager_still_manages_a_demoted_creators_course() {
        let db = init_mem().await.unwrap();
        let creator = user("teacher", Role::Teacher, &db).await;
        let course = course(&creator, &db).await;
        crate::service::user::set_role(&db, creator.get_id(), Role::Student)
            .await
            .unwrap();

        for role in [Role::Manager, Role::Admin] {
            let boss = user(&format!("boss{}", role.as_str()), role, &db).await;
            assert!(can_manage_course(&course, &boss), "{role:?} locked out");
            assert!(owns_course(&course, &boss), "{role:?} cannot delete");
        }
    }
}
