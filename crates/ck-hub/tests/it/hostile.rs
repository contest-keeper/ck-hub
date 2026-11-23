//! Hostile input at the three doors a stranger can reach without a session: the heartbeat, the event
//! and the enrolment routes. Whatever bytes arrive, the Hub answers with a client error or a refusal,
//! never a server error, never a panic, and a request that is not what it claims is never accepted.
//!
//! The number of cases per property comes from `PROPTEST_CASES` (proptest reads it itself), the same
//! variable CI and ck-protocol use.

use std::fs;
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ck_hub::{Hub, MemoryStore, PasswordCost, Registry, router};
use ck_protocol::{SIGNATURE_HEADER, STICK_HEADER, Token};
use proptest::prelude::*;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "token-of-stick-a";

struct Door {
    _dir: TempDir,
    app: Router,
}

/// One Hub for all the cases of all properties: it must survive them, so sharing it is the test.
static DOOR: LazyLock<Door> = LazyLock::new(door);

fn door() -> Door {
    let dir = TempDir::new().expect("temp dir");
    fs::write(
        dir.path().join("registry.json"),
        format!(
            r#"{{"sticks":[{{"stick_id":"stick-a","token":"{TOKEN}","team":"Team A","site":"Lab 1",
               "contest":"Spring","image":"x.img","created_at":"2026-11-10T15:22:00",
               "machine_number":1}}]}}"#
        ),
    )
    .expect("registry");
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
    Door {
        _dir: dir,
        app: router(Arc::new(hub)),
    }
}

impl Door {
    fn post(&self, path: &str, headers: &[(&str, String)], body: Vec<u8>) -> StatusCode {
        let mut request = Request::post(path);
        for (name, value) in headers {
            // A header value the HTTP library itself refuses never reaches the Hub.
            let Ok(value) = axum::http::HeaderValue::from_str(value) else {
                return StatusCode::BAD_REQUEST;
            };
            request = request.header(*name, value);
        }
        let request = request.body(Body::from(body)).expect("request");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            self.app
                .clone()
                .oneshot(request)
                .await
                .expect("a response")
                .status()
        })
    }
}

/// Bodies of every shape: raw bytes, and JSON values of any depth with any types.
fn hostile_body() -> impl Strategy<Value = Vec<u8>> {
    let raw = any::<Vec<u8>>();
    let value = prop::collection::vec(
        (
            "[a-z_]{1,12}",
            prop_oneof![
                Just(serde_json::Value::Null),
                any::<bool>().prop_map(serde_json::Value::from),
                any::<i64>().prop_map(serde_json::Value::from),
                any::<u64>().prop_map(serde_json::Value::from),
                any::<f64>().prop_map(|n| serde_json::Number::from_f64(n)
                    .map_or(serde_json::Value::Null, serde_json::Value::Number)),
                ".{0,40}".prop_map(serde_json::Value::from),
                prop::collection::vec(any::<u8>(), 0..8).prop_map(serde_json::Value::from),
            ],
        ),
        0..10,
    )
    .prop_map(|fields| {
        serde_json::to_vec(&serde_json::Value::Object(fields.into_iter().collect())).expect("json")
    });
    prop_oneof![raw, value]
}

fn is_a_refusal(status: StatusCode) -> bool {
    status.is_client_error()
}

proptest! {
    /// A heartbeat or event signed with the real token but holding anything at all is refused or
    /// accepted with a normal answer, and never crashes the Hub.
    #[test]
    fn signed_garbage_never_makes_a_server_error(
        body in hostile_body(),
        path in prop_oneof![Just("/api/v1/heartbeat"), Just("/api/v1/events")],
    ) {
        let door = &*DOOR;
        let signature = ck_crypto::sign(&Token::new(TOKEN), &body);
        let status = door.post(
            path,
            &[(STICK_HEADER, "stick-a".to_owned()), (SIGNATURE_HEADER, signature)],
            body,
        );
        prop_assert!(!status.is_server_error(), "{path} answered {status}");
    }

    /// Hostile headers: any stick name and any signature, over any body, are never accepted.
    #[test]
    fn a_wrong_stick_or_signature_is_never_accepted(
        stick in ".{0,80}",
        signature in ".{0,120}",
        body in hostile_body(),
        path in prop_oneof![Just("/api/v1/heartbeat"), Just("/api/v1/events")],
    ) {
        let door = &*DOOR;
        let status = door.post(
            path,
            &[(STICK_HEADER, stick), (SIGNATURE_HEADER, signature)],
            body,
        );
        prop_assert!(is_a_refusal(status), "{path} answered {status}");
    }

    /// The enrolment routes take no session; random codes, proofs and bodies never get a credential.
    #[test]
    fn enrolment_never_gives_a_credential_for_a_guess(
        body in hostile_body(),
        code in "[A-Za-z0-9-]{0,40}",
        path in prop_oneof![Just("/api/v1/enrol"), Just("/api/v1/enrol/claim")],
    ) {
        let door = &*DOOR;
        let guess = serde_json::json!({ "code": code, "proof": code, "stick_id": "stick-a" })
            .to_string()
            .into_bytes();
        for body in [body, guess] {
            let status = door.post(
                path,
                &[("content-type", "application/json".to_owned())],
                body,
            );
            prop_assert!(is_a_refusal(status), "{path} answered {status}");
        }
    }
}
