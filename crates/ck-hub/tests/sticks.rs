use std::fs;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, Role, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
const TOKEN: &str = "a-secret-token-of-the-first-stick";

struct Setup {
    dir: TempDir,
    app: Router,
}

fn setup_with_registry(registry_json: &str) -> Setup {
    let dir = TempDir::new().expect("temp dir");
    let registry = if registry_json.is_empty() {
        Registry::empty()
    } else {
        fs::write(dir.path().join("registry.json"), registry_json).expect("registry");
        Registry::open(dir.path().join("registry.json")).expect("open")
    };
    let hub = Hub::new(
        registry,
        Box::new(MemoryStore::default()),
        None,
        Box::new(Utc::now),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    hub.create_user("ana", PASSWORD, Role::Admin)
        .expect("admin");
    hub.create_user("vera", PASSWORD, Role::Viewer)
        .expect("viewer");
    Setup {
        dir,
        app: router(Arc::new(hub)),
    }
}

fn setup() -> Setup {
    setup_with_registry("")
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

async fn cookie_for(app: &Router, name: &str) -> String {
    let request = Request::post("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "username": name, "password": PASSWORD }).to_string(),
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

async fn register(app: &Router, cookie: Option<&str>, body: &Value) -> (StatusCode, Value) {
    let mut request = Request::post("/api/v1/sticks").header("content-type", "application/json");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let (status, text) = call(
        app,
        request.body(Body::from(body.to_string())).expect("request"),
    )
    .await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

fn stick(id: &str, token: &str) -> Value {
    json!({ "stick_id": id, "token": token, "team": "Alpha", "site": "Lab 1",
            "contest": "Spring", "machine_number": 3 })
}

async fn heartbeat_status(app: &Router, id: &str, token: &str) -> StatusCode {
    let body = serde_json::to_vec(&json!({
        "stick_id": id, "boot_id": "b", "seq": 1, "sent_at": "2026-11-13T10:00:00Z", "uptime_s": 5,
        "hostname": "h", "image_version": "v", "config_sha256": "0".repeat(64), "team": "t",
        "site": "s", "recording": { "mode": "low", "state": "recording" }
    }))
    .expect("json");
    let signature = ck_crypto::sign(&ck_protocol::Token::new(token), &body);
    let request = Request::post("/api/v1/heartbeat")
        .header("X-CK-Stick", id)
        .header("X-CK-Signature", signature)
        .body(Body::from(body))
        .expect("request");
    call(app, request).await.0
}

async fn list(app: &Router) -> Vec<Value> {
    let cookie = cookie_for(app, "vera").await;
    let request = Request::get("/api/v1/sticks")
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("request");
    let (status, text) = call(app, request).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_str(&text).expect("json")
}

#[tokio::test]
async fn only_an_admin_may_register_sticks() {
    let setup = setup();
    let body = stick("stick-1", TOKEN);
    assert_eq!(
        register(&setup.app, None, &body).await.0,
        StatusCode::UNAUTHORIZED
    );
    let viewer = cookie_for(&setup.app, "vera").await;
    assert_eq!(
        register(&setup.app, Some(&viewer), &body).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(list(&setup.app).await.len(), 0, "nothing was registered");

    let admin = cookie_for(&setup.app, "ana").await;
    let (status, answer) = register(&setup.app, Some(&admin), &body).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["added"], 1);
}

#[tokio::test]
async fn a_registered_stick_can_report_with_its_secret_and_shows_in_the_list() {
    let setup = setup();
    let admin = cookie_for(&setup.app, "ana").await;
    register(&setup.app, Some(&admin), &stick("stick-1", TOKEN)).await;

    assert_eq!(
        heartbeat_status(&setup.app, "stick-1", TOKEN).await,
        StatusCode::OK
    );
    assert_eq!(
        heartbeat_status(&setup.app, "stick-1", "some-other-secret-entirely").await,
        StatusCode::UNAUTHORIZED
    );
    let sticks = list(&setup.app).await;
    assert_eq!(sticks.len(), 1);
    assert_eq!(sticks[0]["team"], "Alpha");
    assert_eq!(sticks[0]["contest"], "Spring");
    assert_eq!(sticks[0]["machine_number"], 3);
    assert_eq!(sticks[0]["online"], true);
}

#[tokio::test]
async fn registering_twice_changes_nothing_and_a_different_secret_is_refused() {
    let setup = setup();
    let admin = cookie_for(&setup.app, "ana").await;
    register(&setup.app, Some(&admin), &stick("stick-1", TOKEN)).await;

    let (status, again) = register(&setup.app, Some(&admin), &stick("stick-1", TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (again["added"].clone(), again["already_known"].clone()),
        (json!(0), json!(1))
    );

    let other = "a-different-secret-for-the-same-id";
    let (status, answer) = register(&setup.app, Some(&admin), &stick("stick-1", other)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(answer["conflicts"], json!(["stick-1"]));
    // The secret that was registered first still works, and the one offered later does not.
    assert_eq!(
        heartbeat_status(&setup.app, "stick-1", TOKEN).await,
        StatusCode::OK
    );
    assert_eq!(
        heartbeat_status(&setup.app, "stick-1", other).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_whole_registry_file_is_accepted_even_a_big_one() {
    let setup = setup();
    let admin = cookie_for(&setup.app, "ana").await;
    // The admin tool's registry has more fields than the Hub uses; they are ignored.
    let entries: Vec<Value> = (1..=400)
        .map(|n| {
            json!({ "stick_id": format!("sim-{n}"), "token": format!("secret-for-simulated-stick-{n}"),
                    "team": format!("Team {n}"), "site": "Lab", "contest": "Spring",
                    "image": format!("sim-{n}.img"), "created_at": "2026-11-13T09:00:00",
                    "machine_number": n })
        })
        .collect();
    let body = json!({ "sticks": entries });
    assert!(
        body.to_string().len() > 64 * 1024,
        "bigger than any other route allows"
    );
    let (status, answer) = register(&setup.app, Some(&admin), &body).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["added"], 400);
    assert_eq!(list(&setup.app).await.len(), 400);
}

#[tokio::test]
async fn a_bad_request_registers_nothing_at_all() {
    let setup = setup();
    let admin = cookie_for(&setup.app, "ana").await;
    // The second stick of the batch is wrong, so the first is not registered either.
    let batch = json!({ "sticks": [stick("stick-1", TOKEN), stick("stick-2", "short")] });
    let (status, answer) = register(&setup.app, Some(&admin), &batch).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(answer["error"].as_str().expect("text").contains("stick-2"));
    assert_eq!(list(&setup.app).await.len(), 0);

    let mut with_control = stick("stick-3", TOKEN);
    with_control["team"] = json!("bad\nname");
    assert_eq!(
        register(&setup.app, Some(&admin), &with_control).await.0,
        StatusCode::BAD_REQUEST
    );

    let with_space = stick("not a valid id", TOKEN);
    assert!(
        register(&setup.app, Some(&admin), &with_space)
            .await
            .0
            .is_client_error()
    );
    assert_eq!(list(&setup.app).await.len(), 0);
}

#[tokio::test]
async fn a_stick_in_the_registry_file_cannot_be_given_another_secret() {
    let file = r#"{"sticks":[{"stick_id":"file-1","token":"the-secret-from-the-file-1","team":"F","site":"S","contest":"C","machine_number":1}]}"#;
    let setup = setup_with_registry(file);
    let admin = cookie_for(&setup.app, "ana").await;
    let (status, answer) = register(&setup.app, Some(&admin), &stick("file-1", TOKEN)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(
        heartbeat_status(&setup.app, "file-1", "the-secret-from-the-file-1").await,
        StatusCode::OK
    );

    // The same secret as the file has is fine, and the sticks of both sources are listed together.
    let same = stick("file-1", "the-secret-from-the-file-1");
    assert_eq!(
        register(&setup.app, Some(&admin), &same).await.1["already_known"],
        1
    );
    register(&setup.app, Some(&admin), &stick("db-1", TOKEN)).await;
    assert_eq!(list(&setup.app).await.len(), 2);
    let _keep = &setup.dir;
}

#[tokio::test]
async fn the_answers_never_contain_a_secret() {
    let setup = setup();
    let admin = cookie_for(&setup.app, "ana").await;
    let request = Request::post("/api/v1/sticks")
        .header("content-type", "application/json")
        .header("cookie", &admin)
        .body(Body::from(stick("stick-1", TOKEN).to_string()))
        .expect("request");
    let (_, text) = call(&setup.app, request).await;
    assert!(!text.contains(TOKEN));
    let sticks = list(&setup.app).await;
    assert!(
        !serde_json::to_string(&sticks)
            .expect("json")
            .contains(TOKEN)
    );
}
