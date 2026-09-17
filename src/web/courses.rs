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
use crate::domain::role::Role;
use crate::domain::subject::{SubjectDescription, SubjectName};
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::service::course::{can_manage_course, can_view_course};
use crate::state::AppState;

use super::{
    CourseResponse, CurrentUser, Page, PageParams, PersonRef, RequireTeacher, SubjectResponse,
    course_people, paginate, person_map, remove_blob,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_course, list_courses))
        .routes(routes!(my_courses))
        .routes(routes!(get_course, update_course, delete_course))
        .routes(routes!(join_member, list_members))
        .routes(routes!(leave_member))
}

// The route pair below is mounted under `/courses` but *belongs* to another
// module, so it is split out to carry that module's gate as well as the course
// one — see `crate::web::module_gate`. It is merged back in `build_router`, so
// the URL space is unchanged. The exam, session and homework pairs that used to
// sit here moved to `/instances/{id}/...` (see [`super::instances`]), because
// they hang off the instance, not off the catalog row.

pub fn subject_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_subject_in_course, list_course_subjects))
}

#[derive(Deserialize, ToSchema)]
struct CreateCourse {
    #[schema(max_length = 200, example = "Algebra")]
    title: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// `course` (a regular class — the default), `study` (a supervised study
    /// session — etüt), or `club` (a student club — kulüp). Only a `course` is
    /// taught through şube instances (and carries exams, sessions and
    /// homework); a `study`/`club` is joined school-wide.
    #[schema(example = "course")]
    kind: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct JoinMember {
    /// The user to add to the club/etüt.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    user_id: String,
}

/// Public shape of one individual membership in a club or etüt — the
/// school-scoped tier, distinct from a class instance's roster.
#[derive(Serialize, ToSchema)]
struct MembershipResponse {
    #[schema(
        example = "019732e3-7b00-7000-8000-00000000dead_019732e3-7b00-7000-8000-00000000dead"
    )]
    id: String,
    /// The course joined (`GET /courses/{id}`).
    course: String,
    user: PersonRef,
    /// Who placed the membership.
    added_by: PersonRef,
    /// When, UTC unix-milliseconds.
    created_at: i64,
}

impl MembershipResponse {
    fn new(
        membership: &crate::domain::course_membership::CourseMembership,
        people: &std::collections::HashMap<String, PersonRef>,
    ) -> Self {
        Self {
            id: membership.get_id().key(),
            course: membership.get_course().key().to_string(),
            user: PersonRef::resolve(people, membership.get_user()),
            added_by: PersonRef::resolve(people, membership.get_added_by()),
            created_at: membership.get_created_at().as_millis(),
        }
    }
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
}

/// Who may destroy a catalog row: its creator, or anyone `manager` and above.
/// Carries the same live-`teacher` floor as
/// [`crate::service::course::can_manage_course`], and for the same reason: a
/// demoted creator owns nothing.
fn owns_course(course: &Course, user: &User) -> bool {
    can_manage_course(course, user)
}

/// The catalog as one user sees it: every course for manager+, otherwise the
/// courses they are assigned to teach somewhere plus the ones they're enrolled
/// in, newest first.
///
/// The taught half carries the same live-`teacher` floor as
/// [`crate::service::course::can_manage_course`], and for the same reason:
/// `creator` is a historical column no demotion sweeps, so without it a
/// demoted creator kept seeing the course — and, through the `/exams` and
/// `/homework` catalogs that build on this list, its published exams and
/// homework. Below `teacher` a course is visible only the way it is to any
/// other student: by enrollment.
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
    courses.sort_by_key(|course| std::cmp::Reverse(course.get_id().key()));
    Ok(courses)
}

// ---- courses ------------------------------------------------------------

