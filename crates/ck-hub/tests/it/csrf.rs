//! Every state-changing route refuses a request from another origin and, when it carries the session
//! cookie, a body that is not JSON. The list is checked against `routes/mod.rs`, so a new route has to be
//! added here (or to the read-only list) before the tests pass.

use std::fs;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, router};
use tempfile::TempDir;
use tower::ServiceExt;

/// Method and path of every route that changes something.
pub(crate) const STATE_CHANGING: &[(&str, &str)] = &[
    ("POST", "/api/v1/heartbeat"),
    ("POST", "/api/v1/events"),
    ("POST", "/api/v1/enrol"),
    ("POST", "/api/v1/enrol/claim"),
    ("POST", "/api/v1/forge/connect"),
    ("POST", "/api/v1/forge/token"),
    ("POST", "/api/v1/invitations/accept"),
    ("POST", "/api/v1/login"),
    ("POST", "/api/v1/logout"),
    ("POST", "/api/v1/orgs/o/config-hashes"),
    ("POST", "/api/v1/orgs/o/config-signatures"),
    ("POST", "/api/v1/orgs/o/enrolment-codes"),
    ("POST", "/api/v1/orgs/o/hardware-writes"),
    ("POST", "/api/v1/orgs/o/invitations"),
    ("DELETE", "/api/v1/orgs/o/invitations/i"),
    ("DELETE", "/api/v1/orgs/o/members/u"),
    ("POST", "/api/v1/orgs/o/sticks"),
    ("DELETE", "/api/v1/orgs/o/sticks/s"),
    ("POST", "/api/v1/orgs/o/sticks/s/archive"),
    ("POST", "/api/v1/orgs/o/sticks/s/commands"),
    ("DELETE", "/api/v1/orgs/o/sticks/s/credential"),
    ("POST", "/api/v1/orgs/o/sticks/s/restore"),
    ("POST", "/api/v1/password-reset"),
    ("POST", "/api/v1/password-reset/confirm"),
    ("PUT", "/api/v1/profiles/p"),
    ("DELETE", "/api/v1/profiles/p"),
    ("POST", "/api/v1/recordings"),
    ("PUT", "/api/v1/recordings/r/chunks/0"),
    ("POST", "/api/v1/recordings/r/complete"),
    ("POST", "/api/v1/register"),
    ("POST", "/api/v1/staff/organizations"),
    ("POST", "/api/v1/verify-email"),
];

/// Paths that only answer GET, so nothing can be forged through them.
pub(crate) const READ_ONLY: &[&str] = &[
    "/api/v1/config-signing-key",
    "/api/v1/images",
    "/api/v1/me",
    "/api/v1/orgs/{slug}/commands/{command_id}",
    "/api/v1/orgs/{slug}/members",
    "/api/v1/orgs/{slug}/recordings/{recording_id}/view",
    "/api/v1/orgs/{slug}/sticks/{id}/network",
    "/api/v1/orgs/{slug}/sticks/{id}/recordings",
    "/api/v1/orgs/{slug}/sticks/{id}/submissions",
    "/api/v1/profiles",
    "/api/v1/profiles/{id}/wallpaper",
    "/api/v1/recordings/{id}",
];

