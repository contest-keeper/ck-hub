# Registration, Email Verification and Password Reset Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A person can create an account with email and password, confirm the email, and reset a forgotten password, all through the Hub's own pages and API.

**Architecture:** Accounts get a verified flag and single-use, expiring, hashed email tokens (a new `email_tokens` table). A small `Mailer` trait sends the links; this plan ships a console mailer for local runs and an in-memory one for tests. Links carry the token in the URL fragment (`/#verify=...`, `/#reset=...`), so a mail scanner that opens the link does not use it up and the token never reaches a server log. Registration only makes an account: access to an organization still comes from a membership (plan 4 adds invitations). This is piece 2 of 4 of `docs/superpowers/specs/2026-11-14-accounts-and-organizations-design.md`.

**Tech Stack:** Rust 2024, axum 0.8, `postgres` crate, Argon2id, `ck_crypto` for tokens and SHA-256, plain SQL migrations.

## Decisions made here (change them before starting if you disagree)

- **No mail setup, no sign-up.** A Hub started without `--mail` answers `503` on register and password reset, so an open registration never accepts accounts that cannot be confirmed.
- **Accounts that exist are verified.** Migration 0007 marks every current account verified. `ck-hub create-user` and `Hub::create_user` (staff paths) also create verified accounts. Only `POST /api/v1/register` makes unverified ones.
- **Registration does not reveal which emails have accounts.** It answers `202` for any valid email and password. For an address that already has an account, the mail says so (verified account) or carries a fresh link (unverified account). Bad input (`400`) leaks nothing.
- **Login before verifying** gets `403 email not verified`, but only after the password was right, so guessers learn nothing.
- **Password reset also verifies the email** (the link proves the person reads that mailbox), ends every session of the account, and clears its login lockout.
- **Limits:** at most 3 mails per address per 5 minutes and 120 per minute for the Hub. Over the limit the Hub still answers `202` and sends nothing.
- **Unverified accounts are purged** 48 hours after creation, when the next person registers.
- **`ConsoleMailer` prints the whole mail, links included, to stdout.** It exists for local runs only (`--mail console`), the spec asks for it, and it is the one place a token is printed on purpose. It must never be the cloud setting.
- **Not in this plan (needs a decision):** the real mail provider (open item in the spec). The `Mailer` trait is the seam; sending is synchronous inside the request, which is fine for the two mailers here and must move to a background queue when a network provider arrives.

## File Structure

- Create `crates/ck-hub/migrations/0007_email_verification.sql`: `email_verified_at`, `email_tokens`.
- Modify `crates/ck-hub/src/store.rs`: `User`/`NewUser` fields, `TokenPurpose`, six trait methods, memory implementation.
- Modify `crates/ck-hub/src/postgres_store.rs`: migration entry, selects, six methods.
- Create `crates/ck-hub/src/mail.rs`: `Mail`, `Mailer`, `MemoryMailer`, `ConsoleMailer`.
- Create `crates/ck-hub/src/signup.rs`: `Hub::register_account`, `verify_email`, `request_password_reset`, `reset_password`.
- Modify `crates/ck-hub/src/accounts.rs`: `LoginError::NotVerified`, mail limit, visibility of a few helpers.
- Modify `crates/ck-hub/src/hub.rs`: `mailer` and `public_url` fields, `with_mail`.
- Modify `crates/ck-hub/src/routes.rs`: four routes.
- Modify `crates/ck-hub/src/page.html`: register, forgot, reset modes and fragment handling.
- Modify `crates/ck-hub/src/main.rs`, `crates/ck-hub/src/lib.rs`, `README.md`.
- Create `crates/ck-hub/tests/signup.rs`; extend `crates/ck-hub/tests/store.rs`.

---

### Task 1: Store: verified flag, email tokens, password and session helpers

**Files:**
- Create: `crates/ck-hub/migrations/0007_email_verification.sql`
- Modify: `crates/ck-hub/src/store.rs`, `crates/ck-hub/src/postgres_store.rs`
- Test: `crates/ck-hub/tests/store.rs`

- [ ] **Step 1: Write the migration**

```sql
-- An account made by sign-up is unverified until its owner opens the link we mail. Every account
-- that exists now was made by staff or the command line, which vouch for the address.
ALTER TABLE users ADD COLUMN email_verified_at timestamptz;
UPDATE users SET email_verified_at = now();

-- One-time links for confirming an email and for resetting a password. Only a hash of the token
-- is kept, so a copy of this table cannot be used.
CREATE TABLE email_tokens (
    token_hash text PRIMARY KEY,
    user_id    text        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose    text        NOT NULL CHECK (purpose IN ('verify', 'reset')),
    expires_at timestamptz NOT NULL
);
CREATE INDEX email_tokens_by_user ON email_tokens (user_id, purpose);
CREATE INDEX email_tokens_by_expiry ON email_tokens (expires_at);
```

- [ ] **Step 2: Write the failing contract test** in `tests/store.rs`, next to `sessions_contract`. Call it from `the_store_contract`'s caller the same way `identity_contract` is called (read how `the_memory_store_keeps_its_contract` and the Postgres twin call the contracts and add `verification_contract(store)` to both).

