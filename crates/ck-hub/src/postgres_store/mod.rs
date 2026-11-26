//! The store on PostgreSQL, so what sticks reported survives a restart of the Hub.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};

use ck_protocol::{CommandAck, Event, Heartbeat, StickId, Token};
use postgres::error::SqlState;
use postgres::types::ToSql;
use postgres::{Client, Row, Statement};
use time::OffsetDateTime;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::commands::{CommandState, StoredCommand, action_type, settled};
use crate::enrol::{CLAIM_RECOVERY, EnrolledStick, NewEnrolmentCode, Redemption};
use crate::judge::{JudgeSubmission, SubmitEvent, submit_event};

use crate::recordings::{Kind, Recording};
use crate::store::{
    BootSpan, Hardware, Invitation, Member, Membership, NewProfile, NewUser, Organization, Profile,
    ProfileSummary, Removal, Role, SeenHeartbeat, Stick, StickHardware, Store, StoreError,
    StoredEvent, TokenPurpose, User, Wallpaper,
};

/// The role the Hub works as once the schema is up to date. See migration 0008.
const HUB_ROLE: &str = "ck_hub_app";

/// Makes a connection work as the plain Hub role. Row-level security then binds it, whoever logged in.
fn limit_to_hub_role(client: &mut Client) -> Result<(), StoreError> {
    client
        .batch_execute(&format!("SET ROLE {HUB_ROLE}"))
        .map_err(failed)
}

/// How many connections the Hub keeps by default. One connection served about 40 messages a
/// second (docs/capacity.md); a managed database allows far more than this.
pub const DEFAULT_POOL_SIZE: usize = 8;

/// A connection that remembers the statements it prepared. Every query text the store uses is a
/// constant, so each is prepared once per connection instead of on every call.
struct Conn {
    client: Client,
    statements: HashMap<String, Statement>,
}

/// Far more than the store has query texts; only a bug that builds texts from data could reach it.
const MAX_STATEMENTS: usize = 256;

impl Conn {
    fn new(client: Client) -> Self {
        Self {
            client,
            statements: HashMap::new(),
        }
    }

    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    fn statement(&mut self, sql: &str) -> Result<Statement, postgres::Error> {
        if let Some(found) = self.statements.get(sql) {
            return Ok(found.clone());
        }
        let prepared = self.client.prepare(sql)?;
        if self.statements.len() >= MAX_STATEMENTS {
            self.statements.clear();
        }
        self.statements.insert(sql.to_owned(), prepared.clone());
        Ok(prepared)
    }

    fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, postgres::Error> {
        let statement = self.statement(sql)?;
        self.client.query(&statement, params)
    }

    fn query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, postgres::Error> {
        let statement = self.statement(sql)?;
        self.client.query_opt(&statement, params)
    }

    fn query_one(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, postgres::Error> {
        let statement = self.statement(sql)?;
        self.client.query_one(&statement, params)
    }

    fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, postgres::Error> {
        let statement = self.statement(sql)?;
        self.client.execute(&statement, params)
    }

    fn batch_execute(&mut self, sql: &str) -> Result<(), postgres::Error> {
        self.client.batch_execute(sql)
    }

    fn transaction(&mut self) -> Result<postgres::Transaction<'_>, postgres::Error> {
        self.client.transaction()
    }
}

pub struct PostgresStore {
    url: String,
    /// Idle slots. `None` is a slot whose connection has yet to be opened.
    pool: Mutex<Vec<Option<Conn>>>,
    available: Condvar,
}

struct ConnectionLease<'a> {
    store: &'a PostgresStore,
    connection: Option<Conn>,
}

impl Drop for ConnectionLease<'_> {
    fn drop(&mut self) {
        // A panic may have left a transaction open. Return the slot without reusing that connection.
        if std::thread::panicking()
            && let Some(connection) = self.connection.take()
        {
            let _ = std::thread::spawn(move || drop(connection)).join();
        }
        self.store
            .pool
            .lock()
            .expect("database pool lock")
            .push(self.connection.take());
        self.store.available.notify_one();
    }
}

