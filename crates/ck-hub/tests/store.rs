//! The same tests for every store. PostgreSQL runs only when `CK_TEST_DATABASE_URL` is set, for
//! example `postgres://postgres:password@127.0.0.1:54329/ckhub` against a local Docker database:
//! the shared checks do not start a database of their own.

use std::fs;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use ck_hub::{
    Hub, MemoryStore, NewUser, PasswordCost, PostgresStore, Registry, Rejection, Role, Store,
    TokenPurpose,
};
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
    let org = store
        .create_organization(&format!("{id}-contract"), "Contract")
        .expect("write")
        .expect("a new organization")
        .id;
    let a = format!("{id}-a");
    let b = format!("{id}-b");
    let a_id = StickId::parse(&a).expect("id");

    // Heartbeats: the latest one is kept and they are counted, for each stick on its own.
    assert!(store.last_heartbeat(&org, &a_id).expect("read").is_none());
    store
        .record_heartbeat(&org, heartbeat(&a, "boot-1", 1), time(1))
        .expect("write");
    store
        .record_heartbeat(&org, heartbeat(&a, "boot-1", 2), time(2))
        .expect("write");
    store
        .record_heartbeat(&org, heartbeat(&b, "boot-9", 7), time(3))
        .expect("write");
    let seen = store
        .last_heartbeat(&org, &a_id)
        .expect("read")
        .expect("a heartbeat");
    assert_eq!(seen.heartbeat, heartbeat(&a, "boot-1", 2));
    assert_eq!(seen.received_at, time(2));
    assert_eq!(seen.count, 2);
    let other = store
        .last_heartbeat(&org, &StickId::parse(&b).expect("id"))
        .expect("read")
        .expect("a heartbeat");
    assert_eq!((other.heartbeat.seq, other.count), (7, 1));
    // The highest sequence of every boot is remembered, also after a newer boot started.
    store
        .record_heartbeat(&org, heartbeat(&a, "boot-2", 1), time(4))
        .expect("write");
    assert_eq!(
        store
            .heartbeat_seq_of_boot(&org, &a_id, "boot-1")
            .expect("read"),
        Some(2)
    );
    assert_eq!(
        store
            .heartbeat_seq_of_boot(&org, &a_id, "boot-2")
            .expect("read"),
        Some(1)
    );
    assert_eq!(
        store
            .heartbeat_seq_of_boot(&org, &a_id, "boot-3")
            .expect("read"),
        None
    );

    // Events: kept whole, in the order received, for each stick on its own.
    assert!(store.events(&org, &a_id).expect("read").is_empty());
    store
        .append_event(&org, event(&a, 1, "ana"), time(10), None)
        .expect("write");
    store
        .append_event(
            &org,
            event(&a, 2, "bob"),
            time(11),
            Some("a note".to_owned()),
        )
        .expect("write");
    store
        .append_event(&org, event(&b, 1, "cris"), time(12), None)
        .expect("write");
    let events = store.events(&org, &a_id).expect("read");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, event(&a, 1, "ana"));
    assert_eq!(events[0].received_at, time(10));
    assert_eq!(events[0].integrity, None);
    assert_eq!(events[1].event, event(&a, 2, "bob"));
    assert_eq!(events[1].integrity.as_deref(), Some("a note"));
    let last = store
        .last_event_of_boot(&org, &a_id, &events[1].event.boot_id)
        .expect("read")
        .expect("an event");
    assert_eq!(last.event, event(&a, 2, "bob"));
    assert!(
        store
            .last_event_of_boot(&org, &a_id, "no-such-boot")
            .expect("read")
            .is_none()
    );

    sticks_contract(store, &org, &a_id);
    identity_contract(store);
}

fn sticks_contract(store: &dyn Store, org: &str, a_id: &StickId) {
    // Sticks: added once, never replaced, and read back whole.
    let new = |token: &str| ck_hub::Stick {
        stick_id: a_id.clone(),
        token: Token::new(token),
        team: "Alpha".to_owned(),
        site: "Lab 1".to_owned(),
        contest: "Spring".to_owned(),
        machine_number: Some(4),
    };
    assert!(store.stick(a_id).expect("read").is_none());
    assert!(
        store
            .add_stick(org, new("first-secret-value-1"))
            .expect("write")
    );
    assert!(
        !store
            .add_stick(org, new("another-secret-value"))
            .expect("write"),
        "never replaced"
    );
    let (owner, kept) = store.stick(a_id).expect("read").expect("a stick");
    assert_eq!(owner, org);
    assert_eq!(kept.token, Token::new("first-secret-value-1"));
    assert_eq!(
        (kept.team.as_str(), kept.machine_number),
        ("Alpha", Some(4))
    );
    assert!(
        store
            .sticks(org)
            .expect("read")
            .iter()
            .any(|s| &s.stick_id == a_id)
    );
}

