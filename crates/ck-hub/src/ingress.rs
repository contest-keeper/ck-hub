//! The proxy in front of the Hub (`ck-hub ingress`). It passes every request on. When the Hub
//! cannot take a heartbeat or an event, it puts the message in the queue and tells the stick it is
//! accepted; the Hub takes it from there later. It also serves the status page, which therefore
//! stays up when the Hub does not.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ck_protocol::{HEARTBEAT_PATH, SIGNATURE_HEADER, STICK_HEADER, StickId};
use time::OffsetDateTime;
use tokio::sync::Semaphore;

use crate::inbox::Inbox;
use crate::routes::{EVENTS_PATH, MAX_BODY_BYTES};
use crate::source::{CLIENT_ADDRESS, INGRESS_SECRET, PUBLIC_HOST, source_of};
use crate::status::{Outcome, Status};

/// A stick gives up after 5 seconds, so the Hub and then the queue must answer inside that.
const HUB_WAIT_FOR_REPORT: Duration = Duration::from_secs(3);
const QUEUE_WAIT: Duration = Duration::from_millis(1500);
/// Every request holds a thread while it waits, so a hung Hub must not keep them long. Only a chunk
/// of a recording, which the Hub also has to store, may take minutes.
const HUB_WAIT: Duration = Duration::from_secs(30);
const HUB_WAIT_FOR_CHUNK: Duration = Duration::from_secs(300);
/// The status page waits longer than a stick, so a Hub that is only waking up is not called down.
const HUB_WAIT_FOR_STATUS: Duration = Duration::from_secs(20);
/// The largest body the Hub takes is a profile with its wallpaper, under 8 MiB.
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_ANSWER_BYTES: u64 = 64 * 1024 * 1024;
/// How much of the memory request bodies may use at once, in KiB.
const BODY_BUDGET_KIB: u32 = 64 * 1024;
/// Messages one address may have queued in a minute: 500 sticks behind one venue's address send
/// about 5000. It stops one source from filling the queue while the Hub is down.
const MAX_QUEUED_PER_ADDRESS: u32 = 12_000;
/// Headers about one connection, which a proxy does not pass on.
const CONNECTION_HEADERS: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
];

pub struct Ingress {
    /// The Hub's own address.
    hub: String,
    queue: Inbox,
    secret: String,
    agent: ureq::Agent,
    bodies: Semaphore,
    /// Per client address: when the current minute started and what it queued in it.
    queued: Mutex<HashMap<String, (Instant, u32)>>,
    pub(crate) status: Status,
}