/// TLS settings for every connection: the public web roots, no client certificate.
fn tls() -> MakeRustlsConnect {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .expect("the ring provider supports the default protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    MakeRustlsConnect::new(config)
}

fn failed(error: impl std::fmt::Display) -> StoreError {
    StoreError(error.to_string())
}

fn mark_retired(
    transaction: &mut Conn,
    organization: &str,
    id: &StickId,
    now: OffsetDateTime,
) -> Result<bool, postgres::Error> {
    let found = transaction.execute(
        "UPDATE sticks SET retired_at = COALESCE(retired_at, $3)
         WHERE organization_id = $1 AND stick_id = $2",
        &[&organization, &id.as_str(), &now],
    )?;
    Ok(found == 1)
}

fn enrolled_stick(row: &Row) -> EnrolledStick {
    let machine: Option<i32> = row.get(4);
    EnrolledStick {
        organization: row.get(0),
        team: row.get(1),
        site: row.get(2),
        contest: row.get(3),
        machine_number: machine.and_then(|n| u32::try_from(n).ok()),
    }
}

fn recover_enrolment_claim(
    transaction: &mut postgres::Transaction<'_>,
    code_hash: &str,
    now: OffsetDateTime,
    stick: &StickId,
    token: &Token,
) -> Result<Redemption, postgres::Error> {
    transaction.execute(
        "SELECT set_config('ck.stick_id', $1, true)",
        &[&stick.as_str()],
    )?;
    let row = transaction.query_opt(
        "SELECT c.organization_id, c.team, c.site, c.contest, c.machine_number, s.token
         FROM enrolment_codes c
         JOIN sticks s ON s.stick_id = c.claimed_stick_id AND s.organization_id = c.organization_id
         WHERE c.code_hash = $1 AND c.claimed_stick_id = $2
           AND c.used_at > $3 AND s.retired_at IS NULL",
        &[&code_hash, &stick.as_str(), &(now - CLAIM_RECOVERY)],
    )?;
    Ok(match row {
        Some(row) if &Token::new(row.get::<_, String>(5)) == token => {
            Redemption::Done(enrolled_stick(&row))
        }
        _ => Redemption::Invalid,
    })
}

impl PostgresStore {
    /// Connects and brings the schema up to date.
    ///
    /// The connection is encrypted when the database offers it (`sslmode=prefer`, the default) and
    /// always with `sslmode=require`; the certificate is then checked against the public web
    /// roots. A managed database such as Azure Database for PostgreSQL requires it.
    /// `sslmode=disable` keeps the connection plain for a database on the same machine.
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        Self::connect_with_pool(url, DEFAULT_POOL_SIZE)
    }

    /// Like [`connect`](Self::connect), with up to `size` connections (at least one). Only the
    /// first is opened now; the others are opened when calls overlap.
    pub fn connect_with_pool(url: &str, size: usize) -> Result<Self, StoreError> {
        let mut client = Client::connect(url, tls()).map_err(failed)?;
        // Migrations need the account that logged in; everything after them runs as the Hub role.
        migrate(&mut client)?;
        limit_to_hub_role(&mut client)?;
        let mut pool: Vec<_> = (1..size.max(1)).map(|_| None).collect();
        pool.push(Some(Conn::new(client)));
        Ok(Self {
            url: url.to_owned(),
            pool: Mutex::new(pool),
            available: Condvar::new(),
        })
    }

    fn lease(&self) -> ConnectionLease<'_> {
        let mut pool = self
            .available
            .wait_while(self.pool.lock().expect("database pool lock"), |idle| {
                idle.is_empty()
            })
            .expect("database pool lock");
        ConnectionLease {
            store: self,
            connection: pool.pop().expect("an idle slot is available"),
        }
    }

    fn run<T>(
        &self,
        query: impl FnOnce(&mut Conn) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        // Hold one connection for the whole transaction. When the pool is busy, wait for any slot.
        let mut lease = self.lease();
        // After the database restarted the old connection is closed for good, so open a new one
        // instead of failing every request until the Hub restarts.
        if lease.connection.as_ref().is_none_or(Conn::is_closed) {
            let mut client = Client::connect(&self.url, tls()).map_err(failed)?;
            limit_to_hub_role(&mut client)?;
            lease.connection = Some(Conn::new(client));
        }
        let client = lease
            .connection
            .as_mut()
            .expect("the connection was just opened");
        let result = query(client);
        if let Err(error) = &result
            && (client.is_closed()
                || error.is_closed()
                || error.code() == Some(&SqlState::ADMIN_SHUTDOWN))
        {
            // PostgreSQL can report shutdown before the client notices that its socket is closed.
            // Discard the connection without retrying an operation whose result may be uncertain.
            lease.connection = None;
        }
        result.map_err(failed)
    }
}