fn identity_contract(store: &dyn Store) {
    let id = unique();
    let email = format!("{id}@example.org");
    let new = |email: &str, hash: &str| NewUser {
        email: email.to_owned(),
        password_hash: hash.to_owned(),
        email_verified_at: Some(time(0)),
        created_at: time(0),
    };

    // Accounts: an email is taken once, capitals do not make another one, and it reads back whole.
    assert!(store.user_by_email(&email).expect("read").is_none());
    let user = store
        .create_user(new(&email, "hash-1"))
        .expect("write")
        .expect("a new account");
    assert!(
        store
            .create_user(new(&email, "hash-2"))
            .expect("write")
            .is_none(),
        "never replaced"
    );
    assert!(
        store
            .create_user(new(&email.to_uppercase(), "hash-3"))
            .expect("write")
            .is_none(),
        "capitals do not matter"
    );
    let kept = store
        .user_by_email(&email.to_uppercase())
        .expect("read")
        .expect("a user");
    assert_eq!(
        (kept.id.as_str(), kept.password_hash.as_str()),
        (user.id.as_str(), "hash-1")
    );
    assert_eq!(
        store.user(&user.id).expect("read").expect("a user").email,
        email
    );
    assert!(store.user("no-such-id").expect("read").is_none());

    // Organizations: a slug is taken once.
    let slug = format!("{id}-org");
    let org = store
        .create_organization(&slug, "Test organization")
        .expect("write")
        .expect("a new organization");
    assert!(
        store
            .create_organization(&slug, "Other")
            .expect("write")
            .is_none()
    );
    assert_eq!(
        store.organization_by_slug(&slug).expect("read"),
        Some(org.clone())
    );
    assert_eq!(
        store.ensure_organization(&slug, "ignored").expect("write"),
        org
    );
    assert!(
        store
            .organization_by_slug("no-such-slug")
            .expect("read")
            .is_none()
    );

    // Memberships: one role per user and organization, never replaced here.
    assert_eq!(store.membership(&org.id, &user.id).expect("read"), None);
    assert!(
        store
            .add_member(&org.id, &user.id, Role::Admin)
            .expect("write")
    );
    assert!(
        !store
            .add_member(&org.id, &user.id, Role::Viewer)
            .expect("write"),
        "the role is not replaced"
    );
    assert_eq!(
        store.membership(&org.id, &user.id).expect("read"),
        Some(Role::Admin)
    );
    let all = store.memberships(&user.id).expect("read");
    assert_eq!(all.len(), 1);
    assert_eq!(
        (all[0].organization.clone(), all[0].role),
        (org, Role::Admin)
    );

    sessions_contract(store, &id, &user.id);
    verification_contract(store);
}

fn verification_contract(store: &dyn Store) {
    let id = unique();
    let made = |email: &str, verified: Option<DateTime<Utc>>| {
        store
            .create_user(NewUser {
                email: email.to_owned(),
                password_hash: "hash".to_owned(),
                email_verified_at: verified,
                created_at: time(10),
            })
            .expect("write")
            .expect("a new account")
    };
    let fresh = made(&format!("{id}-fresh@example.org"), None);
    let vouched = made(&format!("{id}-vouched@example.org"), Some(time(5)));
    assert_eq!(fresh.email_verified_at, None);
    assert_eq!(vouched.email_verified_at, Some(time(5)));

    store
        .mark_email_verified(&fresh.id, time(20))
        .expect("write");
    assert_eq!(
        store
            .user(&fresh.id)
            .expect("read")
            .expect("user")
            .email_verified_at,
        Some(time(20))
    );

    email_tokens_contract(store, &id, &fresh.id);
    passwords_and_cleanup_contract(store, &id, &fresh, &vouched.id);
}

