//! The `VJudge` watcher against a small local server that answers like the status list of `VJudge`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ck_hub::{
    CodeforcesError, JudgeKind, JudgeSubmission, MemoryStore, Page, Pages, Store, VjudgeClient,
    backfill, poll_once,
};

/// Serves `pages[n]` for `start = n * length` (an empty list after the last) with the given status,
/// and records every request line.
fn serve(pages: Vec<String>, length: u64, status: u16) -> (String, Arc<Mutex<Vec<String>>>) {
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
            let start: u64 = line
                .split("start=")
                .nth(1)
                .and_then(|rest| rest.split(['&', ' ']).next())
                .and_then(|number| number.parse().ok())
                .unwrap_or(0);
            let page = usize::try_from(start / length).expect("page");
            let body = pages
                .get(page)
                .cloned()
                .unwrap_or_else(|| r#"{"data":[]}"#.to_owned());
            write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("answer");
        }
    });
    (base, seen)
}

fn row(id: u64, seconds: i64) -> String {
    format!(
        r#"{{"runId":{id},"time":{},"userName":"a","status":"Accepted","contestId":871412,"contestNum":"C"}}"#,
        (1_800_000_000 + seconds) * 1000
    )
}

fn page(rows: &[String]) -> String {
    format!(r#"{{"data":[{}]}}"#, rows.join(","))
}

fn organization(store: &MemoryStore) -> String {
    store
        .create_organization("org", "Org")
        .expect("write")
        .expect("new")
        .id
}

#[test]
fn keeps_every_page_under_vjudge_and_asks_for_the_contest_newest_first() {
    let (base, seen) = serve(
        vec![
            page(&[row(4, 30), row(3, 20)]),
            page(&[row(2, 10), row(1, 0)]),
            page(&[]),
        ],
        2,
        200,
    );
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = VjudgeClient::new(&base);

    let new = poll_once(
        &client,
        &store,
        &org,
        JudgeKind::Vjudge,
        871_412,
        2,
        Duration::ZERO,
    )
    .expect("polled");

    assert_eq!(new, 4);
    let kept = store
        .judge_submissions(&org, "vjudge", 871_412)
        .expect("read");
    let ids: Vec<u64> = kept.iter().map(|s| s.submission_id).collect();
    assert_eq!(ids, vec![1, 2, 3, 4]);
    assert_eq!(kept[0].submitted_at.unix_timestamp(), 1_800_000_000);
    assert_eq!(kept[0].problem_index, "C");
    // The same contest number under Codeforces is another contest: nothing is kept there.
    assert_eq!(
        store
            .judge_submissions(&org, "codeforces", 871_412)
            .expect("read")
            .len(),
        0
    );
    let first = seen.lock().expect("log")[0].clone();
    assert!(
        first.contains("contestId=871412") && first.contains("orderBy=run_id"),
        "{first}"
    );
}

#[test]
fn a_full_page_with_a_row_that_cannot_be_kept_is_not_the_last_page() {
    let unusable =
        r#"{"runId":9,"time":1800000000000,"userName":"a","contestId":null,"contestNum":null}"#;
    let (base, _) = serve(
        vec![
            page(&[row(4, 30), unusable.to_owned()]),
            page(&[row(3, 20)]),
            page(&[]),
        ],
        2,
        200,
    );
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = VjudgeClient::new(&base);
    let new = poll_once(
        &client,
        &store,
        &org,
        JudgeKind::Vjudge,
        871_412,
        2,
        Duration::ZERO,
    )
    .expect("polled");
    assert_eq!(new, 2, "the row after the unusable one is kept too");
}

#[test]
fn a_challenge_page_keeps_nothing_and_is_an_error() {
    let (base, _) = serve(vec!["<html>Just a moment...</html>".to_owned()], 2, 403);
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = VjudgeClient::new(&base);
    let error = poll_once(
        &client,
        &store,
        &org,
        JudgeKind::Vjudge,
        871_412,
        2,
        Duration::ZERO,
    )
    .expect_err("refused");
    assert!(error.to_string().contains("403"), "{error}");
    assert_eq!(
        store
            .judge_submissions(&org, "vjudge", 871_412)
            .expect("read")
            .len(),
        0
    );
}

#[test]
fn a_down_server_is_an_error_that_keeps_nothing() {
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", closed.local_addr().expect("addr"));
    drop(closed);
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = VjudgeClient::new(&base);
    assert!(
        poll_once(
            &client,
            &store,
            &org,
            JudgeKind::Vjudge,
            1,
            2,
            Duration::ZERO
        )
        .is_err()
    );
}

#[test]
fn a_page_of_only_unusable_rows_does_not_end_the_poll() {
    let unusable =
        r#"{"runId":9,"time":1800000000000,"userName":"a","contestId":null,"contestNum":null}"#;
    let (base, _) = serve(
        vec![
            page(&[unusable.to_owned(), unusable.to_owned()]),
            page(&[row(3, 20)]),
            page(&[]),
        ],
        2,
        200,
    );
    let store = MemoryStore::default();
    let org = organization(&store);
    let client = VjudgeClient::new(&base);
    let new = poll_once(
        &client,
        &store,
        &org,
        JudgeKind::Vjudge,
        871_412,
        2,
        Duration::ZERO,
    )
    .expect("polled");
    assert_eq!(new, 1, "the usable row behind two unusable ones is kept");
}

/// Two pages of two; the second fails once.
struct FailsOnce {
    failed: std::cell::Cell<bool>,
}

impl Pages for FailsOnce {
    fn page(&self, _: u64, from: u64, _: u64) -> Result<Page, CodeforcesError> {
        let rows = |ids: [u64; 2]| {
            ids.map(|id| JudgeSubmission {
                submission_id: id,
                contest_id: 1,
                handles: vec!["a".to_owned()],
                problem_index: "A".to_owned(),
                submitted_at: time::OffsetDateTime::from_unix_timestamp(
                    1_800_000_000 + i64::try_from(id).expect("small"),
                )
                .expect("time"),
            })
            .to_vec()
        };
        match from {
            1 => Ok(Page {
                rows: rows([4, 3]),
                raw: 2,
            }),
            3 if !self.failed.replace(true) => Err(CodeforcesError::Request("down".to_owned())),
            3 => Ok(Page {
                rows: rows([2, 1]),
                raw: 2,
            }),
            _ => Ok(Page {
                rows: Vec::new(),
                raw: 0,
            }),
        }
    }
}

#[test]
fn after_a_failed_page_a_backfill_reads_what_the_next_poll_would_skip() {
    let store = MemoryStore::default();
    let org = organization(&store);
    let judge = FailsOnce {
        failed: std::cell::Cell::new(false),
    };
    poll_once(
        &judge,
        &store,
        &org,
        JudgeKind::Vjudge,
        1,
        2,
        Duration::ZERO,
    )
    .expect_err("page 2 fails");
    // The first page is kept now, so an ordinary poll stops at it and never reaches page 2.
    let stuck = poll_once(
        &judge,
        &store,
        &org,
        JudgeKind::Vjudge,
        1,
        2,
        Duration::ZERO,
    )
    .expect("polled");
    assert_eq!(stuck, 0);
    assert_eq!(
        store
            .judge_submissions(&org, "vjudge", 1)
            .expect("read")
            .len(),
        2
    );
    // A backfill reads on, and the pages after it are kept.
    let new = backfill(
        &judge,
        &store,
        &org,
        JudgeKind::Vjudge,
        1,
        2,
        Duration::ZERO,
    )
    .expect("backfilled");
    assert_eq!(new, 2);
    assert_eq!(
        store
            .judge_submissions(&org, "vjudge", 1)
            .expect("read")
            .len(),
        4
    );
}
