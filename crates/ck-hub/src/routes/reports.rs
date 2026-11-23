//! What a stick sends: heartbeats and events, signed with its own secret.

use super::{
    Arc, Bytes, HeaderMap, Hub, IntoResponse, Json, Rejection, Response, State, StatusCode,
    auth_headers, json, unavailable,
};

pub(super) async fn heartbeat(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (stick, signature) = auth_headers(&headers);
    // The store may talk to a database, which blocks. That belongs on a thread of its own, never
    // on the threads that serve requests.
    let result = tokio::task::spawn_blocking(move || {
        hub.accept_heartbeat_reply(stick.as_deref(), signature.as_deref(), &body)
    })
    .await;
    // The commands waiting for the stick ride in the answer; with none the answer is as before.
    match result {
        Ok(Ok((seq, commands))) if !commands.is_empty() => {
            Json(json!({ "accepted": seq, "commands": commands })).into_response()
        }
        other => accepted(other.map(|answer| answer.map(|(seq, _)| seq))),
    }
}

pub(super) async fn event(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (stick, signature) = auth_headers(&headers);
    let result = tokio::task::spawn_blocking(move || {
        hub.accept_event(stick.as_deref(), signature.as_deref(), &body)
    })
    .await;
    accepted(result)
}

/// The answer to a heartbeat or an event. A 409 means the Hub already has it, which a stick counts
/// as sent.
pub(super) fn accepted(result: Result<Result<u64, Rejection>, tokio::task::JoinError>) -> Response {
    match result {
        Ok(Ok(seq)) => Json(json!({ "accepted": seq })).into_response(),
        Ok(Err(Rejection::BadRequest(why))) => (StatusCode::BAD_REQUEST, why).into_response(),
        Ok(Err(Rejection::Unauthorized)) => {
            (StatusCode::UNAUTHORIZED, "unknown stick or bad signature").into_response()
        }
        Ok(Err(Rejection::Conflict(why))) => (StatusCode::CONFLICT, why).into_response(),
        Ok(Err(Rejection::TooManyRequests)) => {
            (StatusCode::TOO_MANY_REQUESTS, "too many messages").into_response()
        }
        Ok(Err(Rejection::Unavailable)) | Err(_) => unavailable(),
    }
}
