//! Recordings a stick uploads: the chunked, resumable upload, who may start and watch, the limits
//! and the clean-up of expired files.

use std::fs;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{
    CHUNK_BYTES, Hub, MAX_CHUNKS_PER_MINUTE, MemoryBlobs, MemoryStore, PasswordCost, Registry,
    Role, router,
};
use ck_protocol::{SIGNATURE_HEADER, STICK_HEADER, Token};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

struct Setup {
    dir: TempDir,
    app: Router,
    hub: Arc<Hub>,
    blobs: Arc<MemoryBlobs>,
    now: Arc<Mutex<OffsetDateTime>>,
}

fn setup(with_storage: bool) -> Setup {
    let dir = TempDir::new().expect("temp dir");
    fs::write(
        dir.path().join("registry.json"),
        r#"{"sticks":[
            {"stick_id":"stick-a","token":"token-of-stick-a","team":"Team A","site":"Lab 1",
             "contest":"Spring","image":"x.img","created_at":"2026-11-10T15:22:00","machine_number":1},
            {"stick_id":"stick-b","token":"token-of-stick-b","team":"Team B","site":"Lab 1",
             "contest":"Spring","image":"x.img","created_at":"2026-11-10T15:22:00","machine_number":2}
        ]}"#,
    )
    .expect("registry");
    let registry = Registry::open(dir.path().join("registry.json")).expect("open");
    let now = Arc::new(Mutex::new(
        OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time"),
    ));
    let clock = {
        let now = Arc::clone(&now);
        Box::new(move || *now.lock().expect("clock"))
    };
    let blobs = Arc::new(MemoryBlobs::default());
    let mut hub = Hub::new(registry, Arc::new(MemoryStore::default()), None, clock)
        .with_password_cost(PasswordCost {
            memory_kib: 8,
            passes: 1,
        });
    if with_storage {
        hub = hub.with_recordings(blobs.clone(), 30);
    }
    for email in ["owner@example.org", "viewer@example.org"] {
        hub.create_user(email, PASSWORD).expect("a user");
    }
    hub.create_organization("default", "Default", "owner@example.org")
        .expect("an organization");
    hub.add_member("default", "viewer@example.org", Role::Viewer)
        .expect("a viewer");
    let hub = Arc::new(hub);
    Setup {
        dir,
        app: router(Arc::clone(&hub)),
        hub,
        blobs,
        now,
    }
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value, Option<String>) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .map(|v| v.to_str().expect("text").to_owned());
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        location,
    )
}

async fn cookie(app: &Router, email: &str) -> String {
    let request = Request::post("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": email, "password": PASSWORD }).to_string(),
        ))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    response.headers()["set-cookie"]
        .to_str()
        .expect("text")
        .split(';')
        .next()
        .expect("cookie")
        .to_owned()
}

fn sign(token: &str, body: &[u8]) -> String {
    ck_crypto::sign(&Token::new(token), body)
}

fn sha(bytes: &[u8]) -> String {
    ck_crypto::sha256_hex(bytes)
}

/// Some bytes that differ from place to place, so a chunk in the wrong place is noticed.
fn bytes_of(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| u8::try_from(i % 251).expect("fits"))
        .collect()
}

async fn start(
    app: &Router,
    stick: &str,
    token: &str,
    file: &str,
    data: &[u8],
) -> (StatusCode, Value) {
    let body = json!({
        "file": file, "kind": "video", "size": data.len(), "sha256": sha(data),
        "started_at": "2026-11-18T10:15:00Z", "duration_s": 600
    })
    .to_string();
    let request = Request::post("/api/v1/recordings")
        .header(STICK_HEADER, stick)
        .header(SIGNATURE_HEADER, sign(token, body.as_bytes()))
        .body(Body::from(body))
        .expect("request");
    let (status, json, _) = call(app, request).await;
    (status, json)
}

async fn status_of(app: &Router, stick: &str, token: &str, id: &str) -> (StatusCode, Value) {
    let request = Request::get(format!("/api/v1/recordings/{id}"))
        .header(STICK_HEADER, stick)
        .header(
            SIGNATURE_HEADER,
            sign(token, format!("ck-recording-status\n{id}").as_bytes()),
        )
        .body(Body::empty())
        .expect("request");
    let (status, json, _) = call(app, request).await;
    (status, json)
}

