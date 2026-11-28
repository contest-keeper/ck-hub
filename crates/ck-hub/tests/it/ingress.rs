//! The proxy in front of the Hub, with a stand-in Hub and a stand-in queue on this machine.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use ck_hub::{Hub, Inbox, Ingress, MemoryStore, Registry, ingress_router, router};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[derive(Default)]
struct Seen {
    /// What reached the stand-in Hub: path, headers and body size.
    hub: Vec<(String, HeaderMap, usize)>,
    /// What reached the queue.
    queue: Vec<(HeaderMap, Bytes)>,
    queue_refuses: bool,
}

type Shared = Arc<Mutex<Seen>>;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address: SocketAddr = listener.local_addr().expect("address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    format!("http://{address}")
}

async fn hub(seen: Shared) -> String {
    serve(
        Router::new()
            .fallback(
                move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| async move {
                    let path = uri.path().to_owned();
                    seen.lock()
                        .expect("seen")
                        .hub
                        .push((path.clone(), headers, body.len()));
                    match path.as_str() {
                        "/slow" | "/api/v1/heartbeat" => {
                            tokio::time::sleep(Duration::from_secs(4)).await;
                            "late".into_response()
                        }
                        "/limited" => StatusCode::TOO_MANY_REQUESTS.into_response(),
                        "/cookies" => Response::builder()
                            .status(StatusCode::FOUND)
                            .header(header::LOCATION, "https://elsewhere.invalid/file")
                            .header(header::SET_COOKIE, "a=1")
                            .header(header::SET_COOKIE, "b=2")
                            .body(Body::empty())
                            .expect("answer"),
                        _ => format!("{method}").into_response(),
                    }
                },
            )
            // The real Hub takes a recording chunk.
            .layer(axum::extract::DefaultBodyLimit::disable()),
    )
    .await
}

