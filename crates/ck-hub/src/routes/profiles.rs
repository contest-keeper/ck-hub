//! Keeper Forge contest profiles.

use super::{
    Arc, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, HeaderMap, Hub, IntoResponse, Json,
    MAX_SETTINGS_BYTES, MAX_WALLPAPER_BYTES, Path, ProfileError, ProfileInput, Response, Role,
    State, StatusCode, content_type, denied, json, require, rfc3339, unavailable,
};

/// A profile with a wallpaper at the limit: the picture in base64 (a third larger) and the settings.
pub(super) const MAX_PROFILE_BODY_BYTES: usize =
    MAX_WALLPAPER_BYTES.div_ceil(3) * 4 + MAX_SETTINGS_BYTES + 4096;

pub(super) fn no_such_profile() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such profile" })),
    )
        .into_response()
}

/// Runs a read or write of the profiles of the caller's organization. A profile of another
/// organization is not there for the caller, so it answers like one that does not exist.
pub(super) async fn with_profiles<T: Send + 'static>(
    hub: Arc<Hub>,
    headers: &HeaderMap,
    work: impl FnOnce(&Hub, &str) -> Result<T, crate::store::StoreError> + Send + 'static,
) -> Result<T, Box<Response>> {
    let access = require(&hub, headers, Role::Admin)
        .await
        .map_err(|reason| Box::new(denied(reason)))?;
    match tokio::task::spawn_blocking(move || work(&hub, &access.organization.id)).await {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            Err(Box::new(unavailable()))
        }
        Err(_) => Err(Box::new(unavailable())),
    }
}

pub(super) async fn list_profiles(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    match with_profiles(hub, &headers, Hub::profiles).await {
        Ok(profiles) => Json(
            profiles
                .into_iter()
                .map(|p| json!({ "id": p.id, "name": p.name, "updated_at": rfc3339(p.updated_at) }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(response) => *response,
    }
}

pub(super) async fn read_profile(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match with_profiles(hub, &headers, move |hub, org| hub.profile(org, &id)).await {
        Ok(Some(p)) => Json(json!({
            "id": p.id,
            "name": p.name,
            "settings": p.settings,
            "wallpaper": p.wallpaper_name.map(|name| json!({ "name": name })),
            "updated_at": rfc3339(p.updated_at),
        }))
        .into_response(),
        Ok(None) => no_such_profile(),
        Err(response) => *response,
    }
}

pub(super) async fn save_profile(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    // The session is checked before the body is read, so a stranger cannot make the Hub buffer
    // megabytes.
    let (parts, body) = request.into_parts();
    let access = match require(&hub, &parts.headers, Role::Admin).await {
        Ok(access) => access,
        Err(reason) => return denied(reason),
    };
    let Ok(body) = axum::body::to_bytes(body, MAX_PROFILE_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let input: ProfileInput = match serde_json::from_slice(&body) {
        Ok(input) => input,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("the request is not a profile: {e}") })),
            )
                .into_response();
        }
    };
    let result =
        tokio::task::spawn_blocking(move || hub.put_profile(&access.organization.id, &id, input))
            .await;
    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(ProfileError::Invalid(why))) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response()
        }
        Ok(Err(ProfileError::TooLarge(why))) => {
            (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({ "error": why }))).into_response()
        }
        Ok(Err(ProfileError::Unavailable)) | Err(_) => unavailable(),
    }
}

pub(super) async fn profile_wallpaper(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match with_profiles(hub, &headers, move |hub, org| {
        hub.profile_wallpaper(org, &id)
    })
    .await
    {
        Ok(Some(wallpaper)) => {
            let Some(kind) = content_type(&wallpaper.name) else {
                return unavailable();
            };
            (
                [
                    (CONTENT_TYPE, kind),
                    (CONTENT_DISPOSITION, "attachment"),
                    (CACHE_CONTROL, "private, no-store"),
                ],
                wallpaper.data,
            )
                .into_response()
        }
        Ok(None) => no_such_profile(),
        Err(response) => *response,
    }
}

pub(super) async fn remove_profile(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match with_profiles(hub, &headers, move |hub, org| hub.delete_profile(org, &id)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => no_such_profile(),
        Err(response) => *response,
    }
}
