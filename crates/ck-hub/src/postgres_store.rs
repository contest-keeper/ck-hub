//! The store on PostgreSQL, so what sticks reported survives a restart of the Hub.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ck_protocol::{Event, Heartbeat, StickId, Token};
use postgres::Client;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::store::{
    Membership, NewUser, Organization, Role, SeenHeartbeat, Stick, Store, StoreError, StoredEvent,
    TokenPurpose, User,
};

/// The schema, in the order it is applied. A migration that was applied is never run again and
/// never edited: a change is a new file.
const MIGRATIONS: [(&str, &str); 8] = [
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
];

/// The role the Hub works as once the schema is up to date. See migration 0008.
const HUB_ROLE: &str = "ck_hub_app";

/// Makes a connection work as the plain Hub role. Row-level security then binds it, whoever logged in.
fn limit_to_hub_role(client: &mut Client) -> Result<(), StoreError> {
    client
        .batch_execute(&format!("SET ROLE {HUB_ROLE}"))
        .map_err(failed)
}

pub struct PostgresStore {
    url: String,
    /// `None` while the store is being dropped, and after a connection was lost and not yet
    /// opened again.
    client: Mutex<Option<Client>>,
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

impl PostgresStore {
    /// Connects and brings the schema up to date.
    ///
    /// The connection is encrypted when the database offers it (`sslmode=prefer`, the default) and
    /// always with `sslmode=require`; the certificate is then checked against the public web
    /// roots. A managed database such as Azure Database for PostgreSQL requires it.
    /// `sslmode=disable` keeps the connection plain for a database on the same machine.
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let mut client = Client::connect(url, tls()).map_err(failed)?;
        // Migrations need the account that logged in; everything after them runs as the Hub role.
        migrate(&mut client)?;
        limit_to_hub_role(&mut client)?;
        Ok(Self {
            url: url.to_owned(),
            client: Mutex::new(Some(client)),
        })
    }

