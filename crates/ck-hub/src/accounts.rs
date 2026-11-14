//! Accounts: logging in, sessions, organizations and roles.
//!
//! Sticks do not use this. They sign their messages with their own secret.

use std::collections::HashMap;
use std::sync::Mutex;

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, Duration, Utc};

use crate::hub::Hub;
use crate::store::{Membership, NewUser, Organization, Role, Store, StoreError, User};

/// A session ends this long after the login, used or not.
pub const SESSION_LIFETIME_HOURS: i64 = 12;
/// Wrong passwords tried for one email before it is locked for [`NAME_LOCKOUT_SECONDS`].
pub const MAX_FAILED_LOGINS: u32 = 5;
pub const NAME_LOCKOUT_SECONDS: i64 = 300;
/// Wrong passwords tried for any names in [`ALL_LOCKOUT_SECONDS`], which stops guessing across
/// many names at once.
pub const MAX_FAILED_LOGINS_ALL: u32 = 60;
pub const ALL_LOCKOUT_SECONDS: i64 = 60;
pub const MIN_PASSWORD_LENGTH: usize = 12;
pub const MAX_EMAIL_LENGTH: usize = 254;

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
    /// No such organization, or the user is not a member. The two look the same.
    NotFound,
    /// A member whose role is not allowed to do this.
    Forbidden,
    /// No organization was named and the user belongs to several.
    Ambiguous,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreateUserError {
    BadEmail,
    PasswordTooShort,
    EmailTaken,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OrganizationError {
    BadSlug,
    SlugTaken,
    NoSuchOrganization,
    NoSuchUser,
    AlreadyMember,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub email: String,
}

/// Who is asking, in which organization, and with what role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    pub account: Account,
    pub organization: Organization,
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
        // Failures older than the longest lockout no longer count, so they are dropped here and
        // the map cannot grow for ever.
        failures.retain(|_, f| now < f.since + Duration::seconds(NAME_LOCKOUT_SECONDS));
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
    pub fn create_user(&self, email: &str, password: &str) -> Result<User, CreateUserError> {
        create_user(self.store.as_ref(), self.accounts.cost, email, password)
    }

    pub fn create_organization(
        &self,
        slug: &str,
        name: &str,
        owner_email: &str,
    ) -> Result<Organization, OrganizationError> {
        create_organization(self.store.as_ref(), slug, name, owner_email)
    }

    pub fn add_member(&self, slug: &str, email: &str, role: Role) -> Result<(), OrganizationError> {
        add_member(self.store.as_ref(), slug, email, role)
    }

    /// Checks an email and password. On success returns the session token for the browser's cookie.
    pub fn login(&self, email: &str, password: &str) -> Result<(String, Account), LoginError> {
        let now = (self.clock)();
        // No account has a longer email, so such a text is only counted with all the others: it
        // must not make the Hub remember text of any size a stranger sends.
        let key = (email.chars().count() <= MAX_EMAIL_LENGTH).then(|| email.to_lowercase());
        let accounts = &self.accounts;
        // A locked email or a flood of guesses is refused before any hashing is done.
        if key
            .as_deref()
            .is_some_and(|key| accounts.blocked(key, now, MAX_FAILED_LOGINS, NAME_LOCKOUT_SECONDS))
            || accounts.blocked("*", now, MAX_FAILED_LOGINS_ALL, ALL_LOCKOUT_SECONDS)
        {
            return Err(LoginError::TooManyAttempts);
        }

        let user = self.store.user_by_email(email).map_err(|error| {
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
            if let Some(key) = &key {
                accounts.fail(key, now, NAME_LOCKOUT_SECONDS);
            }
            accounts.fail("*", now, ALL_LOCKOUT_SECONDS);
            return Err(LoginError::BadCredentials);
        };
        if let Some(key) = &key {
            accounts.clear(key);
        }

        // Expired sessions are only ever deleted here, so the table does not grow for ever.
        self.store.delete_expired_sessions(now).map_err(|error| {
            unavailable(&error);
            LoginError::Unavailable
        })?;
        let token = ck_crypto::generate_stick_token();
        let token = token.expose().to_owned();
        self.store
            .create_session(
                &token_hash(&token),
                &user.id,
                now + Duration::hours(SESSION_LIFETIME_HOURS),
            )
            .map_err(|error| {
                unavailable(&error);
                LoginError::Unavailable
            })?;
        Ok((
            token,
            Account {
                id: user.id,
                email: user.email,
            },
        ))
    }

    /// The account a session token belongs to.
    pub fn account(&self, token: &str) -> Result<Account, AccessError> {
        let hash = token_hash(token);
        let store_failed = |error: StoreError| {
            unavailable(&error);
            AccessError::Unavailable
        };
        let (user_id, ends) = self
            .store
            .session(&hash)
            .map_err(store_failed)?
            .ok_or(AccessError::Unauthorized)?;
        if ends <= (self.clock)() {
            self.store.delete_session(&hash).map_err(store_failed)?;
            return Err(AccessError::Unauthorized);
        }
        let user = self
            .store
            .user(&user_id)
            .map_err(store_failed)?
            .ok_or(AccessError::Unauthorized)?;
        Ok(Account {
            id: user.id,
            email: user.email,
        })
    }

    /// The organizations the account belongs to.
    pub fn organizations_of(&self, account: &Account) -> Result<Vec<Membership>, AccessError> {
        self.store.memberships(&account.id).map_err(|error| {
            unavailable(&error);
            AccessError::Unavailable
        })
    }

    /// The account, if it is a member of the organization with this slug and its role may do what
    /// `needed` asks for.
    pub fn access(&self, token: &str, slug: &str, needed: Role) -> Result<Access, AccessError> {
        let account = self.account(token)?;
        let store_failed = |error: StoreError| {
            unavailable(&error);
            AccessError::Unavailable
        };
        let organization = self
            .store
            .organization_by_slug(slug)
            .map_err(store_failed)?
            .ok_or(AccessError::NotFound)?;
        // The role is read now, so a changed role applies to a session that is already open.
        let role = self
            .store
            .membership(&organization.id, &account.id)
            .map_err(store_failed)?
            .ok_or(AccessError::NotFound)?;
        if !role.allows(needed) {
            return Err(AccessError::Forbidden);
        }
        Ok(Access {
            account,
            organization,
            role,
        })
    }

    /// For the routes without an organization in the address: the one the account belongs to.
    pub fn access_sole(&self, token: &str, needed: Role) -> Result<Access, AccessError> {
        let account = self.account(token)?;
        let mut memberships = self.organizations_of(&account)?;
        let membership = match memberships.len() {
            0 => return Err(AccessError::NotFound),
            1 => memberships.remove(0),
            _ => return Err(AccessError::Ambiguous),
        };
        if !membership.role.allows(needed) {
            return Err(AccessError::Forbidden);
        }
        Ok(Access {
            account,
            organization: membership.organization,
            role: membership.role,
        })
    }

    /// Ends a session. A token that is not a session is ignored.
    pub fn logout(&self, token: &str) {
        if let Err(error) = self.store.delete_session(&token_hash(token)) {
            unavailable(&error);
        }
    }
}

fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    email.chars().count() <= MAX_EMAIL_LENGTH
        && !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !email.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn valid_slug(slug: &str) -> bool {
    (1..=63).contains(&slug.len())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
}

/// Adds an account to `store`: checks the email and the password, and keeps only a hash of the
/// password. `ck-hub create-user` uses this without starting a Hub.
pub fn create_user(
    store: &dyn Store,
    cost: PasswordCost,
    email: &str,
    password: &str,
) -> Result<User, CreateUserError> {
    if !valid_email(email) {
        return Err(CreateUserError::BadEmail);
    }
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(CreateUserError::PasswordTooShort);
    }
    let password_hash = hash(cost, password).map_err(|error| {
        tracing::error!("cannot hash a password: {error}");
        CreateUserError::Unavailable
    })?;
    let created = store
        .create_user(NewUser {
            email: email.to_owned(),
            password_hash,
        })
        .map_err(|error| {
            unavailable(&error);
            CreateUserError::Unavailable
        })?;
    created.ok_or(CreateUserError::EmailTaken)
}

