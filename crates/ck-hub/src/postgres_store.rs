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
    Invitation, Member, Membership, NewProfile, NewUser, Organization, Profile, ProfileSummary,
    Role, SeenHeartbeat, Stick, Store, StoreError, StoredEvent, TokenPurpose, User, Wallpaper,
};

/// The schema, in the order it is applied. A migration that was applied is never run again and
/// never edited: a change is a new file.
const MIGRATIONS: [(&str, &str); 17] = [
    (
        "0001_heartbeats_and_events",
        include_str!("../migrations/0001_heartbeats_and_events.sql"),
    ),
    (
        "0002_users_and_sessions",
        include_str!("../migrations/0002_users_and_sessions.sql"),
    ),
    ("0003_sticks", include_str!("../migrations/0003_sticks.sql")),
    (
        "0004_heartbeat_boots",
        include_str!("../migrations/0004_heartbeat_boots.sql"),
    ),
    (
        "0005_organizations_and_memberships",
        include_str!("../migrations/0005_organizations_and_memberships.sql"),
    ),
    (
        "0006_organization_scoping",
        include_str!("../migrations/0006_organization_scoping.sql"),
    ),
    (
        "0007_email_verification",
        include_str!("../migrations/0007_email_verification.sql"),
    ),
    (
        "0008_hub_role",
        include_str!("../migrations/0008_hub_role.sql"),
    ),
    (
        "0009_invitations_and_staff",
        include_str!("../migrations/0009_invitations_and_staff.sql"),
    ),
    (
        "0010_judge_submissions",
        include_str!("../migrations/0010_judge_submissions.sql"),
    ),
    (
        "0011_profiles",
        include_str!("../migrations/0011_profiles.sql"),
    ),
    (
        "0012_commands",
        include_str!("../migrations/0012_commands.sql"),
    ),
    (
        "0013_enrolment_codes",
        include_str!("../migrations/0013_enrolment_codes.sql"),
    ),
    (
        "0014_recordings",
        include_str!("../migrations/0014_recordings.sql"),
    ),
    (
        "0015_credential_scopes",
        include_str!("../migrations/0015_credential_scopes.sql"),
    ),
    (
        "0016_retired_stick_credentials",
        include_str!("../migrations/0016_retired_stick_credentials.sql"),
    ),
    (
        "0017_enrolment_claims",
        include_str!("../migrations/0017_enrolment_claims.sql"),
    ),
];

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
                 RETURNING organization_id, team, site, contest, machine_number, credential_scope",
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
        organization: &str,
        heartbeat: Heartbeat,
        received_at: OffsetDateTime,
    ) -> Result<Option<u64>, StoreError> {
        let json = serde_json::to_value(&heartbeat).map_err(failed)?;
        let seq = i64::try_from(heartbeat.seq).map_err(failed)?;
        let accepted = self.in_org_transaction(organization, |transaction| {
            let stored = transaction.execute(
                "WITH newer_boot AS (
                     INSERT INTO heartbeat_boots (organization_id, stick_id, boot_id, seq)
                     VALUES ($1, $2, $3, $4)
                     ON CONFLICT (stick_id, boot_id) DO UPDATE SET seq = EXCLUDED.seq
                     WHERE heartbeat_boots.organization_id = EXCLUDED.organization_id
                       AND heartbeat_boots.seq < EXCLUDED.seq
                     RETURNING organization_id, stick_id, boot_id, seq
                 )
                 INSERT INTO heartbeats
                     (organization_id, stick_id, boot_id, seq, received_at, count, heartbeat)
                 SELECT organization_id, stick_id, boot_id, seq, $5, 1, $6 FROM newer_boot
                 ON CONFLICT (stick_id) DO UPDATE SET
                     boot_id = EXCLUDED.boot_id, seq = EXCLUDED.seq,
                     received_at = EXCLUDED.received_at, heartbeat = EXCLUDED.heartbeat,
                     count = heartbeats.count + 1
                 WHERE heartbeats.organization_id = EXCLUDED.organization_id",
                &[
                    &organization,
                    &heartbeat.stick_id.as_str(),
                    &heartbeat.boot_id,
                    &seq,
                    &received_at,
                    &json,
                ],
            )?;
            // If a different organization owns the latest row, roll back the boot change too.
            Ok((stored == 1, stored == 1))
        })?;
        if accepted {
            return Ok(None);
        }
        // Read the conflict detail after the write attempt. A concurrent writer may have
        // advanced the boot while this transaction waited for its row.
        let top =
            self.heartbeat_seq_of_boot(organization, &heartbeat.stick_id, &heartbeat.boot_id)?;
        top.filter(|&top| top >= heartbeat.seq)
            .map(Some)
            .ok_or_else(|| {
                StoreError("the heartbeat could not be recorded in its organization".to_owned())
            })
    }

    fn last_heartbeat(
        &self,
        organization: &str,
        stick: &StickId,
    ) -> Result<Option<SeenHeartbeat>, StoreError> {
        let row = self.in_org(organization, |transaction| {
            transaction.query_opt(
                "SELECT heartbeat, received_at, count FROM heartbeats
                 WHERE organization_id = $1 AND stick_id = $2",
                &[&organization, &stick.as_str()],
            )
        })?;
        let Some(row) = row else { return Ok(None) };
        let json: serde_json::Value = row.get(0);
        let count: i64 = row.get(2);
        Ok(Some(SeenHeartbeat {
            heartbeat: serde_json::from_value(json).map_err(failed)?,
            received_at: row.get(1),
            count: u64::try_from(count).map_err(failed)?,
        }))
    }

    fn heartbeat_seq_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<u64>, StoreError> {
        let row = self.in_org(organization, |transaction| {
            transaction.query_opt(
                "SELECT seq FROM heartbeat_boots
                 WHERE organization_id = $1 AND stick_id = $2 AND boot_id = $3",
                &[&organization, &stick.as_str(), &boot],
            )
        })?;
        row.map(|row| u64::try_from(row.get::<_, i64>(0)).map_err(failed))
            .transpose()
    }

    fn last_event_of_boot(
        &self,
        organization: &str,
        stick: &StickId,
        boot: &str,
    ) -> Result<Option<StoredEvent>, StoreError> {
        let row = self.in_org(organization, |transaction| {
            transaction.query_opt(
                "SELECT event, received_at, integrity FROM events
                 WHERE organization_id = $1 AND stick_id = $2 AND boot_id = $3
                 ORDER BY id DESC LIMIT 1",
                &[&organization, &stick.as_str(), &boot],
            )
        })?;
        let Some(row) = row else { return Ok(None) };
        let json: serde_json::Value = row.get(0);
        Ok(Some(StoredEvent {
            event: serde_json::from_value(json).map_err(failed)?,
            received_at: row.get(1),
            integrity: row.get(2),
        }))
    }

    fn append_event(
        &self,
        organization: &str,
        event: Event,
        received_at: OffsetDateTime,
        integrity: Option<String>,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_value(&event).map_err(failed)?;
        let seq = i64::try_from(event.seq).map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO events
                     (organization_id, stick_id, boot_id, seq, received_at, integrity, event)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &organization,
                    &event.stick_id.as_str(),
                    &event.boot_id,
                    &seq,
                    &received_at,
                    &integrity,
                    &json,
                ],
            )
        })?;
        Ok(())
    }

    fn events(&self, organization: &str, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT event, received_at, integrity FROM events
                 WHERE organization_id = $1 AND stick_id = $2 ORDER BY id",
                &[&organization, &stick.as_str()],
            )
        })?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let json: serde_json::Value = row.get(0);
            events.push(StoredEvent {
                event: serde_json::from_value(json).map_err(failed)?,
                received_at: row.get(1),
                integrity: row.get(2),
            });
        }
        Ok(events)
    }

    fn add_judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        submissions: &[JudgeSubmission],
        received_at: OffsetDateTime,
    ) -> Result<usize, StoreError> {
        let mut numbers = Vec::with_capacity(submissions.len());
        for submission in submissions {
            numbers.push((
                i64::try_from(submission.contest_id).map_err(failed)?,
                i64::try_from(submission.submission_id).map_err(failed)?,
            ));
        }
        self.in_org(organization, |transaction| {
            let mut added = 0;
            for (submission, (contest, id)) in submissions.iter().zip(&numbers) {
                added += transaction.execute(
                    "INSERT INTO judge_submissions
                         (organization_id, judge, contest_id, submission_id, handles,
                          problem_index, submitted_at, received_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (organization_id, judge, contest_id, submission_id) DO NOTHING",
                    &[
                        &organization,
                        &judge,
                        contest,
                        id,
                        &submission.handles,
                        &submission.problem_index,
                        &submission.submitted_at,
                        &received_at,
                    ],
                )?;
            }
            Ok(added)
        })
        .map(|added| usize::try_from(added).unwrap_or(0))
    }

    fn judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        contest_id: u64,
    ) -> Result<Vec<JudgeSubmission>, StoreError> {
        let contest = i64::try_from(contest_id).map_err(failed)?;
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT submission_id, handles, problem_index, submitted_at FROM judge_submissions
                 WHERE organization_id = $1 AND judge = $2 AND contest_id = $3
                 ORDER BY submitted_at, id",
                &[&organization, &judge, &contest],
            )
        })?;
        let mut found = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row.get(0);
            found.push(JudgeSubmission {
                submission_id: u64::try_from(id).map_err(failed)?,
                contest_id,
                handles: row.get(1),
                problem_index: row.get(2),
                submitted_at: row.get(3),
            });
        }
        Ok(found)
    }

    fn submission_events(&self, organization: &str) -> Result<Vec<SubmitEvent>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT event, received_at, integrity FROM events
                 WHERE organization_id = $1 AND event->>'kind' = 'submission' ORDER BY id",
                &[&organization],
            )
        })?;
        let mut found = Vec::new();
        for row in rows {
            let json: serde_json::Value = row.get(0);
            let stored = StoredEvent {
                event: serde_json::from_value(json).map_err(failed)?,
                received_at: row.get(1),
                integrity: row.get(2),
            };
            found.extend(submit_event(&stored));
        }
        Ok(found)
    }

    fn create_user(&self, user: NewUser) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "INSERT INTO users (email, password_hash, email_verified_at, created_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT DO NOTHING
                 RETURNING id, email, password_hash, email_verified_at, created_at, is_platform_staff",
                &[
                    &user.email,
                    &user.password_hash,
                    &user.email_verified_at,
                    &user.created_at,
                ],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, email, password_hash, email_verified_at, created_at, is_platform_staff
                 FROM users WHERE lower(email) = lower($1)",
                &[&email],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn user(&self, id: &str) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, email, password_hash, email_verified_at, created_at, is_platform_staff
                 FROM users WHERE id = $1",
                &[&id],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn create_organization(
        &self,
        slug: &str,
        name: &str,
    ) -> Result<Option<Organization>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "INSERT INTO organizations (slug, name) VALUES ($1, $2)
                 ON CONFLICT (slug) DO NOTHING RETURNING id, slug, name",
                &[&slug, &name],
            )
        })?;
        Ok(row.as_ref().map(organization_from))
    }

    fn ensure_organization(&self, slug: &str, name: &str) -> Result<Organization, StoreError> {
        let row = self.run(|client| {
            client.execute(
                "INSERT INTO organizations (slug, name) VALUES ($1, $2)
                 ON CONFLICT (slug) DO NOTHING",
                &[&slug, &name],
            )?;
            client.query_one(
                "SELECT id, slug, name FROM organizations WHERE slug = $1",
                &[&slug],
            )
        })?;
        Ok(organization_from(&row))
    }

    fn organization_by_slug(&self, slug: &str) -> Result<Option<Organization>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, slug, name FROM organizations WHERE slug = $1",
                &[&slug],
            )
        })?;
        Ok(row.as_ref().map(organization_from))
    }

    fn add_member(
        &self,
        organization_id: &str,
        user_id: &str,
        role: Role,
    ) -> Result<bool, StoreError> {
        let added = self.run(|client| {
            client.execute(
                "INSERT INTO memberships (organization_id, user_id, role) VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
                &[&organization_id, &user_id, &role.as_str()],
            )
        })?;
        Ok(added == 1)
    }

    fn membership(&self, organization_id: &str, user_id: &str) -> Result<Option<Role>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT role FROM memberships WHERE organization_id = $1 AND user_id = $2",
                &[&organization_id, &user_id],
            )
        })?;
        row.map(|row| role_from(&row.get::<_, String>(0)))
            .transpose()
    }

    fn memberships(&self, user_id: &str) -> Result<Vec<Membership>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT organizations.id, organizations.slug, organizations.name, memberships.role
                 FROM memberships
                 JOIN organizations ON organizations.id = memberships.organization_id
                 WHERE memberships.user_id = $1 ORDER BY organizations.slug",
                &[&user_id],
            )
        })?;
        rows.iter()
            .map(|row| {
                Ok(Membership {
                    organization: organization_from(row),
                    role: role_from(&row.get::<_, String>(3))?,
                })
            })
            .collect()
    }

    fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)",
                &[&token_hash, &user_id, &expires_at],
            )
        })?;
        Ok(())
    }

    fn session(&self, token_hash: &str) -> Result<Option<(String, OffsetDateTime)>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT user_id, expires_at FROM sessions WHERE token_hash = $1",
                &[&token_hash],
            )
        })?;
        Ok(row.map(|row| (row.get(0), row.get(1))))
    }

    fn delete_session(&self, token_hash: &str) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute("DELETE FROM sessions WHERE token_hash = $1", &[&token_hash])
        })?;
        Ok(())
    }

    fn delete_expired_sessions(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.run(|client| client.execute("DELETE FROM sessions WHERE expires_at <= $1", &[&now]))?;
        Ok(())
    }

    fn mark_email_verified(&self, user_id: &str, at: OffsetDateTime) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "UPDATE users SET email_verified_at = $2 WHERE id = $1 AND email_verified_at IS NULL",
                &[&user_id, &at],
            )
        })?;
        Ok(())
    }

    fn set_password_hash(&self, user_id: &str, password_hash: &str) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "UPDATE users SET password_hash = $2 WHERE id = $1",
                &[&user_id, &password_hash],
            )
        })?;
        Ok(())
    }

    fn delete_sessions_of(&self, user_id: &str) -> Result<(), StoreError> {
        self.run(|client| client.execute("DELETE FROM sessions WHERE user_id = $1", &[&user_id]))?;
        Ok(())
    }

    fn put_email_token(
        &self,
        token_hash: &str,
        user_id: &str,
        purpose: TokenPurpose,
        expires_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute(
                "DELETE FROM email_tokens WHERE user_id = $1 AND purpose = $2",
                &[&user_id, &purpose.as_str()],
            )?;
            transaction.execute(
                "INSERT INTO email_tokens (token_hash, user_id, purpose, expires_at)
                 VALUES ($1, $2, $3, $4)",
                &[&token_hash, &user_id, &purpose.as_str(), &expires_at],
            )?;
            transaction.commit()
        })
    }

    fn take_email_token(
        &self,
        token_hash: &str,
        purpose: TokenPurpose,
        now: OffsetDateTime,
    ) -> Result<Option<String>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "DELETE FROM email_tokens WHERE token_hash = $1 AND purpose = $2
                 RETURNING user_id, expires_at",
                &[&token_hash, &purpose.as_str()],
            )
        })?;
        Ok(row.and_then(|row| {
            let ends: OffsetDateTime = row.get(1);
            (ends > now).then(|| row.get(0))
        }))
    }

    fn delete_expired_email_tokens(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute("DELETE FROM email_tokens WHERE expires_at <= $1", &[&now])
        })?;
        Ok(())
    }

    fn delete_unverified_users_before(&self, before: OffsetDateTime) -> Result<(), StoreError> {
        // Sessions and tokens of these accounts go with them (ON DELETE CASCADE).
        self.run(|client| {
            client.execute(
                "DELETE FROM users WHERE email_verified_at IS NULL AND created_at < $1",
                &[&before],
            )
        })?;
        Ok(())
    }

    fn put_invitation(
        &self,
        organization_id: &str,
        email: &str,
        role: Role,
        token_hash: &str,
        invited_by: Option<&str>,
        expires_at: OffsetDateTime,
    ) -> Result<Invitation, StoreError> {
        let row = self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute(
                "DELETE FROM invitations WHERE organization_id = $1 AND lower(email) = lower($2)",
                &[&organization_id, &email],
            )?;
            let row = transaction.query_one(
                "INSERT INTO invitations (organization_id, email, role, token_hash, invited_by, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 RETURNING id, organization_id, email, role, invited_by, expires_at",
                &[
                    &organization_id,
                    &email,
                    &role.as_str(),
                    &token_hash,
                    &invited_by,
                    &expires_at,
                ],
            )?;
            transaction.commit()?;
            Ok(row)
        })?;
        invitation_from(&row)
    }

    fn invitation_for_token(&self, token_hash: &str) -> Result<Option<Invitation>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, organization_id, email, role, invited_by, expires_at
                 FROM invitations WHERE token_hash = $1",
                &[&token_hash],
            )
        })?;
        row.as_ref().map(invitation_from).transpose()
    }

    fn take_invitation(
        &self,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Invitation>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "DELETE FROM invitations WHERE token_hash = $1
                 RETURNING id, organization_id, email, role, invited_by, expires_at",
                &[&token_hash],
            )
        })?;
        let Some(invitation) = row.as_ref().map(invitation_from).transpose()? else {
            return Ok(None);
        };
        Ok((invitation.expires_at > now).then_some(invitation))
    }

    fn invitations_of(&self, organization_id: &str) -> Result<Vec<Invitation>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT id, organization_id, email, role, invited_by, expires_at
                 FROM invitations WHERE organization_id = $1 ORDER BY lower(email)",
                &[&organization_id],
            )
        })?;
        rows.iter().map(invitation_from).collect()
    }

    fn delete_invitation(&self, organization_id: &str, id: &str) -> Result<bool, StoreError> {
        let deleted = self.run(|client| {
            client.execute(
                "DELETE FROM invitations WHERE organization_id = $1 AND id = $2",
                &[&organization_id, &id],
            )
        })?;
        Ok(deleted > 0)
    }

    fn delete_expired_invitations(&self, now: OffsetDateTime) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute("DELETE FROM invitations WHERE expires_at <= $1", &[&now])
        })?;
        Ok(())
    }

    fn members_of(&self, organization_id: &str) -> Result<Vec<Member>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT users.id, users.email, memberships.role
                 FROM memberships JOIN users ON users.id = memberships.user_id
                 WHERE memberships.organization_id = $1 ORDER BY lower(users.email)",
                &[&organization_id],
            )
        })?;
        rows.iter()
            .map(|row| {
                Ok(Member {
                    user_id: row.get(0),
                    email: row.get(1),
                    role: role_from(row.get::<_, &str>(2))?,
                })
            })
            .collect()
    }

    fn remove_member(&self, organization_id: &str, user_id: &str) -> Result<bool, StoreError> {
        let removed = self.run(|client| {
            client.execute(
                "DELETE FROM memberships WHERE organization_id = $1 AND user_id = $2",
                &[&organization_id, &user_id],
            )
        })?;
        Ok(removed > 0)
    }

    fn count_owners(&self, organization_id: &str) -> Result<usize, StoreError> {
        let row = self.run(|client| {
            client.query_one(
                "SELECT count(*) FROM memberships WHERE organization_id = $1 AND role = 'owner'",
                &[&organization_id],
            )
        })?;
        let count: i64 = row.get(0);
        usize::try_from(count).map_err(failed)
    }

    fn set_staff(&self, user_id: &str, staff: bool) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "UPDATE users SET is_platform_staff = $2 WHERE id = $1",
                &[&user_id, &staff],
            )
        })?;
        Ok(())
    }

    fn organizations(&self) -> Result<Vec<Organization>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT id, slug, name FROM organizations ORDER BY slug",
                &[],
            )
        })?;
        Ok(rows.iter().map(organization_from).collect())
    }

    fn add_stick(&self, organization: &str, stick: Stick) -> Result<bool, StoreError> {
        self.save_stick(organization, &stick, None)
    }

    fn retire_stick(
        &self,
        organization: &str,
        id: &StickId,
        file: Option<&Stick>,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        if let Some(file) = file.filter(|file| &file.stick_id == id) {
            return self.save_stick(organization, file, Some(now));
        }
        self.in_org(organization, |transaction| {
            mark_retired(transaction, organization, id, now)
        })
    }

    /// The one lookup that runs without an organization: a stick does not say which one it is in.
    /// Row-level security lets it read exactly the stick it names.
    fn stick(&self, id: &StickId) -> Result<Option<(String, Stick)>, StoreError> {
        let row = self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute(
                "SELECT set_config('ck.stick_id', $1, true)",
                &[&id.as_str()],
            )?;
            let row = transaction.query_opt(
                "SELECT stick_id, token, team, site, contest, machine_number, organization_id,
                        credential_scope, retired_at
                 FROM sticks WHERE stick_id = $1",
                &[&id.as_str()],
            )?;
            transaction.commit()?;
            Ok(row)
        })?;
        row.as_ref()
            .map(|row| Ok((row.get::<_, String>(6), stick_from(row)?)))
            .transpose()
    }

    fn sticks(&self, organization: &str) -> Result<Vec<Stick>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT stick_id, token, team, site, contest, machine_number, credential_scope,
                        retired_at FROM sticks
                 WHERE organization_id = $1",
                &[&organization],
            )
        })?;
        rows.iter().map(stick_from).collect()
    }

    fn put_profile(
        &self,
        organization: &str,
        profile: NewProfile,
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let (wallpaper_name, wallpaper_data) = match &profile.wallpaper {
            Some(wallpaper) => (Some(&wallpaper.name), Some(&wallpaper.data)),
            None => (None, None),
        };
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO profiles
                     (organization_id, id, name, settings, wallpaper_name, wallpaper_data, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (organization_id, id) DO UPDATE SET
                     name = EXCLUDED.name, settings = EXCLUDED.settings,
                     wallpaper_name = EXCLUDED.wallpaper_name,
                     wallpaper_data = EXCLUDED.wallpaper_data,
                     updated_at = EXCLUDED.updated_at",
                &[
                    &organization,
                    &profile.id,
                    &profile.name,
                    &profile.settings,
                    &wallpaper_name,
                    &wallpaper_data,
                    &now,
                ],
            )
        })
        .map(|_| ())
    }

    fn profiles(&self, organization: &str) -> Result<Vec<ProfileSummary>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT id, name, updated_at FROM profiles WHERE organization_id = $1
                 ORDER BY lower(name), id",
                &[&organization],
            )
        })?;
        Ok(rows
            .iter()
            .map(|row| ProfileSummary {
                id: row.get(0),
                name: row.get(1),
                updated_at: row.get(2),
            })
            .collect())
    }

    fn profile(&self, organization: &str, id: &str) -> Result<Option<Profile>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT id, name, settings, wallpaper_name, updated_at FROM profiles
                 WHERE organization_id = $1 AND id = $2",
                &[&organization, &id],
            )
        })?;
        Ok(rows.first().map(|row| Profile {
            id: row.get(0),
            name: row.get(1),
            settings: row.get(2),
            wallpaper_name: row.get(3),
            updated_at: row.get(4),
        }))
    }

    fn profile_wallpaper(
        &self,
        organization: &str,
        id: &str,
    ) -> Result<Option<Wallpaper>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                "SELECT wallpaper_name, wallpaper_data FROM profiles
                 WHERE organization_id = $1 AND id = $2 AND wallpaper_data IS NOT NULL",
                &[&organization, &id],
            )
        })?;
        Ok(rows.first().map(|row| Wallpaper {
            name: row.get(0),
            data: row.get(1),
        }))
    }

    fn delete_profile(&self, organization: &str, id: &str) -> Result<bool, StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "DELETE FROM profiles WHERE organization_id = $1 AND id = $2",
                &[&organization, &id],
            )
        })
        .map(|deleted| deleted > 0)
    }

    fn queue_command(&self, organization: &str, command: StoredCommand) -> Result<(), StoreError> {
        let signed = serde_json::to_value(&command.signed).map_err(failed)?;
        let refusal = refusal_text(command.refusal);
        let stick = command.signed.command.stick_id.to_string();
        let expires_at = command.expires_at();
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO commands
                     (organization_id, id, stick_id, sent_by_user_id, sent_by_email, action_type,
                      signed_command, state, refusal, created_at, delivered_at, finished_at,
                      expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                &[
                    &organization,
                    &command.signed.command.id,
                    &stick,
                    &command.sent_by_user_id,
                    &command.sent_by_email,
                    &action_type(&command.signed.command.action),
                    &signed,
                    &command.state.as_str(),
                    &refusal,
                    &command.created_at,
                    &command.delivered_at,
                    &command.finished_at,
                    &expires_at,
                ],
            )
        })
        .map(|_| ())
    }

    fn pending_commands(
        &self,
        organization: &str,
        stick: &StickId,
        now: OffsetDateTime,
    ) -> Result<Vec<StoredCommand>, StoreError> {
        let stick = stick.to_string();
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {COMMAND_COLUMNS} FROM commands
                     WHERE organization_id = $1 AND stick_id = $2
                       AND state IN ('queued', 'delivered') AND expires_at > $3
                     ORDER BY created_at, id"
                ),
                &[&organization, &stick, &now],
            )
        })?;
        rows.iter().map(command_from).collect()
    }

    fn mark_commands_delivered(
        &self,
        organization: &str,
        ids: &[String],
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "UPDATE commands SET state = 'delivered', delivered_at = $3
                 WHERE organization_id = $1 AND id = ANY($2) AND state = 'queued'",
                &[&organization, &ids, &now],
            )
        })
        .map(|_| ())
    }

    fn record_command_ack(
        &self,
        organization: &str,
        stick: &StickId,
        ack: &CommandAck,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let (state, refusal) = settled(ack);
        let refusal = refusal_text(refusal);
        let stick = stick.to_string();
        self.in_org(organization, |transaction| {
            transaction.execute(
                "UPDATE commands SET state = $4, refusal = $5, finished_at = $6
                 WHERE organization_id = $1 AND id = $2 AND stick_id = $3
                   AND state IN ('queued', 'delivered')",
                &[
                    &organization,
                    &ack.command_id,
                    &stick,
                    &state.as_str(),
                    &refusal,
                    &now,
                ],
            )
        })
        .map(|changed| changed > 0)
    }

    fn commands_of(
        &self,
        organization: &str,
        stick: &StickId,
        limit: u32,
    ) -> Result<Vec<StoredCommand>, StoreError> {
        let stick = stick.to_string();
        let limit = i64::from(limit);
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {COMMAND_COLUMNS} FROM commands
                     WHERE organization_id = $1 AND stick_id = $2
                     ORDER BY created_at DESC, id DESC LIMIT $3"
                ),
                &[&organization, &stick, &limit],
            )
        })?;
        rows.iter().map(command_from).collect()
    }

    fn command(&self, organization: &str, id: &str) -> Result<Option<StoredCommand>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {COMMAND_COLUMNS} FROM commands WHERE organization_id = $1 AND id = $2"
                ),
                &[&organization, &id],
            )
        })?;
        rows.first().map(command_from).transpose()
    }

    fn delete_commands_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "DELETE FROM commands WHERE organization_id = $1 AND created_at < $2",
                &[&organization, &before],
            )
        })
        .map(|_| ())
    }

    fn add_enrolment_code(
        &self,
        organization: &str,
        code: NewEnrolmentCode,
    ) -> Result<(), StoreError> {
        let scope = code
            .credential_scope
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(failed)?;
        let machine = code
            .machine_number
            .map(i32::try_from)
            .transpose()
            .map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO enrolment_codes
                     (code_hash, organization_id, created_by_user_id, team, site, contest,
                      machine_number, created_at, expires_at, credential_scope)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                &[
                    &code.code_hash,
                    &organization,
                    &code.created_by_user_id,
                    &code.team,
                    &code.site,
                    &code.contest,
                    &machine,
                    &code.created_at,
                    &code.expires_at,
                    &scope,
                ],
            )
        })
        .map(|_| ())
    }

    fn redeem_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError> {
        self.use_enrolment_code(code_hash, now, stick, token, false)
    }

    fn claim_enrolment_code(
        &self,
        code_hash: &str,
        now: OffsetDateTime,
        stick: &StickId,
        token: &Token,
    ) -> Result<Redemption, StoreError> {
        self.use_enrolment_code(code_hash, now, stick, token, true)
    }

    fn delete_enrolment_codes_before(
        &self,
        organization: &str,
        before: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "DELETE FROM enrolment_codes WHERE organization_id = $1 AND created_at < $2
                   AND (claimed_stick_id IS NULL OR used_at < $2)",
                &[&organization, &before],
            )
        })
        .map(|_| ())
    }
    fn add_recording(&self, organization: &str, recording: &Recording) -> Result<bool, StoreError> {
        let size = i64::try_from(recording.size).map_err(failed)?;
        let duration = recording
            .duration_s
            .map(i64::try_from)
            .transpose()
            .map_err(failed)?;
        let next = i32::try_from(recording.next_chunk).map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO recordings
                     (organization_id, id, stick_id, file, kind, size, sha256, started_at,
                      duration_s, next_chunk, created_at, completed_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                 ON CONFLICT (organization_id, stick_id, file) DO NOTHING",
                &[
                    &organization,
                    &recording.id,
                    &recording.stick_id,
                    &recording.file,
                    &recording.kind.as_str(),
                    &size,
                    &recording.sha256,
                    &recording.started_at,
                    &duration,
                    &next,
                    &recording.created_at,
                    &recording.completed_at,
                    &recording.expires_at,
                ],
            )
        })
        .map(|inserted| inserted == 1)
    }

    fn recording(&self, organization: &str, id: &str) -> Result<Option<Recording>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {RECORDING_COLUMNS} FROM recordings
                     WHERE organization_id = $1 AND id = $2"
                ),
                &[&organization, &id],
            )
        })?;
        rows.first().map(recording_from_row).transpose()
    }

    fn recordings_of(
        &self,
        organization: &str,
        stick: &StickId,
        limit: u32,
    ) -> Result<Vec<Recording>, StoreError> {
        let stick = stick.to_string();
        let limit = i64::from(limit);
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {RECORDING_COLUMNS} FROM recordings
                     WHERE organization_id = $1 AND stick_id = $2
                     ORDER BY created_at DESC, id DESC LIMIT $3"
                ),
                &[&organization, &stick, &limit],
            )
        })?;
        rows.iter().map(recording_from_row).collect()
    }

    fn advance_recording(
        &self,
        organization: &str,
        id: &str,
        index: u32,
    ) -> Result<bool, StoreError> {
        let index = i32::try_from(index).map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "UPDATE recordings SET next_chunk = $3 + 1
                 WHERE organization_id = $1 AND id = $2 AND next_chunk = $3
                   AND completed_at IS NULL",
                &[&organization, &id, &index],
            )
        })
        .map(|changed| changed == 1)
    }

    fn complete_recording(
        &self,
        organization: &str,
        id: &str,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "UPDATE recordings SET completed_at = $3, expires_at = $4
                 WHERE organization_id = $1 AND id = $2 AND completed_at IS NULL",
                &[&organization, &id, &now, &expires_at],
            )
        })
        .map(|changed| changed == 1)
    }

    fn due_recordings(
        &self,
        organization: &str,
        now: OffsetDateTime,
        abandoned_before: OffsetDateTime,
    ) -> Result<Vec<Recording>, StoreError> {
        let rows = self.in_org(organization, |transaction| {
            transaction.query(
                &format!(
                    "SELECT {RECORDING_COLUMNS} FROM recordings
                     WHERE organization_id = $1
                       AND ((completed_at IS NOT NULL AND expires_at < $2)
                            OR (completed_at IS NULL AND created_at < $3))
                     ORDER BY created_at LIMIT 200"
                ),
                &[&organization, &now, &abandoned_before],
            )
        })?;
        rows.iter().map(recording_from_row).collect()
    }

    fn delete_recording(&self, organization: &str, id: &str) -> Result<(), StoreError> {
        self.in_org(organization, |transaction| {
            transaction.execute(
                "DELETE FROM recordings WHERE organization_id = $1 AND id = $2",
                &[&organization, &id],
            )
        })
        .map(|_| ())
    }
}

