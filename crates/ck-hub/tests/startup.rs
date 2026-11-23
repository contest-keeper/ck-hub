//! The real program, started the way a deployment starts it. The other tests build a Hub in
//! code, so they never run `main`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn status_of(address: &str) -> Option<String> {
    let mut stream = TcpStream::connect(address).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .ok()?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).ok()?;
    answer.lines().next().map(str::to_owned)
}

#[test]
fn the_program_starts_on_postgres_and_answers() {
    let url = match std::env::var("CK_TEST_DATABASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            assert!(
                std::env::var("CK_TEST_DATABASE_REQUIRED").as_deref() != Ok("1"),
                "CK_TEST_DATABASE_REQUIRED=1 but CK_TEST_DATABASE_URL is not set"
            );
            eprintln!("skipped: set CK_TEST_DATABASE_URL to run against PostgreSQL");
            return;
        }
    };
    // A free port: ask the system for one and let it go.
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("a free port")
        .local_addr()
        .expect("address")
        .port();
    let address = format!("127.0.0.1:{port}");
    let child = Command::new(env!("CARGO_BIN_EXE_ck-hub"))
        .args(["--listen", &address, "--database-url", &url])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the program");
    let mut running = Running(child);

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = running.0.try_wait().expect("check the program") {
            panic!("the program stopped by itself with {status}");
        }
        if let Some(line) = status_of(&address) {
            assert!(line.contains("200"), "{line}");
            return;
        }
        assert!(Instant::now() < deadline, "the program never answered");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The Azure key is read from the environment only. Without it the program must stop at once, say
/// where the key belongs, and never print a key it was given.
#[test]
fn acs_mail_needs_its_key_and_never_prints_it() {
    let program = env!("CARGO_BIN_EXE_ck-hub");
    let base = [
        "--listen",
        "127.0.0.1:0",
        "--mail",
        "acs",
        "--public-url",
        "https://hub.example.org",
        "--mail-endpoint",
        "https://x.communication.azure.com",
        "--mail-from",
        "noreply@mail.example.org",
    ];

    let missing = Command::new(program)
        .args(base)
        .env_remove("CK_ACS_KEY")
        .output()
        .expect("run the program");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("CK_ACS_KEY"));

    let secret = "this is not base64 and must not be printed!";
    let bad = Command::new(program)
        .args(base)
        .env("CK_ACS_KEY", secret)
        .output()
        .expect("run the program");
    assert!(!bad.status.success());
    let all = [bad.stdout, bad.stderr].concat();
    assert!(!String::from_utf8_lossy(&all).contains(secret));
}
