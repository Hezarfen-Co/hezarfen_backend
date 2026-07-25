//! Conditional-GET (`ETag` / `If-None-Match` → `304 Not Modified`) middleware.
//!
//! The frontend polls its list endpoints often; this makes an unchanged poll a
//! cheap header-only `304` instead of a full JSON re-send (the browser HTTP
//! cache revalidates on its own once we emit the headers — no client change).
//!
//! Scoped tightly on purpose. Only a `GET` whose handler returned a
//! `200 application/json` response is touched; everything else passes through
//! untouched:
//!   * mutations (`POST`/`PATCH`/`DELETE`) and their non-`200` results,
//!   * errors, redirects, `204`s,
//!   * `text/event-stream` (SSE) and any streamed body — they carry no
//!     `Content-Length`, so they are never buffered (buffering one would hang),
//!   * file blobs (`image/*`, downloads) — non-JSON, so their own `no-store`
//!     survives untouched.
//!
//! The validator is a strong `ETag` — a `DefaultHasher` (SipHash) digest of the
//! body bytes. This is cache validation, not security, so a fast dependency-free
//! hasher is the right tool; no crypto dep is pulled in for it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use axum::body::{Body, HttpBody};
use axum::extract::Request;
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// Bodies larger than this are streamed through without buffering — JSON pages
/// sit far below it, so the cap only bounds worst-case memory. A response whose
/// `Content-Length` exceeds it (or is absent, i.e. a stream) skips the feature.
const MAX_BODY: u64 = 1 << 20; // 1 MiB

/// Add an `ETag` + revalidation headers to `200` JSON `GET`s, and short-circuit
/// a matching `If-None-Match` to a bodyless `304`.
pub async fn etag(request: Request, next: Next) -> Response {
    // `next.run` consumes the request, so capture the precondition up front.
    let is_get = request.method() == Method::GET;
    let if_none_match = request.headers().get(IF_NONE_MATCH).cloned();

    let response = next.run(request).await;

    if !is_get || response.status() != StatusCode::OK || !is_json(&response) {
        return response;
    }
    // A handler that set its own `ETag` owns its validation — its validator
    // scheme need not be a body hash, so we neither overwrite it nor run our
    // hash/304 logic against it. Pass it straight through. (No JSON GET handler
    // does this today; this is preemptive.)
    if response.headers().contains_key(ETAG) {
        return response;
    }
    // Buffer only an already-measured, bounded body. A body with no known upper
    // size is a stream (SSE) — never buffer that; over the cap, leave it alone.
    // (`Content-Length` isn't a header yet here — hyper writes it downstream —
    // so the size hint is the pre-buffer measurement.)
    match response.body().size_hint().upper() {
        Some(len) if len <= MAX_BODY => {}
        _ => return response,
    }

    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY as usize).await else {
        // Unreachable given the Content-Length check above; a truncated body is
        // worse than a missed cache, so fail closed on the empty-parts path.
        return Response::from_parts(parts, Body::empty());
    };

    let tag = compute_etag(&bytes);
    let tag_value = HeaderValue::from_str(&tag).expect("hex etag is a valid header value");
    parts.headers.insert(ETAG, tag_value);
    // `no-cache` = store but revalidate every time — this is what makes the
    // browser resend `If-None-Match` on the next poll. Without it the ETag is
    // inert. `private`: these are per-user authenticated responses. But if the
    // handler set its own directive, it knows best — leave it untouched.
    if !parts.headers.contains_key(CACHE_CONTROL) {
        parts
            .headers
            .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    }

    if matches(if_none_match.as_ref(), &tag) {
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers.remove(CONTENT_LENGTH);
        parts.headers.remove(CONTENT_TYPE);
        return Response::from_parts(parts, Body::empty());
    }

    Response::from_parts(parts, Body::from(bytes))
}

/// A JSON response — the only shape worth revalidating. Excludes SSE, blobs,
/// and Swagger HTML.
fn is_json(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"))
}

/// Strong `ETag` for `bytes`, formatted as a quoted hex string.
fn compute_etag(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}

/// `If-None-Match` compare: `*` matches anything, otherwise the (comma-
/// separated) candidate list must contain our tag. RFC 9110 §13.1.2 mandates the
/// *weak* comparison function here, so the `W/` prefix is stripped from both
/// sides and only the opaque quoted strings are compared — an intermediary that
/// weakens our ETag (nginx does this when it gzips) must still get its `304`.
fn matches(if_none_match: Option<&HeaderValue>, tag: &str) -> bool {
    let Some(value) = if_none_match.and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let value = value.trim();
    value == "*"
        || value
            .split(',')
            .map(str::trim)
            .any(|candidate| opaque(candidate) == opaque(tag))
}

/// The opaque part of a validator — the quoted string without any weak prefix.
fn opaque(validator: &str) -> &str {
    validator.strip_prefix("W/").unwrap_or(validator)
}

#[cfg(test)]
mod tests {
    use super::matches;
    use axum::http::HeaderValue;

    fn m(header: &str, tag: &str) -> bool {
        matches(Some(&HeaderValue::from_str(header).unwrap()), tag)
    }

    #[test]
    fn weak_comparison_is_symmetric() {
        // Both directions and both-weak: the opaque strings decide (RFC 9110
        // §13.1.2), never the `W/` prefix.
        assert!(m("W/\"abc\"", "\"abc\""), "weak candidate vs strong tag");
        assert!(m("\"abc\"", "W/\"abc\""), "strong candidate vs weak tag");
        assert!(m("W/\"abc\"", "W/\"abc\""), "both weak");
        assert!(m("\"abc\"", "\"abc\""), "both strong");
        // A different opaque string still misses, weak prefix or not.
        assert!(!m("W/\"abd\"", "\"abc\""));
        // List membership and `*` keep working through the strip.
        assert!(m("\"x\", W/\"abc\"", "\"abc\""));
        assert!(m("*", "\"abc\""));
        assert!(!m("garbage,,,\"", "\"abc\""));
    }
}
