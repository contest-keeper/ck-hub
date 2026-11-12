//! Organizer accounts: logging in, sessions and roles.
//!
//! Sticks do not use this. They sign their messages with their own secret.

use std::collections::HashMap;
use std::sync::Mutex;

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, Duration, Utc};

use crate::hub::Hub;
use crate::store::{Role, Store, StoreError, User};

/// A browser that has not been used for this long has to log in again.
pub const SESSION_LIFETIME_HOURS: i64 = 12;
/// Wrong passwords tried for one name before it is locked for [`NAME_LOCKOUT_SECONDS`].
pub const MAX_FAILED_LOGINS: u32 = 5;
pub const NAME_LOCKOUT_SECONDS: i64 = 300;
/// Wrong passwords tried for any names in [`ALL_LOCKOUT_SECONDS`], which stops guessing across
/// many names at once.
pub const MAX_FAILED_LOGINS_ALL: u32 = 60;
pub const ALL_LOCKOUT_SECONDS: i64 = 60;
pub const MIN_PASSWORD_LENGTH: usize = 12;
pub const MAX_NAME_LENGTH: usize = 64;

/// How much work hashing a password takes. The default is the minimum OWASP recommends for
/// Argon2id; a slower machine can afford more.
#[derive(Debug, Clone, Copy)]
pub struct PasswordCost {
    pub memory_kib: u32,
    pub passes: u32,
}

