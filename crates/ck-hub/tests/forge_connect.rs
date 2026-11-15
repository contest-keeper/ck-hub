use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, Role, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
const STATE: &str = "a-random-state-of-enough-length";

struct Setup {
    app: Router,
    now: Arc<Mutex<DateTime<Utc>>>,
}

fn setup() -> Setup {
    let now = Arc::new(Mutex::new(Utc::now()));
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
    let request = Request::post("/api/v1/sticks")
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
    let request = Request::post("/api/v1/sticks")
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
