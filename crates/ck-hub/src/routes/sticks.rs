//! Sticks: the list, retiring, archiving and deleting, registering, and what each stick reported.

use super::{
    Access, Arc, Bytes, Deserialize, HeaderMap, Hub, IntoResponse, Json, NewStick, Path,
    RegisterError, Response, Role, State, StatusCode, denied, json, require, require_in,
    unavailable,
};

/// A whole registry of a few hundred sticks. Only the admin-only registering route allows this much.
pub(super) const MAX_REGISTRY_BYTES: usize = 2 * 1024 * 1024;

/// The sticks of the only organization the caller belongs to. The page and the admin tool used this
/// before organizations had addresses.
pub(super) async fn sticks(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    match require(&hub, &headers, Role::Viewer).await {
        Ok(access) => list_sticks(hub, access).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn sticks_of(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => list_sticks(hub, access).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn list_sticks(hub: Arc<Hub>, access: Access) -> Response {
    match tokio::task::spawn_blocking(move || hub.sticks(&access.organization.id)).await {
        Ok(Ok(sticks)) => Json(sticks).into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

pub(super) async fn retire_credential(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match require(&hub, &headers, Role::Admin).await {
        Ok(access) => retire_credential_of(hub, access, id).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn retire_credential_in(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => retire_credential_of(hub, access, id).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn retire_credential_of(hub: Arc<Hub>, access: Access, id: String) -> Response {
    match tokio::task::spawn_blocking(move || hub.retire_stick(&access.organization.id, &id)).await
    {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => denied((StatusCode::NOT_FOUND, "no such stick")),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

/// How long an archived stick is kept before the Hub deletes it.
pub(super) const ARCHIVE_DAYS: [i64; 2] = [7, 30];

#[derive(Deserialize)]
pub(super) struct ArchiveRequest {
    days: i64,
}

pub(super) async fn archive_stick(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let access = match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => access,
        Err(reason) => return denied(reason),
    };
    let Some(days) = serde_json::from_slice::<ArchiveRequest>(&body)
        .ok()
        .map(|request| request.days)
        .filter(|days| ARCHIVE_DAYS.contains(days))
    else {
        return (StatusCode::BAD_REQUEST, "days must be 7 or 30").into_response();
    };
    stick_change(
        tokio::task::spawn_blocking(move || hub.archive_stick(&access.organization.id, &id, days))
            .await,
        "no such stick",
    )
}

pub(super) async fn restore_stick(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => stick_change(
            tokio::task::spawn_blocking(move || hub.restore_stick(&access.organization.id, &id))
                .await,
            "no such archived stick",
        ),
        Err(reason) => denied(reason),
    }
}

/// Deletes an archived stick and everything kept about it now, without waiting for its time.
pub(super) async fn delete_stick(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => stick_change(
            tokio::task::spawn_blocking(move || hub.delete_stick(&access.organization.id, &id))
                .await,
            "no such archived stick",
        ),
        Err(reason) => denied(reason),
    }
}

pub(super) fn stick_change(
    result: Result<Result<bool, crate::store::StoreError>, tokio::task::JoinError>,
    missing: &'static str,
) -> Response {
    match result {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => denied((StatusCode::NOT_FOUND, missing)),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

/// The network history of one stick of the only organization the caller belongs to.
pub(super) async fn network(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match require(&hub, &headers, Role::Viewer).await {
        Ok(access) => show_network(hub, access, id).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn network_of(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => show_network(hub, access, id).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn show_network(hub: Arc<Hub>, access: Access, id: String) -> Response {
    match tokio::task::spawn_blocking(move || hub.network_of(&access.organization.id, &id)).await {
        Ok(Ok(Some(view))) => Json(view).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "there is no such stick").into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

pub(super) async fn submissions(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match require(&hub, &headers, Role::Viewer).await {
        Ok(access) => show_submissions(hub, access, id).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn submissions_of(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => show_submissions(hub, access, id).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn show_submissions(hub: Arc<Hub>, access: Access, id: String) -> Response {
    match tokio::task::spawn_blocking(move || hub.submissions_of(&access.organization.id, &id))
        .await
    {
        Ok(Ok(Some(view))) => Json(view).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "there is no such stick").into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

/// One stick, or a whole registry file of the admin tool (`{"sticks": [...]}`). It is read by hand
/// instead of by an untagged type, so a mistake is reported with the name of the wrong field.
pub(super) fn read_sticks(body: &[u8]) -> Result<Vec<NewStick>, String> {
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

pub(super) async fn register(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require(&hub, &headers, Role::Admin).await {
        Ok(access) => register_into(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn register_in(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => register_into(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn register_into(hub: Arc<Hub>, access: Access, body: Bytes) -> Response {
    let sticks = match read_sticks(&body) {
        Ok(sticks) => sticks,
        Err(why) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response();
        }
    };
    match tokio::task::spawn_blocking(move || hub.register_sticks(&access.organization.id, sticks))
        .await
    {
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