fn email_tokens_contract(store: &dyn Store, id: &str, user: &str) {
    let token = |name: &str| format!("{id}-{name}");
    let fresh = &user;
    // A token is used once, only for its purpose, and not after it ended.
    store
        .put_email_token(&token("verify"), fresh, TokenPurpose::Verify, time(100))
        .expect("write");
    assert_eq!(
        store
            .take_email_token(&token("verify"), TokenPurpose::Reset, time(50))
            .expect("read"),
        None
    );
    assert_eq!(
        store
            .take_email_token(&token("verify"), TokenPurpose::Verify, time(50))
            .expect("read"),
        Some((*fresh).to_owned())
    );
    assert_eq!(
        store
            .take_email_token(&token("verify"), TokenPurpose::Verify, time(50))
            .expect("read"),
        None
    );
    store
        .put_email_token(&token("late"), fresh, TokenPurpose::Reset, time(100))
        .expect("write");
    assert_eq!(
        store
            .take_email_token(&token("late"), TokenPurpose::Reset, time(100))
            .expect("read"),
        None
    );

    // A new token of a purpose replaces the account's earlier one.
    store
        .put_email_token(&token("one"), fresh, TokenPurpose::Reset, time(500))
        .expect("write");
    store
        .put_email_token(&token("two"), fresh, TokenPurpose::Reset, time(500))
        .expect("write");
    assert_eq!(
        store
            .take_email_token(&token("one"), TokenPurpose::Reset, time(1))
            .expect("read"),
        None
    );
    assert_eq!(
        store
            .take_email_token(&token("two"), TokenPurpose::Reset, time(1))
            .expect("read"),
        Some((*fresh).to_owned())
    );
}

fn passwords_and_cleanup_contract(
    store: &dyn Store,
    id: &str,
    fresh: &ck_hub::User,
    vouched: &str,
) {
    let token = |name: &str| format!("{id}-{name}");
    let made = |email: &str| {
        store
            .create_user(NewUser {
                email: email.to_owned(),
                password_hash: "hash".to_owned(),
                email_verified_at: None,
                created_at: time(10),
            })
            .expect("write")
            .expect("a new account")
    };
    // Passwords change, and every session of the account can be ended at once.
    store
        .set_password_hash(&fresh.id, "new-hash")
        .expect("write");
    assert_eq!(
        store
            .user(&fresh.id)
            .expect("read")
            .expect("user")
            .password_hash,
        "new-hash"
    );
    for (name, owner) in [
        ("s1", &fresh.id),
        ("s2", &fresh.id),
        ("s3", &vouched.to_owned()),
    ] {
        store
            .create_session(&token(name), owner, time(900))
            .expect("write");
    }
    store.delete_sessions_of(&fresh.id).expect("write");
    assert!(store.session(&token("s1")).expect("read").is_none());
    assert!(store.session(&token("s2")).expect("read").is_none());
    assert!(store.session(&token("s3")).expect("read").is_some());

    // Expired tokens are removed. Unconfirmed accounts older than the limit are removed with their
    // tokens; confirmed ones stay.
    let stale = made(&format!("{id}-stale@example.org"));
    store
        .put_email_token(&token("old"), &stale.id, TokenPurpose::Verify, time(30))
        .expect("write");
    store.delete_expired_email_tokens(time(40)).expect("write");
    assert_eq!(
        store
            .take_email_token(&token("old"), TokenPurpose::Verify, time(1))
            .expect("read"),
        None
    );
    store
        .delete_unverified_users_before(time(11))
        .expect("write");
    assert!(store.user(&stale.id).expect("read").is_none());
    assert!(store.user(vouched).expect("read").is_some());
    assert!(
        store.user(&fresh.id).expect("read").is_some(),
        "confirmed in the meantime"
    );
}

fn sessions_contract(store: &dyn Store, id: &str, user_id: &str) {
    // Sessions: found by their hash until they are deleted; deleting twice is fine.
    let session = format!("{id}-session");
    assert!(store.session(&session).expect("read").is_none());
    store
        .create_session(&session, user_id, time(100))
        .expect("write");
    assert_eq!(
        store.session(&session).expect("read"),
        Some((user_id.to_owned(), time(100)))
    );
    store.delete_session(&session).expect("delete");
    store.delete_session(&session).expect("delete again");
    assert!(store.session(&session).expect("read").is_none());

    // Expired sessions are removed; the ones still running stay.
    let (old, current) = (format!("{id}-old"), format!("{id}-current"));
    store
        .create_session(&old, user_id, time(50))
        .expect("write");
    store
        .create_session(&current, user_id, time(500))
        .expect("write");
    store.delete_expired_sessions(time(100)).expect("delete");
    assert!(store.session(&old).expect("read").is_none());
    assert!(store.session(&current).expect("read").is_some());
    store.delete_session(&current).expect("delete");
}

