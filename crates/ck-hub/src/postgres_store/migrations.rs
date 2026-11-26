//! The schema migrations and the code that applies them.

use super::{Client, StoreError, failed};

/// The schema, in the order it is applied. A migration that was applied is never run again and
/// never edited: a change is a new file.
pub(super) const MIGRATIONS: [(&str, &str); 23] = [
    (
        "0001_heartbeats_and_events",
        include_str!("../../migrations/0001_heartbeats_and_events.sql"),
    ),
    (
        "0002_users_and_sessions",
        include_str!("../../migrations/0002_users_and_sessions.sql"),
    ),
    (
        "0003_sticks",
        include_str!("../../migrations/0003_sticks.sql"),
    ),
    (
        "0004_heartbeat_boots",
        include_str!("../../migrations/0004_heartbeat_boots.sql"),
    ),
    (
        "0005_organizations_and_memberships",
        include_str!("../../migrations/0005_organizations_and_memberships.sql"),
    ),
    (
        "0006_organization_scoping",
        include_str!("../../migrations/0006_organization_scoping.sql"),
    ),
    (
        "0007_email_verification",
        include_str!("../../migrations/0007_email_verification.sql"),
    ),
    (
        "0008_hub_role",
        include_str!("../../migrations/0008_hub_role.sql"),
    ),
    (
        "0009_invitations_and_staff",
        include_str!("../../migrations/0009_invitations_and_staff.sql"),
    ),
    (
        "0010_judge_submissions",
        include_str!("../../migrations/0010_judge_submissions.sql"),
    ),
    (
        "0011_profiles",
        include_str!("../../migrations/0011_profiles.sql"),
    ),
    (
        "0012_commands",
        include_str!("../../migrations/0012_commands.sql"),
    ),
    (
        "0013_enrolment_codes",
        include_str!("../../migrations/0013_enrolment_codes.sql"),
    ),
    (
        "0014_recordings",
        include_str!("../../migrations/0014_recordings.sql"),
    ),
    (
        "0015_credential_scopes",
        include_str!("../../migrations/0015_credential_scopes.sql"),
    ),
    (
        "0016_retired_stick_credentials",
        include_str!("../../migrations/0016_retired_stick_credentials.sql"),
    ),
    (
        "0017_enrolment_claims",
        include_str!("../../migrations/0017_enrolment_claims.sql"),
    ),
    (
        "0018_archive_sticks",
        include_str!("../../migrations/0018_archive_sticks.sql"),
    ),
    (
        "0019_stick_hardware",
        include_str!("../../migrations/0019_stick_hardware.sql"),
    ),
    (
        "0020_expected_configs",
        include_str!("../../migrations/0020_expected_configs.sql"),
    ),
    (
        "0021_boot_spans",
        include_str!("../../migrations/0021_boot_spans.sql"),
    ),
    (
        "0022_judge_source_hashes",
        include_str!("../../migrations/0022_judge_source_hashes.sql"),
    ),
    (
        "0023_events_once",
        include_str!("../../migrations/0023_events_once.sql"),
    ),
];

/// Any number, the same in every Hub: whoever holds this lock is the one applying migrations.
const MIGRATION_LOCK: i64 = 0x0063_6b68_7562; // "ckhub"

pub(super) fn migrate(client: &mut Client) -> Result<(), StoreError> {
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
