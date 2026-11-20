//! Signing the settings of a stick. The settings file on a stick can be edited by anyone who holds
//! the stick, so Forge sends the exact bytes it is about to write and the Hub signs them with a key
//! that never leaves the Hub. The image carries the public key; the stick refuses a file whose
//! signature does not match.
//!
//! The Hub signs only for an admin, only a settings file of a stick that is not another
//! organization's, and keeps a line saying who signed which stick and which hash. It never keeps
//! the file.

use ck_crypto::{ConfigSignature, ConfigSigningKey, ConfigVerifyingKey};
use ck_protocol::{ENROLMENT_SCHEMA, EnrolmentConfig, SCHEMA, StickConfig, StickId};
use serde::Deserialize;

use crate::accounts::Account;
use crate::enrol::{EnrolError, window_count};
use crate::hub::Hub;
use crate::store::StoreError;

/// The agent reads at most this much of a settings file, so signing more is useless.
pub const MAX_SIGNED_SETTINGS_BYTES: usize = 1024 * 1024;

/// Settings one person may have signed in a minute: a batch of sticks, not a flood.
pub const MAX_SIGNATURES_PER_MINUTE: u32 = 120;

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    /// The Hub was started without `CK_CONFIG_SIGNING_KEY`.
    #[error("this Hub has no settings signing key")]
    NoKey,
    #[error("{0}")]
    BadRequest(String),
    #[error("too many requests in a minute")]
    RateLimited,
    /// The stick id is another organization's, or the stick was deleted.
    #[error("this stick id is not available to this organization")]
    NotYours,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The stick a settings file is for, read from a file of a version the Hub knows.
fn stick_of(bytes: &[u8]) -> Result<StickId, SignError> {
    #[derive(Deserialize)]
    struct Head {
        schema: u32,
    }
    let unreadable = |_| SignError::BadRequest("the settings file cannot be read".to_owned());
    let head: Head = serde_json::from_slice(bytes).map_err(unreadable)?;
    match head.schema {
        ENROLMENT_SCHEMA => serde_json::from_slice::<EnrolmentConfig>(bytes)
            .map(|config| config.stick_id)
            .map_err(unreadable),
        SCHEMA => serde_json::from_slice::<StickConfig>(bytes)
            .map(|config| config.stick_id)
            .map_err(unreadable),
        other => Err(SignError::BadRequest(format!(
            "settings of version {other} are not signed"
        ))),
    }
}

impl Hub {
    /// Lets the Hub sign the settings of sticks.
    #[must_use]
    pub fn with_config_signing_key(mut self, key: ConfigSigningKey) -> Self {
        self.config_key = Some(key);
        self
    }

    /// The public key whose signatures the images trust, or nothing when the Hub cannot sign.
    #[must_use]
    pub fn config_signing_public_key(&self) -> Option<ConfigVerifyingKey> {
        self.config_key
            .as_ref()
            .map(ConfigSigningKey::verifying_key)
    }

    /// Signs the exact bytes of a settings file for a stick of `organization`.
    ///
    /// # Errors
    /// [`SignError::NoKey`] without a signing key, [`SignError::BadRequest`] for a file that is not
    /// stick settings, [`SignError::RateLimited`] past the limit and [`SignError::NotYours`] for the
    /// id of a stick of another organization or a deleted one.
    pub fn sign_stick_settings(
        &self,
        organization: &str,
        signer: &Account,
        bytes: &[u8],
    ) -> Result<ConfigSignature, SignError> {
        let key = self.config_key.as_ref().ok_or(SignError::NoKey)?;
        if bytes.len() > MAX_SIGNED_SETTINGS_BYTES {
            return Err(SignError::BadRequest(
                "the settings file is too large".to_owned(),
            ));
        }
        let stick = stick_of(bytes)?;
        let now = (self.clock)();
        window_count(&self.sign_rates, &signer.id, now, MAX_SIGNATURES_PER_MINUTE).map_err(
            |error| match error {
                EnrolError::RateLimited => SignError::RateLimited,
                _ => SignError::BadRequest(error.to_string()),
            },
        )?;
        if let Some((owner, known)) = self.store.stick(&stick)?
            && (owner != organization || known.deleted_at.is_some())
        {
            return Err(SignError::NotYours);
        }
        let signature = key.sign(bytes);
        tracing::info!(
            organization,
            stick = %stick,
            user = %signer.id,
            sha256 = %ck_crypto::sha256_hex(bytes),
            key = signature.key_id(),
            "stick settings signed"
        );
        Ok(signature)
    }
}
