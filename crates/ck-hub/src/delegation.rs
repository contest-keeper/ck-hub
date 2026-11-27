//! A user delegation key, so that links to storage are signed without the storage account key.
//!
//! The Hub runs as a managed identity. It trades the identity's token for a user delegation key
//! (a key that lives for hours, made by the storage service for that identity) and signs links with
//! it. The identity needs the `Storage Blob Delegator` role on the account, and a data role on each
//! container a link may reach, so a link can never reach more than the identity itself may.
//!
//! A thread keeps the key fresh, so the code that makes a link never waits for the network.

use std::sync::{Arc, RwLock};
use std::time::Duration as StdDuration;

use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// The storage REST version the key is asked for and the links are signed for.
pub const STORAGE_VERSION: &str = "2022-11-02";

/// How long a key asked for now lives.
const KEY_LIFETIME_HOURS: i64 = 6;

/// A key is asked for again this often, so one always has hours left.
const REFRESH_EVERY: StdDuration = StdDuration::from_secs(3600);

/// After a failure, try again soon.
const RETRY_AFTER: StdDuration = StdDuration::from_secs(30);

/// What the storage service answers with, as a link needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationKey {
    pub object_id: String,
    pub tenant_id: String,
    pub starts: String,
    pub expires: String,
    pub expires_at: OffsetDateTime,
    pub service: String,
    pub version: String,
    pub value: Vec<u8>,
}

/// Where the managed identity's token and the storage service are, so a test can point at its own.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// The identity endpoint Container Apps sets in `IDENTITY_ENDPOINT`.
    pub identity: String,
    /// The header value Container Apps sets in `IDENTITY_HEADER`.
    pub identity_header: String,
    /// `https://<account>.blob.core.windows.net`.
    pub storage: String,
}

pub struct Delegation {
    endpoints: Endpoints,
    client_id: Option<String>,
    agent: ureq::Agent,
    current: RwLock<Option<Arc<DelegationKey>>>,
}

impl Delegation {
    /// For the account `account`, with the endpoints Container Apps provides. `None` when this is
    /// not a Container App with a managed identity.
    #[must_use]
    pub fn from_environment(account: &str, client_id: Option<String>) -> Option<Arc<Self>> {
        let identity = std::env::var("IDENTITY_ENDPOINT").ok()?;
        let identity_header = std::env::var("IDENTITY_HEADER").ok()?;
        Some(Self::new(
            Endpoints {
                identity,
                identity_header,
                storage: format!("https://{account}.blob.core.windows.net"),
            },
            client_id,
        ))
    }

