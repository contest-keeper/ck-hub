//! The status page the proxy serves: whether the Hub answers now, and what happened to the sticks'
//! reports in each of the last hours. It is public, so it says states and times, never how many.
//!
//! The history comes from real traffic and from looking at the page: an hour in which nothing was
//! sent says so. Nothing probes on a timer, so the proxy and the Hub can sleep when idle.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::header::CACHE_CONTROL;
use axum::response::{Html, IntoResponse};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::inbox::Inbox;
use crate::ingress::Ingress;

/// Hours shown: a week.
const SHOWN_HOURS: i64 = 24 * 7;
/// Hours kept: 90 days.
const KEPT_HOURS: i64 = 24 * 90;
/// How long the answer about the Hub is reused, so the page cannot be used to flood it.
const PROBE_REUSED: Duration = Duration::from_secs(30);
const BLOB_VERSION: &str = "2023-11-03";

#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    /// The Hub answered.
    Delivered,
    /// The Hub could not take it, and the queue has it.
    Queued,
    /// Neither could: the sender kept it.
    Refused,
}

/// Which outcomes an hour has seen.
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
struct Hour {
    delivered: bool,
    queued: bool,
    refused: bool,
}

impl Hour {
    fn state(self) -> &'static str {
        match (self.refused, self.queued, self.delivered) {
            (true, ..) => "outage",
            (_, true, _) => "delayed",
            (_, _, true) => "ok",
            _ => "idle",
        }
    }
}

pub(crate) struct Status {
    /// Hours since 1970, and whether anything changed since the last save.
    hours: Mutex<(BTreeMap<i64, Hour>, bool)>,
    /// The blob the history is kept in.
    url: Option<String>,
    probe: Mutex<Option<(Instant, bool)>>,
    /// A history that could not be read is not overwritten.
    unread: Mutex<bool>,
}

fn hour_of(at: OffsetDateTime) -> i64 {
    at.unix_timestamp().div_euclid(3600)
}

impl Status {
    pub(crate) fn new(url: Option<String>) -> Self {
        Self {
            hours: Mutex::new((BTreeMap::new(), false)),
            url,
            probe: Mutex::new(None),
            unread: Mutex::new(false),
        }
    }

    pub(crate) fn count(&self, at: OffsetDateTime, outcome: Outcome) {
        let (hours, changed) = &mut *self.hours.lock().expect("hours lock");
        let hour = hours.entry(hour_of(at)).or_default();
        let seen = match outcome {
            Outcome::Delivered => &mut hour.delivered,
            Outcome::Queued => &mut hour.queued,
            Outcome::Refused => &mut hour.refused,
        };
        if !*seen {
            *seen = true;
            *changed = true;
        }
        let oldest = hour_of(at) - KEPT_HOURS;
        hours.retain(|hour, _| *hour > oldest);
    }

    /// Adds the kept history to what was counted so far. Returns false when it cannot be read.
    pub(crate) fn load(&self, identity: &Inbox) -> bool {
        let Some(url) = &self.url else { return true };
        let kept = read(url, identity);
        let mut unread = self.unread.lock().expect("unread lock");
        match kept {
            Ok(kept) => {
                let (hours, _) = &mut *self.hours.lock().expect("hours lock");
                for (at, old) in kept {
                    let hour = hours.entry(at).or_default();
                    hour.delivered |= old.delivered;
                    hour.queued |= old.queued;
                    hour.refused |= old.refused;
                }
                *unread = false;
            }
            Err(reason) => {
                tracing::error!("the status history cannot be read: {reason}");
                *unread = true;
            }
        }
        !*unread
    }

    pub(crate) fn save(&self, identity: &Inbox) {
        let Some(url) = &self.url else { return };
        if *self.unread.lock().expect("unread lock") && !self.load(identity) {
            return;
        }
        let body = {
            let (hours, changed) = &mut *self.hours.lock().expect("hours lock");
            if !std::mem::take(changed) {
                return;
            }
            serde_json::to_vec(hours).expect("the history serializes")
        };
        if let Err(reason) = write(url, identity, &body) {
            tracing::error!("the status history cannot be kept: {reason}");
            self.hours.lock().expect("hours lock").1 = true;
        }
    }

