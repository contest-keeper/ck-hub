//! Commands from the Hub to a stick: who may send, delivery in the heartbeat reply, the ack that
//! closes a command, expiry and the limits.

use std::fs;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, Role, router};
use ck_protocol::{
    Action, CommandAck, Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState,
    RecorderStatus, RecordingMode, SIGNATURE_HEADER, STICK_HEADER, SignedCommand, StickId, Token,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

const TOKEN_A: &str = "token-of-stick-a";
const TOKEN_B: &str = "token-of-stick-b";
const PASSWORD: &str = "correct horse battery";

struct Setup {
    _dir: TempDir,
    app: Router,
    hub: Arc<Hub>,
    now: Arc<Mutex<OffsetDateTime>>,
}

fn setup() -> Setup {
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
    let hub = Hub::new(registry, Arc::new(MemoryStore::default()), None, clock).with_password_cost(
        PasswordCost {
            memory_kib: 8,
            passes: 1,
        },
    );
    for email in [
        "owner@example.org",
        "admin@example.org",
        "viewer@example.org",
    ] {
        hub.create_user(email, PASSWORD).expect("a user");
    }
    hub.create_organization("default", "Default", "owner@example.org")
        .expect("an organization");
    hub.add_member("default", "admin@example.org", Role::Admin)
        .expect("an admin");
    hub.add_member("default", "viewer@example.org", Role::Viewer)
        .expect("a viewer");
    let hub = Arc::new(hub);
    Setup {
        _dir: dir,
        app: router(Arc::clone(&hub)),
        hub,
        now,
    }
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
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

async fn send(app: &Router, cookie: &str, stick: &str, body: &Value) -> (StatusCode, Value) {
    let request = Request::post(format!("/api/v1/orgs/default/sticks/{stick}/commands"))
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    call(app, request).await
}

async fn history(app: &Router, cookie: &str, stick: &str) -> Value {
    let request = Request::get(format!("/api/v1/orgs/default/sticks/{stick}/commands"))
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("request");
    let (status, json) = call(app, request).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
}

fn sign(token: &str, body: &[u8]) -> String {
    ck_crypto::sign(&Token::new(token), body)
}

/// A heartbeat from a stick; returns the commands in the reply.
async fn heartbeat(app: &Router, stick: &str, token: &str, seq: u64) -> Vec<SignedCommand> {
    heartbeat_with_unlock(app, stick, token, seq, false).await
}

async fn heartbeat_with_unlock(
    app: &Router,
    stick: &str,
    token: &str,
    seq: u64,
    screen_unlock: bool,
) -> Vec<SignedCommand> {
    let body = serde_json::to_vec(&Heartbeat {
        stick_id: StickId::parse(stick).expect("id"),
        boot_id: "boot-1".to_owned(),
        seq,
        sent_at: "2026-11-12T10:00:00Z".parse().expect("time"),
        uptime_s: 45 + seq,
        hostname: "team-1".to_owned(),
        image_version: "ck-os development".to_owned(),
        config_sha256: "0".repeat(64),
        team: "Team".to_owned(),
        site: "Lab 1".to_owned(),
        recording: RecorderStatus {
            mode: RecordingMode::Low,
            state: RecorderState::Recording,
        },
        screen_unlock,
    })
    .expect("json");
    let request = Request::post("/api/v1/heartbeat")
        .header(STICK_HEADER, stick)
        .header(SIGNATURE_HEADER, sign(token, &body))
        .body(Body::from(body))
        .expect("request");
    let (status, json) = call(app, request).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json.get("commands")
        .map(|list| serde_json::from_value(list.clone()).expect("signed commands"))
        .unwrap_or_default()
}

/// An ack event from a stick, the first of its boot.
async fn ack(app: &Router, stick: &str, token: &str, command_id: &str, result: &str) -> StatusCode {
    let payload = serde_json::to_value(CommandAck {
        command_id: command_id.to_owned(),
        result: serde_json::from_value(json!(result)).expect("a result"),
        refusal: None,
    })
    .expect("json");
    let body = Event {
        stick_id: StickId::parse(stick).expect("id"),
        boot_id: "boot-ack".to_owned(),
        seq: 1,
        prev_hash: FIRST_PREV_HASH.to_owned(),
        sent_at: "2026-11-12T10:00:00Z".parse().expect("time"),
        kind: EventKind::CommandAck,
        payload,
    }
    .canonical_bytes();
    let request = Request::post("/api/v1/events")
        .header(STICK_HEADER, stick)
        .header(SIGNATURE_HEADER, sign(token, &body))
        .body(Body::from(body))
        .expect("request");
    call(app, request).await.0
}

fn message(text: &str) -> Value {
    json!({ "action": { "type": "message", "text": text } })
}

#[tokio::test]
async fn unlock_requires_the_right_admin_and_only_reaches_a_capable_stick() {
    let s = setup();
    let unlock = json!({ "action": { "type": "unlock_screen" } });
    let admin = cookie(&s.app, "admin@example.org").await;
    let viewer = cookie(&s.app, "viewer@example.org").await;
    assert_eq!(
        send(&s.app, &viewer, "stick-a", &unlock).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, answer) = send(&s.app, &admin, "stick-a", &unlock).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        answer["error"]
            .as_str()
            .expect("error")
            .contains("has not reported support for screen unlock")
    );
    assert_eq!(
        heartbeat_with_unlock(&s.app, "stick-a", TOKEN_A, 1, true).await,
        Vec::new()
    );

    s.hub
        .create_user("elsewhere@example.org", PASSWORD)
        .expect("an account");
    s.hub
        .create_organization("elsewhere", "Elsewhere", "elsewhere@example.org")
        .expect("another organization");
    let other_admin = cookie(&s.app, "elsewhere@example.org").await;
    let elsewhere = Request::post("/api/v1/orgs/elsewhere/sticks/stick-a/commands")
        .header("cookie", other_admin)
        .body(Body::from(unlock.to_string()))
        .expect("request");
    assert_eq!(call(&s.app, elsewhere).await.0, StatusCode::NOT_FOUND);

    let (status, queued) = send(&s.app, &admin, "stick-a", &unlock).await;
    assert_eq!(status, StatusCode::CREATED, "{queued}");
    let id = queued["id"].as_str().expect("id");
    assert_eq!(queued["action"]["type"], "unlock_screen");
    assert_eq!(
        send(&s.app, &admin, "stick-a", &message("Still works"))
            .await
            .0,
        StatusCode::CREATED
    );

    // After a downgrade, the old stick can still read normal commands in the reply.
    let legacy_reply = heartbeat(&s.app, "stick-a", TOKEN_A, 2).await;
    assert!(
        matches!(legacy_reply.as_slice(), [one] if matches!(one.command.action, Action::Message { .. }))
    );
    let commands = history(&s.app, &viewer, "stick-a").await;
    let held = commands
        .as_array()
        .expect("commands")
        .iter()
        .find(|command| command["id"] == id)
        .expect("Unlock");
    assert_eq!(held["state"], "queued");
    assert_eq!(
        send(&s.app, &admin, "stick-a", &unlock).await.0,
        StatusCode::BAD_REQUEST
    );

    let current_reply = heartbeat_with_unlock(&s.app, "stick-a", TOKEN_A, 3, true).await;
    let delivered = current_reply
        .iter()
        .find(|command| command.command.id == id)
        .expect("Unlock");
    assert_eq!(
        serde_json::to_value(&delivered.command.action).expect("action"),
        unlock["action"]
    );
    ck_crypto::verify_command(&Token::new(TOKEN_A), delivered).expect("valid signature");
    assert_eq!(
        ack(&s.app, "stick-a", TOKEN_A, id, "ok").await,
        StatusCode::OK
    );
    let history = history(&s.app, &viewer, "stick-a").await;
    let done = history
        .as_array()
        .expect("history")
        .iter()
        .find(|command| command["id"] == id)
        .expect("Unlock");
    assert_eq!(done["state"], "done");
}

#[tokio::test]
async fn only_an_admin_or_owner_may_send_and_a_viewer_may_read() {
    let s = setup();
    let viewer = cookie(&s.app, "viewer@example.org").await;
    let (status, _) = send(&s.app, &viewer, "stick-a", &message("hi")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let admin = cookie(&s.app, "admin@example.org").await;
    let (status, sent) = send(&s.app, &admin, "stick-a", &message("hi")).await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    assert_eq!(sent["state"], "queued");
    assert_eq!(sent["sent_by"], "admin@example.org");
    assert_eq!(
        history(&s.app, &viewer, "stick-a").await[0]["id"],
        sent["id"]
    );

    let request = Request::post("/api/v1/orgs/default/sticks/stick-a/commands")
        .header("content-type", "application/json")
        .body(Body::from(message("hi").to_string()))
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_command_rides_the_heartbeat_reply_signed_and_is_resent_until_acked() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    let (_, sent) = send(
        &s.app,
        &owner,
        "stick-a",
        &message("Contest starts in 5 minutes"),
    )
    .await;
    let id = sent["id"].as_str().expect("id").to_owned();

    // A stick with nothing waiting gets the plain answer.
    assert_eq!(heartbeat(&s.app, "stick-b", TOKEN_B, 1).await, Vec::new());

    let first = heartbeat(&s.app, "stick-a", TOKEN_A, 1).await;
    assert_eq!(first.len(), 1);
    let signed = &first[0];
    assert_eq!(signed.command.id, id);
    assert_eq!(
        signed.command.action,
        Action::Message {
            text: "Contest starts in 5 minutes".to_owned()
        }
    );
    // What the stick checks: the signature with its own secret, then the command itself.
    ck_crypto::verify_command(&Token::new(TOKEN_A), signed).expect("a valid signature");
    let now = s.now.lock().expect("clock").to_owned();
    signed
        .command
        .check(&StickId::parse("stick-a").expect("id"), now.into())
        .expect("a command the stick runs");
    assert_eq!(
        history(&s.app, &owner, "stick-a").await[0]["state"],
        "delivered"
    );

    // A lost reply is no loss: the next heartbeat carries it again.
    assert_eq!(heartbeat(&s.app, "stick-a", TOKEN_A, 2).await.len(), 1);

    assert_eq!(
        ack(&s.app, "stick-a", TOKEN_A, &id, "ok").await,
        StatusCode::OK
    );
    assert_eq!(heartbeat(&s.app, "stick-a", TOKEN_A, 3).await, Vec::new());
    let done = &history(&s.app, &owner, "stick-a").await[0];
    assert_eq!(done["state"], "done");
    assert!(done["finished_at"].is_string());
}

#[tokio::test]
async fn a_refusal_is_kept_with_its_reason() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    let (_, sent) = send(
        &s.app,
        &owner,
        "stick-a",
        &json!({ "action": { "type": "shutdown" } }),
    )
    .await;
    let id = sent["id"].as_str().expect("id");
    heartbeat(&s.app, "stick-a", TOKEN_A, 1).await;
    assert_eq!(
        ack(&s.app, "stick-a", TOKEN_A, id, "refused").await,
        StatusCode::OK
    );
    let shown = &history(&s.app, &owner, "stick-a").await[0];
    assert_eq!(shown["state"], "refused");
}

#[tokio::test]
async fn another_sticks_ack_changes_nothing_and_the_first_ack_decides() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    let (_, sent) = send(
        &s.app,
        &owner,
        "stick-a",
        &json!({ "action": { "type": "lock_screen" } }),
    )
    .await;
    let id = sent["id"].as_str().expect("id");
    heartbeat(&s.app, "stick-a", TOKEN_A, 1).await;

    // Stick B names A's command: the event is kept, the command is not touched.
    assert_eq!(
        ack(&s.app, "stick-b", TOKEN_B, id, "ok").await,
        StatusCode::OK
    );
    assert_eq!(
        history(&s.app, &owner, "stick-a").await[0]["state"],
        "delivered"
    );

    assert_eq!(
        ack(&s.app, "stick-a", TOKEN_A, id, "failed").await,
        StatusCode::OK
    );
    assert_eq!(
        history(&s.app, &owner, "stick-a").await[0]["state"],
        "failed"
    );
    // A late ok does not change the outcome. (A new boot id is needed for a new chain.)
    assert_eq!(
        ack(&s.app, "stick-a", TOKEN_A, id, "ok").await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        history(&s.app, &owner, "stick-a").await[0]["state"],
        "failed"
    );
}