/// Organization two never sees what organization one stored, whichever way it asks.
fn organizations_are_kept_apart(store: &dyn Store) {
    let id = unique();
    let make = |tag: &str| {
        store
            .create_organization(&format!("{id}-{tag}"), tag)
            .expect("write")
            .expect("a new organization")
    };
    let (one, two) = (make("one"), make("two"));
    let stick = format!("{id}-s");
    let stick_id = StickId::parse(&stick).expect("id");
    let new = ck_hub::Stick {
        stick_id: stick_id.clone(),
        token: Token::new("a-secret-of-some-length"),
        team: "Alpha".to_owned(),
        site: "Lab 1".to_owned(),
        contest: "Spring".to_owned(),
        machine_number: None,
    };

    assert!(store.add_stick(&one.id, new.clone()).expect("write"));
    assert!(
        !store.add_stick(&two.id, new).expect("write"),
        "a stick id is taken for every organization"
    );
    let (owner, _) = store.stick(&stick_id).expect("read").expect("a stick");
    assert_eq!(owner, one.id, "the lookup names the organization");
    store
        .record_heartbeat(&one.id, heartbeat(&stick, "boot-1", 1), time(1))
        .expect("write");
    store
        .append_event(&one.id, event(&stick, 1, "ana"), time(2), None)
        .expect("write");

    assert_eq!(store.sticks(&one.id).expect("read").len(), 1);
    assert!(store.sticks(&two.id).expect("read").is_empty());
    assert!(
        store
            .last_heartbeat(&one.id, &stick_id)
            .expect("read")
            .is_some()
    );
    assert!(
        store
            .last_heartbeat(&two.id, &stick_id)
            .expect("read")
            .is_none()
    );
    assert_eq!(
        store
            .heartbeat_seq_of_boot(&two.id, &stick_id, "boot-1")
            .expect("read"),
        None
    );
    assert_eq!(store.events(&one.id, &stick_id).expect("read").len(), 1);
    assert!(store.events(&two.id, &stick_id).expect("read").is_empty());
    assert!(
        store
            .last_event_of_boot(&two.id, &stick_id, "boot-1")
            .expect("read")
            .is_none()
    );
}

#[test]
fn the_memory_store_keeps_its_contract() {
    the_store_contract(&MemoryStore::default());
    organizations_are_kept_apart(&MemoryStore::default());
}

#[test]
fn the_postgres_store_keeps_its_contract() {
    let Some(url) = database_url() else {
        eprintln!("skipped: set CK_TEST_DATABASE_URL to run against PostgreSQL");
        return;
    };
    let store = PostgresStore::connect(&url).expect("connect");
    the_store_contract(&store);
    organizations_are_kept_apart(&store);
}

/// The policies themselves, with the Hub's queries out of the way: a role that cannot bypass row
/// security sees only the organization it names, or the one stick it looks up.
#[test]
fn postgres_row_security_hides_other_organizations() {
    let Some(url) = database_url() else { return };
    let store = PostgresStore::connect(&url).expect("connect");
    let id = unique();
    let make = |tag: &str| {
        store
            .create_organization(&format!("{id}-{tag}"), tag)
            .expect("write")
            .expect("a new organization")
    };
    let (one, two) = (make("one"), make("two"));
    let stick = format!("{id}-rls");
    store
        .add_stick(
            &one.id,
            ck_hub::Stick {
                stick_id: StickId::parse(&stick).expect("id"),
                token: Token::new("a-secret-of-some-length"),
                team: String::new(),
                site: String::new(),
                contest: String::new(),
                machine_number: None,
            },
        )
        .expect("write");

    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    client
        .batch_execute(
            "DO $$ BEGIN
                 IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'ck_rls_test') THEN
                     CREATE ROLE ck_rls_test NOSUPERUSER NOBYPASSRLS;
                 END IF;
             END $$;
             GRANT SELECT ON ALL TABLES IN SCHEMA public TO ck_rls_test;",
        )
        .expect("a role that row security applies to");
    let mut transaction = client.transaction().expect("transaction");
    transaction
        .batch_execute("SET LOCAL ROLE ck_rls_test")
        .expect("switch role");
    let visible = |transaction: &mut postgres::Transaction<'_>| -> i64 {
        transaction
            .query_one("SELECT count(*) FROM sticks WHERE stick_id = $1", &[&stick])
            .expect("count")
            .get(0)
    };

    assert_eq!(visible(&mut transaction), 0, "no organization named");
    transaction
        .execute("SELECT set_config('ck.org_id', $1, true)", &[&two.id])
        .expect("set");
    assert_eq!(visible(&mut transaction), 0, "the other organization");
    transaction
        .execute("SELECT set_config('ck.org_id', $1, true)", &[&one.id])
        .expect("set");
    assert_eq!(visible(&mut transaction), 1, "its own organization");
    transaction
        .execute("SELECT set_config('ck.org_id', '', true)", &[])
        .expect("clear");
    transaction
        .execute("SELECT set_config('ck.stick_id', $1, true)", &[&stick])
        .expect("set");
    assert_eq!(visible(&mut transaction), 1, "the lookup of that one stick");
}

