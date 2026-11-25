use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, Role, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
const STATE: &str = "a-random-state-of-enough-length";

struct Setup {
    app: Router,
    now: Arc<Mutex<OffsetDateTime>>,
}

fn setup() -> Setup {
    let now = Arc::new(Mutex::new(OffsetDateTime::now_utc()));
    let clock = {
        let now = Arc::clone(&now);
        Box::new(move || *now.lock().expect("clock"))
    };
    let hub = Hub::new(
        Registry::empty(),
        Arc::new(MemoryStore::default()),
        None,
        clock,
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    hub.create_user("ana@example.org", PASSWORD).expect("ana");
    hub.create_user("vera@example.org", PASSWORD).expect("vera");
    hub.create_organization("default", "Default", "ana@example.org")
        .expect("organization");
    hub.add_member("default", "vera@example.org", Role::Viewer)
        .expect("vera");
    Setup {
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
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn cookie_for(app: &Router, name: &str) -> String {
    let request = Request::post("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": name, "password": PASSWORD }).to_string(),
        ))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    response.headers()["set-cookie"]
        .to_str()
        .expect("text")
        .split(';')
        .next()
        .expect("cookie")
        .to_owned()
}

async fn connect(
    app: &Router,
    cookie: Option<&str>,
    port: u64,
    state: &str,
) -> (StatusCode, Value) {
    let mut request =
        Request::post("/api/v1/forge/connect").header("content-type", "application/json");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let body = json!({ "port": port, "state": state }).to_string();
    call(app, request.body(Body::from(body)).expect("request")).await
}