```rust
fn verification_contract(store: &dyn Store) {
    let id = unique();
    let made = |email: &str, verified: Option<DateTime<Utc>>| {
        store
            .create_user(NewUser {
                email: email.to_owned(),
                password_hash: "hash".to_owned(),
                email_verified_at: verified,
                created_at: time(10),
            })
            .expect("write")
            .expect("a new account")
    };
    let fresh = made(&format!("{id}-fresh@example.org"), None);
    let vouched = made(&format!("{id}-vouched@example.org"), Some(time(5)));
    assert_eq!(fresh.email_verified_at, None);
    assert_eq!(vouched.email_verified_at, Some(time(5)));

    store.mark_email_verified(&fresh.id, time(20)).expect("write");
    assert_eq!(
        store.user(&fresh.id).expect("read").expect("user").email_verified_at,
        Some(time(20))
    );

    // A token is used once, only for its purpose, and not after it ended.
    store
        .put_email_token("t-verify", &fresh.id, TokenPurpose::Verify, time(100))
        .expect("write");
    assert_eq!(
        store.take_email_token("t-verify", TokenPurpose::Reset, time(50)).expect("read"),
        None
    );
    assert_eq!(
        store.take_email_token("t-verify", TokenPurpose::Verify, time(50)).expect("read"),
        Some(fresh.id.clone())
    );
    assert_eq!(
        store.take_email_token("t-verify", TokenPurpose::Verify, time(50)).expect("read"),
        None
    );
    store
        .put_email_token("t-late", &fresh.id, TokenPurpose::Reset, time(100))
        .expect("write");
    assert_eq!(
        store.take_email_token("t-late", TokenPurpose::Reset, time(100)).expect("read"),
        None
    );

    // A new token of a purpose replaces the account's earlier one.
    store
        .put_email_token("t-one", &fresh.id, TokenPurpose::Reset, time(500))
        .expect("write");
    store
        .put_email_token("t-two", &fresh.id, TokenPurpose::Reset, time(500))
        .expect("write");
    assert_eq!(
        store.take_email_token("t-one", TokenPurpose::Reset, time(1)).expect("read"),
        None
    );
    assert_eq!(
        store.take_email_token("t-two", TokenPurpose::Reset, time(1)).expect("read"),
        Some(fresh.id.clone())
    );

    // Passwords change, and every session of the account can be ended at once.
    store.set_password_hash(&fresh.id, "new-hash").expect("write");
    assert_eq!(
        store.user(&fresh.id).expect("read").expect("user").password_hash,
        "new-hash"
    );
    store.create_session("s1", &fresh.id, time(900)).expect("write");
    store.create_session("s2", &fresh.id, time(900)).expect("write");
    store.create_session("s3", &vouched.id, time(900)).expect("write");
    store.delete_sessions_of(&fresh.id).expect("write");
    assert!(store.session("s1").expect("read").is_none());
    assert!(store.session("s2").expect("read").is_none());
    assert!(store.session("s3").expect("read").is_some());

    // Expired tokens are removed. Unverified accounts older than the limit are removed with
    // their tokens; verified ones stay.
    let stale = made(&format!("{id}-stale@example.org"), None);
    store
        .put_email_token("t-old", &stale.id, TokenPurpose::Verify, time(30))
        .expect("write");
    store.delete_expired_email_tokens(time(40)).expect("write");
    assert_eq!(
        store.take_email_token("t-old", TokenPurpose::Verify, time(1)).expect("read"),
        None
    );
    store.delete_unverified_users_before(time(11)).expect("write");
    assert!(store.user(&stale.id).expect("read").is_none());
    assert!(store.user(&vouched.id).expect("read").is_some());
    assert!(store.user(&fresh.id).expect("read").is_some(), "verified in the meantime");
}
```

Update the existing `identity_contract` closure `new` (tests/store.rs:205) to pass `email_verified_at: Some(time(0)), created_at: time(0)`.

- [ ] **Step 3: Run to see it fail**

Run: `cargo test -p ck-hub --test store 2>&1 | tail -20`
Expected: compile errors (`email_verified_at`, `TokenPurpose`, the new methods do not exist).

- [ ] **Step 4: Implement in `store.rs`.** Add to `User` and `NewUser`:

```rust
    pub email_verified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
```

Add next to `Role`:

```rust
/// What a mailed one-time token may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenPurpose {
    Verify,
    Reset,
}

impl TokenPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Reset => "reset",
        }
    }
}
```

Add to the `Store` trait, after `delete_expired_sessions`:

```rust
    fn mark_email_verified(&self, user_id: &str, at: DateTime<Utc>) -> Result<(), StoreError>;

    fn set_password_hash(&self, user_id: &str, password_hash: &str) -> Result<(), StoreError>;

    /// Ends every session of the account.
    fn delete_sessions_of(&self, user_id: &str) -> Result<(), StoreError>;

    /// Keeps a hash of a mailed token. It replaces the account's earlier token of the same purpose.
    fn put_email_token(
        &self,
        token_hash: &str,
        user_id: &str,
        purpose: TokenPurpose,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Uses up a token: the account it was made for, if it exists, has this purpose and has not
    /// ended by `now`. It is gone afterwards either way.
    fn take_email_token(
        &self,
        token_hash: &str,
        purpose: TokenPurpose,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, StoreError>;

    /// Removes every token that ended at or before `now`.
    fn delete_expired_email_tokens(&self, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// Removes accounts nobody confirmed that were made before `before`.
    fn delete_unverified_users_before(&self, before: DateTime<Utc>) -> Result<(), StoreError>;
```

`MemoryStore`: add a field `email_tokens: Mutex<Vec<(String, String, TokenPurpose, DateTime<Utc>)>>`, set the two new `User` fields in `create_user`, and implement:

