use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::course::Course;
use crate::domain::enrollment::Enrollment;
use crate::domain::parent_link::ParentLink;
use crate::domain::preferences::{Language, Theme};
use crate::domain::profile::{BirthDate, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::dto::Role as RoleSchema;
use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireAdmin, RequireTeacher, UserResponse, paginate,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users))
        .routes(routes!(update_my_profile))
        .routes(routes!(update_my_preferences))
        .routes(routes!(my_students))
        .routes(routes!(search_users))
        .routes(routes!(get_user))
        .routes(routes!(set_role))
        .routes(routes!(update_user_profile))
        .routes(routes!(update_user_preferences))
        .routes(routes!(link_student, list_parent_students))
        .routes(routes!(unlink_student))
}

#[derive(Deserialize, ToSchema)]
struct SetRole {
    /// The role to assign. Deserialized as a string so an unknown value returns a
    /// uniform `400`; documented as the `Role` enum so the docs list the choices.
    #[schema(value_type = RoleSchema, example = "teacher")]
    role: String,
}

/// Partial personal-info update. Per field: omitted (or `null`) keeps the
/// current value, an empty string clears it, anything else is validated and set.
#[derive(Deserialize, ToSchema)]
struct UpdateProfile {
    #[schema(example = "Ada", max_length = 100)]
    name: Option<String>,
    #[schema(example = "Lovelace", max_length = 100)]
    surname: Option<String>,
    #[schema(example = "ada@example.com", max_length = 254)]
    email: Option<String>,
    /// Any punctuation and spacing; it is the digit count that must land in
    /// `7`–`15`, so no character-length bound applies.
    #[schema(example = "+90 555 123 45 67")]
    phone: Option<String>,
    /// Birth date in `YYYY-MM-DD` form.
    #[schema(example = "1990-01-02")]
    birth_date: Option<String>,
}

/// Partial UI-preference update. Same field semantics as [`UpdateProfile`]:
/// omitted (or `null`) keeps the current value, an empty string clears it back
/// to "never chose" (the client then follows the device preference), anything
/// else is validated and set.
#[derive(Deserialize, ToSchema)]
struct UpdatePreferences {
    /// `light` or `dark`.
    #[schema(example = "dark")]
    theme: Option<String>,
    /// `tr` or `en` (ISO 639-1).
    #[schema(example = "tr")]
    language: Option<String>,
}

/// Resolve one patched field into what the save should write: absent (or
/// `null`) is `None` — the column is not written at all, so a concurrent PATCH
/// of it survives; `""` is `Some(None)`, an explicit clear; anything else must
/// parse into the domain newtype, exactly as a create would.
fn merge_field<T>(
    patch: Option<&str>,
    parse: impl Fn(&str) -> Result<T, ValidationError>,
) -> Result<Option<Option<T>>, ValidationError> {
    match patch {
        None => Ok(None),
        Some("") => Ok(Some(None)),
        Some(value) => Ok(Some(Some(parse(value)?))),
    }
}

/// Validate and persist exactly the info fields `req` carried — nothing is
/// merged from `user`'s snapshot, so a concurrent PATCH of another field is not
/// reverted. Shared by the self-service and admin profile endpoints — they
/// differ only in whose row they load and who may call them.
async fn apply_profile(
    user: User,
    req: &UpdateProfile,
    db: &Database,
) -> Result<UserResponse, AppError> {
    let name = merge_field(req.name.as_deref(), |v| PersonName::try_new("name", v))?;
    let surname = merge_field(req.surname.as_deref(), |v| {
        PersonName::try_new("surname", v)
    })?;
    let email = merge_field(req.email.as_deref(), Email::try_new)?;
    let phone = merge_field(req.phone.as_deref(), Phone::try_new)?;
    let birth_date = merge_field(req.birth_date.as_deref(), BirthDate::try_new)?;
    let updated = user
        .set_profile(name, surname, email, phone, birth_date, db)
        .await?;
    Ok(UserResponse::new(&updated))
}

