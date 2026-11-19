//! One-time enrolment codes: who may make one, what a stick gets for it, and that it works once.

use std::fs;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, Role, router};
use ck_protocol::{
    Heartbeat, RecorderState, RecorderStatus, RecordingMode, SIGNATURE_HEADER, STICK_HEADER,
    StickId, Token,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

struct Setup {
    _dir: TempDir,
    app: Router,
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
    Setup {
        _dir: dir,
        app: router(Arc::new(hub)),
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

async fn mint(app: &Router, cookie: &str, body: &Value) -> (StatusCode, Value) {
    let request = Request::post("/api/v1/orgs/default/enrolment-codes")
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    call(app, request).await
}

async fn enrol(app: &Router, code: &str) -> (StatusCode, Value) {
    let request = Request::post("/api/v1/enrol")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "code": code }).to_string()))
        .expect("request");
    call(app, request).await
}

#[tokio::test]
async fn a_redeemed_key_keeps_its_contest_window() {
    let setup = setup();
    let admin = cookie(&setup.app, "admin@example.org").await;
    let created = *setup.now.lock().expect("clock");
    let valid_from = created + Duration::minutes(5);
    let valid_until = created + Duration::minutes(10);
    let scope = json!({
        "contest_id": "2042",
        "valid_from": valid_from.format(&time::format_description::well_known::Rfc3339).expect("time"),
        "valid_until": valid_until.format(&time::format_description::well_known::Rfc3339).expect("time")
    });
    let (status, minted) = mint(&setup.app, &admin, &json!({"credential_scope": scope})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, enrolled) = enrol(&setup.app, minted["code"].as_str().expect("code")).await;
    assert_eq!(status, StatusCode::OK);
    let stick = enrolled["stick_id"].as_str().expect("stick ID");
    let token = enrolled["token"].as_str().expect("token");
    assert_eq!(
        heartbeat(&setup.app, stick, token).await,
        StatusCode::UNAUTHORIZED
    );
    *setup.now.lock().expect("clock") = valid_from;
    assert_eq!(heartbeat(&setup.app, stick, token).await, StatusCode::OK);
    *setup.now.lock().expect("clock") = valid_until;
    assert_eq!(
        heartbeat(&setup.app, stick, token).await,
        StatusCode::UNAUTHORIZED
    );
}

/// A heartbeat signed with `token`; returns the status.
async fn heartbeat(app: &Router, stick: &str, token: &str) -> StatusCode {
    let body = serde_json::to_vec(&Heartbeat {
        stick_id: StickId::parse(stick).expect("id"),
        boot_id: "boot-1".to_owned(),
        seq: 1,
        sent_at: "2026-11-12T10:00:00Z".parse().expect("time"),
        uptime_s: 46,
        hostname: "team-1".to_owned(),
        image_version: "ck-os development".to_owned(),
        config_sha256: "0".repeat(64),
        team: "Team".to_owned(),
        site: "Lab 1".to_owned(),
        recording: RecorderStatus {
            mode: RecordingMode::Low,
            state: RecorderState::Recording,
        },
    })
    .expect("json");
    let request = Request::post("/api/v1/heartbeat")
        .header(STICK_HEADER, stick)
        .header(SIGNATURE_HEADER, ck_crypto::sign(&Token::new(token), &body))
        .body(Body::from(body))
        .expect("request");
    call(app, request).await.0
}

