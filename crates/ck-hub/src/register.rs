//! Registering sticks: from the admin tool as each one is made, or from its registry file.
//!
//! A stick's secret is never replaced, never echoed and never logged.

use ck_protocol::{StickId, Token};
use serde::Deserialize;

use crate::credentials::CredentialScope;
use crate::hub::Hub;
use crate::store::{Stick, Store};

const MIN_TOKEN_LENGTH: usize = 16;
const MAX_TOKEN_LENGTH: usize = 200;
const MAX_TEXT_LENGTH: usize = 200;

/// A stick to register. It has the same fields as an entry of the admin tool's registry file, and
/// other fields of that file are ignored. It has no `Debug`, so the secret cannot end up in a log.
#[derive(Deserialize)]
pub struct NewStick {
    pub stick_id: StickId,
    pub token: String,
    #[serde(default)]
    pub credential_scope: Option<CredentialScope>,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub contest: String,
    #[serde(default)]
    pub machine_number: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// Something in the request is wrong. Nothing was registered.
    Invalid(String),
    Unavailable,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Registered {
    pub added: usize,
    /// Sticks that were already there with the same secret, so nothing changed.
    pub already_known: usize,
    /// Sticks that were already there with a different secret. They were left as they were.
    pub conflicts: Vec<String>,
}

fn plain_text(text: &str) -> bool {
    text.chars().count() <= MAX_TEXT_LENGTH && !text.chars().any(char::is_control)
}

pub(crate) fn valid_token(token: &str) -> bool {
    (MIN_TOKEN_LENGTH..=MAX_TOKEN_LENGTH).contains(&token.chars().count())
        && !token.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn check(stick: &NewStick) -> Result<(), String> {
    let id = stick.stick_id.as_str();
    if let Some(scope) = &stick.credential_scope {
        scope
            .validate()
            .map_err(|error| format!("stick {id}: {error}"))?;
    }
    if !valid_token(&stick.token) {
        return Err(format!(
            "stick {id}: the secret must be {MIN_TOKEN_LENGTH} to {MAX_TOKEN_LENGTH} characters without spaces"
        ));
    }
    // PostgreSQL keeps the number as a 32-bit integer.
    if stick
        .machine_number
        .is_some_and(|number| i32::try_from(number).is_err())
    {
        return Err(format!("stick {id}: the machine number is too large"));
    }
    if ![&stick.team, &stick.site, &stick.contest]
        .iter()
        .all(|t| plain_text(t))
    {
        return Err(format!(
            "stick {id}: team, site and contest are at most {MAX_TEXT_LENGTH} characters of plain text"
        ));
    }
    Ok(())
}

type KnownKey = (String, Token, Option<CredentialScope>);

fn known_key((owner, stick): (String, Stick)) -> KnownKey {
    (owner, stick.token, stick.credential_scope)
}

/// Registers the sticks in the organization after validating the whole batch.
/// A known ID keeps its original secret and scope.
pub fn register_sticks(
    store: &dyn Store,
    organization: &str,
    in_file: &dyn Fn(&StickId) -> Option<KnownKey>,
    sticks: Vec<NewStick>,
) -> Result<Registered, RegisterError> {
    for stick in &sticks {
        check(stick).map_err(RegisterError::Invalid)?;
    }
    let unavailable = |error: crate::store::StoreError| {
        tracing::error!("{error}");
        RegisterError::Unavailable
    };
    let mut result = Registered::default();
    for new in sticks {
        let token = Token::new(new.token);
        let known = match in_file(&new.stick_id) {
            Some(known) => Some(known),
            None => store
                .stick(&new.stick_id)
                .map_err(unavailable)?
                .map(known_key),
        };
        let known = if let Some(known) = known {
            Some(known)
        } else {
            let stick = Stick {
                stick_id: new.stick_id.clone(),
                token: token.clone(),
                credential_scope: new.credential_scope.clone(),
                retired_at: None,
                archived_at: None,
                delete_after: None,
                deleted_at: None,
                team: new.team,
                site: new.site,
                contest: new.contest,
                machine_number: new.machine_number,
            };
            if store.add_stick(organization, stick).map_err(unavailable)? {
                result.added += 1;
                continue;
            }
            store
                .stick(&new.stick_id)
                .map_err(unavailable)?
                .map(known_key)
        };
        // A legacy retry may omit the scope. The stored assignment is never extended or replaced.
        if known.is_some_and(|(owner, known, scope)| {
            owner == organization
                && known == token
                && (new.credential_scope.is_none() || scope == new.credential_scope)
        }) {
            result.already_known += 1;
        } else {
            result.conflicts.push(new.stick_id.to_string());
        }
    }
    Ok(result)
}

impl Hub {
    /// See [`register_sticks`].
    pub fn register_sticks(
        &self,
        organization: &str,
        sticks: Vec<NewStick>,
    ) -> Result<Registered, RegisterError> {
        let file_organization = self.default_org_id().map_err(|error| {
            tracing::error!("{error}");
            RegisterError::Unavailable
        })?;
        let registry = self.registry.lock().expect("registry lock");
        register_sticks(
            self.store.as_ref(),
            organization,
            &|id| {
                registry.find(id).map(|stick| {
                    (
                        file_organization.clone(),
                        stick.token.clone(),
                        stick.credential_scope.clone(),
                    )
                })
            },
            sticks,
        )
    }
}