```rust
    fn mark_email_verified(&self, user_id: &str, at: DateTime<Utc>) -> Result<(), StoreError> {
        let mut users = self.users.lock().expect("users lock");
        if let Some(user) = users.iter_mut().find(|user| user.id == user_id) {
            user.email_verified_at.get_or_insert(at);
        }
        Ok(())
    }

    fn set_password_hash(&self, user_id: &str, password_hash: &str) -> Result<(), StoreError> {
        let mut users = self.users.lock().expect("users lock");
        if let Some(user) = users.iter_mut().find(|user| user.id == user_id) {
            password_hash.clone_into(&mut user.password_hash);
        }
        Ok(())
    }

    fn delete_sessions_of(&self, user_id: &str) -> Result<(), StoreError> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .retain(|_, (owner, _)| owner != user_id);
        Ok(())
    }

    fn put_email_token(
        &self,
        token_hash: &str,
        user_id: &str,
        purpose: TokenPurpose,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut tokens = self.email_tokens.lock().expect("tokens lock");
        tokens.retain(|(_, owner, kind, _)| !(owner == user_id && *kind == purpose));
        tokens.push((token_hash.to_owned(), user_id.to_owned(), purpose, expires_at));
        Ok(())
    }

    fn take_email_token(
        &self,
        token_hash: &str,
        purpose: TokenPurpose,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, StoreError> {
        let mut tokens = self.email_tokens.lock().expect("tokens lock");
        // A token of another purpose is left alone, as in PostgreSQL.
        let Some(at) = tokens
            .iter()
            .position(|(hash, _, kind, _)| hash == token_hash && *kind == purpose)
        else {
            return Ok(None);
        };
        let (_, owner, _, ends) = tokens.remove(at);
        Ok((ends > now).then_some(owner))
    }

    fn delete_expired_email_tokens(&self, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.email_tokens
            .lock()
            .expect("tokens lock")
            .retain(|(_, _, _, ends)| *ends > now);
        Ok(())
    }

    fn delete_unverified_users_before(&self, before: DateTime<Utc>) -> Result<(), StoreError> {
        let mut users = self.users.lock().expect("users lock");
        let gone: Vec<String> = users
            .iter()
            .filter(|user| user.email_verified_at.is_none() && user.created_at < before)
            .map(|user| user.id.clone())
            .collect();
        users.retain(|user| !gone.contains(&user.id));
        self.email_tokens
            .lock()
            .expect("tokens lock")
            .retain(|(_, owner, _, _)| !gone.contains(owner));
        self.sessions
            .lock()
            .expect("sessions lock")
            .retain(|_, (owner, _)| !gone.contains(owner));
        Ok(())
    }
```

- [ ] **Step 5: Implement in `postgres_store.rs`.** Add `("0007_email_verification", include_str!("../migrations/0007_email_verification.sql"))` to the migration list. Every user query returns `id, email, password_hash, email_verified_at, created_at`: change the select list in `user_by_email`, `user`, and the `RETURNING` of `create_user` (which also inserts the two new columns: `INSERT INTO users (email, password_hash, email_verified_at, created_at) VALUES ($1, $2, $3, $4)`), and extend `user_from`:

```rust
fn user_from(row: &postgres::Row) -> User {
    User {
        id: row.get(0),
        email: row.get(1),
        password_hash: row.get(2),
        email_verified_at: row.get(3),
        created_at: row.get(4),
    }
}
```

Then the methods:

```rust
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
        self.run(|client| client.execute("DELETE FROM email_tokens WHERE expires_at <= $1", &[&now]))?;
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
```

Export `TokenPurpose` from `lib.rs`.

- [ ] **Step 6: Run** `cargo test -p ck-hub --test store` with and without `CK_TEST_DATABASE_URL` (see README for the throwaway database). Expected: PASS. Fix compile errors in other callers of `User`/`NewUser` (`accounts.rs::create_user` gets `email_verified_at: Some(now)` in Task 3; for now pass `Some(Utc::now())` and `created_at: Utc::now()` so the crate builds).

- [ ] **Step 7: Commit** `feat: keep email verification state and mailed tokens in the store`

---

### Task 2: The mail interface

**Files:**
- Create: `crates/ck-hub/src/mail.rs`
- Modify: `crates/ck-hub/src/lib.rs`

- [ ] **Step 1: Write the module.** Unit tests for the memory mailer live in the file.

```rust
//! Sending mail. The Hub only needs to hand a message over; a real provider plugs in behind
//! [`Mailer`] later.

use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub body: String,
}

#[derive(Debug)]
pub struct MailError(pub String);

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub trait Mailer: Send + Sync {
    fn send(&self, mail: Mail) -> Result<(), MailError>;
}

/// Keeps every mail, for tests.
#[derive(Default)]
pub struct MemoryMailer {
    sent: Mutex<Vec<Mail>>,
}

impl MemoryMailer {
    #[must_use]
    pub fn sent(&self) -> Vec<Mail> {
        self.sent.lock().expect("mail lock").clone()
    }

    /// The newest mail to this address.
    #[must_use]
    pub fn last_to(&self, to: &str) -> Option<Mail> {
        self.sent().into_iter().rev().find(|mail| mail.to == to)
    }
}

impl Mailer for MemoryMailer {
    fn send(&self, mail: Mail) -> Result<(), MailError> {
        self.sent.lock().expect("mail lock").push(mail);
        Ok(())
    }
}

/// Prints every mail, links included, to standard output. For a developer's machine only:
/// the links in it open accounts.
pub struct ConsoleMailer;

impl Mailer for ConsoleMailer {
    fn send(&self, mail: Mail) -> Result<(), MailError> {
        println!("--- mail to {} ---\n{}\n\n{}\n---", mail.to, mail.subject, mail.body);
        Ok(())
    }
}

/// The token after `marker` in a mail body, for tests (`#verify=`, `#reset=`).
#[must_use]
pub fn token_after(body: &str, marker: &str) -> Option<String> {
    let rest = body.split_once(marker)?.1;
    Some(
        rest.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_mailer_keeps_mail_and_finds_the_newest_for_an_address() {
        let mailer = MemoryMailer::default();
        for subject in ["one", "two"] {
            mailer
                .send(Mail { to: "a@example.org".into(), subject: subject.into(), body: String::new() })
                .expect("send");
        }
        assert_eq!(mailer.sent().len(), 2);
        assert_eq!(mailer.last_to("a@example.org").expect("mail").subject, "two");
        assert!(mailer.last_to("b@example.org").is_none());
    }

    #[test]
    fn a_token_is_read_up_to_the_end_of_the_link() {
        let body = "Open https://hub.example/#verify=abc-123_x and wait.";
        assert_eq!(token_after(body, "#verify=").as_deref(), Some("abc-123_x"));
        assert_eq!(token_after(body, "#reset="), None);
    }
}
```

Check what characters `generate_stick_token` produces (`ck_crypto`) and widen `token_after`'s character set if it uses others.

- [ ] **Step 2: Export** in `lib.rs`: `mod mail;` and `pub use mail::{ConsoleMailer, Mail, MailError, Mailer, MemoryMailer, token_after};`
- [ ] **Step 3: Run** `cargo test -p ck-hub --lib mail` → PASS.
- [ ] **Step 4: Commit** `feat: add a mail interface with console and in-memory mailers`

---

### Task 3: Sign-up logic in the Hub

**Files:**
- Create: `crates/ck-hub/src/signup.rs`, `crates/ck-hub/tests/signup.rs`
- Modify: `crates/ck-hub/src/accounts.rs`, `crates/ck-hub/src/hub.rs`, `crates/ck-hub/src/lib.rs`

- [ ] **Step 1: Write the failing tests** in `tests/signup.rs`. Copy the `Setup` helper shape from `tests/accounts.rs` (a `MemoryStore`, an adjustable clock, `with_password_cost` cheap) and add `.with_mail(Arc::clone(&mailer) as Arc<dyn Mailer>, "https://hub.example")`. Tests (each short; names say what they pin):

```rust
const NEW: &str = "newcomer@example.org";

#[test]
fn registering_sends_a_link_and_the_account_cannot_log_in_until_it_is_used() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    assert_eq!(s.hub.login(NEW, PASSWORD), Err(LoginError::NotVerified));
    let mail = s.mailer.last_to(NEW).expect("a mail");
    assert!(mail.body.contains("https://hub.example/#verify="));
    let token = token_after(&mail.body, "#verify=").expect("token");
    s.hub.verify_email(&token).expect("verified");
    assert!(s.hub.login(NEW, PASSWORD).is_ok());
}

