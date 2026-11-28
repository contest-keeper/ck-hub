//! Recordings a stick uploads: screen video segments and snapshots. The stick sends a file in
//! numbered chunks; the Hub writes them to a private blob container and keeps a row that says how
//! far an upload got, so it can resume. The bytes are opaque: the Hub never reads, decodes or runs
//! them. See `docs/superpowers/specs/2026-11-18-recordings-design.md`.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ck_protocol::StickId;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::download::{Download, encode};
use crate::hub::Hub;
use crate::store::StoreError;

/// The size of every chunk but the last of a file.
pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// The biggest file the Hub takes.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Uploads of one stick that are started and not complete.
pub const MAX_OPEN_PER_STICK: usize = 3;

/// All the recordings of one stick together.
pub const MAX_STICK_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Chunks one stick may send in a minute. Heartbeats and events have a budget of their own.
pub const MAX_CHUNKS_PER_MINUTE: u32 = 600;

/// How long a complete recording is kept when the Hub is not told otherwise.
pub const DEFAULT_RETENTION_DAYS: i64 = 30;

/// The longest retention a Hub may be set to.
pub const MAX_RETENTION_DAYS: i64 = 365;

/// An upload that was started and never completed is removed after this long.
pub const ABANDONED_DAYS: i64 = 3;

/// Recordings listed for one stick.
pub const LIST_LIMIT: u32 = 500;

/// What a recording is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A segment of screen video (`.mkv`).
    Video,
    /// One picture of the screen (`.jpg`).
    Snapshot,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Snapshot => "snapshot",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "video" => Some(Self::Video),
            "snapshot" => Some(Self::Snapshot),
            _ => None,
        }
    }

    /// The type the file is always served as, whatever the stick said.
    #[must_use]
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Video => "video/x-matroska",
            Self::Snapshot => "image/jpeg",
        }
    }
}

/// A recording as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub id: String,
    pub stick_id: String,
    /// The stick's own name for the file.
    pub file: String,
    pub kind: Kind,
    pub size: u64,
    pub sha256: String,
    pub started_at: Option<OffsetDateTime>,
    pub duration_s: Option<u64>,
    /// How many chunks the blob store has accepted.
    pub next_chunk: u32,
    pub created_at: OffsetDateTime,
    pub completed_at: Option<OffsetDateTime>,
    pub expires_at: Option<OffsetDateTime>,
}

impl Recording {
    /// How many chunks the whole file is cut into.
    #[must_use]
    pub fn chunk_count(&self) -> u32 {
        u32::try_from(self.size.div_ceil(CHUNK_BYTES as u64)).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.completed_at.is_some()
    }

    /// Where the bytes are kept, inside the recordings container.
    #[must_use]
    pub fn blob(&self, organization: &str) -> String {
        format!("{organization}/{}/{}", self.stick_id, self.id)
    }
}

/// What an organizer sees of a recording.
#[derive(Debug, Clone, Serialize)]
pub struct RecordingView {
    pub id: String,
    pub file: String,
    pub kind: Kind,
    pub size: u64,
    pub sha256: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    pub duration_s: Option<u64>,
    pub complete: bool,
    /// Chunks received so far, for an upload still going.
    pub chunks_received: u32,
    pub chunks_total: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

impl From<&Recording> for RecordingView {
    fn from(r: &Recording) -> Self {
        Self {
            id: r.id.clone(),
            file: r.file.clone(),
            kind: r.kind,
            size: r.size,
            sha256: r.sha256.clone(),
            started_at: r.started_at,
            duration_s: r.duration_s,
            complete: r.is_complete(),
            chunks_received: r.next_chunk,
            chunks_total: r.chunk_count(),
            created_at: r.created_at,
            completed_at: r.completed_at,
            expires_at: r.expires_at,
        }
    }
}

/// What a stick gets back from the upload calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UploadStatus {
    pub id: String,
    pub chunk_size: usize,
    pub next_chunk: u32,
    pub complete: bool,
}

