//! The same tests for every store. PostgreSQL runs only when `CK_TEST_DATABASE_URL` is set, for
//! example `postgres://postgres:password@127.0.0.1:54329/ckhub` against a local Docker database:
//! the shared checks do not start a database of their own.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use ck_hub::{Hub, MemoryStore, PasswordCost, PostgresStore, Registry, Rejection, Role, Store};
use ck_protocol::{
    Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, RecorderStatus, RecordingMode,
    StickId, Token,
};
use tempfile::TempDir;

fn database_url() -> Option<String> {
    std::env::var("CK_TEST_DATABASE_URL").ok()
}

/// A prefix no earlier run used, because the events of a database are never deleted.
fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    format!("t{nanos}")
}

fn time(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("time")
}

fn heartbeat(stick: &str, boot: &str, seq: u64) -> Heartbeat {
    Heartbeat {
        stick_id: StickId::parse(stick).expect("id"),
        boot_id: boot.to_owned(),
        seq,
        sent_at: time(0),
        uptime_s: 10 * seq,
        hostname: "team-a-1".to_owned(),
        image_version: "ck-os-base development".to_owned(),
        config_sha256: "0".repeat(64),
        team: "Team A".to_owned(),
        site: "Lab 1".to_owned(),
        recording: RecorderStatus {
            mode: RecordingMode::Low,
            state: RecorderState::Recording,
        },
    }
}

fn event(stick: &str, seq: u64, user: &str) -> Event {
    Event {
        stick_id: StickId::parse(stick).expect("id"),
        boot_id: "boot-1".to_owned(),
        seq,
        prev_hash: FIRST_PREV_HASH.to_owned(),
        sent_at: time(0),
        kind: EventKind::Login,
        payload: serde_json::json!({ "username": user, "nested": { "b": 1, "a": [1, 2] } }),
    }
}

/// What every store must do.
fn the_store_contract(store: &dyn Store) {
    let id = unique();
    let a = format!("{id}-a");
    let b = format!("{id}-b");
    let a_id = StickId::parse(&a).expect("id");

    // Heartbeats: the latest one is kept and they are counted, for each stick on its own.
    assert!(store.last_heartbeat(&a_id).expect("read").is_none());
    store
        .record_heartbeat(heartbeat(&a, "boot-1", 1), time(1))
        .expect("write");
    store
        .record_heartbeat(heartbeat(&a, "boot-1", 2), time(2))
        .expect("write");
    store
        .record_heartbeat(heartbeat(&b, "boot-9", 7), time(3))
        .expect("write");
    let seen = store
        .last_heartbeat(&a_id)
        .expect("read")
        .expect("a heartbeat");
    assert_eq!(seen.heartbeat, heartbeat(&a, "boot-1", 2));
    assert_eq!(seen.received_at, time(2));
    assert_eq!(seen.count, 2);
    let other = store
        .last_heartbeat(&StickId::parse(&b).expect("id"))
        .expect("read")
        .expect("a heartbeat");
    assert_eq!((other.heartbeat.seq, other.count), (7, 1));

    // Events: kept whole, in the order received, for each stick on its own.
    assert!(store.events(&a_id).expect("read").is_empty());
    store
        .append_event(event(&a, 1, "ana"), time(10), None)
        .expect("write");
    store
        .append_event(event(&a, 2, "bob"), time(11), Some("a note".to_owned()))
        .expect("write");
    store
        .append_event(event(&b, 1, "cris"), time(12), None)
        .expect("write");
    let events = store.events(&a_id).expect("read");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, event(&a, 1, "ana"));
    assert_eq!(events[0].received_at, time(10));
    assert_eq!(events[0].integrity, None);
    assert_eq!(events[1].event, event(&a, 2, "bob"));
    assert_eq!(events[1].integrity.as_deref(), Some("a note"));

    // Sticks: added once, never replaced, and read back whole.
    let new = |token: &str| ck_hub::Stick {
        stick_id: a_id.clone(),
        token: Token::new(token),
        team: "Alpha".to_owned(),
        site: "Lab 1".to_owned(),
        contest: "Spring".to_owned(),
        machine_number: Some(4),
    };
    assert!(store.stick(&a_id).expect("read").is_none());
    assert!(store.add_stick(new("first-secret-value-1")).expect("write"));
    assert!(
        !store.add_stick(new("another-secret-value")).expect("write"),
        "never replaced"
    );
    let kept = store.stick(&a_id).expect("read").expect("a stick");
    assert_eq!(kept.token, Token::new("first-secret-value-1"));
    assert_eq!(
        (kept.team.as_str(), kept.machine_number),
        ("Alpha", Some(4))
    );
    assert!(
        store
            .sticks()
            .expect("read")
            .iter()
            .any(|s| s.stick_id == a_id)
    );
}

#[test]
fn the_memory_store_keeps_its_contract() {
    the_store_contract(&MemoryStore::default());
}

#[test]
fn the_postgres_store_keeps_its_contract() {
    let Some(url) = database_url() else {
        eprintln!("skipped: set CK_TEST_DATABASE_URL to run against PostgreSQL");
        return;
    };
    the_store_contract(&PostgresStore::connect(&url).expect("connect"));
}

