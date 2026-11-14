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
    let Ok(url) = std::env::var("CK_TEST_DATABASE_URL") else {
        eprintln!("skipped: set CK_TEST_DATABASE_URL to run against PostgreSQL");
        return;
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