/// Validate and persist exactly the preference fields `req` carried — same
/// no-merge reasoning as [`apply_profile`]. Shared by the
/// self-service and admin preference endpoints — they differ only in whose row
/// they load and who may call them.
async fn apply_preferences(
    user: User,
    req: &UpdatePreferences,
    db: &Database,
) -> Result<UserResponse, AppError> {
    let theme = merge_field(req.theme.as_deref(), Theme::try_from_str)?;
    let language = merge_field(req.language.as_deref(), Language::try_from_str)?;
    let updated = user.set_preferences(theme, language, db).await?;
    Ok(UserResponse::new(&updated))
}

#[derive(Deserialize, IntoParams)]
struct SearchUsers {
    /// Case-insensitive fragment of a username, name, or surname. May be
    /// blank when `role` is given — that lists the whole role.
    q: String,
    /// Restrict matches to one role: `parent`, `student`, `teacher`,
    /// `manager`, or `admin`. Omit to search every role.
    role: Option<String>,
    /// Max matches to return. Omit for every match; when given, `1`–`500`.
    #[param(minimum = 1, maximum = 500, example = 100)]
    limit: Option<i64>,
    /// Matches to skip from the start. Defaults to `0`.
    #[param(minimum = 0, example = 0)]
    offset: Option<i64>,
}

/// Find users by username or name — backs the pickers (enroll, grade, mark
/// attendance). Requires teacher+. `role` narrows to one role (e.g.
/// `role=student` for an enroll picker); a blank `q` with a `role` lists
/// everyone in that role. Paged via `?limit=&offset=` like the other lists
/// (omit `limit` for every match); returns a `{items, total, limit, offset}`
/// envelope carrying only id/username/display name — no contact details.
#[utoipa::path(
    get,
    path = "/search",
    tag = "users",
    security(("session_cookie" = [])),
    params(SearchUsers),
    responses(
        (status = 200, description = "A page of matching users (all matches when unpaged)", body = Page<PersonRef>),
        (status = 400, description = "Blank query without a role, unknown role, or invalid limit/offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn search_users(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Query(req): Query<SearchUsers>,
) -> Result<Json<Page<PersonRef>>, AppError> {
    if req.q.trim().is_empty() && req.role.is_none() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "q",
            reason: "must not be empty unless role is given",
        }));
    }
    let (limit, offset) = PageParams {
        limit: req.limit,
        offset: req.offset,
    }
    .resolve()?;
    let role = req.role.as_deref().map(Role::try_from_str).transpose()?;
    let (users, total) = User::search(&req.q, role, limit, offset, &st.db).await?;
    let items = users.iter().map(PersonRef::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// List every user with their role, newest first. Admin only. Paged: pass
/// `?limit=&offset=` to take a window (omit `limit` for the whole list); the
/// response is a `{items, total, limit, offset}` envelope where `total` counts
/// every user.
#[utoipa::path(
    get,
    path = "/",
    tag = "users",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of users (the full list when unpaged)", body = Page<UserResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
    ),
)]
async fn list_users(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<UserResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (users, total) = User::list_all(limit, offset, &st.db).await?;
    let items = users.iter().map(UserResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Update the caller's own personal info: name, surname, email, phone, birth
/// date. Any authenticated role. Omitted fields stay as they are; an empty
/// string clears a field.
#[utoipa::path(
    patch,
    path = "/me",
    tag = "users",
    security(("session_cookie" = [])),
    request_body = UpdateProfile,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid field", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn update_my_profile(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<UpdateProfile>,
) -> Result<Json<UserResponse>, AppError> {
    Ok(Json(apply_profile(user, &req, &st.db).await?))
}

/// Update the caller's own UI preferences: `theme` (`light`/`dark`) and
/// `language` (`tr`/`en`). Any authenticated role. Omitted fields stay as they
/// are; an empty string clears one back to "never chose" (the client then
/// follows the device preference). Read them back on any user response, e.g.
/// `GET /auth/me`.
#[utoipa::path(
    patch,
    path = "/me/preferences",
    tag = "users",
    security(("session_cookie" = [])),
    request_body = UpdatePreferences,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid theme or language", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn update_my_preferences(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<UpdatePreferences>,
) -> Result<Json<UserResponse>, AppError> {
    Ok(Json(apply_preferences(user, &req, &st.db).await?))
}