const RECORDING_COLUMNS: &str = "id, stick_id, file, kind, size, sha256, started_at, duration_s, \
     next_chunk, created_at, completed_at, expires_at";

fn recording_from_row(row: &Row) -> Result<Recording, StoreError> {
    let kind: String = row.get("kind");
    let size: i64 = row.get("size");
    let duration: Option<i64> = row.get("duration_s");
    let next: i32 = row.get("next_chunk");
    Ok(Recording {
        id: row.get("id"),
        stick_id: row.get("stick_id"),
        file: row.get("file"),
        kind: Kind::parse(&kind).ok_or_else(|| failed("a recording has an unknown kind"))?,
        size: u64::try_from(size).map_err(failed)?,
        sha256: row.get("sha256"),
        started_at: row.get("started_at"),
        duration_s: duration.map(u64::try_from).transpose().map_err(failed)?,
        next_chunk: u32::try_from(next).map_err(failed)?,
        created_at: row.get("created_at"),
        completed_at: row.get("completed_at"),
        expires_at: row.get("expires_at"),
    })
}

const COMMAND_COLUMNS: &str = "sent_by_user_id, sent_by_email, signed_command, state, refusal,
     created_at, delivered_at, finished_at";

fn refusal_text(refusal: Option<ck_protocol::Refusal>) -> Option<String> {
    refusal.and_then(|r| {
        serde_json::to_value(r)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
    })
}

