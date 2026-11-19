//! One-time enrolment codes: how a new stick gets its own secret without one being written to its
//! disk first. A stick can trade the code at `/api/v1/enrol` for a generated credential, or claim
//! a credential it has already stored at `/api/v1/enrol/claim`.
//!
//! An enrolment record keeps the hash of the code and, after a claim, the public stick id.
//! Logs and errors contain neither the code nor the token.

use ck_protocol::{StickId, Token};
use serde::Serialize;
use time::{Duration, OffsetDateTime};

use crate::accounts::Account;
use crate::credentials::CredentialScope;
use crate::hub::Hub;
use crate::register::valid_token;
use crate::store::StoreError;

/// A code that is not used by then is worthless.
pub const DEFAULT_LIFETIME_MINUTES: i64 = 60;
pub const MIN_LIFETIME_MINUTES: i64 = 5;
pub const MAX_LIFETIME_MINUTES: i64 = 24 * 60;

/// Codes one person may mint in a minute.
pub const MAX_MINTS_PER_MINUTE: u32 = 30;

/// Failed attempts the Hub takes from everyone together in a minute. The code is far too random to
/// guess, so this only keeps a flood of guesses from costing database work.
pub const MAX_FAILED_PER_MINUTE: u32 = 30;

/// Codes made and used long ago are removed when a new one is made.
const KEEP_DAYS: i64 = 1;

/// Time after the first claim in which the same proof recovers a lost acknowledgement.
pub(crate) const CLAIM_RECOVERY: Duration = Duration::hours(1);

const MAX_TEXT_LENGTH: usize = 200;

/// 32 letters and digits without the ones that look alike (I, O, 0, 1).
const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

const CODE_LENGTH: usize = 20;

/// What a stick gets with its code: the team, the site and the number it was minted for.
pub struct NewEnrolmentCode {
    pub code_hash: String,
    pub created_by_user_id: String,
    pub team: String,
    pub site: String,
    pub contest: String,
    pub machine_number: Option<u32>,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub credential_scope: Option<CredentialScope>,
}

/// What the Hub knows about the stick a code was made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolledStick {
    pub organization: String,
    pub team: String,
    pub site: String,
    pub contest: String,
    pub machine_number: Option<u32>,
}

/// How an attempt to redeem a code ended in the store.
#[derive(Debug, PartialEq, Eq)]
pub enum Redemption {
    /// Unknown, already used or expired: the three look the same from outside.
    Invalid,
    /// The stick id chosen is taken. The code was not used up, so the attempt can be repeated.
    IdTaken,
    Done(EnrolledStick),
}

