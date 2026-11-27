//! What a stick sent while the Hub was away, taken in later from the queue.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use ck_hub::{Hub, Inbox, MemoryStore, Registry, Rejection, Store};
use ck_protocol::{Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, RecorderStatus};
use ck_protocol::{RecordingMode, StickId, Token};
use tempfile::TempDir;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

const TOKEN: &str = "token-of-stick-a";
const NOW: &str = "2026-11-12T20:00:00Z";
/// Inside the stick's contest window, which has closed by [`NOW`].
const SENT: &str = "2026-11-12T10:00:00Z";

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).expect("time")
}

fn hub() -> (TempDir, Arc<Hub>, Arc<MemoryStore>) {
    hub_at(NOW)
}

fn hub_at(now: &'static str) -> (TempDir, Arc<Hub>, Arc<MemoryStore>) {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(
        dir.path().join("registry.json"),
        format!(
            r#"{{"sticks":[{{"stick_id":"stick-a","token":"{TOKEN}","team":"A","site":"Lab",
               "contest":"Spring","image":"x.img","created_at":"2026-11-10T15:22:00",
               "credential_scope":{{"contest_id":"7","valid_from":"2026-11-12T08:00:00Z",
               "valid_until":"2026-11-12T15:00:00Z"}}}}]}}"#
        ),
    )
    .expect("registry");
    let registry = Registry::open(dir.path().join("registry.json")).expect("open");
    let store = Arc::new(MemoryStore::default());
    let hub = Hub::new(registry, store.clone(), None, Box::new(move || at(now)));
    (dir, Arc::new(hub), store)
}

/// The events a stick chains in one boot, each naming the one before it.
fn chain(length: u64) -> Vec<Vec<u8>> {
    let mut previous = FIRST_PREV_HASH.to_owned();
    (1..=length)
        .map(|seq| {
            let next = event_after(seq, &previous);
            previous = ck_crypto::sha256_hex(&next.canonical_bytes());
            serde_json::to_vec(&next).expect("json")
        })
        .collect()
}

fn event(seq: u64) -> Vec<u8> {
    serde_json::to_vec(&event_after(seq, FIRST_PREV_HASH)).expect("json")
}

fn event_after(seq: u64, previous: &str) -> Event {
    Event {
        stick_id: StickId::parse("stick-a").expect("id"),
        boot_id: "boot-1".to_owned(),
        seq,
        sent_at: SENT.parse().expect("time"),
        kind: EventKind::Login,
        payload: serde_json::json!({ "user": "ana" }),
        prev_hash: previous.to_owned(),
    }
}

fn heartbeat(seq: u64) -> Vec<u8> {
    serde_json::to_vec(&Heartbeat {
        stick_id: StickId::parse("stick-a").expect("id"),
        boot_id: "boot-1".to_owned(),
        seq,
        sent_at: SENT.parse().expect("time"),
        uptime_s: 60,
        hostname: "pc".to_owned(),
        image_version: "1".to_owned(),
        config_sha256: "0".repeat(64),
        team: "A".to_owned(),
        site: "Lab".to_owned(),
        recording: RecorderStatus {
            mode: RecordingMode::Off,
            state: RecorderState::Off,
        },
        screen_unlock: false,
        config_trust: None,
    })
    .expect("json")
}

fn signature(body: &[u8]) -> String {
    ck_crypto::sign(&Token::new(TOKEN.to_owned()), body)
}

#[test]
fn queued_messages_are_kept_as_of_when_they_were_received() {
    let (_dir, hub, store) = hub();
    let stick = StickId::parse("stick-a").expect("id");
    let org = {
        // The same body sent live, after the window closed, is refused.
        let body = event(1);
        assert_eq!(
            hub.accept_event(Some("stick-a"), Some(&signature(&body)), &body),
            Err(Rejection::Unauthorized)
        );
        store
            .ensure_organization("default", "Default")
            .expect("organization")
            .id
    };

    // Far more than one minute's worth, with the first one arriving last.
    let bodies = chain(200);
    for seq in (2..=200).chain([1, 1]) {
        let body = bodies[usize::try_from(seq - 1).expect("small")].clone();
        let sent = at(SENT) + Duration::seconds(i64::from(seq));
        hub.replay_event(Some("stick-a"), Some(&signature(&body)), &body, sent)
            .expect("kept");
    }
    let kept = store.events(&org, &stick).expect("events");
    let order: Vec<u64> = kept.iter().map(|stored| stored.event.seq).collect();
    assert_eq!(order, (1..=200).collect::<Vec<_>>());
    assert_eq!(kept[0].received_at, at(SENT) + Duration::seconds(1));

    // Nothing is missing, so nothing is flagged; the outage is said instead.
    let view = &hub.sticks(&org).expect("sticks")[0];
    assert_eq!(view.warnings, Vec::<String>::new());
    assert!(
        view.notes[0].contains("was down: 200 reports"),
        "{:?}",
        view.notes
    );

    let body = heartbeat(1);
    hub.replay_heartbeat(Some("stick-a"), Some(&signature(&body)), &body, at(SENT))
        .expect("kept");
    let seen = store
        .last_heartbeat(&org, &stick)
        .expect("read")
        .expect("a heartbeat");
    assert_eq!(seen.received_at, at(SENT));

    // The queue is trusted for the time only: the stick's signature still counts.
    assert_eq!(
        hub.replay_event(Some("stick-a"), Some("00"), &event(201), at(SENT)),
        Err(Rejection::Unauthorized)
    );
}

