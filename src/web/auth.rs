use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{RESERVED_USERNAMES, SESSION_DURATION_DAYS};
use crate::domain::session::Session;
use crate::domain::user::{Password, PasswordHash, User, Username};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::rate_limit::{RateLimitConfig, RateLimiter};
use crate::state::AppState;

use super::{CurrentUser, UserResponse};

pub fn routes(rate_limit: &RateLimitConfig) -> OpenApiRouter<AppState> {
    // Strict per-IP limit on the two credential endpoints only — the
    // brute-force and username-enumeration surface. `me` and `logout` stay
    // outside it: browser frontends poll `me` on every page load, and being
    // over the auth limit must never block an intentional logout.
    let limiter = RateLimiter::per_minute(rate_limit.auth_per_minute, rate_limit.trust_proxy);
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
    #[schema(example = "ada")]
    username: String,
    #[schema(example = "correct horse battery")]
    password: String,
}

/// Register a new user account.
#[utoipa::path(
    post,
    path = "/register",
    tag = "auth",
    request_body = Credentials,
    responses(
        (status = 201, description = "Account created", body = UserResponse),
        (status = 400, description = "Invalid username or password", body = ErrorResponse),
        (status = 409, description = "Username already taken", body = ErrorResponse),
        (status = 429, description = "Too many attempts from this address; see Retry-After", body = ErrorResponse),
    ),
)]
async fn register(
    State(st): State<AppState>,
    Json(req): Json<Credentials>,
) -> Result<(StatusCode, Json<UserResponse>), AppError> {
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
    let password_hash = Password::try_new(&req.password)?.hash_async().await?;
    let user = User::create(username, password_hash, &st.db).await?;
    Ok((StatusCode::CREATED, Json(UserResponse::new(&user))))
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