async fn chunk(
    app: &Router,
    stick: &str,
    token: &str,
    id: &str,
    index: u32,
    bytes: &[u8],
) -> (StatusCode, Value) {
    let digest = sha(bytes);
    let signed = format!("ck-recording-chunk\n{id}\n{index}\n{digest}");
    let request = Request::put(format!("/api/v1/recordings/{id}/chunks/{index}"))
        .header(STICK_HEADER, stick)
        .header(SIGNATURE_HEADER, sign(token, signed.as_bytes()))
        .header("x-ck-chunk-sha256", digest)
        .body(Body::from(bytes.to_vec()))
        .expect("request");
    let (status, json, _) = call(app, request).await;
    (status, json)
}

async fn complete(app: &Router, stick: &str, token: &str, id: &str) -> (StatusCode, Value) {
    let request = Request::post(format!("/api/v1/recordings/{id}/complete"))
        .header(STICK_HEADER, stick)
        .header(
            SIGNATURE_HEADER,
            sign(token, format!("ck-recording-complete\n{id}").as_bytes()),
        )
        .body(Body::empty())
        .expect("request");
    let (status, json, _) = call(app, request).await;
    (status, json)
}

#[tokio::test]
async fn a_scoped_recording_can_finish_during_its_day_of_grace() {
    let setup = setup(true);
    let path = setup.dir.path().join("registry.json");
    let mut registry: Value =
        serde_json::from_slice(&fs::read(&path).expect("fixture registry")).expect("json");
    let expiry = *setup.now.lock().expect("clock");
    let utc = |instant: OffsetDateTime| {
        instant
            .format(&time::format_description::well_known::Rfc3339)
            .expect("utc")
    };
    let mut scoped = registry["sticks"][0].clone();
    scoped["stick_id"] = json!("scoped-stick");
    scoped["token"] = json!("a-distinct-scoped-stick-secret");
    scoped["credential_scope"] = json!({
        "contest_id": "2042", "valid_from": utc(expiry - Duration::hours(2)), "valid_until": utc(expiry),
    });
    registry["sticks"]
        .as_array_mut()
        .expect("sticks")
        .push(scoped);
    fs::write(path, registry.to_string()).expect("scoped registry");
    let app = &setup.app;
    let stick = "scoped-stick";
    let token = "a-distinct-scoped-stick-secret";
    let bytes = bytes_of(100);
    *setup.now.lock().expect("clock") = expiry - Duration::hours(3);
    assert_eq!(
        start(app, stick, token, "finish.mkv", &bytes).await.0,
        StatusCode::UNAUTHORIZED
    );
    *setup.now.lock().expect("clock") = expiry;
    let (status, begun) = start(app, stick, token, "finish.mkv", &bytes).await;
    assert_eq!(status, StatusCode::OK);
    let id = begun["id"].as_str().expect("recording ID");
    assert_eq!(status_of(app, stick, token, id).await.0, StatusCode::OK);
    assert_eq!(
        chunk(app, stick, token, id, 0, &bytes).await.0,
        StatusCode::OK
    );
    *setup.now.lock().expect("clock") = expiry + Duration::hours(24) - Duration::seconds(1);
    assert_eq!(complete(app, stick, token, id).await.0, StatusCode::OK);
    *setup.now.lock().expect("clock") = expiry + Duration::hours(24);
    assert_eq!(
        status_of(app, stick, token, id).await.0,
        StatusCode::UNAUTHORIZED
    );
}

/// A file of two chunks and a bit, uploaded in full.
async fn upload_whole(s: &Setup, file: &str) -> (String, Vec<u8>) {
    let data = bytes_of(2 * CHUNK_BYTES + 1000);
    let (status, started) = start(&s.app, "stick-a", "token-of-stick-a", file, &data).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let id = started["id"].as_str().expect("id").to_owned();
    for (index, part) in data.chunks(CHUNK_BYTES).enumerate() {
        let (status, answer) = chunk(
            &s.app,
            "stick-a",
            "token-of-stick-a",
            &id,
            u32::try_from(index).expect("small"),
            part,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
    }
    let (status, done) = complete(&s.app, "stick-a", "token-of-stick-a", &id).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["complete"], true);
    (id, data)
}

