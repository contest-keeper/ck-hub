//! Turning rows into the store's types.

use super::{
    CommandState, Invitation, Kind, Organization, Recording, Role, Row, Stick, StickId, StoreError,
    StoredCommand, Token, User, failed,
};

pub(super) const RECORDING_COLUMNS: &str = "id, stick_id, file, kind, size, sha256, started_at, duration_s, \
     next_chunk, created_at, completed_at, expires_at";

pub(super) fn recording_from_row(row: &Row) -> Result<Recording, StoreError> {
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

pub(super) const COMMAND_COLUMNS: &str =
    "sent_by_user_id, sent_by_email, signed_command, state, refusal,
     created_at, delivered_at, finished_at";

pub(super) fn refusal_text(refusal: Option<ck_protocol::Refusal>) -> Option<String> {
    refusal.and_then(|r| {
        serde_json::to_value(r)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
    })
}

pub(super) fn command_from(row: &Row) -> Result<StoredCommand, StoreError> {
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

pub(super) fn stick_from(row: &Row) -> Result<Stick, StoreError> {
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
        archived_at: row.get("archived_at"),
        delete_after: row.get("delete_after"),
        deleted_at: row.get("deleted_at"),
        team: row.get(2),
        site: row.get(3),
        contest: row.get(4),
        machine_number: machine.map(u32::try_from).transpose().map_err(failed)?,
    })
}

pub(super) fn user_from(row: &Row) -> User {
    User {
        id: row.get(0),
        email: row.get(1),
        password_hash: row.get(2),
        email_verified_at: row.get(3),
        created_at: row.get(4),
        is_platform_staff: row.get(5),
    }
}

pub(super) fn invitation_from(row: &Row) -> Result<Invitation, StoreError> {
    Ok(Invitation {
        id: row.get(0),
        organization_id: row.get(1),
        email: row.get(2),
        role: role_from(row.get::<_, &str>(3))?,
        invited_by: row.get(4),
        expires_at: row.get(5),
    })
}

pub(super) fn organization_from(row: &Row) -> Organization {
    Organization {
        id: row.get(0),
        slug: row.get(1),
        name: row.get(2),
    }
}

pub(super) fn role_from(text: &str) -> Result<Role, StoreError> {
    Role::parse(text).ok_or_else(|| failed(format!("unknown role {text}")))
}