async fn trade(app: &Router, code: &str) -> (StatusCode, Value) {
    let request = Request::post("/api/v1/forge/token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "code": code }).to_string()))
        .expect("request");
    call(app, request).await
}

fn code_of(redirect: &str) -> String {
    redirect
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .expect("a code")
        .to_owned()
}

#[tokio::test]
async fn a_signed_in_admin_is_sent_back_to_this_pc_with_a_code_and_the_state() {
    let s = setup();
    let cookie = cookie_for(&s.app, "ana@example.org").await;
    let (status, answer) = connect(&s.app, Some(&cookie), 49321, STATE).await;
    assert_eq!(status, StatusCode::OK);
    let redirect = answer["redirect"].as_str().expect("redirect");
    assert!(redirect.starts_with("http://127.0.0.1:49321/callback?code="));
    assert!(redirect.ends_with(&format!("&state={STATE}")));
}

#[tokio::test]
async fn connecting_needs_a_session() {
    let s = setup();
    assert_eq!(
        connect(&s.app, None, 49321, STATE).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        connect(&s.app, Some("ck_session=nope"), 49321, STATE)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_port_or_state_the_forge_would_not_send_is_refused() {
    let s = setup();
    let cookie = cookie_for(&s.app, "ana@example.org").await;
    for (port, state) in [
        (80, STATE),
        (0, STATE),
        (49321, "short"),
        (49321, "a-state-with-a-slash/../and-more"),
        (49321, "a-state&redirect=evil-and-long-enough"),
    ] {
        let (status, _) = connect(&s.app, Some(&cookie), port, state).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{port} {state}");
    }
    let too_big = connect(&s.app, Some(&cookie), 70000, STATE).await.0;
    assert!(too_big.is_client_error());
}

#[tokio::test]
async fn the_code_gives_a_session_the_registry_route_accepts_as_a_bearer_token() {
    let s = setup();
    let cookie = cookie_for(&s.app, "ana@example.org").await;
    let (_, answer) = connect(&s.app, Some(&cookie), 49321, STATE).await;
    let code = code_of(answer["redirect"].as_str().expect("redirect"));

    let (status, token) = trade(&s.app, &code).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(token["email"], "ana@example.org");
    let bearer = token["token"].as_str().expect("token");

    let stick = json!({ "stick_id": "stick-1", "token": "a-secret-token-of-a-stick", "team": "Alpha",
                        "site": "Lab 1", "contest": "Spring", "machine_number": 1 });
    let request = Request::post("/api/v1/orgs/default/sticks")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(stick.to_string()))
        .expect("request");
    let (status, answer) = call(&s.app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["added"], 1);
}

#[tokio::test]
async fn a_code_works_once() {
    let s = setup();
    let cookie = cookie_for(&s.app, "ana@example.org").await;
    let (_, answer) = connect(&s.app, Some(&cookie), 49321, STATE).await;
    let code = code_of(answer["redirect"].as_str().expect("redirect"));
    assert_eq!(trade(&s.app, &code).await.0, StatusCode::OK);
    assert_eq!(trade(&s.app, &code).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_code_ends_after_a_minute_and_a_guess_gets_nothing() {
    let s = setup();
    let cookie = cookie_for(&s.app, "ana@example.org").await;
    let (_, answer) = connect(&s.app, Some(&cookie), 49321, STATE).await;
    let code = code_of(answer["redirect"].as_str().expect("redirect"));
    *s.now.lock().expect("clock") += Duration::seconds(61);
    assert_eq!(trade(&s.app, &code).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(trade(&s.app, "not-a-code").await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_viewer_connects_but_the_registry_route_still_needs_an_admin() {
    let s = setup();
    let cookie = cookie_for(&s.app, "vera@example.org").await;
    let (_, answer) = connect(&s.app, Some(&cookie), 49321, STATE).await;
    let code = code_of(answer["redirect"].as_str().expect("redirect"));
    let (_, token) = trade(&s.app, &code).await;
    let request = Request::post("/api/v1/orgs/default/sticks")
        .header(
            "authorization",
            format!("Bearer {}", token["token"].as_str().expect("token")),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({ "sticks": [] }).to_string()))
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_connect_page_is_served_without_a_session() {
    let s = setup();
    let request = Request::get("/forge/connect?port=49321&state=x")
        .body(Body::empty())
        .expect("request");
    let response = s.app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}

async fn post_json(
    app: &Router,
    path: &str,
    cookie: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::post(path).header("content-type", "application/json");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    call(
        app,
        request.body(Body::from(body.to_string())).expect("request"),
    )
    .await
}

async fn start(app: &Router) -> (String, String) {
    let (status, answer) = post_json(app, "/api/v1/forge/pairings", None, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["expires_in"], 600);
    (
        answer["code"].as_str().expect("code").to_owned(),
        answer["poll_key"].as_str().expect("key").to_owned(),
    )
}

async fn poll(app: &Router, key: &str) -> (StatusCode, Value) {
    post_json(
        app,
        "/api/v1/forge/pairings/poll",
        None,
        json!({ "poll_key": key }),
    )
    .await
}

async fn answer(app: &Router, cookie: Option<&str>, code: &str, approve: bool) -> StatusCode {
    post_json(
        app,
        "/api/v1/forge/pairings/answer",
        cookie,
        json!({ "code": code, "approve": approve }),
    )
    .await
    .0
}

#[tokio::test]
async fn a_pairing_code_connects_forge_once_the_admin_agrees() {
    let s = setup();
    let (code, key) = start(&s.app).await;
    // Eight letters and digits without characters that look alike, shown as two groups of four.
    assert_eq!(code.len(), 9);
    assert_eq!(code.as_bytes()[4], b'-');
    assert!(
        code.chars()
            .all(|c| c == '-' || "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(c))
    );
    assert_eq!(poll(&s.app, &key).await.1["status"], "waiting");

    let cookie = cookie_for(&s.app, "ana@example.org").await;
    // The page may send the code in lower case, with or without the dash.
    let sloppy = code.to_lowercase().replace('-', " ");
    assert_eq!(
        answer(&s.app, Some(&cookie), &sloppy, true).await,
        StatusCode::NO_CONTENT
    );

    let (status, done) = poll(&s.app, &key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["status"], "approved");
    assert_eq!(done["email"], "ana@example.org");
    assert_eq!(done["expires_in"], 12 * 3600);
    let token = done["token"].as_str().expect("token");
    let me = Request::get("/api/v1/me")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&s.app, me).await.0, StatusCode::OK);
    // The pairing is used up: the same key gets nothing again, and the code is gone.
    assert_eq!(poll(&s.app, &key).await.0, StatusCode::GONE);
    assert_eq!(
        answer(&s.app, Some(&cookie), &code, true).await,
        StatusCode::GONE
    );
}

#[tokio::test]
async fn a_pairing_nobody_agrees_to_ends_after_ten_minutes() {
    let s = setup();
    let (code, key) = start(&s.app).await;
    *s.now.lock().expect("clock") += Duration::minutes(11);
    let cookie = cookie_for(&s.app, "ana@example.org").await;
    assert_eq!(
        answer(&s.app, Some(&cookie), &code, true).await,
        StatusCode::GONE
    );
    assert_eq!(poll(&s.app, &key).await.0, StatusCode::GONE);
}

#[tokio::test]
async fn refusing_a_pairing_ends_it_and_no_session_is_made() {
    let s = setup();
    let (code, key) = start(&s.app).await;
    let cookie = cookie_for(&s.app, "ana@example.org").await;
    assert_eq!(
        answer(&s.app, Some(&cookie), &code, false).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(poll(&s.app, &key).await.0, StatusCode::GONE);
}

#[tokio::test]
async fn only_a_signed_in_person_can_answer_and_only_the_key_holder_can_collect() {
    let s = setup();
    let (code, key) = start(&s.app).await;
    assert_eq!(
        answer(&s.app, None, &code, true).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(poll(&s.app, "not-the-key").await.0, StatusCode::GONE);
    // The code is what people read aloud; it never collects anything.
    assert_eq!(poll(&s.app, &code).await.0, StatusCode::GONE);
    assert_eq!(poll(&s.app, &key).await.1["status"], "waiting");
}

#[tokio::test]
async fn guessing_codes_is_stopped_after_a_few_wrong_ones() {
    let s = setup();
    let (code, _key) = start(&s.app).await;
    let cookie = cookie_for(&s.app, "vera@example.org").await;
    for _ in 0..5 {
        assert_eq!(
            answer(&s.app, Some(&cookie), "AAAA-AAAA", true).await,
            StatusCode::GONE
        );
    }
    // A sixth try is refused even with the right code: a guesser cannot keep going.
    assert_eq!(
        answer(&s.app, Some(&cookie), &code, true).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn starting_pairings_is_limited_for_everyone_together() {
    let s = setup();
    for _ in 0..30 {
        start(&s.app).await;
    }
    let (status, _) = post_json(&s.app, "/api/v1/forge/pairings", None, json!({})).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}