impl Ingress {
    /// Reads the kept status history and starts saving it. Blocks while it does.
    #[must_use]
    pub fn start(hub: &str, queue: Inbox, secret: String, status_url: Option<String>) -> Arc<Self> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // A redirect is the client's to follow.
            .max_redirects(0)
            .build()
            .into();
        // The first message of an outage should not wait for a token as well.
        if let Err(reason) = queue.token() {
            tracing::error!("no token for the queue yet: {reason}");
        }
        let ingress = Arc::new(Self {
            hub: hub.trim_end_matches('/').to_owned(),
            queue,
            secret,
            agent,
            bodies: Semaphore::new(BODY_BUDGET_KIB as usize),
            queued: Mutex::new(HashMap::new()),
            status: Status::new(status_url),
        });
        ingress.status.load(&ingress.queue);
        let saver = Arc::clone(&ingress);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(60));
                saver.save_status();
            }
        });
        ingress
    }

    /// Keeps the status history, when it changed.
    pub fn save_status(&self) {
        self.status.save(&self.queue);
    }

    /// Whether the Hub answers now. Its page needs no database, so asking does not wake one.
    pub(crate) fn hub_answers(&self) -> bool {
        self.agent
            .get(format!("{}/", self.hub))
            .config()
            .timeout_global(Some(HUB_WAIT_FOR_STATUS))
            .build()
            .call()
            .is_ok_and(|answer| answer.status().is_success())
    }

    fn may_queue(&self, address: &str) -> bool {
        let mut queued = self.queued.lock().expect("queued lock");
        let now = Instant::now();
        queued.retain(|_, (since, _)| now.duration_since(*since) < Duration::from_secs(60));
        let (_, count) = queued.entry(address.to_owned()).or_insert((now, 0));
        *count += 1;
        *count <= MAX_QUEUED_PER_ADDRESS
    }

    fn send_on(
        &self,
        method: &Method,
        target: &str,
        headers: &HeaderMap,
        body: &[u8],
        wait: Duration,
    ) -> Result<Response, ureq::Error> {
        let mut request = ureq::http::Request::builder()
            .method(method.clone())
            .uri(format!("{}{target}", self.hub));
        for (name, value) in headers {
            request = request.header(name, value);
        }
        // A request without a body must not announce one.
        let answer = if body.is_empty() && matches!(*method, Method::GET | Method::HEAD) {
            let request = request.body(())?;
            let request = self.agent.configure_request(request);
            self.agent.run(request.timeout_global(Some(wait)).build())?
        } else {
            let request = request.body(body)?;
            let request = self.agent.configure_request(request);
            self.agent.run(
                request
                    .timeout_global(Some(wait))
                    .allow_non_standard_methods(true)
                    .build(),
            )?
        };
        let (mut parts, mut answer) = answer.into_parts();
        let answer = answer.with_config().limit(MAX_ANSWER_BYTES).read_to_vec()?;
        strip(&mut parts.headers);
        // A HEAD answer has the length of the body it does not carry.
        if *method != Method::HEAD {
            parts.headers.remove(header::CONTENT_LENGTH);
        }
        Ok(Response::from_parts(parts, Body::from(answer)))
    }

    /// Passes one request on, and queues it when it is a report the Hub cannot take now.
    fn pass(
        &self,
        method: &Method,
        path: &str,
        target: &str,
        headers: &HeaderMap,
        body: &[u8],
        address: &str,
    ) -> Response {
        let report = *method == Method::POST && (path == HEARTBEAT_PATH || path == EVENTS_PATH);
        let wait = if report {
            HUB_WAIT_FOR_REPORT
        } else if *method == Method::PUT && path.starts_with("/api/v1/recordings/") {
            HUB_WAIT_FOR_CHUNK
        } else {
            HUB_WAIT
        };
        let received_at = OffsetDateTime::now_utc();
        let answer = self.send_on(method, target, headers, body, wait);
        let down = answer
            .as_ref()
            .map_or(true, |answer| matches!(answer.status().as_u16(), 502..=504));
        let unreachable = || (StatusCode::BAD_GATEWAY, "the Hub cannot be reached").into_response();
        if !report {
            return answer.unwrap_or_else(|_| unreachable());
        }
        if !down {
            self.status.count(received_at, Outcome::Delivered);
            return answer.unwrap_or_else(|_| unreachable());
        }
        // Only what looks like a stick's report goes in: the Hub still checks the signature.
        let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        let stick = text(STICK_HEADER).filter(|id| StickId::parse(id).is_ok());
        let signature = text(SIGNATURE_HEADER)
            .filter(|s| (1..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()));
        let (Some(stick), Some(signature)) = (stick, signature) else {
            return answer.unwrap_or_else(|_| unreachable());
        };
        if !self.may_queue(address) {
            return (StatusCode::TOO_MANY_REQUESTS, "too many messages").into_response();
        }
        match self
            .queue
            .put(path, stick, signature, received_at, body, QUEUE_WAIT)
        {
            // The stick drops its copy at this answer, so it is given only once the queue has it.
            Ok(()) => {
                self.status.count(received_at, Outcome::Queued);
                (
                    StatusCode::ACCEPTED,
                    [(header::CONTENT_TYPE, "application/json")],
                    "{}",
                )
                    .into_response()
            }
            Err(reason) => {
                tracing::error!(stick, "a report could not be queued: {reason}");
                self.status.count(received_at, Outcome::Refused);
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the Hub cannot take this now",
                )
                    .into_response()
            }
        }
    }
}

/// Drops the headers about the connection, the ones the sender names included.
fn strip(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| name.trim().parse().ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in CONNECTION_HEADERS {
        headers.remove(name);
    }
}

async fn forward(State(ingress): State<Arc<Ingress>>, request: Request) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let (parts, body) = request.into_parts();
    let address = source_of(&parts.headers, peer).0;
    let path = parts.uri.path().to_owned();
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| path.clone(), |target| target.as_str().to_owned());
    let mut headers = parts.headers;
    let host = headers.remove(header::HOST);
    strip(&mut headers);
    headers.remove(header::CONTENT_LENGTH);
    // What the proxy says about the client replaces whatever the client said.
    headers.remove(PUBLIC_HOST);
    if let Some(host) = host {
        headers.insert(PUBLIC_HOST, host);
    }
    for (name, value) in [
        (INGRESS_SECRET, &ingress.secret),
        (CLIENT_ADDRESS, &address),
    ] {
        headers.remove(name);
        if let Ok(value) = value.parse() {
            headers.insert(name, value);
        }
    }

    // A report is small; anything else says its size, or is counted as the largest.
    let limit = if path == HEARTBEAT_PATH || path == EVENTS_PATH {
        MAX_BODY_BYTES
    } else {
        MAX_REQUEST_BYTES
    };
    let announced = axum::body::HttpBody::size_hint(&body)
        .upper()
        .and_then(|size| usize::try_from(size).ok())
        .unwrap_or(limit);
    if announced > limit {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let kib = u32::try_from(announced.div_ceil(1024)).unwrap_or(BODY_BUDGET_KIB);
    let Ok(_room) = ingress.bodies.acquire_many(kib).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(body): Result<Bytes, _> = axum::body::to_bytes(body, limit).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let worker = Arc::clone(&ingress);
    let method = parts.method;
    tokio::task::spawn_blocking(move || {
        worker.pass(&method, &path, &target, &headers, &body, &address)
    })
    .await
    .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

/// Every request goes to the Hub, except the status page and its data.
pub fn ingress_router(ingress: Arc<Ingress>) -> Router {
    Router::new()
        .route("/status", get(crate::status::page))
        .route("/status.json", get(crate::status::data))
        .fallback(forward)
        .with_state(ingress)
}
