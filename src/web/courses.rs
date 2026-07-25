use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::answer_image::AnswerImage;
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
use crate::domain::course_session::{CourseSession, SessionTopic};
use crate::domain::enrollment::{ENROLL_LOCK, Enrollment};
use crate::domain::exam::{
    Exam, ExamAttemptLimit, ExamDescription, ExamDuration, ExamKind, ExamMode, ExamSchedule,
    ExamTitle,
};
use crate::domain::homework::{Homework, HomeworkTitle};
use crate::domain::homework_file::HomeworkFile;
use crate::domain::question_image::QuestionImage;
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::subject::{Subject, SubjectDescription, SubjectName};
use crate::domain::term::TERM_LOCK;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::homework::{HOMEWORK_LOCK, description_or_none, resolve_assigned};
use super::sessions::resolve_session_teacher;
use super::subjects::subject_in_course;
use super::terms::resolve_term;
use super::{
    CourseResponse, CurrentUser, ExamResponse, HomeworkResponse, Page, PageParams, PersonRef,
    RequireManager, RequireTeacher, SessionResponse, SubjectResponse, check_not_past,
    check_time_range, course_people, paginate, person_map, remove_blob, set_or_clear,
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
        .routes(routes!(create_exam_in_course, list_course_exams))
        .routes(routes!(create_session_in_course, list_course_sessions))
        .routes(routes!(create_subject_in_course, list_course_subjects))
        .routes(routes!(create_homework_in_course, list_course_homework))
}

#[derive(Deserialize, ToSchema)]
struct CreateCourse {
    #[schema(example = "Algebra")]
    title: String,
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
    title: Option<String>,
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
    #[schema(example = 5_400_000_i64)]
    duration_ms: Option<i64>,
    /// How many attempts each student gets, `1`–`100`, or `0` for unlimited.
    /// Defaults to `1` — the classic single sitting.
    #[schema(example = 1)]
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

/// Who may write inside a specific course (edit it, enroll, add exams,
/// sessions, subjects, grade): its creator, a teacher a manager assigned to
/// it, or anyone `manager` and above. Callers have already cleared the
/// `teacher` bar via `RequireTeacher`.
///
/// Deleting the course and changing its teacher list sit *above* this bar —
/// see [`owns_course`].
pub(crate) fn can_manage_course(course: &Course, user: &User) -> bool {
    course.is_creator(user.get_id())
        || course.is_assigned(user.get_id())
        || user.get_role().at_least(Role::Manager)
}

/// Who may destroy a course: its creator, or anyone `manager` and above. An
/// assigned teacher runs the course but does not own it — they cannot delete
/// it out from under the person who made it.
fn owns_course(course: &Course, user: &User) -> bool {
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
    let mut courses = Course::list_for_teacher(user.get_id(), db).await?;
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
    // [`TERM_LOCK`] holds the term lookup and the save together, so the link
    // can't be written onto a term a concurrent delete just cleared. Only a
    // write that actually links a term needs it.
    let _term_guard = match req.term_id {
        Some(_) => Some(TERM_LOCK.lock().await),
        None => None,
    };
    let term = resolve_term(req.term_id.as_deref(), &st.db).await?;
    check_capacity(req.capacity)?;
    let course = Course::create(
        user.get_id(),
        title,
        description,
        kind,
        term,
        req.capacity,
        &st.db,
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
    let courses = Course::list_enrolled(user.get_id(), &st.db).await?;
    let total = courses.len() as i64;
    let window = paginate(&courses, limit, offset);
    let people = person_map(window.iter().flat_map(course_people), &st.db).await?;
    let items = window
        .iter()
        .map(|course| CourseResponse::new(course, &people))
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
        (status = 403, description = "Not enrolled, not the course creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can edit this course",
        ));
    }

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
    // Same [`TERM_LOCK`] window as create — held over the lookup and the save
    // whenever this PATCH links a term (clearing or omitting needs no guard).
    let _term_guard = match req.term_id {
        Some(Some(_)) => Some(TERM_LOCK.lock().await),
        _ => None,
    };
    // Both columns are nullable, so both stay clearable: omitted is `None`
    // (keep), an explicit `null` is `Some(None)` (write `NONE`).
    let term = match req.term_id {
        // Explicit `null` clears the link; a value must name a real term.
        Some(ref update) => Some(resolve_term(update.as_deref(), &st.db).await?),
        None => None,
    };
    // Explicit `null` lifts the cap; a value must be positive.
    check_capacity(req.capacity.flatten())?;
    let capacity = req.capacity;

    let updated = course
        .update(title, description, kind, term, capacity, &st.db)
        .await?;
    let people = person_map(course_people(&updated), &st.db).await?;
    Ok(Json(CourseResponse::new(&updated, &people)))
}

