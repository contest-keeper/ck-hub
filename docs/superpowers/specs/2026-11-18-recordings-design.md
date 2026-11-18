# Recordings upload (C4, H.5): Hub side

Sticks record the screen (`ck-recorder`: H.264 `.mkv` segments, or `.jpg` snapshots in snapshots
mode) and keep the files on the stick. When the recording settings say `upload`, the agent sends
the files to the Hub so an organizer can watch them without collecting sticks. The agent side is
lane B task B6; this note and `crates/ck-hub/src/recordings.rs` are the Hub side.

## Decisions

| Question | Decision |
|---|---|
| Where the bytes live | A private Azure blob container `recordings` (the user chose blob over database on 2026-11-18). The database keeps only metadata. Same storage account and key as the downloads (`CK_DOWNLOAD_ACCOUNT`, `CK_DOWNLOAD_KEY`); container name `CK_RECORDINGS_CONTAINER`, default `recordings`. Without the download settings the upload routes answer 404. |
| Who talks to Azure | Only the Hub. The stick sends chunks to the Hub (so the stick only needs the Hub allowed on its network and H.3 pinning stays one address). The Hub writes them as blocks of a block blob (Put Block, then Put Block List) using short-lived SAS links with exact permissions (`c`, `w`, `r`, `d`), made like the download links. Azure calls run on `spawn_blocking`, never under the Hub's write lock. |
| Unit | One recording is one file: a `.mkv` segment or a `.jpg` snapshot. Blob path `<organization id>/<stick id>/<recording id>`. The Hub never reads, decodes or runs the bytes; it stores them as opaque data and serves them with a fixed content type (`video/x-matroska` or `image/jpeg`) and `x-content-type-options: nosniff`. |
| Resumable | The file is cut into chunks of at most 4 MiB, numbered from 0, sent in order. The Hub keeps `next_chunk` per recording (only moved after Azure accepted the block). After a cut the stick asks for the status and continues at `next_chunk`. A chunk below `next_chunk` is answered 200 without writing (a repeat); a chunk above it is 409. The block id is the chunk number, so a repeat overwrites itself. |
| Integrity | Each chunk carries its SHA-256; the Hub checks it. On `complete` the Hub checks that every chunk arrived and that the byte total equals the size announced at the start. The announced whole-file SHA-256 is kept and shown; the Hub does not re-read the blob to check it (the stick agent verifies before it deletes the local file). |
| Signing | Same stick secret as heartbeats (`ck_crypto::verify`), but a raw chunk is not JSON, so the signed bytes are a fixed text, one per call (below). They bind the recording id and chunk number, so a chunk cannot be replayed into another recording. |
| Limits | Chunk at most 4 MiB. File at most 2 GiB. At most 3 open (not complete) uploads per stick and 20 GiB kept per stick. At most 8 chunks are handled at once in the whole Hub (a semaphore, like password hashing; the container has 0.5 GiB). Chunks have their own rate budget (600 a minute per stick) separate from heartbeats and events, so an uploading stick never looks offline. Larger bodies are refused before they are read. |
| Retention | Decided by the Hub, not by the stick: `CK_RECORDING_RETENTION_DAYS`, default 30, allowed 1 to 365. The `retention_days` setting in the stick's settings is for the stick's own local clean-up; the Hub does not trust a stick to shorten the life of evidence. (When the Hub can map a stick to its settings profile, a per-organization value can replace the default; not needed now.) `expires_at` = time of `complete` + days. |
| Clean-up job | One thread, every hour. Per organization: for every recording past `expires_at`, delete the blob (404 counts as already gone), then the row. Uploads never completed and older than 3 days are removed the same way. Azure drops uncommitted blocks by itself after a week. |
| Who may watch | Members of the stick's organization (viewer or higher), by session. `GET .../recordings/{id}/view` answers a 302 to a read-only link that works 2 hours (long enough for a seek-heavy session; link lifetime for downloads, decision 9, is separate), with the content type forced by the link. The storage address is never listed. |
| "Alerts" | The Hub has no alert list; there are warnings and verdicts on the stick rows. The viewer is a "Recordings" panel in the stick's details, next to those warnings and the network history, listing each file with its time and a Watch link. No new alert subsystem. Seeking to the moment of a warning is not built; a later step can pass the time to the viewer. |
| Isolation | Table `recordings` has the usual row-level security on `organization_id`. The stick-signed calls find the stick's organization from its secret, like events. |
| Never logged | SAS links and signatures are never logged or put in an error. |

## Wire format (for lane B, B6)

All calls carry `x-ck-stick: <stick id>` and `x-ck-signature: <signature>` (the same headers as
heartbeats). Signatures are over the text shown, with `\n` as a line break.

1. `POST /api/v1/recordings`, JSON body signed as the bytes (like an event):
   `{ "file": "screen-20261118-101500.mkv", "kind": "video" | "snapshot", "size": 123456789, "sha256": "<hex>", "started_at": "<RFC 3339, optional>", "duration_s": 600 }`.
   `file` is the stick's own name for the file; it is unique per stick, so posting it again returns the same recording (idempotent).
   Answer `200 { "id", "chunk_size": 4194304, "next_chunk": 0, "complete": false }`.
2. `GET /api/v1/recordings/{id}` status; signature over `ck-recording-status\n<id>`. Answer as above with the current `next_chunk`.
3. `PUT /api/v1/recordings/{id}/chunks/{n}` raw bytes, header `x-ck-chunk-sha256: <hex>`; signature over
   `ck-recording-chunk\n<id>\n<n>\n<chunk sha256>`. Answer `200 { "next_chunk" }`.
4. `POST /api/v1/recordings/{id}/complete` no body; signature over `ck-recording-complete\n<id>`. Answer `200 { "complete": true }`; a missing chunk is `409` with the `next_chunk` to resume from.

Errors: 400 unreadable, 401 unknown stick or bad signature, 404 unknown recording, 409 out of order or missing chunks, 413 too big, 429 too many, 503 storage not available (the stick retries later).
The agent should upload at idle priority and may throttle itself; the Hub does not slow a chunk down.

## Plan

1. Design (this note).
2. Store (migration 0014, memory and Postgres), blob backend trait (Azure + in-memory for tests), upload routes, retention thread, tests.
3. Organizer routes and the viewer on the Hub page.
4. ck-infra: the `recordings` container and settings (develop only; applied in C7).
5. H.4, the Hub part (stick keys tied to a contest and a validity window): a separate, later PR.

## Not in scope

Server-side transcoding or thumbnails, per-organization retention, deleting a recording by hand
(an organizer cannot yet; retention does it), downloading a whole contest as a zip.
