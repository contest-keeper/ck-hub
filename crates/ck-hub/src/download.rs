//! Download links that hide where a file is kept. The installer sits in a private Azure Storage
//! container. A visitor asks the Hub for `/download/forge` and is sent to the file with a read-only
//! link that stops working after a few minutes, so the storage address itself is never public.

use std::fmt::Write;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::delegation::Delegation;

/// The storage REST version the link is signed for.
const SAS_VERSION: &str = crate::delegation::STORAGE_VERSION;

/// How long a link works. Long enough for a slow download to start, short enough that a shared link
/// is useless soon after.
pub const LINK_LIFETIME_MINUTES: i64 = 5;

/// What links are signed with: the account key, or a user delegation key of the Hub's identity.
#[derive(Clone)]
enum Signer {
    AccountKey(Vec<u8>),
    Delegation(Arc<Delegation>),
}

/// A private storage container the Hub may hand out files from.
#[derive(Clone)]
pub struct Download {
    account: String,
    container: String,
    signer: Signer,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("the storage account name must be 3 to 24 lowercase letters or digits")]
    Account,
    #[error("the container name must be lowercase letters, digits and dashes")]
    Container,
    #[error("the storage key is not valid base64")]
    Key,
    #[error("there is no storage signing key right now")]
    NoKey,
}

impl Download {
    pub fn new(account: &str, container: &str, key_base64: &str) -> Result<Self, DownloadError> {
        let key = STANDARD
            .decode(key_base64.trim())
            .map_err(|_| DownloadError::Key)?;
        Self::with_signer(account, container, Signer::AccountKey(key))
    }

    /// Links signed with the user delegation key of the Hub's managed identity, no account key.
    pub fn with_delegation(
        account: &str,
        container: &str,
        delegation: Arc<Delegation>,
    ) -> Result<Self, DownloadError> {
        Self::with_signer(account, container, Signer::Delegation(delegation))
    }

    fn with_signer(account: &str, container: &str, signer: Signer) -> Result<Self, DownloadError> {
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
        Ok(Self {
            account: account.to_owned(),
            container: container.to_owned(),
            signer,
        })
    }

    /// A read-only link to `blob` (a path inside the container) that works for
    /// [`LINK_LIFETIME_MINUTES`] from `now`.
    ///
    /// # Errors
    /// [`DownloadError::NoKey`] while no delegation key is available.
    pub fn link(&self, blob: &str, now: OffsetDateTime) -> Result<String, DownloadError> {
        self.sas(
            blob,
            "r",
            Duration::minutes(LINK_LIFETIME_MINUTES),
            None,
            now,
        )
    }

