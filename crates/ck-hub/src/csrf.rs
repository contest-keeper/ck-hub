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
    if !same_origin(&request) {
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    let headers = request.headers();
    if carries_cookie(headers) && !content_type_is_acceptable(headers) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "send application/json").into_response();
    }
    next.run(request).await
}

/// No `Origin` is a client that is not a browser (a stick, Keeper Forge, curl). A browser always
/// sends one on a state-changing request, and it must name this server: the `Host` header, the
/// authority of an HTTP/2 request (which has no `Host` header), or the host a proxy forwarded. A
/// page on another site cannot set `X-Forwarded-Host` without a preflight the Hub never allows.
fn same_origin(request: &Request<Body>) -> bool {
    let headers = request.headers();
    let Some(origin) = headers.get(ORIGIN) else {
        return true;
    };
    let Some(authority) = origin
        .to_str()
        .ok()
        .and_then(|origin| origin.split_once("://"))
        .map(|(_, authority)| authority)
    else {
        return false;
    };
    let forwarded = headers
        .get("x-forwarded-host")
        .and_then(|host| host.to_str().ok())
        .and_then(|hosts| hosts.split(',').next());
    [
        headers.get(HOST).and_then(|host| host.to_str().ok()),
        request
            .uri()
            .authority()
            .map(axum::http::uri::Authority::as_str),
        forwarded,
    ]
    .into_iter()
    .flatten()
    .any(|host| host.trim().eq_ignore_ascii_case(authority))
}

fn carries_cookie(headers: &HeaderMap) -> bool {
    headers.contains_key(COOKIE)
}

/// A cookie request may name only JSON or raw bytes. Naming nothing is allowed only without a body
/// (a bodyless POST or DELETE); some HTTP/2 bodies carry no length, so a missing content type
/// is judged by the length only when there is one.
fn content_type_is_acceptable(headers: &HeaderMap) -> bool {
    match headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        Some(value) => {
            let essence = value.split(';').next().unwrap_or("").trim();
            essence.eq_ignore_ascii_case("application/json")
                || essence.eq_ignore_ascii_case("application/octet-stream")
        }
        None => {
            !headers.contains_key(TRANSFER_ENCODING)
                && headers
                    .get(CONTENT_LENGTH)
                    .and_then(|length| length.to_str().ok())
                    .is_none_or(|length| length.trim() == "0")
        }
    }
}
