//! The store on PostgreSQL, so what sticks reported survives a restart of the Hub.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ck_protocol::{Event, Heartbeat, StickId};
use postgres::{Client, NoTls};

use crate::store::{SeenHeartbeat, Store, StoreError, StoredEvent};

/// The schema, in the order it is applied. A migration that was applied is never run again and
/// never edited: a change is a new file.
const MIGRATIONS: [(&str, &str); 1] = [(
    "0001_heartbeats_and_events",
    include_str!("../migrations/0001_heartbeats_and_events.sql"),
)];

pub struct PostgresStore {
    /// `None` only while the store is being dropped.
    client: Mutex<Option<Client>>,
}

fn failed(error: impl std::fmt::Display) -> StoreError {
    StoreError(error.to_string())
}

impl PostgresStore {
    /// Connects and brings the schema up to date.
    ///
    /// The connection is not encrypted: the database is meant to run next to the Hub, on the same
    /// machine or the same private network.
    ///
    /// # Errors
    /// When the database cannot be reached or a migration fails.
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let mut client = Client::connect(url, NoTls).map_err(failed)?;
        migrate(&mut client)?;
        Ok(Self {
            client: Mutex::new(Some(client)),
        })
    }

    fn client(&self) -> std::sync::MutexGuard<'_, Option<Client>> {
        self.client.lock().expect("database lock")
    }
}

impl Drop for PostgresStore {
    /// Closing the connection blocks and starts a runtime of its own, which panics on a thread that
    /// already runs async tasks, such as the Hub's at shutdown. A plain thread does it safely.
    fn drop(&mut self) {
        if let Ok(slot) = self.client.get_mut()
            && let Some(client) = slot.take()
        {
            let _ = std::thread::spawn(move || drop(client)).join();
        }
    }
}

/// Any number, the same in every Hub: whoever holds this lock is the one applying migrations.
const MIGRATION_LOCK: i64 = 0x0063_6b68_7562; // "ckhub"

fn migrate(client: &mut Client) -> Result<(), StoreError> {
    // Two Hubs starting together must not both apply a migration. The second waits here until the
    // first is done, and then finds everything applied.
    client
        .execute("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK])
        .map_err(failed)?;
    let result = apply_migrations(client);
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK])
        .map_err(failed)?;
    result
}

fn apply_migrations(client: &mut Client) -> Result<(), StoreError> {
    client
        .batch_execute("CREATE TABLE IF NOT EXISTS schema_migrations (name text PRIMARY KEY)")
        .map_err(failed)?;
    for (name, sql) in MIGRATIONS {
        let done = client
            .query_opt("SELECT 1 FROM schema_migrations WHERE name = $1", &[&name])
            .map_err(failed)?
            .is_some();
        if done {
            continue;
        }
        // The migration and the note that it ran are one transaction, so a failure leaves neither.
        let mut transaction = client.transaction().map_err(failed)?;
        transaction.batch_execute(sql).map_err(failed)?;
        transaction
            .execute("INSERT INTO schema_migrations (name) VALUES ($1)", &[&name])
            .map_err(failed)?;
        transaction.commit().map_err(failed)?;
    }
    Ok(())
}

impl Store for PostgresStore {
    fn record_heartbeat(
        &self,
        heartbeat: Heartbeat,
        received_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_value(&heartbeat).map_err(failed)?;
        let seq = i64::try_from(heartbeat.seq).map_err(failed)?;
        self.client()
            .as_mut()
            .expect("the connection is open")
            .execute(
                "INSERT INTO heartbeats (stick_id, boot_id, seq, received_at, count, heartbeat)
                 VALUES ($1, $2, $3, $4, 1, $5)
                 ON CONFLICT (stick_id) DO UPDATE SET
                     boot_id = EXCLUDED.boot_id, seq = EXCLUDED.seq,
                     received_at = EXCLUDED.received_at, heartbeat = EXCLUDED.heartbeat,
                     count = heartbeats.count + 1",
                &[
                    &heartbeat.stick_id.as_str(),
                    &heartbeat.boot_id,
                    &seq,
                    &received_at,
                    &json,
                ],
            )
            .map_err(failed)?;
        Ok(())
    }

    fn last_heartbeat(&self, stick: &StickId) -> Result<Option<SeenHeartbeat>, StoreError> {
        let row = self
            .client()
            .as_mut()
            .expect("the connection is open")
            .query_opt(
                "SELECT heartbeat, received_at, count FROM heartbeats WHERE stick_id = $1",
                &[&stick.as_str()],
            )
            .map_err(failed)?;
        let Some(row) = row else { return Ok(None) };
        let json: serde_json::Value = row.get(0);
        let count: i64 = row.get(2);
        Ok(Some(SeenHeartbeat {
            heartbeat: serde_json::from_value(json).map_err(failed)?,
            received_at: row.get(1),
            count: u64::try_from(count).map_err(failed)?,
        }))
    }

    fn append_event(
        &self,
        event: Event,
        received_at: DateTime<Utc>,
        integrity: Option<String>,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_value(&event).map_err(failed)?;
        let seq = i64::try_from(event.seq).map_err(failed)?;
        self.client()
            .as_mut()
            .expect("the connection is open")
            .execute(
                "INSERT INTO events (stick_id, boot_id, seq, received_at, integrity, event)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &event.stick_id.as_str(),
                    &event.boot_id,
                    &seq,
                    &received_at,
                    &integrity,
                    &json,
                ],
            )
            .map_err(failed)?;
        Ok(())
    }

    fn events(&self, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError> {
        let rows = self
            .client()
            .as_mut()
            .expect("the connection is open")
            .query(
                "SELECT event, received_at, integrity FROM events WHERE stick_id = $1 ORDER BY id",
                &[&stick.as_str()],
            )
            .map_err(failed)?;
        rows.into_iter()
            .map(|row| {
                let json: serde_json::Value = row.get(0);
                Ok(StoredEvent {
                    event: serde_json::from_value(json).map_err(failed)?,
                    received_at: row.get(1),
                    integrity: row.get(2),
                })
            })
            .collect()
    }
}
