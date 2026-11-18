//! Sign-up, email confirmation and password reset.

use time::{Duration, OffsetDateTime};

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

/// What differs between the two mails that carry a token.
struct Letter {
    purpose: TokenPurpose,
    lifetime: Duration,
    subject: &'static str,
    text: &'static str,
    /// The word before `=` in the link: the page reads it from the address.
    marker: &'static str,
}

const VERIFY: Letter = Letter {
    purpose: TokenPurpose::Verify,
    lifetime: Duration::hours(VERIFY_LIFETIME_HOURS),
    subject: "Confirm your Keeper Hub email",
    text: "Open this link to confirm your email. It works once, for a day.",
    marker: "verify",
};

const RESET: Letter = Letter {
    purpose: TokenPurpose::Reset,
    lifetime: Duration::minutes(RESET_LIFETIME_MINUTES),
    subject: "Reset your Keeper Hub password",
    text: "Open this link to choose a new password. It works once, for an hour. If you did not \
           ask for it, ignore this mail: nothing changes.",
    marker: "reset",
};

fn store_failed(error: &StoreError) {
    unavailable(error);
}

impl Hub {
    /// Makes an unverified account and mails its link. The answer is the same for an address that
    /// already has an account, so nobody can use this to find out who has one.
    ///
    /// # Errors
    /// For a bad email or password, a Hub that cannot send mail, or a store that failed.
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
        let user = match created {
            Some(user) => user,
            None => match self.store.user_by_email(email).map_err(failed)? {
                Some(user) => user,
                None => return Ok(()),
            },
        };
        if user.email_verified_at.is_none() {
            self.mail_token(&user, &VERIFY, now)
        } else {
            self.mail_notice(&user, now);
            Ok(())
        }
    }

    /// Confirms the account the link was made for.
    ///
    /// # Errors
    /// For a link that is not valid, or a store that failed.
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
        self.store
            .mark_email_verified(&user_id, now)
            .map_err(failed)
    }

    /// Mails a reset link if the address has an account. Says nothing either way.
    ///
    /// # Errors
    /// For a Hub that cannot send mail, or a store that failed.
    pub fn request_password_reset(&self, email: &str) -> Result<(), SignupError> {
        if self.mailer.is_none() {
            return Err(SignupError::MailNotSetUp);
        }
        let user = self.store.user_by_email(email).map_err(|error| {
            store_failed(&error);
            SignupError::Unavailable
        })?;
        match user {
            Some(user) => self.mail_token(&user, &RESET, (self.clock)()),
            None => Ok(()),
        }
    }

    /// Sets a new password with a reset link. Ends every session of the account, clears its login
    /// lockout, and counts the address as confirmed.
    ///
    /// # Errors
    /// For a password that is too short, a link that is not valid, or a store that failed.
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
        self.store
            .mark_email_verified(&user_id, now)
            .map_err(failed)?;
        if let Some(user) = self.store.user(&user_id).map_err(failed)? {
            self.accounts.clear(&user.email.to_lowercase());
        }
        Ok(())
    }

    pub(crate) fn base_url(&self) -> &str {
        self.mailer.as_ref().map_or("", |(_, base)| base.as_str())
    }

    /// For someone who tried to sign up with an address that already has an account.
    fn mail_notice(&self, user: &User, now: OffsetDateTime) {
        if !self.accounts.mail_allowed(&user.email, now) {
            return;
        }
        self.send(Mail {
            to: user.email.clone(),
            subject: "You already have a Keeper Hub account".to_owned(),
            body: format!(
                "Someone, maybe you, tried to sign up with this address. You already have an \
                 account: log in at {}/. If you forgot the password, use \"Forgot password\" \
                 there. If it was not you, ignore this mail.",
                self.base_url()
            ),
        });
    }

    fn mail_token(
        &self,
        user: &User,
        letter: &Letter,
        now: OffsetDateTime,
    ) -> Result<(), SignupError> {
        // Over the limit nothing is sent and nothing says so.
        if !self.accounts.mail_allowed(&user.email, now) {
            return Ok(());
        }
        let token = ck_crypto::generate_stick_token();
        let token = token.expose();
        self.store
            .put_email_token(
                &token_hash(token),
                &user.id,
                letter.purpose,
                now + letter.lifetime,
            )
            .map_err(|error| {
                store_failed(&error);
                SignupError::Unavailable
            })?;
        self.send(Mail {
            to: user.email.clone(),
            subject: letter.subject.to_owned(),
            body: format!(
                "{}\n\n{}/#{}={token}\n",
                letter.text,
                self.base_url(),
                letter.marker
            ),
        });
        Ok(())
    }

    /// A mail that cannot be sent is logged, not reported to the caller: the answer must not
    /// depend on it.
    pub(crate) fn send(&self, mail: Mail) {
        if let Some((mailer, _)) = &self.mailer
            && let Err(error) = mailer.send(mail)
        {
            tracing::error!("cannot send mail: {error}");
        }
    }
}