#[tokio::test]
async fn a_file_uploaded_in_chunks_is_stored_whole_and_listed() {
    let s = setup(true);
    let (id, data) = upload_whole(&s, "screen-1.mkv").await;
    let blob = s.blobs.names();
    assert_eq!(blob.len(), 1);
    assert!(blob[0].ends_with(&format!("/stick-a/{id}")), "{blob:?}");
    assert_eq!(s.blobs.get(&blob[0]).expect("stored"), data);

    let viewer = cookie(&s.app, "viewer@example.org").await;
    let request = Request::get("/api/v1/orgs/default/sticks/stick-a/recordings")
        .header("cookie", &viewer)
        .body(Body::empty())
        .expect("request");
    let (status, list, _) = call(&s.app, request).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list[0]["file"], "screen-1.mkv");
    assert_eq!(list[0]["complete"], true);
    assert_eq!(list[0]["chunks_total"], 3);
    assert_eq!(list[0]["size"], data.len());
}

#[tokio::test]
async fn an_interrupted_upload_resumes_where_it_stopped() {
    let s = setup(true);
    let data = bytes_of(2 * CHUNK_BYTES + 1000);
    let (_, started) = start(&s.app, "stick-a", "token-of-stick-a", "cut.mkv", &data).await;
    let id = started["id"].as_str().expect("id").to_owned();
    let parts: Vec<&[u8]> = data.chunks(CHUNK_BYTES).collect();
    chunk(&s.app, "stick-a", "token-of-stick-a", &id, 0, parts[0]).await;

    // The stick lost its connection and starts again: the same file is the same upload.
    let (_, again) = start(&s.app, "stick-a", "token-of-stick-a", "cut.mkv", &data).await;
    assert_eq!(again["id"], id.as_str());
    assert_eq!(again["next_chunk"], 1);
    let (_, status) = status_of(&s.app, "stick-a", "token-of-stick-a", &id).await;
    assert_eq!(status["next_chunk"], 1);

    // A repeated chunk is accepted and changes nothing; one too far ahead is refused with the
    // place to continue from.
    let (code, repeat) = chunk(&s.app, "stick-a", "token-of-stick-a", &id, 0, parts[0]).await;
    assert_eq!(
        (code, repeat["next_chunk"].as_u64()),
        (StatusCode::OK, Some(1))
    );
    let (code, ahead) = chunk(&s.app, "stick-a", "token-of-stick-a", &id, 2, parts[2]).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(ahead["next_chunk"], 1);
    // Completing with a chunk missing says where to go on.
    let (code, early) = complete(&s.app, "stick-a", "token-of-stick-a", &id).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(early["next_chunk"], 1);

    chunk(&s.app, "stick-a", "token-of-stick-a", &id, 1, parts[1]).await;
    chunk(&s.app, "stick-a", "token-of-stick-a", &id, 2, parts[2]).await;
    let (code, done) = complete(&s.app, "stick-a", "token-of-stick-a", &id).await;
    assert_eq!((code, &done["complete"]), (StatusCode::OK, &json!(true)));
    let name = &s.blobs.names()[0];
    assert_eq!(s.blobs.get(name).expect("stored"), data);
    // Completing again changes nothing.
    let (code, _) = complete(&s.app, "stick-a", "token-of-stick-a", &id).await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test]
