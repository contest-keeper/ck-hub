use std::fs;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{
    Download, Hub, Image, Judge, MemoryStore, ONLINE_WINDOW_SECONDS, PasswordCost, Registry, Role,
    StaticImages, router,
};
use ck_protocol::{
    Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, RecorderStatus, RecordingMode,
    SIGNATURE_HEADER, STICK_HEADER, StickId, Token,
};
use http_body_util::BodyExt;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

const TOKEN_A: &str = "token-of-stick-a";
const TOKEN_B: &str = "token-of-stick-b";
const ORGANIZER_PASSWORD: &str = "correct horse battery";

struct Setup {
    dir: TempDir,
    app: Router,
    now: Arc<Mutex<OffsetDateTime>>,
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
    setup_with(None)
}

fn setup_with(judge: Option<Arc<dyn Judge>>) -> Setup {
    build(judge, false)
}

fn build(judge: Option<Arc<dyn Judge>>, with_images: bool) -> Setup {
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
            .parse::<ck_protocol::Timestamp>()
            .expect("time")
            .as_offset_date_time(),
    ));
    let clock = {
        let now = Arc::clone(&now);
        Box::new(move || *now.lock().expect("clock"))
    };
    // Hashing is made cheap here: the tests log in many times.
    let hub = Hub::new(registry, Arc::new(MemoryStore::default()), Some(log), clock)
        .with_password_cost(PasswordCost {
            memory_kib: 8,
            passes: 1,
        });
    hub.create_user("owner@example.org", ORGANIZER_PASSWORD)
        .expect("an owner");
    hub.create_user("organizer@example.org", ORGANIZER_PASSWORD)
        .expect("an organizer");
    hub.create_organization("default", "Default", "owner@example.org")
        .expect("an organization");
    hub.add_member("default", "organizer@example.org", Role::Viewer)
        .expect("a viewer");
    let hub = match judge {
        Some(judge) => hub.with_judge(judge),
        None => hub,
    };
    let hub = if with_images {
        let download =
            Download::new("ckdlabc12", "downloads", "a2V5LWZvci10ZXN0cw==").expect("valid");
        hub.with_download(download)
            .with_images(Arc::new(StaticImages(vec![Image {
                id: "keeper-os-0.3.0".to_owned(),
                name: "Keeper OS".to_owned(),
                version: "0.3.0".to_owned(),
                blob: "images/keeper-os-0.3.0.img.zst".to_owned(),
                size: 1_500_000_000,
                sha256: "a".repeat(64),
                family: "ubuntu".to_owned(),
                profile: "generic".to_owned(),
                source_offer: Some("images/keeper-os-0.3.0.SOURCE-OFFER.txt".to_owned()),
            }])))
    } else {
        hub
    };
    let app = router(Arc::new(hub));
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

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, String) {
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
    call(app, request.body(Body::from(body)).expect("request")).await
}

/// A correctly signed heartbeat from stick A.
async fn heartbeat(app: &Router, boot: &str, seq: u64) -> (StatusCode, String) {
    let body = heartbeat_body("stick-a", boot, seq);
    let signature = sign(TOKEN_A, &body);
    send(app, Some("stick-a"), Some(&signature), body).await
}

/// Logs in as the organizer and returns the cookie to send back.
async fn organizer_cookie(app: &Router) -> String {
    let request = Request::post("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "email": "organizer@example.org", "password": ORGANIZER_PASSWORD })
                .to_string(),
        ))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let set = response.headers()["set-cookie"]
        .to_str()
        .expect("text")
        .to_owned();
    set.split(';').next().expect("cookie").to_owned()
}

