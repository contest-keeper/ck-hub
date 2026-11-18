//! The same tests for every store. PostgreSQL runs only when `CK_TEST_DATABASE_URL` is set, for
//! example `postgres://postgres:password@127.0.0.1:54329/ckhub` against a local Docker database:
//! the shared checks do not start a database of their own.

use std::fs;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ck_hub::{
    Hub, MemoryStore, NewUser, PasswordCost, PostgresStore, Registry, Rejection, Role, Store,
    TokenPurpose,
};
use ck_protocol::{
    Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, RecorderStatus, RecordingMode,
    StickId, Token,
};
use tempfile::TempDir;
use time::OffsetDateTime;

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

fn time(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).expect("time")
}

fn heartbeat(stick: &str, boot: &str, seq: u64) -> Heartbeat {
    Heartbeat {
        stick_id: StickId::parse(stick).expect("id"),
        boot_id: boot.to_owned(),
        seq,
        sent_at: time(0).into(),
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
        sent_at: time(0).into(),
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
    invitations_contract(store);
}

fn verification_contract(store: &dyn Store) {
    let id = unique();
    let made = |email: &str, verified: Option<OffsetDateTime>| {
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

/// Judge submissions are kept once, per organization, and the stick side of the matching can be
/// read back.
fn judge_contract(store: &dyn Store) {
    let id = unique();
    let make = |tag: &str| {
        store
            .create_organization(&format!("{id}-{tag}"), tag)
            .expect("write")
            .expect("a new organization")
            .id
    };
    let (org, other) = (make("judge"), make("elsewhere"));
    let first = ck_hub::JudgeSubmission {
        submission_id: 10,
        contest_id: 1900,
        handles: vec!["tourist".to_owned()],
        problem_index: "A".to_owned(),
        submitted_at: time(0),
    };
    let second = ck_hub::JudgeSubmission {
        submission_id: 20,
        submitted_at: time(60),
        ..first.clone()
    };

    let added = store
        .add_judge_submissions(
            &org,
            "codeforces",
            &[second.clone(), first.clone()],
            time(5),
        )
        .expect("write");
    assert_eq!(added, 2);
    let again = store
        .add_judge_submissions(&org, "codeforces", std::slice::from_ref(&first), time(9))
        .expect("write");
    assert_eq!(again, 0, "a submission seen again changes nothing");

    let kept = store
        .judge_submissions(&org, "codeforces", 1900)
        .expect("read");
    assert_eq!(kept, vec![first.clone(), second]);
    assert_eq!(
        store
            .judge_submissions(&org, "codeforces", 1)
            .expect("read"),
        Vec::new()
    );
    assert_eq!(
        store
            .judge_submissions(&org, "kilonova", 1900)
            .expect("read"),
        Vec::new()
    );
    assert_eq!(
        store
            .judge_submissions(&other, "codeforces", 1900)
            .expect("read"),
        Vec::new()
    );

    // What sticks said: two sticks' Codeforces submits come back in the order received; a login,
    // another judge's submit and another organization's submit do not.
    let submit = |stick: &str, seq: u64, judge: &str| Event {
        kind: EventKind::Submission,
        payload: serde_json::json!({
            "judge": judge, "handle": "tourist", "contest_id": 1900,
            "problem_index": "A", "submission_id": 10,
        }),
        ..event(stick, seq, "x")
    };
    let (a, b) = (format!("{id}-ja"), format!("{id}-jb"));
    store
        .append_event(&org, submit(&a, 1, "codeforces"), time(20), None)
        .expect("write");
    store
        .append_event(&org, event(&a, 2, "login"), time(21), None)
        .expect("write");
    store
        .append_event(&org, submit(&b, 1, "kilonova"), time(22), None)
        .expect("write");
    store
        .append_event(&org, submit(&b, 2, "codeforces"), time(23), None)
        .expect("write");
    let elsewhere = format!("{id}-jc");
    store
        .append_event(&other, submit(&elsewhere, 1, "codeforces"), time(24), None)
        .expect("write");

    let seen = store.submission_events(&org).expect("read");
    let keys: Vec<(String, u64)> = seen.iter().map(|e| (e.key.0.clone(), e.key.2)).collect();
    assert_eq!(keys, vec![(a, 1), (b, 2)]);
    assert_eq!(seen[0].submission_id, Some(10));
}

/// Profiles are kept per organization; saving again replaces the one row, and the picture's bytes
/// come back as they went in.
fn profiles_contract(store: &dyn Store) {
    let id = unique();
    let make = |tag: &str| {
        store
            .create_organization(&format!("{id}-{tag}"), tag)
            .expect("write")
            .expect("a new organization")
    };
    let (one, two) = (make("one"), make("two"));
    let new = |name: &str, wallpaper: Option<(&str, &[u8])>| ck_hub::NewProfile {
        id: "p-1".to_owned(),
        name: name.to_owned(),
        settings: serde_json::json!({ "contest": name }),
        wallpaper: wallpaper.map(|(name, data)| ck_hub::Wallpaper {
            name: name.to_owned(),
            data: data.to_vec(),
        }),
    };

    store
        .put_profile(
            &one.id,
            new("First", Some(("w.png", &[0, 1, 255]))),
            time(0),
        )
        .expect("write");
    store
        .put_profile(&one.id, new("Second", None), time(5))
        .expect("write");
    let listed = store.profiles(&one.id).expect("read");
    assert_eq!(listed.len(), 1, "saving again replaces");
    assert_eq!(
        (listed[0].name.as_str(), listed[0].updated_at),
        ("Second", time(5))
    );
    assert!(
        store
            .profile_wallpaper(&one.id, "p-1")
            .expect("read")
            .is_none()
    );

    store
        .put_profile(
            &one.id,
            new("Second", Some(("w.png", &[0, 1, 255]))),
            time(6),
        )
        .expect("write");
    let kept = store
        .profile(&one.id, "p-1")
        .expect("read")
        .expect("a profile");
    assert_eq!(kept.settings, serde_json::json!({ "contest": "Second" }));
    assert_eq!(kept.wallpaper_name.as_deref(), Some("w.png"));
    let wallpaper = store
        .profile_wallpaper(&one.id, "p-1")
        .expect("read")
        .expect("a picture");
    assert_eq!(wallpaper.data, vec![0, 1, 255]);

    assert_eq!(store.profiles(&two.id).expect("read"), Vec::new());
    assert!(store.profile(&two.id, "p-1").expect("read").is_none());
    assert!(
        store
            .profile_wallpaper(&two.id, "p-1")
            .expect("read")
            .is_none()
    );
    assert!(!store.delete_profile(&two.id, "p-1").expect("write"));
    store
        .put_profile(&two.id, new("Theirs", None), time(7))
        .expect("write");
    assert_eq!(
        store
            .profile(&one.id, "p-1")
            .expect("read")
            .expect("kept")
            .name,
        "Second"
    );

    assert!(store.delete_profile(&one.id, "p-1").expect("write"));
    assert!(store.profile(&one.id, "p-1").expect("read").is_none());
    assert!(store.profile(&two.id, "p-1").expect("read").is_some());
}

#[allow(clippy::too_many_lines)]
fn commands_contract(store: &dyn Store) {
    use ck_hub::{CommandState, StoredCommand};
    use ck_protocol::{AckResult, Action, Command, CommandAck, Refusal};

    let id = unique();
    let make = |tag: &str| {
        store
            .create_organization(&format!("{id}-{tag}"), tag)
            .expect("write")
            .expect("a new organization")
    };
    let (one, two) = (make("one"), make("two"));
    let stick = StickId::parse("stick-a").expect("id");
    let other = StickId::parse("stick-b").expect("id");
    let token = Token::new("secret");
    let command = |name: &str, at: i64, ends: i64| StoredCommand {
        signed: ck_crypto::sign_command(
            &token,
            Command {
                id: format!("{id}{name}"),
                stick_id: stick.clone(),
                issued_at: time(at).into(),
                expires_at: time(ends).into(),
                action: Action::Message {
                    text: "hello".to_owned(),
                },
            },
        ),
        sent_by_user_id: "u-1".to_owned(),
        sent_by_email: "owner@example.org".to_owned(),
        state: CommandState::Queued,
        refusal: None,
        created_at: time(at),
        delivered_at: None,
        finished_at: None,
    };
    let ack = |name: &str, result: AckResult, refusal: Option<Refusal>| CommandAck {
        command_id: format!("{id}{name}"),
        result,
        refusal,
    };

    store
        .queue_command(&one.id, command("a", 0, 300))
        .expect("write");
    store
        .queue_command(&one.id, command("b", 10, 20))
        .expect("write");

    // Open and not expired: both at 15 s, only the first at 25 s.
    let open = store
        .pending_commands(&one.id, &stick, time(15))
        .expect("read");
    assert_eq!(open.len(), 2);
    assert_eq!(open[0].id(), format!("{id}a"));
    assert_eq!(open[0].state, CommandState::Queued);
    assert_eq!(
        open[0].signed,
        command("a", 0, 300).signed,
        "kept exactly as signed"
    );
    assert_eq!(
        store
            .pending_commands(&one.id, &stick, time(25))
            .expect("read")
            .len(),
        1
    );
    assert_eq!(
        store
            .pending_commands(&one.id, &other, time(15))
            .expect("read"),
        Vec::new()
    );
    assert_eq!(
        store
            .pending_commands(&two.id, &stick, time(15))
            .expect("read"),
        Vec::new()
    );

    store
        .mark_commands_delivered(&one.id, &[format!("{id}a")], time(16))
        .expect("write");
    let delivered = store
        .command(&one.id, &format!("{id}a"))
        .expect("read")
        .expect("kept");
    assert_eq!(delivered.state, CommandState::Delivered);
    assert_eq!(delivered.delivered_at, Some(time(16)));
    // Marking again, or from another organization, changes nothing.
    store
        .mark_commands_delivered(&one.id, &[format!("{id}a")], time(99))
        .expect("write");
    store
        .mark_commands_delivered(&two.id, &[format!("{id}b")], time(99))
        .expect("write");
    assert_eq!(
        store
            .command(&one.id, &format!("{id}a"))
            .expect("read")
            .expect("kept")
            .delivered_at,
        Some(time(16))
    );
    assert_eq!(
        store
            .command(&one.id, &format!("{id}b"))
            .expect("read")
            .expect("kept")
            .state,
        CommandState::Queued
    );

    // Only the stick the command is for, in its own organization, can settle it, and once.
    assert!(
        !store
            .record_command_ack(&one.id, &other, &ack("a", AckResult::Ok, None), time(20))
            .expect("write")
    );
    assert!(
        !store
            .record_command_ack(&two.id, &stick, &ack("a", AckResult::Ok, None), time(20))
            .expect("write")
    );
    assert!(
        !store
            .record_command_ack(&one.id, &stick, &ack("nope", AckResult::Ok, None), time(20))
            .expect("write")
    );
    assert!(
        store
            .record_command_ack(&one.id, &stick, &ack("a", AckResult::Ok, None), time(21))
            .expect("write")
    );
    assert!(
        !store
            .record_command_ack(
                &one.id,
                &stick,
                &ack("a", AckResult::Failed, None),
                time(22)
            )
            .expect("write")
    );
    let done = store
        .command(&one.id, &format!("{id}a"))
        .expect("read")
        .expect("kept");
    assert_eq!(done.state, CommandState::Done);
    assert_eq!(done.finished_at, Some(time(21)));

    assert!(
        store
            .record_command_ack(
                &one.id,
                &stick,
                &ack("b", AckResult::Refused, Some(Refusal::Replayed)),
                time(23)
            )
            .expect("write")
    );
    let refused = store
        .command(&one.id, &format!("{id}b"))
        .expect("read")
        .expect("kept");
    assert_eq!(
        (refused.state, refused.refusal),
        (CommandState::Refused, Some(Refusal::Replayed))
    );
    assert_eq!(
        store
            .pending_commands(&one.id, &stick, time(15))
            .expect("read"),
        Vec::new()
    );

    // The history is newest first, limited, and private to the organization.
    let history = store.commands_of(&one.id, &stick, 10).expect("read");
    assert_eq!(
        history.iter().map(StoredCommand::id).collect::<Vec<_>>(),
        [format!("{id}b"), format!("{id}a")]
    );
    assert_eq!(
        store.commands_of(&one.id, &stick, 1).expect("read").len(),
        1
    );
    assert_eq!(
        store.commands_of(&two.id, &stick, 10).expect("read"),
        Vec::new()
    );
    assert!(
        store
            .command(&two.id, &format!("{id}a"))
            .expect("read")
            .is_none()
    );

    // Old history is removed, and only the organization's own.
    store
        .queue_command(&two.id, command("c", 0, 300))
        .expect("write");
    store
        .delete_commands_before(&one.id, time(5))
        .expect("write");
    assert!(
        store
            .command(&one.id, &format!("{id}a"))
            .expect("read")
            .is_none()
    );
    assert!(
        store
            .command(&one.id, &format!("{id}b"))
            .expect("read")
            .is_some()
    );
    assert!(
        store
            .command(&two.id, &format!("{id}c"))
            .expect("read")
            .is_some()
    );
}

#[test]
fn the_memory_store_keeps_its_contract() {
    commands_contract(&MemoryStore::default());
    profiles_contract(&MemoryStore::default());
    the_store_contract(&MemoryStore::default());
    judge_contract(&MemoryStore::default());
    organizations_are_kept_apart(&MemoryStore::default());
}

#[test]
fn the_postgres_store_keeps_its_contract() {
    let Some(url) = database_url() else {
        eprintln!("skipped: set CK_TEST_DATABASE_URL to run against PostgreSQL");
        return;
    };
    let store = PostgresStore::connect(&url).expect("connect");
    commands_contract(&store);
    profiles_contract(&store);
    the_store_contract(&store);
    judge_contract(&store);
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
            Box::new(OffsetDateTime::now_utc),
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
        Box::new(OffsetDateTime::now_utc),
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

/// The Hub switches to a plain role after it migrated, so row security binds it even when it logs in as
/// the server's admin (a superuser here, the server admin in Azure), and again after a reconnect.
#[test]
fn postgres_hub_runs_as_a_role_row_security_applies_to_also_after_a_reconnect() {
    let Some(url) = database_url() else { return };
    let tag = format!("ck-reconnect-{}", unique());
    let joiner = if url.contains('?') { '&' } else { '?' };
    let tagged = format!("{url}{joiner}application_name={tag}");
    let store = PostgresStore::connect(&tagged).expect("connect");
    assert!(store.row_security_applies().expect("read"));

    // End only this store's own connection, so other tests are left alone.
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    let ended = admin
        .query_one(
            "SELECT count(*) FROM (SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                                   WHERE application_name = $1) ended",
            &[&tag],
        )
        .expect("end the connection")
        .get::<_, i64>(0);
    assert_eq!(ended, 1, "one connection of this store");
    // The client only learns the connection is gone when it uses it, so the first call fails (as it
    // always did) and the next one opens a new connection.
    let _ = store.row_security_applies();
    assert!(
        store
            .row_security_applies()
            .expect("read after a reconnect"),
        "the new connection is limited too"
    );
}

#[test]
fn postgres_hub_role_cannot_change_the_schema_or_the_event_log() {
    let Some(url) = database_url() else { return };
    let tagged = format!(
        "{url}{}application_name=ck-limits",
        if url.contains('?') { '&' } else { '?' }
    );
    let _store = PostgresStore::connect(&tagged).expect("connect");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    let mut transaction = client.transaction().expect("transaction");
    transaction
        .batch_execute("SET LOCAL ROLE ck_hub_app")
        .expect("the hub role exists");
    for statement in [
        "CREATE TABLE ck_must_not_exist (x int)",
        "DROP TABLE schema_migrations",
        "UPDATE events SET stick_id = stick_id",
        "DELETE FROM events",
        "UPDATE schema_migrations SET name = name",
    ] {
        transaction.batch_execute("SAVEPOINT s").expect("savepoint");
        assert!(
            transaction.batch_execute(statement).is_err(),
            "the hub role may not run: {statement}"
        );
        transaction
            .batch_execute("ROLLBACK TO SAVEPOINT s")
            .expect("rollback");
    }
}

// One long scenario on purpose: its steps share the same organization and people.
#[allow(clippy::too_many_lines)]
fn invitations_contract(store: &dyn Store) {
    let id = unique();
    let org = store
        .create_organization(&format!("{id}-invite"), "Invite")
        .expect("write")
        .expect("a new organization")
        .id;
    let person = |name: &str| {
        store
            .create_user(NewUser {
                email: format!("{id}-{name}@example.org"),
                password_hash: "hash".to_owned(),
                email_verified_at: Some(time(0)),
                created_at: time(0),
            })
            .expect("write")
            .expect("a new account")
    };
    let (ana, bo, cy) = (person("ana"), person("bo"), person("cy"));
    let hash = |name: &str| format!("{id}-hash-{name}");

    // One open invitation per organization and address, capitals aside: inviting again replaces it.
    let first = store
        .put_invitation(
            &org,
            &bo.email,
            Role::Viewer,
            &hash("one"),
            Some(&ana.id),
            time(100),
        )
        .expect("write");
    let second = store
        .put_invitation(
            &org,
            &bo.email.to_uppercase(),
            Role::Admin,
            &hash("two"),
            None,
            time(200),
        )
        .expect("write");
    assert_ne!(first.id, second.id);
    assert_eq!(second.role, Role::Admin);
    assert_eq!(second.invited_by, None);
    assert_eq!(
        store.invitation_for_token(&hash("one")).expect("read"),
        None,
        "replaced"
    );
    assert_eq!(
        store.invitation_for_token(&hash("two")).expect("read"),
        Some(second.clone())
    );
    assert_eq!(
        store.invitations_of(&org).expect("read"),
        vec![second.clone()]
    );

    // Taking uses it up, and only gives it back before it ended.
    assert_eq!(
        store
            .take_invitation(&hash("two"), time(150))
            .expect("write"),
        Some(second.clone())
    );
    assert_eq!(
        store
            .take_invitation(&hash("two"), time(150))
            .expect("write"),
        None
    );
    store
        .put_invitation(
            &org,
            &cy.email,
            Role::Viewer,
            &hash("late"),
            None,
            time(100),
        )
        .expect("write");
    assert_eq!(
        store
            .take_invitation(&hash("late"), time(100))
            .expect("write"),
        None
    );
    assert_eq!(
        store.invitation_for_token(&hash("late")).expect("read"),
        None,
        "gone either way"
    );

    // Listed by address, revoked by id within the organization only.
    let kept = store
        .put_invitation(
            &org,
            &cy.email,
            Role::Viewer,
            &hash("cy"),
            Some(&ana.id),
            time(300),
        )
        .expect("write");
    let other_org = store
        .create_organization(&format!("{id}-other"), "Other")
        .expect("write")
        .expect("a new organization")
        .id;
    assert!(
        !store
            .delete_invitation(&other_org, &kept.id)
            .expect("write")
    );
    assert!(store.delete_invitation(&org, &kept.id).expect("write"));
    assert!(!store.delete_invitation(&org, &kept.id).expect("write"));
    store
        .put_invitation(&org, &cy.email, Role::Viewer, &hash("old"), None, time(10))
        .expect("write");
    store.delete_expired_invitations(time(50)).expect("write");
    assert_eq!(store.invitations_of(&org).expect("read"), []);

    // Members: listed by email, removed once, owners counted.
    assert!(store.add_member(&org, &ana.id, Role::Owner).expect("write"));
    assert!(store.add_member(&org, &cy.id, Role::Viewer).expect("write"));
    assert!(store.add_member(&org, &bo.id, Role::Admin).expect("write"));
    let listed = store.members_of(&org).expect("read");
    assert_eq!(
        listed
            .iter()
            .map(|m| (m.email.clone(), m.role))
            .collect::<Vec<_>>(),
        vec![
            (ana.email.clone(), Role::Owner),
            (bo.email.clone(), Role::Admin),
            (cy.email.clone(), Role::Viewer),
        ]
    );
    assert_eq!(store.count_owners(&org).expect("read"), 1);
    assert!(store.remove_member(&org, &cy.id).expect("write"));
    assert!(!store.remove_member(&org, &cy.id).expect("write"));
    assert_eq!(store.members_of(&org).expect("read").len(), 2);

    // Staff flag, and the list of organizations.
    assert!(
        !store
            .user(&ana.id)
            .expect("read")
            .expect("user")
            .is_platform_staff
    );
    store.set_staff(&ana.id, true).expect("write");
    assert!(
        store
            .user(&ana.id)
            .expect("read")
            .expect("user")
            .is_platform_staff
    );
    store.set_staff(&ana.id, false).expect("write");
    assert!(
        !store
            .user(&ana.id)
            .expect("read")
            .expect("user")
            .is_platform_staff
    );
    let slugs: Vec<String> = store
        .organizations()
        .expect("read")
        .into_iter()
        .map(|o| o.slug)
        .collect();
    assert!(slugs.contains(&format!("{id}-invite")) && slugs.contains(&format!("{id}-other")));
}
