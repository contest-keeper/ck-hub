//! The HTTP side: turns requests into calls on the [`Hub`] and the Hub's answers into responses.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use ck_protocol::{HEARTBEAT_PATH, SIGNATURE_HEADER, STICK_HEADER};
use serde_json::json;

use serde::Deserialize;

use crate::accounts::{AccessError, Account, LoginError, SESSION_LIFETIME_HOURS};
use crate::hub::{Hub, Rejection, StickView};
use crate::register::{NewStick, RegisterError};
use crate::store::Role;

/// The page organizers watch. It is one file with no outside resources, so it works on a network
/// with no internet.
const PAGE: &str = include_str!("page.html");

/// Where sticks send events. The stick's agent uses the same address.
const EVENTS_PATH: &str = "/api/v1/events";

/// A whole registry of a few hundred sticks. Only the admin-only registering route allows this much.
const MAX_REGISTRY_BYTES: usize = 2 * 1024 * 1024;

/// A heartbeat is a few hundred bytes. Anything much bigger is not one.
const MAX_BODY_BYTES: usize = 64 * 1024;

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(page))
        .route(HEARTBEAT_PATH, post(heartbeat))
        .route(EVENTS_PATH, post(event))
        .route(
            "/api/v1/sticks",
            get(sticks)
                .post(register)
                .layer(DefaultBodyLimit::max(MAX_REGISTRY_BYTES)),
        )
        .route("/api/v1/login", post(login))
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/me", get(me))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(hub)
}

async fn heartbeat(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    let (stick, signature) = auth_headers(&headers);
    // The store may talk to a database, which blocks. That belongs on a thread of its own, never
    // on the threads that serve requests.
    let result = tokio::task::spawn_blocking(move || {
        hub.accept_heartbeat(stick.as_deref(), signature.as_deref(), &body)
    })
    .await;
    accepted(result)
}

async fn event(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    let (stick, signature) = auth_headers(&headers);
    let result = tokio::task::spawn_blocking(move || {
        hub.accept_event(stick.as_deref(), signature.as_deref(), &body)
    })
    .await;
    accepted(result)
}

/// The answer to a heartbeat or an event. A 409 means the Hub already has it, which a stick counts
/// as sent.
fn accepted(result: Result<Result<u64, Rejection>, tokio::task::JoinError>) -> Response {
    match result {
        Ok(Ok(seq)) => Json(json!({ "accepted": seq })).into_response(),
        Ok(Err(Rejection::BadRequest(why))) => (StatusCode::BAD_REQUEST, why).into_response(),
        Ok(Err(Rejection::Unauthorized)) => {
            (StatusCode::UNAUTHORIZED, "unknown stick or bad signature").into_response()
        }
        Ok(Err(Rejection::Conflict(why))) => (StatusCode::CONFLICT, why).into_response(),
        Ok(Err(Rejection::Unavailable)) | Err(_) => unavailable(),
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn sticks(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Err(reason) = require(&hub, &headers, Role::Viewer).await {
        return denied(reason);
    }
    match tokio::task::spawn_blocking(move || hub.sticks()).await {
        Ok(Ok(sticks)) => Json::<Vec<StickView>>(sticks).into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

/// The stick keeps what it could not send and tries again later.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "the Hub cannot keep or read its data right now",
    )
        .into_response()
}

/// The stick id and the signature, as owned text so they can move to the blocking thread.
fn auth_headers(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    (get(STICK_HEADER), get(SIGNATURE_HEADER))
}

/// The name of the cookie that holds the session token.
const SESSION_COOKIE: &str = "ck_session";

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(State(hub): State<Arc<Hub>>, Json(body): Json<LoginBody>) -> Response {
    // Hashing a password is slow on purpose, so it runs off the request threads.
    let result =
        tokio::task::spawn_blocking(move || hub.login(&body.username, &body.password)).await;
    match result {
        Ok(Ok((token, account))) => (
            [(
                SET_COOKIE,
                session_cookie(&token, SESSION_LIFETIME_HOURS * 3600),
            )],
            Json(account_json(&account)),
        )
            .into_response(),
        Ok(Err(LoginError::BadCredentials)) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "wrong name or password" })),
        )
            .into_response(),
        Ok(Err(LoginError::TooManyAttempts)) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "too many wrong passwords, wait a few minutes" })),
        )
            .into_response(),
        Ok(Err(LoginError::Unavailable)) | Err(_) => unavailable(),
    }
}