fn app() -> (TempDir, Router) {
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("registry.json"), r#"{"sticks":[]}"#).expect("registry");
    let registry = Registry::open(dir.path().join("registry.json")).expect("open");
    let hub = Hub::new(
        registry,
        Arc::new(MemoryStore::default()),
        None,
        Box::new(|| time::OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time")),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    (dir, router(Arc::new(hub)))
}

async fn status(app: &Router, request: Request<Body>) -> StatusCode {
    app.clone()
        .oneshot(request)
        .await
        .expect("response")
        .status()
}

fn request(method: &str, path: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header("host", "hub.example")
}

#[tokio::test]
async fn a_request_from_another_origin_is_refused_on_every_state_changing_route() {
    let (_dir, app) = app();
    for (method, path) in STATE_CHANGING {
        for origin in [
            "https://evil.example",
            "null",
            "http://hub.example.evil.example",
        ] {
            let forged = request(method, path)
                .header("origin", origin)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("request");
            assert_eq!(
                status(&app, forged).await,
                StatusCode::FORBIDDEN,
                "{method} {path} from {origin}"
            );
        }
    }
}

#[tokio::test]
async fn a_cookie_request_that_is_not_json_is_refused_on_every_state_changing_route() {
    let (_dir, app) = app();
    for (method, path) in STATE_CHANGING {
        for content_type in [
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=x"),
            None,
        ] {
            let mut builder = request(method, path)
                .header("origin", "https://hub.example")
                .header("cookie", "ck_session=anything");
            if let Some(content_type) = content_type {
                builder = builder.header("content-type", content_type);
            }
            let forged = builder
                .header("content-length", "3")
                .body(Body::from("a=b"))
                .expect("request");
            assert_eq!(
                status(&app, forged).await,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{method} {path} as {content_type:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_hubs_own_origin_and_clients_without_one_still_get_through() {
    let (_dir, app) = app();
    for origin in [Some("https://hub.example"), None] {
        let mut builder = request("POST", "/api/v1/login")
            .header("content-type", "application/json")
            .header("cookie", "ck_session=anything");
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        let answer = status(
            &app,
            builder
                .body(Body::from(r#"{"email":"a@b.c","password":"x"}"#))
                .expect("request"),
        )
        .await;
        assert_eq!(answer, StatusCode::UNAUTHORIZED, "origin {origin:?}");
    }
}

#[tokio::test]
async fn the_origin_may_be_named_by_a_proxy_or_by_an_http2_authority() {
    let (_dir, app) = app();
    let body = r#"{"email":"a@b.c","password":"x"}"#;
    let forwarded = Request::post("/api/v1/login")
        .header("host", "ca-hub.internal")
        .header("x-forwarded-host", "hub.example")
        .header("origin", "https://hub.example")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request");
    assert_eq!(status(&app, forwarded).await, StatusCode::UNAUTHORIZED);
    let http2 = Request::post("https://hub.example/api/v1/login")
        .header("origin", "https://hub.example")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request");
    assert_eq!(status(&app, http2).await, StatusCode::UNAUTHORIZED);
    let wrong = Request::post("/api/v1/login")
        .header("host", "ca-hub.internal")
        .header("x-forwarded-host", "other.example")
        .header("origin", "https://hub.example")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request");
    assert_eq!(status(&app, wrong).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_cookie_request_with_a_form_type_is_refused_even_without_a_length() {
    let (_dir, app) = app();
    let forged = request("POST", "/api/v1/orgs/o/sticks")
        .header("cookie", "ck_session=anything")
        .header("content-type", "text/plain")
        .body(Body::from("a=b"))
        .expect("request");
    assert_eq!(
        status(&app, forged).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[tokio::test]
async fn no_listed_method_is_refused_as_not_allowed() {
    let (_dir, app) = app();
    for (method, path) in STATE_CHANGING {
        let probe = request(method, path)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .expect("request");
        let answer = status(&app, probe).await;
        // 404 is a real answer (no such organization, recordings off), 405 is a stale list.
        assert_ne!(answer, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
    }
}

#[test]
fn the_list_covers_every_route_in_routes_rs() {
    let source = include_str!("../../src/routes/mod.rs");
    let mut paths = Vec::new();
    for (index, _) in source.match_indices("\"/api/v1/") {
        let rest = &source[index + 1..];
        let path = &rest[..rest.find('"').expect("closing quote")];
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    assert!(paths.len() > 40, "found {} routes", paths.len());
    let matches = |listed: &str, path: &str| {
        let mut a = listed.split('/');
        let mut b = path.split('/');
        loop {
            match (a.next(), b.next()) {
                (None, None) => return true,
                (Some(x), Some(y)) if x == y || y.starts_with('{') => {}
                _ => return false,
            }
        }
    };
    for path in paths {
        let known = STATE_CHANGING
            .iter()
            .any(|(_, listed)| matches(listed, path))
            || READ_ONLY.contains(&path);
        assert!(
            known,
            "{path} is in routes/mod.rs but in neither list of tests/csrf.rs"
        );
    }
}
