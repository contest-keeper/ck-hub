//! Where the Hub keeps what sticks report.
//!
//! The rules (what to accept, what is a warning) live in the Hub. A store only keeps and returns
//! data, so the same rules run on memory in tests and on PostgreSQL in production.
//!
//! Events are append-only: a store never changes or deletes one.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ck_protocol::{Event, Heartbeat, StickId};

#[derive(Debug, thiserror::Error)]
#[error("the store failed: {0}")]
pub struct StoreError(pub String);

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

pub trait Store: Send + Sync {
    /// Keeps `heartbeat` as the latest of its stick and counts it.
    ///
    /// # Errors
    /// When the data cannot be written.
    fn record_heartbeat(
        &self,
        heartbeat: Heartbeat,
        received_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// # Errors
    /// When the data cannot be read.
    fn last_heartbeat(&self, stick: &StickId) -> Result<Option<SeenHeartbeat>, StoreError>;

    /// Adds an event after the ones already kept for its stick.
    ///
    /// # Errors
    /// When the data cannot be written.
    fn append_event(
        &self,
        event: Event,
        received_at: DateTime<Utc>,
        integrity: Option<String>,
    ) -> Result<(), StoreError>;

    /// Every event of the stick, in the order they were received.
    ///
    /// # Errors
    /// When the data cannot be read.
    fn events(&self, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError>;
}

/// Keeps everything in memory. Used by the tests, and by a Hub started without a database; the
/// data is gone when the program stops.
#[derive(Default)]
pub struct MemoryStore {
    heartbeats: Mutex<HashMap<StickId, SeenHeartbeat>>,
    events: Mutex<HashMap<StickId, Vec<StoredEvent>>>,
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
}
