//! A load simulator for the Hub: N fake sticks, each with its own secret, send signed heartbeats and
//! network events the way real sticks do, and the simulator reports how the Hub answered. It is a
//! test tool and is not shipped.
//!
//! Prepare and run (see `docs/capacity.md`):
//!
//! ```text
//! cargo run --release --example loadsim -- registry --sticks 500 --out registry.json
//! ck-hub --database-url ... --registry registry.json --listen 127.0.0.1:8088
//! cargo run --release --example loadsim -- run --hub http://127.0.0.1:8088 \
//!     --registry registry.json --minutes 30
//! ```
//!
//! Per stick: a heartbeat every `--heartbeat-s` seconds (the Hub's default is 15) and one network
//! event of `--entries` requests every `--event-s` seconds, the rate of a stick whose contestant
//! browses and submits. The sticks start spread over one heartbeat period, as real sticks do.
#![allow(
    clippy::print_stdout,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ck_protocol::{
    Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, RecorderStatus, RecordingMode,
    SIGNATURE_HEADER, STICK_HEADER, StickId, Token,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn usage() -> ! {
    eprintln!(
        "usage:\n  loadsim registry --sticks N --out FILE\n  loadsim run --hub URL --registry FILE \
         [--minutes M] [--heartbeat-s S] [--event-s S] [--entries N]"
    );
    std::process::exit(2);
}

fn option(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn number(args: &[String], name: &str, default: u64) -> u64 {
    option(args, name).map_or(default, |v| v.parse().unwrap_or_else(|_| usage()))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("registry") => make_registry(&args),
        Some("run") => run(&args),
        _ => usage(),
    }
}

fn make_registry(args: &[String]) {
    let count = number(args, "--sticks", 100);
    let out = option(args, "--out").unwrap_or_else(|| usage());
    let sticks: Vec<Value> = (1..=count)
        .map(|n| {
            json!({
                "stick_id": format!("sim-{n:04}"),
                "token": format!("simulated-secret-of-stick-{n:04}-0123456789abcdef"),
                "team": format!("Team {n}"), "site": "Simulated lab", "contest": "Load test",
                "image": "sim.img", "created_at": "2026-11-18T10:00:00", "machine_number": n,
            })
        })
        .collect();
    fs::write(
        &out,
        serde_json::to_vec_pretty(&json!({ "sticks": sticks })).expect("json"),
    )
    .expect("write the registry");
    println!("wrote {count} sticks to {out}");
}

/// Latencies in milliseconds and error counts, per kind of call.
#[derive(Default)]
struct Results {
    ok: BTreeMap<&'static str, Vec<f64>>,
    errors: BTreeMap<&'static str, BTreeMap<String, u64>>,
}

fn now_text() -> String {
    OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("zero")
        .format(&Rfc3339)
        .expect("format")
}

fn run(args: &[String]) {
    let hub = option(args, "--hub").unwrap_or_else(|| usage());
    let file = option(args, "--registry").unwrap_or_else(|| usage());
    let minutes = number(args, "--minutes", 30);
    let heartbeat_s = number(args, "--heartbeat-s", 15);
    let event_s = number(args, "--event-s", 10);
    let entries = number(args, "--entries", 20);

    let registry: Value = serde_json::from_slice(&fs::read(&file).expect("read")).expect("json");
    let sticks: Vec<(String, String)> = registry["sticks"]
        .as_array()
        .expect("sticks")
        .iter()
        .map(|s| {
            (
                s["stick_id"].as_str().expect("id").to_owned(),
                s["token"].as_str().expect("token").to_owned(),
            )
        })
        .collect();
    println!(
        "{} sticks, heartbeat every {heartbeat_s} s, an event of {entries} requests every {event_s} s, {minutes} min",
        sticks.len()
    );

    let results = Arc::new(Mutex::new(Results::default()));
    let end = Instant::now() + Duration::from_secs(minutes * 60);
    let handles: Vec<_> = sticks
        .into_iter()
        .enumerate()
        .map(|(index, (id, token))| {
            let (hub, results) = (hub.clone(), Arc::clone(&results));
            thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    stick(
                        &hub,
                        &id,
                        &token,
                        index,
                        heartbeat_s,
                        event_s,
                        entries,
                        end,
                        &results,
                    );
                })
                .expect("a thread")
        })
        .collect();
    for handle in handles {
        let _ = handle.join();
    }
    report(&results.lock().expect("results"), minutes);
}

