use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{
    MAX_MAX_FILE_BYTES, MAX_PROFILE_CLASSES, MAX_PROFILE_COURSES, UPLOAD_BODY_OVERHEAD_BYTES,
};
use crate::database::Database;
use crate::domain::badge::{self, BadgeAward};
use crate::domain::class_group::{ClassGroup, ClassGroupId};
use crate::domain::class_member::ClassMember;
use crate::domain::course::Course;
use crate::domain::parent_link::ParentLink;
use crate::domain::preferences::{Language, PaletteColor, Theme};
use crate::domain::profile::{Bio, BirthDate, DisplayName, Email, PersonName, Phone, ProfileStats};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::visible_courses;
use super::dto::AssignableRole;
use super::dto::Role as RoleSchema;
use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireAdmin, RequireTeacher, UploadFileForm,
    UserResponse, ensure_can_observe, paginate, read_image_upload, remove_blob, serve_inline_blob,
    store_blob,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users))
        .routes(routes!(update_my_profile))
        .routes(routes!(update_my_preferences))
        .routes(routes!(my_students))
        .routes(routes!(my_profile))
        .routes(routes!(search_users))
        .routes(routes!(get_user))
        .routes(routes!(set_role))
        .routes(routes!(update_user_profile, get_user_profile))
        .routes(routes!(update_user_preferences))
        .routes(routes!(link_student, list_parent_students))
        .routes(routes!(unlink_student))
        // The avatar routes get their own HTTP body cap, like the note-file and
        // question-image ones: the server-wide hard ceiling plus multipart
        // framing headroom. It must stay on this sub-router — on the whole
        // `/users` router every JSON endpoint would start accepting 25 MB.
        .merge(
            OpenApiRouter::new()
                .routes(routes!(upload_my_avatar, get_my_avatar, delete_my_avatar))
                .routes(routes!(get_avatar, delete_avatar))
                .layer(DefaultBodyLimit::max(
                    MAX_MAX_FILE_BYTES as usize + UPLOAD_BODY_OVERHEAD_BYTES,
                )),
        )
}

