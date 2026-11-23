//! Downloads: the Keeper Forge installer and the Keeper OS images, through short-lived storage links.

use super::{
    Arc, CACHE_CONTROL, Denied, HeaderMap, Hub, IntoResponse, Json, LOCATION, Path, Response,
    State, StatusCode, denied, denied_for, json, unavailable, with_session,
};

/// The installer's place in the private container. The forge pipeline keeps the name the same.
pub(super) const FORGE_INSTALLER: &str = "forge/keeper-forge-setup-x64.exe";

/// The Keeper Forge installer. The visitor is sent to a read-only link that works for a few
/// minutes, so the storage address is never published and a copied link soon stops working.
pub(super) async fn download_forge(State(hub): State<Arc<Hub>>) -> Response {
    let Some(download) = hub.download.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    redirect_to(download.link(FORGE_INSTALLER, (hub.clock)()))
}

/// Sends the visitor to a storage link, or answers 503 when the Hub has no key to sign one with.
pub(super) fn redirect_to(link: Result<String, crate::download::DownloadError>) -> Response {
    match link {
        Ok(link) => (
            StatusCode::FOUND,
            [(LOCATION, link), (CACHE_CONTROL, "no-store".to_owned())],
        )
            .into_response(),
        Err(_) => unavailable(),
    }
}

/// The images an organizer may download. Each one has a Hub address to ask for a short-lived link;
/// the place in storage is never shown.
pub(super) async fn list_images(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    let Some(source) = hub.images.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let result = with_session(hub, &headers, move |hub, token| {
        hub.account(token)
            .map_err(|reason| denied_for(&reason))
            .and_then(|_| source.list().map_err(|_| image_list_unavailable()))
    })
    .await;
    match result {
        Err(reason) | Ok(Err(reason)) => denied(reason),
        Ok(Ok(images)) => Json(json!({
            "images": images
                .iter()
                .map(|image| {
                    let mut entry = json!({
                        "id": image.id,
                        "name": image.name,
                        "version": image.version,
                        "url": format!("/download/image/{}", image.id),
                        "size": image.size,
                        "sha256": image.sha256,
                        "family": image.family,
                        "profile": image.profile,
                        "source_offer_url": image.source_offer.as_ref()
                            .map(|_| format!("/download/image/{}/source-offer", image.id)),
                    });
                    if let Some(filename) = &image.filename {
                        entry["filename"] = json!(filename);
                    }
                    entry
                })
                .collect::<Vec<_>>()
        }))
        .into_response(),
    }
}

pub(super) fn image_list_unavailable() -> Denied {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "the image list is not available right now",
    )
}

/// The written source offer of one image, for whoever may download that image.
pub(super) async fn download_source_offer(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (Some(source), Some(_)) = (hub.images.clone(), hub.download.as_ref()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let wanted = id;
    let result = with_session(Arc::clone(&hub), &headers, move |hub, token| {
        hub.account(token)
            .map_err(|reason| denied_for(&reason))
            .and_then(|_| source.list().map_err(|_| image_list_unavailable()))
            .map(|images| {
                images
                    .into_iter()
                    .find(|image| image.id == wanted)
                    .and_then(|image| image.source_offer)
            })
    })
    .await;
    match result {
        Err(reason) | Ok(Err(reason)) => denied(reason),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no source offer for this image" })),
        )
            .into_response(),
        Ok(Ok(Some(blob))) => {
            let download = hub.download.as_ref().expect("checked above");
            redirect_to(download.link(&blob, (hub.clock)()))
        }
    }
}

/// One image. Only a logged-in organizer is sent on, to a read-only link that works for a few
/// minutes. A browser can follow it because the session cookie travels with the request.
pub(super) async fn download_image(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (Some(source), Some(_)) = (hub.images.clone(), hub.download.as_ref()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let wanted = id;
    let result = with_session(Arc::clone(&hub), &headers, move |hub, token| {
        hub.account(token)
            .map_err(|reason| denied_for(&reason))
            .and_then(|_| source.list().map_err(|_| image_list_unavailable()))
            .map(|images| images.into_iter().find(|image| image.id == wanted))
    })
    .await;
    match result {
        Err(reason) | Ok(Err(reason)) => denied(reason),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such image" })),
        )
            .into_response(),
        Ok(Ok(Some(image))) => {
            let download = hub.download.as_ref().expect("checked above");
            redirect_to(download.link(&image.blob, (hub.clock)()))
        }
    }
}