fn command_from(row: &Row) -> Result<StoredCommand, StoreError> {
    let signed: serde_json::Value = row.get(2);
    let state: String = row.get(3);
    let refusal: Option<String> = row.get(4);
    Ok(StoredCommand {
        signed: serde_json::from_value(signed).map_err(failed)?,
        sent_by_user_id: row.get(0),
        sent_by_email: row.get(1),
        state: CommandState::parse(&state)
            .ok_or_else(|| StoreError(format!("unknown command state {state}")))?,
        refusal: refusal
            .map(|text| serde_json::from_value(serde_json::Value::String(text)))
            .transpose()
            .map_err(failed)?,
        created_at: row.get(5),
        delivered_at: row.get(6),
        finished_at: row.get(7),
    })
}

fn stick_from(row: &Row) -> Result<Stick, StoreError> {
    let id: String = row.get(0);
    let token: String = row.get(1);
    let machine: Option<i32> = row.get(5);
    let scope: Option<serde_json::Value> = row.get("credential_scope");
    let credential_scope: Option<crate::CredentialScope> = scope
        .map(serde_json::from_value)
        .transpose()
        .map_err(failed)?;
    if let Some(scope) = &credential_scope {
        scope.validate().map_err(failed)?;
    }
    Ok(Stick {
        stick_id: StickId::parse(&id).map_err(failed)?,
        token: Token::new(token),
        credential_scope,
        retired_at: row.get("retired_at"),
        team: row.get(2),
        site: row.get(3),
        contest: row.get(4),
        machine_number: machine.map(u32::try_from).transpose().map_err(failed)?,
    })
}

