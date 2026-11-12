//! Registering sticks: from the admin tool as each one is made, or from its registry file.
//!
//! A stick's secret is never replaced, never echoed and never logged.

use ck_protocol::{StickId, Token};
use serde::Deserialize;

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

/// What registering did.
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

fn check(stick: &NewStick) -> Result<(), String> {
    let id = stick.stick_id.as_str();
    let token_ok = (MIN_TOKEN_LENGTH..=MAX_TOKEN_LENGTH).contains(&stick.token.chars().count())
        && !stick
            .token
            .chars()
            .any(|c| c.is_control() || c.is_whitespace());
    if !token_ok {
        return Err(format!(
            "stick {id}: the secret must be {MIN_TOKEN_LENGTH} to {MAX_TOKEN_LENGTH} characters without spaces"
        ));
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

/// Registers `sticks` in `store`. `in_file` is the secret of a stick the registry file already has.
///
/// Every stick is checked before any is written. A stick that is already known with the same
/// secret is left alone, so sending the same registry twice is safe; one known with a different
/// secret is reported and left alone, never overwritten.
///
/// # Errors
/// See [`RegisterError`].
pub fn register_sticks(
    store: &dyn Store,
    in_file: &dyn Fn(&StickId) -> Option<Token>,
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
                .map(|stick| stick.token),
        };
        if let Some(known) = known {
            if known == token {
                result.already_known += 1;
            } else {
                result.conflicts.push(new.stick_id.to_string());
            }
            continue;
        }
        let stick = Stick {
            stick_id: new.stick_id,
            token,
            team: new.team,
            site: new.site,
            contest: new.contest,
            machine_number: new.machine_number,
        };
        if store.add_stick(stick).map_err(unavailable)? {
            result.added += 1;
        } else {
            // Another request registered it between the check and the write.
            result.already_known += 1;
        }
    }
    Ok(result)
}

impl Hub {
    /// Registers sticks. See [`register_sticks`].
    ///
    /// # Errors
    /// See [`RegisterError`].
    pub fn register_sticks(&self, sticks: Vec<NewStick>) -> Result<Registered, RegisterError> {
        let registry = self.registry.lock().expect("registry lock");
        register_sticks(
            &*self.store,
            &|id| registry.find(id).map(|entry| entry.token.clone()),
            sticks,
        )
    }
}