/// What the minting person gets, once. It has no `Debug`, so it cannot reach a log.
#[derive(Serialize)]
pub struct MintedCode {
    pub code: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// What the stick gets, once. It has no `Debug`, so the secret cannot reach a log.
#[derive(Serialize)]
pub struct Enrolment {
    pub stick_id: StickId,
    pub token: String,
    pub team: String,
    pub site: String,
    pub contest: String,
    pub machine_number: Option<u32>,
}

/// What an admin decides about the stick when minting.
#[derive(Debug, Default)]
pub struct EnrolmentSpec {
    pub team: String,
    pub site: String,
    pub contest: String,
    pub machine_number: Option<u32>,
    pub lifetime_minutes: Option<i64>,
    pub credential_scope: Option<CredentialScope>,
}

#[derive(Debug, thiserror::Error)]
pub enum EnrolError {
    #[error("{0}")]
    BadRequest(String),
    #[error("too many requests in a minute")]
    RateLimited,
    /// The code is not valid. The reason is not told.
    #[error("the code is not valid")]
    Invalid,
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn plain_text(text: &str) -> bool {
    text.chars().count() <= MAX_TEXT_LENGTH && !text.chars().any(char::is_control)
}

/// The code as typed, made comparable: capitals, no dashes or spaces. `None` when it cannot be a
/// code, which saves a database call for nonsense.
fn normalized(text: &str) -> Option<String> {
    let clean: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let valid = clean.len() == CODE_LENGTH && clean.bytes().all(|b| ALPHABET.contains(&b));
    valid.then_some(clean)
}

fn hash_of(normalized: &str) -> String {
    ck_crypto::sha256_hex(normalized.as_bytes())
}

/// A new code: 20 characters from a fresh random secret, written in groups of four.
fn new_code() -> (String, String) {
    let secret = ck_crypto::generate_stick_token();
    let bytes = ck_crypto::sha256_hex(secret.expose().as_bytes());
    // Two hex digits are one byte; 256 is a multiple of 32, so every letter is equally likely.
    let plain: String = (0..CODE_LENGTH)
        .map(|i| {
            let byte = u8::from_str_radix(&bytes[2 * i..2 * i + 2], 16).expect("hex");
            char::from(ALPHABET[usize::from(byte) % ALPHABET.len()])
        })
        .collect();
    let shown = plain
        .as_bytes()
        .chunks(4)
        .map(|group| String::from_utf8_lossy(group).into_owned())
        .collect::<Vec<_>>()
        .join("-");
    (shown, plain)
}

fn new_stick_id() -> StickId {
    let secret = ck_crypto::generate_stick_token();
    let hex = ck_crypto::sha256_hex(secret.expose().as_bytes());
    StickId::parse(&hex[..8]).expect("eight hex digits are a stick id")
}

impl Hub {
    /// Makes a code for one new stick of `organization`. The code is shown once, here.
    ///
    /// # Errors
    /// [`EnrolError::BadRequest`] for a text or a lifetime that is not allowed,
    /// [`EnrolError::RateLimited`] past the limit.
    pub fn mint_enrolment_code(
        &self,
        organization: &str,
        creator: &Account,
        spec: EnrolmentSpec,
    ) -> Result<MintedCode, EnrolError> {
        if let Some(scope) = &spec.credential_scope {
            scope
                .validate()
                .map_err(|error| EnrolError::BadRequest(error.to_owned()))?;
        }
        if ![&spec.team, &spec.site, &spec.contest]
            .iter()
            .all(|text| plain_text(text))
        {
            return Err(EnrolError::BadRequest(format!(
                "the team, site and contest must be at most {MAX_TEXT_LENGTH} characters without control characters"
            )));
        }
        if spec
            .machine_number
            .is_some_and(|number| i32::try_from(number).is_err())
        {
            return Err(EnrolError::BadRequest(
                "the machine number is too large".to_owned(),
            ));
        }
        let minutes = spec.lifetime_minutes.unwrap_or(DEFAULT_LIFETIME_MINUTES);
        if !(MIN_LIFETIME_MINUTES..=MAX_LIFETIME_MINUTES).contains(&minutes) {
            return Err(EnrolError::BadRequest(format!(
                "the lifetime must be {MIN_LIFETIME_MINUTES} to {MAX_LIFETIME_MINUTES} minutes"
            )));
        }
        let now = (self.clock)();
        window_count(&self.enrol_mints, &creator.id, now, MAX_MINTS_PER_MINUTE)?;
        let (shown, plain) = new_code();
        let expires_at = now + Duration::minutes(minutes);
        self.store.add_enrolment_code(
            organization,
            NewEnrolmentCode {
                code_hash: hash_of(&plain),
                created_by_user_id: creator.id.clone(),
                team: spec.team,
                site: spec.site,
                contest: spec.contest,
                machine_number: spec.machine_number,
                created_at: now,
                expires_at,
                credential_scope: spec.credential_scope,
            },
        )?;
        self.store
            .delete_enrolment_codes_before(organization, now - Duration::days(KEEP_DAYS))?;
        Ok(MintedCode {
            code: shown,
            expires_at,
        })
    }

    /// Trades a code for a new stick: the Hub makes its id and secret, registers it in the
    /// organization the code was made for and hands both over, once.
    ///
    /// # Errors
    /// [`EnrolError::Invalid`] for a code that is unknown, used or expired,
    /// [`EnrolError::RateLimited`] when too many attempts failed lately.
    pub fn enrol(&self, code: &str) -> Result<Enrolment, EnrolError> {
        let now = (self.clock)();
        let hash = self.hash_for_attempt(code, now)?;
        let token = ck_crypto::generate_stick_token();
        // A taken id is a one in four billion chance; try again a few times before giving up.
        for _ in 0..3 {
            let id = new_stick_id();
            match self.store.redeem_enrolment_code(&hash, now, &id, &token)? {
                Redemption::Done(stick) => {
                    return Ok(Enrolment {
                        stick_id: id,
                        token: token.expose().to_owned(),
                        team: stick.team,
                        site: stick.site,
                        contest: stick.contest,
                        machine_number: stick.machine_number,
                    });
                }
                Redemption::Invalid => return Err(self.failed_attempt(now)),
                Redemption::IdTaken => {}
            }
        }
        Err(EnrolError::Store(StoreError(
            "no free stick id was found".to_owned(),
        )))
    }

