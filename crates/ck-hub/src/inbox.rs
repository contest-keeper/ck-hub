//! Messages that waited in an Azure Service Bus queue while the Hub could not be reached. The
//! proxy in front of the Hub puts them there; the Hub takes them out here, over the queue's REST
//! interface, signed in with its managed identity.
//!
//! The queue is trusted for one thing only: when a message arrived. Only the proxy's identity may
//! send to it, and every message still needs the stick's signature.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ck_protocol::{HEARTBEAT_PATH, SIGNATURE_HEADER, STICK_HEADER};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use ureq::http::HeaderMap;

use crate::delegation::identity_token;
use crate::hub::{Hub, Rejection};
use crate::routes::{EVENTS_PATH, MAX_BODY_BYTES};

/// The properties the proxy adds to a message, next to the stick's own two headers.
const PATH_PROPERTY: &str = "ck-path";
const TIME_PROPERTY: &str = "ck-received-at";

/// How long one request for a message waits for one to arrive.
const WAIT_SECONDS: u64 = 30;
/// The identity endpoint keeps its tokens for hours; asking again this often is cheap.
const TOKEN_LIFETIME: Duration = Duration::from_secs(600);
const FIRST_RETRY: Duration = Duration::from_secs(5);
const LAST_RETRY: Duration = Duration::from_secs(300);
/// The longest wait while a message is held: well inside the shortest lock a queue is given.
const LAST_HELD_RETRY: Duration = Duration::from_secs(20);

/// The queue, for the Hub that takes messages out and for the proxy that puts them in, so both
/// name the properties the same way.
pub struct Inbox {
    /// `https://<namespace>.servicebus.windows.net/<queue>`.
    queue: String,
    identity: String,
    identity_header: String,
    client_id: Option<String>,
    agent: ureq::Agent,
    token: Mutex<Option<(String, Instant)>>,
}

impl Inbox {
    /// `queue` is the queue's address; `identity` and `identity_header` are what Container Apps
    /// sets in `IDENTITY_ENDPOINT` and `IDENTITY_HEADER`.
    #[must_use]
    pub fn new(
        queue: &str,
        identity: String,
        identity_header: String,
        client_id: Option<String>,
    ) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(WAIT_SECONDS + 15)))
            .http_status_as_error(false)
            // A redirect would carry the token elsewhere.
            .max_redirects(0)
            .build()
            .into();
        Self {
            queue: queue.trim_end_matches('/').to_owned(),
            identity,
            identity_header,
            client_id,
            agent,
            token: Mutex::new(None),
        }
    }

    /// Puts a stick's message in the queue, as the proxy does when the Hub cannot take it. `Ok`
    /// only when the queue says it has it. `wait` bounds the whole request: the stick is waiting.
    pub(crate) fn put(
        &self,
        path: &str,
        stick: &str,
        signature: &str,
        received_at: OffsetDateTime,
        body: &[u8],
        wait: Duration,
    ) -> Result<(), String> {
        let token = self.token()?;
        let received_at = received_at
            .format(&Rfc3339)
            .map_err(|_| "the time cannot be written".to_owned())?;
        let request = self
            .agent
            .post(format!("{}/messages", self.queue))
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            // The queue keeps a text property only when it comes in quotes.
            .header(PATH_PROPERTY, &format!("\"{path}\""))
            .header(TIME_PROPERTY, &format!("\"{received_at}\""))
            .header(STICK_HEADER, &format!("\"{stick}\""))
            .header(SIGNATURE_HEADER, &format!("\"{signature}\""));
        let response = request
            .config()
            .timeout_global(Some(wait))
            .build()
            .send(body)
            .map_err(|_| "the queue cannot be reached".to_owned())?;
        match response.status().as_u16() {
            201 => Ok(()),
            status => Err(self.refused(status)),
        }
    }

    /// Takes messages out of the queue for as long as the program runs, from a thread of its own.
    ///
    /// # Errors
    /// When the thread cannot be started.
    pub fn start(self, hub: Arc<Hub>) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name("inbox".into())
            .spawn(move || {
                let mut retry = FIRST_RETRY;
                loop {
                    match self.take_one(&hub) {
                        Ok(_) => retry = FIRST_RETRY,
                        Err(reason) => {
                            tracing::error!("the queue is not being emptied: {reason}");
                            std::thread::sleep(retry);
                            retry = (retry * 2).min(LAST_RETRY);
                        }
                    }
                }
            })
            .map(|_| ())
    }

    /// Hands the oldest waiting message to the Hub and removes it. `false` when none is waiting.
    /// While the Hub cannot keep the message, it stays held here and no other one is taken.
    ///
    /// shortcut: one message at a time keeps their order; take several at once when a long outage's
    /// backlog drains too slowly.
    ///
    /// # Errors
    /// When the queue cannot be used. A message not yet removed comes back when its lock ends.
    pub fn take_one(&self, hub: &Hub) -> Result<bool, String> {
        let token = self.token()?;
        let mut response = self
            .agent
            .post(format!(
                "{}/messages/head?timeout={WAIT_SECONDS}",
                self.queue
            ))
            .header("Authorization", &format!("Bearer {token}"))
            .send_empty()
            .map_err(|_| "the queue cannot be reached".to_owned())?;
        match response.status().as_u16() {
            201 => {}
            204 => return Ok(false),
            status => return Err(self.refused(status)),
        }
        let headers = response.headers().clone();
        let lock = property(&headers, "location")
            // The token goes with the removal, so only to the queue it came from.
            .filter(|address| address.starts_with(&format!("{}/messages/", self.queue)))
            .ok_or("the queue named no usable lock address")?;
        let body = response
            .body_mut()
            .with_config()
            // The proxy takes no body larger than the live routes do.
            .limit(MAX_BODY_BYTES as u64)
            .read_to_vec();
        match body {
            Ok(body) => self.hold(&lock, FIRST_RETRY, || deliver(hub, &headers, &body))?,
            // It can never be accepted, and left in place it would block the ones behind it.
            Err(ureq::Error::BodyExceedsLimit(_)) => {
                tracing::warn!("a queued message is too large and is dropped");
            }
            Err(_) => return Err("a queued message cannot be read".to_owned()),
        }
        let removed = self
            .agent
            .delete(&lock)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|_| "the queue cannot be reached".to_owned())?;
        match removed.status().as_u16() {
            200 => Ok(true),
            status => Err(self.refused(status)),
        }
    }

    /// Repeats `give` until it succeeds, renewing the message's lock between tries. Letting go
    /// instead would let later messages overtake this one, and every new lock counts towards the
    /// limit at which the queue sets a message aside for good.
    fn hold(
        &self,
        lock: &str,
        mut wait: Duration,
        mut give: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        while let Err(reason) = give() {
            tracing::error!("{reason}; trying again");
            std::thread::sleep(wait);
            wait = (wait * 2).min(LAST_HELD_RETRY);
            let token = self.token()?;
            let renewed = self
                .agent
                .post(lock)
                .header("Authorization", &format!("Bearer {token}"))
                .send_empty()
                .map_err(|_| "the queue cannot be reached".to_owned())?;
            if renewed.status().as_u16() != 200 {
                return Err(self.refused(renewed.status().as_u16()));
            }
        }
        Ok(())
    }

    /// A token for the storage the status history is kept in. Asked for each time: once a minute
    /// at most.
    pub(crate) fn storage_token(&self) -> Result<String, String> {
        identity_token(
            &self.agent,
            &self.identity,
            &self.identity_header,
            self.client_id.as_deref(),
            "https://storage.azure.com/",
        )
    }

    fn refused(&self, status: u16) -> String {
        if status == 401 {
            *self.token.lock().expect("token lock") = None;
        }
        format!("the queue answered {status}")
    }

    pub(crate) fn token(&self) -> Result<String, String> {
        let mut kept = self.token.lock().expect("token lock");
        if let Some((token, since)) = kept.as_ref()
            && since.elapsed() < TOKEN_LIFETIME
        {
            return Ok(token.clone());
        }
        let token = identity_token(
            &self.agent,
            &self.identity,
            &self.identity_header,
            self.client_id.as_deref(),
            "https://servicebus.azure.net",
        )?;
        *kept = Some((token.clone(), Instant::now()));
        Ok(token)
    }
}