#[test]
fn a_wrong_password_is_still_a_wrong_password_for_an_unverified_account() {
    // NotVerified must only be said after the password matched.
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    assert_eq!(s.hub.login(NEW, "not the password!!"), Err(LoginError::BadCredentials));
}

#[test]
fn a_verification_link_works_once_and_not_after_a_day() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    let token = token_after(&s.mailer.last_to(NEW).expect("mail").body, "#verify=").expect("token");
    s.hub.verify_email(&token).expect("first use");
    assert_eq!(s.hub.verify_email(&token), Err(TokenError::Invalid));

    s.hub.register_account("late@example.org", PASSWORD).expect("registered");
    let late = token_after(&s.mailer.last_to("late@example.org").expect("mail").body, "#verify=").expect("token");
    s.advance(Duration::hours(VERIFY_LIFETIME_HOURS) + Duration::seconds(1));
    assert_eq!(s.hub.verify_email(&late), Err(TokenError::Invalid));
}

#[test]
fn registering_validates_input_and_needs_a_mailer() {
    let s = setup();
    assert_eq!(s.hub.register_account("nope", PASSWORD), Err(SignupError::BadEmail));
    assert_eq!(s.hub.register_account(NEW, "short"), Err(SignupError::PasswordTooShort));
    assert!(s.mailer.sent().is_empty());
    let bare = Hub::new(/* as in setup, without with_mail */);
    assert_eq!(bare.register_account(NEW, PASSWORD), Err(SignupError::MailNotSetUp));
    assert_eq!(bare.request_password_reset(NEW), Err(SignupError::MailNotSetUp));
}

#[test]
fn registering_a_known_address_looks_the_same_and_never_changes_the_account() {
    let s = setup();
    s.hub.create_user("ana@example.org", PASSWORD).expect("ana");
    s.hub.register_account("ana@example.org", "another long password").expect("same answer");
    let mail = s.mailer.last_to("ana@example.org").expect("a mail");
    assert!(!mail.body.contains("#verify="), "a verified account gets a notice, not a link");
    assert!(s.hub.login("ana@example.org", PASSWORD).is_ok(), "the old password still works");
    assert_eq!(
        s.hub.login("ana@example.org", "another long password"),
        Err(LoginError::BadCredentials)
    );
}

#[test]
fn registering_again_before_confirming_sends_a_fresh_link_and_keeps_the_first_password() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("first");
    let first = token_after(&s.mailer.last_to(NEW).expect("mail").body, "#verify=").expect("token");
    s.hub.register_account(NEW, "a different password").expect("second");
    let second = token_after(&s.mailer.last_to(NEW).expect("mail").body, "#verify=").expect("token");
    assert_ne!(first, second);
    assert_eq!(s.hub.verify_email(&first), Err(TokenError::Invalid), "the older link is replaced");
    s.hub.verify_email(&second).expect("verified");
    assert!(s.hub.login(NEW, PASSWORD).is_ok());
}

#[test]
fn mail_is_limited_per_address_and_for_the_whole_hub() {
    let s = setup();
    for _ in 0..MAX_MAILS_PER_ADDRESS + 2 {
        s.hub.register_account(NEW, PASSWORD).expect("always the same answer");
    }
    assert_eq!(s.mailer.sent().len(), MAX_MAILS_PER_ADDRESS as usize);
    s.advance(Duration::seconds(MAIL_WINDOW_SECONDS + 1));
    s.hub.register_account(NEW, PASSWORD).expect("answer");
    assert_eq!(s.mailer.sent().len(), MAX_MAILS_PER_ADDRESS as usize + 1);
}

#[test]
fn a_reset_link_sets_a_new_password_ends_sessions_and_verifies() {
    let s = setup();
    s.hub.create_user("ana@example.org", PASSWORD).expect("ana");
    let (session, _) = s.hub.login("ana@example.org", PASSWORD).expect("login");
    s.hub.request_password_reset("ana@example.org").expect("asked");
    let token = token_after(&s.mailer.last_to("ana@example.org").expect("mail").body, "#reset=").expect("token");
    assert_eq!(s.hub.reset_password(&token, "short"), Err(TokenError::PasswordTooShort));
    s.hub.reset_password(&token, "a brand new password").expect("reset");
    assert_eq!(s.hub.reset_password(&token, "a brand new password"), Err(TokenError::Invalid));
    assert!(s.hub.account(&session).is_err(), "old sessions ended");
    assert!(s.hub.login("ana@example.org", PASSWORD).is_err());
    assert!(s.hub.login("ana@example.org", "a brand new password").is_ok());
}

#[test]
fn a_reset_for_an_unknown_address_sends_nothing_and_says_nothing() {
    let s = setup();
    s.hub.request_password_reset("ghost@example.org").expect("same answer");
    assert!(s.mailer.sent().is_empty());
}