#[test]
fn a_gap_the_queue_fills_stops_being_a_warning() {
    let (_dir, hub, store) = hub_at(SENT);
    let org = store
        .ensure_organization("default", "Default")
        .expect("organization")
        .id;
    let bodies = chain(3);
    let live = |body: &[u8]| hub.accept_event(Some("stick-a"), Some(&signature(body)), body);
    live(&bodies[0]).expect("first");
    live(&bodies[2]).expect("third, while the second waits");
    assert_eq!(hub.sticks(&org).expect("sticks")[0].warnings.len(), 1);

    hub.replay_event(
        Some("stick-a"),
        Some(&signature(&bodies[1])),
        &bodies[1],
        at(SENT),
    )
    .expect("second, from the queue");
    let view = &hub.sticks(&org).expect("sticks")[0];
    assert_eq!(view.warnings, Vec::<String>::new());
    assert!(
        view.notes[0].contains("was down: 1 reports"),
        "{:?}",
        view.notes
    );
}

/// A queue and an identity endpoint that answer the given requests in turn. Returns the first line
/// of each request it got.
fn serve(
    listener: TcpListener,
    answers: Vec<(String, Vec<u8>)>,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        for (head, body) in answers {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut first = String::new();
            reader.read_line(&mut first).expect("request line");
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().expect("length");
                }
            }
            reader.read_exact(&mut vec![0; length]).expect("body");
            let mut stream = reader.into_inner();
            write!(
                stream,
                "HTTP/1.1 {head}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("head");
            stream.write_all(&body).expect("body");
            seen.push(first.trim().to_owned());
        }
        seen
    })
}

#[test]
fn a_message_is_removed_from_the_queue_once_the_hub_has_it() {
    let (_dir, hub, store) = hub();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let body = event(1);
    // The queue returns a text property in quotes.
    let received = format!(
        "201 Created\r\nLocation: {base}/hub/messages/1/lock\r\nck-path: \"/api/v1/events\"\r\n\
         ck-received-at: \"{SENT}\"\r\nx-ck-stick: \"stick-a\"\r\nx-ck-signature: \"{}\"\r\n",
        signature(&body)
    );
    let elsewhere =
        "201 Created\r\nLocation: http://example.invalid/hub/messages/2/lock\r\n".to_owned();
    let server = serve(
        listener,
        vec![
            ("200 OK\r\n".to_owned(), br#"{"access_token":"t"}"#.to_vec()),
            (received, body),
            ("200 OK\r\n".to_owned(), Vec::new()),
            (elsewhere, Vec::new()),
            ("204 No Content\r\n".to_owned(), Vec::new()),
        ],
    );
    let inbox = Inbox::new(
        &format!("{base}/hub"),
        format!("{base}/token"),
        "header".to_owned(),
        None,
    );

    assert_eq!(inbox.take_one(&hub), Ok(true));
    assert!(inbox.take_one(&hub).is_err(), "a lock address elsewhere");
    assert_eq!(inbox.take_one(&hub), Ok(false));

    let seen = server.join().expect("server");
    assert!(seen[1].starts_with("POST /hub/messages/head?timeout=30 "));
    assert!(seen[2].starts_with("DELETE /hub/messages/1/lock "));
    let org = store
        .ensure_organization("default", "Default")
        .expect("organization")
        .id;
    let stick = StickId::parse("stick-a").expect("id");
    assert_eq!(store.events(&org, &stick).expect("events").len(), 1);
}
