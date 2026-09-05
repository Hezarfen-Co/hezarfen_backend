//! The school-scoped replacement for `axum::extract::State`.
//!
//! [`State`] is named to **shadow** `axum::extract::State` on purpose: a
//! handler file swaps one import and every `State(st)` in it starts working in
//! the caller's school instead of the control database, with no handler body
//! touched. That is the whole design — there is no `st.db` a handler could
//! forget to scope, because the only `st` it can name is already scoped.
//!
//! Three files keep the real `axum::extract::State` because they legitimately
//! see the control database: `web::auth` (it resolves the school itself, and
//! its rate limiter is deployment-wide), `web::limits` (constants, no rows) and
//! `web::ai` (bridge certificate, no rows).

use std::path::PathBuf;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;

use crate::database::Database;
use crate::error::AppError;
use crate::state::AppState;
use crate::tenant::Slug;

/// The school a request has already been resolved into, injected as a request
/// extension by an in-process caller that has no cookie — today only the AI
/// bridge. Extensions cannot be set from outside the process, so this is
/// unforgeable over HTTP, exactly like
/// [`crate::web::extractor::AiPrincipal`].
#[derive(Clone)]
pub struct TenantExt {
    pub slug: Slug,
    pub db: Database,
}

/// Split a `session` cookie into its school part and its token. `builder` in
/// the first position is the deployment operator; anything else is a slug.
///
/// Splits at the **first** dot, so a token can never be mistaken for a slug no
/// matter what it contains (it is hex today, and this does not depend on that).
pub fn split_cookie(value: &str) -> Option<(&str, &str)> {
    let (prefix, token) = value.split_once('.')?;
    (!prefix.is_empty() && !token.is_empty()).then_some((prefix, token))
}

/// The school behind this request: the `TenantExt` extension if one was
/// injected, else the `session` cookie's prefix.
///
/// Shared with [`crate::web::extractor`] so the two can never disagree about
/// which school a request belongs to — the authenticated user and the rows
/// they read must come from the same database.
pub(crate) async fn resolve_tenant<S>(
    parts: &mut Parts,
    state: &S,
) -> Result<(Slug, Database), AppError>
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    if let Some(tenant) = parts.extensions.get::<TenantExt>() {
        return Ok((tenant.slug.clone(), tenant.db.clone()));
    }
    let jar = CookieJar::from_request_parts(parts, state)
        .await
        .map_err(|_| AppError::Unauthorized)?;
    let cookie = jar.get("session").ok_or(AppError::Unauthorized)?;
    // A builder cookie names no school, and a cookie with no dot names nothing
    // at all. Both are `401` here: the builder surface is a different extractor
    // (`RequireBuilder`), and this one must never let it in.
    let (prefix, _) = split_cookie(cookie.value()).ok_or(AppError::Unauthorized)?;
    let slug = Slug::try_new(prefix).map_err(|_| AppError::Unauthorized)?;

    let app = AppState::from_ref(state);
    let db = app.tenants.get(&slug).await?;
    Ok((slug, db))
}

/// The application state, narrowed to the caller's school. Drop-in for
/// `axum::extract::State<AppState>` in every handler that reads school data.
///
/// Unauthenticated callers are refused *here*, before the handler runs — which
/// is correct for every route that uses it, since all of them already require a
/// session to do anything.
pub struct State<T>(pub T);

impl<S> FromRequestParts<S> for State<AppState>
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let (slug, db) = resolve_tenant(parts, state).await?;
        let app = AppState::from_ref(state);
        Ok(State(AppState {
            db,
            files_path: school_files_path(&app.files_path, &slug),
            ..app
        }))
    }
}

/// Just the school, for the handlers that need it as a key prefix (the exam
/// presence map, the whiteboard hub) rather than as a database handle.
pub struct SchoolSlug(pub Slug);

impl<S> FromRequestParts<S> for SchoolSlug
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Ok(SchoolSlug(resolve_tenant(parts, state).await?.0))
    }
}

