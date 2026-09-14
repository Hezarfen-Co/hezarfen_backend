use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{RESERVED_USERNAMES, SESSION_DURATION_DAYS};
use crate::domain::role::Role as DomainRole;
use crate::domain::user::{Password, PasswordHash, Username};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::rate_limit::{RateLimitConfig, RateLimiter};
use crate::service;
use crate::state::AppState;
use crate::tenant::Slug;

use super::dto::Role;
use super::tenant_state::split_cookie;
use super::{BUILDER_COOKIE_PREFIX, CurrentUser, PERSON_COOKIE_PREFIX, UserResponse};

pub fn routes(state: &AppState) -> OpenApiRouter<AppState> {
    // Strict per-IP limit on the two credential endpoints only — the
    // brute-force and username-enumeration surface. `me` and `logout` stay
    // outside it: browser frontends poll `me` on every page load, and being
    // over the auth limit must never block an intentional logout.
    let rate_limit: &RateLimitConfig = &state.rate_limit;
    let limiter = RateLimiter::per_minute(rate_limit.auth_per_minute, rate_limit.trust_proxy);
    // The budget survives a restart — the tier only means something if a
    // brute-forcer cannot reset it by waiting out a deploy.
    limiter.share("auth", state.db.clone());
    OpenApiRouter::new()
        .routes(routes!(register))
        .routes(routes!(login))
        // `route_layer` wraps only the routes registered above it.
        .route_layer(middleware::from_fn(move |req: Request, next: Next| {
            let limiter = limiter.clone();
            async move { limiter.enforce(req, next).await }
        }))
        .routes(routes!(select_school))
        .routes(routes!(logout))
        .routes(routes!(me))
}

/// What `POST /auth/register` takes: registration is still aimed at a school.
/// Split from the login body — a login names no school, it picks the school
/// afterwards. Serde ignores unknown fields, so a frontend that still sends
/// `school` to login is merely ignored, not broken.
#[derive(Deserialize, ToSchema)]
struct RegisterCredentials {
    /// The school's slug — where the new account (or the new membership of an
    /// existing person) lives.
    #[schema(example = "demo", min_length = 2, max_length = 32)]
    school: String,
    #[schema(example = "ada", min_length = 3, max_length = 32)]
    username: String,
    #[schema(example = "correct horse battery", min_length = 6, max_length = 128)]
    password: String,
}

/// What `POST /auth/login` takes. The account is a **person**: one global
/// username + password, no school — a person with several memberships chooses
/// one afterwards (`POST /auth/school`).
#[derive(Deserialize, ToSchema)]
struct LoginCredentials {
    #[schema(example = "ada", min_length = 3, max_length = 32)]
    username: String,
    #[schema(example = "correct horse battery", min_length = 6, max_length = 128)]
    password: String,
}

/// One school offered to a multi-school person.
#[derive(Serialize, ToSchema)]
struct SchoolChoice {
    #[schema(example = "demo")]
    slug: String,
    #[schema(example = "Demo")]
    name: String,
}

/// What `POST /auth/login` answers when the person belongs to more than one
/// active school: no `id` (no school is entered yet, so there is no school
/// row to speak of), and no school cookie. The `schools` list is in slug
/// order and omits suspended schools.
#[derive(Serialize, ToSchema)]
struct SchoolChoiceResponse {
    #[schema(example = "ada")]
    username: String,
    schools: Vec<SchoolChoice>,
}

/// The two login outcomes, told apart by shape: a school was entered (a
/// [`UserResponse`] and a `<slug>.<token>` cookie), or one must be chosen (a
/// [`SchoolChoiceResponse`] and a `person.<token>` cookie). Disjoint by the
/// `id` field, so an untagged `oneOf` is unambiguous on the wire.
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
enum LoginResponse {
    LoggedIn(Box<UserResponse>),
    ChooseSchool(SchoolChoiceResponse),
}

/// What `POST /auth/school` takes: which of the person's memberships to bind.
#[derive(Deserialize, ToSchema)]
struct SelectSchool {
    #[schema(example = "demo", min_length = 2, max_length = 32)]
    school: String,
}

/// What `POST /auth/register` answers with — deliberately *not* [`UserResponse`].
/// It carries only what is true of both outcomes (created, and already-taken):
/// the echoed username and the role every fresh account gets. No `id`, because
/// the taken path has no row to name and a fabricated one would be a lie the
/// client stores; no profile fields, because a fresh account has none. Anything
/// added here must hold for both paths, or the reply starts leaking existence.
#[derive(Serialize, ToSchema)]
struct RegisterResponse {
    #[schema(example = "ada")]
    username: String,
    role: Role,
}