async fn list(app: &Router) -> serde_json::Value {
    let cookie = organizer_cookie(app).await;
    let request = Request::get("/api/v1/sticks")
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("request");
    let (status, text) = call(app, request).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_str(&text).expect("json")
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
        page.contains("/api/v1/orgs/") && page.contains("/sticks"),
        "the page reads the stick list of an organization"
    );
    assert!(
        page.contains("/api/v1/register")
            && page.contains("/api/v1/verify-email")
            && page.contains("/api/v1/password-reset/confirm")
            && page.contains("location.hash"),
        "the page signs people up and handles the links in their mail"
    );
    assert!(
        page.contains("/api/v1/invitations/accept")
            && page.contains("/invitations")
            && page.contains("/members")
            && page.contains("/api/v1/staff/organizations")
            && page.contains("\"invite\""),
        "the page accepts invitations, lists people and lets staff make organizations"
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
    call(app, request).await
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
async fn network_events_make_a_history_with_sites_and_refused_requests() {
    let setup = setup();
    let entries = serde_json::json!([
        { "t": 100, "method": "GET", "host": "Codeforces.com", "path": "/enter", "status": 200,
          "result": "TCP_MISS", "bytes": 1000, "ms": 50, "type": "text/html" },
        { "t": 101, "method": "CONNECT", "host": "codeforces.com:443", "path": "", "status": 200,
          "result": "TCP_TUNNEL", "bytes": 500, "ms": 900, "type": "-" },
        { "t": 102, "method": "GET", "host": "docs.python.org", "path": "/3/", "status": 403,
          "result": "TCP_DENIED", "bytes": 3900, "ms": 0, "type": "text/html" },
        { "t": 103, "method": "GET", "host": "codeforces.com", "path": "/", "status": 200,
          "result": "TCP_HIT", "bytes": 2000, "ms": 5, "type": "text/html" },
        { "t": 104, "method": "GET", "host": "codeforces.com", "path": "/static/app.js", "status": 200,
          "result": "TCP_MISS", "bytes": 9000, "ms": 5, "type": "application/javascript" },
        { "t": 105, "method": "GET", "host": "firefox.settings.services.mozilla.com", "path": "/v1/",
          "status": 403, "result": "TCP_DENIED", "bytes": 100, "ms": 0, "type": "text/html" },
        { "t": 106, "method": "GET", "host": "docs.python.org", "path": "/3/", "status": 403,
          "result": "TCP_DENIED", "bytes": 3900, "ms": 0, "type": "text/html" },
        { "t": 107, "method": "GET", "host": "chat.example.org", "path": "/", "status": 403,
          "result": "NONE_NONE", "bytes": 3900, "ms": 0, "type": "text/html" },
        { "t": 109, "method": "GET", "host": "www.google.com", "path": "/complete/search", "status": 403,
          "result": "TCP_DENIED", "bytes": 100, "ms": 0, "type": "text/html" },
        { "t": 110, "method": "GET", "host": "update.googleapis.com", "path": "/service/update2/crx",
          "status": 403, "result": "TCP_DENIED", "bytes": 100, "ms": 0, "type": "text/html" },
        { "t": 108, "method": "CONNECT", "host": "chat.example.org:443", "path": "", "status": 200,
          "result": "TCP_DENIED", "bytes": 0, "ms": 0, "type": "-" }
    ]);
    let batch = Event {
        stick_id: StickId::parse("stick-a").expect("id"),
        boot_id: "boot-1234567890".to_owned(),
        seq: 1,
        prev_hash: FIRST_PREV_HASH.to_owned(),
        sent_at: "2026-11-12T10:05:00Z".parse().expect("time"),
        kind: EventKind::Network,
        payload: serde_json::json!({ "entries": entries, "cookie": "must never be shown" }),
    }
    .canonical_bytes();
    let request = Request::post("/api/v1/events")
        .header(STICK_HEADER, "stick-a")
        .header(SIGNATURE_HEADER, sign(TOKEN_A, &batch))
        .body(Body::from(batch))
        .expect("request");
    assert_eq!(call(&setup.app, request).await.0, StatusCode::OK);

    // Shown: the two pages, the refused request to docs.python.org twice, and the refused one to
    // chat.example.org (its refused tunnel says the same). Hidden: the tunnel, the script and the
    // browser's own request.
    let sticks = list(&setup.app).await;
    assert_eq!(sticks[0]["network_requests"], 5);
    assert_eq!(sticks[0]["blocked_requests"], 3);
    assert_eq!(sticks[1]["network_requests"], 0);

    let cookie = organizer_cookie(&setup.app).await;
    let request = Request::get("/api/v1/sticks/stick-a/network")
        .header("cookie", cookie.clone())
        .body(Body::empty())
        .expect("request");
    let (status, text) = call(&setup.app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !text.contains("never be shown"),
        "only named fields are passed on"
    );
    let history: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(history["requests"], 5);
    assert_eq!(history["hidden"], 6);
    assert_eq!(history["blocked"], 3);
    assert_eq!(history["bytes"], 1000 + 2000 + 3900 + 3900 + 3900);
    let entries = history["entries"].as_array().expect("entries");
    // The same refused request is one line with a count; newest first.
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0]["host"], "chat.example.org");
    assert_eq!(
        entries[0]["blocked"], true,
        "the proxy's error page is a refusal"
    );
    assert_eq!(entries[1]["host"], "docs.python.org");
    assert_eq!(entries[1]["count"], 2);
    assert_eq!(entries[2]["path"], "/");
    assert_eq!(entries[3]["path"], "/enter");
    assert_eq!(entries[3]["boot"], "boot-123");
    assert!(
        !text.contains("mozilla"),
        "the browser's background traffic is left out"
    );
    // The busiest site first; case and the port do not make a site another one.
    assert_eq!(history["hosts"][0]["host"], "codeforces.com");
    assert_eq!(history["hosts"][0]["requests"], 2);
    assert_eq!(history["hosts"][1]["host"], "docs.python.org");
    assert_eq!(history["hosts"][1]["blocked"], 2);

    let request = Request::get("/api/v1/sticks/stick-b/network")
        .header("cookie", cookie.clone())
        .body(Body::empty())
        .expect("request");
    let (_, text) = call(&setup.app, request).await;
    let empty: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(empty["requests"], 0);

    let request = Request::get("/api/v1/sticks/not%20an%20id/network")
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&setup.app, request).await.0, StatusCode::NOT_FOUND);

    let request = Request::get("/api/v1/sticks/stick-a/network")
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&setup.app, request).await.0, StatusCode::UNAUTHORIZED);
}