async fn a_bad_signature_a_bad_hash_or_a_wrong_size_is_refused() {
    let s = setup(true);
    let data = bytes_of(CHUNK_BYTES + 10);
    let (_, started) = start(&s.app, "stick-a", "token-of-stick-a", "x.mkv", &data).await;
    let id = started["id"].as_str().expect("id").to_owned();

    // Stick B's secret does not sign for stick A, and a made-up secret signs for nobody.
    let (code, _) = chunk(
        &s.app,
        "stick-a",
        "token-of-stick-b",
        &id,
        0,
        &data[..CHUNK_BYTES],
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _) = start(&s.app, "stick-a", "wrong", "y.mkv", &data).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);

    // A chunk that is not what its hash says.
    let digest = sha(&data[..CHUNK_BYTES]);
    let signed = format!("ck-recording-chunk\n{id}\n0\n{digest}");
    let request = Request::put(format!("/api/v1/recordings/{id}/chunks/0"))
        .header(STICK_HEADER, "stick-a")
        .header(
            SIGNATURE_HEADER,
            sign("token-of-stick-a", signed.as_bytes()),
        )
        .header("x-ck-chunk-sha256", digest)
        .body(Body::from(vec![0_u8; CHUNK_BYTES]))
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::BAD_REQUEST);

    // A signature for another chunk number does not carry over.
    let digest = sha(&data[..CHUNK_BYTES]);
    let signed = format!("ck-recording-chunk\n{id}\n1\n{digest}");
    let request = Request::put(format!("/api/v1/recordings/{id}/chunks/0"))
        .header(STICK_HEADER, "stick-a")
        .header(
            SIGNATURE_HEADER,
            sign("token-of-stick-a", signed.as_bytes()),
        )
        .header("x-ck-chunk-sha256", digest)
        .body(Body::from(data[..CHUNK_BYTES].to_vec()))
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::UNAUTHORIZED);

    // A short chunk where a full one is needed.
    let (code, _) = chunk(&s.app, "stick-a", "token-of-stick-a", &id, 0, &data[..1000]).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(s.blobs.names(), Vec::<String>::new());
}

