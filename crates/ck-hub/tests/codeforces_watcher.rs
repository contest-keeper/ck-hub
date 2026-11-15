//! The Codeforces watcher against a small local server that answers like Codeforces does.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ck_hub::{CodeforcesClient, MemoryStore, Store, poll_once};

/// Serves `pages[n]` for `from = n * count + 1` (an empty result after the last) and records
/// every request line.
fn serve(pages: Vec<String>, count: u64) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("connection");
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .expect("request line");
            log.lock().expect("log").push(line.clone());
            let from: u64 = line
                .split("from=")
                .nth(1)
                .and_then(|rest| rest.split(['&', ' ']).next())
                .and_then(|number| number.parse().ok())
                .unwrap_or(1);
            let page = usize::try_from((from - 1) / count).expect("page");
            let body = pages
                .get(page)
                .cloned()
                .unwrap_or_else(|| r#"{"status":"OK","result":[]}"#.to_owned());
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("answer");
        }
    });
    (base, seen)
}

fn row(id: u64, seconds: i64) -> String {
    format!(
        r#"{{"id":{id},"contestId":1900,"creationTimeSeconds":{},"problem":{{"index":"A"}},"author":{{"members":[{{"handle":"a"}}]}}}}"#,
        1_800_000_000 + seconds
    )
}

fn page(rows: &[String]) -> String {
    format!(r#"{{"status":"OK","result":[{}]}}"#, rows.join(","))
}

fn organization(store: &MemoryStore) -> String {
    store
        .create_organization("org", "Org")
        .expect("write")
        .expect("new")
        .id
}

#[test]
fn keeps_every_page_until_the_last_short_one() {
    let (base, _) = serve(
        vec![
            page(&[row(4, 30), row(3, 20)]),
            page(&[row(2, 10), row(1, 0)]),
            page(&[]),
        ],
        2,
    );
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = CodeforcesClient::new(&base, None);

    let new = poll_once(&client, &store, &org, 1900, 2, Duration::ZERO).expect("polled");

    assert_eq!(new, 4);
    let ids: Vec<u64> = store
        .judge_submissions(&org, "codeforces", 1900)
        .expect("read")
        .iter()
        .map(|s| s.submission_id)
        .collect();
    assert_eq!(ids, vec![1, 2, 3, 4]);
}

#[test]
fn a_second_poll_stops_at_what_it_already_has() {
    let (base, seen) = serve(vec![page(&[row(2, 10), row(1, 0)]), page(&[])], 2);
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = CodeforcesClient::new(&base, None);
    poll_once(&client, &store, &org, 1900, 2, Duration::ZERO).expect("polled");
    let before = seen.lock().expect("log").len();

    let new = poll_once(&client, &store, &org, 1900, 2, Duration::ZERO).expect("polled");

    assert_eq!(new, 0);
    assert_eq!(seen.lock().expect("log").len(), before + 1, "one page only");
}

#[test]
fn a_refusal_is_an_error_and_keeps_nothing() {
    let (base, _) = serve(
        vec![r#"{"status":"FAILED","comment":"contestId: not found"}"#.to_owned()],
        2,
    );
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = CodeforcesClient::new(&base, None);

    let error = poll_once(&client, &store, &org, 1900, 2, Duration::ZERO).expect_err("refused");

    assert!(error.to_string().contains("contestId: not found"));
    let kept = store
        .judge_submissions(&org, "codeforces", 1900)
        .expect("read");
    assert!(kept.is_empty());
}

#[test]
fn a_signed_client_sends_the_key_and_signature_but_not_the_secret() {
    let (base, seen) = serve(vec![page(&[])], 2);
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = CodeforcesClient::new(&base, Some(("KEY".to_owned(), "SECRET".to_owned())));

    poll_once(&client, &store, &org, 1900, 2, Duration::ZERO).expect("polled");

    let log = seen.lock().expect("log");
    assert!(log[0].contains("apiKey=KEY"), "{}", log[0]);
    assert!(log[0].contains("apiSig="), "{}", log[0]);
    assert!(!log[0].contains("SECRET"), "{}", log[0]);
}

#[test]
fn an_unreachable_judge_is_an_error_that_does_not_show_the_key() {
    let client = CodeforcesClient::new(
        "http://127.0.0.1:9",
        Some(("KEY".to_owned(), "S".to_owned())),
    );
    let store = MemoryStore::default();
    let org = organization(&store);

    let error = poll_once(&client, &store, &org, 1900, 2, Duration::ZERO).expect_err("down");

    assert!(!error.to_string().contains("KEY"));
}