async fn logout(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        let _ = tokio::task::spawn_blocking(move || hub.logout(&token)).await;
    }
    // The browser is told to forget the cookie whether or not there was a session.
    (
        [(SET_COOKIE, session_cookie("", 0))],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn me(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    match require(&hub, &headers, Role::Viewer).await {
        Ok(account) => Json(account_json(&account)).into_response(),
        Err(reason) => denied(reason),
    }
}

fn account_json(account: &Account) -> serde_json::Value {
    json!({ "username": account.name, "role": account.role.as_str() })
}

/// The cookie that carries a session. `HttpOnly` keeps scripts from reading it, `SameSite=Strict`
/// keeps other sites from using it, and `Secure` keeps it off plain HTTP, so run the Hub behind HTTPS.
fn session_cookie(token: &str, max_age_seconds: i64) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age_seconds}"
    )
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    cookies
        .split(';')
        .filter_map(|cookie| cookie.trim().strip_prefix("ck_session="))
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Why a request was not allowed: the status to answer with and a short text.
type Denied = (StatusCode, &'static str);

fn denied(denied: Denied) -> Response {
    (denied.0, Json(json!({ "error": denied.1 }))).into_response()
}

/// The organizer behind the request, or why not: 401 when there is no valid session, 403 when the
/// role is not enough, 503 when the data cannot be read.
async fn require(hub: &Arc<Hub>, headers: &HeaderMap, needed: Role) -> Result<Account, Denied> {
    let Some(token) = session_token(headers) else {
        return Err((StatusCode::UNAUTHORIZED, "log in first"));
    };
    let hub = Arc::clone(hub);
    match tokio::task::spawn_blocking(move || hub.access(&token, needed)).await {
        Ok(Ok(account)) => Ok(account),
        Ok(Err(AccessError::Unauthorized)) => Err((StatusCode::UNAUTHORIZED, "log in first")),
        Ok(Err(AccessError::Forbidden)) => {
            Err((StatusCode::FORBIDDEN, "this needs a different role"))
        }
        Ok(Err(AccessError::Unavailable)) | Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the Hub cannot read its data right now",
        )),
    }
}

/// One stick, or a whole registry file of the admin tool (`{"sticks": [...]}`). It is read by hand
/// instead of by an untagged type, so a mistake is reported with the name of the wrong field.
fn read_sticks(body: &[u8]) -> Result<Vec<NewStick>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("the request is not valid JSON: {e}"))?;
    let invalid = |e: serde_json::Error| format!("the request is not a stick or a registry: {e}");
    match value.get("sticks") {
        Some(list) => serde_json::from_value(list.clone()).map_err(invalid),
        None => serde_json::from_value(value)
            .map(|stick| vec![stick])
            .map_err(invalid),
    }
}

async fn register(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(reason) = require(&hub, &headers, Role::Admin).await {
        return denied(reason);
    }
    let sticks = match read_sticks(&body) {
        Ok(sticks) => sticks,
        Err(why) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response();
        }
    };
    match tokio::task::spawn_blocking(move || hub.register_sticks(sticks)).await {
        Ok(Ok(done)) => {
            let answer = json!({
                "added": done.added,
                "already_known": done.already_known,
                "conflicts": done.conflicts,
            });
            // A different secret for a known stick is refused, but the rest was still registered.
            let status = if done.conflicts.is_empty() {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            (status, Json(answer)).into_response()
        }
        Ok(Err(RegisterError::Invalid(why))) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response()
        }
        Ok(Err(RegisterError::Unavailable)) | Err(_) => unavailable(),
    }
}