    #[must_use]
    pub fn new(endpoints: Endpoints, client_id: Option<String>) -> Arc<Self> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(StdDuration::from_secs(15)))
            .http_status_as_error(false)
            .build()
            .into();
        Arc::new(Self {
            endpoints,
            client_id,
            agent,
            current: RwLock::new(None),
        })
    }

    /// Gets a key now, then keeps one fresh from a thread of its own.
    pub fn start(self: &Arc<Self>) {
        if let Err(reason) = self.refresh(OffsetDateTime::now_utc()) {
            tracing::error!("no storage delegation key yet: {reason}");
        }
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            let mut wait = REFRESH_EVERY;
            loop {
                std::thread::sleep(wait);
                wait = match this.refresh(OffsetDateTime::now_utc()) {
                    Ok(()) => REFRESH_EVERY,
                    Err(reason) => {
                        tracing::error!("storage delegation key not renewed: {reason}");
                        RETRY_AFTER
                    }
                };
            }
        });
    }

    /// Puts a key in place without asking the storage service, for tests of the links.
    #[cfg(test)]
    pub(crate) fn set_key(&self, key: DelegationKey) {
        *self.current.write().expect("delegation lock") = Some(Arc::new(key));
    }

    /// The key to sign with, while it still has time left.
    #[must_use]
    pub fn key(&self, now: OffsetDateTime) -> Option<Arc<DelegationKey>> {
        self.current
            .read()
            .expect("delegation lock")
            .clone()
            .filter(|key| key.expires_at > now)
    }

    /// Asks the storage service for a new key.
    ///
    /// # Errors
    /// A text that never holds a token or key, when the identity or the storage refuses.
    pub fn refresh(&self, now: OffsetDateTime) -> Result<(), String> {
        let token = self.token()?;
        let key = self.fetch_key(&token, now)?;
        *self.current.write().expect("delegation lock") = Some(Arc::new(key));
        Ok(())
    }

    fn token(&self) -> Result<String, String> {
        identity_token(
            &self.agent,
            &self.endpoints.identity,
            &self.endpoints.identity_header,
            self.client_id.as_deref(),
            "https://storage.azure.com/",
        )
    }

    fn fetch_key(&self, token: &str, now: OffsetDateTime) -> Result<DelegationKey, String> {
        let start = format_time(now - Duration::minutes(5));
        let expiry = format_time(now + Duration::hours(KEY_LIFETIME_HOURS));
        let body = format!("<KeyInfo><Start>{start}</Start><Expiry>{expiry}</Expiry></KeyInfo>");
        let url = format!(
            "{}/?restype=service&comp=userdelegationkey",
            self.endpoints.storage
        );
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("x-ms-version", STORAGE_VERSION)
            .header("Content-Type", "application/xml")
            .send(body.as_bytes())
            .map_err(|_| "the storage service cannot be reached".to_owned())?;
        if !response.status().is_success() {
            return Err(format!(
                "the storage service answered {} to the delegation key request",
                response.status().as_u16()
            ));
        }
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|_| "the storage answer cannot be read".to_owned())?;
        parse_key(&text)
    }
}

/// A token of the managed identity for `resource`, from the endpoint Container Apps provides.
/// The error never holds a token.
pub(crate) fn identity_token(
    agent: &ureq::Agent,
    identity: &str,
    identity_header: &str,
    client_id: Option<&str>,
    resource: &str,
) -> Result<String, String> {
    let mut url = format!(
        "{identity}?api-version=2019-08-01&resource={}",
        crate::download::encode(resource)
    );
    if let Some(client_id) = client_id {
        url.push_str("&client_id=");
        url.push_str(&crate::download::encode(client_id));
    }
    let mut response = agent
        .get(&url)
        .header("X-IDENTITY-HEADER", identity_header)
        .call()
        .map_err(|_| "the identity endpoint cannot be reached".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "the identity endpoint answered {}",
            response.status().as_u16()
        ));
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|_| "the identity answer cannot be read".to_owned())?;
    parse_token(&text)
}

fn format_time(at: OffsetDateTime) -> String {
    at.replace_nanosecond(0)
        .expect("zero is a valid nanosecond")
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .expect("a time in range formats")
}

/// The token out of the identity endpoint's JSON answer.
fn parse_token(text: &str) -> Result<String, String> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "the identity answer is not JSON".to_owned())?;
    json.get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "the identity answer has no token".to_owned())
}

/// The text between `<tag>` and `</tag>`. The answer is a flat document of known tags.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let from = xml.find(&open)? + open.len();
    let to = xml[from..].find(&close)? + from;
    Some(xml[from..to].trim())
}

