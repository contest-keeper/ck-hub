use std::fs;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use ck_hub::{Hub, ONLINE_WINDOW_SECONDS, Registry, router};
use ck_protocol::{
    Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, RecorderStatus, RecordingMode,
    SIGNATURE_HEADER, STICK_HEADER, StickId, Token,
};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN_A: &str = "token-of-stick-a";
const TOKEN_B: &str = "token-of-stick-b";

struct Setup {
    dir: TempDir,
    app: Router,
    now: Arc<Mutex<DateTime<Utc>>>,
}

fn registry_json(sticks: &[(&str, &str, &str, u32)]) -> String {
    let entries: Vec<String> = sticks
        .iter()
        .map(|(id, token, team, number)| {
            format!(
                r#"{{"stick_id":"{id}","token":"{token}","team":"{team}","site":"Lab 1",
                   "contest":"Spring","image":"x.img","created_at":"2026-11-10T15:22:00",
                   "machine_number":{number}}}"#
            )
        })
        .collect();
    format!(r#"{{"sticks":[{}]}}"#, entries.join(","))
}

fn setup() -> Setup {
    let dir = TempDir::new().expect("temp dir");
    fs::write(
        dir.path().join("registry.json"),
        registry_json(&[
            ("stick-a", TOKEN_A, "Team A", 1),
            ("stick-b", TOKEN_B, "Team B", 2),
        ]),
    )
    .expect("registry");
    let registry = Registry::open(dir.path().join("registry.json")).expect("open");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.path().join("hub.log"))
        .expect("log");
    let now = Arc::new(Mutex::new(
        "2026-11-12T10:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("time"),
    ));
    let clock = {
        let now = Arc::clone(&now);
        Box::new(move || *now.lock().expect("clock"))
    };
    let app = router(Arc::new(Hub::new(registry, Some(log), clock)));
    Setup { dir, app, now }
}

fn heartbeat_body(stick: &str, boot: &str, seq: u64) -> Vec<u8> {
    let heartbeat = Heartbeat {
        stick_id: StickId::parse(stick).expect("id"),
        boot_id: boot.to_owned(),
        seq,
        sent_at: "2026-11-12T10:00:00Z".parse().expect("time"),
        uptime_s: 45 + seq,
        hostname: "team-a-1".to_owned(),
        image_version: "ck-os-base development".to_owned(),
        config_sha256: "0".repeat(64),
        team: "Team A".to_owned(),
        site: "Lab 1".to_owned(),
        recording: RecorderStatus {
            mode: RecordingMode::Low,
            state: RecorderState::Recording,
        },
    };
    serde_json::to_vec(&heartbeat).expect("json")
}

fn sign(token: &str, body: &[u8]) -> String {
    ck_crypto::sign(&Token::new(token), body)
}