    /// A link to `blob` with exactly the permissions named (`r` read, `c` create, `w` write, `d`
    /// delete) for `lifetime` from `now`. With `content_type` the storage answers with that type
    /// whatever the file was stored as, so an uploaded file can never choose how a browser treats it.
    ///
    /// # Errors
    /// [`DownloadError::NoKey`] while no delegation key is available.
    pub fn sas(
        &self,
        blob: &str,
        permissions: &str,
        lifetime: Duration,
        content_type: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<String, DownloadError> {
        let mut ends = now + lifetime;
        let delegation = match &self.signer {
            Signer::AccountKey(_) => None,
            Signer::Delegation(source) => {
                let key = source.key(now).ok_or(DownloadError::NoKey)?;
                // A link may not outlive the key that signs it.
                ends = ends.min(key.expires_at);
                Some(key)
            }
        };
        let ends = ends
            .replace_nanosecond(0)
            .expect("zero is a valid nanosecond");
        let expiry = ends
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .expect("a time in range formats");
        let resource = format!("/blob/{}/{}/{}", self.account, self.container, blob);
        let content_type = content_type.unwrap_or("");
        let (to_sign, key, identity) = match (&self.signer, &delegation) {
            (Signer::AccountKey(key), _) => (
                account_key_string(permissions, &expiry, &resource, content_type),
                key.as_slice(),
                String::new(),
            ),
            (Signer::Delegation(_), Some(key)) => (
                delegation_string(permissions, &expiry, &resource, key, content_type),
                key.value.as_slice(),
                format!(
                    "&skoid={}&sktid={}&skt={}&ske={}&sks={}&skv={}",
                    encode(&key.object_id),
                    encode(&key.tenant_id),
                    encode(&key.starts),
                    encode(&key.expires),
                    encode(&key.service),
                    encode(&key.version),
                ),
            ),
            (Signer::Delegation(_), None) => return Err(DownloadError::NoKey),
        };
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
        mac.update(to_sign.as_bytes());
        let signature = STANDARD.encode(mac.finalize().into_bytes());
        let override_type = if content_type.is_empty() {
            String::new()
        } else {
            format!("&rsct={}", encode(content_type))
        };
        Ok(format!(
            "https://{}.blob.core.windows.net/{}/{}?sv={SAS_VERSION}&sp={permissions}&sr=b&se={}&spr=https{identity}{override_type}&sig={}",
            self.account,
            self.container,
            blob,
            encode(&expiry),
            encode(&signature),
        ))
    }
}

/// What the account key signs: the fields in the order Azure signs them (permissions, start,
/// expiry, resource, identifier, IP range, protocol, version, resource type, snapshot time,
/// encryption scope and the five response header overrides), the ones not used left empty.
fn account_key_string(
    permissions: &str,
    expiry: &str,
    resource: &str,
    content_type: &str,
) -> String {
    [
        permissions,
        "",
        expiry,
        resource,
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
        content_type,
    ]
    .join(
        "
",
    )
}

/// What a delegation key signs. The identifier is replaced by the key's own fields; the authorized,
/// unauthorized and correlation ids and the IP range stay empty.
fn delegation_string(
    permissions: &str,
    expiry: &str,
    resource: &str,
    key: &crate::delegation::DelegationKey,
    content_type: &str,
) -> String {
    [
        permissions,
        "",
        expiry,
        resource,
        &key.object_id,
        &key.tenant_id,
        &key.starts,
        &key.expires,
        &key.service,
        &key.version,
        "",
        "",
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
        content_type,
    ]
    .join(
        "
",
    )
}

/// Percent-encodes what a query value may not hold as it is. The signature and the expiry only
/// contain letters, digits and `+/=:-`, so that is all this needs to handle.
pub(crate) fn encode(value: &str) -> String {
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

    fn at(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).expect("a valid time")
    }

    #[test]
    fn a_link_names_the_file_and_expires_in_minutes() {
        let download =
            Download::new("ckdlabc12", "downloads", "a2V5LWZvci10ZXN0cw==").expect("valid");
        let link = download
            .link("forge/setup.exe", at("2026-11-14T12:00:00Z"))
            .expect("a link");
        assert!(link.starts_with(
            "https://ckdlabc12.blob.core.windows.net/downloads/forge/setup.exe?sv=2022-11-02&sp=r&sr=b&se=2026-11-14T12%3A05%3A00Z&spr=https&sig="
        ));
    }

    #[test]
    fn a_known_signature_matches_the_one_made_with_another_tool() {
        // Made with: python3 hmac-sha256 over the same 16 newline-joined fields.
        let download =
            Download::new("ckdlabc12", "downloads", "a2V5LWZvci10ZXN0cw==").expect("valid");
        let link = download
            .link("forge/setup.exe", at("2026-11-14T12:00:00Z"))
            .expect("a link");
        assert!(
            link.ends_with("&sig=LJkscj3n8xulIt%2FQnl088ZPKFvLVm%2FTmL0XeLv8gX2M%3D"),
            "{link}"
        );
    }

