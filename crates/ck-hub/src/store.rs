//! Where the Hub keeps what sticks report.
//!
//! The rules (what to accept, what is a warning) live in the Hub. A store only keeps and returns
//! data, so the same rules run on memory in tests and on PostgreSQL in production.
//!
//! Events are append-only: a store never changes or deletes one.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ck_protocol::{Event, Heartbeat, StickId, Token};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
#[error("the store failed: {0}")]
pub struct StoreError(pub String);

/// What an organizer may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Manages contests, sticks and users, and sees everything a viewer sees.
    Admin,
    /// Only looks at the dashboard.
    Viewer,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Viewer => "viewer",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "admin" => Some(Self::Admin),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }

    /// Whether someone with this role may do what `needed` asks for.
    #[must_use]
    pub fn allows(self, needed: Role) -> bool {
        self == Self::Admin || needed == Self::Viewer
    }
}

/// A stick the admin made. The Hub keeps its secret to check what the stick signs. It reads as
/// an entry of the admin tool's registry file, whose other fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Stick {
    pub stick_id: StickId,
    pub token: Token,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub contest: String,
    #[serde(default)]
    pub machine_number: Option<u32>,
}

/// An organizer account. The password is only ever kept as an Argon2 hash.
#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub password_hash: String,
    pub role: Role,
}

/// The latest heartbeat of a stick, when it arrived and how many there have been.
#[derive(Debug, Clone)]
pub struct SeenHeartbeat {
    pub heartbeat: Heartbeat,
    pub received_at: DateTime<Utc>,
    pub count: u64,
}

/// An event as the Hub received it, with what was wrong with its place in the chain, if anything.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub received_at: DateTime<Utc>,
    pub event: Event,
    pub integrity: Option<String>,
}

/// Every method fails with [`StoreError`] when the data cannot be read or written.
pub trait Store: Send + Sync {
    /// Keeps `heartbeat` as the latest of its stick and counts it.
    fn record_heartbeat(
        &self,
        heartbeat: Heartbeat,
        received_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    fn last_heartbeat(&self, stick: &StickId) -> Result<Option<SeenHeartbeat>, StoreError>;

    /// Adds an event after the ones already kept for its stick.
    fn append_event(
        &self,
        event: Event,
        received_at: DateTime<Utc>,
        integrity: Option<String>,
    ) -> Result<(), StoreError>;

    /// Every event of the stick, in the order they were received.
    fn events(&self, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError>;

    /// Adds an account. Returns false, and changes nothing, when the name is taken.
    fn create_user(&self, user: User) -> Result<bool, StoreError>;

    fn user(&self, name: &str) -> Result<Option<User>, StoreError>;

    /// Starts a session. `token_hash` is a hash of the token the browser holds, never the token.
    fn create_session(
        &self,
        token_hash: &str,
        user: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// The user a session belongs to and when it ends, if there is such a session.
    fn session(&self, token_hash: &str) -> Result<Option<(String, DateTime<Utc>)>, StoreError>;

    fn delete_session(&self, token_hash: &str) -> Result<(), StoreError>;

    /// Adds a stick. Returns false, and changes nothing, when its id is already known: a stick's
    /// secret is never replaced.
    fn add_stick(&self, stick: Stick) -> Result<bool, StoreError>;

    fn stick(&self, id: &StickId) -> Result<Option<Stick>, StoreError>;

    /// Every registered stick.
    fn sticks(&self) -> Result<Vec<Stick>, StoreError>;
}

/// Keeps everything in memory. Used by the tests, and by a Hub started without a database; the
/// data is gone when the program stops.
#[derive(Default)]
pub struct MemoryStore {
    heartbeats: Mutex<HashMap<StickId, SeenHeartbeat>>,
    events: Mutex<HashMap<StickId, Vec<StoredEvent>>>,
    users: Mutex<HashMap<String, User>>,
    sessions: Mutex<HashMap<String, (String, DateTime<Utc>)>>,
    sticks: Mutex<HashMap<StickId, Stick>>,
}

impl Store for MemoryStore {
    fn record_heartbeat(
        &self,
        heartbeat: Heartbeat,
        received_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut heartbeats = self.heartbeats.lock().expect("heartbeats lock");
        let count = heartbeats
            .get(&heartbeat.stick_id)
            .map_or(0, |seen| seen.count)
            + 1;
        heartbeats.insert(
            heartbeat.stick_id.clone(),
            SeenHeartbeat {
                heartbeat,
                received_at,
                count,
            },
        );
        Ok(())
    }

    fn last_heartbeat(&self, stick: &StickId) -> Result<Option<SeenHeartbeat>, StoreError> {
        Ok(self
            .heartbeats
            .lock()
            .expect("heartbeats lock")
            .get(stick)
            .cloned())
    }

    fn append_event(
        &self,
        event: Event,
        received_at: DateTime<Utc>,
        integrity: Option<String>,
    ) -> Result<(), StoreError> {
        self.events
            .lock()
            .expect("events lock")
            .entry(event.stick_id.clone())
            .or_default()
            .push(StoredEvent {
                received_at,
                event,
                integrity,
            });
        Ok(())
    }

    fn events(&self, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError> {
        Ok(self
            .events
            .lock()
            .expect("events lock")
            .get(stick)
            .cloned()
            .unwrap_or_default())
    }

    fn create_user(&self, user: User) -> Result<bool, StoreError> {
        let mut users = self.users.lock().expect("users lock");
        if users.contains_key(&user.name) {
            return Ok(false);
        }
        users.insert(user.name.clone(), user);
        Ok(true)
    }

    fn user(&self, name: &str) -> Result<Option<User>, StoreError> {
        Ok(self.users.lock().expect("users lock").get(name).cloned())
    }

    fn create_session(
        &self,
        token_hash: &str,
        user: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .insert(token_hash.to_owned(), (user.to_owned(), expires_at));
        Ok(())
    }

    fn session(&self, token_hash: &str) -> Result<Option<(String, DateTime<Utc>)>, StoreError> {
        Ok(self
            .sessions
            .lock()
            .expect("sessions lock")
            .get(token_hash)
            .cloned())
    }

    fn delete_session(&self, token_hash: &str) -> Result<(), StoreError> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .remove(token_hash);
        Ok(())
    }

    fn add_stick(&self, stick: Stick) -> Result<bool, StoreError> {
        let mut sticks = self.sticks.lock().expect("sticks lock");
        if sticks.contains_key(&stick.stick_id) {
            return Ok(false);
        }
        sticks.insert(stick.stick_id.clone(), stick);
        Ok(true)
    }

    fn stick(&self, id: &StickId) -> Result<Option<Stick>, StoreError> {
        Ok(self.sticks.lock().expect("sticks lock").get(id).cloned())
    }

    fn sticks(&self) -> Result<Vec<Stick>, StoreError> {
        Ok(self
            .sticks
            .lock()
            .expect("sticks lock")
            .values()
            .cloned()
            .collect())
    }
}
