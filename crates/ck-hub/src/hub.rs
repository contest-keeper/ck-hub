//! What the Hub knows about the sticks, and the rules for accepting a heartbeat.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ck_protocol::{Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, StickId};
use serde::Serialize;
use serde_json::json;

use crate::registry::Registry;

/// A stick that has sent nothing for longer than this is shown as offline.
pub const ONLINE_WINDOW_SECONDS: i64 = 60;

/// Why a heartbeat was not accepted. The caller turns this into an HTTP answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The message itself is wrong: a missing header, unreadable JSON, or a heartbeat for another
    /// stick than the one that signed it.
    BadRequest(&'static str),
    /// The stick is unknown or the signature does not match. The two look the same from outside, so
    /// nobody can use the answer to find out which stick ids exist.
    Unauthorized,
    /// A message that is not newer than the last one of this boot.
    Conflict(String),
}

struct Seen {
    heartbeat: Heartbeat,
    received_at: DateTime<Utc>,
    count: u64,
}

/// The state of every stick, as the page and the API show it.
#[derive(Debug, Serialize, PartialEq)]
pub struct StickView {
    pub stick_id: String,
    pub team: String,
    pub site: String,
    pub contest: String,
    pub machine_number: Option<u32>,
    /// `false` until the first heartbeat, and again after [`ONLINE_WINDOW_SECONDS`] of silence.
    pub online: bool,
    pub last: Option<LastHeartbeat>,
    /// Events received from this stick since the Hub started.
    pub events: usize,
    pub last_event_at: Option<DateTime<Utc>>,
    /// Who logged in last during the stick's latest boot, if anyone did.
    pub current_user: Option<String>,
    /// Things an organizer should look at: a chain problem, two users on one stick.
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct LastHeartbeat {
    pub received_at: DateTime<Utc>,
    pub seconds_ago: i64,
    pub boot_id: String,
    pub seq: u64,
    pub uptime_s: u64,
    pub hostname: String,
    pub image_version: String,
    pub recorder: String,
    /// How many heartbeats were accepted from this stick since the Hub started.
    pub heartbeats: u64,
}

/// An event the Hub received, and what was wrong with its place in the chain, if anything.
struct Stored {
    received_at: DateTime<Utc>,
    event: Event,
    integrity: Option<&'static str>,
}

pub struct Hub {
    registry: Mutex<Registry>,
    events: Mutex<HashMap<StickId, Vec<Stored>>>,
    seen: Mutex<HashMap<StickId, Seen>>,
    log: Option<Mutex<File>>,
    clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl Hub {
    #[must_use]
    pub fn new(
        registry: Registry,
        log: Option<File>,
        clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Self {
        Self {
            registry: Mutex::new(registry),
            seen: Mutex::new(HashMap::new()),
            events: Mutex::new(HashMap::new()),
            log: log.map(Mutex::new),
            clock,
        }
    }

    /// Checks a heartbeat and, when it is fine, remembers it. Returns its sequence number.
    ///
    /// # Errors
    /// See [`Rejection`].
    pub fn accept_heartbeat(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let result = self.check(stick_header, signature, body);
        let (outcome, seq) = match &result {
            Ok(seq) => ("accepted", Some(*seq)),
            Err(Rejection::BadRequest(why)) => (*why, None),
            Err(Rejection::Unauthorized) => ("unknown stick or bad signature", None),
            Err(Rejection::Conflict(_)) => ("sequence not newer", None),
        };
        self.record(outcome, stick_header, seq);
        result
    }

    fn check(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let id = self.authenticate(stick_header, signature, body)?;
        let heartbeat: Heartbeat = serde_json::from_slice(body)
            .map_err(|_| Rejection::BadRequest("the heartbeat cannot be read"))?;
        if heartbeat.stick_id != id {
            return Err(Rejection::BadRequest("the heartbeat is for another stick"));
        }

        let mut seen = self.seen.lock().expect("state lock");
        let previous = seen.get(&id);
        // The sequence starts again at every boot, so only a message of the same boot is compared.
        if let Some(previous) = previous
            && previous.heartbeat.boot_id == heartbeat.boot_id
            && heartbeat.seq <= previous.heartbeat.seq
        {
            return Err(Rejection::Conflict(format!(
                "sequence {} is not newer than {}",
                heartbeat.seq, previous.heartbeat.seq
            )));
        }
        let seq = heartbeat.seq;
        let count = previous.map_or(0, |p| p.count) + 1;
        seen.insert(
            id,
            Seen {
                heartbeat,
                received_at: (self.clock)(),
                count,
            },
        );
        Ok(seq)
    }

    /// Who sent this: the stick named in the header, if the registry knows it and its secret
    /// signed exactly these bytes.
    fn authenticate(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<StickId, Rejection> {
        let (Some(stick_header), Some(signature)) = (stick_header, signature) else {
            return Err(Rejection::BadRequest("a header is missing"));
        };
        let id = StickId::parse(stick_header)
            .map_err(|_| Rejection::BadRequest("the stick header is not a stick id"))?;
        let token = {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            registry.find(&id).map(|entry| entry.token.clone())
        };
        let Some(token) = token else {
            return Err(Rejection::Unauthorized);
        };
        if !ck_crypto::verify(&token, body, signature) {
            return Err(Rejection::Unauthorized);
        }
        Ok(id)
    }

    /// Takes in an event. A problem with the chain never makes the Hub refuse it: the stick would
    /// send it again for ever and block everything behind it. The problem is kept with the event
    /// and shown as a warning, because a gap or a changed event is the thing worth seeing.
    ///
    /// # Errors
    /// [`Rejection::Conflict`] for an event the Hub already has, which the stick counts as sent.
    /// See [`Rejection`] for the rest.
    pub fn accept_event(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let result = self.check_event(stick_header, signature, body);
        let (outcome, seq) = match &result {
            Ok(seq) => ("event accepted", Some(*seq)),
            Err(Rejection::BadRequest(why)) => (*why, None),
            Err(Rejection::Unauthorized) => ("unknown stick or bad signature", None),
            Err(Rejection::Conflict(_)) => ("event already received", None),
        };
        self.record(outcome, stick_header, seq);
        result
    }

    fn check_event(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let id = self.authenticate(stick_header, signature, body)?;
        let event: Event = serde_json::from_slice(body)
            .map_err(|_| Rejection::BadRequest("the event cannot be read"))?;
        if event.stick_id != id {
            return Err(Rejection::BadRequest("the event is for another stick"));
        }

        let mut events = self.events.lock().expect("events lock");
        let stored = events.entry(id).or_default();
        let last_of_boot = stored
            .iter()
            .rev()
            .find(|s| s.event.boot_id == event.boot_id);
        let integrity = match last_of_boot {
            Some(last) if event.seq <= last.event.seq => {
                return Err(Rejection::Conflict(format!(
                    "event {} was already received",
                    event.seq
                )));
            }
            Some(last) if event.seq > last.event.seq + 1 => {
                Some("an event is missing before this one")
            }
            Some(last)
                if event.prev_hash != ck_crypto::sha256_hex(&last.event.canonical_bytes()) =>
            {
                Some("this event does not follow the one before it")
            }
            // The first event of a boot, or the first the Hub has seen after it restarted.
            None if event.seq == 1 && event.prev_hash == FIRST_PREV_HASH => None,
            None if event.seq == 1 => Some("the first event of a boot names a previous one"),
            None => Some("the Hub has no earlier events of this boot"),
            Some(_) => None,
        };
        let seq = event.seq;
        stored.push(Stored {
            received_at: (self.clock)(),
            event,
            integrity,
        });
        Ok(seq)
    }

    /// Every stick the admin made, with its last heartbeat when it has sent one.
    #[must_use]
    pub fn sticks(&self) -> Vec<StickView> {
        let now = (self.clock)();
        let mut registry = self.registry.lock().expect("registry lock");
        registry.refresh();
        let seen = self.seen.lock().expect("state lock");
        let events = self.events.lock().expect("events lock");
        registry
            .all()
            .into_iter()
            .map(|entry| {
                let last = seen.get(&entry.stick_id).map(|s| LastHeartbeat {
                    received_at: s.received_at,
                    seconds_ago: (now - s.received_at).num_seconds().max(0),
                    boot_id: s.heartbeat.boot_id.clone(),
                    seq: s.heartbeat.seq,
                    uptime_s: s.heartbeat.uptime_s,
                    hostname: s.heartbeat.hostname.clone(),
                    image_version: s.heartbeat.image_version.clone(),
                    recorder: recorder_text(s.heartbeat.recording.state).to_owned(),
                    heartbeats: s.count,
                });
                let stored = events.get(&entry.stick_id).map_or(&[][..], Vec::as_slice);
                StickView {
                    stick_id: entry.stick_id.to_string(),
                    team: entry.team.clone(),
                    site: entry.site.clone(),
                    contest: entry.contest.clone(),
                    machine_number: entry.machine_number,
                    online: last
                        .as_ref()
                        .is_some_and(|l| l.seconds_ago <= ONLINE_WINDOW_SECONDS),
                    last,
                    events: stored.len(),
                    last_event_at: stored.last().map(|s| s.received_at),
                    current_user: users_of_latest_boot(stored).pop(),
                    warnings: warnings(stored),
                }
            })
            .collect()
    }

    /// One line per message in the log file: what happened and to which stick, never the secret,
    /// the signature or the body.
    fn record(&self, outcome: &str, stick: Option<&str>, seq: Option<u64>) {
        tracing::info!(outcome, stick, seq, "heartbeat");
        let Some(log) = &self.log else { return };
        let line = json!({
            "at": (self.clock)(),
            "outcome": outcome,
            "stick": stick,
            "seq": seq,
        });
        if let Ok(mut file) = log.lock() {
            let _ = writeln!(file, "{line}");
        }
    }
}

fn recorder_text(state: RecorderState) -> &'static str {
    match state {
        RecorderState::Off => "off",
        RecorderState::Waiting => "waiting",
        RecorderState::Recording => "recording",
        RecorderState::LowDisk => "low disk",
    }
}

/// The user names of the logins in the latest boot that has any events, in order of login and
/// without repeats of the same name in a row.
fn users_of_latest_boot(stored: &[Stored]) -> Vec<String> {
    let Some(boot) = stored.last().map(|s| s.event.boot_id.as_str()) else {
        return Vec::new();
    };
    let mut users: Vec<String> = Vec::new();
    for item in stored.iter().filter(|s| s.event.boot_id == boot) {
        if item.event.kind != EventKind::Login {
            continue;
        }
        if let Some(name) = item.event.payload.get("username").and_then(|v| v.as_str())
            && users.last().map(String::as_str) != Some(name)
        {
            users.push(name.to_owned());
        }
    }
    users
}

/// Why an organizer should look at this stick.
fn warnings(stored: &[Stored]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut distinct: Vec<String> = users_of_latest_boot(stored);
    distinct.sort();
    distinct.dedup();
    if distinct.len() > 1 {
        found.push(format!(
            "{} different users logged in during one boot: {}",
            distinct.len(),
            distinct.join(", ")
        ));
    }
    for item in stored {
        if let Some(problem) = item.integrity {
            let text = format!(
                "event {} of boot {}: {problem}",
                item.event.seq, item.event.boot_id
            );
            if !found.contains(&text) {
                found.push(text);
            }
        }
    }
    found
}
