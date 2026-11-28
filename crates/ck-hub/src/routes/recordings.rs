//! Recordings: the upload protocol sticks use and the links organizers watch them from.

use std::io::Write as _;

use axum::body::Body;
use axum::extract::Query;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use serde::Deserialize;

use super::{
    Access, Arc, Bytes, CACHE_CONTROL, CHUNK_BYTES, HeaderMap, Hub, IntoResponse, Json, Path,
    RecordingError, Response, Role, State, StatusCode, UploadStatus, auth_headers, denied, json,
    require_in, unavailable,
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

/// What a browser needs to be handed bytes: how many bytes go to the browser before the sender
/// waits for it to catch up.
const STREAM_BUFFER_CHUNKS: usize = 4;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// A body filled by `fill` on a blocking thread, a piece at a time, so a big recording never sits
/// in memory. When the person closes the page the next write fails and `fill` stops.
fn streamed(fill: impl FnOnce(&mut ChannelWriter) -> std::io::Result<()> + Send + 'static) -> Body {
    let (sender, receiver) = tokio::sync::mpsc::channel(STREAM_BUFFER_CHUNKS);
    tokio::task::spawn_blocking(move || {
        let mut writer = ChannelWriter {
            sender,
            buffer: Vec::with_capacity(STREAM_CHUNK_BYTES),
        };
        let done = fill(&mut writer).and_then(|()| writer.flush());
        if let Err(error) = done {
            // The body ends in an error, which makes the browser drop the file instead of
            // keeping half of it as if it were whole.
            let _ = writer.sender.blocking_send(Err(error));
        }
    });
    Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(receiver))
}

struct ChannelWriter {
    sender: tokio::sync::mpsc::Sender<std::io::Result<Bytes>>,
    buffer: Vec<u8>,
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(data);
        if self.buffer.len() >= STREAM_CHUNK_BYTES {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::take(&mut self.buffer));
        self.sender
            .blocking_send(Ok(chunk))
            .map_err(|_| std::io::Error::other("the person left"))
    }
}

#[derive(Deserialize)]
pub(super) struct ViewQuery {
    /// `?download=1` saves the file instead of showing it.
    download: Option<String>,
}

/// Serves a complete recording of the organization through the Hub, so the storage address never
/// reaches the browser. The file is shown as the type the Hub chose, so the file's own claims
/// about itself never matter, and the page is locked down so a file cannot run anything.
pub(super) async fn view_recording(
    State(hub): State<Arc<Hub>>,
    Path((slug, recording_id)): Path<(String, String)>,
    Query(query): Query<ViewQuery>,
    headers: HeaderMap,
) -> Response {
    let access = match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => access,
        Err(reason) => return denied(reason),
    };
    let opened = tokio::task::spawn_blocking(move || {
        hub.open_recording(&access.organization.id, &recording_id)
    })
    .await;
    match opened {
        Ok(Ok(Some((recording, mut reader)))) => {
            let disposition = if query.download.is_some() {
                "attachment"
            } else {
                "inline"
            };
            let body = streamed(move |out| std::io::copy(&mut reader, out).map(|_| ()));
            (
                [
                    (CONTENT_TYPE, recording.kind.content_type().to_owned()),
                    (
                        CONTENT_DISPOSITION,
                        format!("{disposition}; filename=\"{}\"", recording.file),
                    ),
                    (CACHE_CONTROL, "private, no-store".to_owned()),
                    (CONTENT_SECURITY_POLICY, MEDIA_POLICY.to_owned()),
                ],
                body,
            )
                .into_response()
        }
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

/// A file opened on its own may show itself and nothing else: no script, no outside address.
const MEDIA_POLICY: &str = "default-src 'none'; img-src 'self'; media-src 'self'; sandbox";

/// All the complete recordings of a stick in one zip, written as it is sent.
pub(super) async fn download_recordings(
    State(hub): State<Arc<Hub>>,
    Path((slug, stick)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let access = match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => access,
        Err(reason) => return denied(reason),
    };
    let archive = tokio::task::spawn_blocking(move || {
        hub.recordings_archive(&access.organization.id, &stick)
    })
    .await;
    match archive {
        Ok(Ok(Some(archive))) if !archive.is_empty() => (
            [
                (CONTENT_TYPE, "application/zip".to_owned()),
                (
                    CONTENT_DISPOSITION,
                    "attachment; filename=\"recordings.zip\"".to_owned(),
                ),
                (CACHE_CONTROL, "private, no-store".to_owned()),
            ],
            streamed(move |out| archive.write_zip(out)),
        )
            .into_response(),
        Ok(Ok(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "this stick has no recordings to download" })),
        )
            .into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}
