use crate::web::tenant_state::{SchoolIdCookie, State};
use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{
    MAX_MAX_FILE_BYTES, MAX_PROFILE_CLASSES, MAX_PROFILE_COURSES, RESERVED_USERNAMES,
    UPLOAD_BODY_OVERHEAD_BYTES,
};
use crate::database::Database;
use crate::domain::badge::{self, BadgeAward};
use crate::domain::class_group::ClassGroupId;

use crate::domain::preferences::{Language, PaletteColor, Theme};
use crate::domain::profile::{
    Address, Bio, BirthDate, DisplayName, Email, Gender, PersonName, Phone, ProfileStats,
};
use crate::domain::role::Role;
use crate::domain::user::{Password, StudentNumber, User, UserId, Username};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service::parent_link::ensure_can_observe;
use crate::state::AppState;

use super::courses::visible_courses;
use super::dto::AssignableRole;
use super::dto::Role as RoleSchema;
use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireAdmin, UploadFileForm, UserResponse, paginate,
    read_image_upload, remove_blob, serve_inline_blob, store_blob,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users, create_user))
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
    /// Self-declared gender: `female`, `male`, `other`, or `undisclosed`.
    /// A closed vocabulary — anything else is a `400`.
    #[schema(example = "female")]
    gender: Option<String>,
    /// Postal address, at most `max_address_len` characters (from `GET /limits`).
    #[schema(example = "Çamlık Mah. 2. Sk. No: 7, Bornova / İzmir", max_length = 500)]
    address: Option<String>,
    /// Who to call when the person is unreachable. Same rules as `name`.
    #[schema(example = "Mehmet Yılmaz", max_length = 100)]
    emergency_contact_name: Option<String>,
    /// The emergency contact's phone, same shape as `phone`.
    #[schema(example = "+90 555 987 65 43")]
    emergency_contact_phone: Option<String>,
    /// The teacher's subject specialisation: one of the school's `branches` from
    /// `GET /settings`, by name. Omit to keep the current value; send `""` to
    /// clear it (a school that lists no subjects stores none).
    #[schema(example = "Matematik", max_length = 50)]
    branch: Option<String>,
    /// The school-issued student number of the account being patched, unique
    /// inside the school. Only a `student` account may hold one — naming one
    /// for any other role, or naming a number another student already holds,
    /// is refused. Omit to keep the current value; send `""` (or whitespace)
    /// to clear it.
    #[schema(example = "1234", max_length = 32)]
    student_number: Option<String>,
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

/// Resolve the `branch` a request names. Same shape as [`merge_field`] — absent
/// is "keep", `""` is an explicit clear — with the one check a free string
/// cannot make: the value must be one the *school* lists (`GET /settings`).
/// The vocabulary is the school's, so nothing else can decide it, and a school
/// with no list can only ever have its `branch` cleared.
fn merge_branch(
    patch: Option<&str>,
    allowed: &[String],
) -> Result<Option<Option<String>>, ValidationError> {
    match patch {
        None => Ok(None),
        Some("") => Ok(Some(None)),
        Some(value) => {
            if !allowed.iter().any(|name| name == value) {
                return Err(ValidationError::Invalid {
                    field: "branch",
                    reason: "branch must be one of the school's configured branş list",
                });
            }
            Ok(Some(Some(value.to_string())))
        }
    }
}

/// Resolve the `student_number` a request names. Same merge shape as
/// [`merge_field`] — absent (or `null`) is "keep", blank is an explicit
/// clear — plus the two checks a plain field does not carry:
///
/// * **The role gate.** A number names a student of this school, so only a
///   `student` row may receive one; naming one for any other role is a 400,
///   not a silent write the next role change would clear.
/// * **The shape**, through [`StudentNumber`]: non-blank after trimming and
///   within the published bound. Blank is a *clear* rather than a parse
///   failure, so whitespace can never become a stored number — the same
///   reading every other patchable field has.
///
/// `""` on a non-student stays a clear (a no-op: such a row never holds one),
/// and a value that collides with another student's is the unique index's
/// refusal — a 409 from the writer, never a pre-check that could race.
fn merge_student_number(
    patch: Option<&str>,
    role: Role,
) -> Result<Option<Option<StudentNumber>>, ValidationError> {
    match patch {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(Some(None)),
        Some(raw) => {
            if role != Role::Student {
                return Err(StudentNumber::non_student_refusal());
            }
            Ok(Some(Some(StudentNumber::try_new(raw)?)))
        }
    }
}