async fn send(
    app: &Router,
    stick: Option<&str>,
    signature: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, String) {
    let mut request = Request::post("/api/v1/heartbeat");
    if let Some(stick) = stick {
        request = request.header(STICK_HEADER, stick);
    }
    if let Some(signature) = signature {
        request = request.header(SIGNATURE_HEADER, signature);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A correctly signed heartbeat from stick A.
async fn heartbeat(app: &Router, boot: &str, seq: u64) -> (StatusCode, String) {
    let body = heartbeat_body("stick-a", boot, seq);
    let signature = sign(TOKEN_A, &body);
    send(app, Some("stick-a"), Some(&signature), body).await
}

async fn list(app: &Router) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/sticks")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn a_signed_heartbeat_is_accepted_and_shows_in_the_list() {
    let setup = setup();
    let (status, body) = heartbeat(&setup.app, "boot-1", 1).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let sticks = list(&setup.app).await;
    let a = &sticks[0];
    assert_eq!(a["stick_id"], "stick-a");
    assert_eq!(a["team"], "Team A");
    assert_eq!(a["machine_number"], 1);
    assert_eq!(a["online"], true);
    assert_eq!(a["last"]["seq"], 1);
    assert_eq!(a["last"]["hostname"], "team-a-1");
    assert_eq!(a["last"]["recorder"], "recording");
    assert_eq!(a["last"]["heartbeats"], 1);

    // The other stick is known to the admin but has said nothing yet.
    assert_eq!(sticks[1]["stick_id"], "stick-b");
    assert_eq!(sticks[1]["online"], false);
    assert!(sticks[1]["last"].is_null());
}

#[tokio::test]
async fn unsigned_forged_tampered_and_unknown_messages_are_refused() {
    let setup = setup();
    let body = heartbeat_body("stick-a", "boot-1", 1);
    let good = sign(TOKEN_A, &body);

    let (status, _) = send(&setup.app, Some("stick-a"), None, body.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no signature header");
    let (status, _) = send(&setup.app, None, Some(&good), body.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no stick header");

    let (status, _) = send(
        &setup.app,
        Some("stick-a"),
        Some(&sign("wrong", &body)),
        body.clone(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "signed with another secret"
    );
    let (status, _) = send(&setup.app, Some("stick-a"), Some("not-hex"), body.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "malformed signature");

    let mut tampered = body.clone();
    tampered.extend_from_slice(b" ");
    let (status, _) = send(&setup.app, Some("stick-a"), Some(&good), tampered).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "body changed after signing"
    );

    let (status, message) = send(&setup.app, Some("stick-zzz"), Some(&good), body.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "unknown stick");
    let (_, known) = send(&setup.app, Some("stick-a"), Some("bad"), body).await;
    assert_eq!(
        message, known,
        "an unknown stick and a bad signature look the same"
    );

    assert!(
        list(&setup.app).await[0]["last"].is_null(),
        "nothing was stored"
    );
}

#[tokio::test]
async fn a_message_must_be_for_the_stick_that_signed_it_and_must_be_a_heartbeat() {
    let setup = setup();
    // Stick B signs a heartbeat that says it is from stick A.
    let body = heartbeat_body("stick-a", "boot-1", 1);
    let (status, why) = send(
        &setup.app,
        Some("stick-b"),
        Some(&sign(TOKEN_B, &body)),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(why.contains("another stick"), "{why}");

    let garbage = b"{\"hello\": true}".to_vec();
    let (status, _) = send(
        &setup.app,
        Some("stick-a"),
        Some(&sign(TOKEN_A, &garbage)),
        garbage,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_heartbeat_that_is_not_newer_is_refused_until_the_stick_boots_again() {
    let setup = setup();
    assert_eq!(heartbeat(&setup.app, "boot-1", 1).await.0, StatusCode::OK);
    assert_eq!(heartbeat(&setup.app, "boot-1", 2).await.0, StatusCode::OK);
    assert_eq!(
        heartbeat(&setup.app, "boot-1", 5).await.0,
        StatusCode::OK,
        "a gap is allowed"
    );

    let (status, why) = heartbeat(&setup.app, "boot-1", 5).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(why.contains("not newer"), "{why}");
    assert_eq!(
        heartbeat(&setup.app, "boot-1", 3).await.0,
        StatusCode::CONFLICT,
        "out of order"
    );

    // After a reboot the sequence starts again at 1.
    assert_eq!(heartbeat(&setup.app, "boot-2", 1).await.0, StatusCode::OK);
    let sticks = list(&setup.app).await;
    assert_eq!(sticks[0]["last"]["boot_id"], "boot-2");
    assert_eq!(sticks[0]["last"]["heartbeats"], 4);
}

#[tokio::test]
async fn a_stick_that_goes_quiet_is_shown_offline() {
    let setup = setup();
    assert_eq!(heartbeat(&setup.app, "boot-1", 1).await.0, StatusCode::OK);
    *setup.now.lock().expect("clock") += Duration::seconds(ONLINE_WINDOW_SECONDS);
    let sticks = list(&setup.app).await;
    assert_eq!(sticks[0]["online"], true, "still inside the window");
    assert_eq!(sticks[0]["last"]["seconds_ago"], ONLINE_WINDOW_SECONDS);

    *setup.now.lock().expect("clock") += Duration::seconds(1);
    assert_eq!(list(&setup.app).await[0]["online"], false);
}

#[tokio::test]
async fn a_stick_the_admin_adds_later_is_known_without_restarting() {
    let setup = setup();
    let body = heartbeat_body("stick-c", "boot-1", 1);
    let signature = sign("token-of-stick-c", &body);
    let (status, _) = send(&setup.app, Some("stick-c"), Some(&signature), body.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "not made yet");

    fs::write(
        setup.dir.path().join("registry.json"),
        registry_json(&[
            ("stick-a", TOKEN_A, "Team A", 1),
            ("stick-b", TOKEN_B, "Team B", 2),
            ("stick-c", "token-of-stick-c", "Team C", 3),
        ]),
    )
    .expect("registry");
    let (status, why) = send(&setup.app, Some("stick-c"), Some(&signature), body).await;
    assert_eq!(status, StatusCode::OK, "{why}");
    assert_eq!(list(&setup.app).await.as_array().expect("array").len(), 3);
}

#[tokio::test]
async fn a_registry_that_cannot_be_read_keeps_the_sticks_already_known() {
    let setup = setup();
    assert_eq!(heartbeat(&setup.app, "boot-1", 1).await.0, StatusCode::OK);
    // The admin tool is half way through writing the file.
    fs::write(setup.dir.path().join("registry.json"), "{ \"sticks\": [").expect("half file");
    assert_eq!(heartbeat(&setup.app, "boot-1", 2).await.0, StatusCode::OK);
}

#[tokio::test]
async fn the_log_names_what_happened_and_never_holds_a_secret_or_signature() {
    let setup = setup();
    let body = heartbeat_body("stick-a", "boot-1", 1);
    let signature = sign(TOKEN_A, &body);
    send(&setup.app, Some("stick-a"), Some(&signature), body.clone()).await;
    send(&setup.app, Some("stick-a"), Some("bad-signature"), body).await;

    let log = fs::read_to_string(setup.dir.path().join("hub.log")).expect("log");
    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("one json object per line"))
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["outcome"], "accepted");
    assert_eq!(lines[0]["stick"], "stick-a");
    assert_eq!(lines[0]["seq"], 1);
    assert_eq!(lines[1]["outcome"], "unknown stick or bad signature");
    for secret in [TOKEN_A, signature.as_str(), "bad-signature"] {
        assert!(!log.contains(secret), "the log holds {secret}");
    }
}

#[tokio::test]
async fn an_oversized_body_is_refused_before_it_is_read() {
    let setup = setup();
    let huge = vec![b'x'; 100 * 1024];
    let (status, _) = send(&setup.app, Some("stick-a"), Some("x"), huge).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn an_unknown_route_is_not_found() {
    let setup = setup();
    let response = setup
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/nothing")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_page_is_served_and_needs_nothing_from_outside() {
    let setup = setup();
    let response = setup
        .app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers()["content-type"]
        .to_str()
        .expect("text")
        .to_owned();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let page = String::from_utf8_lossy(&bytes);
    assert!(
        page.contains("/api/v1/sticks"),
        "the page reads the stick list"
    );
    // Everything is in the one file: no scripts, styles or images from another address.
    assert!(
        !page.contains("http://") && !page.contains("https://"),
        "an outside address"
    );
    assert!(
        !page.contains("innerHTML"),
        "stick names must not be put into the page as markup"
    );
}

fn event_bytes(boot: &str, seq: u64, prev_hash: &str, user: &str) -> Vec<u8> {
    Event {
        stick_id: StickId::parse("stick-a").expect("id"),
        boot_id: boot.to_owned(),
        seq,
        prev_hash: prev_hash.to_owned(),
        sent_at: "2026-11-12T10:00:00Z".parse().expect("time"),
        kind: EventKind::Login,
        payload: serde_json::json!({ "username": user }),
    }
    .canonical_bytes()
}

/// A properly chained event, signed by stick A, posted to the events route. Returns what the Hub
/// answered and the hash the next event of the chain must name.
async fn send_event(
    app: &Router,
    boot: &str,
    seq: u64,
    prev_hash: &str,
    user: &str,
) -> (StatusCode, String) {
    let body = event_bytes(boot, seq, prev_hash, user);
    let request = Request::post("/api/v1/events")
        .header(STICK_HEADER, "stick-a")
        .header(SIGNATURE_HEADER, sign(TOKEN_A, &body))
        .body(Body::from(body))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn next_hash(boot: &str, seq: u64, prev_hash: &str, user: &str) -> String {
    ck_crypto::sha256_hex(&event_bytes(boot, seq, prev_hash, user))
}

#[tokio::test]
async fn a_chained_login_event_shows_who_is_logged_in() {
    let setup = setup();
    let first = send_event(&setup.app, "b1", 1, FIRST_PREV_HASH, "ana").await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    let h1 = next_hash("b1", 1, FIRST_PREV_HASH, "ana");
    assert_eq!(
        send_event(&setup.app, "b1", 2, &h1, "ana").await.0,
        StatusCode::OK
    );

    let sticks = list(&setup.app).await;
    assert_eq!(sticks[0]["events"], 2);
    assert_eq!(sticks[0]["current_user"], "ana");
    assert_eq!(
        sticks[0]["warnings"],
        serde_json::json!([]),
        "a clean chain, one user"
    );
    assert!(sticks[1]["current_user"].is_null());
}

#[tokio::test]
async fn an_event_the_hub_already_has_is_a_conflict_and_forged_ones_are_refused() {
    let setup = setup();
    assert_eq!(
        send_event(&setup.app, "b1", 1, FIRST_PREV_HASH, "ana")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send_event(&setup.app, "b1", 1, FIRST_PREV_HASH, "ana")
            .await
            .0,
        StatusCode::CONFLICT,
        "the stick counts a 409 as sent"
    );

    let body = event_bytes("b1", 2, "x", "ana");
    let request = Request::post("/api/v1/events")
        .header(STICK_HEADER, "stick-a")
        .header(SIGNATURE_HEADER, sign("wrong", &body))
        .body(Body::from(body.clone()))
        .expect("request");
    let response = setup.app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Signed by stick B but saying it is from stick A.
    let request = Request::post("/api/v1/events")
        .header(STICK_HEADER, "stick-b")
        .header(SIGNATURE_HEADER, sign(TOKEN_B, &body))
        .body(Body::from(body))
        .expect("request");
    let response = setup.app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(list(&setup.app).await[0]["events"], 1);
}

#[tokio::test]
async fn a_gap_a_changed_event_and_a_late_start_are_kept_as_warnings_not_refused() {
    let setup = setup();
    assert_eq!(
        send_event(&setup.app, "b1", 1, FIRST_PREV_HASH, "ana")
            .await
            .0,
        StatusCode::OK
    );
    // Event 2 never arrives; event 3 does.
    let h2 = next_hash("b1", 2, "whatever", "ana");
    assert_eq!(
        send_event(&setup.app, "b1", 3, &h2, "ana").await.0,
        StatusCode::OK
    );
    // Event 4 does not follow event 3: it names another previous hash.
    assert_eq!(
        send_event(&setup.app, "b1", 4, "forged", "ana").await.0,
        StatusCode::OK
    );
    // The Hub has seen nothing of boot b2 before event 5.
    assert_eq!(
        send_event(&setup.app, "b2", 5, "x", "ana").await.0,
        StatusCode::OK
    );

    let warnings = list(&setup.app).await[0]["warnings"].to_string();
    assert!(warnings.contains("missing"), "{warnings}");
    assert!(warnings.contains("does not follow"), "{warnings}");
    assert!(warnings.contains("no earlier events"), "{warnings}");
}

#[tokio::test]
async fn two_different_users_in_one_boot_are_a_warning_but_a_new_boot_starts_clean() {
    let setup = setup();
    assert_eq!(
        send_event(&setup.app, "b1", 1, FIRST_PREV_HASH, "ana")
            .await
            .0,
        StatusCode::OK
    );
    let h1 = next_hash("b1", 1, FIRST_PREV_HASH, "ana");
    assert_eq!(
        send_event(&setup.app, "b1", 2, &h1, "bob").await.0,
        StatusCode::OK
    );

    let stick = &list(&setup.app).await[0];
    assert_eq!(stick["current_user"], "bob");
    let warnings = stick["warnings"].to_string();
    assert!(warnings.contains("2 different users"), "{warnings}");
    assert!(
        warnings.contains("ana") && warnings.contains("bob"),
        "{warnings}"
    );

    // After a reboot only the new boot counts: one user, no warning about users.
    assert_eq!(
        send_event(&setup.app, "b2", 1, FIRST_PREV_HASH, "cris")
            .await
            .0,
        StatusCode::OK
    );
    let stick = &list(&setup.app).await[0];
    assert_eq!(stick["current_user"], "cris");
    assert!(!stick["warnings"].to_string().contains("different users"));
}