fn user_from(row: &Row) -> User {
    User {
        id: row.get(0),
        email: row.get(1),
        password_hash: row.get(2),
        email_verified_at: row.get(3),
        created_at: row.get(4),
        is_platform_staff: row.get(5),
    }
}

fn invitation_from(row: &Row) -> Result<Invitation, StoreError> {
    Ok(Invitation {
        id: row.get(0),
        organization_id: row.get(1),
        email: row.get(2),
        role: role_from(row.get::<_, &str>(3))?,
        invited_by: row.get(4),
        expires_at: row.get(5),
    })
}

fn organization_from(row: &Row) -> Organization {
    Organization {
        id: row.get(0),
        slug: row.get(1),
        name: row.get(2),
    }
}

fn role_from(text: &str) -> Result<Role, StoreError> {
    Role::parse(text).ok_or_else(|| failed(format!("unknown role {text}")))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::PostgresStore;

    #[test]
    fn waiting_request_uses_whichever_connection_is_released() {
        let Ok(url) = std::env::var("CK_TEST_DATABASE_URL") else {
            return;
        };
        let store = PostgresStore::connect_with_pool(&url, 2).expect("PostgreSQL test database");
        let (acquired, holds_connection) = mpsc::channel();
        let (release_first, first) = mpsc::channel();
        let (release_second, second) = mpsc::channel();
        let (completed, finished) = mpsc::channel();
        let wait = Duration::from_secs(3);
        thread::scope(|scope| {
            for release in [first, second] {
                let store = &store;
                let acquired = acquired.clone();
                scope.spawn(move || {
                    store
                        .run(|_| {
                            acquired.send(()).expect("connection acquired");
                            release.recv_timeout(wait * 3).expect("release connection");
                            Ok(())
                        })
                        .expect("hold connection");
                });
                holds_connection
                    .recv_timeout(wait)
                    .expect("held connection");
            }
            scope.spawn(|| {
                store.run(|_| Ok(())).expect("use available connection");
                completed.send(()).expect("request finished");
            });
            assert!(finished.recv_timeout(Duration::from_millis(100)).is_err());
            release_second.send(()).expect("release second connection");
            let result = finished.recv_timeout(wait);
            release_first.send(()).expect("release first connection");
            assert!(
                result.is_ok(),
                "the request waited on a busy connection while another was idle"
            );
        });
    }
}