#[test]
fn a_reset_clears_the_login_lockout_and_confirms_an_unverified_account() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    s.hub.request_password_reset(NEW).expect("asked");
    let token = token_after(&s.mailer.last_to(NEW).expect("mail").body, "#reset=").expect("token");
    for _ in 0..MAX_FAILED_LOGINS {
        let _ = s.hub.login(NEW, "wrong wrong wrong");
    }
    assert_eq!(s.hub.login(NEW, PASSWORD), Err(LoginError::TooManyAttempts));
    s.hub.reset_password(&token, "a brand new password").expect("reset");
    assert!(s.hub.login(NEW, "a brand new password").is_ok());
}

#[test]
fn unconfirmed_accounts_are_removed_after_two_days() {
    let s = setup();
    s.hub.register_account(NEW, PASSWORD).expect("registered");
    s.advance(Duration::hours(49));
    s.hub.register_account("someone@example.org", PASSWORD).expect("purges as it goes");
    s.hub.register_account(NEW, PASSWORD).expect("the address is free again");
    assert_eq!(s.hub.login(NEW, PASSWORD), Err(LoginError::NotVerified));
}
```

Task 1 left `create_user` passing `Utc::now()`; the last test needs the Hub clock, so `accounts::create_user` takes `now` (next step).

- [ ] **Step 2: Run to see them fail.** `cargo test -p ck-hub --test signup` → compile errors.

- [ ] **Step 3: Change `accounts.rs`.**
  - `LoginError` gets `NotVerified` ("the password was right but the email was never confirmed").
  - In `login`, after `let Some(user) = user.filter(|_| matches) else {...};` and the `clear(key)`, add `if user.email_verified_at.is_none() { return Err(LoginError::NotVerified); }` before sessions are touched.
  - Make `hash`, `token_hash`, `unavailable`, `valid_email` and `Accounts::clear` `pub(crate)`.
  - Free `create_user(store, cost, email, password)` gains `now: DateTime<Utc>` and creates verified accounts: `email_verified_at: Some(now), created_at: now`. `Hub::create_user` passes `(self.clock)()`; `main.rs` passes `Utc::now()`. Fix callers the compiler lists.
  - Add to `impl Accounts`:

```rust
    /// Whether one more mail may go to `email`. Counts the mail when it may.
    pub(crate) fn mail_allowed(&self, email: &str, now: DateTime<Utc>) -> bool {
        let key = format!("mail:{}", email.to_lowercase());
        if self.blocked(&key, now, MAX_MAILS_PER_ADDRESS, MAIL_WINDOW_SECONDS)
            || self.blocked("mail:*", now, MAX_MAILS_ALL, MAIL_ALL_WINDOW_SECONDS)
        {
            return false;
        }
        self.fail(&key, now, MAIL_WINDOW_SECONDS);
        self.fail("mail:*", now, MAIL_ALL_WINDOW_SECONDS);
        true
    }
```

  with the constants `pub const MAX_MAILS_PER_ADDRESS: u32 = 3; pub const MAIL_WINDOW_SECONDS: i64 = 300; pub const MAX_MAILS_ALL: u32 = 120; pub const MAIL_ALL_WINDOW_SECONDS: i64 = 60;`. (`fail` is named for logins but is just a windowed counter; its cleanup drops entries after 300 seconds, which covers both windows.) Note `blocked`'s `count >= max` means "max mails allowed" is exact.

- [ ] **Step 4: `hub.rs`.** Add `mailer: Option<(Arc<dyn Mailer>, String)>` (mailer and public base URL, no trailing slash) set to `None` in `new`, and:

```rust
    /// Lets the Hub send mail. `public_url` is the address people reach the Hub at, as the links
    /// in the mail start with it.
    #[must_use]
    pub fn with_mail(mut self, mailer: Arc<dyn Mailer>, public_url: &str) -> Self {
        self.mailer = Some((mailer, public_url.trim_end_matches('/').to_owned()));
        self
    }
```

- [ ] **Step 5: Write `signup.rs`.**

```rust
//! Sign-up, email confirmation and password reset.

use chrono::Duration;

use crate::accounts::{MIN_PASSWORD_LENGTH, hash, token_hash, unavailable, valid_email};
use crate::hub::Hub;
use crate::mail::Mail;
use crate::store::{NewUser, StoreError, TokenPurpose, User};

pub const VERIFY_LIFETIME_HOURS: i64 = 24;
pub const RESET_LIFETIME_MINUTES: i64 = 60;
/// Accounts nobody confirmed are removed after this long.
pub const UNVERIFIED_LIFETIME_HOURS: i64 = 48;

#[derive(Debug, PartialEq, Eq)]
pub enum SignupError {
    BadEmail,
    PasswordTooShort,
    /// The Hub was started without a way to send mail.
    MailNotSetUp,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    /// Unknown, used, ended, or made for something else. The reasons look the same.
    Invalid,
    PasswordTooShort,
    Unavailable,
}

fn store_failed(error: &StoreError) {
    unavailable(error);
}

impl Hub {
    /// Makes an unverified account and mails its link. The answer is the same for an address that
    /// already has an account, so nobody can use this to find out who has one.
    pub fn register_account(&self, email: &str, password: &str) -> Result<(), SignupError> {
        if self.mailer.is_none() {
            return Err(SignupError::MailNotSetUp);
        }
        if !valid_email(email) {
            return Err(SignupError::BadEmail);
        }
        if password.chars().count() < MIN_PASSWORD_LENGTH {
            return Err(SignupError::PasswordTooShort);
        }
        let now = (self.clock)();
        let failed = |error: StoreError| {
            store_failed(&error);
            SignupError::Unavailable
        };
        self.store
            .delete_expired_email_tokens(now)
            .map_err(failed)?;
        self.store
            .delete_unverified_users_before(now - Duration::hours(UNVERIFIED_LIFETIME_HOURS))
            .map_err(failed)?;
        // Hashed even when the address is taken, so both answers take as long.
        let password_hash = hash(self.accounts.cost, password).map_err(|error| {
            tracing::error!("cannot hash a password: {error}");
            SignupError::Unavailable
        })?;
        let created = self
            .store
            .create_user(NewUser {
                email: email.to_owned(),
                password_hash,
                email_verified_at: None,
                created_at: now,
            })
            .map_err(failed)?;
        match created {
            Some(user) => self.mail_verification(&user, now),
            None => {
                let Some(user) = self.store.user_by_email(email).map_err(failed)? else {
                    return Ok(());
                };
                if user.email_verified_at.is_none() {
                    self.mail_verification(&user, now)
                } else {
                    self.mail_notice(&user, now);
                    Ok(())
                }
            }
        }
    }