#[allow(clippy::too_many_arguments)]
fn stick(
    hub: &str,
    id: &str,
    token: &str,
    index: usize,
    heartbeat_s: u64,
    event_s: u64,
    entries: u64,
    end: Instant,
    results: &Mutex<Results>,
) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into();
    let stick_id = StickId::parse(id).expect("id");
    let secret = Token::new(token);
    let boot = format!("boot-{index}");
    // Spread the start over one heartbeat period.
    let offset = Duration::from_millis(index as u64 * 7919 % (heartbeat_s * 1000));
    thread::sleep(offset);

    let (mut heartbeat_seq, mut event_seq) = (0_u64, 0_u64);
    let mut previous_hash = FIRST_PREV_HASH.to_owned();
    let started = Instant::now();
    let (mut next_heartbeat, mut next_event) = (Instant::now(), Instant::now());
    while Instant::now() < end {
        let now = Instant::now();
        if now >= next_heartbeat {
            heartbeat_seq += 1;
            let body = serde_json::to_vec(&Heartbeat {
                stick_id: stick_id.clone(),
                boot_id: boot.clone(),
                seq: heartbeat_seq,
                sent_at: now_text().parse().expect("time"),
                uptime_s: started.elapsed().as_secs(),
                hostname: format!("sim-{index}"),
                image_version: "ck-os loadsim".to_owned(),
                config_sha256: "0".repeat(64),
                team: format!("Team {index}"),
                site: "Simulated lab".to_owned(),
                recording: RecorderStatus {
                    mode: RecordingMode::Low,
                    state: RecorderState::Recording,
                },
            })
            .expect("json");
            send(
                &agent,
                hub,
                "/api/v1/heartbeat",
                id,
                &secret,
                &body,
                "heartbeat",
                results,
            );
            next_heartbeat += Duration::from_secs(heartbeat_s);
        }
        if now >= next_event {
            event_seq += 1;
            let event = Event {
                stick_id: stick_id.clone(),
                boot_id: boot.clone(),
                seq: event_seq,
                prev_hash: previous_hash.clone(),
                sent_at: now_text().parse().expect("time"),
                kind: EventKind::Network,
                payload: network_payload(entries, index, event_seq),
            };
            let body = event.canonical_bytes();
            let ok = send(
                &agent,
                hub,
                "/api/v1/events",
                id,
                &secret,
                &body,
                "event",
                results,
            );
            if ok {
                previous_hash = ck_crypto::sha256_hex(&body);
            } else {
                // A failed event is sent again with the same number, as a real stick does.
                event_seq -= 1;
            }
            next_event += Duration::from_secs(event_s);
        }
        let wake = next_heartbeat.min(next_event).min(end);
        thread::sleep(wake.saturating_duration_since(Instant::now()));
    }
}

/// A batch like a browsing contestant's: pages, a few refused sites and many background files.
fn network_payload(entries: u64, index: usize, seq: u64) -> Value {
    let list: Vec<Value> = (0..entries)
        .map(|i| {
            let refused = i % 11 == 0;
            json!({
                "t": 1_800_000_000_u64 + seq * 10 + i,
                "method": if i % 7 == 0 { "POST" } else { "GET" },
                "host": if refused { "chat.example.net".to_owned() } else { format!("site{}.example.org:443", (index + i as usize) % 9) },
                "path": format!("/problems/{}/{}", seq % 40, i),
                "status": if refused { 403 } else { 200 },
                "result": if refused { "NONE_NONE/403" } else { "TCP_MISS/200" },
                "bytes": 800 + i * 37,
                "ms": 20 + i * 3,
                "type": "text/html",
            })
        })
        .collect();
    json!({ "entries": list })
}

#[allow(clippy::too_many_arguments)]
fn send(
    agent: &ureq::Agent,
    hub: &str,
    path: &str,
    id: &str,
    secret: &Token,
    body: &[u8],
    kind: &'static str,
    results: &Mutex<Results>,
) -> bool {
    let signature = ck_crypto::sign(secret, body);
    let began = Instant::now();
    let answer = agent
        .post(format!("{hub}{path}"))
        .header(STICK_HEADER, id)
        .header(SIGNATURE_HEADER, &signature)
        .send(body);
    let ms = began.elapsed().as_secs_f64() * 1000.0;
    let mut results = results.lock().expect("results");
    match answer {
        Ok(response) if response.status().is_success() => {
            results.ok.entry(kind).or_default().push(ms);
            true
        }
        Ok(response) => {
            *results
                .errors
                .entry(kind)
                .or_default()
                .entry(format!("HTTP {}", response.status().as_u16()))
                .or_default() += 1;
            false
        }
        Err(error) => {
            *results
                .errors
                .entry(kind)
                .or_default()
                .entry(format!("{error}"))
                .or_default() += 1;
            false
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let at = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[at]
}

fn report(results: &Results, minutes: u64) {
    println!("\nkind        ok     errors   p50 ms   p95 ms   p99 ms   max ms   per second");
    for kind in ["heartbeat", "event"] {
        let mut times = results.ok.get(kind).cloned().unwrap_or_default();
        times.sort_by(f64::total_cmp);
        let errors: u64 = results.errors.get(kind).map_or(0, |m| m.values().sum());
        println!(
            "{kind:<10} {:>6} {errors:>8} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>11.1}",
            times.len(),
            percentile(&times, 0.50),
            percentile(&times, 0.95),
            percentile(&times, 0.99),
            times.last().copied().unwrap_or(0.0),
            times.len() as f64 / (minutes as f64 * 60.0),
        );
        if let Some(kinds) = results.errors.get(kind) {
            for (what, n) in kinds {
                println!("  error: {what} x{n}");
            }
        }
    }
}