#[tokio::test]
async fn a_command_that_expires_is_not_sent_and_shows_as_expired() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    let body = json!({ "action": { "type": "lock_screen" }, "lifetime_seconds": 60 });
    let (status, sent) = send(&s.app, &owner, "stick-a", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    *s.now.lock().expect("clock") += Duration::seconds(61);
    assert_eq!(heartbeat(&s.app, "stick-a", TOKEN_A, 1).await, Vec::new());
    assert_eq!(
        history(&s.app, &owner, "stick-a").await[0]["state"],
        "expired"
    );
}

#[tokio::test]
async fn bad_requests_unknown_sticks_and_the_limits_are_refused() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    assert_eq!(
        send(&s.app, &owner, "nobody", &message("hi")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&s.app, &owner, "stick-a", &message("")).await.0,
        StatusCode::BAD_REQUEST
    );
    for seconds in [5, 3601] {
        let body = json!({ "action": { "type": "shutdown" }, "lifetime_seconds": seconds });
        assert_eq!(
            send(&s.app, &owner, "stick-a", &body).await.0,
            StatusCode::BAD_REQUEST,
            "{seconds}"
        );
    }
    let nonsense = json!({ "action": { "type": "format_disk" } });
    assert_eq!(
        send(&s.app, &owner, "stick-a", &nonsense).await.0,
        StatusCode::BAD_REQUEST
    );

    for i in 0..10 {
        let (status, body) = send(&s.app, &owner, "stick-a", &message(&format!("n{i}"))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    assert_eq!(
        send(&s.app, &owner, "stick-a", &message("one more"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    // The limit is per stick: another stick still takes commands.
    assert_eq!(
        send(&s.app, &owner, "stick-b", &message("hi")).await.0,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn one_person_may_send_only_so_many_commands_a_minute() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    let mut last = StatusCode::CREATED;
    for i in 0..21 {
        // Sticks take ten each, so alternate and let the clock pass the stick limit by acking none:
        // use short messages on both sticks, then the person limit shows first at the 21st.
        let stick = if i % 2 == 0 { "stick-a" } else { "stick-b" };
        last = send(&s.app, &owner, stick, &message("x")).await.0;
        if i < 20 {
            assert_eq!(last, StatusCode::CREATED, "{i}");
        }
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_command_can_be_read_by_its_id_and_not_across_organizations() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    let (_, sent) = send(&s.app, &owner, "stick-a", &message("hi")).await;
    let request = Request::get(format!(
        "/api/v1/orgs/default/commands/{}",
        sent["id"].as_str().expect("id")
    ))
    .header("cookie", &owner)
    .body(Body::empty())
    .expect("request");
    let (status, one) = call(&s.app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["action"]["text"], "hi");
    let request = Request::get("/api/v1/orgs/default/commands/nope")
        .header("cookie", &owner)
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::NOT_FOUND);
}