impl UploadStatus {
    fn of(r: &Recording) -> Self {
        Self {
            id: r.id.clone(),
            chunk_size: CHUNK_BYTES,
            next_chunk: r.next_chunk,
            complete: r.is_complete(),
        }
    }
}

/// The body of the call that starts an upload.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    file: String,
    kind: String,
    size: u64,
    sha256: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    started_at: Option<OffsetDateTime>,
    #[serde(default)]
    duration_s: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    /// Uploads are not set up on this Hub, or there is no such recording.
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(&'static str),
    #[error("unknown stick or bad signature")]
    Unauthorized,
    /// Out of order or incomplete; the stick resumes at `next_chunk`.
    #[error("{message}")]
    Conflict {
        message: &'static str,
        next_chunk: u32,
    },
    #[error("too much data")]
    TooBig,
    #[error("too many requests")]
    TooMany,
    #[error("the storage cannot be reached right now")]
    Unavailable,
}

impl From<StoreError> for RecordingError {
    fn from(error: StoreError) -> Self {
        tracing::error!("{error}");
        Self::Unavailable
    }
}

/// Why the blob store did not do what was asked.
#[derive(Debug, thiserror::Error)]
#[error("the blob store failed")]
pub struct BlobError;

/// Where the bytes of recordings are kept. Every method blocks, so callers run on a blocking thread.
pub trait Blobs: Send + Sync {
    /// Stores one block of `blob`. The same block id again replaces the block.
    ///
    /// # Errors
    /// [`BlobError`] when the store did not take it.
    fn put_block(&self, blob: &str, block_id: &str, bytes: &[u8]) -> Result<(), BlobError>;

    /// Makes `blob` out of the blocks, in this order.
    ///
    /// # Errors
    /// [`BlobError`] when the store did not take it.
    fn commit(&self, blob: &str, block_ids: &[String], content_type: &str)
    -> Result<(), BlobError>;

    /// Removes `blob` and its blocks. A blob that is already gone is not an error.
    ///
    /// # Errors
    /// [`BlobError`] when the store did not do it.
    fn delete(&self, blob: &str) -> Result<(), BlobError>;

    /// The bytes of `blob`, read as they are sent on: the Hub serves recordings itself, so no
    /// storage address ever reaches a browser.
    ///
    /// # Errors
    /// [`BlobError`] when the store has no such blob or cannot be reached.
    fn open(&self, blob: &str) -> Result<Box<dyn Read + Send>, BlobError>;
}

/// The id of block `index`: all ids of a blob must have the same length.
#[must_use]
pub fn block_id(index: u32) -> String {
    STANDARD.encode(format!("{index:08}"))
}

/// Azure blob storage, reached with short-lived links that carry only the permission needed.
pub struct AzureBlobs {
    download: Download,
    agent: ureq::Agent,
}

/// The storage REST version the links are signed for; sent with every call.
use crate::delegation::STORAGE_VERSION;

/// A link for one call lives this long: enough for a slow 4 MiB chunk.
const CALL_LINK_MINUTES: i64 = 10;

impl AzureBlobs {
    /// `download` names the recordings container.
    #[must_use]
    pub fn new(download: Download) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(StdDuration::from_secs(120)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { download, agent }
    }

    fn call_link(&self, blob: &str, permissions: &str) -> Result<String, BlobError> {
        self.download
            .sas(
                blob,
                permissions,
                Duration::minutes(CALL_LINK_MINUTES),
                None,
                OffsetDateTime::now_utc(),
            )
            .map_err(|_| BlobError)
    }
}

impl Blobs for AzureBlobs {
    fn put_block(&self, blob: &str, block_id: &str, bytes: &[u8]) -> Result<(), BlobError> {
        // The link carries a signature, so it is never logged or put in an error.
        let link = format!(
            "{}&comp=block&blockid={}",
            self.call_link(blob, "cw")?,
            encode(block_id)
        );
        let response = self
            .agent
            .put(&link)
            .header("x-ms-version", STORAGE_VERSION)
            .send(bytes)
            .map_err(|_| BlobError)?;
        response
            .status()
            .is_success()
            .then_some(())
            .ok_or(BlobError)
    }