impl PostgresStore {
    /// Whether row-level security binds the role the Hub works as. It always should, because the Hub
    /// switches to a plain role (migration 0008); `false` means that did not work, and then only the
    /// store's own filters keep organizations apart.
    pub fn row_security_applies(&self) -> Result<bool, StoreError> {
        let row = self.run(|client| {
            client.query_one(
                "SELECT NOT (rolsuper OR rolbypassrls) FROM pg_roles WHERE rolname = current_user",
                &[],
            )
        })?;
        Ok(row.get(0))
    }

    /// Like [`run`](Self::run), inside a transaction that names the organization, so row-level
    /// security shows only its rows. Every query also names the organization itself: that is the
    /// rule, and row security is the second guard for a query that forgets it.
    fn in_org<T>(
        &self,
        organization: &str,
        query: impl FnOnce(&mut Conn) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        self.in_org_transaction(organization, |conn| query(conn).map(|value| (value, true)))
    }

    fn in_org_transaction<T>(
        &self,
        organization: &str,
        query: impl FnOnce(&mut Conn) -> Result<(T, bool), postgres::Error>,
    ) -> Result<T, StoreError> {
        self.run(|conn| {
            // BEGIN and the setting go in one simple query, and COMMIT in another: three round
            // trips for a call instead of six. An id made of plain characters is put in the
            // query text; anything else takes the parameterised way, so nothing is ever spliced
            // into SQL.
            let plain = !organization.is_empty()
                && organization.len() <= 128
                && organization
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
            let result = (|| {
                if plain {
                    conn.batch_execute(&format!("BEGIN; SET LOCAL ck.org_id = '{organization}'"))?;
                } else {
                    conn.batch_execute("BEGIN")?;
                    conn.execute("SELECT set_config('ck.org_id', $1, true)", &[&organization])?;
                }
                let (value, commit) = query(conn)?;
                conn.batch_execute(if commit { "COMMIT" } else { "ROLLBACK" })?;
                Ok(value)
            })();
            if result.is_err() {
                // Leave no half-open transaction on a connection that will be used again.
                let _ = conn.batch_execute("ROLLBACK");
            }
            result
        })
    }