/// Makes an organization whose first owner is the account with `owner_email`.
pub fn create_organization(
    store: &dyn Store,
    slug: &str,
    name: &str,
    owner_email: &str,
) -> Result<Organization, OrganizationError> {
    let store_failed = |error: StoreError| {
        unavailable(&error);
        OrganizationError::Unavailable
    };
    if !valid_slug(slug) || name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(OrganizationError::BadSlug);
    }
    let owner = store
        .user_by_email(owner_email)
        .map_err(store_failed)?
        .ok_or(OrganizationError::NoSuchUser)?;
    let organization = store
        .create_organization(slug, name)
        .map_err(store_failed)?
        .ok_or(OrganizationError::SlugTaken)?;
    store
        .add_member(&organization.id, &owner.id, Role::Owner)
        .map_err(store_failed)?;
    Ok(organization)
}

/// Makes the account with `email` a member of the organization with this slug.
pub fn add_member(
    store: &dyn Store,
    slug: &str,
    email: &str,
    role: Role,
) -> Result<(), OrganizationError> {
    let store_failed = |error: StoreError| {
        unavailable(&error);
        OrganizationError::Unavailable
    };
    let organization = store
        .organization_by_slug(slug)
        .map_err(store_failed)?
        .ok_or(OrganizationError::NoSuchOrganization)?;
    let user = store
        .user_by_email(email)
        .map_err(store_failed)?
        .ok_or(OrganizationError::NoSuchUser)?;
    if store
        .add_member(&organization.id, &user.id, role)
        .map_err(store_failed)?
    {
        Ok(())
    } else {
        Err(OrganizationError::AlreadyMember)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::registry::Registry;
    use crate::store::MemoryStore;

    fn hub(now: Arc<Mutex<DateTime<Utc>>>) -> Hub {
        let clock = Box::new(move || *now.lock().expect("clock"));
        Hub::new(
            Registry::empty(),
            Arc::new(MemoryStore::default()),
            None,
            clock,
        )
        .with_password_cost(PasswordCost {
            memory_kib: 8,
            passes: 1,
        })
    }

    fn remembered(hub: &Hub) -> usize {
        hub.accounts.failures.lock().expect("failures lock").len()
    }

    #[test]
    fn wrong_logins_cannot_fill_the_memory_of_the_hub() {
        let now = Arc::new(Mutex::new(Utc::now()));
        let hub = hub(Arc::clone(&now));

        // A name no account can have is refused like any other, but not remembered by name.
        let huge = "x".repeat(60_000);
        assert_eq!(
            hub.login(&huge, "whatever").err(),
            Some(LoginError::BadCredentials)
        );
        assert_eq!(remembered(&hub), 1, "only the count for all names");

        for name in ["a", "b", "c"] {
            assert!(hub.login(name, "whatever").is_err());
        }
        assert_eq!(remembered(&hub), 4);

        // Once their lockout time has passed, old failures are forgotten at the next one.
        *now.lock().expect("clock") += Duration::seconds(NAME_LOCKOUT_SECONDS);
        assert!(hub.login("d", "whatever").is_err());
        assert_eq!(
            remembered(&hub),
            2,
            "the new name and the count for all names"
        );
    }
}
