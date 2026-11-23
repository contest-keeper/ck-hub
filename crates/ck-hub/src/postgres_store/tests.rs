//! The connection pool, against a real database.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::PostgresStore;

#[test]
fn waiting_request_uses_whichever_connection_is_released() {
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
    let store = PostgresStore::connect_with_pool(&url, 2).expect("PostgreSQL test database");
    let (acquired, holds_connection) = mpsc::channel();
    let (release_first, first) = mpsc::channel();
    let (release_second, second) = mpsc::channel();
    let (completed, finished) = mpsc::channel();
    let wait = Duration::from_secs(3);
    thread::scope(|scope| {
        for release in [first, second] {
            let store = &store;
            let acquired = acquired.clone();
            scope.spawn(move || {
                store
                    .run(|_| {
                        acquired.send(()).expect("connection acquired");
                        release.recv_timeout(wait * 3).expect("release connection");
                        Ok(())
                    })
                    .expect("hold connection");
            });
            holds_connection
                .recv_timeout(wait)
                .expect("held connection");
        }
        scope.spawn(|| {
            store.run(|_| Ok(())).expect("use available connection");
            completed.send(()).expect("request finished");
        });
        assert!(finished.recv_timeout(Duration::from_millis(100)).is_err());
        release_second.send(()).expect("release second connection");
        let result = finished.recv_timeout(wait);
        release_first.send(()).expect("release first connection");
        assert!(
            result.is_ok(),
            "the request waited on a busy connection while another was idle"
        );
    });
}