/// Validate and persist exactly the info fields `req` carried — nothing is
/// merged from `user`'s snapshot, so a concurrent PATCH of another field is not
/// reverted. Shared by the self-service and admin profile endpoints — they
/// differ only in whose row they load and who may call them.
///
/// `student_number` is resolved against the target's *live* role, read with the
/// row we hold — so the role gate cannot be raced by a role change landing in
/// between (a promotion that lands first makes this a 400; this write landing
/// first leaves nothing behind, because the role write clears the number in the
/// same statement).
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
    let gender = merge_field(req.gender.as_deref(), Gender::try_from_str)?;
    let address = merge_field(req.address.as_deref(), Address::try_new)?;
    let emergency_contact_name = merge_field(req.emergency_contact_name.as_deref(), |v| {
        PersonName::try_new("emergency_contact_name", v)
    })?;
    let emergency_contact_phone =
        merge_field(req.emergency_contact_phone.as_deref(), Phone::try_new)?;
    let student_number = merge_student_number(req.student_number.as_deref(), user.get_role())?;
    // Only a request that actually names a branch pays the settings read.
    let branch = match req.branch {
        Some(_) => {
            let school = crate::service::settings::load(db).await?;
            merge_branch(req.branch.as_deref(), &school.get_branches())?
        }
        None => None,
    };
    let updated = crate::service::user::set_profile(
        db,
        user.get_id(),
        name,
        surname,
        email,
        phone,
        birth_date,
        display_name,
        bio,
        gender,
        address,
        emergency_contact_name,
        emergency_contact_phone,
        branch,
        student_number,
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
    let updated =
        crate::service::user::set_preferences(db, user.get_id(), theme, language, palette_color)
            .await?;
    Ok(UserResponse::new(&updated))
}