/// Fetch one user with their role and personal info. Admin only.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The user", body = UserResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn get_user(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
) -> Result<Json<UserResponse>, AppError> {
    let user = User::read(&UserId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(UserResponse::new(&user)))
}

/// Update any user's personal info. Admin only — the school-office path for
/// maintaining records on behalf of students and staff. Same field semantics
/// as `PATCH /users/me`.
#[utoipa::path(
    patch,
    path = "/{id}/profile",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    request_body = UpdateProfile,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid field", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn update_user_profile(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<UpdateProfile>,
) -> Result<Json<UserResponse>, AppError> {
    let user = User::read(&UserId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(apply_profile(user, &req, &st.db).await?))
}

/// Update any user's UI preferences. Admin only — everyone else manages their
/// own through `PATCH /users/me/preferences`, which this mirrors field for
/// field.
#[utoipa::path(
    patch,
    path = "/{id}/preferences",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    request_body = UpdatePreferences,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid theme or language", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn update_user_preferences(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<UpdatePreferences>,
) -> Result<Json<UserResponse>, AppError> {
    let user = User::read(&UserId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(apply_preferences(user, &req, &st.db).await?))
}

/// Set a user's role. Admin only. An admin cannot change their own role — that
/// guard keeps a sole admin from accidentally locking everyone out of role
/// management (recover such a lockout with the SurrealQL in the README).
/// Setting any non-`student` role also drops the user's course enrollments —
/// only students enroll, so a promoted user leaves every roster. Demoting below
/// `teacher` drops their course teaching assignments for the mirror reason.
#[utoipa::path(
    patch,
    path = "/{id}/role",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    request_body = SetRole,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid role", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin, or attempted to change own role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn set_role(
    State(st): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<SetRole>,
) -> Result<Json<UserResponse>, AppError> {
    let role = Role::try_from_str(&req.role)?;
    let target = UserId::from_key(&id);
    if &target == admin.get_id() {
        return Err(AppError::Forbidden("cannot change your own role"));
    }
    let user = User::read(&target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let updated = user.set_role(role, &st.db).await?;
    // Roster hygiene: only students enroll, so a non-student sheds all their
    // enrollment rows (security checks re-read the live role and never
    // trusted these; this just stops them polluting rosters and counts).
    // Parent links get the same sweep on both sides: only students are
    // observed and only parents observe, so a role change off either end
    // drops the rows instead of leaving dead grants around.
    if role != Role::Student {
        Enrollment::delete_for_user(&target, &st.db).await?;
        ParentLink::delete_where_student(&target, &st.db).await?;
    }
    if role != Role::Parent {
        ParentLink::delete_where_parent(&target, &st.db).await?;
    }
    // Course staffing gets the same sweep: only teacher+ may be assigned to
    // run a course, so a demotion drops every assignment instead of leaving
    // rows that grant nothing and still list a demoted user as its teacher.
    if !role.at_least(Role::Teacher) {
        Course::unassign_everywhere(&target, &st.db).await?;
    }
    Ok(Json(UserResponse::new(&updated)))
}

#[derive(Deserialize, ToSchema)]
struct LinkStudent {
    /// The student to tie to this parent.
    user_id: String,
}

#[derive(Serialize, ToSchema)]
struct ParentLinkResponse {
    parent: PersonRef,
    student: PersonRef,
    /// The admin who created the tie.
    linked_by: PersonRef,
}

/// One page of a parent's observed students, sorted by username. Shared by the
/// admin listing and the parent's own `/me/students` — they differ only in
/// whose links they read and who may call them.
async fn students_page(
    parent: &UserId,
    page: PageParams,
    db: &Database,
) -> Result<Page<PersonRef>, AppError> {
    let (limit, offset) = page.resolve()?;
    let links = ParentLink::list_for_parent(parent, db).await?;
    let ids: Vec<UserId> = links
        .iter()
        .map(|link| link.get_student().clone())
        .collect();
    let mut students = User::list_by_ids(&ids, db).await?;
    students.sort_by(|a, b| a.get_username().as_str().cmp(b.get_username().as_str()));
    let total = students.len() as i64;
    // Paged in the web layer: the rows come from a link table and are
    // re-sorted by username in Rust.
    let items = paginate(&students, limit, offset)
        .iter()
        .map(PersonRef::new)
        .collect();
    Ok(Page::new(items, total, limit, offset))
}

/// Tie a student to a parent account. Admin only — family ties are school-office
/// records, like roles. `{id}` must hold the `parent` role and `user_id` the
/// `student` role; a parent may observe any number of students. Idempotent:
/// linking the same pair again keeps the one tie (the `linked_by` stamp moves
/// to the latest linker, like re-enrolling). The tie grants the
/// parent read access to the student's marks, attendance, and pomodoro reports
/// — nothing else, and never any write.
#[utoipa::path(
    post,
    path = "/{id}/students",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Parent user id")),
    request_body = LinkStudent,
    responses(
        (status = 200, description = "The tie (created or already present)", body = ParentLinkResponse),
        (status = 400, description = "The target is not a parent, or user_id is unknown or not a student", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "Parent user not found", body = ErrorResponse),
    ),
)]
async fn link_student(
    State(st): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<LinkStudent>,
) -> Result<Json<ParentLinkResponse>, AppError> {
    let parent = User::read(&UserId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if parent.get_role() != Role::Parent {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "id",
            reason: "students can only be tied to a parent account",
        }));
    }
    let Some(student) = User::read(&UserId::from_key(&req.user_id), &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };
    if student.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be tied to a parent",
        }));
    }
    let link = ParentLink::link(parent.get_id(), student.get_id(), admin.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&parent, &student, &admin]);
    Ok(Json(ParentLinkResponse {
        parent: PersonRef::resolve(&people, link.get_parent()),
        student: PersonRef::resolve(&people, link.get_student()),
        linked_by: PersonRef::resolve(&people, link.get_linked_by()),
    }))
}

