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
    let result = hub.accept_heartbeat(
        header(&headers, STICK_HEADER),
        header(&headers, SIGNATURE_HEADER),
        &body,
    );
    match result {
        Ok(seq) => Json(json!({ "accepted": seq })).into_response(),
        Err(Rejection::BadRequest(why)) => (StatusCode::BAD_REQUEST, why).into_response(),
        Err(Rejection::Unauthorized) => {
            (StatusCode::UNAUTHORIZED, "unknown stick or bad signature").into_response()
        }
        Err(Rejection::Conflict(why)) => (StatusCode::CONFLICT, why).into_response(),
    }
}

async fn event(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    let result = hub.accept_event(
        header(&headers, STICK_HEADER),
        header(&headers, SIGNATURE_HEADER),
        &body,
    );
    match result {
        Ok(seq) => Json(json!({ "accepted": seq })).into_response(),
        Err(Rejection::BadRequest(why)) => (StatusCode::BAD_REQUEST, why).into_response(),
        Err(Rejection::Unauthorized) => {
            (StatusCode::UNAUTHORIZED, "unknown stick or bad signature").into_response()
        }
        // The stick treats this as "already have it" and stops sending that event.
        Err(Rejection::Conflict(why)) => (StatusCode::CONFLICT, why).into_response(),
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn sticks(State(hub): State<Arc<Hub>>) -> Json<Vec<StickView>> {
    Json(hub.sticks())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}
