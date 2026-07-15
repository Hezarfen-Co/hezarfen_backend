use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::course::{Course, CourseDescription, CourseId, CourseTitle};
use crate::domain::course_session::{CourseSession, SessionTopic};
use crate::domain::enrollment::Enrollment;
use crate::domain::exam::{
    Exam, ExamAttemptLimit, ExamDescription, ExamDuration, ExamKind, ExamMode, ExamSchedule,
    ExamTitle,
};
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::sessions::resolve_session_teacher;
use super::terms::resolve_term;
use super::{
    CourseResponse, CurrentUser, ExamResponse, Page, PageParams, PersonRef, RequireTeacher,
    SessionResponse, check_not_past, check_time_range, paginate, person_map, set_or_clear,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_course, list_courses))
        .routes(routes!(my_courses))
        .routes(routes!(get_course, update_course, delete_course))
        .routes(routes!(enroll, list_roster))
        .routes(routes!(unenroll))
        .routes(routes!(create_exam_in_course, list_course_exams))
        .routes(routes!(create_session_in_course, list_course_sessions))
}

#[derive(Deserialize, ToSchema)]
struct CreateCourse {
    #[schema(example = "Algebra")]
    title: String,
    description: Option<String>,
    /// The academic term this course belongs to (`GET /terms`). Optional.
    term_id: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateCourse {
    title: Option<String>,
    description: Option<String>,
    /// Omit to keep the current term, send `null` to unlink, or send a term
    /// id to (re)assign.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    term_id: Option<Option<String>>,
}

#[derive(Deserialize, ToSchema)]
struct EnrollUser {
    /// The user to enroll.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    user_id: String,
}

#[derive(Deserialize, ToSchema)]
struct CreateExamInCourse {
    #[schema(example = "Midterm")]
    title: String,
    description: Option<String>,
    /// The assessment form — one of the school's exam kinds (`GET /settings`;
    /// defaults: `homework`, `quiz`, `midterm`, `final`, `project`, `oral`).
    /// The kind's settings-configured weight decides how heavily the exam
    /// counts into the course average.
    #[schema(example = "midterm")]
    kind: String,
    /// `sync` (one fixed window for everyone), `async` (each student starts
    /// inside the window and gets `duration_ms`), or `open` (no window — sit
    /// anytime). Omit for an offline-graded draft that cannot be sat.
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
    #[schema(example = 5_400_000_i64)]
    duration_ms: Option<i64>,
    /// How many attempts each student gets, `1`–`100`, or `0` for unlimited.
    /// Defaults to `1` — the classic single sitting.
    #[schema(example = 1)]
    max_attempts: Option<i64>,
    /// Whether a student who left the exam room may come back in and keep
    /// answering. Defaults to `true`; editable live while the exam runs.
    allow_rejoin: Option<bool>,
}

#[derive(Serialize, ToSchema)]
struct EnrollmentResponse {
    id: String,
    course: String,
    /// The enrolled student.
    user: PersonRef,
    /// Who enrolled them.
    enrolled_by: PersonRef,
}

impl EnrollmentResponse {
    fn new(enrollment: &Enrollment, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: enrollment.get_id().key().to_string(),
            course: enrollment.get_course().key().to_string(),
            user: PersonRef::resolve(people, enrollment.get_user()),
            enrolled_by: PersonRef::resolve(people, enrollment.get_enrolled_by()),
        }
    }
}

/// Who may write inside a specific course (edit/delete it, enroll, add exams,
/// grade): its creator, or anyone `manager` and above. Callers have already
/// cleared the `teacher` bar via `RequireTeacher`.
pub(crate) fn can_manage_course(course: &Course, user: &User) -> bool {
    course.is_creator(user.get_id()) || user.get_role().at_least(Role::Manager)
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
        Enrollment::read_for_user(course.get_id(), user.get_id(), db)
            .await?
            .is_some(),
    )
}