/// List the students tied to a parent account, sorted by username. Admin only
/// — parents read their own list at `GET /users/me/students`. Paged via
/// `?limit=&offset=` like the other lists.
#[utoipa::path(
    get,
    path = "/{id}/students",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Parent user id"), PageParams),
    responses(
        (status = 200, description = "A page of the parent's students", body = Page<PersonRef>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn list_parent_students(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<PersonRef>>, AppError> {
    let parent = UserId::from_key(&id);
    User::read(&parent, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(students_page(&parent, page, &st.db).await?))
}

/// Untie a student from a parent account. Admin only. The student's data is
/// untouched — only the parent's read grant goes away.
#[utoipa::path(
    delete,
    path = "/{id}/students/{student}",
    tag = "users",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Parent user id"),
        ("student" = String, Path, description = "Student user id"),
    ),
    responses(
        (status = 204, description = "Tie removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "No such tie", body = ErrorResponse),
    ),
)]
async fn unlink_student(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path((id, student)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    if ParentLink::remove(&UserId::from_key(&id), &UserId::from_key(&student), &st.db)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The students the calling parent observes, sorted by username. Requires the
/// `parent` role. Each entry's reports live at `GET /marks/{user}`,
/// `GET /attendance/{user}`, and `GET /pomodoro/{user}`. Paged via
/// `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/me/students",
    tag = "users",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's students", body = Page<PersonRef>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires the parent role", body = ErrorResponse),
    ),
)]
async fn my_students(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<PersonRef>>, AppError> {
    if user.get_role() != Role::Parent {
        return Err(AppError::Forbidden("requires the parent role"));
    }
    Ok(Json(students_page(user.get_id(), page, &st.db).await?))
}