/// Where a school's uploaded blobs live: one directory per school under the
/// deployment's `FILES_PATH`. The slug's character set is what makes this a
/// plain `join` — it can contain no separator and no `..`.
pub fn school_files_path(root: &std::path::Path, slug: &Slug) -> PathBuf {
    root.join(slug.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Router, body::Body, http::Request};
    use tower::ServiceExt;

    use crate::database::init_mem_tenants;
    use crate::domain::builder::{Builder, BuilderSession};
    use crate::domain::session::Session;
    use crate::domain::user::{Password, User, Username};
    use crate::tenant::DEMO_SLUG;
    use crate::web::{CurrentUser, RequireBuilder};

    async fn school_surface(State(_): State<AppState>, CurrentUser(_): CurrentUser) -> StatusCode {
        StatusCode::OK
    }

    async fn builder_surface(RequireBuilder(_): RequireBuilder) -> StatusCode {
        StatusCode::OK
    }

    /// A two-route app, one live cookie of each kind, and the registry behind
    /// them.
    async fn harness() -> (Router, String, String, crate::tenant::Tenants) {
        let tenants = init_mem_tenants().await.expect("in-memory deployment");
        let school = tenants
            .get(&Slug::try_new(DEMO_SLUG).unwrap())
            .await
            .expect("the demo school");

        let user = User::create(
            Username::try_new("ada").unwrap(),
            Password::try_new("secret1")
                .unwrap()
                .hash_async()
                .await
                .unwrap(),
            &school,
        )
        .await
        .unwrap();
        let session = Session::create(user.get_id(), &school).await.unwrap();
        let school_cookie = format!("session={DEMO_SLUG}.{}", session.token().as_str());

        Builder::ensure(
            Username::try_new("operator").unwrap(),
            Password::try_new("secret1").unwrap(),
            tenants.control(),
        )
        .await
        .unwrap();
        let builder = Builder::find_by_username("operator", tenants.control())
            .await
            .unwrap()
            .unwrap();
        let builder_session = BuilderSession::create(builder.get_id(), tenants.control())
            .await
            .unwrap();
        let builder_cookie = format!("session=builder.{}", builder_session.token().as_str());

        let state = AppState {
            db: tenants.control().clone(),
            tenants: tenants.clone(),
            files_path: std::env::temp_dir(),
            cookie_secure: false,
            rate_limit: crate::rate_limit::RateLimitConfig::unlimited(),
            chatbot_limit: Default::default(),
            exam_presence: Default::default(),
            board_hub: Default::default(),
            db_up: Default::default(),
            ai: None,
        };
        let app = Router::new()
            .route("/school", get(school_surface))
            .route("/builder", get(builder_surface))
            .with_state(state);
        (app, school_cookie, builder_cookie, tenants)
    }

    async fn status(app: &Router, uri: &str, cookie: Option<&str>) -> StatusCode {
        let mut builder = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// The two principals share a cookie name and nothing else. Neither may
    /// ever be accepted where the other belongs — a builder holding school
    /// power, or a student reaching the school-management surface, is the one
    /// failure this whole split exists to prevent.
    #[tokio::test]
    async fn neither_cookie_works_on_the_others_surface() {
        let (app, school, builder, _) = harness().await;

        assert_eq!(status(&app, "/school", Some(&school)).await, StatusCode::OK);
        assert_eq!(
            status(&app, "/builder", Some(&builder)).await,
            StatusCode::OK
        );

        assert_eq!(
            status(&app, "/school", Some(&builder)).await,
            StatusCode::UNAUTHORIZED,
            "a builder cookie must not pass a school extractor"
        );
        assert_eq!(
            status(&app, "/builder", Some(&school)).await,
            StatusCode::UNAUTHORIZED,
            "a school cookie must not pass RequireBuilder"
        );

        // A cookie with no school part names nothing resolvable — including the
        // pre-tenancy shape, which is deliberately not accepted anywhere.
        let bare = school.replace(&format!("{DEMO_SLUG}."), "");
        for uri in ["/school", "/builder"] {
            assert_eq!(
                status(&app, uri, Some(&bare)).await,
                StatusCode::UNAUTHORIZED,
                "{uri} accepted a cookie with no school"
            );
            assert_eq!(
                status(&app, uri, None).await,
                StatusCode::UNAUTHORIZED,
                "{uri} accepted no cookie at all"
            );
        }
    }

    /// A suspended school is refused at the edge, before any handler — with a
    /// session that was perfectly good a moment earlier, and through a handle
    /// the registry had already cached.
    #[tokio::test]
    async fn a_suspension_refuses_a_live_session() {
        let (app, school, _, tenants) = harness().await;
        let demo = Slug::try_new(DEMO_SLUG).unwrap();
        assert_eq!(status(&app, "/school", Some(&school)).await, StatusCode::OK);

        tenants
            .set_status(&demo, crate::tenant::SchoolStatus::Suspended)
            .await
            .unwrap();
        assert_eq!(
            status(&app, "/school", Some(&school)).await,
            StatusCode::FORBIDDEN,
            "a suspended school must be refused immediately, cached handle or not"
        );

        tenants
            .set_status(&demo, crate::tenant::SchoolStatus::Active)
            .await
            .unwrap();
        assert_eq!(
            status(&app, "/school", Some(&school)).await,
            StatusCode::OK,
            "un-suspending must restore the very same session"
        );
    }

    #[test]
    fn a_cookie_splits_at_its_first_dot_or_not_at_all() {
        assert_eq!(split_cookie("demo.abc123"), Some(("demo", "abc123")));
        assert_eq!(split_cookie("builder.abc123"), Some(("builder", "abc123")));
        // First dot, so a dotted token still yields the school it named.
        assert_eq!(split_cookie("demo.a.b"), Some(("demo", "a.b")));
        // A pre-tenancy cookie, an empty half, or nothing at all names no
        // school — every one of these must be refused, not guessed at.
        for bad in ["abc123", "", ".", "demo.", ".abc123"] {
            assert_eq!(split_cookie(bad), None, "{bad:?}");
        }
    }
}