async fn queue(seen: Shared) -> String {
    serve(Router::new().fallback(
        move |uri: Uri, headers: HeaderMap, body: Bytes| async move {
            if uri.path() == "/token" {
                return r#"{"access_token":"t"}"#.into_response();
            }
            let mut seen = seen.lock().expect("seen");
            if seen.queue_refuses {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            seen.queue.push((headers, body));
            StatusCode::CREATED.into_response()
        },
    ))
    .await
}

async fn proxy(hub: &str, queue: &str) -> Router {
    let inbox = Inbox::new(
        &format!("{queue}/hub"),
        format!("{queue}/token"),
        "header".to_owned(),
        None,
    );
    let hub = hub.to_owned();
    let ingress = tokio::task::spawn_blocking(move || {
        Ingress::start(&hub, inbox, "shared-secret".to_owned(), None)
    })
    .await
    .expect("start");
    ingress_router(ingress)
}

fn report(path: &str) -> Request<Body> {
    Request::post(path)
        .header("x-ck-stick", "stick-a")
        .header("x-ck-signature", "ab12")
        .body(Body::from(r#"{"seq":1}"#))
        .expect("request")
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let answer = app.clone().oneshot(request).await.expect("answer");
    let (parts, body) = answer.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    (parts.status, parts.headers, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_report_is_queued_only_when_the_hub_cannot_take_it() {
    let seen = Shared::default();
    let queue = queue(Arc::clone(&seen)).await;
    // Nothing listens on port 9 of this machine.
    let down = proxy("http://127.0.0.1:9", &queue).await;

    let (status, _, body) = send(&down, report("/api/v1/events")).await;
    assert_eq!((status, &body[..]), (StatusCode::ACCEPTED, &b"{}"[..]));
    {
        let seen = seen.lock().expect("seen");
        let (headers, body) = &seen.queue[0];
        assert_eq!(headers["ck-path"], "\"/api/v1/events\"");
        assert_eq!(headers["x-ck-stick"], "\"stick-a\"");
        assert_eq!(headers["x-ck-signature"], "\"ab12\"");
        assert!(
            headers["ck-received-at"]
                .to_str()
                .expect("text")
                .starts_with('"')
        );
        assert_eq!(&body[..], br#"{"seq":1}"#);
    }

    // Nothing but a stick's report goes in the queue.
    for request in [
        Request::post("/api/v1/login").body(Body::empty()),
        Request::post("/api/v1/events").body(Body::from("junk")),
    ] {
        let (status, _, _) = send(&down, request.expect("request")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
    assert_eq!(seen.lock().expect("seen").queue.len(), 1);

    // With no queue either, the stick must keep its copy.
    seen.lock().expect("seen").queue_refuses = true;
    let (status, _, _) = send(&down, report("/api/v1/events")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, _, body) = send(
        &down,
        Request::get("/status")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("Keeper Hub is down"));
}

#[tokio::test(flavor = "multi_thread")]
async fn everything_else_passes_through_as_it_is() {
    let seen = Shared::default();
    let queue = queue(Arc::clone(&seen)).await;
    let up = proxy(&hub(Arc::clone(&seen)).await, &queue).await;

    let request = Request::get("/cookies?x=1")
        .header(header::HOST, "hub.example.org")
        .header("x-forwarded-for", "6.6.6.6, 1.2.3.4")
        .header("x-ck-ingress-secret", "made up")
        .header("x-ck-client-ip", "6.6.6.6")
        .body(Body::empty())
        .expect("request");
    let (status, headers, _) = send(&up, request).await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(headers[header::LOCATION], "https://elsewhere.invalid/file");
    assert_eq!(headers.get_all(header::SET_COOKIE).iter().count(), 2);
    {
        let seen = seen.lock().expect("seen");
        let sent = &seen.hub[0].1;
        assert_eq!(sent["x-ck-ingress-secret"], "shared-secret");
        assert_eq!(sent["x-ck-client-ip"], "1.2.3.4");
        assert_eq!(sent["x-ck-forwarded-host"], "hub.example.org");
    }

    // A chunk of a recording, and an answer slower than a stick's report may be.
    let chunk = Request::put("/api/v1/recordings/1/chunks/0")
        .body(Body::from(vec![7; 4 * 1024 * 1024]))
        .expect("request");
    assert_eq!(send(&up, chunk).await.0, StatusCode::OK);
    assert_eq!(seen.lock().expect("seen").hub[1].2, 4 * 1024 * 1024);
    let slow = Request::post("/slow").body(Body::empty()).expect("request");
    assert_eq!(&send(&up, slow).await.2[..], b"late");
    let limited = send(&up, report("/limited")).await;
    assert_eq!(limited.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(seen.lock().expect("seen").queue.len(), 0);

    // The Hub takes longer than a stick waits: the report is queued.
    assert_eq!(
        send(&up, report("/api/v1/heartbeat")).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(seen.lock().expect("seen").queue.len(), 1);
}

#[tokio::test]
async fn a_browser_signs_in_through_the_proxy() {
    let hub = Hub::new(
        Registry::empty(),
        Arc::new(MemoryStore::default()),
        None,
        Box::new(time::OffsetDateTime::now_utc),
    )
    .with_ingress_secret("shared-secret".to_owned());
    let hub = router(Arc::new(hub));
    let login = |secret: &str| {
        Request::post("/api/v1/login")
            // The host the proxy reaches the Hub at, and the one the browser opened.
            .header(header::HOST, "hub.internal.example")
            .header(header::ORIGIN, "https://hub.example.org")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-ck-forwarded-host", "hub.example.org")
            .header("x-ck-ingress-secret", secret)
            .body(Body::from(
                r#"{"email":"a@example.org","password":"wrong"}"#,
            ))
            .expect("request")
    };
    // Past the cross-origin guard: refused only for the password.
    assert_eq!(
        send(&hub, login("shared-secret")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(send(&hub, login("a guess")).await.0, StatusCode::FORBIDDEN);
}

/// A body that never arrives.
struct Stalled;

impl http_body::Body for Stalled {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        std::task::Poll::Pending
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_sends_nothing_does_not_keep_its_room() {
    let seen = Shared::default();
    let proxy = proxy("http://127.0.0.1:9", &queue(seen).await).await;
    let request = Request::post("/api/v1/events")
        .body(Body::new(Stalled))
        .expect("request");
    assert_eq!(send(&proxy, request).await.0, StatusCode::REQUEST_TIMEOUT);
    // Room was given back: the next report is taken.
    assert_eq!(
        send(&proxy, report("/api/v1/events")).await.0,
        StatusCode::ACCEPTED
    );
}