    fn run<T>(
        &self,
        query: impl FnOnce(&mut Client) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        let mut slot = self.client.lock().expect("database lock");
        // After the database restarted the old connection is closed for good, so open a new one
        // instead of failing every request until the Hub restarts.
        if slot.as_ref().is_none_or(Client::is_closed) {
            let mut client = Client::connect(&self.url, tls()).map_err(failed)?;
            limit_to_hub_role(&mut client)?;
            *slot = Some(client);
        }
        let client = slot.as_mut().expect("the connection was just opened");
        query(client).map_err(failed)
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
        query: impl FnOnce(&mut postgres::Transaction<'_>) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute("SELECT set_config('ck.org_id', $1, true)", &[&organization])?;
            let result = query(&mut transaction)?;
            transaction.commit()?;
            Ok(result)
        })
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
        organization: &str,
        heartbeat: Heartbeat,
        received_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_value(&heartbeat).map_err(failed)?;
        let seq = i64::try_from(heartbeat.seq).map_err(failed)?;
        self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO heartbeat_boots (organization_id, stick_id, boot_id, seq)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (stick_id, boot_id)
                 DO UPDATE SET seq = GREATEST(heartbeat_boots.seq, EXCLUDED.seq)
                 WHERE heartbeat_boots.organization_id = EXCLUDED.organization_id",
                &[
                    &organization,
                    &heartbeat.stick_id.as_str(),
                    &heartbeat.boot_id,
                    &seq,
                ],
            )?;
            transaction.execute(
                "INSERT INTO heartbeats
                     (organization_id, stick_id, boot_id, seq, received_at, count, heartbeat)
                 VALUES ($1, $2, $3, $4, $5, 1, $6)
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
            )
        })?;
        Ok(())
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
        received_at: DateTime<Utc>,
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

    fn create_user(&self, user: NewUser) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "INSERT INTO users (email, password_hash, email_verified_at, created_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT DO NOTHING
                 RETURNING id, email, password_hash, email_verified_at, created_at",
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
                "SELECT id, email, password_hash, email_verified_at, created_at
                 FROM users WHERE lower(email) = lower($1)",
                &[&email],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn user(&self, id: &str) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, email, password_hash, email_verified_at, created_at
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
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)",
                &[&token_hash, &user_id, &expires_at],
            )
        })?;
        Ok(())
    }

    fn session(&self, token_hash: &str) -> Result<Option<(String, DateTime<Utc>)>, StoreError> {
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

    fn delete_expired_sessions(&self, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.run(|client| client.execute("DELETE FROM sessions WHERE expires_at <= $1", &[&now]))?;
        Ok(())
    }

    fn mark_email_verified(&self, user_id: &str, at: DateTime<Utc>) -> Result<(), StoreError> {
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
        expires_at: DateTime<Utc>,
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
        now: DateTime<Utc>,
    ) -> Result<Option<String>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "DELETE FROM email_tokens WHERE token_hash = $1 AND purpose = $2
                 RETURNING user_id, expires_at",
                &[&token_hash, &purpose.as_str()],
            )
        })?;
        Ok(row.and_then(|row| {
            let ends: DateTime<Utc> = row.get(1);
            (ends > now).then(|| row.get(0))
        }))
    }

    fn delete_expired_email_tokens(&self, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute("DELETE FROM email_tokens WHERE expires_at <= $1", &[&now])
        })?;
        Ok(())
    }

    fn delete_unverified_users_before(&self, before: DateTime<Utc>) -> Result<(), StoreError> {
        // Sessions and tokens of these accounts go with them (ON DELETE CASCADE).
        self.run(|client| {
            client.execute(
                "DELETE FROM users WHERE email_verified_at IS NULL AND created_at < $1",
                &[&before],
            )
        })?;
        Ok(())
    }

    fn add_stick(&self, organization: &str, stick: Stick) -> Result<bool, StoreError> {
        let machine = stick
            .machine_number
            .map(i32::try_from)
            .transpose()
            .map_err(failed)?;
        // The conflict is on the stick id alone, so an id that belongs to another organization is
        // refused too.
        let added = self.in_org(organization, |transaction| {
            transaction.execute(
                "INSERT INTO sticks
                     (organization_id, stick_id, token, team, site, contest, machine_number)
                 VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (stick_id) DO NOTHING",
                &[
                    &organization,
                    &stick.stick_id.as_str(),
                    &stick.token.expose(),
                    &stick.team,
                    &stick.site,
                    &stick.contest,
                    &machine,
                ],
            )
        })?;
        Ok(added == 1)
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
                "SELECT stick_id, token, team, site, contest, machine_number, organization_id
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
                "SELECT stick_id, token, team, site, contest, machine_number FROM sticks
                 WHERE organization_id = $1",
                &[&organization],
            )
        })?;
        rows.iter().map(stick_from).collect()
    }
}

fn stick_from(row: &postgres::Row) -> Result<Stick, StoreError> {
    let id: String = row.get(0);
    let token: String = row.get(1);
    let machine: Option<i32> = row.get(5);
    Ok(Stick {
        stick_id: StickId::parse(&id).map_err(failed)?,
        token: Token::new(token),
        team: row.get(2),
        site: row.get(3),
        contest: row.get(4),
        machine_number: machine.map(u32::try_from).transpose().map_err(failed)?,
    })
}

fn user_from(row: &postgres::Row) -> User {
    User {
        id: row.get(0),
        email: row.get(1),
        password_hash: row.get(2),
        email_verified_at: row.get(3),
        created_at: row.get(4),
    }
}

fn organization_from(row: &postgres::Row) -> Organization {
    Organization {
        id: row.get(0),
        slug: row.get(1),
        name: row.get(2),
    }
}

fn role_from(text: &str) -> Result<Role, StoreError> {
    Role::parse(text).ok_or_else(|| failed(format!("unknown role {text}")))
}