#[derive(Deserialize, ToSchema)]
struct SetRole {
    /// The role to assign. Deserialized as a string so an unknown value returns a
    /// uniform `400`; documented as the `AssignableRole` enum so the docs list
    /// exactly the choices the server accepts (the response-side `Role` also
    /// names the `ai` service principal, which this endpoint has always refused).
    #[schema(value_type = AssignableRole, example = "teacher")]
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
    /// The name the public profile is shown under, instead of the legal one.
    #[schema(example = "Ada", max_length = 50)]
    display_name: Option<String>,
    /// Free text under the profile's name.
    #[schema(example = "Sınıfın en hızlı pomodorocusu.", max_length = 500)]
    bio: Option<String>,
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
    /// Accent color as a 6-digit hex with a leading `#`; stored lowercase. Any
    /// valid hex, not a fixed palette.
    #[schema(example = "#fefae0")]
    palette_color: Option<String>,
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
    let display_name = merge_field(req.display_name.as_deref(), DisplayName::try_new)?;
    let bio = merge_field(req.bio.as_deref(), Bio::try_new)?;
    let updated = user
        .set_profile(
            name,
            surname,
            email,
            phone,
            birth_date,
            display_name,
            bio,
            db,
        )
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
    let palette_color = merge_field(req.palette_color.as_deref(), PaletteColor::try_from_str)?;
    let updated = user
        .set_preferences(theme, language, palette_color, db)
        .await?;
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
/// date, plus the public-profile pair `display_name` and `bio` (both readable
/// school-wide at `GET /users/{id}/profile`, unlike the contact fields). Any
/// authenticated role. Omitted fields stay as they are; an empty string clears
/// a field.
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
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_my_profile(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<UpdateProfile>,
) -> Result<Json<UserResponse>, AppError> {
    Ok(Json(apply_profile(user, &req, &st.db).await?))
}

/// Update the caller's own UI preferences: `theme` (`light`/`dark`),
/// `language` (`tr`/`en`), and `palette_color` (accent color as `#rrggbb`).
/// Any authenticated role. Omitted fields stay as they
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
        (status = 400, description = "Invalid theme, language, or palette color", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
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
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
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
        (status = 400, description = "Invalid theme, language, or palette color", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
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

/// Set a user's role. Admin only. An admin cannot change their own role, and
/// the school's **last** admin cannot be demoted by anyone (`409`) — together
/// those keep role management from locking everyone out, including when two
/// admins demote each other at the same instant (the floor is serialized, see
/// `ADMIN_FLOOR_LOCK`). A school that has already lost its admins is recovered
/// with the SurrealQL in the README, since the seed never promotes.
/// Setting any non-`student` role also drops the user's course enrollments —
/// only students enroll, so a promoted user leaves every roster. Demoting below
/// `teacher` drops their course teaching assignments for the mirror reason, and
/// withdraws their published appointment slots, cancelling the live bookings on
/// them: nothing could reach either afterwards — a slot is listed only on its
/// own teacher's calendar and deleted only by a teacher+, and a booking on one
/// is decided only by a teacher+ and cancelled only by its requester, who is
/// refused once the window opens. The requester keeps the booking as
/// `cancelled`, naming the ex-teacher and the reason.
/// Demoting to `parent` additionally gives back the seats they hold on
/// still-open event signup lists: a parent can no longer free them, and nobody
/// else may. Seats on lists that have already closed stay as they are — that
/// roster is history. It also takes the account off every whiteboard roster and
/// **closes every board it created** — permanently read-only, nothing deleted.
/// A demoted creator is a `404` on their own board, the four commands that could
/// end it are the creator's alone, and no route lists a board the caller is not
/// on, so the room would otherwise be commandable by nobody while its
/// participants kept drawing on it. Closed, they keep reading the board, its
/// history and its epochs; only writes are refused.
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
        (status = 409, description = "That account is the school's last admin", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
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
    // The role write and every sweep it owes commit together ([`User::set_role`]
    // carries the whole list and the reasoning behind each arm). The write
    // ordering *within* a request is closed from the other end: a handler that
    // assigns a teacher-only role re-reads the live role after its write
    // ([`super::undo_if_demoted`]), so a demotion racing an assignment is caught
    // by whichever side is second.
    let (updated, boards) = user.set_role(role, &st.db).await?;
    // Whiteboard rooms the commit above changed, prompted with the same frames
    // their own routes publish — after the commit, because the room re-reads the
    // database before it acts on a frame. A room whose creator was demoted is
    // now closed, and its `closed` frame is what turns the open sockets
    // read-only; told only about the roster, they would draw on until each
    // stroke came back refused.
    for board in boards {
        st.board_hub.publish(
            board.get_id().key(),
            json!({
                "type": "participants",
                "creator": board.get_creator().key(),
                "participants": board
                    .get_participants()
                    .iter()
                    .map(|user| user.key())
                    .collect::<Vec<_>>(),
            })
            .to_string(),
        );
        if let Some(closed_at) = board.get_closed_at() {
            st.board_hub.publish(
                board.get_id().key(),
                json!({"type": "closed", "closed_at": closed_at.as_millis()}).to_string(),
            );
        }
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
    // The link row alone is not the grant, exactly as [`ensure_can_observe`]
    // says: a link whose student side changed role (a sweep lost a race with
    // `link_student`) is inert everywhere else, so it must not name a person
    // here either. Filtered at read time rather than swept, which also makes
    // any such row already on disk inert without a migration.
    students.retain(|student| student.get_role() == Role::Student);
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
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
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

// ---- the public profile -----------------------------------------------------
// The school-wide read half of a user row: who they are, what they belong to,
// and the counters that motivate. Deliberately *not* `UserResponse` — that one
// carries email, phone, and birth date, which keep exactly the gate they have
// today (self, teacher+, admin). Nothing here widens where they are reachable.

/// A user's public profile. Contact details are not part of it, at any role.
#[derive(Serialize, ToSchema)]
struct ProfileResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    #[schema(example = "ada")]
    username: String,
    /// The self-chosen `display_name`, falling back to `"Name Surname"`, then
    /// `null` when the row carries neither.
    #[schema(example = "Ada Lovelace")]
    display_name: Option<String>,
    role: RoleSchema,
    #[schema(example = "Sınıfın en hızlı pomodorocusu.")]
    bio: Option<String>,
    /// Present only once a picture is uploaded; the bytes are at
    /// `GET /users/{id}/avatar`.
    avatar: Option<ProfileAvatar>,
    /// At most `max_profile_classes` sections — the full list is at
    /// `GET /classes/me`.
    classes: Vec<ProfileClassRef>,
    /// At most `max_profile_courses` courses, and only the ones the *reader*
    /// may already read at `GET /courses/{id}` — a stranger sees an empty
    /// block, the owner and manager+ see it whole. The full list is at
    /// `GET /courses/me`.
    courses: Vec<ProfileCourseRef>,
    /// Every badge this account has earned, oldest first. Ids and stamps only —
    /// the label and the icon are the client's, keyed by id off the `badges`
    /// catalog at `GET /limits`.
    badges: Vec<ProfileBadge>,
    stats: ProfileStatsResponse,
}

/// One earned badge. Permanent: once it appears here it never leaves, even if
/// the counter behind it falls back below the threshold that earned it.
#[derive(Serialize, ToSchema)]
struct ProfileBadge {
    /// A catalog id from `GET /limits` — `badges.catalog[].id`.
    #[schema(example = "pomodoro_finished_10")]
    id: String,
    /// When it was *first* earned, UTC unix-milliseconds. Later syncs never
    /// move it.
    #[schema(example = 1_759_000_000_000_i64)]
    earned_at: i64,
}

/// What a stored avatar is, without its bytes.
#[derive(Serialize, ToSchema)]
struct ProfileAvatar {
    #[schema(example = "image/png")]
    content_type: String,
    /// Size in bytes.
    #[schema(example = 20_480)]
    size: i64,
}

/// A class section as a profile shows it: the label, and nothing about who runs
/// it. The office-side view (creator, homeroom teacher) stays behind
/// `GET /classes/{id}`, which is teacher+.
#[derive(Serialize, ToSchema)]
struct ProfileClassRef {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    #[schema(example = "9-A")]
    name: String,
    #[schema(example = "9")]
    grade: Option<String>,
}

/// A course as a profile shows it — a label, nothing more.
#[derive(Serialize, ToSchema)]
struct ProfileCourseRef {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    #[schema(example = "Matematik")]
    title: String,
    /// `course`, `study` (etüt), or `club` (kulüp).
    #[schema(example = "course")]
    kind: String,
}

/// The motivational counters. Derived, never settable, and never `null`: an
/// account with no rows behind a counter reads a true `0`.
///
/// Two kinds live here. The first four are **computed at read time** from the
/// rows that exist right now, so deleting the rows moves them down. Every
/// `*_total` one is a **stored lifetime counter**, incremented as the work
/// happens and never recomputed — they can therefore outlive the rows behind
/// them (an exam deleted by its teacher still counts as sat), which is exactly
/// why a badge earned off them stays earned. Each is named for a `stat` of the
/// badge catalog at `GET /limits`, plus `_total`, and that spelling is the join
/// a client makes between the two surfaces — so the suffix is on the key even
/// where the counter is not a running total (`study_streak_total` is a longest,
/// not a sum).
///
/// Some are staff counters and some student ones; a role that never does the
/// work simply reads zero, which is why no key is conditional.
///
/// Every number here is the **owner's** true total for every reader, never
/// narrowed per viewer — deliberately so even where the rows behind it sit
/// behind a gate this reader would fail (`GET /pomodoro/{id}`,
/// `GET /marks/user/{id}`, `GET /attendance/user/{id}`). A magnitude names no
/// course, class, lesson or exam, and a per-reader counter would make one
/// profile read differently to different people.
#[derive(Serialize, ToSchema)]
struct ProfileStatsResponse {
    /// Finished pomodoro stints; an open one counts for nothing.
    pomodoro_sessions: i64,
    /// Total focused milliseconds across those stints.
    pomodoro_focus_ms: i64,
    /// Every course behind the `courses` block, not just the embedded window.
    courses: i64,
    /// Every class behind the `classes` block, not just the embedded window.
    classes: i64,
    /// Lifetime homework hand-ins. Withdrawing a submission gives one back;
    /// editing one moves nothing.
    homework_submitted_total: i64,
    /// Lifetime hand-ins made before their deadline, judged at hand-in.
    homework_on_time_total: i64,
    /// Lifetime exams sat, counted at the first sitting of each exam — a retake
    /// moves nothing, and deleting the exam does not take one back.
    exam_sat_total: i64,
    /// Lifetime finished pomodoro stints.
    pomodoro_finished_total: i64,
    /// Lifetime focused milliseconds.
    pomodoro_focus_ms_total: i64,
    /// Lifetime grades this teacher recorded — exam sittings and homework
    /// submissions alike, once per pair: a regrade moves nothing, and deleting
    /// the grade gives it back.
    marks_given_total: i64,
    /// Lifetime lessons this teacher held, credited by the **first roll call
    /// taken at or after the lesson's own `starts_at`** — a lesson scheduled
    /// and cancelled counts for nothing, and neither does one marked before it
    /// has begun.
    lessons_held_total: i64,
    /// Lifetime pool questions this teacher approved.
    pool_approved_total: i64,
    /// Lifetime questions of this author that reached the school-wide pool,
    /// counted when the approval lands.
    pool_published_total: i64,
    /// Lifetime lesson roll calls that put this **student** `present` or
    /// `late`; a correction to any other status takes one back. Daily and event
    /// attendance moves it not at all.
    lessons_attended_total: i64,
    /// Lifetime exam marks at or above the high-mark cut published as
    /// `badges.high_mark_min` on `GET /limits`, one per graded sitting;
    /// deleting the mark gives it back, like the grader's own counter.
    /// Homework marks never count — they are optional and often status-only.
    high_mark_total: i64,
    /// Longest run of consecutive study days ever held — a high-water mark, so
    /// breaking the run never brings it down. Days end at midnight UTC.
    study_streak_total: i64,
}

/// Build one profile as `viewer` may see it. The course block is chosen off the
/// owner's **live** role, not off the `creator`/`teachers` columns: those are
/// historical and no demotion sweeps them, so a demoted ex-teacher lists what
/// they are enrolled in, like any other student.
///
/// A course carries a title, and `GET /courses/{id}` hands that title only to
/// the enrolled, the staff who run it, and manager+ — so the block is
/// intersected with [`visible_courses`], the same catalog `GET /courses`
/// serves. Reading your own profile, or reading as manager+, needs no
/// intersection: both already see the whole list.
///
/// The class block answers to [`ensure_can_observe`], the gate
/// `GET /classes/user/{id}` holds — teacher+, or a parent linked to the target
/// — because every other `/classes` read is teacher+ and a class name and grade
/// are exactly what those routes withhold. That gate is all-or-nothing rather
/// than per class, so a viewer who fails it gets an empty block, not a 403: the
/// rest of the profile is still public to them.
///
/// So no *named* thing here crosses a gate the viewer would fail directly. The
/// magnitudes deliberately do — **every** one of them, not just the two that
/// count the gated blocks. `stats.courses` and `stats.classes` are the obvious
/// pair; `pomodoro_focus_ms` and `pomodoro_focus_ms_total` are byte-identical
/// to the `total_focus_ms` that `GET /pomodoro/{id}` serves behind
/// [`ensure_can_observe`]; and `lessons_attended_total`,
/// `homework_submitted_total`, `homework_on_time_total`, `exam_sat_total` and
/// `high_mark_total` are magnitudes of the same report data that gate holds —
/// for a reader who is exactly [`Role::Teacher`] they are in fact *wider* than
/// the reports themselves, which [`crate::web::marks`] and
/// [`crate::web::attendance`] narrow to the courses that teacher manages.
///
/// That is the decision, not an oversight: every magnitude in `stats` is the
/// owner's true total for every reader. They are the motivational counters, a
/// per-reader number would make one profile read differently to different
/// people, and a magnitude names no course, no class, no lesson and no exam —
/// the *named* things stay behind their own gates, above.
async fn profile_of(
    st: &AppState,
    user: &User,
    viewer: &User,
) -> Result<ProfileResponse, AppError> {
    let id = user.get_id();
    // One catalog read per request, never one authorization call per course.
    let readable = match viewer.get_id() == id || viewer.get_role().at_least(Role::Manager) {
        true => None,
        false => Some(
            visible_courses(viewer, &st.db)
                .await?
                .iter()
                .map(|course| course.get_id().clone())
                .collect::<Vec<_>>(),
        ),
    };
    // The window is cut after the intersection, so a filtered viewer still gets
    // up to `MAX_PROFILE_COURSES` courses they can actually see.
    let (mut courses, course_total) = match user.get_role().at_least(Role::Teacher) {
        // The teacher read is unpaged, so the total is what came back.
        true => {
            let courses = Course::list_for_teacher(id, &st.db).await?;
            let total = courses.len() as i64;
            (courses, total)
        }
        // Unfiltered readers can take the window from the database.
        false => {
            let window = readable.is_none().then_some(MAX_PROFILE_COURSES as i64);
            Course::list_enrolled(id, window, 0, &st.db).await?
        }
    };
    if let Some(readable) = &readable {
        courses.retain(|course| readable.contains(course.get_id()));
    }
    courses.truncate(MAX_PROFILE_COURSES);
    let (mut members, class_total) =
        ClassMember::list_for_user(id, Some(MAX_PROFILE_CLASSES as i64), 0, &st.db).await?;
    // The window is safe to take from the database here: the class gate is
    // all-or-nothing, so it drops the whole page or none of it — never a row
    // out of the middle of one.
    if viewer.get_id() != id && ensure_can_observe(viewer, id, &st.db).await.is_err() {
        members.clear();
    }
    let class_ids: Vec<ClassGroupId> = members.iter().map(|row| row.get_class().clone()).collect();
    let classes = ClassGroup::list_by_ids(&class_ids, &st.db).await?;
    // Both totals are the full counts, not the windowed ones — the blocks are a
    // preview, the stats are the truth.
    let stats = ProfileStats::load(id, course_total, class_total, &st.db).await?;
    let badges = badges_of(st, id, &stats).await?;
    Ok(ProfileResponse {
        id: id.key().to_string(),
        username: user.get_username().as_str().to_string(),
        // The whole three-step resolve lives in `PersonRef` — one spelling of
        // it, so this profile and every embedded person ref cannot disagree.
        display_name: PersonRef::new(user).display_name,
        role: user.get_role().into(),
        bio: user.get_bio().map(|bio| bio.as_str().to_string()),
        avatar: user
            .get_avatar_content_type()
            .map(|content_type| ProfileAvatar {
                content_type: content_type.as_str().to_string(),
                size: user.get_avatar_size().unwrap_or_default(),
            }),
        classes: classes
            .iter()
            .map(|class| ProfileClassRef {
                id: class.get_id().key().to_string(),
                name: class.get_name().as_str().to_string(),
                grade: class.get_grade().map(|grade| grade.as_str().to_string()),
            })
            .collect(),
        courses: courses
            .iter()
            .map(|course| ProfileCourseRef {
                id: course.get_id().key().to_string(),
                title: course.get_title().as_str().to_string(),
                kind: course.get_kind().as_str().to_string(),
            })
            .collect(),
        badges: badges
            .iter()
            .map(|award| ProfileBadge {
                id: award.get_badge().to_string(),
                earned_at: award.get_earned_at().as_millis(),
            })
            .collect(),
        stats: ProfileStatsResponse {
            pomodoro_sessions: stats.get_pomodoro_sessions(),
            pomodoro_focus_ms: stats.get_pomodoro_focus_ms(),
            courses: stats.get_courses(),
            classes: stats.get_classes(),
            homework_submitted_total: stats.get_totals().get_homework_submitted(),
            homework_on_time_total: stats.get_totals().get_homework_on_time(),
            exam_sat_total: stats.get_totals().get_exam_sat(),
            pomodoro_finished_total: stats.get_totals().get_pomodoro_finished(),
            pomodoro_focus_ms_total: stats.get_totals().get_pomodoro_focus_ms(),
            marks_given_total: stats.get_totals().get_marks_given(),
            lessons_held_total: stats.get_totals().get_lessons_held(),
            pool_approved_total: stats.get_totals().get_pool_approved(),
            pool_published_total: stats.get_totals().get_pool_published(),
            lessons_attended_total: stats.get_totals().get_lessons_attended(),
            high_mark_total: stats.get_totals().get_high_mark(),
            study_streak_total: stats.get_totals().get_study_streak(),
        },
    })
}

/// The owner's earned badges, and the backstop that keeps them honest.
///
/// Every write that moves a counter syncs the badges behind it, so in the
/// steady state the earned set holds nothing the award rows do not already
/// carry and this is one read and **no write at all**. Only a threshold crossed
/// by a path whose sync was lost — a transient database error, which those
/// callers log and swallow on purpose — leaves a gap, and that heals here on
/// the next profile read of that account.
///
/// Nothing here revokes: [`badge::sync`] only ever adds, so a counter that has
/// since fallen back below its threshold leaves the badge standing. The sync's
/// own error is logged and dropped, and the awards already read are served —
/// a decoration may not fail the profile it decorates.
async fn badges_of(
    st: &AppState,
    user: &UserId,
    stats: &ProfileStats,
) -> Result<Vec<BadgeAward>, AppError> {
    let awards = BadgeAward::list_for(user, &st.db).await?;
    let complete = badge::earned(stats.get_totals())
        .iter()
        .all(|id| awards.iter().any(|award| award.get_badge() == *id));
    if complete {
        return Ok(awards);
    }
    if let Err(err) = badge::sync(user, &st.db).await {
        tracing::warn!("failed to sync badges for {}: {err}", user.key());
        return Ok(awards);
    }
    // Re-read so the badge just healed appears on *this* response, stamp and
    // all, rather than only on the next one.
    BadgeAward::list_for(user, &st.db).await
}

/// The profile gate: any authenticated account reads any profile — except a
/// parent, who is an observer of their own children and of nobody else. The
/// link alone is not the grant either; [`ensure_can_observe`] re-reads the
/// target's live role, so a student who was promoted out stops being readable.
async fn ensure_may_read_profile(
    caller: &User,
    target: &UserId,
    db: &Database,
) -> Result<(), AppError> {
    if caller.get_role() == Role::Parent && caller.get_id() != target {
        ensure_can_observe(caller, target, db).await?;
    }
    Ok(())
}

/// The target's row, 404 if it is gone, once the caller is allowed to see it.
async fn readable_profile_user(st: &AppState, caller: &User, id: &str) -> Result<User, AppError> {
    let target = UserId::from_key(id);
    ensure_may_read_profile(caller, &target, &st.db).await?;
    User::read(&target, &st.db).await?.ok_or(AppError::NotFound)
}

/// The caller's own public profile — what everyone else sees of them.
#[utoipa::path(
    get,
    path = "/me/profile",
    tag = "users",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's profile", body = ProfileResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_profile(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<ProfileResponse>, AppError> {
    Ok(Json(profile_of(&st, &user, &user).await?))
}

/// One user's public profile: display name, bio, avatar meta, their class and
/// course blocks, the badges they have earned, and the motivational counters.
/// Badges carry an id and the instant they were first earned; their labels and
/// icons come from the `badges` catalog at `GET /limits`. Readable by every
/// authenticated account — except a parent, who reads only their own and their
/// linked students'. Never carries email, phone, or birth date; those stay on
/// `GET /users/{id}` (admin) and `GET /auth/me`. The embedded blocks are capped
/// at `max_profile_classes` / `max_profile_courses` (see `GET /limits`) — the
/// full paged lists are `GET /classes/me` and `GET /courses/me`. The course
/// block is also cut to what the *reader* may already see: only courses they
/// would pass `GET /courses/{id}` on. The class block holds the same bar as
/// `GET /classes/user/{id}` — teacher+, a parent linked to this student, or the
/// owner themselves; every other reader gets an empty `classes` array rather
/// than a 403. Every number in `stats` stays the owner's true total either way,
/// including the ones whose underlying reports are gated (`/pomodoro/{id}`,
/// `/marks/user/{id}`, `/attendance/user/{id}`): they are motivational
/// counters, and a magnitude names no course, class, lesson or exam.
#[utoipa::path(
    get,
    path = "/{id}/profile",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The user's profile", body = ProfileResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "A parent without a link to this student", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn get_user_profile(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<ProfileResponse>, AppError> {
    let target = readable_profile_user(&st, &caller, &id).await?;
    Ok(Json(profile_of(&st, &target, &caller).await?))
}

// ---- the avatar -------------------------------------------------------------
// One optional picture per user, the same one-image-on-the-owning-row shape the
// question pool uses: bytes on disk under a server-generated ULID, metadata on
// the user row. Every write here must take the replaced blob off disk — no
// route deletes a user, so nothing else would ever collect it.

/// Upload (or replace) the caller's own avatar. `multipart/form-data` with the
/// image under a `file` field; the declared content type must be `image/png`,
/// `image/jpeg`, `image/webp`, or `image/gif` (rasters only — no SVG), the
/// bytes at most the school's `max_file_bytes` (settings). Replacing one drops
/// the previous picture.
#[utoipa::path(
    post,
    path = "/me/avatar",
    tag = "users",
    security(("session_cookie" = [])),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Avatar stored", body = ProfileAvatar),
        (status = 400, description = "Missing file field, empty file, or a content type outside the image allowlist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "The account no longer exists", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_my_avatar(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ProfileAvatar>), AppError> {
    let upload = read_image_upload(&st, &mut multipart).await?;
    let size = upload.size();

    let file = ulid::Ulid::new().to_string();
    store_blob(&st, &file, &upload.data, || async {
        match User::set_avatar(user.get_id(), &file, &upload.content_type, size, &st.db).await? {
            // Row written; the picture this one replaced comes off disk.
            Some(before) => Ok(((), before.get_avatar_file().map(str::to_string))),
            // The account went away mid-upload — the fresh blob is an orphan.
            None => Err(AppError::NotFound),
        }
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(ProfileAvatar {
            content_type: upload.content_type.as_str().to_string(),
            size,
        }),
    ))
}

/// The caller's own avatar bytes. The self alias of `GET /{id}/avatar` — the
/// static `/me/avatar` segment wins over `/{id}/avatar` in the router, so
/// without this a client that never learned its own id gets a bodyless `405`
/// on the obvious route. It serves the caller's own row and nothing else, so
/// it asks no gate: the session already proves the reach.
#[utoipa::path(
    get,
    path = "/me/avatar",
    tag = "users",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The image bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "The caller has no avatar", body = ErrorResponse),
    ),
)]
async fn get_my_avatar(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Response, AppError> {
    serve_avatar(&st, &user).await
}

/// The avatar bytes. Same reach as the profile itself: every authenticated
/// account, except a parent, who is limited to their own and their linked
/// students'.
#[utoipa::path(
    get,
    path = "/{id}/avatar",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The image bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "A parent without a link to this student", body = ErrorResponse),
        (status = 404, description = "No such user, or they have no avatar", body = ErrorResponse),
    ),
)]
async fn get_avatar(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let target = readable_profile_user(&st, &caller, &id).await?;
    serve_avatar(&st, &target).await
}