struct FakeJudge;

impl Judge for FakeJudge {
    fn submissions_of(
        &self,
        contest_id: u64,
        handle: &str,
    ) -> Result<Vec<ck_hub::JudgeSubmission>, ck_hub::CodeforcesError> {
        assert_eq!((contest_id, handle), (2285, "ana"));
        let at = |text: &str| {
            text.parse::<ck_protocol::Timestamp>()
                .expect("time")
                .as_offset_date_time()
        };
        let row = |id, problem: &str, when: &str| ck_hub::JudgeSubmission {
            submission_id: id,
            contest_id: 2285,
            handles: vec!["Ana".to_owned()],
            problem_index: problem.to_owned(),
            submitted_at: at(when),
        };
        Ok(vec![
            row(900, "C", "2026-11-12T10:20:00Z"),
            row(800, "A", "2026-11-12T10:05:10Z"),
        ])
    }
}

async fn send_kind(
    app: &Router,
    seq: u64,
    kind: EventKind,
    payload: serde_json::Value,
) -> StatusCode {
    let body = Event {
        stick_id: StickId::parse("stick-a").expect("id"),
        boot_id: "boot-1234567890".to_owned(),
        seq,
        prev_hash: String::new(),
        sent_at: "2026-11-12T10:05:00Z".parse().expect("time"),
        kind,
        payload,
    }
    .canonical_bytes();
    let request = Request::post("/api/v1/events")
        .header(STICK_HEADER, "stick-a")
        .header(SIGNATURE_HEADER, sign(TOKEN_A, &body))
        .body(Body::from(body))
        .expect("request");
    call(app, request).await.0
}

