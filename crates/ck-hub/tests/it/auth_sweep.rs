//! Every route that is not public refuses a request with no credentials, with 401, before it looks
//! at anything else: not 404 for an organization it would have to look up, not 400 for a body it
//! would have to read. The routes come from the lists in `csrf.rs`, which are checked against
//! `routes/mod.rs`, so a new route is covered when it is added there.

use std::fs;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, router};
use tower::ServiceExt;

use crate::csrf::{READ_ONLY, STATE_CHANGING};

/// Routes anyone may call: they take no session. Each says why.
const PUBLIC: &[&str] = &[
    // Log in, sign up and the links in a mail.
    "/api/v1/login",
    "/api/v1/register",
    "/api/v1/verify-email",
    "/api/v1/password-reset",
    "/api/v1/password-reset/confirm",
    // A person ends a session by naming it; an unknown one is ignored.
    "/api/v1/logout",
    // A new stick trades its code; the code is the credential.
    "/api/v1/enrol",
    "/api/v1/enrol/claim",
    // The invitation's token is the credential.
    "/api/v1/invitations/accept",
    // The forge trades a code the person got after logging in.
    "/api/v1/forge/token",
    // Forge starts a pairing and collects the answer with a secret only it holds; the person answers
    // with a session, which the sweep checks.
    "/api/v1/forge/pairings",
    "/api/v1/forge/pairings/poll",
    // The public half of a key.
    "/api/v1/config-signing-key",
    // The list of images is public: the download itself needs a session.
    "/api/v1/images",
];

/// Routes a stick calls with its own signature, which an empty request does not carry.
const STICK_SIGNED: &[&str] = &[
    "/api/v1/heartbeat",
    "/api/v1/events",
    "/api/v1/recordings",
    "/api/v1/recordings/r/chunks/0",
    "/api/v1/recordings/r/complete",
    "/api/v1/recordings/x",
];

/// Every field any route reads, so a body is never what stops the request before the session check.
const BODY: &str = r#"{"email":"a@b.example","password":"a long enough password","role":"admin","slug":"x","name":"x","owner_email":"a@b.example","token":"x","code":"x","port":1,"state":"x"}"#;

fn concrete(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        out.push('x');
        rest = &rest[rest.find('}').expect("a closing brace") + 1..];
    }
    out.push_str(rest);
    out
}

#[tokio::test]
async fn every_route_that_is_not_public_refuses_a_request_without_credentials() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    fs::write(dir.path().join("registry.json"), r#"{"sticks":[]}"#).expect("registry");
    let hub = Hub::new(
        Registry::open(dir.path().join("registry.json")).expect("open"),
        Arc::new(MemoryStore::default()),
        None,
        Box::new(|| time::OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time")),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    let app = router(Arc::new(hub));

    let routes = STATE_CHANGING
        .iter()
        .map(|(method, path)| (*method, concrete(path)))
        .chain(READ_ONLY.iter().map(|path| ("GET", concrete(path))));
    let mut wrong = Vec::new();
    for (method, path) in routes {
        if PUBLIC.contains(&path.as_str()) || STICK_SIGNED.contains(&path.as_str()) {
            continue;
        }
        let request = Request::builder()
            .method(method)
            .uri(&path)
            .header("host", "hub.example")
            .header("content-type", "application/json")
            .body(Body::from(BODY))
            .expect("request");
        let status = app
            .clone()
            .oneshot(request)
            .await
            .expect("response")
            .status();
        if status != StatusCode::UNAUTHORIZED {
            wrong.push(format!("{method} {path}: {status}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "not 401 without a session:\n{}",
        wrong.join("\n")
    );
}