    fn commit(
        &self,
        blob: &str,
        block_ids: &[String],
        content_type: &str,
    ) -> Result<(), BlobError> {
        let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for id in block_ids {
            body.push_str("<Latest>");
            body.push_str(id);
            body.push_str("</Latest>");
        }
        body.push_str("</BlockList>");
        let link = format!("{}&comp=blocklist", self.call_link(blob, "cw")?);
        let response = self
            .agent
            .put(&link)
            .header("x-ms-version", STORAGE_VERSION)
            .header("x-ms-blob-content-type", content_type)
            .send(body.as_bytes())
            .map_err(|_| BlobError)?;
        response
            .status()
            .is_success()
            .then_some(())
            .ok_or(BlobError)
    }

    fn delete(&self, blob: &str) -> Result<(), BlobError> {
        let link = self.call_link(blob, "d")?;
        let response = self
            .agent
            .delete(&link)
            .header("x-ms-version", STORAGE_VERSION)
            .call()
            .map_err(|_| BlobError)?;
        let status = response.status();
        // A blob that was never committed, or is already gone, is what clean-up wants anyway.
        (status.is_success() || status.as_u16() == 404)
            .then_some(())
            .ok_or(BlobError)
    }

    fn open(&self, blob: &str) -> Result<Box<dyn Read + Send>, BlobError> {
        let link = self.call_link(blob, "r")?;
        let response = self
            .agent
            .get(&link)
            .header("x-ms-version", STORAGE_VERSION)
            .call()
            .map_err(|_| BlobError)?;
        if !response.status().is_success() {
            return Err(BlobError);
        }
        Ok(Box::new(response.into_body().into_reader()))
    }
}

/// Keeps blobs in memory, for tests and for a Hub with no storage account.
#[derive(Default)]
pub struct MemoryBlobs {
    staged: Mutex<HashMap<(String, String), Vec<u8>>>,
    committed: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryBlobs {
    /// The bytes of a committed blob.
    #[must_use]
    pub fn get(&self, blob: &str) -> Option<Vec<u8>> {
        self.committed
            .lock()
            .expect("blobs lock")
            .get(blob)
            .cloned()
    }

    /// The names of all committed blobs, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .committed
            .lock()
            .expect("blobs lock")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }
}

impl Blobs for MemoryBlobs {
    fn put_block(&self, blob: &str, block_id: &str, bytes: &[u8]) -> Result<(), BlobError> {
        self.staged
            .lock()
            .expect("blobs lock")
            .insert((blob.to_owned(), block_id.to_owned()), bytes.to_vec());
        Ok(())
    }

    fn commit(
        &self,
        blob: &str,
        block_ids: &[String],
        _content_type: &str,
    ) -> Result<(), BlobError> {
        let mut staged = self.staged.lock().expect("blobs lock");
        let mut whole = Vec::new();
        for id in block_ids {
            let part = staged
                .get(&(blob.to_owned(), id.clone()))
                .ok_or(BlobError)?;
            whole.extend_from_slice(part);
        }
        staged.retain(|(name, _), _| name != blob);
        self.committed
            .lock()
            .expect("blobs lock")
            .insert(blob.to_owned(), whole);
        Ok(())
    }

    fn delete(&self, blob: &str) -> Result<(), BlobError> {
        self.staged
            .lock()
            .expect("blobs lock")
            .retain(|(name, _), _| name != blob);
        self.committed.lock().expect("blobs lock").remove(blob);
        Ok(())
    }

    fn open(&self, blob: &str) -> Result<Box<dyn Read + Send>, BlobError> {
        let bytes = self.get(blob).ok_or(BlobError)?;
        Ok(Box::new(std::io::Cursor::new(bytes)))
    }
}

fn valid_file_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn valid_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A recording id: 32 hex digits from a fresh random secret.
fn new_recording_id() -> String {
    let secret = ck_crypto::generate_stick_token();
    ck_crypto::sha256_hex(secret.expose().as_bytes())[..32].to_owned()
}

