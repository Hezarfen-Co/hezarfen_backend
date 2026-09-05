//! Conditional-GET: the ETag middleware emits a revalidatable `ETag` +
//! `Cache-Control` on `200` JSON GETs, answers a matching `If-None-Match` with
//! a bodyless `304`, and never touches a mutation.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get as route_get;
use common::mem_app;
use tower::ServiceExt;

/// Wrap a one-route router in the real ETag middleware — lets a test stand up a
/// JSON GET whose handler pre-sets its own cache headers (no such endpoint
/// exists in the app, so it can't be exercised in-tree).
fn behind_etag(router: Router) -> Router {
    router.layer(middleware::from_fn(hezarfen_backend::web::etag::etag))
}

/// `GET uri`, optionally carrying an `If-None-Match`. Returns status, headers,
/// and the (possibly empty) body bytes.
async fn get(
    app: &axum::Router,
    uri: &str,
    if_none_match: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(v) = if_none_match {
        builder = builder.header("if-none-match", v);
    }
    let res = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

#[tokio::test]
async fn get_carries_etag_and_revalidation() {
    let app = mem_app().await;
    let (status, headers, body) = get(&app, "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("etag").is_some(), "a 200 GET carries an ETag");
    assert_eq!(
        headers.get("cache-control").unwrap(),
        "private, no-cache",
        "no-cache is what makes the browser revalidate"
    );
    assert!(!body.is_empty(), "the first fetch returns the body");
}

#[tokio::test]
async fn matching_if_none_match_gets_304_empty() {
    let app = mem_app().await;
    let (_, headers, _) = get(&app, "/health", None).await;
    let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();

    // The same fetch with that validator revalidates to a bodyless 304.
    let (status, headers, body) = get(&app, "/health", Some(&etag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty(), "a 304 carries no body");
    assert_eq!(headers.get("etag").unwrap().to_str().unwrap(), etag);

    // A stale validator still gets the full 200 payload.
    let (status, _, body) = get(&app, "/health", Some("\"deadbeef\"")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());
}

/// RFC 9110 §13.1.2: `If-None-Match` uses the *weak* comparison function, so a
/// weakened copy of our own tag — what a gzipping reverse proxy sends back —
/// must still revalidate to a `304`.
#[tokio::test]
async fn weak_if_none_match_gets_304_empty() {
    let app = mem_app().await;
    let (_, headers, _) = get(&app, "/health", None).await;
    let etag = headers.get("etag").unwrap().to_str().unwrap().to_string();

    let (status, _, body) = get(&app, "/health", Some(&format!("W/{etag}"))).await;
    assert_eq!(
        status,
        StatusCode::NOT_MODIFIED,
        "W/ prefix must not defeat the match"
    );
    assert!(body.is_empty(), "a 304 carries no body");

    // A weakened *different* tag is still a miss.
    let (status, _, body) = get(&app, "/health", Some("W/\"deadbeef\"")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());
}

#[tokio::test]
async fn handler_cache_control_is_not_overwritten() {
    async fn handler() -> impl IntoResponse {
        (
            [
                (CONTENT_TYPE, "application/json"),
                (CACHE_CONTROL, "public, max-age=60"),
            ],
            "{\"a\":1}",
        )
    }
    let app = behind_etag(Router::new().route("/x", route_get(handler)));
    let (status, headers, _) = get(&app, "/x", None).await;
    assert_eq!(status, StatusCode::OK);
    // We still add our validator...
    assert!(
        headers.get("etag").is_some(),
        "no handler ETag, so we set one"
    );
    // ...but the handler's own directive wins.
    assert_eq!(headers.get("cache-control").unwrap(), "public, max-age=60");
}

#[tokio::test]
async fn handler_owned_etag_passes_through_and_never_304s() {
    async fn handler() -> impl IntoResponse {
        (
            [
                (CONTENT_TYPE, "application/json"),
                (ETAG, "\"handler-owned\""),
            ],
            "{\"a\":1}",
        )
    }
    let app = behind_etag(Router::new().route("/x", route_get(handler)));

    // The handler's ETag is left exactly as-is; no Cache-Control is imposed.
    let (status, headers, body) = get(&app, "/x", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("etag").unwrap(), "\"handler-owned\"");
    assert!(headers.get("cache-control").is_none());
    assert!(!body.is_empty());

    // Even If-None-Match matching that ETag stays a full 200 — our body-hash
    // 304 logic never runs against a handler's own validator.
    let (status, _, body) = get(&app, "/x", Some("\"handler-owned\"")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());
}

#[tokio::test]
async fn mutations_are_never_etagged() {
    let app = mem_app().await;
    let creds = serde_json::json!({ "school": "demo", "username": "ann", "password": "secret1" });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(creds.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    assert!(
        res.headers().get("etag").is_none(),
        "a mutation is never given an ETag"
    );
}
