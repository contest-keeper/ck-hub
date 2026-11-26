//! Recordings: the upload protocol sticks use and the links organizers watch them from.

use super::{
    Access, Arc, Bytes, CACHE_CONTROL, CHUNK_BYTES, HeaderMap, Hub, IntoResponse, Json, LOCATION,
    Path, RecordingError, Response, Role, State, StatusCode, UploadStatus, auth_headers, denied,
    json, require_in, unavailable,
};

/// Chunks of recordings in flight at once, in the whole Hub. Each one is held in memory while it is
/// checked and written, and the container has 0.5 GiB.
pub(super) static CHUNKS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

/// How long one chunk's body may take to arrive once its slot is held.
pub(super) const BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Header that carries the SHA-256 of a chunk.
pub(super) const CHUNK_SHA_HEADER: &str = "x-ck-chunk-sha256";

pub(super) async fn start_upload(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (stick, signature) = auth_headers(&headers);
    let result = tokio::task::spawn_blocking(move || {
        hub.start_upload(stick.as_deref(), signature.as_deref(), &body)
    })
    .await;
    upload_answer(result)
}

pub(super) async fn upload_status(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (stick, signature) = auth_headers(&headers);
    let result = tokio::task::spawn_blocking(move || {
        hub.upload_status(stick.as_deref(), signature.as_deref(), &id)
    })
    .await;
    upload_answer(result)
}

/// One chunk of a recording. The stick is checked before the body is read, and only a few chunks
/// are held in memory at a time: a body nobody signed for is never buffered.
pub(super) async fn put_chunk(
    State(hub): State<Arc<Hub>>,
    Path((id, index)): Path<(String, u32)>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let (stick, signature) = auth_headers(&parts.headers);
    let sha = parts
        .headers
        .get(CHUNK_SHA_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let Some(sha) = sha else {
        return upload_answer(Ok(Err(RecordingError::BadRequest(
            "the x-ck-chunk-sha256 header is missing",
        ))));
    };
    let known = {
        let (hub, stick, signature) = (Arc::clone(&hub), stick.clone(), signature.clone());
        let (id, sha) = (id.clone(), sha.clone());
        tokio::task::spawn_blocking(move || {
            hub.check_chunk_sender(stick.as_deref(), signature.as_deref(), &id, index, &sha)
        })
        .await
    };
    match known {
        Ok(Ok(())) => {}
        Ok(Err(refusal)) => return upload_answer(Ok(Err(refusal))),
        Err(failed) => return upload_answer(Err(failed)),
    }
    let Ok(_slot) = CHUNKS.acquire().await else {
        return unavailable();
    };
    // A stalled body must not keep a slot for ever: 8 slow signed uploads would block all recordings.
    let read =
        tokio::time::timeout(BODY_TIMEOUT, axum::body::to_bytes(body, CHUNK_BYTES + 1)).await;
    let body = match read {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => return upload_answer(Ok(Err(RecordingError::TooBig))),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let result = tokio::task::spawn_blocking(move || {
        hub.put_chunk(
            stick.as_deref(),
            signature.as_deref(),
            &id,
            index,
            &sha,
            &body,
        )
    })
    .await;
    upload_answer(result)
}

pub(super) async fn complete_upload(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (stick, signature) = auth_headers(&headers);
    let result = tokio::task::spawn_blocking(move || {
        hub.complete_upload(stick.as_deref(), signature.as_deref(), &id)
    })
    .await;
    upload_answer(result)
}

pub(super) fn upload_answer(
    result: Result<Result<UploadStatus, RecordingError>, tokio::task::JoinError>,
) -> Response {
    match result {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(RecordingError::NotFound)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(RecordingError::BadRequest(why))) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response()
        }
        Ok(Err(RecordingError::Unauthorized)) => {
            (StatusCode::UNAUTHORIZED, "unknown stick or bad signature").into_response()
        }
        Ok(Err(RecordingError::Conflict {
            message,
            next_chunk,
        })) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": message, "next_chunk": next_chunk })),
        )
            .into_response(),
        Ok(Err(RecordingError::TooBig)) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Ok(Err(RecordingError::TooMany)) => {
            (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response()
        }
        Ok(Err(RecordingError::Unavailable)) | Err(_) => unavailable(),
    }
}

pub(super) async fn recordings_of(
    State(hub): State<Arc<Hub>>,
    Path((slug, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => show_recordings(hub, access, id).await,
        Err(reason) => denied(reason),
    }
}

pub(super) async fn show_recordings(hub: Arc<Hub>, access: Access, id: String) -> Response {
    match tokio::task::spawn_blocking(move || hub.recordings_of_stick(&access.organization.id, &id))
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

/// Sends a member of the organization to a read-only link to a complete recording. The link works
/// for a couple of hours and shows the file as the type the Hub chose, so the file's own claims
/// about itself never matter. A browser follows it in its own tab, so the Hub page needs no more
/// permission than it has.
pub(super) async fn view_recording(
    State(hub): State<Arc<Hub>>,
    Path((slug, recording_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let access = match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => access,
        Err(reason) => return denied(reason),
    };
    match tokio::task::spawn_blocking(move || {
        hub.recording_link(&access.organization.id, &recording_id)
    })
    .await
    {
        Ok(Ok(Some(link))) => (
            StatusCode::FOUND,
            [(LOCATION, link), (CACHE_CONTROL, "no-store".to_owned())],
        )
            .into_response(),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such recording" })),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}
