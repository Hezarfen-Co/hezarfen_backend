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
use crate::domain::session::Session;
use crate::domain::user::{Password, PasswordHash, User, Username};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::rate_limit::{RateLimitConfig, RateLimiter};
use crate::state::AppState;

use super::dto::Role;
use super::{CurrentUser, UserResponse};

pub fn routes(state: &AppState) -> OpenApiRouter<AppState> {
    // Strict per-IP limit on the two credential endpoints only — the
    // brute-force and username-enumeration surface. `me` and `logout` stay
    // outside it: browser frontends poll `me` on every page load, and being
    // over the auth limit must never block an intentional logout.
    let rate_limit: &RateLimitConfig = &state.rate_limit;
    let limiter = RateLimiter::per_minute(rate_limit.auth_per_minute, rate_limit.trust_proxy);
    // The budget survives a restart — the tier only means something if a
    // brute-forcer cannot reset it by waiting out a deploy.
    limiter.share("auth", state.db.clone(), state.db_up.clone());
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

/// Register a new user account.
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
        (status = 429, description = "Too many attempts from this address; see Retry-After", body = ErrorResponse),
    ),
)]
async fn register(
    State(st): State<AppState>,
    Json(req): Json<Credentials>,
) -> Result<(StatusCode, Json<RegisterResponse>), AppError> {
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
    // Hash BEFORE the availability check inside `User::create`, never after: the
    // ~33ms of argon2 is what makes both outcomes cost the same, so a taken
    // username can't be spotted by a fast reply. The only work the taken path
    // skips is the insert itself, orders of magnitude below hashing.
    let password_hash = Password::try_new(&req.password)?.hash_async().await?;
    // Built once, before the branch: the created and the taken path answer with
    // the very same value, so they cannot be told apart by construction rather
    // than by keeping two field lists in sync. Every fresh account starts as a
    // student (`User::create`), so this holds whichever way the insert goes.
    let body = RegisterResponse {
        username: username.as_str().to_string(),
        role: DomainRole::Student.into(),
    };
    match User::create(username, password_hash, &st.db).await {
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

/// Log in with username + password. Sets a `session` cookie on success.
#[utoipa::path(
    post,
    path = "/login",
    tag = "auth",
    request_body = Credentials,
    responses(
        (status = 200, description = "Logged in; session cookie set", body = UserResponse),
        (status = 401, description = "Bad credentials", body = ErrorResponse),
        (status = 429, description = "Too many attempts from this address; see Retry-After", body = ErrorResponse),
    ),
)]
async fn login(
    State(st): State<AppState>,
    jar: CookieJar,
    Json(req): Json<Credentials>,
) -> Result<(CookieJar, Json<UserResponse>), AppError> {
    let password = Password::try_new(&req.password).map_err(|_| AppError::Unauthorized)?;
    // Usernames are stored trimmed (see `Username::try_new`); trim the lookup
    // the same way so a padded login attempt matches the canonical name.
    let user = match User::find_by_username(req.username.trim(), &st.db).await? {
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
    let _ = Session::purge_expired(&st.db).await;

    let session = Session::create(user.get_id(), &st.db).await?;
    let cookie = Cookie::build(("session", session.token().as_str().to_string()))
        .path("/")
        .http_only(true)
        .secure(st.cookie_secure)
        // Lax is part of the CORS defense: it keeps this cookie off cross-site
        // requests. Before relaxing toward SameSite=None, first make sure
        // `cors_layer` (lib.rs) can never mirror origins with credentials on.
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(SESSION_DURATION_DAYS))
        .build();

    Ok((jar.add(cookie), Json(UserResponse::new(&user))))
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
    if let Some(cookie) = jar.get("session") {
        Session::delete_by_token(cookie.value(), &st.db).await?;
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