#[derive(Deserialize, IntoParams)]
struct SearchUsers {
    /// Case-insensitive fragment of a username, name, or surname. Omit it —
    /// or leave it blank — to list everyone the caller may see, optionally
    /// narrowed by `role`.
    q: Option<String>,
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
/// attendance) and, for a student or parent, the one way to find the staff
/// member they are allowed to message. Any authenticated user may ask; a
/// caller below teacher only ever sees the roles they may message (teacher,
/// manager, admin), in the items *and* in `total`. A blank or omitted `q`
/// lists everyone the caller may see — what the pickers open with; `role`
/// narrows to one role (e.g. `role=student` for an enroll picker), and a
/// student or parent naming a role they may not message is refused. Paged
/// via `?limit=&offset=` like the other lists (omit `limit` for every
/// match); returns a `{items, total, limit, offset}` envelope carrying only
/// id/username/display name and the student number — no contact details.
#[utoipa::path(
    get,
    path = "/search",
    tag = "users",
    security(("session_cookie" = [])),
    params(SearchUsers),
    responses(
        (status = 200, description = "A page of matching users (all matches when unpaged)", body = Page<PersonRef>),
        (status = 400, description = "Unknown role, or invalid limit/offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "A student or parent asked for a role they may not message", body = ErrorResponse),
    ),
)]
async fn search_users(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(req): Query<SearchUsers>,
) -> Result<Json<Page<PersonRef>>, AppError> {
    let (limit, offset) = PageParams {
        limit: req.limit,
        offset: req.offset,
    }
    .resolve()?;
    let role = req.role.as_deref().map(Role::try_from_str).transpose()?;
    // Below staff the caller may only see whom they may write to. Asking for a
    // role outside that set is a refusal, not a silently empty page.
    let allowed = user.get_role().messageable_roles();
    if let (Some(allowed), Some(role)) = (allowed, role)
        && !allowed.contains(&role)
    {
        return Err(AppError::Forbidden(
            "students and parents may only search staff (teacher or higher)",
        ));
    }
    let (users, total) = crate::service::user::search(
        &st.db,
        req.q.as_deref().unwrap_or(""),
        role,
        allowed,
        limit,
        offset,
    )
    .await?;
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
    let (users, total) = crate::service::user::list_all(&st.db, limit, offset).await?;
    let items = users.iter().map(UserResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[derive(Deserialize, ToSchema)]
struct CreateUser {
    /// The new account's username — a global credential, exactly as at
    /// `POST /auth/register`.
    #[schema(example = "ada", min_length = 3, max_length = 32)]
    username: String,
    #[schema(example = "correct horse battery", min_length = 6, max_length = 128)]
    password: String,
    /// The role the account is born with. Omitted → `student`.
    #[schema(example = "teacher")]
    role: Option<String>,
    /// The school-issued student number, unique inside the school. Only a
    /// `student` account may hold one — naming one while creating any other
    /// role is a `400`, and a number another student already holds is a `409`.
    /// Omit to create the account unnumbered; blank is the same as omitted.
    #[schema(example = "1234", max_length = 32)]
    student_number: Option<String>,
}

/// Create a school account directly — the school-office path for adding a
/// student or a staff member with no self-registration and no invite. Admin
/// only. `{username, password}` are required and `role` is optional (omitted →
/// `student`); the row is born with its role rather than promoted into it, so
/// a new teacher is never briefly a student. `student_number` numbers a new
/// student at mint time — a `409` when another student in this school already
/// holds it, a `400` on a non-student role. The username is a global
/// **person** credential exactly as at `POST /auth/register`: a name new
/// everywhere creates the person and this school's `app_user`, while a person
/// who already exists is attached to this school only when the password
/// matches the stored credential — a mismatch is a `409`. A username already
/// taken in this school is a `409`, and the reserved staff-reading names
/// (`admin`, `root`, …) are a `400`, the same policy registration holds.
#[utoipa::path(
    post,
    path = "/",
    tag = "users",
    security(("session_cookie" = [])),
    request_body = CreateUser,
    responses(
        (status = 201, description = "Account created, holding the requested role", body = UserResponse),
        (status = 400, description = "Invalid username or password (a reserved username included), an unknown role, or a student number on a non-student role", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 409, description = "The username is taken in this school, that person exists under a different password, or the student number is already taken (`code: student_number_taken`)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_user(
    State(st): State<AppState>,
    SchoolIdCookie(school): SchoolIdCookie,
    _admin: RequireAdmin,
    Json(req): Json<CreateUser>,
) -> Result<(StatusCode, Json<UserResponse>), AppError> {
    let username = Username::try_new(&req.username)?;
    // The same policy `POST /auth/register` enforces: these names read as
    // staff and invite impersonation, so no minting route may claim them. The
    // `ADMIN_USERNAME` bootstrap does not come through here.
    if RESERVED_USERNAMES.contains(&username.as_str()) {
        return Err(ValidationError::Invalid {
            field: "username",
            reason: "this username is reserved",
        }
        .into());
    }
    let role = match req.role.as_deref() {
        Some(raw) => Role::try_from_str(raw)?,
        None => Role::Student,
    };
    // The number is validated against the role the row is being born with
    // (the same gate the profile writer applies), then bound into the mint —
    // one statement, so a refused number leaves no half-created account.
    let student_number = merge_student_number(req.student_number.as_deref(), role)?.flatten();
    let password = Password::try_new(&req.password)?;
    let password_hash = password.hash_async().await?;

    // Person half (control database): create the global account, or meet the
    // one already standing — the credential login verifies, exactly as the
    // register and school-create paths treat it. The stored hash is never
    // rewritten here; the caller must prove they know it below.
    let control = st.tenants.control();
    let person =
        crate::service::person::create_or_load(control, username.clone(), password_hash).await?;
    if !person.get_password_hash().verify_async(&password).await {
        return Err(AppError::Conflict(
            "an account with that username exists under a different password",
        ));
    }

    // School half, born with its role: a fresh row holds no enrollments or
    // teaching assignments for a later promotion to sweep. A username already
    // taken in this school refuses here, before the membership is written, and
    // so does a student number another student already holds.
    let user = crate::service::user::create_with_role(
        &st.db,
        username,
        Some(*person.get_id()),
        role,
        student_number,
    )
    .await?;
    crate::service::person::link_school(control, person.get_id(), &school).await?;
    Ok((StatusCode::CREATED, Json(UserResponse::new(&user))))
}

/// Update the caller's own personal info: name, surname, email, phone, birth
/// date, plus the public-profile pair `display_name` and `bio` (both readable
/// school-wide at `GET /users/{id}/profile`, unlike the contact fields). Any
/// authenticated role. Omitted fields stay as they are; an empty string clears
/// a field. `student_number` follows the same merge semantics, with two
/// refusals of its own: naming one while the account is not a `student` is a
/// `400`, and a number another student in this school already holds is a `409`
/// (`code: student_number_taken`).
#[utoipa::path(
    patch,
    path = "/me",
    tag = "users",
    security(("session_cookie" = [])),
    request_body = UpdateProfile,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid field, or a student number on a non-student account", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 409, description = "The student number is already taken (`code: student_number_taken`)", body = ErrorResponse),
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
    let user = crate::service::user::read(&st.db, &UserId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(UserResponse::new(&user)))
}

/// Update any user's personal info. Admin only — the school-office path for
/// maintaining records on behalf of students and staff. Same field semantics
/// as `PATCH /users/me`, `student_number` included: the number belongs to the
/// student identity the office maintains, so this is the door that issues it —
/// a `400` when the target is not a `student`, a `409`
/// (`code: student_number_taken`) when another student in this school already
/// holds it. A role change away from `student` clears it on its own
/// (`PATCH /users/{id}/role`), which is also the answer to "this account is no
/// longer a student".
#[utoipa::path(
    patch,
    path = "/{id}/profile",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    request_body = UpdateProfile,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid field, or a student number on a non-student account", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
        (status = 409, description = "The student number is already taken (`code: student_number_taken`)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_user_profile(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<UpdateProfile>,
) -> Result<Json<UserResponse>, AppError> {
    let user = crate::service::user::read(&st.db, &UserId::from_key(&id))
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
    let user = crate::service::user::read(&st.db, &UserId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(apply_preferences(user, &req, &st.db).await?))
}

/// Set a user's role. Admin only. An admin cannot change their own role, and
/// the school's **last** admin cannot be demoted by anyone (`409`) — together
/// those keep role management from locking everyone out, including when two
/// admins demote each other at the same instant (the floor is a predicate on
/// the role write itself, so the racing demotions serialize on row locks).
/// A school that has already lost its admins is recovered by hand against the
/// database, since the seed never promotes.
/// Setting any non-`student` role also drops the user's course enrollments —
/// only students enroll, so a promoted user leaves every roster — and clears
/// the account's student number in the same transaction: the number names a
/// student of this school, so no window exists where a promoted account still
/// holds one. Demoting below
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
    SchoolIdCookie(school): SchoolIdCookie,
    RequireAdmin(admin): RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<SetRole>,
) -> Result<Json<UserResponse>, AppError> {
    let role = Role::try_from_str(&req.role)?;
    let target = UserId::from_key(&id);
    if &target == admin.get_id() {
        return Err(AppError::Forbidden("cannot change your own role"));
    }
    // The role write and every sweep it owes commit together
    // ([`crate::service::user::set_role`] carries the whole list and the
    // reasoning behind each arm). The write ordering *within* a request is
    // closed from the other end: a handler that assigns a teacher-only role
    // re-reads the live role after its write ([`super::undo_if_demoted`]), so
    // a demotion racing an assignment is caught by whichever side is second.
    let (updated, boards) = crate::service::user::set_role(&st.db, &target, role).await?;
    // Whiteboard rooms the commit above changed, prompted with the same frames
    // their own routes publish — after the commit, because the room re-reads the
    // database before it acts on a frame. A room whose creator was demoted is
    // now closed, and its `closed` frame is what turns the open sockets
    // read-only; told only about the roster, they would draw on until each
    // stroke came back refused.
    for board in boards {
        st.board_hub.publish(
            &school,
            board.get_id().key().as_str(),
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
                &school,
                board.get_id().key().as_str(),
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
    let links = crate::service::parent_link::list_for_parent(db, parent).await?;
    let ids: Vec<UserId> = links.iter().map(|link| *link.get_student()).collect();
    let mut students = crate::service::user::list_by_ids(db, &ids).await?;
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
    let (link, parent, student) = crate::service::parent_link::link(
        &st.db,
        &UserId::from_key(&id),
        &UserId::from_key(&req.user_id),
        admin.get_id(),
    )
    .await?;
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
    crate::service::user::read(&st.db, &parent)
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
    if crate::service::parent_link::remove(
        &st.db,
        &UserId::from_key(&id),
        &UserId::from_key(&student),
    )
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
//
// The student number is deliberately on `UserResponse` (and `PersonRef`) and
// *not* here: it is the school office's identifier for a student, readable
// wherever the reader is already looking at student identity — the admin user
// surfaces, the pickers, a parent's own children's roster — while this profile
// is readable by every authenticated account school-wide. A number is not a
// secret, but broadcasting one to the whole school is a widening nobody asked
// for.

/// A user's public profile. Contact details are not part of it, at any role.
#[derive(Serialize, ToSchema)]
struct ProfileResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
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
    /// The teacher's subject specialisation, one of the school's `branches`
    /// (`GET /settings`); `null` when none is set — or when the school lists none.
    #[schema(example = "Matematik")]
    branch: Option<String>,
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
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    id: String,
    #[schema(example = "9-A")]
    name: String,
    #[schema(example = "9")]
    grade: Option<String>,
}

/// A course as a profile shows it — a label, nothing more.
#[derive(Serialize, ToSchema)]
struct ProfileCourseRef {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    id: String,
    #[schema(example = "Matematik")]
    title: String,
    /// `course`, `study` (supervised study), or `club`.
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
            let courses = crate::service::course::list_for_teacher(&st.db, id).await?;
            let total = courses.len() as i64;
            (courses, total)
        }
        // Unfiltered readers can take the window from the database.
        false => {
            let window = readable.is_none().then_some(MAX_PROFILE_COURSES as i64);
            crate::service::course::list_enrolled(&st.db, id, window, 0).await?
        }
    };
    if let Some(readable) = &readable {
        courses.retain(|course| readable.contains(course.get_id()));
    }
    courses.truncate(MAX_PROFILE_COURSES);
    let (mut members, class_total) = crate::service::class_member::list_for_user(
        &st.db,
        id,
        Some(MAX_PROFILE_CLASSES as i64),
        0,
    )
    .await?;
    // The window is safe to take from the database here: the class gate is
    // all-or-nothing, so it drops the whole page or none of it — never a row
    // out of the middle of one.
    if viewer.get_id() != id && ensure_can_observe(viewer, id, &st.db).await.is_err() {
        members.clear();
    }
    let class_ids: Vec<ClassGroupId> = members.iter().map(|row| row.get_class().clone()).collect();
    let classes = crate::service::class_group::list_by_ids(&st.db, &class_ids).await?;
    // Both totals are the full counts, not the windowed ones — the blocks are a
    // preview, the stats are the truth.
    let stats = crate::service::profile::load(&st.db, id, course_total, class_total).await?;
    let badges = badges_of(st, id, &stats).await?;
    Ok(ProfileResponse {
        id: id.key().to_string(),
        username: user.get_username().as_str().to_string(),
        // The whole three-step resolve lives in `PersonRef` — one spelling of
        // it, so this profile and every embedded person ref cannot disagree.
        display_name: PersonRef::new(user).display_name,
        role: user.get_role().into(),
        bio: user.get_bio().map(|bio| bio.as_str().to_string()),
        branch: user.get_branch().map(str::to_string),
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
    let awards = crate::service::badge::list_for(&st.db, user).await?;
    let complete = badge::earned(stats.get_totals())
        .iter()
        .all(|id| awards.iter().any(|award| award.get_badge() == *id));
    if complete {
        return Ok(awards);
    }
    if let Err(err) = crate::service::badge::sync(&st.db, user).await {
        tracing::warn!("failed to sync badges for {}: {err}", user.key());
        return Ok(awards);
    }
    // Re-read so the badge just healed appears on *this* response, stamp and
    // all, rather than only on the next one.
    crate::service::badge::list_for(&st.db, user).await
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
    crate::service::user::read(&st.db, &target)
        .await?
        .ok_or(AppError::NotFound)
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

    let file = crate::domain::monotonic_id::next_uuid().to_string();
    store_blob(&st, &file, &upload.data, || async {
        match crate::service::user::set_avatar(
            &st.db,
            user.get_id(),
            &file,
            &upload.content_type,
            size,
        )
        .await?
        {
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
    let before = crate::service::user::clear_avatar(&st.db, user)
        .await?
        .ok_or(AppError::NotFound)?;
    let Some(file) = before.get_avatar_file() else {
        return Err(AppError::NotFound);
    };
    remove_blob(&st.files_path, file).await;
    Ok(StatusCode::NO_CONTENT)
}