    #[test]
    fn write_and_typed_links_match_signatures_made_with_another_tool() {
        // Made with python3 hmac-sha256 over the 16 newline-joined fields, as for the read link.
        let download =
            Download::new("ckdlabc12", "downloads", "a2V5LWZvci10ZXN0cw==").expect("valid");
        let now = at("2026-11-14T12:00:00Z");
        let typed = download
            .sas(
                "forge/setup.exe",
                "r",
                Duration::hours(2),
                Some("video/x-matroska"),
                now,
            )
            .expect("a link");
        assert!(
            typed.ends_with("&sig=G7L8zlXcLgRoN%2BET%2FxwuuOm6X%2FQGgQLgnXAOXfql7QY%3D"),
            "{typed}"
        );
        let write = download
            .sas("forge/setup.exe", "cw", Duration::hours(2), None, now)
            .expect("a link");
        assert!(
            write.ends_with("&sig=mGmZSkU2pjG6zTBGa%2F3AyQcu3uKBkQ5Uns0fFUwZuBc%3D"),
            "{write}"
        );
    }

    fn delegating() -> (Download, Arc<Delegation>) {
        let delegation = Delegation::new(
            crate::delegation::Endpoints {
                identity: "http://127.0.0.1:1".to_owned(),
                identity_header: "h".to_owned(),
                storage: "http://127.0.0.1:1".to_owned(),
            },
            None,
        );
        let download = Download::with_delegation("ckdlabc12", "downloads", Arc::clone(&delegation))
            .expect("valid");
        (download, delegation)
    }

    fn test_key() -> crate::delegation::DelegationKey {
        crate::delegation::DelegationKey {
            object_id: "11111111-1111-1111-1111-111111111111".to_owned(),
            tenant_id: "22222222-2222-2222-2222-222222222222".to_owned(),
            starts: "2026-11-14T11:55:00Z".to_owned(),
            expires: "2026-11-14T18:00:00Z".to_owned(),
            expires_at: at("2026-11-14T18:00:00Z"),
            service: "b".to_owned(),
            version: "2022-11-02".to_owned(),
            value: b"key-for-tests".to_vec(),
        }
    }

    #[test]
    fn a_delegation_link_names_the_key_and_matches_a_signature_made_with_another_tool() {
        // Made with python3 hmac-sha256 over the 24 newline-joined fields of a user delegation SAS.
        let (download, delegation) = delegating();
        delegation.set_key(test_key());
        let link = download
            .link("forge/setup.exe", at("2026-11-14T12:00:00Z"))
            .expect("a link");
        assert!(link.contains(
            "&skoid=11111111-1111-1111-1111-111111111111&sktid=22222222-2222-2222-2222-222222222222&skt=2026-11-14T11%3A55%3A00Z&ske=2026-11-14T18%3A00%3A00Z&sks=b&skv=2022-11-02"
        ), "{link}");
        assert!(
            link.ends_with("&sig=ltidS09rrqUhfXxxUuiIJvnqe%2FmSLR4tpbOnh9foO2U%3D"),
            "{link}"
        );
    }

    #[test]
    fn a_delegation_link_never_outlives_its_key_and_needs_one() {
        let (download, delegation) = delegating();
        let now = at("2026-11-14T12:00:00Z");
        assert!(matches!(download.link("a", now), Err(DownloadError::NoKey)));
        delegation.set_key(test_key());
        let long = download
            .sas("a", "r", Duration::hours(12), None, now)
            .expect("a link");
        assert!(long.contains("se=2026-11-14T18%3A00%3A00Z"), "{long}");
        // The key has run out.
        assert!(download.link("a", at("2026-11-14T18:00:01Z")).is_err());
    }

    #[test]
    fn bad_settings_are_refused() {
        assert!(Download::new("UP", "downloads", "a2V5").is_err());
        assert!(Download::new("ckdlabc12", "Bad Name", "a2V5").is_err());
        assert!(Download::new("ckdlabc12", "downloads", "not base64!").is_err());
    }
}
