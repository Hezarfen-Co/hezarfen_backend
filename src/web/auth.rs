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
use super::{BUILDER_COOKIE_PREFIX, CurrentUser, UserResponse};

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
        .routes(routes!(logout))
        .routes(routes!(me))
}

#[derive(Deserialize, ToSchema)]
struct Credentials {
    /// The school's slug — the name in front of the dot in the session cookie.
    /// One backend serves many schools, so a username only identifies an
    /// account together with this.
    #[schema(example = "demo", min_length = 2, max_length = 32)]
    school: String,
    #[schema(example = "ada", min_length = 3, max_length = 32)]
    username: String,
    #[schema(example = "correct horse battery", min_length = 6, max_length = 128)]
    password: String,
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

/// Register a new user account: `{school, username, password}` in, `{username, role}`
/// back (no `id`; new accounts are `student`). Always `201`, even if the name
/// was already taken — see "Auth model".
///
/// Answers `201` whether or not the username was free: a distinguishable
/// "already taken" reply lets anyone unauthenticated enumerate every account in
/// the school, which would make the constant-cost decoy on the login path
/// pointless. A taken username is *not* re-created or overwritten — only the
/// reply is uniform. The UX cost (a typo-collision looks like success until the
/// user tries to log in) is deliberate. Both outcomes return the *same*
/// [`RegisterResponse`] value, built once before the branch — there is no
/// per-path body that could drift apart.
#[utoipa::path(
    post,
    path = "/register",
    tag = "auth",
    request_body = Credentials,
    responses(
        (status = 201, description = "Account created, or the username was already taken — deliberately indistinguishable. Carries no `id`: on the taken path there is no row to name, so log in to learn who you are", body = RegisterResponse),
        (status = 400, description = "Invalid username or password", body = ErrorResponse),
        (status = 401, description = "No such school — deliberately the same answer a bad credential gets", body = ErrorResponse),
        (status = 403, description = "The school is suspended", body = ErrorResponse),
        (status = 429, description = "Too many attempts from this address; see Retry-After", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn register(
    State(st): State<AppState>,
    Json(req): Json<Credentials>,
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
    // Hash BEFORE the availability check inside the user-create path, never
    // after: the
    // ~33ms of argon2 is what makes both outcomes cost the same, so a taken
    // username can't be spotted by a fast reply. The only work the taken path
    // skips is the insert itself, orders of magnitude below hashing.
    let password_hash = Password::try_new(&req.password)?.hash_async().await?;
    // Built once, before the branch: the created and the taken path answer with
    // the very same value, so they cannot be told apart by construction rather
    // than by keeping two field lists in sync. Every fresh account starts as a
    // student (`service::user::create`), so this holds whichever way the insert goes.
    let body = RegisterResponse {
        username: username.as_str().to_string(),
        role: DomainRole::Student.into(),
    };
    match crate::service::user::create(&db, username, password_hash).await {
        Ok(_) => {}
        // Taken. Log the real reason server-side; the caller gets the same 201
        // and the same body, because telling the two apart is the whole thing
        // we're denying. The account is not re-created or overwritten.
        Err(AppError::Conflict(_)) => {
            tracing::info!("register: username already taken, answering 201");
        }
        Err(err) => return Err(err),
    }
    Ok((StatusCode::CREATED, Json(body)))
}

/// Log in with school + username + password. Sets a `session` cookie
/// (`<school>.<token>`) on success.
#[utoipa::path(
    post,
    path = "/login",
    tag = "auth",
    request_body = Credentials,
    responses(
        (status = 200, description = "Logged in; session cookie set", body = UserResponse),
        (status = 401, description = "Bad credentials, or no such school — deliberately indistinguishable", body = ErrorResponse),
        (status = 403, description = "The school is suspended", body = ErrorResponse),
        (status = 429, description = "Too many attempts from this address; see Retry-After", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn login(
    State(st): State<AppState>,
    jar: CookieJar,
    Json(req): Json<Credentials>,
) -> Result<(CookieJar, Json<UserResponse>), AppError> {
    // Resolved *before* the argon2 work: a login aimed at a school that does
    // not exist is a bad credential (`401`, indistinguishable from a bad
    // password — the school list is not public), and a suspended school is
    // `403` for every request including this one, so neither should buy an
    // attacker a hash.
    let school = Slug::try_new(&req.school).map_err(|_| AppError::Unauthorized)?;
    let db = st.tenants.get(&school).await?;
    let password = Password::try_new(&req.password).map_err(|_| AppError::Unauthorized)?;
    // Usernames are stored trimmed (see `Username::try_new`); trim the lookup
    // the same way so a padded login attempt matches the canonical name.
    let user = match crate::service::user::find_by_username(&db, req.username.trim()).await? {
        // Verification is `.await`ed so argon2 runs on the blocking pool instead
        // of stalling an async worker; that rules out a match guard, which
        // cannot await.
        Some(user) => {
            if !user.get_password_hash().verify_async(&password).await {
                return Err(AppError::Unauthorized);
            }
            user
        }
        None => {
            // No such user. Still do the argon2 work against a decoy so the reply
            // takes as long as a real (wrong-password) check — otherwise the
            // timing difference leaks which usernames exist.
            PasswordHash::verify_decoy_async(&password).await;
            return Err(AppError::Unauthorized);
        }
    };

    // The caller is genuine; opportunistically drop any expired session rows.
    let _ = service::session::purge_expired(&db).await;

    let session = service::session::create(&db, user.get_id()).await?;
    // `<slug>.<token>`: the cookie carries the school, so every later request
    // finds its database without a second lookup path that could disagree.
    let cookie = session_cookie(
        format!("{school}.{}", session.token().as_str()),
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
    // Best-effort: the cookie names its own school, so a logout resolves that
    // school and deletes the row there. A cookie naming a school that is gone
    // or suspended still clears below — logging out must never fail.
    if let Some((prefix, token)) = jar.get("session").and_then(|c| split_cookie(c.value())) {
        if prefix == BUILDER_COOKIE_PREFIX {
            crate::service::builder::delete_by_token(&st.db, token).await?;
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
