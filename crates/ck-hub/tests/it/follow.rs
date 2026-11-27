//! What a stick shows makes the Hub check the judge, with no contest named anywhere.

use std::sync::{Arc, Mutex};

use ck_hub::{CodeforcesError, Hub, Judge, JudgeSubmission, MemoryStore, Registry, Store};
use ck_protocol::{Event, EventKind, FIRST_PREV_HASH, StickId, Token};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};

/// Records who was asked about and answers with one submission no stick reported.
#[derive(Default)]
struct Asked(Mutex<Vec<(u64, String)>>);

impl Judge for Asked {
    fn submissions_of(
        &self,
        contest_id: u64,
        handle: &str,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        self.0
            .lock()
            .expect("asked")
            .push((contest_id, handle.to_owned()));
        Ok(vec![JudgeSubmission {
            submission_id: 9,
            contest_id,
            handles: vec![handle.to_owned()],
            problem_index: "B".to_owned(),
            submitted_at: start(),
        }])
    }
}

fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time")
}

#[test]
fn who_logged_in_is_checked_in_the_opened_contest_and_again_later() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(
        dir.path().join("registry.json"),
        r#"{"sticks":[{"stick_id":"stick-a","token":"secret-of-a","team":"A","site":"Lab",
            "contest":"Spring","image":"x.img","created_at":"2026-11-10T15:22:00"}]}"#,
    )
    .expect("registry");
    let registry = Registry::open(dir.path().join("registry.json")).expect("open");
    let now = Arc::new(Mutex::new(start()));
    let clock = Arc::clone(&now);
    let judge = Arc::new(Asked::default());
    let store = Arc::new(MemoryStore::default());
    let hub = Hub::new(
        registry,
        store.clone(),
        None,
        Box::new(move || *clock.lock().expect("clock")),
    )
    .with_judge(judge.clone());
    // A Hub started again on the same store.
    let restarted = || {
        let registry = Registry::open(dir.path().join("registry.json")).expect("open");
        let clock = Arc::clone(&now);
        Hub::new(
            registry,
            store.clone(),
            None,
            Box::new(move || *clock.lock().expect("clock")),
        )
        .with_judge(judge.clone())
    };
    let pass = |seconds| *now.lock().expect("clock") += Duration::seconds(seconds);
    assert!(!hub.check_next_followed().expect("nobody yet"));

    // A login and an opened contest page, and no submit from the stick at all.
    let seen = [
        (
            EventKind::Login,
            serde_json::json!({"platform": "codeforces", "username": "Ana"}),
        ),
        (
            EventKind::Network,
            serde_json::json!({"entries": [{"host": "codeforces.com",
                "path": "/contest/1900/problem/A"}]}),
        ),
    ];
    for (seq, (kind, payload)) in (1..).zip(seen) {
        let body = serde_json::to_vec(&Event {
            stick_id: StickId::parse("stick-a").expect("id"),
            boot_id: "boot-1".to_owned(),
            seq,
            sent_at: start().into(),
            kind,
            payload,
            prev_hash: FIRST_PREV_HASH.to_owned(),
        })
        .expect("json");
        let signature = ck_crypto::sign(&Token::new("secret-of-a".to_owned()), &body);
        hub.accept_event(Some("stick-a"), Some(&signature), &body)
            .expect("kept");
    }

    assert!(hub.check_next_followed().expect("asked"));
    assert!(!hub.check_next_followed().expect("not due again yet"));
    // The judge's submit 9 came from elsewhere, and the list says so without anyone opening the
    // stick.
    let organization = store
        .ensure_organization("default", "Default")
        .expect("organization")
        .id;
    let sticks = hub.sticks(&organization).expect("sticks");
    assert!(sticks[0].submission_problems > 0);

    pass(61);
    assert!(hub.check_next_followed().expect("asked again"));
    // After a restart the same person is followed, with no new event from the stick.
    let again = restarted();
    again.follow_recent().expect("restored");
    assert!(again.check_next_followed().expect("asked after a restart"));
    pass(7 * 3600);
    assert!(!hub.check_next_followed().expect("forgotten"));
    assert_eq!(
        *judge.0.lock().expect("asked"),
        vec![(1900, "Ana".to_owned()); 3]
    );
}
