use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ck_hub::{AcsMailer, Mail, Mailer};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

// The 32 bytes "0123456789abcdef" twice, as base64, the form Azure shows an access key in.
const KEY_B64: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";

/// Answers one request with 202 and returns what it received: the header lines and the body.
fn fake_azure() -> (String, mpsc::Receiver<(Vec<String>, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("port");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("a request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut headers = Vec::new();
        let mut length = 0_usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("header line");
            let line = line.trim_end().to_owned();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().expect("length");
            }
            headers.push(line);
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).expect("body");
        let mut stream = stream;
        stream
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .expect("answer");
        tx.send((headers, String::from_utf8(body).expect("utf8")))
            .expect("send");
    });
    (endpoint, rx)
}

#[test]
fn a_mail_is_posted_signed_with_the_key_and_the_right_fields() {
    let (endpoint, received) = fake_azure();
    let mailer =
        AcsMailer::new(&endpoint, "noreply@mail.dev.example.org", KEY_B64).expect("mailer");
    mailer
        .send(Mail {
            to: "ana@example.org".into(),
            subject: "Confirm".into(),
            body: "Open the link.".into(),
        })
        .expect("queued");

    let (headers, body) = received
        .recv_timeout(Duration::from_secs(10))
        .expect("the request");
    let header = |name: &str| {
        headers.iter().find_map(|h| {
            h.to_ascii_lowercase()
                .strip_prefix(&format!("{name}:"))
                .map(|_| h.split_once(':').expect("colon").1.trim().to_owned())
        })
    };
    assert!(
        headers[0].starts_with("POST /emails:send?api-version=2023-03-31 "),
        "{}",
        headers[0]
    );
    let authorization = header("authorization").expect("authorization");
    assert!(
        authorization
            .starts_with("HMAC-SHA256 SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature="),
        "{authorization}"
    );
    // Recompute the signature from what arrived, the way Azure does: the string to sign names the
    // path, the date, the host and the hash of the body, and the key signs it.
    let date = header("x-ms-date").expect("date");
    let hash = header("x-ms-content-sha256").expect("content hash");
    let host = header("host").expect("host");
    assert_eq!(hash, STANDARD.encode(Sha256::digest(body.as_bytes())));
    let to_sign = format!("POST\n/emails:send?api-version=2023-03-31\n{date};{host};{hash}");
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&STANDARD.decode(KEY_B64).expect("base64")).expect("key");
    mac.update(to_sign.as_bytes());
    let expected = STANDARD.encode(mac.finalize().into_bytes());
    assert!(
        authorization.ends_with(&format!("&Signature={expected}")),
        "{authorization}"
    );
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["senderAddress"], "noreply@mail.dev.example.org");
    assert_eq!(json["recipients"]["to"][0]["address"], "ana@example.org");
    assert_eq!(json["content"]["subject"], "Confirm");
    assert_eq!(json["content"]["plainText"], "Open the link.");
}

#[test]
fn settings_that_are_not_safe_are_refused() {
    let https = "https://x.communication.azure.com";
    assert!(AcsMailer::new(https, "a@b.example", KEY_B64).is_ok());
    assert!(
        AcsMailer::new("http://x.communication.azure.com", "a@b.example", KEY_B64).is_err(),
        "plain http to a real host"
    );
    assert!(AcsMailer::new("x.communication.azure.com", "a@b.example", KEY_B64).is_err());
    assert!(AcsMailer::new(https, "not an address", KEY_B64).is_err());
    assert!(AcsMailer::new(https, "a@b.example", "not base64!").is_err());
}

#[test]
fn an_unreachable_azure_does_not_fail_or_slow_the_caller() {
    // Nothing listens on this port: the background thread logs the failure, the caller gets Ok at once.
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("address")
        .port();
    let mailer = AcsMailer::new(&format!("http://127.0.0.1:{port}"), "a@b.example", KEY_B64)
        .expect("mailer");
    let started = std::time::Instant::now();
    mailer
        .send(Mail {
            to: "ana@example.org".into(),
            subject: "Hello".into(),
            body: "Text".into(),
        })
        .expect("queued");
    assert!(started.elapsed() < Duration::from_secs(1));
}