#[tokio::test]
async fn submissions_are_compared_with_the_judge_and_the_unreported_one_is_a_suspect() {
    let setup = setup_with(Some(Arc::new(FakeJudge)));
    // The chain is not checked here: only that the events are read.
    let login = serde_json::json!({"platform": "codeforces", "username": "ana"});
    let submit = serde_json::json!({"platform": "codeforces", "username": "ana",
        "contest_id": "2285", "problem": "A", "language": "91", "accepted": true});
    send_kind(&setup.app, 1, EventKind::Login, login).await;
    send_kind(&setup.app, 2, EventKind::Submission, submit).await;

    let cookie = organizer_cookie(&setup.app).await;
    let request = Request::get("/api/v1/sticks/stick-a/submissions")
        .header("cookie", cookie.clone())
        .body(Body::empty())
        .expect("request");
    let (status, text) = call(&setup.app, request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let seen: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(seen["suspect"], 1);
    assert_eq!(seen["submissions"][0]["judge_id"], 800);
    assert_eq!(seen["judge"][0]["submission_id"], 900);
    assert_eq!(seen["judge"][0]["suspect"], true);
    assert_eq!(seen["judge"][1]["from_stick"], true);

    let request = Request::get("/api/v1/sticks/stick-b/submissions")
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("request");
    let (_, text) = call(&setup.app, request).await;
    let none: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(none["suspect"], 0);

    let request = Request::get("/api/v1/sticks/stick-a/submissions")
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&setup.app, request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn without_a_judge_the_submissions_say_nothing_is_compared() {
    let setup = setup();
    let cookie = organizer_cookie(&setup.app).await;
    let request = Request::get("/api/v1/sticks/stick-a/submissions")
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("request");
    let (_, text) = call(&setup.app, request).await;
    let seen: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert!(
        seen["judge_note"]
            .as_str()
            .expect("note")
            .contains("not set up")
    );
}

#[tokio::test]
async fn a_submission_event_is_counted_and_its_problem_and_language_are_shown() {
    let setup = setup();
    assert_eq!(
        send_event(&setup.app, "b1", 1, FIRST_PREV_HASH, "ana")
            .await
            .0,
        StatusCode::OK
    );
    let h1 = next_hash("b1", 1, FIRST_PREV_HASH, "ana");
    let submission = Event {
        stick_id: StickId::parse("stick-a").expect("id"),
        boot_id: "b1".to_owned(),
        seq: 2,
        prev_hash: h1,
        sent_at: "2026-11-12T10:05:00Z".parse().expect("time"),
        kind: EventKind::Submission,
        payload: serde_json::json!({
            "platform": "codeforces", "problem": "1A", "language": "C++20", "submission_id": 98765,
            "code_sha256": "ab", "password": "must never be shown"
        }),
    }
    .canonical_bytes();
    let first_hash = ck_crypto::sha256_hex(&submission);
    let request = Request::post("/api/v1/events")
        .header(STICK_HEADER, "stick-a")
        .header(SIGNATURE_HEADER, sign(TOKEN_A, &submission))
        .body(Body::from(submission))
        .expect("request");
    assert_eq!(call(&setup.app, request).await.0, StatusCode::OK);

    // The site first asked for a check, so the stick saw this one refused: it is not a submission.
    let refused = Event {
        stick_id: StickId::parse("stick-a").expect("id"),
        boot_id: "b1".to_owned(),
        seq: 3,
        prev_hash: first_hash,
        sent_at: "2026-11-12T10:05:30Z".parse().expect("time"),
        kind: EventKind::Submission,
        payload: serde_json::json!({ "platform": "codeforces", "problem": "1B", "accepted": false }),
    }
    .canonical_bytes();
    let request = Request::post("/api/v1/events")
        .header(STICK_HEADER, "stick-a")
        .header(SIGNATURE_HEADER, sign(TOKEN_A, &refused))
        .body(Body::from(refused))
        .expect("request");
    assert_eq!(call(&setup.app, request).await.0, StatusCode::OK);

    let sticks = list(&setup.app).await;
    assert_eq!(sticks[0]["submissions"], 1);
    assert_eq!(
        sticks[0]["current_user"], "ana",
        "a submission is not a login"
    );
    let last = &sticks[0]["last_submission"];
    assert_eq!(last["problem"], "1A");
    assert_eq!(last["language"], "C++20");
    assert_eq!(last["submission_id"], "98765");
    assert!(
        !last.to_string().contains("never be shown"),
        "only named fields are passed on"
    );
    assert_eq!(sticks[1]["submissions"], 0);
    assert!(sticks[1]["last_submission"].is_null());
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

/// A script that does not parse leaves the page blank, with nothing to say why, and a Rust test
/// cannot run a browser. Node can at least read the script, so this checks it when Node is there.
#[tokio::test]
async fn the_page_script_is_valid_javascript() {
    let setup = setup();
    let request = Request::get("/").body(Body::empty()).expect("request");
    let (_, page) = call(&setup.app, request).await;
    let start = page.find("<script>").expect("a script") + "<script>".len();
    let end = page.find("</script>").expect("the end of the script");

    let file = setup.dir.path().join("page.js");
    fs::write(&file, &page[start..end]).expect("write the script");
    let Ok(output) = std::process::Command::new("node")
        .arg("--check")
        .arg(&file)
        .output()
    else {
        eprintln!("skipped: Node is not installed, so the page script was not parsed");
        return;
    };
    assert!(
        output.status.success(),
        "the page script does not parse:
{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn a_recorded_heartbeat_of_an_older_boot_cannot_be_replayed() {
    let setup = setup();
    assert_eq!(heartbeat(&setup.app, "boot-1", 1).await.0, StatusCode::OK);
    assert_eq!(heartbeat(&setup.app, "boot-2", 1).await.0, StatusCode::OK);
    assert_eq!(
        heartbeat(&setup.app, "boot-1", 1).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn a_stick_that_sends_too_much_is_slowed_down_and_may_send_again_next_minute() {
    let setup = setup();
    for seq in 1..=120 {
        assert_eq!(heartbeat(&setup.app, "b", seq).await.0, StatusCode::OK);
    }
    assert_eq!(
        heartbeat(&setup.app, "b", 121).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    *setup.now.lock().expect("clock") += Duration::seconds(61);
    assert_eq!(heartbeat(&setup.app, "b", 121).await.0, StatusCode::OK);
}

#[tokio::test]
async fn values_the_database_could_not_keep_are_refused_once() {
    let setup = setup();
    let body = heartbeat_body("stick-a", "boot", u64::MAX - 100);
    let signature = sign(TOKEN_A, &body);
    let (status, _) = send(&setup.app, Some("stick-a"), Some(&signature), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let text = String::from_utf8(heartbeat_body("stick-a", "boot", 1)).expect("text");
    let body = text.replace("\"boot\"", "\"bo\\u0000ot\"").into_bytes();
    let signature = sign(TOKEN_A, &body);
    let (status, _) = send(&setup.app, Some("stick-a"), Some(&signature), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn every_answer_forbids_framing_and_outside_resources() {
    let setup = setup();
    let response = setup
        .app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let policy = response.headers()["content-security-policy"]
        .to_str()
        .expect("text");
    assert!(policy.contains("frame-ancestors 'none'") && policy.contains("default-src 'none'"));
}

#[tokio::test]
async fn the_forge_download_is_not_found_until_a_container_is_set() {
    let setup = setup();
    let response = setup
        .app
        .oneshot(
            Request::get("/download/forge")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_forge_download_sends_the_visitor_to_a_short_lived_link() {
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("registry.json"), registry_json(&[])).expect("registry");
    let registry = Registry::open(dir.path().join("registry.json")).expect("open");
    let download = Download::new("ckdlabc12", "downloads", "a2V5LWZvci10ZXN0cw==").expect("valid");
    let hub = Hub::new(
        registry,
        Arc::new(MemoryStore::default()),
        None,
        Box::new(|| {
            "2026-11-14T12:00:00Z"
                .parse::<ck_protocol::Timestamp>()
                .expect("a time")
                .as_offset_date_time()
        }),
    )
    .with_download(download);
    let response = router(Arc::new(hub))
        .oneshot(
            Request::get("/download/forge")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FOUND);
    let location = response.headers()["location"].to_str().expect("text");
    assert!(location.starts_with(
        "https://ckdlabc12.blob.core.windows.net/downloads/forge/keeper-forge-setup-x64.exe?sv="
    ));
    assert!(location.contains("&se=2026-11-14T12%3A05%3A00Z&"));
    assert_eq!(response.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn the_image_list_and_download_need_a_login() {
    let setup = build(None, true);
    for path in [
        "/api/v1/images",
        "/download/image/keeper-os-0.3.0",
        "/download/image/keeper-os-0.3.0/source-offer",
    ] {
        let (status, _) = call(
            &setup.app,
            Request::get(path).body(Body::empty()).expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn images_are_not_found_until_they_are_set_up() {
    let setup = setup();
    let cookie = organizer_cookie(&setup.app).await;
    for path in ["/api/v1/images", "/download/image/keeper-os-0.3.0"] {
        let (status, _) = call(
            &setup.app,
            Request::get(path)
                .header("cookie", &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn an_organizer_sees_the_manifest_without_the_storage_path() {
    let setup = build(None, true);
    let cookie = organizer_cookie(&setup.app).await;
    let (status, text) = call(
        &setup.app,
        Request::get("/api/v1/images")
            .header("cookie", cookie)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    let image = &json["images"][0];
    assert_eq!(image["id"], "keeper-os-0.3.0");
    assert_eq!(image["url"], "/download/image/keeper-os-0.3.0");
    assert_eq!(image["size"], 1_500_000_000u64);
    assert_eq!(image["sha256"], "a".repeat(64));
    assert_eq!(image["family"], "ubuntu");
    assert_eq!(image["profile"], "generic");
    assert_eq!(
        image["source_offer_url"],
        "/download/image/keeper-os-0.3.0/source-offer"
    );
    assert!(!text.contains("blob") && !text.contains("images/keeper"));
}

#[tokio::test]
async fn an_organizer_is_sent_to_a_five_minute_link_and_a_wrong_id_is_refused() {
    let setup = build(None, true);
    let cookie = organizer_cookie(&setup.app).await;
    let response = setup
        .app
        .clone()
        .oneshot(
            Request::get("/download/image/keeper-os-0.3.0")
                .header("cookie", &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FOUND);
    let location = response.headers()["location"].to_str().expect("text");
    assert!(location.starts_with(
        "https://ckdlabc12.blob.core.windows.net/downloads/images/keeper-os-0.3.0.img.zst?sv="
    ));
    // The test clock reads 10:00:00, so the link ends at 10:05:00.
    assert!(
        location.contains("&se=2026-11-12T10%3A05%3A00Z&"),
        "{location}"
    );
    assert_eq!(response.headers()["cache-control"], "no-store");

    for id in ["nope", "..%2Fsecret", "keeper-os-0.3.0%2F..%2Fx"] {
        let (status, _) = call(
            &setup.app,
            Request::get(format!("/download/image/{id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
    }
}

#[tokio::test]
async fn the_source_offer_of_an_image_is_served_through_a_short_link_to_organizers() {
    let setup = build(None, true);
    let cookie = organizer_cookie(&setup.app).await;
    let response = setup
        .app
        .clone()
        .oneshot(
            Request::get("/download/image/keeper-os-0.3.0/source-offer")
                .header("cookie", &cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FOUND);
    let location = response.headers()["location"].to_str().expect("text");
    assert!(
        location.starts_with(
            "https://ckdlabc12.blob.core.windows.net/downloads/images/keeper-os-0.3.0.SOURCE-OFFER.txt?sv="
        ),
        "{location}"
    );
    let (status, _) = call(
        &setup.app,
        Request::get("/download/image/nope/source-offer")
            .header("cookie", &cookie)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_page_has_the_command_buttons_with_a_confirm_step() {
    let setup = setup();
    let (status, page) = call(
        &setup.app,
        Request::get("/").body(Body::empty()).expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for wanted in [
        "commandPanel",
        "/commands",
        "Message",
        "Lock",
        "Shutdown",
        "Confirm",
        "Cancel",
    ] {
        assert!(page.contains(wanted), "{wanted}");
    }
    // Only admins and owners see the buttons, and nothing is put into the page as markup.
    assert!(page.contains("myRole === \"owner\" || myRole === \"admin\""));
    assert!(!page.contains("innerHTML"));
}
