//! Signing in Keeper Forge through the browser.
//!
//! The forge opens the Hub in the browser and listens on a port of this PC. Once the admin is
//! signed in and agrees, the Hub sends the browser back to that port with a code that works once
//! and only for a minute. The forge trades the code for a session of its own, so the long-lived
//! session never passes through the browser's address bar or history.

use time::{Duration, OffsetDateTime};

use crate::accounts::{AccessError, Account, SESSION_LIFETIME_HOURS, token_hash, unavailable};
use crate::hub::Hub;
use crate::signup::TokenError;

pub const FORGE_CODE_LIFETIME_SECONDS: i64 = 60;
/// More codes than this at once are not people signing in.
const MAX_OPEN_CODES: usize = 1000;
const MIN_PORT: u16 = 1024;
const MIN_STATE_LENGTH: usize = 16;
const MAX_STATE_LENGTH: usize = 128;

/// An account and the time its code stops working.
pub(crate) type OpenCodes = std::collections::HashMap<String, (String, OffsetDateTime)>;

#[derive(Debug, PartialEq, Eq)]
pub enum ForgeError {
    /// The port or the state the forge sent is not one it would send.
    BadRequest,
    /// The session is not valid.
    Unauthorized,
    Unavailable,
}

impl Hub {
    /// Makes the address the browser goes back to, for the account behind `session`. The host is
    /// always this PC, so a link made by someone else cannot send the code to another machine.
    pub fn forge_redirect(
        &self,
        session: &str,
        port: u16,
        state: &str,
    ) -> Result<String, ForgeError> {
        if port < MIN_PORT
            || !(MIN_STATE_LENGTH..=MAX_STATE_LENGTH).contains(&state.len())
            || !state
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ForgeError::BadRequest);
        }
        let account = self.account(session).map_err(|reason| match reason {
            AccessError::Unavailable => ForgeError::Unavailable,
            _ => ForgeError::Unauthorized,
        })?;
        let now = (self.clock)();
        let code = ck_crypto::generate_stick_token().expose().to_owned();
        let mut open = self.accounts.forge_codes.lock().expect("codes lock");
        open.retain(|_, (_, ends)| now < *ends);
        if open.len() >= MAX_OPEN_CODES {
            return Err(ForgeError::Unavailable);
        }
        open.insert(
            token_hash(&code),
            (
                account.id,
                now + Duration::seconds(FORGE_CODE_LIFETIME_SECONDS),
            ),
        );
        Ok(format!(
            "http://127.0.0.1:{port}/callback?code={code}&state={state}"
        ))
    }

    /// Trades a code for a session. The code is used up whether or not it was still good.
    pub fn forge_session(&self, code: &str) -> Result<(String, Account), TokenError> {
        let now = (self.clock)();
        let found = self
            .accounts
            .forge_codes
            .lock()
            .expect("codes lock")
            .remove(&token_hash(code));
        let (user_id, ends) = found.ok_or(TokenError::Invalid)?;
        if ends <= now {
            return Err(TokenError::Invalid);
        }
        let store_failed = |error| {
            unavailable(&error);
            TokenError::Unavailable
        };
        let user = self
            .store
            .user(&user_id)
            .map_err(store_failed)?
            .ok_or(TokenError::Invalid)?;
        let token = ck_crypto::generate_stick_token().expose().to_owned();
        self.store
            .create_session(
                &token_hash(&token),
                &user.id,
                now + Duration::hours(SESSION_LIFETIME_HOURS),
            )
            .map_err(store_failed)?;
        Ok((
            token,
            Account {
                id: user.id,
                email: user.email,
                staff: user.is_platform_staff,
            },
        ))
    }
}
