//! The HTTP side: turns requests into calls on the [`Hub`] and the Hub's answers into responses.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use ck_protocol::{HEARTBEAT_PATH, SIGNATURE_HEADER, STICK_HEADER};
use serde_json::json;

use crate::hub::{Hub, Rejection, StickView};

/// The page organizers watch. It is one file with no outside resources, so it works on a network
/// with no internet.
const PAGE: &str = include_str!("page.html");

/// Where sticks send events. The stick's agent uses the same address.
const EVENTS_PATH: &str = "/api/v1/events";

/// A heartbeat is a few hundred bytes. Anything much bigger is not one.
const MAX_BODY_BYTES: usize = 64 * 1024;

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(page))
        .route(HEARTBEAT_PATH, post(heartbeat))
        .route(EVENTS_PATH, post(event))
        .route("/api/v1/sticks", get(sticks))
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

async fn sticks(State(hub): State<Arc<Hub>>) -> Response {
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
