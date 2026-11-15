//! Follows a contest on the judge and keeps what it reports.

use std::thread::sleep;
use std::time::Duration;

use chrono::Utc;

use crate::codeforces::{Client, CodeforcesError};
use crate::store::{Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error(transparent)]
    Judge(#[from] CodeforcesError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Reads pages from the newest submission backwards and keeps them, until a page has nothing the
/// Hub did not already have, or is short (the oldest page). Returns how many were new.
///
/// A failed page stops the poll with an error: what was kept stays, and the next poll starts
/// again from the newest page, so nothing is skipped. `pause` sits between pages because
/// Codeforces allows about one call every two seconds.
///
/// # Errors
/// The judge or the store failing.
pub fn poll_once(
    client: &Client,
    store: &dyn Store,
    organization: &str,
    contest_id: u64,
    page_size: u64,
    pause: Duration,
) -> Result<usize, WatchError> {
    let mut total = 0;
    let mut from = 1;
    loop {
        let page = client.status(contest_id, from, page_size)?;
        let added = store.add_judge_submissions(organization, "codeforces", &page, Utc::now())?;
        total += added;
        let short = page.len() < usize::try_from(page_size).unwrap_or(usize::MAX);
        if added == 0 || short {
            return Ok(total);
        }
        from += page_size;
        sleep(pause);
    }
}

/// Polls forever: every `interval`, and after a failure the wait doubles up to `max_wait`, then
/// goes back to `interval` after a success. A failure is logged without the request, which
/// carries the API key.
pub fn run(
    client: &Client,
    store: &dyn Store,
    organization: &str,
    contest_id: u64,
    interval: Duration,
    max_wait: Duration,
) {
    let mut wait = interval;
    loop {
        match poll_once(
            client,
            store,
            organization,
            contest_id,
            100,
            Duration::from_secs(2),
        ) {
            Ok(new) => {
                tracing::info!(contest_id, new, "polled codeforces");
                wait = interval;
            }
            Err(error) => {
                tracing::warn!(contest_id, %error, "codeforces poll failed");
                wait = (wait * 2).min(max_wait);
            }
        }
        sleep(wait);
    }
}