    pub(crate) fn claim_enrolment(
        &self,
        code: &str,
        stick_id: &str,
        token: String,
    ) -> Result<StickId, EnrolError> {
        let now = (self.clock)();
        let hash = self.hash_for_attempt(code, now)?;
        let id = StickId::parse(stick_id).map_err(|_| self.failed_attempt(now))?;
        if !valid_token(&token) {
            return Err(self.failed_attempt(now));
        }
        {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            if registry.find(&id).is_some() {
                return Err(self.failed_attempt(now));
            }
        }
        match self
            .store
            .claim_enrolment_code(&hash, now, &id, &Token::new(token))?
        {
            Redemption::Done(_) => Ok(id),
            Redemption::Invalid | Redemption::IdTaken => Err(self.failed_attempt(now)),
        }
    }

    fn hash_for_attempt(&self, code: &str, now: OffsetDateTime) -> Result<String, EnrolError> {
        self.refuse_when_flooded(now)?;
        let plain = normalized(code).ok_or_else(|| self.failed_attempt(now))?;
        Ok(hash_of(&plain))
    }

    fn refuse_when_flooded(&self, now: OffsetDateTime) -> Result<(), EnrolError> {
        let failed = self.enrol_failures.lock().expect("enrol failures lock");
        match *failed {
            (since, count)
                if (now - since).whole_seconds() < 60 && count >= MAX_FAILED_PER_MINUTE =>
            {
                Err(EnrolError::RateLimited)
            }
            _ => Ok(()),
        }
    }

    fn failed_attempt(&self, now: OffsetDateTime) -> EnrolError {
        let mut failed = self.enrol_failures.lock().expect("enrol failures lock");
        if (now - failed.0).whole_seconds() >= 60 {
            *failed = (now, 0);
        }
        failed.1 += 1;
        EnrolError::Invalid
    }
}

/// Counts one more in the person's current minute; refuses past `max`.
fn window_count(
    rates: &std::sync::Mutex<std::collections::HashMap<String, (OffsetDateTime, u32)>>,
    key: &str,
    now: OffsetDateTime,
    max: u32,
) -> Result<(), EnrolError> {
    let mut rates = rates.lock().expect("enrol rates lock");
    let (since, count) = rates.entry(key.to_owned()).or_insert((now, 0));
    if (now - *since).whole_seconds() >= 60 {
        (*since, *count) = (now, 0);
    }
    *count += 1;
    if *count > max {
        return Err(EnrolError::RateLimited);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_twenty_characters_in_groups_and_is_read_back_any_way_it_is_typed() {
        let (shown, plain) = new_code();
        assert_eq!(shown.len(), 24);
        assert_eq!(shown.matches('-').count(), 4);
        assert_eq!(plain.len(), CODE_LENGTH);
        assert_eq!(normalized(&shown), Some(plain.clone()));
        assert_eq!(normalized(&shown.to_lowercase()), Some(plain.clone()));
        assert_eq!(
            normalized(&format!(" {} ", shown.replace('-', " "))),
            Some(plain)
        );
        assert_ne!(new_code().1, new_code().1);
    }

    #[test]
    fn nonsense_is_not_a_code() {
        for text in [
            "",
            "abc",
            "0000-0000-0000-0000-0000",
            "ABCD-EFGH-JKLM-NPQR-STUV-W",
            "ABCD-EFGH-JKLM-NPQR-STU!",
        ] {
            assert_eq!(normalized(text), None, "{text}");
        }
    }

    #[test]
    fn the_hash_is_not_the_code() {
        let (_, plain) = new_code();
        let hash = hash_of(&plain);
        assert_eq!(hash.len(), 64);
        assert!(!hash.contains(&plain));
    }

    #[test]
    fn a_stick_id_is_eight_hex_digits() {
        let id = new_stick_id();
        assert_eq!(id.as_str().len(), 8);
    }
}