/// The catalog as one user sees it: every course for manager+, otherwise the
/// courses they created plus the ones they're enrolled in, newest first.
pub(crate) async fn visible_courses(user: &User, db: &Database) -> Result<Vec<Course>, AppError> {
    if user.get_role().at_least(Role::Manager) {
        return Course::list_all(db).await;
    }
    let mut courses = Course::list_created(user.get_id(), db).await?;
    for course in Course::list_enrolled(user.get_id(), db).await? {
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

/// Create a course owned by the current user. Requires the `teacher` role or higher.
#[utoipa::path(
    post,
    path = "/",
    tag = "courses",
    security(("session_cookie" = [])),
    request_body = CreateCourse,
    responses(
        (status = 201, description = "Course created", body = CourseResponse),
        (status = 400, description = "Invalid fields", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn create_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Json(req): Json<CreateCourse>,
) -> Result<(StatusCode, Json<CourseResponse>), AppError> {
    let title = CourseTitle::try_new(&req.title)?;
    let description = CourseDescription::try_new(&req.description.unwrap_or_default())?;
    let term = resolve_term(req.term_id.as_deref(), &st.db).await?;
    let course = Course::create(user.get_id(), title, description, term, &st.db).await?;
    Ok((StatusCode::CREATED, Json(CourseResponse::new(&course))))
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
    let items = paginate(&courses, limit, offset)
        .iter()
        .map(CourseResponse::new)
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
    let courses = Course::list_enrolled(user.get_id(), &st.db).await?;
    let total = courses.len() as i64;
    let items = paginate(&courses, limit, offset)
        .iter()
        .map(CourseResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single course by id. Visible to its enrolled users, its creator,
/// and managers/admins.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 200, description = "The course", body = CourseResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not the creator, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_course(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, or a manager/admin can view this course",
        ));
    }
    Ok(Json(CourseResponse::new(&course)))
}

/// Update a course. Requires teacher+; the creator may edit their own course
/// and managers/admins may edit anyone's. Omitted fields keep their value.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = UpdateCourse,
    responses(
        (status = 200, description = "Updated course", body = CourseResponse),
        (status = 400, description = "Invalid fields", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn update_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateCourse>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can edit this course",
        ));
    }

    let title = match req.title {
        Some(ref title) => CourseTitle::try_new(title)?,
        None => course.get_title().clone(),
    };
    let description = match req.description {
        Some(ref description) => CourseDescription::try_new(description)?,
        None => course.get_description().clone(),
    };
    let term = match req.term_id {
        // Explicit `null` clears the link; a value must name a real term.
        Some(update) => resolve_term(update.as_deref(), &st.db).await?,
        None => course.get_term().cloned(),
    };

    let updated = course.update(title, description, term, &st.db).await?;
    Ok(Json(CourseResponse::new(&updated)))
}

/// Delete a course. Requires teacher+; the creator may delete their own course
/// and managers/admins may delete anyone's. Cascades the course's exams, their
/// results, and all enrollments.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn delete_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can delete this course",
        ));
    }
    course.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- enrollments ----------------------------------------------------------

/// Enroll a user into a course (idempotent upsert). Requires teacher+ and
/// course management rights.
#[utoipa::path(
    post,
    path = "/{id}/enrollments",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = EnrollUser,
    responses(
        (status = 200, description = "Enrolled (or already enrolled)", body = EnrollmentResponse),
        (status = 400, description = "Unknown user", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn enroll(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<EnrollUser>,
) -> Result<Json<EnrollmentResponse>, AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can enroll users",
        ));
    }

    let target = UserId::from_key(&req.user_id);
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };

    let enrollment = Enrollment::enroll(course.get_id(), &target, user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&target_user, &user]);
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
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
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can list the roster",
        ));
    }
    let enrollments = Enrollment::list_for_course(course.get_id(), &st.db).await?;
    let total = enrollments.len() as i64;
    // Join people onto the page alone — the lookup shrinks with the window.
    let rows = paginate(&enrollments, limit, offset);
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn unenroll(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can unenroll users",
        ));
    }
    let removed = Enrollment::remove(course.get_id(), &UserId::from_key(&target), &st.db).await?;
    if removed.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- exams in a course ----------------------------------------------------

