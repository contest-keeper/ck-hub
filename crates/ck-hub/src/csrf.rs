//! Cross-site request forgery guard for the routes a browser session can reach.
//!
//! The session cookie is `SameSite=Strict`, which already keeps other sites from using it. This is
//! the second line, for a sibling site on the same registrable domain and for browsers that ignore
//! the attribute: a state-changing request must come from the Hub's own origin, and one that
//! carries the session cookie must say it is JSON (or raw bytes for a chunk), which a plain HTML
//! form cannot send.

use axum::body::Body;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, ORIGIN, TRANSFER_ENCODING};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub async fn guard(request: Request<Body>, next: Next) -> Response {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }
    let headers = request.headers();
    if !same_origin(headers) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    if carries_cookie(headers) && has_body(headers) && !is_allowed_content_type(headers) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "send application/json").into_response();
    }
    next.run(request).await
}

/// No `Origin` is a client that is not a browser (a stick, Keeper Forge, curl). A browser always
/// sends one on a state-changing request, and it must name this server.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN) else {
        return true;
    };
    let (Some(origin), Some(host)) = (
        origin.to_str().ok(),
        headers.get(HOST).and_then(|host| host.to_str().ok()),
    ) else {
        return false;
    };
    origin
        .split_once("://")
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

fn carries_cookie(headers: &HeaderMap) -> bool {
    headers.contains_key(COOKIE)
}

fn has_body(headers: &HeaderMap) -> bool {
    headers.contains_key(TRANSFER_ENCODING)
        || headers
            .get(CONTENT_LENGTH)
            .and_then(|length| length.to_str().ok())
            .is_some_and(|length| length.trim() != "0")
}

fn is_allowed_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let essence = value.split(';').next().unwrap_or("").trim();
    essence.eq_ignore_ascii_case("application/json")
        || essence.eq_ignore_ascii_case("application/octet-stream")
}