/// The text a stick signs for a call that has no JSON body.
fn signed_text(call: &str, parts: &[&str]) -> String {
    let mut text = format!("ck-recording-{call}");
    for part in parts {
        text.push('\n');
        text.push_str(part);
    }
    text
}

impl Hub {
    /// Lets sticks upload recordings, stored in `blobs`. Complete recordings are kept for
    /// `retention_days` (1 to a year).
    #[must_use]
    pub fn with_recordings(mut self, blobs: Arc<dyn Blobs>, retention_days: i64) -> Self {
        self.recordings = Some((blobs, retention_days.clamp(1, MAX_RETENTION_DAYS)));
        self
    }

    fn blobs(&self) -> Result<(&Arc<dyn Blobs>, i64), RecordingError> {
        self.recordings
            .as_ref()
            .map(|(blobs, days)| (blobs, *days))
            .ok_or(RecordingError::NotFound)
    }

    /// The stick that signed `signed`, and its organization.
    fn stick_for_upload(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        signed: &[u8],
    ) -> Result<(String, StickId), RecordingError> {
        self.authenticate(stick_header, signature, signed, Duration::hours(24))
            .map(|(organization, stick, _)| (organization, stick))
            .map_err(|_| RecordingError::Unauthorized)
    }

    /// A chunk counts against the stick's own minute, not the one heartbeats and events share.
    fn chunk_rate_limit(&self, id: &StickId) -> Result<(), RecordingError> {
        let now = (self.clock)();
        let mut rates = self.chunk_rates.lock().expect("chunk rates lock");
        let (since, count) = rates.entry(id.clone()).or_insert((now, 0));
        if (now - *since).whole_seconds() >= 60 {
            (*since, *count) = (now, 0);
        }
        *count += 1;
        if *count > MAX_CHUNKS_PER_MINUTE {
            return Err(RecordingError::TooMany);
        }
        Ok(())
    }

    /// The recording of this stick, or a plain not-found: a stick cannot tell another's recording
    /// from none.
    fn own_recording(
        &self,
        organization: &str,
        stick: &StickId,
        id: &str,
    ) -> Result<Recording, RecordingError> {
        match self.store.recording(organization, id)? {
            Some(r) if r.stick_id == stick.to_string() => Ok(r),
            _ => Err(RecordingError::NotFound),
        }
    }