    /// Confirms the account the link was made for.
    pub fn verify_email(&self, token: &str) -> Result<(), TokenError> {
        let now = (self.clock)();
        let failed = |error: StoreError| {
            store_failed(&error);
            TokenError::Unavailable
        };
        let user_id = self
            .store
            .take_email_token(&token_hash(token), TokenPurpose::Verify, now)
            .map_err(failed)?
            .ok_or(TokenError::Invalid)?;
        self.store.mark_email_verified(&user_id, now).map_err(failed)
    }

    /// Mails a reset link if the address has an account. Says nothing either way.
    pub fn request_password_reset(&self, email: &str) -> Result<(), SignupError> {
        if self.mailer.is_none() {
            return Err(SignupError::MailNotSetUp);
        }
        let now = (self.clock)();
        let user = self.store.user_by_email(email).map_err(|error| {
            store_failed(&error);
            SignupError::Unavailable
        })?;
        if let Some(user) = user {
            self.mail_token(
                &user,
                TokenPurpose::Reset,
                Duration::minutes(RESET_LIFETIME_MINUTES),
                now,
                "Reset your Keeper Hub password",
                "Open this link to choose a new password. It works once, for an hour. If you did \
                 not ask for it, ignore this mail: nothing changes.",
                "reset",
            )?;
        }
        Ok(())
    }

    /// Sets a new password with a reset link. Ends every session of the account, clears its
    /// login lockout, and counts the address as confirmed.
    pub fn reset_password(&self, token: &str, password: &str) -> Result<(), TokenError> {
        // Checked first, so a mistyped short password does not use the link up.
        if password.chars().count() < MIN_PASSWORD_LENGTH {
            return Err(TokenError::PasswordTooShort);
        }
        let now = (self.clock)();
        let failed = |error: StoreError| {
            store_failed(&error);
            TokenError::Unavailable
        };
        let user_id = self
            .store
            .take_email_token(&token_hash(token), TokenPurpose::Reset, now)
            .map_err(failed)?
            .ok_or(TokenError::Invalid)?;
        let password_hash = hash(self.accounts.cost, password).map_err(|error| {
            tracing::error!("cannot hash a password: {error}");
            TokenError::Unavailable
        })?;
        self.store
            .set_password_hash(&user_id, &password_hash)
            .map_err(failed)?;
        self.store.delete_sessions_of(&user_id).map_err(failed)?;
        self.store.mark_email_verified(&user_id, now).map_err(failed)?;
        if let Some(user) = self.store.user(&user_id).map_err(failed)? {
            self.accounts.clear(&user.email.to_lowercase());
        }
        Ok(())
    }

    fn mail_verification(&self, user: &User, now: chrono::DateTime<chrono::Utc>) -> Result<(), SignupError> {
        self.mail_token(
            user,
            TokenPurpose::Verify,
            Duration::hours(VERIFY_LIFETIME_HOURS),
            now,
            "Confirm your Keeper Hub email",
            "Open this link to confirm your email. It works once, for a day.",
            "verify",
        )
    }

