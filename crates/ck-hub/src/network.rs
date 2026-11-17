//! The network history of a stick: what its proxy carried or refused, read from its network events.
//!
//! Only what an organizer can use is shown: the pages that were opened, the forms that were sent,
//! and the requests the proxy refused. The files a page loads, the tunnels and what the browser
//! asks for in the background are counted as hidden, and the same request repeated is one line.
//!
//! The payload of such an event is `{"entries": [...]}`. Its shape is fixed by the stick, so it is
//! read loosely here: a field that is missing or of another type is left empty.

use std::collections::{HashMap, HashSet};

use ck_protocol::EventKind;
use serde::Serialize;
use serde_json::Value;

use crate::store::StoredEvent;

/// The newest requests that are listed one by one. The totals and the sites count all of them.
const MAX_ENTRIES: usize = 500;
const MAX_HOSTS: usize = 100;

#[derive(Debug, Serialize, PartialEq)]
pub struct NetworkView {
    /// The requests that are shown, repeats counted.
    pub requests: usize,
    /// Requests that are left out: files a page loads, tunnels, browser background traffic.
    pub hidden: usize,
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
    /// How many times the same request was made.
    pub count: usize,
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
    let status = number(entry, "status");
    EntryView {
        t: entry.get("t").and_then(Value::as_i64).unwrap_or(0),
        method: text(entry, "method"),
        host: site(&text(entry, "host")),
        path: text(entry, "path"),
        blocked: refused(&result, status),
        status,
        result,
        bytes: number(entry, "bytes"),
        ms: number(entry, "ms"),
        content_type: text(entry, "type"),
        boot: boot.chars().take(8).collect(),
        count: 1,
    }
}

/// The proxy refused it: its own denial, or the error page it makes for a site that is not allowed.
fn refused(result: &str, status: u64) -> bool {
    result.contains("DENIED") || (status == 403 && result.starts_with("NONE"))
}

/// What a browser asks for by itself while it runs; no one on the stick chose it.
const BACKGROUND_SITES: [&str; 12] = [
    "mozilla.com",
    "mozilla.net",
    "mozilla.org",
    "firefox.com",
    "firefox-portal-detection.com",
    "openh264.org",
    "googleapis.com",
    "gstatic.com",
    "googletagmanager.com",
    "google-analytics.com",
    "cloudflareinsights.com",
    "cloudflare.com",
];

fn background(host: &str, path: &str) -> bool {
    // The address bar asking a search engine for suggestions while someone types.
    if host.ends_with("google.com") && path.starts_with("/complete/") {
        return true;
    }
    BACKGROUND_SITES
        .iter()
        .any(|site| host == *site || host.ends_with(&format!(".{site}")))
}

/// A page that was opened or a form that was sent: the answer is a page or a redirect. The scripts,
/// images and polling a page makes are not.
fn useful(entry: &EntryView) -> bool {
    if background(&entry.host, &entry.path) {
        return false;
    }
    if entry.blocked {
        return true;
    }
    if entry.method == "CONNECT" {
        return false;
    }
    entry.content_type.contains("html")
        || ((300..400).contains(&entry.status) && entry.status != 304)
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
    // A refused tunnel to a site that also has a refused request says the same twice.
    let refused_sites: HashSet<String> = all
        .iter()
        .filter(|e| e.blocked && e.method != "CONNECT")
        .map(|e| e.host.clone())
        .collect();
    let total = all.len();
    all.retain(|e| {
        useful(e) && !(e.blocked && e.method == "CONNECT" && refused_sites.contains(&e.host))
    });
    let requests = all.len();

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

    // Newest first; the same request again adds to the count of the line already there.
    let mut entries: Vec<EntryView> = Vec::new();
    let mut seen: HashMap<(String, String, String, u64, bool), usize> = HashMap::new();
    for entry in all.into_iter().rev() {
        let key = (
            entry.method.clone(),
            entry.host.clone(),
            entry.path.clone(),
            entry.status,
            entry.blocked,
        );
        if let Some(&at) = seen.get(&key) {
            entries[at].count += 1;
        } else {
            seen.insert(key, entries.len());
            entries.push(entry);
        }
    }
    entries.truncate(MAX_ENTRIES);
    NetworkView {
        requests,
        hidden: total - requests,
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