#[tokio::test]
async fn only_an_admin_or_owner_may_make_a_code() {
    let s = setup();
    let viewer = cookie(&s.app, "viewer@example.org").await;
    assert_eq!(
        mint(&s.app, &viewer, &json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    let request = Request::post("/api/v1/orgs/default/enrolment-codes")
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::UNAUTHORIZED);
    let admin = cookie(&s.app, "admin@example.org").await;
    let (status, minted) = mint(&s.app, &admin, &json!({ "team": "Team A" })).await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    let code = minted["code"].as_str().expect("code");
    assert_eq!(code.len(), 24);
    assert_eq!(code.matches('-').count(), 4);
    assert!(minted["expires_at"].is_string());
}

#[tokio::test]
async fn a_stick_trades_the_code_once_and_the_secret_it_gets_works() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    let spec =
        json!({ "team": "Team Z", "site": "Lab 9", "contest": "Final", "machine_number": 7 });
    let (_, minted) = mint(&s.app, &owner, &spec).await;
    let code = minted["code"].as_str().expect("code").to_owned();

    // Typed in small letters and without dashes, as a person might.
    let typed = code.replace('-', "").to_lowercase();
    let (status, got) = enrol(&s.app, &typed).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    let stick = got["stick_id"].as_str().expect("id");
    let token = got["token"].as_str().expect("token");
    assert_eq!(stick.len(), 8);
    assert!(token.len() >= 32);
    assert_eq!(
        (
            got["team"].as_str(),
            got["site"].as_str(),
            got["contest"].as_str(),
            got["machine_number"].as_u64()
        ),
        (Some("Team Z"), Some("Lab 9"), Some("Final"), Some(7))
    );

    // The stick is now known to the organization and its secret signs heartbeats.
    assert_eq!(heartbeat(&s.app, stick, token).await, StatusCode::OK);
    let request = Request::get("/api/v1/orgs/default/sticks")
        .header("cookie", &owner)
        .body(Body::empty())
        .expect("request");
    let (_, list) = call(&s.app, request).await;
    assert!(
        list.as_array()
            .expect("list")
            .iter()
            .any(|row| row["stick_id"] == stick && row["team"] == "Team Z")
    );

    // The same code again, in any spelling, gets nothing.
    for again in [code.as_str(), typed.as_str()] {
        let (status, refused) = enrol(&s.app, again).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");
        assert!(refused.get("token").is_none());
    }
}

#[tokio::test]
async fn a_code_that_ran_out_or_never_existed_gets_the_same_answer() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    let (_, minted) = mint(&s.app, &owner, &json!({ "lifetime_minutes": 5 })).await;
    let code = minted["code"].as_str().expect("code").to_owned();
    *s.now.lock().expect("clock") += Duration::minutes(6);
    let expired = enrol(&s.app, &code).await;
    let unknown = enrol(&s.app, "AAAA-BBBB-CCCC-DDDD-EEEE").await;
    let nonsense = enrol(&s.app, "hello").await;
    assert_eq!(expired.0, StatusCode::UNAUTHORIZED);
    assert_eq!(expired, unknown);
    assert_eq!(unknown, nonsense);
}

#[tokio::test]
async fn a_flood_of_wrong_codes_is_slowed_down_for_everyone() {
    let s = setup();
    for i in 0..30 {
        assert_eq!(
            enrol(&s.app, "wrong").await.0,
            StatusCode::UNAUTHORIZED,
            "{i}"
        );
    }
    assert_eq!(
        enrol(&s.app, "wrong").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A minute later it is open again.
    *s.now.lock().expect("clock") += Duration::seconds(61);
    assert_eq!(enrol(&s.app, "wrong").await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bad_requests_for_a_code_are_refused() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    for body in [
        json!({ "lifetime_minutes": 1 }),
        json!({ "lifetime_minutes": 100_000 }),
        json!({ "machine_number": 4_000_000_000_u64 }),
        json!({ "team": "bad\nname" }),
        json!({ "team": "x".repeat(201) }),
        json!({ "machine_number": "seven" }),
    ] {
        assert_eq!(
            mint(&s.app, &owner, &body).await.0,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
    let request = Request::post("/api/v1/enrol")
        .body(Body::from("not json"))
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::BAD_REQUEST);
    // An empty body is a code with nothing preset.
    let request = Request::post("/api/v1/orgs/default/enrolment-codes")
        .header("cookie", &owner)
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn one_person_may_make_only_so_many_codes_a_minute() {
    let s = setup();
    let owner = cookie(&s.app, "owner@example.org").await;
    for i in 0..30 {
        assert_eq!(
            mint(&s.app, &owner, &json!({})).await.0,
            StatusCode::CREATED,
            "{i}"
        );
    }
    assert_eq!(
        mint(&s.app, &owner, &json!({})).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}