/// Create a catalog course owned by the current user. Requires the `teacher`
/// role or higher. `kind` picks the flavor — `course` (a regular class, the
/// default), `study` (a supervised study session — etüt), or `club` (a
/// student club — kulüp). A catalog row teaches nobody by itself: a şube
/// attaches it into an instance (`POST /classes/{id}/instances`), and a
/// `study`/`club` is joined school-wide (`POST /courses/{id}/members`).
#[utoipa::path(
    post,
    path = "/",
    tag = "courses",
    security(("session_cookie" = [])),
    request_body = CreateCourse,
    responses(
        (status = 201, description = "Course created", body = CourseResponse),
        (status = 400, description = "Invalid fields or kind", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
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
    let course = service::course::create(&st.db, user.get_id(), title, description, kind).await?;
    // The creator is the caller — already loaded, no extra lookup.
    let people = PersonRef::map_of(&[&user]);
    Ok((
        StatusCode::CREATED,
        Json(CourseResponse::new(&course, &people)),
    ))
}

/// List the catalog courses visible to the caller: every course for manager+,
/// otherwise the courses they teach somewhere plus the ones they're enrolled
/// in. Paged via `?limit=&offset=` (omit `limit` for the full list); returns a
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

/// The catalog courses the current user is reached by, paged via
/// `?limit=&offset=` (omit `limit` for all of them); returns a
/// `{items, total, limit, offset}` envelope.
///
/// Both membership tiers are here: a student's enrollments in the instances
/// their şubeler teach, and an individual club/etüt membership. The
/// *instances* themselves are `GET /instances/me`.
#[utoipa::path(
    get,
    path = "/me",
    tag = "courses",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's courses (all of them when unpaged)", body = Page<CourseResponse>),
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
    // Two sources, one list: the courses reached through the instances the
    // caller is enrolled in, and the ones they joined individually. Either
    // read is unpaged — the union is a Rust list, so it is paged here (the
    // `visible_courses` shape).
    let mut courses = service::course::list_enrolled(&st.db, user.get_id(), None, 0)
        .await?
        .0;
    let (joined, _) =
        crate::db::course_membership::list_for_user(&st.db, user.get_id(), None, 0).await?;
    let known: Vec<CourseId> = courses
        .iter()
        .map(|course| course.get_id().clone())
        .collect();
    let mut missing: Vec<CourseId> = joined
        .iter()
        .map(|membership| membership.get_course().clone())
        .filter(|id| !known.contains(id))
        .collect();
    missing.sort_by_key(|id| std::cmp::Reverse(id.key()));
    missing.dedup();
    courses.extend(service::course::list_by_ids(&st.db, &missing).await?);
    courses.sort_by_key(|course| std::cmp::Reverse(course.get_id().key()));
    let total = courses.len() as i64;
    let window = paginate(&courses, limit, offset);
    let people = person_map(window.iter().flat_map(course_people), &st.db).await?;
    let items = window
        .iter()
        .map(|course| CourseResponse::new(course, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single catalog course by id. Visible to the people it reaches —
/// students enrolled in any of its instances, members of the course itself —
/// and to its creator and managers/admins while those accounts are still
/// `teacher`+.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 200, description = "The course", body = CourseResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not reached by this course, and not a still-`teacher`+ course creator or manager/admin", body = ErrorResponse),
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
            "only a student enrolled in one of this course's instances, a member of it, its creator, or a manager/admin can view this course",
        ));
    }
    let people = person_map(course_people(&course), &st.db).await?;
    Ok(Json(CourseResponse::new(&course, &people)))
}

/// Update a catalog course. Requires teacher+ and catalog rights — its
/// creator, or a manager/admin. Omitted fields keep their value.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = UpdateCourse,
    responses(
        (status = 200, description = "Updated course", body = CourseResponse),
        (status = 400, description = "Invalid fields or kind", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
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
            "only the course creator or a manager/admin can edit this course",
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

    let updated = service::course::update(&st.db, course, title, description, kind).await?;
    let people = person_map(course_people(&updated), &st.db).await?;
    Ok(Json(CourseResponse::new(&updated, &people)))
}

/// Delete a catalog course. Requires teacher+; only its creator or a
/// manager/admin may delete it. Refused with a 409 while the course is still
/// taught anywhere — detach it from every şube (`DELETE
/// /classes/{id}/instances/{instance}`) and remove its individual members
/// first, so a course that carries teaching is never dropped by accident.
/// Once free, it cascades the instances' exams (with their results, questions,
/// answers, and question images), homework (with submissions, submission
/// files, and grades), sessions and roll call, its individual memberships, its
/// subjects, and the teacher links. It also strikes its id out of every class
/// blueprint that named it — a template holding a course nothing can resolve
/// is a stocking run that skips it and a `PATCH` that refuses the very list
/// the template already holds.
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
        (status = 409, description = "A class still teaches this course, or students still hold an individual membership in it", body = ErrorResponse),
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
    // The workflow — blob-key collection and the guarded cascade — is
    // [`service::course::delete`]'s.
    // Blob unlinking stays here because only the web layer knows `files_path`.
    let outcome = service::course::delete(&st.db, &course).await?;
    if !outcome.deleted {
        return Err(AppError::Conflict(
            "a class still teaches this course, or students still hold an individual \
             membership in it — detach the classes and remove the members first",
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

// ---- club/etüt membership ---------------------------------------------------

/// Add a user to a club or etüt — the **school-scoped** membership tier.
/// Requires teacher+ and catalog rights (its creator, or a manager/admin).
/// Only students can be added, and only to a `study` (etüt) or `club`
/// (kulüp): a regular ders (`kind` `course`) has no school-wide roster — its
/// students come from the şubeler that teach it, and that join is
/// `POST /instances/{id}/enrollments` (400 here). Idempotent: a pair that
/// already holds a membership is returned as-is.
#[utoipa::path(
    post,
    path = "/{id}/members",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = JoinMember,
    responses(
        (status = 200, description = "Member added (or already a member)", body = MembershipResponse),
        (status = 400, description = "Unknown user, user is not a student, or the course is a class-delivered ders", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn join_member(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<JoinMember>,
) -> Result<Json<MembershipResponse>, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can add members to this course",
        ));
    }
    let target = UserId::from_key(&req.user_id);
    let membership =
        service::enrollment::join_activity(&st.db, course.get_id(), &target, user.get_id()).await?;
    let people = person_map([target, *user.get_id()], &st.db).await?;
    Ok(Json(MembershipResponse::new(&membership, &people)))
}

/// List a club/etüt's members, newest first, paged via `?limit=&offset=`
/// (omit `limit` for the whole list). Requires teacher+ and catalog rights.
/// Returns a `{items, total, limit, offset}` envelope. An instance's roster
/// is `GET /instances/{id}/enrollments`.
#[utoipa::path(
    get,
    path = "/{id}/members",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id"), PageParams),
    responses(
        (status = 200, description = "A page of members (the whole list when unpaged)", body = Page<MembershipResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_members(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<MembershipResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can list this course's members",
        ));
    }
    let (rows, total) =
        crate::db::course_membership::list_for_course(&st.db, course.get_id(), limit, offset)
            .await?;
    // Join people onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        rows.iter()
            .flat_map(|row| [*row.get_user(), *row.get_added_by()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|row| MembershipResponse::new(row, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Remove a user from a club or etüt. Requires teacher+ and catalog rights.
/// Existing exam results and badges are untouched — the membership is a door,
/// not a record of what happened inside. A pair holding no membership is a
/// 404.
#[utoipa::path(
    delete,
    path = "/{id}/members/{user}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Course id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found, or that user held no membership", body = ErrorResponse),
    ),
)]
async fn leave_member(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can remove members from this course",
        ));
    }
    service::enrollment::leave_activity(&st.db, course.get_id(), &UserId::from_key(&target))
        .await?;
    Ok(StatusCode::NO_CONTENT)
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