#[test]
fn postgres_keeps_data_across_a_reconnect_and_migrations_can_run_again() {
    let Some(url) = database_url() else { return };
    let id = unique();
    let stick = format!("{id}-a");
    let id_stick = StickId::parse(&stick).expect("id");

    let first = PostgresStore::connect(&url).expect("connect");
    let org = first
        .create_organization(&format!("{id}-org"), "Reconnect")
        .expect("write")
        .expect("a new organization")
        .id;
    first
        .record_heartbeat(&org, heartbeat(&stick, "boot-1", 4), time(1))
        .expect("write");
    first
        .append_event(&org, event(&stick, 1, "ana"), time(2), None)
        .expect("write");
    drop(first);

    // A new connection, as after a restart of the Hub: the schema is already there and so is the data.
    let second = PostgresStore::connect(&url).expect("connect again");
    assert_eq!(
        second
            .last_heartbeat(&org, &id_stick)
            .expect("read")
            .expect("kept")
            .heartbeat
            .seq,
        4
    );
    assert_eq!(second.events(&org, &id_stick).expect("read").len(), 1);
}

#[test]
fn postgres_never_changes_or_deletes_an_event() {
    let Some(url) = database_url() else { return };
    let store = PostgresStore::connect(&url).expect("connect");
    let id = unique();
    let stick = format!("{id}-a");
    let org = store
        .create_organization(&format!("{id}-org"), "Append only")
        .expect("write")
        .expect("a new organization")
        .id;
    store
        .append_event(&org, event(&stick, 1, "ana"), time(2), None)
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
            .events(&org, &StickId::parse(&stick).expect("id"))
            .expect("read")
            .len(),
        1
    );
}

/// The id of the organization the registry file's sticks belong to, made if the database has none.
fn default_organization(url: &str) -> String {
    PostgresStore::connect(url)
        .expect("connect")
        .ensure_organization("default", "Default")
        .expect("the default organization")
        .id
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
            Arc::new(PostgresStore::connect(&url).expect("connect")),
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
    let default = default_organization(&url);
    let listed = after.sticks(&default).expect("list");
    let ours = listed
        .iter()
        .find(|view| view.stick_id == stick)
        .expect("the stick of the registry file");
    assert_eq!(ours.last.as_ref().map(|l| l.heartbeats), Some(3));
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().expect("cookie").to_owned()
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
    let store = tokio::task::spawn_blocking(move || {
        let store = PostgresStore::connect(&url).expect("connect");
        store
            .ensure_organization("default", "Default")
            .expect("the default organization");
        store
    })
    .await
    .expect("join");
    let hub = Hub::new(
        Registry::open(dir.path().join("registry.json")).expect("registry"),
        Arc::new(store),
        None,
        Box::new(Utc::now),
    )
    .with_password_cost(PasswordCost {
        memory_kib: 8,
        passes: 1,
    });
    let organizer = format!("{stick}-organizer@example.org");
    let hub = Arc::new(hub);
    // The database client blocks, so even a test must not call it from async code.
    let for_user = Arc::clone(&hub);
    let organizer_email = organizer.clone();
    tokio::task::spawn_blocking(move || {
        for_user
            .create_user(&organizer_email, "correct horse battery")
            .expect("an organizer");
        for_user
            .add_member("default", &organizer_email, Role::Viewer)
            .expect("a member of the default organization");
    })
    .await
    .expect("join");
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
    let cookie = cookie_pair(login.headers()["set-cookie"].to_str().expect("text"));
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
    let ours = list
        .as_array()
        .expect("a list")
        .iter()
        .find(|view| view["stick_id"] == stick.as_str())
        .expect("the stick of the registry file");
    assert_eq!(ours["last"]["seq"], 1);
}

#[test]
fn postgres_says_whether_row_security_applies_to_the_hub_role() {
    let Some(url) = database_url() else { return };
    let store = PostgresStore::connect(&url).expect("connect");
    // The test database user is a superuser, so row security does not apply to it.
    assert!(!store.row_security_applies().expect("read"));
}
