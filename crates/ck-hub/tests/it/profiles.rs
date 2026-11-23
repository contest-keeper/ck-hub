use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, Role, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest of a picture";

struct Setup {
    app: Router,
    ana: String,
    bob: String,
    vera: String,
}

async fn setup() -> Setup {
    let hub = Hub::new(
        Registry::empty(),
        Arc::new(MemoryStore::default()),
        None,
        Box::new(time::OffsetDateTime::now_utc),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    for who in ["ana", "bob", "vera"] {
        hub.create_user(&format!("{who}@example.org"), PASSWORD)
            .expect("user");
    }
    hub.create_organization("one", "One", "ana@example.org")
        .expect("one");
    hub.create_organization("two", "Two", "bob@example.org")
        .expect("two");
    hub.add_member("one", "vera@example.org", Role::Viewer)
        .expect("vera");
    let app = router(Arc::new(hub));
    Setup {
        ana: cookie_for(&app, "ana").await,
        bob: cookie_for(&app, "bob").await,
        vera: cookie_for(&app, "vera").await,
        app,
    }
}

async fn cookie_for(app: &Router, who: &str) -> String {
    let request = Request::post("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": format!("{who}@example.org"), "password": PASSWORD }).to_string(),
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

struct Answer {
    status: StatusCode,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> Answer {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
    let response = app
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().expect("text").to_owned());
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer {
        status,
        content_type,
        bytes,
    }
}

fn picture(name: &str, bytes: &[u8]) -> Value {
    json!({ "name": name, "data_base64": STANDARD.encode(bytes) })
}

fn profile(name: &str, wallpaper: Value) -> Value {
    let mut body = json!({ "name": name, "settings": { "contest": name, "fps": 30 } });
    body["wallpaper"] = wallpaper;
    body
}

async fn put(s: &Setup, cookie: &str, id: &str, body: Value) -> Answer {
    send(
        &s.app,
        "PUT",
        &format!("/api/v1/profiles/{id}"),
        Some(cookie),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn a_profile_is_saved_listed_read_and_its_wallpaper_served() {
    let s = setup().await;
    let saved = put(
        &s,
        &s.ana,
        "p-1",
        profile("Finals", picture("wall.png", PNG)),
    )
    .await;
    assert_eq!(saved.status, StatusCode::NO_CONTENT);

    let list = send(&s.app, "GET", "/api/v1/profiles", Some(&s.ana), None).await;
    assert_eq!(list.status, StatusCode::OK);
    let list = list.json();
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["id"], "p-1");
    assert_eq!(list[0]["name"], "Finals");
    assert!(list[0]["updated_at"].is_string());

    let one = send(&s.app, "GET", "/api/v1/profiles/p-1", Some(&s.ana), None).await;
    let one = one.json();
    assert_eq!(one["settings"], json!({ "contest": "Finals", "fps": 30 }));
    assert_eq!(one["wallpaper"], json!({ "name": "wall.png" }));
    assert!(one["updated_at"].is_string());

    let wall = send(
        &s.app,
        "GET",
        "/api/v1/profiles/p-1/wallpaper",
        Some(&s.ana),
        None,
    )
    .await;
    assert_eq!(wall.status, StatusCode::OK);
    assert_eq!(wall.content_type.as_deref(), Some("image/png"));
    assert_eq!(wall.bytes, PNG);
}

#[tokio::test]
async fn saving_again_replaces_the_profile_and_keeps_one_row() {
    let s = setup().await;
    put(&s, &s.ana, "p-1", profile("Old", picture("wall.png", PNG))).await;
    let jpg = b"\xff\xd8\xff\xe0jpeg bytes";
    let again = put(&s, &s.ana, "p-1", profile("New", picture("photo.JPG", jpg))).await;
    assert_eq!(again.status, StatusCode::NO_CONTENT);

    let list = send(&s.app, "GET", "/api/v1/profiles", Some(&s.ana), None)
        .await
        .json();
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["name"], "New");
    let wall = send(
        &s.app,
        "GET",
        "/api/v1/profiles/p-1/wallpaper",
        Some(&s.ana),
        None,
    )
    .await;
    assert_eq!(wall.content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(wall.bytes, jpg);

    put(&s, &s.ana, "p-1", profile("New", Value::Null)).await;
    let one = send(&s.app, "GET", "/api/v1/profiles/p-1", Some(&s.ana), None)
        .await
        .json();
    assert_eq!(one["wallpaper"], Value::Null);
    let wall = send(
        &s.app,
        "GET",
        "/api/v1/profiles/p-1/wallpaper",
        Some(&s.ana),
        None,
    )
    .await;
    assert_eq!(wall.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn another_organization_cannot_see_change_or_delete_a_profile() {
    let s = setup().await;
    put(&s, &s.ana, "p-1", profile("Ours", picture("wall.png", PNG))).await;

    let list = send(&s.app, "GET", "/api/v1/profiles", Some(&s.bob), None)
        .await
        .json();
    assert_eq!(list, json!([]));
    for path in ["/api/v1/profiles/p-1", "/api/v1/profiles/p-1/wallpaper"] {
        let answer = send(&s.app, "GET", path, Some(&s.bob), None).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
    }
    let deleted = send(&s.app, "DELETE", "/api/v1/profiles/p-1", Some(&s.bob), None).await;
    assert_eq!(deleted.status, StatusCode::NOT_FOUND);

    // The same id in another organization is a profile of its own and leaves ours alone.
    put(&s, &s.bob, "p-1", profile("Theirs", Value::Null)).await;
    let ours = send(&s.app, "GET", "/api/v1/profiles/p-1", Some(&s.ana), None)
        .await
        .json();
    assert_eq!(ours["name"], "Ours");
    assert_eq!(ours["wallpaper"], json!({ "name": "wall.png" }));
}

#[tokio::test]
async fn deleting_removes_the_profile() {
    let s = setup().await;
    put(&s, &s.ana, "p-1", profile("Finals", Value::Null)).await;
    let deleted = send(&s.app, "DELETE", "/api/v1/profiles/p-1", Some(&s.ana), None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = send(&s.app, "GET", "/api/v1/profiles/p-1", Some(&s.ana), None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    let again = send(&s.app, "DELETE", "/api/v1/profiles/p-1", Some(&s.ana), None).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn it_needs_a_session_and_the_admin_role() {
    let s = setup().await;
    put(&s, &s.ana, "p-1", profile("Finals", Value::Null)).await;
    for (method, path) in [
        ("GET", "/api/v1/profiles"),
        ("GET", "/api/v1/profiles/p-1"),
        ("GET", "/api/v1/profiles/p-1/wallpaper"),
        ("PUT", "/api/v1/profiles/p-1"),
        ("DELETE", "/api/v1/profiles/p-1"),
    ] {
        let none = send(&s.app, method, path, None, Some(profile("x", Value::Null))).await;
        assert_eq!(none.status, StatusCode::UNAUTHORIZED, "{method} {path}");
        let viewer = send(
            &s.app,
            method,
            path,
            Some(&s.vera),
            Some(profile("x", Value::Null)),
        )
        .await;
        assert_eq!(viewer.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

#[tokio::test]
async fn wallpaper_names_must_be_plain_png_or_jpeg_names() {
    let s = setup().await;
    for name in [
        "../wall.png",
        "a/wall.png",
        "a\\wall.png",
        ".",
        "..",
        "",
        "wall.gif",
        "wall",
        "wall.svg",
        "wall.png\n",
        ".png.exe",
    ] {
        let answer = put(&s, &s.ana, "p-1", profile("x", picture(name, PNG))).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{name:?}");
    }
    for name in ["wall.png", "WALL.PNG", "a b.jpeg", "x.jpg"] {
        let bytes: &[u8] = if name.ends_with("png") || name.ends_with("PNG") {
            PNG
        } else {
            b"\xff\xd8\xffdata"
        };
        let answer = put(&s, &s.ana, "p-1", profile("x", picture(name, bytes))).await;
        assert_eq!(answer.status, StatusCode::NO_CONTENT, "{name:?}");
    }
    let list = send(&s.app, "GET", "/api/v1/profiles", Some(&s.ana), None)
        .await
        .json();
    assert_eq!(list.as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn the_wallpaper_must_be_the_picture_its_name_says() {
    let s = setup().await;
    let wrong = put(
        &s,
        &s.ana,
        "p-1",
        profile("x", picture("wall.png", b"<svg onload=x>")),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    let broken = put(
        &s,
        &s.ana,
        "p-1",
        profile("x", json!({ "name": "wall.png", "data_base64": "%%%" })),
    )
    .await;
    assert_eq!(broken.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sizes_are_limited() {
    let s = setup().await;
    let mut big = PNG.to_vec();
    big.resize(5 * 1024 * 1024 + 1, 0);
    let too_big = put(&s, &s.ana, "p-1", profile("x", picture("wall.png", &big))).await;
    assert_eq!(too_big.status, StatusCode::PAYLOAD_TOO_LARGE);
    big.truncate(5 * 1024 * 1024);
    let fits = put(&s, &s.ana, "p-1", profile("x", picture("wall.png", &big))).await;
    assert_eq!(fits.status, StatusCode::NO_CONTENT);

    let settings = json!({ "text": "a".repeat(300 * 1024) });
    let heavy = put(
        &s,
        &s.ana,
        "p-2",
        json!({ "name": "x", "settings": settings, "wallpaper": null }),
    )
    .await;
    assert_eq!(heavy.status, StatusCode::PAYLOAD_TOO_LARGE);

    let huge = "a".repeat(20 * 1024 * 1024);
    let flood = put(
        &s,
        &s.ana,
        "p-3",
        json!({ "name": "x", "settings": { "t": huge }, "wallpaper": null }),
    )
    .await;
    assert_eq!(flood.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn ids_and_names_are_checked() {
    let s = setup().await;
    for id in ["a.b", "a%20b", &"x".repeat(65)] {
        let answer = put(&s, &s.ana, id, profile("x", Value::Null)).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{id}");
    }
    let blank = put(&s, &s.ana, "p-1", profile("  ", Value::Null)).await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST);
    let long = put(&s, &s.ana, "p-1", profile(&"n".repeat(201), Value::Null)).await;
    assert_eq!(long.status, StatusCode::BAD_REQUEST);
    let not_a_profile = send(
        &s.app,
        "PUT",
        "/api/v1/profiles/p-1",
        Some(&s.ana),
        Some(json!({ "name": "x" })),
    )
    .await;
    assert_eq!(not_a_profile.status, StatusCode::BAD_REQUEST);
}