/// Create a subject inside a course. Requires teacher+ and catalog rights
/// (its creator, or a manager/admin). Subjects are the curriculum topics of the
/// *catalog* row — every exam of every instance teaching it tags its questions
/// with one of them.
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
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
            "only the course creator or a manager/admin can add subjects to this course",
        ));
    }

    let name = SubjectName::try_new(&req.name)?;
    let description = SubjectDescription::try_new(&req.description.unwrap_or_default())?;
    let subject = service::subject::create(&st.db, course.get_id(), name, description).await?;
    Ok((StatusCode::CREATED, Json(SubjectResponse::new(&subject))))
}

/// List a course's subjects in creation order, paged via `?limit=&offset=`
/// (omit `limit` for all of them). Visible to its creator, to managers/admins,
/// and to anyone the course reaches (a student enrolled in one of its
/// instances, or a member of it). Returns a `{items, total, limit, offset}`
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
        (status = 403, description = "Not reached by the course, and not its creator or a manager/admin", body = ErrorResponse),
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
            "only a user this course reaches, its creator, or a manager/admin can view this course",
        ));
    }
    let (subjects, total) =
        service::subject::list_for_course(&st.db, course.get_id(), limit, offset).await?;
    let items = subjects.iter().map(SubjectResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_test_db;
    use crate::domain::user::Username;

    /// A user at `role`, minted through the real create path.
    async fn user(username: &str, role: Role, db: &Database) -> User {
        let user = crate::service::user::create(db, Username::try_new(username).unwrap(), None)
            .await
            .unwrap();
        crate::service::user::set_role(db, user.get_id(), role)
            .await
            .unwrap()
            .0
    }

    /// A catalog course `creator` made.
    async fn course(creator: &User, db: &Database) -> Course {
        service::course::create(
            db,
            creator.get_id(),
            CourseTitle::try_new("Matematik").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::try_new("course").unwrap(),
        )
        .await
        .unwrap()
    }

    /// The leak: `creator` is a historical column that demotion never sweeps,
    /// so the grant itself has to re-read the live role on every call.
    #[tokio::test]
    async fn demoted_creator_loses_management_and_ownership() {
        let (db, _leases) = init_test_db().await;
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

    /// No course is left orphaned by the floor: manager+ reaches a course whose
    /// creator was demoted.
    #[tokio::test]
    async fn manager_still_manages_a_demoted_creators_course() {
        let (db, _leases) = init_test_db().await;
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

    /// The assignment list is off the catalog (D6): a teacher assigned to an
    /// instance runs that instance, but holds no rights over the catalog row
    /// itself — the gate that grants them the instance is
    /// [`super::instances::can_manage_instance`].
    #[tokio::test]
    async fn an_instance_teacher_holds_no_catalog_rights() {
        let (db, _leases) = init_test_db().await;
        let creator = user("owner", Role::Manager, &db).await;
        let assigned = user("assigned", Role::Teacher, &db).await;
        let course = course(&creator, &db).await;
        assert!(!can_manage_course(&course, &assigned));
        assert!(!owns_course(&course, &assigned));
    }
}