impl Default for PasswordCost {
    fn default() -> Self {
        Self {
            memory_kib: 19 * 1024,
            passes: 2,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginError {
    /// The same answer for an unknown name and a wrong password.
    BadCredentials,
    TooManyAttempts,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AccessError {
    /// No session, or one that ended.
    Unauthorized,
    /// A session whose role is not allowed to do this.
    Forbidden,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreateUserError {
    BadName,
    PasswordTooShort,
    NameTaken,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub role: Role,
}

struct Failures {
    count: u32,
    since: DateTime<Utc>,
}

/// What the Hub keeps for accounts besides the database.
pub struct Accounts {
    cost: PasswordCost,
    /// A hash nobody can match, checked when the name is unknown so that an unknown name takes as
    /// long to refuse as a wrong password.
    dummy_hash: String,
    failures: Mutex<HashMap<String, Failures>>,
}

impl Accounts {
    pub(crate) fn new(cost: PasswordCost) -> Self {
        Self {
            cost,
            dummy_hash: hash(cost, "a password nobody uses").expect("the dummy hash"),
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn blocked(&self, key: &str, now: DateTime<Utc>, max: u32, window: i64) -> bool {
        let failures = self.failures.lock().expect("failures lock");
        failures
            .get(key)
            .is_some_and(|f| f.count >= max && now < f.since + Duration::seconds(window))
    }

    fn fail(&self, key: &str, now: DateTime<Utc>, window: i64) {
        let mut failures = self.failures.lock().expect("failures lock");
        let entry = failures.entry(key.to_owned()).or_insert(Failures {
            count: 0,
            since: now,
        });
        // A new window starts when the old one is over.
        if now >= entry.since + Duration::seconds(window) {
            *entry = Failures {
                count: 0,
                since: now,
            };
        }
        entry.count += 1;
    }

    fn clear(&self, key: &str) {
        self.failures.lock().expect("failures lock").remove(key);
    }
}

fn hash(cost: PasswordCost, password: &str) -> Result<String, String> {
    let params = Params::new(cost.memory_kib, cost.passes, 1, None).map_err(|e| e.to_string())?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| e.to_string())
}

fn token_hash(token: &str) -> String {
    ck_crypto::sha256_hex(token.as_bytes())
}

fn unavailable(error: &StoreError) {
    tracing::error!("{error}");
}

impl Hub {
    /// Adds an organizer account.
    ///
    /// # Errors
    /// See [`CreateUserError`].
    pub fn create_user(
        &self,
        name: &str,
        password: &str,
        role: Role,
    ) -> Result<(), CreateUserError> {
        create_user(&*self.store, self.accounts.cost, name, password, role)
    }

    /// Checks a name and password. On success returns the session token for the browser's cookie.
    ///
    /// # Errors
    /// See [`LoginError`].
    pub fn login(&self, name: &str, password: &str) -> Result<(String, Account), LoginError> {
        let now = (self.clock)();
        let key = name.to_lowercase();
        let accounts = &self.accounts;
        // A locked name or a flood of guesses is refused before any hashing is done.
        if accounts.blocked(&key, now, MAX_FAILED_LOGINS, NAME_LOCKOUT_SECONDS)
            || accounts.blocked("*", now, MAX_FAILED_LOGINS_ALL, ALL_LOCKOUT_SECONDS)
        {
            return Err(LoginError::TooManyAttempts);
        }

        let user = self.store.user(name).map_err(|error| {
            unavailable(&error);
            LoginError::Unavailable
        })?;
        let hash = user
            .as_ref()
            .map_or(accounts.dummy_hash.as_str(), |u| u.password_hash.as_str());
        let matches = Argon2::default()
            .verify_password(password.as_bytes(), hash)
            .is_ok();
        let Some(user) = user.filter(|_| matches) else {
            accounts.fail(&key, now, NAME_LOCKOUT_SECONDS);
            accounts.fail("*", now, ALL_LOCKOUT_SECONDS);
            return Err(LoginError::BadCredentials);
        };
        accounts.clear(&key);

        let token = ck_crypto::generate_stick_token();
        let token = token.expose().to_owned();
        self.store
            .create_session(
                &token_hash(&token),
                &user.name,
                now + Duration::hours(SESSION_LIFETIME_HOURS),
            )
            .map_err(|error| {
                unavailable(&error);
                LoginError::Unavailable
            })?;
        Ok((
            token,
            Account {
                name: user.name,
                role: user.role,
            },
        ))
    }

    /// Who the session token belongs to, if its role may do what `needed` asks.
    ///
    /// # Errors
    /// See [`AccessError`].
    pub fn access(&self, token: &str, needed: Role) -> Result<Account, AccessError> {
        let hash = token_hash(token);
        let store_failed = |error: StoreError| {
            unavailable(&error);
            AccessError::Unavailable
        };
        let (name, ends) = self
            .store
            .session(&hash)
            .map_err(store_failed)?
            .ok_or(AccessError::Unauthorized)?;
        if ends <= (self.clock)() {
            self.store.delete_session(&hash).map_err(store_failed)?;
            return Err(AccessError::Unauthorized);
        }
        // The role is read now, so a changed role applies to a session that is already open.
        let user = self
            .store
            .user(&name)
            .map_err(store_failed)?
            .ok_or(AccessError::Unauthorized)?;
        if !user.role.allows(needed) {
            return Err(AccessError::Forbidden);
        }
        Ok(Account {
            name: user.name,
            role: user.role,
        })
    }

    /// Ends a session. A token that is not a session is ignored.
    pub fn logout(&self, token: &str) {
        if let Err(error) = self.store.delete_session(&token_hash(token)) {
            unavailable(&error);
        }
    }
}

/// Adds an organizer account to `store`: checks the name and the password, and keeps only a hash of
/// the password. `ck-hub create-user` uses this without starting a Hub.
///
/// # Errors
/// See [`CreateUserError`].
pub fn create_user(
    store: &dyn Store,
    cost: PasswordCost,
    name: &str,
    password: &str,
    role: Role,
) -> Result<(), CreateUserError> {
    let valid_name = !name.trim().is_empty()
        && name.chars().count() <= MAX_NAME_LENGTH
        && !name.chars().any(char::is_control);
    if !valid_name {
        return Err(CreateUserError::BadName);
    }
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(CreateUserError::PasswordTooShort);
    }
    let password_hash = hash(cost, password).map_err(|error| {
        tracing::error!("cannot hash a password: {error}");
        CreateUserError::Unavailable
    })?;
    let created = store
        .create_user(User {
            name: name.to_owned(),
            password_hash,
            role,
        })
        .map_err(|error| {
            unavailable(&error);
            CreateUserError::Unavailable
        })?;
    if created {
        Ok(())
    } else {
        Err(CreateUserError::NameTaken)
    }
}