/// Register a new user account, or attach an existing person to one more
/// school: `{school, username, password}` in, `{username, role}` back (no
/// `id`; new accounts are `student`). Always `201` — see below.
///
/// The account is a **person**: a global username in the control plane that
/// can hold memberships in many schools. Registering a username that is new
/// everywhere creates the person and the school's `app_user` (student). A
/// person who already exists is attached to this school too — but only when
/// the password matches the person credential; a wrong password is the same
/// `201` and joins nothing.
///
/// Every outcome answers `201` with the *same* body, built once before the
/// branch: a distinguishable "already taken" reply would let anyone
/// unauthenticated enumerate accounts, making the login decoy pointless.
/// The UX cost (a typo-collision looks like success until the user tries to
/// log in) is deliberate. The hash comes before any lookup, so every path
/// pays the same argon2 bill first.
#[utoipa::path(
    post,
    path = "/register",
    tag = "auth",
    request_body = RegisterCredentials,
    responses(
        (status = 201, description = "Account created, the username was already taken, or an existing person was attached to this school — deliberately indistinguishable. Carries no `id`: on the taken path there is no row to name, so log in to learn who you are", body = RegisterResponse),
        (status = 400, description = "Invalid username or password", body = ErrorResponse),
        (status = 401, description = "No such school — deliberately the same answer a bad credential gets", body = ErrorResponse),
        (status = 403, description = "The school is suspended", body = ErrorResponse),
        (status = 429, description = "Too many attempts from this address; see Retry-After", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn register(
    State(st): State<AppState>,
    Json(req): Json<RegisterCredentials>,
) -> Result<(StatusCode, Json<RegisterResponse>), AppError> {
    // The school first: a registration into a school that does not exist, or
    // one that is suspended, must not reach the (expensive) hashing path — and
    // an unknown school answers `401` here for the same anti-enumeration reason
    // login does, rather than confirming which schools this deployment serves.
    let school = Slug::try_new(&req.school).map_err(|_| AppError::Unauthorized)?;
    let db = st.tenants.get(&school).await?;
    let username = Username::try_new(&req.username)?;
    // Registration-level policy, not a `Username` invariant: these names read
    // as staff and invite impersonation, but the `ADMIN_USERNAME` bootstrap
    // must still be able to seed e.g. `admin` through the same newtype.
    if RESERVED_USERNAMES.contains(&username.as_str()) {
        return Err(ValidationError::Invalid {
            field: "username",
            reason: "this username is reserved",
        }
        .into());
    }
    // Hash BEFORE any lookup, never after: the ~33ms of argon2 is what makes
    // every outcome cost the same up front, so taken-vs-free can't be spotted
    // by a fast reply.
    let password = Password::try_new(&req.password)?;
    let password_hash = password.hash_async().await?;
    // Built once, before the branch: every path answers with the very same
    // value, so they cannot be told apart by construction rather than by
    // keeping two field lists in sync. Every fresh account starts as a
    // student, so this holds whichever way the writes go.
    let body = RegisterResponse {
        username: username.as_str().to_string(),
        role: DomainRole::Student.into(),
    };

    // Person half (control database): create the global account, or meet the
    // one already standing. The stored credential is never overwritten here —
    // the caller has not proven they know it until the verify below.
    let person =
        service::person::create_or_load(&st.db, username.clone(), password_hash.clone()).await?;
    if person.get_password_hash().verify_async(&password).await {
        // School half, in the cross-database order `app_user` then
        // membership (no distributed transaction spans the two databases, so
        // each half is idempotent and a retry after a torn pair completes
        // it). A taken school username means the person is already a member
        // — the same 201.
        match service::user::create(&db, username, Some(*person.get_id())).await {
            Ok(_) => {}
            Err(AppError::Conflict(_)) => {
                tracing::info!("register: school username already taken, answering 201");
            }
            Err(err) => return Err(err),
        }
        service::person::link_school(&st.db, person.get_id(), &school).await?;
    } else {
        tracing::info!("register: username taken, password does not match, answering 201");
    }
    Ok((StatusCode::CREATED, Json(body)))
}

/// Log in with username + password — no school. Sets a `session` cookie on
/// success: exactly one active membership enters that school right away
/// (`<slug>.<token>` and the full [`UserResponse`], unchanged for
/// single-school clients), several answer a [`SchoolChoiceResponse`] with a
/// `person.<token>` cookie that `POST /auth/school` binds.
#[utoipa::path(
    post,
    path = "/login",
    tag = "auth",
    request_body = LoginCredentials,
    responses(
        (status = 200, description = "Logged in — either straight into the one school (a `UserResponse` and a `<slug>.<token>` cookie), or as a multi-school person (a `{username, schools}` choice list and a `person.<token>` cookie to bind with `POST /auth/school`). Suspended schools appear in neither shape", body = LoginResponse),
        (status = 401, description = "Bad credentials — unknown username, wrong password, or no school membership left to enter, all deliberately indistinguishable", body = ErrorResponse),
        (status = 403, description = "Every school the person belongs to is suspended", body = ErrorResponse),
        (status = 429, description = "Too many attempts from this address; see Retry-After", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn login(
    State(st): State<AppState>,
    jar: CookieJar,
    Json(req): Json<LoginCredentials>,
) -> Result<(CookieJar, Json<LoginResponse>), AppError> {
    // A malformed password is a bad credential (`401`, not a `422`): the body
    // parsed, the credential did not. Same answer an unknown username gets —
    // neither may help a caller tell them apart.
    let password = Password::try_new(&req.password).map_err(|_| AppError::Unauthorized)?;
    // The account is a person in the control database; a school is picked
    // after login, not named by it. Usernames are stored trimmed (see
    // `Username::try_new`); trim the lookup the same way so a padded attempt
    // matches the canonical name.
    let person = match service::person::find_by_username(&st.db, req.username.trim()).await? {
        // Verification is `.await`ed so argon2 runs on the blocking pool instead
        // of stalling an async worker; that rules out a match guard, which
        // cannot await.
        Some(person) => {
            if !person.get_password_hash().verify_async(&password).await {
                return Err(AppError::Unauthorized);
            }
            person
        }
        None => {
            // No such person. Still do the argon2 work against a decoy so the reply
            // takes as long as a real (wrong-password) check — otherwise the
            // timing difference leaks which usernames exist.
            PasswordHash::verify_decoy_async(&password).await;
            return Err(AppError::Unauthorized);
        }
    };

    let memberships = service::person::memberships(&st.db, person.get_id()).await?;
    let active: Vec<_> = memberships.iter().filter(|m| m.is_active()).collect();
    match active.as_slice() {
        // A person with no membership cannot enter anything: the same `401`
        // a bad credential gets, so a register that created only the person
        // half (a torn cross-database pair) is not a signal either.
        [] if memberships.is_empty() => Err(AppError::Unauthorized),
        // Every membership sits in a suspended school: closed to its users,
        // including this one.
        [] => Err(AppError::Forbidden("school is suspended")),
        [only] => {
            let db = st.tenants.get(only.slug()).await?;
            // The school-side row the session binds to. A membership without
            // its `app_user` (a torn create) refuses like a bad credential
            // rather than confirm anything.
            let Some(user) =
                service::user::find_by_username(&db, person.get_username().as_str()).await?
            else {
                return Err(AppError::Unauthorized);
            };
            // The caller is genuine; opportunistically drop expired session rows.
            let _ = service::session::purge_expired(&db).await;
            let session = service::session::create(&db, user.get_id()).await?;
            // `<slug>.<token>`: the cookie carries the school, so every later
            // request finds its database without a second lookup path that
            // could disagree.
            let cookie = session_cookie(
                format!("{}.{}", only.slug(), session.token().as_str()),
                st.cookie_secure,
            );
            Ok((
                jar.add(cookie),
                Json(LoginResponse::LoggedIn(Box::new(UserResponse::new(&user)))),
            ))
        }
        many => {
            // Not entering any school yet: the `person.<token>` cookie is the
            // not-chosen state, and `POST /auth/school` exchanges it. The
            // list carries slug and display name, suspended schools omitted.
            let session = service::person::create_session(&st.db, person.get_id()).await?;
            let cookie = session_cookie(
                format!("{PERSON_COOKIE_PREFIX}.{}", session.token().as_str()),
                st.cookie_secure,
            );
            let schools = many
                .iter()
                .map(|m| SchoolChoice {
                    slug: m.slug().as_str().to_string(),
                    name: m.name().to_string(),
                })
                .collect();
            Ok((
                jar.add(cookie),
                Json(LoginResponse::ChooseSchool(SchoolChoiceResponse {
                    username: person.get_username().as_str().to_string(),
                    schools,
                })),
            ))
        }
    }
}

/// Bind a `person.<token>` session to one of the person's schools: the
/// cookie is replaced with that school's own `<slug>.<token>` and the person
/// session is revoked. Deliberately outside the credential rate-limit tier —
/// this is not a credential guess, and it requires a session cookie already.
#[utoipa::path(
    post,
    path = "/school",
    tag = "auth",
    request_body = SelectSchool,
    responses(
        (status = 200, description = "School selected; the person cookie is replaced by the school session cookie", body = UserResponse),
        (status = 401, description = "No person session, or the school is not among the caller's memberships — deliberately indistinguishable (no cookie at all, a school or builder cookie, and a slug the person does not hold all answer the same)", body = ErrorResponse),
        (status = 403, description = "The school is suspended", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn select_school(
    State(st): State<AppState>,
    jar: CookieJar,
    Json(req): Json<SelectSchool>,
) -> Result<(CookieJar, Json<UserResponse>), AppError> {
    // Only a person cookie binds a school: a school cookie already names its
    // school (switching is logout + login again), and a builder cookie names
    // no school at all — the same `401` every school surface gives them.
    let Some((prefix, token)) = jar.get("session").and_then(|c| split_cookie(c.value())) else {
        return Err(AppError::Unauthorized);
    };
    if prefix != PERSON_COOKIE_PREFIX {
        return Err(AppError::Unauthorized);
    }
    let session = service::person::find_session_by_token(&st.db, token)
        .await?
        .filter(|s| !s.is_expired())
        .ok_or(AppError::Unauthorized)?;
    // Membership required, and a slug that names nothing is the same refusal
    // as one the person does not hold — the list login already gave them is
    // not a directory of the deployment.
    let slug = Slug::try_new(&req.school).map_err(|_| AppError::Unauthorized)?;
    service::person::membership_of(&st.db, session.person(), &slug)
        .await?
        .ok_or(AppError::Unauthorized)?;
    // Suspended → `403` here, like on every school door.
    let db = st.tenants.get(&slug).await?;
    let person = service::person::read(&st.db, session.person())
        .await?
        .ok_or(AppError::Unauthorized)?;
    let Some(user) = service::user::find_by_username(&db, person.get_username().as_str()).await?
    else {
        return Err(AppError::Unauthorized);
    };
    // The school session first, then the person session goes: if the create
    // fails the person cookie still works and the same request can be
    // retried — a revoked cookie with no school session is a lockout.
    let school_session = service::session::create(&db, user.get_id()).await?;
    service::person::delete_session_by_token(&st.db, token).await?;
    let cookie = session_cookie(
        format!("{slug}.{}", school_session.token().as_str()),
        st.cookie_secure,
    );
    Ok((jar.add(cookie), Json(UserResponse::new(&user))))
}

/// The `session` cookie, however it was earned. Shared by school login and the
/// builder surface (`web::builder`) so the two can never drift apart on path,
/// flags or lifetime — the value is the only difference between them
/// (`<slug>.<token>` against `builder.<token>`).
pub(crate) fn session_cookie(value: String, secure: bool) -> Cookie<'static> {
    Cookie::build(("session", value))
        .path("/")
        .http_only(true)
        .secure(secure)
        // Lax is part of the CORS defense: it keeps this cookie off cross-site
        // requests. Before relaxing toward SameSite=None, first make sure
        // `cors_layer` (lib.rs) can never mirror origins with credentials on.
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(SESSION_DURATION_DAYS))
        .build()
}

/// Log out: revoke the current session (if any) and clear the cookie.
/// Idempotent — no session required; answers `204` either way.
#[utoipa::path(
    post,
    path = "/logout",
    tag = "auth",
    responses((status = 204, description = "Logged out (no-op without a session)")),
)]
async fn logout(
    State(st): State<AppState>,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), AppError> {
    // Best-effort: the cookie names its own session kind, so a logout
    // resolves it and deletes the row there. A cookie naming a school that is
    // gone or suspended still clears below — logging out must never fail.
    if let Some((prefix, token)) = jar.get("session").and_then(|c| split_cookie(c.value())) {
        if prefix == BUILDER_COOKIE_PREFIX {
            crate::service::builder::delete_by_token(&st.db, token).await?;
        } else if prefix == PERSON_COOKIE_PREFIX {
            service::person::delete_session_by_token(&st.db, token).await?;
        } else if let Ok(slug) = Slug::try_new(prefix)
            && let Ok(db) = st.tenants.get(&slug).await
        {
            service::session::delete_by_token(&db, token).await?;
        }
    }
    let jar = jar.remove(Cookie::build(("session", "")).path("/").build());
    Ok((jar, StatusCode::NO_CONTENT))
}

/// Return the currently authenticated user.
#[utoipa::path(
    get,
    path = "/me",
    tag = "auth",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The current user", body = UserResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn me(CurrentUser(user): CurrentUser) -> Json<UserResponse> {
    Json(UserResponse::new(&user))
}
