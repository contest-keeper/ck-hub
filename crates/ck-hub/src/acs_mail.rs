//! Sending mail through Azure Communication Services Email. The request is signed with the service's
//! access key; see the HMAC header tutorial in Azure's Communication Services documentation.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::mail::{Mail, MailError, Mailer};

/// The path and query the signature covers. The API version is part of it.
const SEND_PATH: &str = "/emails:send?api-version=2023-03-31";

/// Mail waiting to be sent. A full queue drops the next mail (and says so in the log) instead of
/// holding up a request.
const QUEUE: usize = 256;

fn content_hash(body: &[u8]) -> String {
    STANDARD.encode(Sha256::digest(body))
}

fn string_to_sign(path_and_query: &str, date: &str, host: &str, content_hash: &str) -> String {
    format!("POST\n{path_and_query}\n{date};{host};{content_hash}")
}

fn sign(key: &[u8], to_sign: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(to_sign.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

struct Settings {
    endpoint: String,
    host: String,
    sender: String,
    key: Vec<u8>,
}

/// Sends mail through Azure Communication Services. Mail is queued and sent from a background
/// thread, so a slow or unavailable Azure never holds up or fails a request.
pub struct AcsMailer {
    queue: SyncSender<Mail>,
}

impl AcsMailer {
    /// `endpoint` is the Communication Service's address (`https://<name>.communication.azure.com`),
    /// `sender` an address of a verified sender domain, `key_base64` the service's access key.
    /// Plain `http` is accepted only for a loopback address.
    ///
    /// # Errors
    /// When the endpoint, the sender or the key is not usable.
    pub fn new(endpoint: &str, sender: &str, key_base64: &str) -> Result<Self, MailError> {
        let endpoint = endpoint.trim_end_matches('/').to_owned();
        let (scheme, host) = endpoint
            .split_once("://")
            .ok_or_else(|| MailError("the mail endpoint must be https://host".into()))?;
        let host = host.to_owned();
        let loopback = host.starts_with("127.0.0.1")
            || host.starts_with("localhost")
            || host.starts_with("[::1]");
        let scheme_ok = scheme == "https" || (scheme == "http" && loopback);
        if !scheme_ok || host.is_empty() || host.contains('/') {
            return Err(MailError("the mail endpoint must be https://host".into()));
        }
        if !sender.contains('@') || sender.contains(char::is_whitespace) {
            return Err(MailError("the mail sender is not an email address".into()));
        }
        let key = STANDARD
            .decode(key_base64.trim())
            .map_err(|_| MailError("the mail key is not valid base64".into()))?;
        let settings = Settings {
            endpoint,
            host,
            sender: sender.to_owned(),
            key,
        };

        let (queue, inbox) = sync_channel::<Mail>(QUEUE);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        std::thread::Builder::new()
            .name("mail".into())
            .spawn(move || {
                for mail in inbox {
                    if let Err(error) = post(&agent, &settings, &mail) {
                        tracing::error!("cannot send mail: {error}");
                    }
                }
            })
            .map_err(|e| MailError(e.to_string()))?;
        Ok(Self { queue })
    }
}

impl Mailer for AcsMailer {
    fn send(&self, mail: Mail) -> Result<(), MailError> {
        self.queue
            .try_send(mail)
            .map_err(|_| MailError("the mail queue is full, the mail was dropped".into()))
    }
}

fn post(agent: &ureq::Agent, settings: &Settings, mail: &Mail) -> Result<(), String> {
    let body = serde_json::json!({
        "senderAddress": settings.sender,
        "recipients": { "to": [{ "address": mail.to }] },
        "content": { "subject": mail.subject, "plainText": mail.body },
    })
    .to_string();
    let date = Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let hash = content_hash(body.as_bytes());
    let signature = sign(
        &settings.key,
        &string_to_sign(SEND_PATH, &date, &settings.host, &hash),
    );
    agent
        .post(format!("{}{SEND_PATH}", settings.endpoint))
        .header("x-ms-date", &date)
        .header("x-ms-content-sha256", &hash)
        .header(
            "Authorization",
            format!(
                "HMAC-SHA256 SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature={signature}"
            ),
        )
        .header("Content-Type", "application/json")
        .send(body)
        // Only the status goes to the log: an answer can name the address.
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // key = the 32 bytes "0123456789abcdef" twice; body = {"a":1}. The expected values were made with
    // Python's hmac and hashlib, not with this code.
    const KEY_B64: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

    #[test]
    fn a_known_signature_matches_the_one_made_with_another_tool() {
        let key = STANDARD.decode(KEY_B64).expect("base64");
        let body = br#"{"a":1}"#;
        assert_eq!(
            content_hash(body),
            "AVq9f1zFei3ZS3WQ8ErYCEJzkF7jPsXOvq5iJ2qX+GI="
        );
        let to_sign = string_to_sign(
            SEND_PATH,
            "Sun, 15 Nov 2026 10:00:00 GMT",
            "acs-test.communication.azure.com",
            &content_hash(body),
        );
        assert_eq!(
            sign(&key, &to_sign),
            "HM7XDmW+GUoZJnyZHYMXrYZlqw4KKJg0O2y5H9JYsag="
        );
    }
}