    /// For someone who tried to sign up with an address that already has an account.
    fn mail_notice(&self, user: &User, now: chrono::DateTime<chrono::Utc>) {
        if !self.accounts.mail_allowed(&user.email, now) {
            return;
        }
        let base = self.mailer.as_ref().map_or("", |(_, base)| base.as_str());
        self.send(Mail {
            to: user.email.clone(),
            subject: "You already have a Keeper Hub account".to_owned(),
            body: format!(
                "Someone, maybe you, tried to sign up with this address. You already have an \
                 account: log in at {base}/. If you forgot the password, use \"Forgot password\" \
                 there. If it was not you, ignore this mail."
            ),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn mail_token(
        &self,
        user: &User,
        purpose: TokenPurpose,
        lifetime: Duration,
        now: chrono::DateTime<chrono::Utc>,
        subject: &str,
        text: &str,
        marker: &str,
    ) -> Result<(), SignupError> {
        // Over the limit nothing is sent and nothing says so.
        if !self.accounts.mail_allowed(&user.email, now) {
            return Ok(());
        }
        let token = ck_crypto::generate_stick_token();
        let token = token.expose();
        self.store
            .put_email_token(&token_hash(token), &user.id, purpose, now + lifetime)
            .map_err(|error| {
                store_failed(&error);
                SignupError::Unavailable
            })?;
        let base = self.mailer.as_ref().map_or("", |(_, base)| base.as_str());
        self.send(Mail {
            to: user.email.clone(),
            subject: subject.to_owned(),
            body: format!("{text}\n\n{base}/#{marker}={token}\n"),
        });
        Ok(())
    }

    /// A mail that cannot be sent is logged, not reported to the caller: the answer must not
    /// depend on it.
    fn send(&self, mail: Mail) {
        if let Some((mailer, _)) = &self.mailer {
            if let Err(error) = mailer.send(mail) {
                tracing::error!("cannot send mail: {error}");
            }
        }
    }
}
```

Replace the `chrono::DateTime<chrono::Utc>` paths with an import if rustfmt/clippy prefers. `too_many_arguments` is allowed once here because the two callers differ only in these strings; if clippy objects to the `#[allow]`, fold `subject`, `text`, `marker` into a small `struct Kind`.

- [ ] **Step 6: Export** from `lib.rs`: `mod signup;` and `pub use signup::{RESET_LIFETIME_MINUTES, SignupError, TokenError, UNVERIFIED_LIFETIME_HOURS, VERIFY_LIFETIME_HOURS};` plus the four mail-limit constants from `accounts`.
- [ ] **Step 7: Run** `cargo test -p ck-hub` → all PASS (the existing suites included: nothing in them registers, and every account they make is verified).
- [ ] **Step 8: Commit** `feat: sign up, confirm email and reset password in the hub`

---

### Task 4: HTTP routes

**Files:**
- Modify: `crates/ck-hub/src/routes.rs`
- Test: `crates/ck-hub/tests/signup.rs` (HTTP tests at the bottom, `oneshot` style as in `tests/accounts.rs`)

- [ ] **Step 1: Write the failing HTTP tests.** Using the `post_json(app, path, body)` helper pattern from `tests/accounts.rs` (copy it):
  - `POST /api/v1/register` with a good body → `202`, a mail exists; bad email → `400`; short password → `400` with the minimum in the text; a Hub without mail → `503`.
  - `POST /api/v1/login` for the unverified account → `403` with `"error":"confirm your email first"`; no `Set-Cookie`.
  - `POST /api/v1/verify-email` `{"token":...}` → `204`, then the same token → `400`.
  - `POST /api/v1/password-reset` `{"email":...}` → `202` for a known and an unknown address with identical bodies.
  - `POST /api/v1/password-reset/confirm` `{"token","password"}` → `204`; short password → `400` and the token still works afterwards.
- [ ] **Step 2: Run to see them fail** (404s).
- [ ] **Step 3: Implement.** Routes:

```rust
        .route("/api/v1/register", post(sign_up))
        .route("/api/v1/verify-email", post(verify_email))
        .route("/api/v1/password-reset", post(password_reset))
        .route("/api/v1/password-reset/confirm", post(password_reset_confirm))
```

Handlers follow `login`: `HASHING` permit for the two that hash (`sign_up`, `password_reset_confirm`), `spawn_blocking`, JSON bodies via `#[derive(Deserialize)]` structs (`RegisterBody { email, password }`, `TokenBody { token }`, `EmailBody { email }`, `ResetBody { token, password }`). Answers:

| Call | Result | Status and body |
|---|---|---|
| `register_account` | `Ok` | `202 {"status":"check your email"}` |
| | `BadEmail` | `400 {"error":"that is not an email address"}` |
| | `PasswordTooShort` | `400 {"error":"the password needs at least 12 characters"}` (use `MIN_PASSWORD_LENGTH`) |
| | `MailNotSetUp` | `503 {"error":"this Hub cannot send email, so sign-up is closed"}` |
| `verify_email` | `Ok` | `204` |
| | `Invalid` | `400 {"error":"this link is not valid any more"}` |
| `request_password_reset` | `Ok` | `202 {"status":"if that address has an account, a link is on its way"}` |
| | `MailNotSetUp` | as above |
| `reset_password` | `Ok` | `204` |
| | `Invalid` / `PasswordTooShort` | `400`, texts as above |

Any `Unavailable` or a failed task → the existing `unavailable()` response. Extend the `login` match with `LoginError::NotVerified => 403 {"error":"confirm your email first: open the link we mailed you"}`.

Make the shared parts small helpers (`fn bad_request(text: &str) -> Response`, `fn no_mail() -> Response`) rather than repeating `json!` literals.

- [ ] **Step 4: Run** `cargo test -p ck-hub --test signup` → PASS, then `cargo test -p ck-hub`.
- [ ] **Step 5: Commit** `feat: add register, verify-email and password-reset routes`

---

### Task 5: The page

**Files:**
- Modify: `crates/ck-hub/src/page.html`
- Test: `crates/ck-hub/tests/api.rs` (the existing page test asserts strings; add `/api/v1/register` and `#verify=` / `#reset=` handling strings)

The `#login` form is reused for four modes, so the existing styling applies unchanged.

- [ ] **Step 1: Markup.** In the `#login` form add, under the submit button:

```html
    <div id="login-links"><a href="#" id="to-register">Create an account</a> · <a href="#" id="to-forgot">Forgot password</a><a href="#" id="to-login" hidden>Back to log in</a></div>
```

and give `<h2>`, the `.intro` paragraph, the email field's wrapping `<div>`, the password label and the submit button ids (`login-title`, `login-intro`, `email-row`, `pass-label`, `login-submit`). Style `#login-links` like `.intro` (centered, `font-size: 14px`, links in the accent colour already used by the page) in the existing CSS block, next to the other `#login` rules.

- [ ] **Step 2: Script.** Replace the `login` submit handler with a mode-driven one:

```js
const MODES = {
  login: { title: "Log in to Keeper Hub", intro: "See which contest machines are online and which need attention.", email: true, pass: "Password", autocomplete: "current-password", button: "Log in", path: "/api/v1/login" },
  register: { title: "Create an account", intro: "We mail you a link to confirm your address. After that, an owner of your organization adds you to it.", email: true, pass: "Choose a password (12 characters or more)", autocomplete: "new-password", button: "Create account", path: "/api/v1/register", done: "Check your email for the link." },
  forgot: { title: "Reset your password", intro: "Enter your email and we mail you a link.", email: true, pass: null, button: "Send link", path: "/api/v1/password-reset", done: "If that address has an account, a link is on its way." },
  reset: { title: "Choose a new password", intro: "At least 12 characters.", email: false, pass: "New password", autocomplete: "new-password", button: "Change password", path: "/api/v1/password-reset/confirm", done: "Password changed. Log in with it." },
};
let mode = "login";
let resetToken = null;

function setMode(next) {
  mode = next;
  const m = MODES[next];
  document.getElementById("login-title").textContent = m.title;
  document.getElementById("login-intro").textContent = m.intro;
  document.getElementById("email-row").hidden = !m.email;
  document.getElementById("user").required = m.email;
  const passRow = document.getElementById("pass").parentElement;
  passRow.hidden = m.pass === null;
  document.getElementById("pass").required = m.pass !== null;
  if (m.pass) {
    document.getElementById("pass-label").textContent = m.pass;
    document.getElementById("pass").autocomplete = m.autocomplete;
  }
  document.getElementById("login-submit").textContent = m.button;
  document.getElementById("to-register").hidden = next !== "login";
  document.getElementById("to-forgot").hidden = next !== "login";
  document.getElementById("to-login").hidden = next === "login";
  document.getElementById("login-error").textContent = "";
}

for (const [id, target] of [["to-register", "register"], ["to-forgot", "forgot"], ["to-login", "login"]]) {
  document.getElementById(id).addEventListener("click", (event) => { event.preventDefault(); setMode(target); });
}

document.getElementById("login").addEventListener("submit", async (event) => {
  event.preventDefault();
  const m = MODES[mode];
  const message = document.getElementById("login-error");
  message.textContent = "";
  const email = document.getElementById("user").value;
  const password = document.getElementById("pass").value;
  const body = mode === "forgot" ? { email }
    : mode === "reset" ? { token: resetToken, password }
    : { email, password };
  let response;
  try {
    response = await fetch(m.path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  } catch (error) {
    message.textContent = "Could not reach the Hub.";
    return;
  }
  document.getElementById("pass").value = "";
  if (mode === "login" && response.ok) { await refresh(); return; }
  if (response.ok) {
    const done = m.done;
    if (mode === "reset") { resetToken = null; setMode("login"); }
    message.textContent = done;
    return;
  }
  try { message.textContent = (await response.json()).error; }
  catch (error) { message.textContent = "That did not work."; }
});
```

Then, before the final `refresh();`, handle a link that was opened (the token is in the fragment, so it is not in any server log, and a mail scanner that fetches the page does not use it up):

```js
async function openedFromMail() {
  const [kind, token] = location.hash.slice(1).split("=");
  if (kind !== "verify" && kind !== "reset") return;
  history.replaceState(null, "", location.pathname);
  if (kind === "reset") { resetToken = token; setMode("reset"); return; }
  let text = "That link is not valid any more.";
  try {
    const response = await fetch("/api/v1/verify-email", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ token }) });
    if (response.ok) text = "Email confirmed. You can log in now.";
  } catch (error) { text = "Could not reach the Hub."; }
  document.getElementById("login-error").textContent = text;
}
openedFromMail().then(refresh);
```

(replace the bare `refresh();` call with the line above). `refresh()` shows the login form on `401`, which is where the messages appear; `setMode("reset")` must survive it, so `showLogin` stays as it is and does not reset the mode.

- [ ] **Step 2b: Keep the two forms of `refresh` from wiping a message.** `refresh()` runs every `REFRESH_MS`; it must not clear `#login-error`. It does not touch it today, so nothing to change; confirm in the browser (Step 4).
- [ ] **Step 3: Update the page assertions** in `tests/api.rs` and run `cargo test -p ck-hub --test api`.
- [ ] **Step 4: Check it in a browser.** Start `cargo run -p ck-hub -- --listen 127.0.0.1:18088 --mail console --public-url http://127.0.0.1:18088` (flags arrive in Task 6, so do this step after it), register, copy the link from the terminal output, open it, log in (this account has no organization: the page must say so, see its existing message), then "Forgot password" and the reset link. Take screenshots of each mode in light and dark and in the classic theme.
- [ ] **Step 5: Commit** `feat: add sign-up, confirmation and reset to the login page`

---

### Task 6: Starting the Hub, docs, checks

**Files:**
- Modify: `crates/ck-hub/src/main.rs`, `README.md`

- [ ] **Step 1: Flags** in `ServeArgs`:

```rust
    /// How the Hub sends mail. `console` prints it, links included, and is for a developer's
    /// machine only. Without this, sign-up and password reset answer 503
    #[arg(long, env = "CK_MAIL", value_enum, requires = "public_url")]
    mail: Option<MailKind>,
    /// The address people reach this Hub at, for the links in mail, such as
    /// https://hub.contestkeeper.com
    #[arg(long, env = "CK_PUBLIC_URL")]
    public_url: Option<String>,
```

with `#[derive(Clone, clap::ValueEnum)] enum MailKind { Console }` (the real provider adds a variant). In `run`, after the download block: `if let (Some(MailKind::Console), Some(url)) = (&args.mail, &args.public_url) { tracing::warn!("mail goes to the console: do not use this on a shared Hub"); hub = hub.with_mail(Arc::new(ConsoleMailer), url); }`. Also change `create_user(...)` calls for the new `now` argument.

- [ ] **Step 2: README.** Add to the routes table the four routes; add a paragraph to "Accounts and organizations": sign-up makes an account only, an owner or staff must add it to an organization (`ck-hub add-member`), `--mail console` for local runs, a `503` without mail, the limits, the 48-hour purge, and that links carry the token after `#`.
- [ ] **Step 3: Checks.** `cargo fmt --all`; `cargo clippy --all-targets -- -D warnings` (the local Rust is 1.93 and the pin is 1.99.1: add `-A clippy::doc_markdown` locally and say so in the PR); `cargo test` on memory; `cargo test` with `CK_TEST_DATABASE_URL` set to a throwaway Postgres (fresh database, then the legacy-schema path: a database that already ran 0006 with users in it must get `email_verified_at` filled by 0007, check with `SELECT count(*) FROM users WHERE email_verified_at IS NULL` → 0).
- [ ] **Step 4: Mutation check** that the key test is not vacuous: make `login` skip the `NotVerified` check and confirm `registering_sends_a_link_and_the_account_cannot_log_in_until_it_is_used` fails; restore it.
- [ ] **Step 5: Commit** `docs: describe sign-up and password reset`, push `feature/registration-and-verification`, open a PR into `develop` titled `feat: sign up, confirm email and reset password`.

## Self-review against the spec

- Register, verification, login on email, lockout keyed on email: Tasks 3, 4. Lockout stays as it was.
- Password reset with a short-lived token stored hashed: Tasks 1, 3.
- Mail interface with a real provider in the cloud and console output locally: the interface and console and memory mailers are here; **the real provider is not**, pending the open item in the spec.
- "No login until verified": Task 3 (`NotVerified`).
- Out of this plan and left to plans 3 and 4: Google/Microsoft sign-in, `identities`, nullable `password_hash`, invitations, staff routes, `make-staff`.
