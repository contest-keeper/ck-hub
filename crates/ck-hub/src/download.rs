//! Download links that hide where a file is kept. The installer sits in a private Azure Storage
//! container. A visitor asks the Hub for `/download/forge` and is sent to the file with a read-only
//! link that stops working after a few minutes, so the storage address itself is never public.

use std::fmt::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The storage REST version the link is signed for.
const SAS_VERSION: &str = "2022-11-02";

/// How long a link works. Long enough for a slow download to start, short enough that a shared link
/// is useless soon after.
pub const LINK_LIFETIME_MINUTES: i64 = 5;

/// A private storage container the Hub may hand out files from.
#[derive(Clone)]
pub struct Download {
    account: String,
    container: String,
    key: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("the storage account name must be 3 to 24 lowercase letters or digits")]
    Account,
    #[error("the container name must be lowercase letters, digits and dashes")]
    Container,
    #[error("the storage key is not valid base64")]
    Key,
}

impl Download {
    pub fn new(account: &str, container: &str, key_base64: &str) -> Result<Self, DownloadError> {
        let account_ok = (3..=24).contains(&account.len())
            && account
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if !account_ok {
            return Err(DownloadError::Account);
        }
        let container_ok = !container.is_empty()
            && container
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !container_ok {
            return Err(DownloadError::Container);
        }
        let key = STANDARD
            .decode(key_base64.trim())
            .map_err(|_| DownloadError::Key)?;
        Ok(Self {
            account: account.to_owned(),
            container: container.to_owned(),
            key,
        })
    }

    /// A read-only link to `blob` (a path inside the container) that works for
    /// [`LINK_LIFETIME_MINUTES`] from `now`.
    #[must_use]
    pub fn link(&self, blob: &str, now: DateTime<Utc>) -> String {
        let expiry = (now + Duration::minutes(LINK_LIFETIME_MINUTES))
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        let resource = format!("/blob/{}/{}/{}", self.account, self.container, blob);
        // The fields in the order Azure signs them: permissions, start, expiry, resource,
        // identifier, IP range, protocol, version, resource type, snapshot time, encryption scope
        // and the five response header overrides. Only the ones used are filled in.
        let to_sign = [
            "r",
            "",
            &expiry,
            &resource,
            "",
            "",
            "https",
            SAS_VERSION,
            "b",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]
        .join("\n");
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC takes any key length");
        mac.update(to_sign.as_bytes());
        let signature = STANDARD.encode(mac.finalize().into_bytes());
        format!(
            "https://{}.blob.core.windows.net/{}/{}?sv={SAS_VERSION}&sp=r&sr=b&se={}&spr=https&sig={}",
            self.account,
            self.container,
            blob,
            encode(&expiry),
            encode(&signature),
        )
    }
}

/// Percent-encodes what a query value may not hold as it is. The signature and the expiry only
/// contain letters, digits and `+/=:-`, so that is all this needs to handle.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a valid time")
    }

    #[test]
    fn a_link_names_the_file_and_expires_in_minutes() {
        let download =
            Download::new("ckdlabc12", "downloads", "a2V5LWZvci10ZXN0cw==").expect("valid");
        let link = download.link("forge/setup.exe", at("2026-11-14T12:00:00Z"));
        assert!(link.starts_with(
            "https://ckdlabc12.blob.core.windows.net/downloads/forge/setup.exe?sv=2022-11-02&sp=r&sr=b&se=2026-11-14T12%3A05%3A00Z&spr=https&sig="
        ));
    }

    #[test]
    fn the_signature_is_the_same_for_the_same_input_and_changes_with_the_file() {
        let download =
            Download::new("ckdlabc12", "downloads", "a2V5LWZvci10ZXN0cw==").expect("valid");
        let now = at("2026-11-14T12:00:00Z");
        assert_eq!(
            download.link("forge/a.exe", now),
            download.link("forge/a.exe", now)
        );
        assert_ne!(
            download.link("forge/a.exe", now),
            download.link("forge/b.exe", now)
        );
    }

    #[test]
    fn a_known_signature_matches_the_one_made_with_another_tool() {
        // Made with: python3 hmac-sha256 over the same 16 newline-joined fields.
        let download =
            Download::new("ckdlabc12", "downloads", "a2V5LWZvci10ZXN0cw==").expect("valid");
        let link = download.link("forge/setup.exe", at("2026-11-14T12:00:00Z"));
        assert!(
            link.ends_with("&sig=LJkscj3n8xulIt%2FQnl088ZPKFvLVm%2FTmL0XeLv8gX2M%3D"),
            "{link}"
        );
    }

    #[test]
    fn bad_settings_are_refused() {
        assert!(Download::new("UP", "downloads", "a2V5").is_err());
        assert!(Download::new("ckdlabc12", "Bad Name", "a2V5").is_err());
        assert!(Download::new("ckdlabc12", "downloads", "not base64!").is_err());
    }
}
