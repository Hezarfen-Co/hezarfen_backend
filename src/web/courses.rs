use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::course::{Course, CourseDescription, CourseId, CourseTitle};
use crate::domain::enrollment::Enrollment;
use crate::domain::exam::{Exam, ExamDescription, ExamKind, ExamTitle, ExamWeight};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{CourseResponse, CurrentUser, ExamResponse, RequireTeacher};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_course, list_courses))
        .routes(routes!(my_courses))
        .routes(routes!(get_course, update_course, delete_course))
        .routes(routes!(enroll, list_roster))
        .routes(routes!(unenroll))
        .routes(routes!(create_exam_in_course, list_course_exams))
}

#[derive(Deserialize, ToSchema)]
struct CreateCourse {
    #[schema(example = "Algebra")]
    title: String,
    description: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateCourse {
    title: Option<String>,
    description: Option<String>,
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
    /// The assessment form: `homework`, `quiz`, `midterm`, `final`, `project`,
    /// or `oral`. Informational — `weight` drives the course average.
    #[schema(example = "midterm")]
    kind: String,
    /// How many times this exam counts into the course average, `1`–`100`.
    #[schema(example = 3)]
    weight: i64,
}

#[derive(Serialize, ToSchema)]
struct EnrollmentResponse {
    id: String,
    course: String,
    user: String,
    enrolled_by: String,
}

impl EnrollmentResponse {
    fn new(enrollment: &Enrollment) -> Self {
        Self {
            id: enrollment.get_id().key().to_string(),
            course: enrollment.get_course().key().to_string(),
            user: enrollment.get_user().key().to_string(),
            enrolled_by: enrollment.get_enrolled_by().key().to_string(),
        }
    }
}

/// Who may write inside a specific course (edit/delete it, enroll, add exams,
/// grade): its creator, or anyone `manager` and above. Callers have already
/// cleared the `teacher` bar via `RequireTeacher`.
pub(crate) fn can_manage_course(course: &Course, user: &User) -> bool {
    course.is_creator(user.get_id()) || user.get_role().at_least(Role::Manager)
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
    let course = Course::create(user.get_id(), title, description, &st.db).await?;
    Ok((StatusCode::CREATED, Json(CourseResponse::new(&course))))
}

/// List all courses.
#[utoipa::path(
    get,
    path = "/",
    tag = "courses",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "All courses", body = [CourseResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_courses(
    State(st): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<CourseResponse>>, AppError> {
    let courses = Course::list_all(&st.db).await?;
    Ok(Json(courses.iter().map(CourseResponse::new).collect()))
}

/// The courses the current user is enrolled in.
#[utoipa::path(
    get,
    path = "/me",
    tag = "courses",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's enrolled courses", body = [CourseResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_courses(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<Vec<CourseResponse>>, AppError> {
    let courses = Course::list_enrolled(user.get_id(), &st.db).await?;
    Ok(Json(courses.iter().map(CourseResponse::new).collect()))
}

/// Fetch a single course by id.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 200, description = "The course", body = CourseResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_course(
    State(st): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = Course::read(&CourseId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
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

    let updated = course.update(title, description, &st.db).await?;
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
    if User::read(&target, &st.db).await?.is_none() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    }

    let enrollment = Enrollment::enroll(course.get_id(), &target, user.get_id(), &st.db).await?;
    Ok(Json(EnrollmentResponse::new(&enrollment)))
}

/// List a course's roster. Requires teacher+ — students see their own courses
/// via `GET /courses/me`.
#[utoipa::path(
    get,
    path = "/{id}/enrollments",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 200, description = "All enrollments", body = [EnrollmentResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_roster(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<Vec<EnrollmentResponse>>, AppError> {
    let course_id = CourseId::from_key(&id);
    // Course must exist — a missing course is a 404, not an empty roster.
    Course::read(&course_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let enrollments = Enrollment::list_for_course(&course_id, &st.db).await?;
    Ok(Json(
        enrollments.iter().map(EnrollmentResponse::new).collect(),
    ))
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
/// rights; the exam's marks count `weight` times into the course average.
#[utoipa::path(
    post,
    path = "/{id}/exams",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = CreateExamInCourse,
    responses(
        (status = 201, description = "Exam created", body = ExamResponse),
        (status = 400, description = "Invalid fields, kind, or weight", body = ErrorResponse),
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
    let kind = ExamKind::try_new(&req.kind)?;
    let weight = ExamWeight::try_new(req.weight)?;
    let exam = Exam::create(
        user.get_id(),
        course.get_id(),
        title,
        description,
        kind,
        weight,
        &st.db,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(ExamResponse::new(&exam))))
}

/// List a course's exams.
#[utoipa::path(
    get,
    path = "/{id}/exams",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 200, description = "The course's exams", body = [ExamResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_course_exams(
    State(st): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<ExamResponse>>, AppError> {
    let course_id = CourseId::from_key(&id);
    // Course must exist — a missing course is a 404, not an empty exam list.
    Course::read(&course_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let exams = Exam::list_for_course(&course_id, &st.db).await?;
    Ok(Json(exams.iter().map(ExamResponse::new).collect()))
}
