//! The network history of a stick: what its proxy carried or refused, read from its network events.
//!
//! The payload of such an event is `{"entries": [...]}`. Its shape is fixed by the stick, so it is
//! read loosely here: a field that is missing or of another type is left empty.

use std::collections::HashMap;

use ck_protocol::EventKind;
use serde::Serialize;
use serde_json::Value;

use crate::store::StoredEvent;

/// The newest requests that are listed one by one. The totals and the sites count all of them.
const MAX_ENTRIES: usize = 500;
const MAX_HOSTS: usize = 100;

#[derive(Debug, Serialize, PartialEq)]
pub struct NetworkView {
    pub requests: usize,
    /// Refused by the proxy: the site is not one the contest allows.
    pub blocked: usize,
    pub bytes: u64,
    /// The busiest sites first.
    pub hosts: Vec<HostView>,
    /// Newest first.
    pub entries: Vec<EntryView>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct HostView {
    pub host: String,
    pub requests: usize,
    pub blocked: usize,
    pub bytes: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct EntryView {
    /// Seconds since 1970, by the stick's clock.
    pub t: i64,
    pub method: String,
    pub host: String,
    pub path: String,
    pub status: u64,
    pub result: String,
    pub bytes: u64,
    pub ms: u64,
    #[serde(rename = "type")]
    pub content_type: String,
    pub blocked: bool,
    pub boot: String,
}

fn text(entry: &Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// The site without its port, so a tunnel to `site:443` and a request to `site` count together.
fn site(host: &str) -> String {
    let host = host.to_lowercase();
    match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            name.to_owned()
        }
        _ => host,
    }
}

fn number(entry: &Value, key: &str) -> u64 {
    entry.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn entry_of(entry: &Value, boot: &str) -> EntryView {
    let result = text(entry, "result");
    EntryView {
        t: entry.get("t").and_then(Value::as_i64).unwrap_or(0),
        method: text(entry, "method"),
        host: site(&text(entry, "host")),
        path: text(entry, "path"),
        status: number(entry, "status"),
        blocked: result.contains("DENIED"),
        result,
        bytes: number(entry, "bytes"),
        ms: number(entry, "ms"),
        content_type: text(entry, "type"),
        boot: boot.chars().take(8).collect(),
    }
}

/// Everything the stick's network events say, in the order they were received.
pub fn view(stored: &[StoredEvent]) -> NetworkView {
    let mut all: Vec<EntryView> = Vec::new();
    for item in stored.iter().filter(|s| s.event.kind == EventKind::Network) {
        let Some(entries) = item.event.payload.get("entries").and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            all.push(entry_of(entry, &item.event.boot_id));
        }
    }
    let mut sites: HashMap<&str, HostView> = HashMap::new();
    let mut bytes = 0;
    let mut blocked = 0;
    for entry in &all {
        bytes += entry.bytes;
        let site = sites.entry(&entry.host).or_insert_with(|| HostView {
            host: entry.host.clone(),
            requests: 0,
            blocked: 0,
            bytes: 0,
        });
        site.requests += 1;
        site.bytes += entry.bytes;
        if entry.blocked {
            site.blocked += 1;
            blocked += 1;
        }
    }
    let mut hosts: Vec<HostView> = sites.into_values().collect();
    hosts.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.host.cmp(&b.host))
    });
    hosts.truncate(MAX_HOSTS);
    let requests = all.len();
    let mut entries = all;
    entries.reverse();
    entries.truncate(MAX_ENTRIES);
    NetworkView {
        requests,
        blocked,
        bytes,
        hosts,
        entries,
    }
}

/// How many requests the stick's proxy carried and how many it refused, for the list of sticks.
pub fn counts(stored: &[StoredEvent]) -> (usize, usize) {
    let seen = view(stored);
    (seen.requests, seen.blocked)
}