#[tokio::test]
async fn a_chunk_over_the_limit_is_refused_before_it_is_read() {
    let s = setup(true);
    let data = bytes_of(CHUNK_BYTES + 10);
    let (_, started) = start(&s.app, "stick-a", "token-of-stick-a", "big.mkv", &data).await;
    let id = started["id"].as_str().expect("id").to_owned();
    let (code, _) = chunk(
        &s.app,
        "stick-a",
        "token-of-stick-a",
        &id,
        0,
        &bytes_of(CHUNK_BYTES + 2),
    )
    .await;
    assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_stick_cannot_touch_another_sticks_recording() {
    let s = setup(true);
    let data = bytes_of(100);
    let (_, started) = start(&s.app, "stick-a", "token-of-stick-a", "mine.mkv", &data).await;
    let id = started["id"].as_str().expect("id").to_owned();
    let (code, _) = status_of(&s.app, "stick-b", "token-of-stick-b", &id).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = chunk(&s.app, "stick-b", "token-of-stick-b", &id, 0, &data).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = complete(&s.app, "stick-b", "token-of-stick-b", &id).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_file_name_size_and_open_upload_limits_hold() {
    let s = setup(true);
    let data = bytes_of(100);
    for bad in ["../etc/passwd", ".hidden", "", "a b.mkv"] {
        let (code, _) = start(&s.app, "stick-a", "token-of-stick-a", bad, &data).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
    }
    // The same name with other content is a conflict, not a second upload.
    start(&s.app, "stick-a", "token-of-stick-a", "same.mkv", &data).await;
    let (code, _) = start(
        &s.app,
        "stick-a",
        "token-of-stick-a",
        "same.mkv",
        &bytes_of(101),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT);
    // Three open uploads at most.
    start(&s.app, "stick-a", "token-of-stick-a", "two.mkv", &data).await;
    start(&s.app, "stick-a", "token-of-stick-a", "three.mkv", &data).await;
    let (code, _) = start(&s.app, "stick-a", "token-of-stick-a", "four.mkv", &data).await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
    // A file past the limit.
    let body =
        json!({"file": "huge.mkv", "kind": "video", "size": 3_u64 << 30, "sha256": "a".repeat(64)})
            .to_string();
    let request = Request::post("/api/v1/recordings")
        .header(STICK_HEADER, "stick-b")
        .header(SIGNATURE_HEADER, sign("token-of-stick-b", body.as_bytes()))
        .body(Body::from(body))
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn upload_calls_have_a_budget_of_their_own_that_resets_each_minute() {
    let s = setup(true);
    let data = bytes_of(100);
    let (_, started) = start(&s.app, "stick-a", "token-of-stick-a", "busy.mkv", &data).await;
    let id = started["id"].as_str().expect("id").to_owned();
    for _ in 0..MAX_CHUNKS_PER_MINUTE {
        let (code, _) = status_of(&s.app, "stick-a", "token-of-stick-a", &id).await;
        if code == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
    }
    let (code, _) = status_of(&s.app, "stick-a", "token-of-stick-a", &id).await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
    // The next minute starts the count again.
    *s.now.lock().expect("clock") += Duration::seconds(61);
    let (code, _) = status_of(&s.app, "stick-a", "token-of-stick-a", &id).await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test]
async fn organizers_watch_through_a_link_and_only_their_own() {
    let s = setup(true);
    let (id, _) = upload_whole(&s, "watch.mkv").await;
    let viewer = cookie(&s.app, "viewer@example.org").await;

    let request = Request::get(format!("/api/v1/orgs/default/recordings/{id}/view"))
        .header("cookie", &viewer)
        .body(Body::empty())
        .expect("request");
    let (code, _, location) = call(&s.app, request).await;
    assert_eq!(code, StatusCode::FOUND);
    let location = location.expect("a link");
    assert!(location.contains(&format!("/stick-a/{id}")), "{location}");
    assert!(location.contains("video%2Fx-matroska"), "{location}");

    // No login, no link; and an unknown or incomplete recording has none.
    let request = Request::get(format!("/api/v1/orgs/default/recordings/{id}/view"))
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::UNAUTHORIZED);
    let request = Request::get("/api/v1/orgs/default/recordings/nothing/view")
        .header("cookie", &viewer)
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::NOT_FOUND);
    let data = bytes_of(100);
    let (_, open) = start(&s.app, "stick-a", "token-of-stick-a", "open.mkv", &data).await;
    let request = Request::get(format!(
        "/api/v1/orgs/default/recordings/{}/view",
        open["id"].as_str().expect("id")
    ))
    .header("cookie", &viewer)
    .body(Body::empty())
    .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn expired_recordings_and_abandoned_uploads_are_removed_with_their_bytes() {
    let s = setup(true);
    let (_, _) = upload_whole(&s, "old.mkv").await;
    let data = bytes_of(100);
    let (_, abandoned) = start(&s.app, "stick-a", "token-of-stick-a", "lost.mkv", &data).await;
    let lost = abandoned["id"].as_str().expect("id").to_owned();
    chunk(&s.app, "stick-a", "token-of-stick-a", &lost, 0, &data).await;
    assert_eq!(s.hub.purge_recordings().expect("purge"), 0);

    // Past the abandoned-upload age the unfinished one goes; the complete one stays 30 days.
    *s.now.lock().expect("clock") += Duration::days(4);
    assert_eq!(s.hub.purge_recordings().expect("purge"), 1);
    assert_eq!(s.blobs.names().len(), 1);
    *s.now.lock().expect("clock") += Duration::days(27);
    assert_eq!(s.hub.purge_recordings().expect("purge"), 1);
    assert_eq!(s.blobs.names(), Vec::<String>::new());
    let viewer = cookie(&s.app, "viewer@example.org").await;
    let request = Request::get("/api/v1/orgs/default/sticks/stick-a/recordings")
        .header("cookie", &viewer)
        .body(Body::empty())
        .expect("request");
    let (_, after, _) = call(&s.app, request).await;
    assert_eq!(after, json!([]));
}

#[tokio::test]
async fn without_storage_the_routes_do_not_exist() {
    let s = setup(false);
    let (code, _) = start(
        &s.app,
        "stick-a",
        "token-of-stick-a",
        "x.mkv",
        &bytes_of(10),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_an_archived_stick_removes_its_recording_files_from_storage() {
    let s = setup(true);
    upload_whole(&s, "one.mkv").await;
    upload_whole(&s, "two.mkv").await;
    assert_eq!(s.blobs.names().len(), 2);
    let owner = cookie(&s.app, "owner@example.org").await;
    let send = |method: &'static str, path: &'static str, body: Body| {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("cookie", &owner)
            .header("content-type", "application/json")
            .body(body)
            .expect("request");
        call(&s.app, request)
    };

    // A stick that is not archived cannot be deleted, and its files stay.
    let stick = "/api/v1/orgs/default/sticks/stick-a";
    assert_eq!(
        send("DELETE", stick, Body::empty()).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(s.blobs.names().len(), 2);

    let archive = "/api/v1/orgs/default/sticks/stick-a/archive";
    let days = Body::from(json!({ "days": 7 }).to_string());
    assert_eq!(send("POST", archive, days).await.0, StatusCode::NO_CONTENT);
    // An archived stick uploads nothing more, and keeps what it has until it is deleted.
    let (status, _) = start(
        &s.app,
        "stick-a",
        "token-of-stick-a",
        "three.mkv",
        &bytes_of(100),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(s.blobs.names().len(), 2);

    assert_eq!(
        send("DELETE", stick, Body::empty()).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(s.blobs.names().is_empty(), "{:?}", s.blobs.names());
}

#[tokio::test]
async fn an_unsigned_chunk_is_refused_before_its_body_is_read() {
    let s = setup(true);
    let data = bytes_of(CHUNK_BYTES + 10);
    let (_, started) = start(&s.app, "stick-a", "token-of-stick-a", "big.mkv", &data).await;
    let id = started["id"].as_str().expect("id").to_owned();
    let big = bytes_of(CHUNK_BYTES + 2);
    let digest = sha(&big);
    let request = Request::put(format!("/api/v1/recordings/{id}/chunks/0"))
        .header(STICK_HEADER, "stick-a")
        .header(SIGNATURE_HEADER, "00")
        .header("x-ck-chunk-sha256", digest)
        .body(Body::from(big))
        .expect("request");
    let (code, _, _) = call(&s.app, request).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_duration_the_database_cannot_keep_is_a_bad_request() {
    let s = setup(true);
    let data = bytes_of(10);
    let body = json!({
        "file": "a.mkv", "kind": "video", "size": data.len(), "sha256": sha(&data),
        "duration_s": u64::MAX
    })
    .to_string();
    let request = Request::post("/api/v1/recordings")
        .header(STICK_HEADER, "stick-a")
        .header(SIGNATURE_HEADER, sign("token-of-stick-a", body.as_bytes()))
        .body(Body::from(body))
        .expect("request");
    let (code, _, _) = call(&s.app, request).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

/// A body whose bytes never arrive.
struct Stalled;

impl http_body::Body for Stalled {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::task::Poll::Pending
    }
}

#[tokio::test(start_paused = true)]
async fn a_chunk_body_that_never_arrives_times_out_and_frees_its_slot() {
    let s = setup(true);
    let data = bytes_of(CHUNK_BYTES + 10);
    let (_, started) = start(&s.app, "stick-a", "token-of-stick-a", "slow.mkv", &data).await;
    let id = started["id"].as_str().expect("id").to_owned();
    let digest = sha(&data[..CHUNK_BYTES]);
    let signed = format!("ck-recording-chunk\n{id}\n0\n{digest}");
    // More stalled uploads than there are slots: a slot that was not given back would block the
    // later ones for good.
    for _ in 0..12 {
        let request = Request::put(format!("/api/v1/recordings/{id}/chunks/0"))
            .header(STICK_HEADER, "stick-a")
            .header(
                SIGNATURE_HEADER,
                sign("token-of-stick-a", signed.as_bytes()),
            )
            .header("x-ck-chunk-sha256", digest.clone())
            .body(Body::new(Stalled))
            .expect("request");
        let (code, _, _) = call(&s.app, request).await;
        assert_eq!(code, StatusCode::REQUEST_TIMEOUT);
    }
    // The slot is free again: a real chunk goes through.
    let (code, _) = chunk(
        &s.app,
        "stick-a",
        "token-of-stick-a",
        &id,
        0,
        &data[..CHUNK_BYTES],
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}