/// A header of the answer. The queue returns a text property in double quotes.
fn property(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches('"').to_owned())
}

/// Gives one message to the Hub. `Ok` means it is finished with: kept, already there, or never
/// going to be accepted. `Err` means it must stay in the queue.
fn deliver(hub: &Hub, headers: &HeaderMap, body: &[u8]) -> Result<(), String> {
    let path = property(headers, PATH_PROPERTY);
    let stick = property(headers, STICK_HEADER);
    let signature = property(headers, SIGNATURE_HEADER);
    let received_at = property(headers, TIME_PROPERTY)
        .and_then(|text| OffsetDateTime::parse(&text, &Rfc3339).ok());
    let (Some(path), Some(received_at)) = (path, received_at) else {
        tracing::warn!("a queued message without its address or time is dropped");
        return Ok(());
    };
    let (stick, signature) = (stick.as_deref(), signature.as_deref());
    let result = match path.as_str() {
        HEARTBEAT_PATH => hub.replay_heartbeat(stick, signature, body, received_at),
        EVENTS_PATH => hub.replay_event(stick, signature, body, received_at),
        _ => {
            tracing::warn!("a queued message for an address the queue does not serve is dropped");
            return Ok(());
        }
    };
    // Every other answer would be the same next time. The Hub has already logged it.
    match result {
        Err(Rejection::Unavailable) => Err("the Hub cannot keep a queued message now".to_owned()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn a_message_the_hub_cannot_keep_stays_locked_until_it_can() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("address"));
        let server = std::thread::spawn(move || {
            let answers = [r#"{"access_token":"t"}"#, "", ""];
            answers.map(|body| {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream);
                let mut first = String::new();
                reader.read_line(&mut first).expect("request line");
                let mut line = String::new();
                while line != "\r\n" {
                    line.clear();
                    reader.read_line(&mut line).expect("header");
                }
                write!(
                    reader.into_inner(),
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("answer");
                first
            })
        });
        let inbox = Inbox::new(&base, format!("{base}/token"), "header".to_owned(), None);

        let mut tries = 0;
        let held = inbox.hold(&format!("{base}/messages/1/lock"), Duration::ZERO, || {
            tries += 1;
            if tries < 3 {
                Err("not now".to_owned())
            } else {
                Ok(())
            }
        });

        assert_eq!(held, Ok(()));
        let seen = server.join().expect("server");
        for renewal in &seen[1..] {
            assert!(renewal.starts_with("POST /messages/1/lock "), "{renewal}");
        }
    }
}