#[test]
fn postgres_keeps_data_across_a_reconnect_and_migrations_can_run_again() {
    let Some(url) = database_url() else { return };
    let id = unique();
    let stick = format!("{id}-a");
    let id_stick = StickId::parse(&stick).expect("id");

    let first = PostgresStore::connect(&url).expect("connect");
    first
        .record_heartbeat(heartbeat(&stick, "boot-1", 4), time(1))
        .expect("write");
    first
        .append_event(event(&stick, 1, "ana"), time(2), None)
        .expect("write");
    drop(first);

    // A new connection, as after a restart of the Hub: the schema is already there and so is the data.
    let second = PostgresStore::connect(&url).expect("connect again");
    assert_eq!(
        second
            .last_heartbeat(&id_stick)
            .expect("read")
            .expect("kept")
            .heartbeat
            .seq,
        4
    );
    assert_eq!(second.events(&id_stick).expect("read").len(), 1);
}

#[test]
fn postgres_never_changes_or_deletes_an_event() {
    let Some(url) = database_url() else { return };
    let store = PostgresStore::connect(&url).expect("connect");
    let stick = format!("{}-a", unique());
    store
        .append_event(event(&stick, 1, "ana"), time(2), None)
        .expect("write");

    // Even with direct access to the table, the database itself refuses.
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    let update = client
        .execute(
            "UPDATE events SET integrity = 'changed' WHERE stick_id = $1",
            &[&stick],
        )
        .expect_err("an update must fail");
    assert!(update.to_string().contains("append-only") || update.as_db_error().is_some());
    let delete = client
        .execute("DELETE FROM events WHERE stick_id = $1", &[&stick])
        .expect_err("a delete must fail");
    assert!(delete.as_db_error().is_some());
    assert_eq!(
        store
            .events(&StickId::parse(&stick).expect("id"))
            .expect("read")
            .len(),
        1
    );
}

/// The Hub's rules on top of PostgreSQL: after a restart it still knows what it accepted, so a
/// heartbeat it already has is refused and the event chain carries on.
#[test]
fn a_restarted_hub_on_postgres_remembers_what_it_accepted() {
    let Some(url) = database_url() else { return };
    let stick = format!("{}-a", unique());
    let dir = TempDir::new().expect("temp dir");
    fs::write(
        dir.path().join("registry.json"),
        format!(
            r#"{{"sticks":[{{"stick_id":"{stick}","token":"secret","team":"T","site":"S","contest":"C","machine_number":1}}]}}"#
        ),
    )
    .expect("registry");
    let token = Token::new("secret");
    let start = || {
        Hub::new(
            Registry::open(dir.path().join("registry.json")).expect("registry"),
            Box::new(PostgresStore::connect(&url).expect("connect")),
            None,
            Box::new(Utc::now),
        )
    };
    let send_heartbeat = |hub: &Hub, seq: u64| {
        let body = serde_json::to_vec(&heartbeat(&stick, "boot-1", seq)).expect("json");
        hub.accept_heartbeat(Some(&stick), Some(&ck_crypto::sign(&token, &body)), &body)
    };

    let before = start();
    assert_eq!(send_heartbeat(&before, 1), Ok(1));
    assert_eq!(send_heartbeat(&before, 2), Ok(2));
    drop(before);

    let after = start();
    assert!(
        matches!(send_heartbeat(&after, 2), Err(Rejection::Conflict(_))),
        "already known"
    );
    assert_eq!(send_heartbeat(&after, 3), Ok(3));
    let listed = after.sticks().expect("list");
    assert_eq!(listed[0].last.as_ref().map(|l| l.heartbeats), Some(3));
}

/// The database client blocks, so the server must not call it on its request threads. Going through
/// the real router on PostgreSQL is what shows that.
#[tokio::test]
async fn a_hub_on_postgres_answers_real_http_requests() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let Some(url) = database_url() else { return };
    let stick = format!("{}-a", unique());
    let dir = TempDir::new().expect("temp dir");
    fs::write(
        dir.path().join("registry.json"),
        format!(r#"{{"sticks":[{{"stick_id":"{stick}","token":"secret","team":"T","site":"S","contest":"C","machine_number":1}}]}}"#),
    )
    .expect("registry");
    let store = tokio::task::spawn_blocking(move || PostgresStore::connect(&url))
        .await
        .expect("join")
        .expect("connect");
    let hub = Hub::new(
        Registry::open(dir.path().join("registry.json")).expect("registry"),
        Box::new(store),
        None,
        Box::new(Utc::now),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    let organizer = format!("{stick}-organizer");
    let hub = std::sync::Arc::new(hub);
    // The database client blocks, so even a test must not call it from async code.
    let for_user = std::sync::Arc::clone(&hub);
    let organizer_name = organizer.clone();
    tokio::task::spawn_blocking(move || {
        for_user.create_user(&organizer_name, "correct horse battery", Role::Viewer)
    })
    .await
    .expect("join")
    .expect("an organizer");
    let app = ck_hub::router(hub);

    let body = serde_json::to_vec(&heartbeat(&stick, "boot-1", 1)).expect("json");
    let request = Request::post("/api/v1/heartbeat")
        .header("X-CK-Stick", &stick)
        .header(
            "X-CK-Signature",
            ck_crypto::sign(&Token::new("secret"), &body),
        )
        .body(Body::from(body))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let login = Request::post("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "username": organizer, "password": "correct horse battery" })
                .to_string(),
        ))
        .expect("request");
    let login = app.clone().oneshot(login).await.expect("response");
    assert_eq!(
        login.status(),
        StatusCode::OK,
        "sessions work on PostgreSQL too"
    );
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .expect("text")
        .split(';')
        .next()
        .expect("cookie")
        .to_owned();
    let response = app
        .oneshot(
            Request::get("/api/v1/sticks")
                .header("cookie", cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let list: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(list[0]["last"]["seq"], 1);
}