/// The shared tail of both avatar reads: the bytes, or a `404` when the row
/// carries none. The caller owns the gate — this one is reached already.
async fn serve_avatar(st: &AppState, user: &User) -> Result<Response, AppError> {
    match (user.get_avatar_file(), user.get_avatar_content_type()) {
        (Some(file), Some(content_type)) => {
            serve_inline_blob(&st.files_path, file, content_type).await
        }
        _ => Err(AppError::NotFound),
    }
}

/// Remove the caller's own avatar.
#[utoipa::path(
    delete,
    path = "/me/avatar",
    tag = "users",
    security(("session_cookie" = [])),
    responses(
        (status = 204, description = "Avatar removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No avatar to remove", body = ErrorResponse),
    ),
)]
async fn delete_my_avatar(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<StatusCode, AppError> {
    drop_avatar(&st, user.get_id()).await
}

/// Remove any user's avatar. Admin only — the moderation path: an offensive
/// picture is a school problem, and no route deletes the account it hangs on.
#[utoipa::path(
    delete,
    path = "/{id}/avatar",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    responses(
        (status = 204, description = "Avatar removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "No such user, or no avatar to remove", body = ErrorResponse),
    ),
)]
async fn delete_avatar(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    drop_avatar(&st, &UserId::from_key(&id)).await
}

/// Clear the row's avatar and take its blob off disk — the shared tail of the
/// self and moderation deletes. A row without one (or no row at all) is a 404.
async fn drop_avatar(st: &AppState, user: &UserId) -> Result<StatusCode, AppError> {
    let before = User::clear_avatar(user, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let Some(file) = before.get_avatar_file() else {
        return Err(AppError::NotFound);
    };
    remove_blob(&st.files_path, file).await;
    Ok(StatusCode::NO_CONTENT)
}