    /// The last [`SHOWN_HOURS`] hours up to `now`, oldest first.
    fn shown(&self, now: OffsetDateTime) -> Vec<(OffsetDateTime, &'static str)> {
        let (hours, _) = &*self.hours.lock().expect("hours lock");
        let last = hour_of(now);
        (last - SHOWN_HOURS + 1..=last)
            .map(|at| {
                let start = OffsetDateTime::from_unix_timestamp(at * 3600).unwrap_or(now);
                (start, hours.get(&at).copied().unwrap_or_default().state())
            })
            .collect()
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

fn read(url: &str, identity: &Inbox) -> Result<BTreeMap<i64, Hour>, String> {
    let token = identity.storage_token()?;
    let mut answer = agent()
        .get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("x-ms-version", BLOB_VERSION)
        .call()
        .map_err(|_| "the storage cannot be reached".to_owned())?;
    match answer.status().as_u16() {
        200 => {
            let body = answer
                .body_mut()
                .read_to_vec()
                .map_err(|_| "the kept history cannot be fetched".to_owned())?;
            serde_json::from_slice(&body).map_err(|_| "the kept history is not readable".to_owned())
        }
        // Nothing was kept yet.
        404 => Ok(BTreeMap::new()),
        status => Err(format!("the storage answered {status}")),
    }
}

fn write(url: &str, identity: &Inbox, body: &[u8]) -> Result<(), String> {
    let token = identity.storage_token()?;
    let answer = agent()
        .put(url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("x-ms-version", BLOB_VERSION)
        .header("x-ms-blob-type", "BlockBlob")
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|_| "the storage cannot be reached".to_owned())?;
    match answer.status().as_u16() {
        201 => Ok(()),
        status => Err(format!("the storage answered {status}")),
    }
}

/// Whether the Hub answers, asked at most once in [`PROBE_REUSED`].
async fn hub_up(ingress: Arc<Ingress>) -> bool {
    tokio::task::spawn_blocking(move || {
        // Held while asking, so many viewers at once cause one request.
        let mut probe = ingress.status.probe.lock().expect("probe lock");
        if let Some((at, up)) = *probe
            && at.elapsed() < PROBE_REUSED
        {
            return up;
        }
        let up = ingress.hub_answers();
        *probe = Some((Instant::now(), up));
        if !up {
            ingress
                .status
                .count(OffsetDateTime::now_utc(), Outcome::Queued);
        }
        up
    })
    .await
    .unwrap_or(false)
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .expect("a time formats")
}

pub(crate) async fn data(State(ingress): State<Arc<Ingress>>) -> impl IntoResponse {
    let up = hub_up(Arc::clone(&ingress)).await;
    let hours: Vec<_> = ingress
        .status
        .shown(OffsetDateTime::now_utc())
        .into_iter()
        .map(|(at, state)| serde_json::json!({ "hour": rfc3339(at), "state": state }))
        .collect();
    (
        [(CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "hub": if up { "up" } else { "down" }, "hours": hours })),
    )
}

pub(crate) async fn page(State(ingress): State<Arc<Ingress>>) -> impl IntoResponse {
    let up = hub_up(Arc::clone(&ingress)).await;
    let hours = ingress.status.shown(OffsetDateTime::now_utc());
    (
        [
            (CACHE_CONTROL, "no-store"),
            // One page with its own style and nothing else.
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
        ],
        Html(render(up, &hours)),
    )
}

/// The page. Everything in it is made here: no text from a request goes in.
fn render(up: bool, hours: &[(OffsetDateTime, &'static str)]) -> String {
    let (class, headline) = if up {
        ("ok", "Keeper Hub is working")
    } else {
        (
            "outage",
            "Keeper Hub is down. Reports from sticks are being queued.",
        )
    };
    let mut days = String::new();
    for day in hours.chunk_by(|a, b| a.0.date() == b.0.date()) {
        let date = day[0].0.date();
        write!(days, "<h2>{date}</h2><ol>").expect("writing to a string");
        for (at, state) in day {
            let hour = at.hour();
            let words = match *state {
                "ok" => "working",
                "delayed" => "Hub down or slow",
                "outage" => "reports refused",
                _ => "nothing sent",
            };
            write!(
                days,
                "<li class=\"{state}\" style=\"grid-column:{}\" title=\"{hour:02}:00 UTC, {words}\">\
                 <span>{hour:02}:00 {words}</span></li>",
                hour + 1
            )
            .expect("writing to a string");
        }
        days.push_str("</ol>");
    }
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta http-equiv=\"refresh\" content=\"60\"><title>Keeper status</title><style>{STYLE}</style>\
         </head><body><main><h1 class=\"{class}\">{headline}</h1>\
         <p>Each bar is one hour (UTC).</p>\
         <p class=\"key\"><b class=\"ok\"></b>working <b class=\"delayed\"></b>Hub down or \
         slow <b class=\"outage\"></b>reports refused <b class=\"idle\"></b>nothing sent</p>\
         {days}</main></body></html>"
    )
}

const STYLE: &str = ":root{color-scheme:light dark}\
body{font:16px system-ui,sans-serif;margin:0;padding:16px}main{max-width:720px;margin:auto}\
h1{font-size:1.2rem;color:#fff;padding:16px;border-radius:8px}h2{font-size:.85rem;margin:16px 0 4px}\
ol{display:grid;grid-template-columns:repeat(24,1fr);gap:2px;list-style:none;margin:0;padding:0}\
li{height:32px;border-radius:2px}li span{position:absolute;clip-path:inset(50%);white-space:nowrap}\
.key b{display:inline-block;width:12px;height:12px;margin:0 4px 0 12px}\
.ok{background:#1a7f37}.delayed{background:#9a6700}.outage{background:#b42318}.idle{background:#8c959f66}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_hour_says_the_worst_that_happened_in_it() {
        let status = Status::new(None);
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time");
        let earlier = now - time::Duration::hours(2);
        status.count(earlier, Outcome::Delivered);
        status.count(earlier, Outcome::Queued);
        status.count(now, Outcome::Delivered);
        let latest: Vec<&str> = status
            .shown(now)
            .into_iter()
            .rev()
            .take(3)
            .map(|(_, state)| state)
            .collect();
        assert_eq!(latest, ["ok", "idle", "delayed"]);
        assert!(render(false, &status.shown(now)).contains("class=\"delayed\""));
    }
}
