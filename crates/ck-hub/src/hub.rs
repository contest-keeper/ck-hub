//! What the Hub knows about the sticks, and the rules for accepting a heartbeat.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ck_protocol::{Heartbeat, RecorderState, StickId};
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

pub struct Hub {
    registry: Mutex<Registry>,
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

    /// Every stick the admin made, with its last heartbeat when it has sent one.
    #[must_use]
    pub fn sticks(&self) -> Vec<StickView> {
        let now = (self.clock)();
        let mut registry = self.registry.lock().expect("registry lock");
        registry.refresh();
        let seen = self.seen.lock().expect("state lock");
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