/// Delete a course. Requires teacher+; only its creator or a manager/admin may
/// delete it — an assigned teacher runs the course but does not own it.
/// Refused with a 409 while anyone is still enrolled — empty the roster first,
/// so a course that carries students is never dropped by accident. Once empty,
/// it cascades the course's exams (with their results, questions, answers, and
/// question images), its homework (with submissions, submission files, and
/// grades), its sessions and roll call, and its subjects.
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
        (status = 409, description = "Students are still enrolled in this course", body = ErrorResponse),
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
    if !owns_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can delete this course",
        ));
    }
    // [`ENROLL_LOCK`] holds the roster check and the delete together, so an
    // enroll that just passed its capacity check can't land its row on a
    // course that vanished mid-flight.
    let _guard = ENROLL_LOCK.lock().await;
    if Enrollment::any_for_course(course.get_id(), &st.db).await? {
        return Err(AppError::Conflict(
            "students are still enrolled in this course — remove them first",
        ));
    }
    // Rows go first (the delete cascades them), blobs after — a crash in
    // between strands at worst an unreachable blob.
    let image_files = QuestionImage::file_keys_for_course(course.get_id(), &st.db).await?;
    let answer_image_files = AnswerImage::file_keys_for_course(course.get_id(), &st.db).await?;
    let homework_files = HomeworkFile::file_keys_for_course(course.get_id(), &st.db).await?;
    course.delete(&st.db).await?;
    for file in image_files
        .iter()
        .chain(&answer_image_files)
        .chain(&homework_files)
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
    ),
)]
async fn assign_teacher(
    State(st): State<AppState>,
    RequireManager(_manager): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<AssignTeacher>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let target = UserId::from_key(&req.user_id);
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };
    // Assignment hands out course-management rights, which every gate behind
    // it re-checks against the `teacher` bar — assigning anyone below it would
    // write a row that can never be used.
    if !target_user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "assigned teacher must hold the teacher role or higher",
        }));
    }

    let updated = course.assign_teacher(&target, &st.db).await?;
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
    ),
)]
async fn unassign_teacher(
    State(st): State<AppState>,
    RequireManager(_manager): RequireManager,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let removed = course
        .unassign_teacher(&UserId::from_key(&target), &st.db)
        .await?;
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
        (status = 409, description = "The course is full", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can enroll users",
        ));
    }

    let target = UserId::from_key(&req.user_id);
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };

    // Enrollment is student membership: it gates sitting exams, being graded,
    // and appearing on a lesson roster — all student-only. Staff run courses,
    // they don't enroll in them.
    if target_user.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be enrolled in a course",
        }));
    }

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
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can list the roster",
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can unenroll users",
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
            "only the course creator, an assigned teacher, or a manager/admin can add exams to this course",
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
        req.allow_review.unwrap_or(false),
        req.draft.unwrap_or(false),
        &st.db,
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
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
        ));
    }
    let mut exams = Exam::list_for_course(course.get_id(), &st.db).await?;
    // Drafts are the managers' workbench — enrolled students don't see them.
    if !can_manage_course(&course, &user) {
        exams.retain(|exam| !exam.is_draft());
    }
    let total = exams.len() as i64;
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
    #[schema(example = "Limits and continuity")]
    name: String,
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
    ),
)]
async fn create_subject_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateSubject>,
) -> Result<(StatusCode, Json<SubjectResponse>), AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can add subjects to this course",
        ));
    }

    let name = SubjectName::try_new(&req.name)?;
    let description = SubjectDescription::try_new(&req.description.unwrap_or_default())?;
    let subject = Subject::create(course.get_id(), name, description, &st.db).await?;
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
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
        ));
    }
    let subjects = Subject::list_for_course(course.get_id(), &st.db).await?;
    let total = subjects.len() as i64;
    let items = paginate(&subjects, limit, offset)
        .iter()
        .map(SubjectResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

// ---- homework in a course --------------------------------------------------
// A teacher assigns homework per course, tagged with one of the course's
// subjects and due at a future time. `assigned` optionally narrows it to a
// subset of the enrolled students; omit it for the whole course.

#[derive(Deserialize, ToSchema)]
struct CreateHomework {
    #[schema(example = "Read chapter 3 and answer Q1-Q5")]
    title: String,
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
    ),
)]
async fn create_homework_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateHomework>,
) -> Result<(StatusCode, Json<HomeworkResponse>), AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can add homework to this course",
        ));
    }

    let title = HomeworkTitle::try_new(&req.title)?;
    let description = match req.description.as_deref() {
        Some(text) => description_or_none(text)?,
        None => None,
    };
    let due_at = Timestamp::from_millis(req.due_at);
    check_not_past("due_at", Some(due_at))?;
    // Reader lease of [`HOMEWORK_LOCK`], held from the subject check through
    // the create: a subject delete (a writer, which checks for homework) can't
    // vanish the subject between its validation here and the row landing with
    // it.
    let _guard = HOMEWORK_LOCK.read().await;
    let subject = subject_in_course(&req.subject_id, course.get_id(), &st.db).await?;
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
/// homework they are assigned (whole-course ones plus subsets that name them).
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
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
        ));
    }
    let mut homework = Homework::list_for_course(course.get_id(), &st.db).await?;
    // A student sees only the homework they are assigned; managers see all.
    if !can_manage_course(&course, &user) {
        homework.retain(|hw| hw.student_sees(user.get_id()));
    }
    let total = homework.len() as i64;
    let items = paginate(&homework, limit, offset)
        .iter()
        .map(HomeworkResponse::new)
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can add sessions to this course",
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
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course",
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
