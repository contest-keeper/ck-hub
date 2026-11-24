//! Commands an organizer queues for a stick and the stick acknowledges.

use super::{
    Access, Arc, Bytes, CommandError, Deserialize, HeaderMap, Hub, IntoResponse, Json, Path,
    Response, Role, State, StatusCode, denied, json, require_in, unavailable,
};

pub(super) async fn commands_of(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => show_commands(hub, access, id).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn show_commands(hub: Arc<Hub>, access: Access, id: String) -> Response {
    match tokio::task::spawn_blocking(move || hub.commands_of_stick(&access.organization.id, &id))
        .await
    {
        Ok(Ok(Some(list))) => Json(list).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "there is no such stick").into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

pub(super) async fn one_command(
    State(hub): State<Arc<Hub>>,
    Path((slug, command_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let access = match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => access,
        Err(reason) => return denied(reason),
    };
    match tokio::task::spawn_blocking(move || hub.command_of(&access.organization.id, &command_id))
        .await
    {
        Ok(Ok(Some(command))) => Json(command).into_response(),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such command" })),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

pub(super) async fn send_command_in(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => queue_command(hub, access, id, body).await,
        Err(reason) => denied(reason),
    }
}

#[derive(Deserialize)]
pub(super) struct NewCommandBody {
    action: ck_protocol::Action,
    #[serde(default)]
    lifetime_seconds: Option<i64>,
}

pub(super) async fn queue_command(
    hub: Arc<Hub>,
    access: Access,
    id: String,
    body: Bytes,
) -> Response {
    let bad =
        |why: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response();
    let body: NewCommandBody = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => return bad(format!("the command cannot be read: {error}")),
    };
    let result = tokio::task::spawn_blocking(move || {
        hub.send_command(
            &access.organization.id,
            &access.account,
            &id,
            body.action,
            body.lifetime_seconds,
        )
    })
    .await;
    let refuse =
        |status: StatusCode, why: String| (status, Json(json!({ "error": why }))).into_response();
    match result {
        Ok(Ok(command)) => (StatusCode::CREATED, Json(command)).into_response(),
        Ok(Err(CommandError::BadRequest(why))) => bad(why),
        Ok(Err(error @ CommandError::NotFound)) => refuse(StatusCode::NOT_FOUND, error.to_string()),
        Ok(Err(error @ CommandError::TooManyOpen)) => {
            refuse(StatusCode::CONFLICT, error.to_string())
        }
        Ok(Err(error @ CommandError::RateLimited)) => {
            refuse(StatusCode::TOO_MANY_REQUESTS, error.to_string())
        }
        Ok(Err(CommandError::Store(error))) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}
