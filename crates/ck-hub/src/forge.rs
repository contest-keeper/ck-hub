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

/// How long a pairing code waits for the admin to answer in the browser.
pub const PAIRING_LIFETIME_SECONDS: i64 = 600;
/// More waiting pairings than this at once are not people signing in.
const MAX_OPEN_PAIRINGS: usize = 1000;
/// New pairings a minute, all callers together: the start is open to anyone who can reach the Hub.
const MAX_PAIRING_STARTS: u32 = 30;
const PAIRING_START_WINDOW_SECONDS: i64 = 60;
/// Wrong codes one account may try in a window, so a logged-in person cannot guess a code that
/// belongs to someone else's Forge (the guess would connect that Forge to the guesser's account).
const MAX_WRONG_CODES: u32 = 5;
const WRONG_CODE_WINDOW_SECONDS: i64 = 600;
/// Letters and digits that cannot be mistaken for each other: no I, O, 0 or 1. Exactly 32, so a
/// random byte maps to a letter without bias.
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// A pairing waiting for an answer: the hash of the secret Forge polls with, when it ends, and the
/// account that agreed.
pub(crate) struct Pairing {
    secret_hash: String,
    ends: OffsetDateTime,
    approved_by: Option<String>,
}

/// Waiting pairings by their code, without the dash.
pub(crate) type OpenPairings = std::collections::HashMap<String, Pairing>;

/// What Forge learns when it asks about its pairing.
#[derive(Debug, PartialEq, Eq)]
pub enum PairingState {
    Waiting,
    Approved(String, Account),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairingError {
    /// No such code, or it ended, or it was refused.
    Ended,
    TooManyAttempts,
    Unauthorized,
    Unavailable,
}

/// The code as people read it: `K7QM-42XP`.
fn show_code(code: &str) -> String {
    format!("{}-{}", &code[..4], &code[4..])
}

/// What someone typed or the page sent, as the code is kept: upper case, no dash or spaces.
fn clean_code(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn new_code() -> String {
    // 32 random bytes, hashed, give bytes that are uniform; 8 of them make the code.
    let hex = ck_crypto::sha256_hex(ck_crypto::generate_stick_token().expose().as_bytes());
    (0..8)
        .map(|i| {
            let byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex");
            CODE_ALPHABET[usize::from(byte % 32)] as char
        })
        .collect()
}

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

    /// Keeper Forge asks for a pairing. It gets the code to show the admin and a secret to ask
    /// about the answer with; nothing is tied to an account yet.
    pub fn start_pairing(&self) -> Result<(String, String), PairingError> {
        let now = (self.clock)();
        if !self.accounts.under_limit(
            "pair-start:*",
            now,
            MAX_PAIRING_STARTS,
            PAIRING_START_WINDOW_SECONDS,
        ) {
            return Err(PairingError::TooManyAttempts);
        }
        self.accounts
            .count("pair-start:*", now, PAIRING_START_WINDOW_SECONDS);
        let secret = ck_crypto::generate_stick_token().expose().to_owned();
        let mut open = self.accounts.forge_pairings.lock().expect("pairings lock");
        open.retain(|_, p| now < p.ends);
        if open.len() >= MAX_OPEN_PAIRINGS {
            return Err(PairingError::Unavailable);
        }
        let mut code = new_code();
        while open.contains_key(&code) {
            code = new_code();
        }
        open.insert(
            code.clone(),
            Pairing {
                secret_hash: token_hash(&secret),
                ends: now + Duration::seconds(PAIRING_LIFETIME_SECONDS),
                approved_by: None,
            },
        );
        Ok((show_code(&code), secret))
    }

    /// The signed-in admin says whether the code on the page is the one Keeper Forge shows.
    pub fn answer_pairing(
        &self,
        session: &str,
        code: &str,
        approve: bool,
    ) -> Result<(), PairingError> {
        let account = self.account(session).map_err(|reason| match reason {
            AccessError::Unavailable => PairingError::Unavailable,
            _ => PairingError::Unauthorized,
        })?;
        let now = (self.clock)();
        let key = format!("pair-try:{}", account.id);
        if !self
            .accounts
            .under_limit(&key, now, MAX_WRONG_CODES, WRONG_CODE_WINDOW_SECONDS)
        {
            return Err(PairingError::TooManyAttempts);
        }
        let mut open = self.accounts.forge_pairings.lock().expect("pairings lock");
        open.retain(|_, p| now < p.ends);
        let code = clean_code(code);
        if !approve {
            // Refusing a code that is not there is not a guess worth counting, but it is not an answer either.
            return open.remove(&code).map(|_| ()).ok_or(PairingError::Ended);
        }
        match open.get_mut(&code) {
            Some(pairing) if pairing.approved_by.is_none() => {
                pairing.approved_by = Some(account.id);
                Ok(())
            }
            _ => {
                self.accounts.count(&key, now, WRONG_CODE_WINDOW_SECONDS);
                Err(PairingError::Ended)
            }
        }
    }

    /// Keeper Forge asks whether the admin agreed. The answer is given once: the session is made
    /// and the pairing is used up.
    pub fn poll_pairing(&self, secret: &str) -> Result<PairingState, PairingError> {
        let now = (self.clock)();
        let hash = token_hash(secret);
        let user_id = {
            let mut open = self.accounts.forge_pairings.lock().expect("pairings lock");
            open.retain(|_, p| now < p.ends);
            let Some((code, pairing)) = open.iter().find(|(_, p)| p.secret_hash == hash) else {
                return Err(PairingError::Ended);
            };
            let Some(user_id) = pairing.approved_by.clone() else {
                return Ok(PairingState::Waiting);
            };
            let code = code.clone();
            open.remove(&code);
            user_id
        };
        let store_failed = |error| {
            unavailable(&error);
            PairingError::Unavailable
        };
        let user = self
            .store
            .user(&user_id)
            .map_err(store_failed)?
            .ok_or(PairingError::Ended)?;
        let token = ck_crypto::generate_stick_token().expose().to_owned();
        self.store
            .create_session(
                &token_hash(&token),
                &user.id,
                now + Duration::hours(SESSION_LIFETIME_HOURS),
            )
            .map_err(store_failed)?;
        Ok(PairingState::Approved(
            token,
            Account {
                id: user.id,
                email: user.email,
                staff: user.is_platform_staff,
            },
        ))
    }
}
