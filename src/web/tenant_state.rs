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
use crate::module::ModuleSet;
use crate::state::AppState;
use crate::tenant::Slug;

/// Everything one registry read tells us about the school behind a request:
/// its handle and what it has bought. Cached in the request's extensions by
/// [`resolve_tenant`], so a request that needs the school twice (the shadow
/// [`State`] *and* [`crate::web::CurrentUser`], which is most of them) reads
/// the registry once.
#[derive(Clone)]
pub struct ResolvedTenant {
    pub slug: Slug,
    pub db: Database,
    pub modules: ModuleSet,
}

/// The school a request has already been resolved into, injected as a request
/// extension by an in-process caller that has no cookie — today only the AI
/// bridge. Extensions cannot be set from outside the process, so this is
/// unforgeable over HTTP, exactly like
/// [`crate::web::extractor::AiPrincipal`].
#[derive(Clone)]
pub struct TenantExt {
    pub slug: Slug,
    pub db: Database,
    pub modules: ModuleSet,
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
) -> Result<ResolvedTenant, AppError>
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    if let Some(tenant) = parts.extensions.get::<ResolvedTenant>() {
        return Ok(tenant.clone());
    }
    if let Some(tenant) = parts.extensions.get::<TenantExt>() {
        return Ok(ResolvedTenant {
            slug: tenant.slug.clone(),
            db: tenant.db.clone(),
            modules: tenant.modules.clone(),
        });
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
    let resolved = app.tenants.resolve(&slug).await?;
    // The one caller-derived label telemetry is allowed to carry: which school,
    // never which user. The span field and the metric slot are set together so
    // a trace and its latency sample agree.
    tracing::Span::current().record("school", slug.as_str());
    if let Some(slot) = parts.extensions.get::<crate::telemetry::SchoolSlot>() {
        slot.set(slug.as_str());
    }
    // Memoized on the request, not just returned: the next extractor on this
    // same request (and the module gate ahead of both) reuses this verdict
    // instead of re-reading the registry row.
    parts.extensions.insert(resolved.clone());
    Ok(resolved)
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
        let tenant = resolve_tenant(parts, state).await?;
        let app = AppState::from_ref(state);
        Ok(State(AppState {
            db: tenant.db,
            files_path: school_files_path(&app.files_path, &tenant.slug),
            ..app
        }))
    }
}

/// The resolved school itself, for a handler that must carry it somewhere the
/// request does not reach — today the chatbot's detached answer task, which
/// hands it back to the bridge as a [`TenantExt`].
impl<S> FromRequestParts<S> for ResolvedTenant
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        resolve_tenant(parts, state).await
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
        Ok(SchoolSlug(resolve_tenant(parts, state).await?.slug))
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

    use crate::database::init_test_tenants;
    use crate::db::session;
    use crate::domain::user::{Password, Username};
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
        let tenants = init_test_tenants().await;
        let school = tenants
            .get(&Slug::try_new(DEMO_SLUG).unwrap())
            .await
            .expect("the demo school");

        let user = crate::service::user::create(&school, Username::try_new("ada").unwrap(), None)
            .await
            .unwrap();
        let session = session::create(&school, user.get_id()).await.unwrap();
        let school_cookie = format!("session={DEMO_SLUG}.{}", session.token().as_str());

        crate::service::builder::ensure(
            tenants.control(),
            Username::try_new("operator").unwrap(),
            Password::try_new("secret1").unwrap(),
        )
        .await
        .unwrap();
        let builder = crate::service::builder::find_by_username(tenants.control(), "operator")
            .await
            .unwrap()
            .unwrap();
        let builder_session =
            crate::service::builder::create_session(tenants.control(), builder.get_id())
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
            ai: None,
            metrics: crate::telemetry::Metrics::noop(),
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

    /// One request, one registry read. Measured by making the registry answer
    /// *differently* the second time: the school is suspended between the two
    /// extractor calls on the same request, so a second read would refuse. It
    /// still passes — the verdict came from the memoized extension.
    #[tokio::test]
    async fn a_request_resolves_its_school_exactly_once() {
        let tenants = init_test_tenants().await;
        let demo = Slug::try_new(DEMO_SLUG).unwrap();
        let state = AppState {
            db: tenants.control().clone(),
            tenants: tenants.clone(),
            files_path: std::env::temp_dir(),
            cookie_secure: false,
            rate_limit: crate::rate_limit::RateLimitConfig::unlimited(),
            chatbot_limit: Default::default(),
            exam_presence: Default::default(),
            board_hub: Default::default(),
            ai: None,
            metrics: crate::telemetry::Metrics::noop(),
        };
        let request = Request::builder()
            .uri("/school")
            .header("cookie", format!("session={DEMO_SLUG}.whatever"))
            .body(Body::empty())
            .unwrap();
        let (mut parts, _) = request.into_parts();

        resolve_tenant(&mut parts, &state)
            .await
            .expect("the first resolve reads the registry");
        assert!(
            parts.extensions.get::<ResolvedTenant>().is_some(),
            "the first resolve must memoize its verdict on the request"
        );

        tenants
            .set_status(&demo, crate::tenant::SchoolStatus::Suspended)
            .await
            .unwrap();
        let second = resolve_tenant(&mut parts, &state)
            .await
            .expect("a second read would have seen the suspension and refused");
        assert_eq!(second.slug, demo);
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