fn parse_key(xml: &str) -> Result<DelegationKey, String> {
    use base64::Engine;
    let field = |name: &str| {
        tag(xml, name)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("the delegation key has no {name}"))
    };
    let expires = field("SignedExpiry")?;
    let expires_at = OffsetDateTime::parse(&expires, &Rfc3339)
        .map_err(|_| "the delegation key has no valid expiry".to_owned())?;
    let value = base64::engine::general_purpose::STANDARD
        .decode(field("Value")?)
        .map_err(|_| "the delegation key value is not base64".to_owned())?;
    Ok(DelegationKey {
        object_id: field("SignedOid")?,
        tenant_id: field("SignedTid")?,
        starts: field("SignedStart")?,
        expires,
        expires_at,
        service: field("SignedService")?,
        version: field("SignedVersion")?,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const KEY_XML: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><UserDelegationKey>\
        <SignedOid>11111111-1111-1111-1111-111111111111</SignedOid>\
        <SignedTid>22222222-2222-2222-2222-222222222222</SignedTid>\
        <SignedStart>2026-11-14T11:55:00Z</SignedStart>\
        <SignedExpiry>2026-11-14T18:00:00Z</SignedExpiry>\
        <SignedService>b</SignedService><SignedVersion>2022-11-02</SignedVersion>\
        <Value>a2V5LWZvci10ZXN0cw==</Value></UserDelegationKey>";

    fn at(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).expect("a valid time")
    }

    #[test]
    fn a_key_answer_is_read_and_a_broken_one_is_not() {
        let key = parse_key(KEY_XML).expect("a key");
        assert_eq!(key.object_id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(key.expires_at, at("2026-11-14T18:00:00Z"));
        assert_eq!(key.value, b"key-for-tests");
        assert!(parse_key("<UserDelegationKey></UserDelegationKey>").is_err());
        assert!(parse_key(&KEY_XML.replace("a2V5LWZvci10ZXN0cw==", "not base64!")).is_err());
    }

    #[test]
    fn a_token_answer_is_read() {
        assert_eq!(
            parse_token(r#"{"access_token":"abc","expires_on":"1"}"#).as_deref(),
            Ok("abc")
        );
        assert!(parse_token(r#"{"access_token":""}"#).is_err());
        assert!(parse_token("nonsense").is_err());
    }

    /// Answers each connection with the next canned reply and keeps the request text.
    fn serve(replies: Vec<(u16, &'static str)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = format!("http://{}", listener.local_addr().expect("address"));
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().expect("accept");
                // The head and the body may arrive in separate packets: read until it goes quiet.
                stream
                    .set_read_timeout(Some(std::time::Duration::from_millis(300)))
                    .expect("timeout");
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                }
                seen.push(String::from_utf8_lossy(&request).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).expect("write");
            }
            seen
        });
        (address, handle)
    }

    #[test]
    fn refreshing_trades_the_identity_token_for_a_key() {
        let (identity, identity_seen) = serve(vec![(200, r#"{"access_token":"tok-1"}"#)]);
        let (storage, storage_seen) = serve(vec![(200, KEY_XML)]);
        let delegation = Delegation::new(
            Endpoints {
                identity,
                identity_header: "header-secret".to_owned(),
                storage,
            },
            Some("client-1".to_owned()),
        );
        let now = at("2026-11-14T12:00:00Z");
        assert!(delegation.key(now).is_none());
        delegation.refresh(now).expect("a key");
        assert!(delegation.key(now).is_some());
        assert!(delegation.key(at("2026-11-14T18:00:01Z")).is_none());

        let identity_request = identity_seen.join().expect("thread").remove(0);
        assert!(identity_request.contains("client_id=client-1"));
        assert!(identity_request.contains("resource=https%3A%2F%2Fstorage.azure.com%2F"));
        assert!(
            identity_request
                .to_lowercase()
                .contains("x-identity-header: header-secret")
        );
        let storage_request = storage_seen.join().expect("thread").remove(0);
        assert!(storage_request.starts_with("POST /?restype=service&comp=userdelegationkey"));
        assert!(storage_request.contains("Bearer tok-1"));
        assert!(storage_request.contains("<Start>2026-11-14T11:55:00Z</Start>"));
        assert!(storage_request.contains("<Expiry>2026-11-14T18:00:00Z</Expiry>"));
    }

    #[test]
    fn a_refusal_keeps_the_old_key_and_says_why_without_secrets() {
        let (identity, _) = serve(vec![(500, "boom")]);
        let delegation = Delegation::new(
            Endpoints {
                identity,
                identity_header: "h".to_owned(),
                storage: "http://127.0.0.1:1".to_owned(),
            },
            None,
        );
        let reason = delegation
            .refresh(at("2026-11-14T12:00:00Z"))
            .expect_err("refused");
        assert_eq!(reason, "the identity endpoint answered 500");
    }
}
