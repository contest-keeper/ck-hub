//! Signing the settings of a stick: who may ask, what the Hub signs, and that the signature checks.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ck_crypto::{ConfigSigningKey, verify_config};
use ck_hub::{Hub, MAX_SIGNATURES_PER_MINUTE, MemoryStore, PasswordCost, Registry, Role, router};
use ck_protocol::{
    ENROLMENT_SCHEMA, EnrolmentCode, EnrolmentConfig, SCHEMA, Settings, StickConfig, StickId, Token,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

struct Setup {
    app: Router,
    hub: Arc<Hub>,
    key: Option<ConfigSigningKey>,
}

fn setup(with_key: bool) -> Setup {
    let key = with_key.then(ConfigSigningKey::generate);
    let seed = key.as_ref().map(ConfigSigningKey::seed_base64);
    let mut hub = Hub::new(
        Registry::empty(),
        Arc::new(MemoryStore::default()),
        None,
        Box::new(OffsetDateTime::now_utc),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    if let Some(seed) = &seed {
        hub = hub.with_config_signing_key(
            ConfigSigningKey::from_seed_base64(seed).expect("the seed just made"),
        );
    }
    for email in [
        "owner@example.org",
        "viewer@example.org",
        "other@example.org",
    ] {
        hub.create_user(email, PASSWORD).expect("a user");
    }
    hub.create_organization("default", "Default", "owner@example.org")
        .expect("an organization");
    hub.create_organization("other", "Other", "other@example.org")
        .expect("another organization");
    hub.add_member("default", "viewer@example.org", Role::Viewer)
        .expect("a viewer");
    let hub = Arc::new(hub);
    Setup {
        app: router(Arc::clone(&hub)),
        hub,
        key,
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
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

fn stick_config(id: &str) -> Vec<u8> {
    let config = StickConfig {
        schema: SCHEMA,
        stick_id: StickId::parse(id).expect("id"),
        created_at: "2026-11-10T15:22:05".parse().expect("time"),
        token: Token::new("a-secret-token-of-the-stick"),
        machine_number: Some(1),
        settings: Settings::default(),
    };
    serde_json::to_vec_pretty(&config).expect("json")
}

fn enrolment_config(id: &str) -> Vec<u8> {
    let config = EnrolmentConfig::new(
        StickId::parse(id).expect("id"),
        "2026-11-10T15:22:05".parse().expect("time"),
        Some(1),
        EnrolmentCode::parse("ABCD-EFGH-JKLM-NPQR-STUV").expect("code"),
        Settings::default(),
    );
    serde_json::to_vec_pretty(&config).expect("json")
}

async fn sign(app: &Router, cookie: Option<&str>, org: &str, bytes: &[u8]) -> (StatusCode, Value) {
    let mut request = Request::post(format!("/api/v1/orgs/{org}/config-signatures"))
        .header("content-type", "application/json");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let body = json!({ "config": STANDARD.encode(bytes) }).to_string();
    call(app, request.body(Body::from(body)).expect("request")).await
}

#[tokio::test]
async fn a_hub_without_a_key_cannot_sign_and_has_no_public_key() {
    let s = setup(false);
    let owner = cookie(&s.app, "owner@example.org").await;
    let (status, body) = sign(&s.app, Some(&owner), "default", &stick_config("aaaa1111")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let request = Request::get("/api/v1/config-signing-key")
        .body(Body::empty())
        .expect("request");
    assert_eq!(call(&s.app, request).await.0, StatusCode::NOT_FOUND);
    assert!(s.hub.config_signing_public_key().is_none());
}

#[tokio::test]
async fn the_public_key_is_public_and_matches_the_signatures() {
    let s = setup(true);
    let request = Request::get("/api/v1/config-signing-key")
        .body(Body::empty())
        .expect("request");
    let (status, key) = call(&s.app, request).await;
    assert_eq!(status, StatusCode::OK);
    let public = s.key.as_ref().expect("key").verifying_key();
    assert_eq!(key["public_key"], public.to_base64());
    assert_eq!(key["key_id"], public.key_id());
    assert!(
        !key.to_string()
            .contains(&s.key.as_ref().expect("key").seed_base64())
    );
}

#[tokio::test]
async fn an_admin_gets_a_signature_that_checks_for_both_kinds_of_settings() {
    let s = setup(true);
    let owner = cookie(&s.app, "owner@example.org").await;
    let trusted = [s.key.as_ref().expect("key").verifying_key()];
    for bytes in [stick_config("aaaa1111"), enrolment_config("bbbb2222")] {
        let (status, body) = sign(&s.app, Some(&owner), "default", &bytes).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let file = body["signature"].as_str().expect("signature");
        assert_eq!(verify_config(&trusted, &bytes, file), Ok(()));
        // Not for any other bytes.
        let mut edited = bytes.clone();
        edited.push(b' ');
        assert!(verify_config(&trusted, &edited, file).is_err());
    }
}

#[tokio::test]
async fn only_an_admin_of_the_organization_may_ask() {
    let s = setup(true);
    let viewer = cookie(&s.app, "viewer@example.org").await;
    let other = cookie(&s.app, "other@example.org").await;
    let bytes = stick_config("aaaa1111");
    assert_eq!(
        sign(&s.app, None, "default", &bytes).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        sign(&s.app, Some(&viewer), "default", &bytes).await.0,
        StatusCode::FORBIDDEN
    );
    // Someone who is not in the organization does not even learn that it exists.
    assert_eq!(
        sign(&s.app, Some(&other), "default", &bytes).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn only_settings_of_a_stick_are_signed() {
    let s = setup(true);
    let owner = cookie(&s.app, "owner@example.org").await;
    let mut old_version: Value = serde_json::from_slice(&stick_config("aaaa1111")).expect("json");
    old_version["schema"] = json!(1);
    let mut unknown_version = old_version.clone();
    unknown_version["schema"] = json!(ENROLMENT_SCHEMA + 40);
    for bad in [
        b"not json".to_vec(),
        b"{}".to_vec(),
        b"[1, 2]".to_vec(),
        serde_json::to_vec(&json!({ "schema": SCHEMA })).expect("json"),
        serde_json::to_vec(&old_version).expect("json"),
        serde_json::to_vec(&unknown_version).expect("json"),
        vec![b' '; ck_hub::MAX_SIGNED_SETTINGS_BYTES + 1],
    ] {
        assert_eq!(
            sign(&s.app, Some(&owner), "default", &bad).await.0,
            StatusCode::BAD_REQUEST,
            "{}",
            String::from_utf8_lossy(&bad[..bad.len().min(40)])
        );
    }
    // The request itself must be what the route expects.
    for body in ["{}", "not json", r#"{"config": "not base64 !"}"#] {
        let request = Request::post("/api/v1/orgs/default/config-signatures")
            .header("cookie", &owner)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("request");
        assert_eq!(
            call(&s.app, request).await.0,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
}

#[tokio::test]
async fn the_id_of_a_stick_of_another_organization_or_a_deleted_one_is_refused() {
    let s = setup(true);
    let owner = cookie(&s.app, "owner@example.org").await;
    let other = cookie(&s.app, "other@example.org").await;
    let request = Request::post("/api/v1/orgs/default/sticks")
        .header("cookie", &owner)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "stick_id": "owned-stick", "token": "token-of-the-owned-stick" }).to_string(),
        ))
        .expect("request");
    assert_eq!(call(&s.app, request).await.1["added"], 1);
    let bytes = stick_config("owned-stick");

    // Its own organization may sign it; another may not, and learns nothing else.
    assert_eq!(
        sign(&s.app, Some(&owner), "default", &bytes).await.0,
        StatusCode::OK
    );
    assert_eq!(
        sign(&s.app, Some(&other), "other", &bytes).await.0,
        StatusCode::CONFLICT
    );

    // After it is deleted its id is not available to anyone.
    let send = |method: &'static str, path: &str, body: Body| {
        let request = Request::builder()
            .method(method)
            .uri(path.to_owned())
            .header("cookie", &owner)
            .header("content-type", "application/json")
            .body(body)
            .expect("request");
        call(&s.app, request)
    };
    let stick = "/api/v1/orgs/default/sticks/owned-stick";
    let days = Body::from(json!({ "days": 7 }).to_string());
    assert_eq!(
        send("POST", &format!("{stick}/archive"), days).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send("DELETE", stick, Body::empty()).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        sign(&s.app, Some(&owner), "default", &bytes).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn signing_has_a_budget_for_each_minute() {
    let s = setup(true);
    let owner = cookie(&s.app, "owner@example.org").await;
    let bytes = stick_config("aaaa1111");
    for _ in 0..MAX_SIGNATURES_PER_MINUTE {
        assert_eq!(
            sign(&s.app, Some(&owner), "default", &bytes).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        sign(&s.app, Some(&owner), "default", &bytes).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}