    /// Starts an upload, or finds the one already started for this file of the stick.
    ///
    /// # Errors
    /// See [`RecordingError`].
    pub fn start_upload(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<UploadStatus, RecordingError> {
        self.blobs()?;
        let (organization, stick) = self.stick_for_upload(stick_header, signature, body)?;
        self.chunk_rate_limit(&stick)?;
        let request: StartRequest = serde_json::from_slice(body)
            .map_err(|_| RecordingError::BadRequest("the request cannot be read"))?;
        let kind = Kind::parse(&request.kind)
            .ok_or(RecordingError::BadRequest("the kind is video or snapshot"))?;
        if !valid_file_name(&request.file) {
            return Err(RecordingError::BadRequest("the file name is not valid"));
        }
        if !valid_sha256(&request.sha256) {
            return Err(RecordingError::BadRequest(
                "the sha256 is 64 lowercase hex digits",
            ));
        }
        if request.size == 0 {
            return Err(RecordingError::BadRequest("the file is empty"));
        }
        if request.size > MAX_FILE_BYTES {
            return Err(RecordingError::TooBig);
        }
        // PostgreSQL cannot keep it; refused here, the stick is told once instead of getting a 503
        // and retrying for ever.
        if request
            .duration_s
            .is_some_and(|s| i64::try_from(s).is_err())
        {
            return Err(RecordingError::BadRequest("the duration is not valid"));
        }
        let existing = self.store.recordings_of(&organization, &stick, 10_000)?;
        if let Some(same) = existing.iter().find(|r| r.file == request.file) {
            if same.size != request.size || same.sha256 != request.sha256 {
                return Err(RecordingError::Conflict {
                    message: "this file name was already used for other content",
                    next_chunk: same.next_chunk,
                });
            }
            return Ok(UploadStatus::of(same));
        }
        let open = existing.iter().filter(|r| !r.is_complete()).count();
        if open >= MAX_OPEN_PER_STICK {
            return Err(RecordingError::TooMany);
        }
        let kept: u64 = existing.iter().map(|r| r.size).sum();
        if kept + request.size > MAX_STICK_BYTES {
            return Err(RecordingError::TooBig);
        }
        let recording = Recording {
            id: new_recording_id(),
            stick_id: stick.to_string(),
            file: request.file.clone(),
            kind,
            size: request.size,
            sha256: request.sha256.clone(),
            started_at: request.started_at,
            duration_s: request.duration_s,
            next_chunk: 0,
            created_at: (self.clock)(),
            completed_at: None,
            expires_at: None,
        };
        if self.store.add_recording(&organization, &recording)? {
            return Ok(UploadStatus::of(&recording));
        }
        // Two starts of the same file raced; the other one won and is the upload.
        self.store
            .recordings_of(&organization, &stick, 10_000)?
            .iter()
            .find(|r| r.file == request.file)
            .map(UploadStatus::of)
            .ok_or(RecordingError::Unavailable)
    }

    /// How far an upload got, so the stick can resume.
    ///
    /// # Errors
    /// See [`RecordingError`].
    pub fn upload_status(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        id: &str,
    ) -> Result<UploadStatus, RecordingError> {
        self.blobs()?;
        let text = signed_text("status", &[id]);
        let (organization, stick) =
            self.stick_for_upload(stick_header, signature, text.as_bytes())?;
        self.chunk_rate_limit(&stick)?;
        let recording = self.own_recording(&organization, &stick, id)?;
        Ok(UploadStatus::of(&recording))
    }

    /// Checks who sends chunk `index` before its body is read.
    ///
    /// # Errors
    /// [`RecordingError::Unauthorized`] for an unknown stick or a bad signature.
    pub fn check_chunk_sender(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        id: &str,
        index: u32,
        chunk_sha256: &str,
    ) -> Result<(), RecordingError> {
        self.blobs()?;
        let text = signed_text("chunk", &[id, &index.to_string(), chunk_sha256]);
        self.stick_for_upload(stick_header, signature, text.as_bytes())
            .map(|_| ())
    }

    /// Takes chunk `index` of a recording. A chunk the Hub already has is answered as accepted
    /// without being written again.
    ///
    /// # Errors
    /// See [`RecordingError`].
    pub fn put_chunk(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        id: &str,
        index: u32,
        chunk_sha256: &str,
        bytes: &[u8],
    ) -> Result<UploadStatus, RecordingError> {
        let (blobs, _) = self.blobs()?;
        let index_text = index.to_string();
        let text = signed_text("chunk", &[id, &index_text, chunk_sha256]);
        let (organization, stick) =
            self.stick_for_upload(stick_header, signature, text.as_bytes())?;
        self.chunk_rate_limit(&stick)?;
        if bytes.len() > CHUNK_BYTES {
            return Err(RecordingError::TooBig);
        }
        if ck_crypto::sha256_hex(bytes) != chunk_sha256 {
            return Err(RecordingError::BadRequest(
                "the chunk does not match its sha256",
            ));
        }
        let recording = self.own_recording(&organization, &stick, id)?;
        if recording.is_complete() || index < recording.next_chunk {
            return Ok(UploadStatus::of(&recording));
        }
        if index > recording.next_chunk {
            return Err(RecordingError::Conflict {
                message: "a chunk is missing before this one",
                next_chunk: recording.next_chunk,
            });
        }
        // Every chunk is full except the last, so the sizes always add up to the announced size.
        let last = recording.chunk_count() - 1;
        let expected = if index == last {
            recording.size - u64::from(index) * CHUNK_BYTES as u64
        } else {
            CHUNK_BYTES as u64
        };
        if index > last || bytes.len() as u64 != expected {
            return Err(RecordingError::BadRequest(
                "the chunk is not the size the file needs here",
            ));
        }
        blobs
            .put_block(&recording.blob(&organization), &block_id(index), bytes)
            .map_err(|_| RecordingError::Unavailable)?;
        self.store.advance_recording(&organization, id, index)?;
        let after = self.own_recording(&organization, &stick, id)?;
        Ok(UploadStatus::of(&after))
    }

    /// Closes an upload once every chunk is in. It is kept for the Hub's retention period from now.
    ///
    /// # Errors
    /// See [`RecordingError`].
    pub fn complete_upload(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        id: &str,
    ) -> Result<UploadStatus, RecordingError> {
        let (blobs, days) = self.blobs()?;
        let text = signed_text("complete", &[id]);
        let (organization, stick) =
            self.stick_for_upload(stick_header, signature, text.as_bytes())?;
        self.chunk_rate_limit(&stick)?;
        let recording = self.own_recording(&organization, &stick, id)?;
        if recording.is_complete() {
            return Ok(UploadStatus::of(&recording));
        }
        let total = recording.chunk_count();
        if recording.next_chunk != total {
            return Err(RecordingError::Conflict {
                message: "chunks are missing",
                next_chunk: recording.next_chunk,
            });
        }
        let ids: Vec<String> = (0..total).map(block_id).collect();
        blobs
            .commit(
                &recording.blob(&organization),
                &ids,
                recording.kind.content_type(),
            )
            .map_err(|_| RecordingError::Unavailable)?;
        let now = (self.clock)();
        self.store
            .complete_recording(&organization, id, now, now + Duration::days(days))?;
        let after = self.own_recording(&organization, &stick, id)?;
        Ok(UploadStatus::of(&after))
    }

    /// The recordings of a stick of the organization, newest first, or `None` for an id that is not
    /// a stick id.
    ///
    /// # Errors
    /// [`StoreError`] when the list cannot be read.
    pub fn recordings_of_stick(
        &self,
        organization: &str,
        stick: &str,
    ) -> Result<Option<Vec<RecordingView>>, StoreError> {
        let Ok(id) = StickId::parse(stick) else {
            return Ok(None);
        };
        let list = self.store.recordings_of(organization, &id, LIST_LIMIT)?;
        Ok(Some(list.iter().map(RecordingView::from).collect()))
    }

    /// A complete recording of the organization, ready to be read, or `None` when there is no
    /// such complete recording.
    ///
    /// # Errors
    /// [`StoreError`] when the recording cannot be read or its bytes cannot be opened.
    pub fn open_recording(
        &self,
        organization: &str,
        id: &str,
    ) -> Result<Option<OpenedRecording>, StoreError> {
        let Some((blobs, _)) = self.recordings.as_ref() else {
            return Ok(None);
        };
        let Some(recording) = self
            .store
            .recording(organization, id)?
            .filter(Recording::is_complete)
        else {
            return Ok(None);
        };
        let reader = blobs
            .open(&recording.blob(organization))
            .map_err(|_| StoreError("the recording cannot be read from storage".to_owned()))?;
        Ok(Some((recording, reader)))
    }

    /// Everything needed to write all the complete recordings of a stick into one zip, or `None`
    /// for an id that is not a stick id or a Hub with no recordings. The zip is written by
    /// [`Archive::write_zip`], which the caller runs on a blocking thread.
    ///
    /// # Errors
    /// [`StoreError`] when the list cannot be read.
    pub fn recordings_archive(
        &self,
        organization: &str,
        stick: &str,
    ) -> Result<Option<Archive>, StoreError> {
        let (Some((blobs, _)), Ok(id)) = (self.recordings.as_ref(), StickId::parse(stick)) else {
            return Ok(None);
        };
        let list = self.store.recordings_of(organization, &id, LIST_LIMIT)?;
        let files = list
            .iter()
            .filter(|r| r.is_complete())
            .map(|r| ArchiveFile {
                name: archive_name(r),
                blob: r.blob(organization),
            })
            .collect();
        Ok(Some(Archive {
            blobs: Arc::clone(blobs),
            files,
        }))
    }

    /// Removes the recordings that have expired and the uploads that were never finished: the blob
    /// first, then the row, so a failure leaves the row for the next round. Returns how many went.
    ///
    /// # Errors
    /// [`StoreError`] when the organizations or their recordings cannot be read.
    pub fn purge_recordings(&self) -> Result<usize, StoreError> {
        let Some((blobs, _)) = self.recordings.as_ref() else {
            return Ok(0);
        };
        let now = (self.clock)();
        let mut removed = 0;
        for organization in self.store.organizations()? {
            let due = self.store.due_recordings(
                &organization.id,
                now,
                now - Duration::days(ABANDONED_DAYS),
            )?;
            for recording in due {
                if blobs.delete(&recording.blob(&organization.id)).is_err() {
                    tracing::warn!("a recording could not be removed from storage; will retry");
                    continue;
                }
                self.store
                    .delete_recording(&organization.id, &recording.id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// A complete recording and its bytes, ready to be read.
pub type OpenedRecording = (Recording, Box<dyn Read + Send>);

/// One file of a stick's zip.
struct ArchiveFile {
    name: String,
    blob: String,
}

/// The recordings of one stick, to be written as one zip.
pub struct Archive {
    blobs: Arc<dyn Blobs>,
    files: Vec<ArchiveFile>,
}

impl Archive {
    /// How many files the zip holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Writes the zip to `out` as it reads the files, one at a time, so memory stays small and the
    /// zip needs no seeking. Video and pictures are already compressed, so files are only stored.
    ///
    /// # Errors
    /// An error when `out` stops taking bytes (the person left) or a file cannot be read.
    pub fn write_zip(&self, out: impl std::io::Write) -> std::io::Result<()> {
        let mut zip = zip::ZipWriter::new_stream(out);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(true);
        for file in &self.files {
            let mut reader = self
                .blobs
                .open(&file.blob)
                .map_err(|_| std::io::Error::other("a recording could not be read"))?;
            zip.start_file(file.name.as_str(), options)
                .map_err(std::io::Error::other)?;
            std::io::copy(&mut reader, &mut zip)?;
        }
        zip.finish().map_err(std::io::Error::other)?;
        Ok(())
    }
}

/// The name of a recording inside the zip: when it was made, then the file name the stick gave
/// (which is plain letters, digits, dots, dashes and underscores), so two files never clash.
fn archive_name(recording: &Recording) -> String {
    let when = recording.started_at.unwrap_or(recording.created_at);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{}",
        when.year(),
        u8::from(when.month()),
        when.day(),
        when.hour(),
        when.minute(),
        when.second(),
        recording.file
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_ids_have_one_length_and_differ() {
        assert_eq!(block_id(0).len(), block_id(123_456).len());
        assert_ne!(block_id(1), block_id(2));
    }

    #[test]
    fn file_names_are_plain() {
        assert!(valid_file_name("screen-20261118-101500.mkv"));
        assert!(!valid_file_name(".hidden"));
        assert!(!valid_file_name("a/b.mkv"));
        assert!(!valid_file_name(""));
        assert!(!valid_file_name(&"a".repeat(129)));
    }

    #[test]
    fn a_file_is_cut_into_the_chunks_it_needs() {
        let r = |size| Recording {
            id: "i".into(),
            stick_id: "s".into(),
            file: "f".into(),
            kind: Kind::Video,
            size,
            sha256: "0".repeat(64),
            started_at: None,
            duration_s: None,
            next_chunk: 0,
            created_at: OffsetDateTime::UNIX_EPOCH,
            completed_at: None,
            expires_at: None,
        };
        assert_eq!(r(1).chunk_count(), 1);
        assert_eq!(r(CHUNK_BYTES as u64).chunk_count(), 1);
        assert_eq!(r(CHUNK_BYTES as u64 + 1).chunk_count(), 2);
    }
}