/// Create an exam inside a course. Requires teacher+ and course management
/// rights; the exam's marks count into the course average with its kind's
/// weight (`GET /settings`). Omit `mode` for an offline-graded draft nobody
/// can sit; `sync`/`async` take a window (async also `duration_ms`), `open`
/// is sittable anytime with an optional per-attempt `duration_ms`.
/// `max_attempts` (default 1, `0` = unlimited) meters retakes and
/// `allow_rejoin` (default `true`) is the exam-room door — both stay editable
/// while the exam runs.
#[utoipa::path(
    post,
    path = "/{id}/exams",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = CreateExamInCourse,
    responses(
        (status = 201, description = "Exam created", body = ExamResponse),
        (status = 400, description = "Invalid fields, kind, attempt limit, or schedule (malformed window, or times in the past)", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn create_exam_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateExamInCourse>,
) -> Result<(StatusCode, Json<ExamResponse>), AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can add exams to this course",
        ));
    }

    let title = ExamTitle::try_new(&req.title)?;
    let description = ExamDescription::try_new(&req.description.unwrap_or_default())?;
    let school = Settings::load(&st.db).await?;
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
    let exam = Exam::create(
        user.get_id(),
        course.get_id(),
        title,
        description,
        kind,
        schedule,
        max_attempts,
        req.allow_rejoin.unwrap_or(true),
        &st.db,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(ExamResponse::new(&exam))))
}

/// List a course's exams, paged via `?limit=&offset=` (omit `limit` for all of
/// them). Visible to the course's enrolled users, its creator, and
/// managers/admins. Returns a `{items, total, limit, offset}` envelope.
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
        (status = 403, description = "Not enrolled, not the creator, and not a manager/admin", body = ErrorResponse),
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
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, or a manager/admin can view this course",
        ));
    }
    let exams = Exam::list_for_course(course.get_id(), &st.db).await?;
    let total = exams.len() as i64;
    let items = paginate(&exams, limit, offset)
        .iter()
        .map(ExamResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

// ---- sessions in a course --------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct CreateSessionInCourse {
    /// What the lesson covers. Optional.
    #[schema(example = "Limits and continuity")]
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn create_session_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateSessionInCourse>,
) -> Result<(StatusCode, Json<SessionResponse>), AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can add sessions to this course",
        ));
    }

    let topic = SessionTopic::try_new(&req.topic.unwrap_or_default())?;
    let teacher = resolve_session_teacher(req.teacher_id.as_deref(), &user, &st.db).await?;
    let starts_at = Timestamp::from_millis(req.starts_at);
    let ends_at = req.ends_at.map(Timestamp::from_millis);
    check_not_past("starts_at", Some(starts_at))?;
    check_not_past("ends_at", ends_at)?;
    check_time_range(Some(starts_at), ends_at)?;

    let session = CourseSession::create(
        course.get_id(),
        teacher.get_id(),
        topic,
        starts_at,
        ends_at,
        &st.db,
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
        (status = 403, description = "Not enrolled, not the creator, and not a manager/admin", body = ErrorResponse),
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
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, or a manager/admin can view this course",
        ));
    }
    let sessions = CourseSession::list_for_course(course.get_id(), &st.db).await?;
    let total = sessions.len() as i64;
    // Join teachers onto the page alone — the lookup shrinks with the window.
    let rows = paginate(&sessions, limit, offset);
    let people = person_map(rows.iter().map(|s| s.get_teacher().clone()), &st.db).await?;
    let items = rows
        .iter()
        .map(|s| SessionResponse::new(s, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}