    fn save_stick(
        &self,
        organization: &str,
        stick: &Stick,
        retired_at: Option<OffsetDateTime>,
    ) -> Result<bool, StoreError> {
        let scope = stick
            .credential_scope
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(failed)?;
        let machine = stick.machine_number.map(i32::try_from).transpose();
        // The file supports unsigned numbers that the database cannot represent. The credential
        // can still be retired, even if its file-only machine number cannot be carried over.
        let machine = match retired_at {
            Some(_) => machine.unwrap_or(None),
            None => machine.map_err(failed)?,
        };
        self.in_org(organization, |transaction| {
            // A stick ID belongs to one organization. On conflict, only retirement in the
            // owning organization can write to the existing row.
            let added = transaction.execute(
                "INSERT INTO sticks
                     (organization_id, stick_id, token, team, site, contest, machine_number,
                      credential_scope, retired_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (stick_id) DO NOTHING",
                &[
                    &organization,
                    &stick.stick_id.as_str(),
                    &stick.token.expose(),
                    &stick.team,
                    &stick.site,
                    &stick.contest,
                    &machine,
                    &scope,
                    &retired_at,
                ],
            )?;
            match (added, retired_at) {
                (0, Some(now)) => mark_retired(transaction, organization, &stick.stick_id, now),
                _ => Ok(added == 1),
            }
        })
    }

    fn use_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
        recoverable: bool,
    ) -> Result<Redemption, StoreError> {
        // One transaction: the code is used up and the stick is registered, or neither happens.
        self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute("SELECT set_config('ck.code_hash', $1, true)", &[&code_hash])?;
            let claimed = recoverable.then_some(stick.as_str());
            let used = transaction.query_opt(
                "UPDATE enrolment_codes SET used_at = $2, claimed_stick_id = $3
                 WHERE code_hash = $1 AND used_at IS NULL AND expires_at > $2
                 RETURNING organization_id, team, site, contest, machine_number, credential_scope,
                           hardware_fingerprint, hardware_serial_hint",
                &[&code_hash, &now, &claimed],
            )?;
            let Some(row) = used else {
                let result = if recoverable {
                    recover_enrolment_claim(&mut transaction, code_hash, now, stick, token)?
                } else {
                    Redemption::Invalid
                };
                transaction.commit()?;
                return Ok(result);
            };
            let enrolled = enrolled_stick(&row);
            let machine: Option<i32> = row.get(4);
            let scope: Option<serde_json::Value> = row.get(5);
            let fingerprint: Option<String> = row.get(6);
            let serial_hint: Option<String> = row.get(7);
            transaction.execute(
                "SELECT set_config('ck.org_id', $1, true)",
                &[&enrolled.organization],
            )?;
            let added = transaction.execute(
                "INSERT INTO sticks
                     (organization_id, stick_id, token, team, site, contest, machine_number, credential_scope)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (stick_id) DO NOTHING",
                &[
                    &enrolled.organization,
                    &stick.as_str(),
                    &token.expose(),
                    &enrolled.team,
                    &enrolled.site,
                    &enrolled.contest,
                    &machine,
                    &scope,
                ],
            )?;
            if added == 0 {
                transaction.rollback()?;
                return Ok(Redemption::IdTaken);
            }
            if let Some(fingerprint) = fingerprint {
                // The write counts now, when the stick that was written trades its code.
                transaction.execute(
                    "INSERT INTO stick_hardware
                         (organization_id, fingerprint, serial_hint, write_count,
                          first_written_at, last_written_at, current_stick_id)
                     VALUES ($1, $2, $3, 1, $4, $4, $5)
                     ON CONFLICT (organization_id, fingerprint) DO UPDATE SET
                         write_count = stick_hardware.write_count + 1,
                         last_written_at = $4, serial_hint = $3, current_stick_id = $5",
                    &[
                        &enrolled.organization,
                        &fingerprint,
                        &serial_hint.unwrap_or_default(),
                        &now,
                        &stick.as_str(),
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(Redemption::Done(enrolled))
        })
    }
}

impl Drop for PostgresStore {
    /// Closing the connection blocks and starts a runtime of its own, which panics on a thread that
    /// already runs async tasks, such as the Hub's at shutdown. A plain thread does it safely.
    fn drop(&mut self) {
        if let Ok(slots) = self.pool.get_mut()
            && !slots.is_empty()
        {
            let connections = std::mem::take(slots);
            let _ = std::thread::spawn(move || drop(connections)).join();
        }
    }
}

mod migrations;
mod rows;
mod store_impl;
#[cfg(test)]
mod tests;

use migrations::migrate;

use rows::{
    COMMAND_COLUMNS, RECORDING_COLUMNS, command_from, invitation_from, organization_from,
    recording_from_row, refusal_text, role_from, stick_from, user_from,
};
