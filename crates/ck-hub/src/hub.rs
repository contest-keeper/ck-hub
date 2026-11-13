//! What the Hub knows about the sticks, and the rules for accepting a heartbeat.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use ck_protocol::{Event, EventKind, FIRST_PREV_HASH, Heartbeat, RecorderState, StickId};
use serde::Serialize;
use serde_json::json;

use crate::accounts::{Accounts, PasswordCost};
use crate::registry::Registry;
use crate::store::{Stick, Store, StoreError, StoredEvent};

/// A stick that has sent nothing for longer than this is shown as offline.
pub const ONLINE_WINDOW_SECONDS: i64 = 60;

/// A stick sends a heartbeat every few seconds and a few events; this leaves room for a burst and
/// stops a stick with a leaked secret from filling the database.
const MAX_MESSAGES_PER_MINUTE: u32 = 120;

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
    /// One stick sent more than [`MAX_MESSAGES_PER_MINUTE`] messages in a minute.
    TooManyRequests,
    /// The Hub could not keep or read the data. The stick tries again later.
    Unavailable,
}

impl From<StoreError> for Rejection {
    fn from(error: StoreError) -> Self {
        tracing::error!("{error}");
        Self::Unavailable
    }
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

pub struct Hub {
    pub(crate) registry: Mutex<Registry>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) accounts: Accounts,
    /// One message at a time is read, judged and kept, so two heartbeats of one stick cannot both
    /// pass the sequence check.
    write: Mutex<()>,
    /// Per stick: when the current minute started and how many messages it has seen.
    rates: Mutex<HashMap<StickId, (DateTime<Utc>, u32)>>,
    log: Option<Mutex<File>>,
    pub(crate) clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl Hub {
    #[must_use]
    pub fn new(
        registry: Registry,
        store: Arc<dyn Store>,
        log: Option<File>,
        clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> Self {
        Self {
            registry: Mutex::new(registry),
            store,
            accounts: Accounts::new(PasswordCost::default()),
            write: Mutex::new(()),
            rates: Mutex::new(HashMap::new()),
            log: log.map(Mutex::new),
            clock,
        }
    }

    /// Changes how much work hashing a password takes, for a machine that can afford more or less.
    #[must_use]
    pub fn with_password_cost(mut self, cost: PasswordCost) -> Self {
        self.accounts = Accounts::new(cost);
        self
    }

    /// Checks a heartbeat and, when it is fine, remembers it. Returns its sequence number.
    pub fn accept_heartbeat(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let result = self.check(stick_header, signature, body);
        self.record(&result, "accepted", "sequence not newer", stick_header);
        result
    }

    fn check(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let id = self.authenticate(stick_header, signature, body)?;
        self.rate_limit(&id)?;
        let heartbeat: Heartbeat = serde_json::from_slice(body)
            .map_err(|_| Rejection::BadRequest("the heartbeat cannot be read"))?;
        if heartbeat.stick_id != id {
            return Err(Rejection::BadRequest("the heartbeat is for another stick"));
        }
        storable(body, heartbeat.seq)?;

        let _one_at_a_time = self.write.lock().expect("write lock");
        // The sequence starts again at every boot, so only a message of the same boot is compared.
        // The highest one of each boot is kept, so an old boot cannot be replayed after a newer
        // one.
        if let Some(top) = self.store.heartbeat_seq_of_boot(&id, &heartbeat.boot_id)?
            && heartbeat.seq <= top
        {
            return Err(Rejection::Conflict(format!(
                "sequence {} is not newer than {top}",
                heartbeat.seq
            )));
        }
        let seq = heartbeat.seq;
        self.store.record_heartbeat(heartbeat, (self.clock)())?;
        Ok(seq)
    }

    /// Counts a message of `id` in the current minute and refuses it past the limit.
    fn rate_limit(&self, id: &StickId) -> Result<(), Rejection> {
        let now = (self.clock)();
        let mut rates = self.rates.lock().expect("rates lock");
        let (since, count) = rates.entry(id.clone()).or_insert((now, 0));
        if (now - *since).num_seconds() >= 60 {
            (*since, *count) = (now, 0);
        }
        *count += 1;
        if *count > MAX_MESSAGES_PER_MINUTE {
            return Err(Rejection::TooManyRequests);
        }
        Ok(())
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
        let from_file = {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            registry.find(&id).map(|entry| entry.token.clone())
        };
        // A stick is known from the registry file or from the database, whichever has it.
        let token = match from_file {
            Some(token) => Some(token),
            None => self.store.stick(&id)?.map(|stick| stick.token),
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
    /// An event the Hub already has is a [`Rejection::Conflict`], which the stick counts as sent.
    pub fn accept_event(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let result = self.check_event(stick_header, signature, body);
        self.record(
            &result,
            "event accepted",
            "event already received",
            stick_header,
        );
        result
    }

    fn check_event(
        &self,
        stick_header: Option<&str>,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<u64, Rejection> {
        let id = self.authenticate(stick_header, signature, body)?;
        self.rate_limit(&id)?;
        let event: Event = serde_json::from_slice(body)
            .map_err(|_| Rejection::BadRequest("the event cannot be read"))?;
        if event.stick_id != id {
            return Err(Rejection::BadRequest("the event is for another stick"));
        }
        storable(body, event.seq)?;

        let _one_at_a_time = self.write.lock().expect("write lock");
        let last_of_boot = self.store.last_event_of_boot(&id, &event.boot_id)?;
        let last_of_boot = last_of_boot.as_ref();
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
        self.store
            .append_event(event, (self.clock)(), integrity.map(str::to_owned))?;
        Ok(seq)
    }

    /// Every stick the admin made, with its last heartbeat when it has sent one.
    pub fn sticks(&self) -> Result<Vec<StickView>, StoreError> {
        let now = (self.clock)();
        let mut entries: Vec<Stick> = {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            registry.all().cloned().collect()
        };
        for stick in self.store.sticks()? {
            if !entries.iter().any(|e| e.stick_id == stick.stick_id) {
                entries.push(stick);
            }
        }
        // The order an organizer reads them in: by contest, then machine number.
        entries.sort_by(|a, b| {
            (&a.contest, a.machine_number, a.stick_id.as_str()).cmp(&(
                &b.contest,
                b.machine_number,
                b.stick_id.as_str(),
            ))
        });
        let mut views = Vec::new();
        for entry in &entries {
            let seen = self.store.last_heartbeat(&entry.stick_id)?;
            let stored = self.store.events(&entry.stick_id)?;
            let boot = seen.as_ref().map(|s| s.heartbeat.boot_id.clone());
            let last = seen.map(|s| LastHeartbeat {
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
            views.push(StickView {
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
                current_user: users_of_boot(&stored, boot.as_deref()).pop(),
                warnings: warnings(&stored, boot.as_deref()),
            });
        }
        Ok(views)
    }

    /// One line per message in the log file: what happened and to which stick, never the secret,
    /// the signature or the body. `accepted` and `conflict` are the words for those two outcomes.
    fn record(
        &self,
        result: &Result<u64, Rejection>,
        accepted: &'static str,
        conflict: &'static str,
        stick: Option<&str>,
    ) {
        let (outcome, seq) = match result {
            Ok(seq) => (accepted, Some(*seq)),
            Err(Rejection::BadRequest(why)) => (*why, None),
            Err(Rejection::Unauthorized) => ("unknown stick or bad signature", None),
            Err(Rejection::Conflict(_)) => (conflict, None),
            Err(Rejection::TooManyRequests) => ("too many messages from this stick", None),
            Err(Rejection::Unavailable) => ("the store failed", None),
        };
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

/// Refuses what PostgreSQL could not keep, so the stick is told once instead of retrying for ever:
/// a sequence above `i64::MAX`, or text with a NUL character.
fn storable(body: &[u8], seq: u64) -> Result<(), Rejection> {
    if i64::try_from(seq).is_err() || body.windows(6).any(|w| w == b"\\u0000") {
        return Err(Rejection::BadRequest("a value cannot be stored"));
    }
    Ok(())
}

/// The user names of the logins in `boot` (the boot of the last heartbeat, or of the last event
/// when there is no heartbeat), in order of login and without repeats of the same name in a row.
fn users_of_boot(stored: &[StoredEvent], boot: Option<&str>) -> Vec<String> {
    let Some(boot) = boot.or_else(|| stored.last().map(|s| s.event.boot_id.as_str())) else {
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
fn warnings(stored: &[StoredEvent], boot: Option<&str>) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut distinct: Vec<String> = users_of_boot(stored, boot);
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
        if let Some(problem) = &item.integrity {
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
