//! Follows a contest on the judge and keeps what it reports.

use std::thread::sleep;
use std::time::Duration;

use time::OffsetDateTime;

use crate::codeforces::{Client, CodeforcesError};
use crate::judge::{JudgeKind, JudgeSubmission};
use crate::store::{Store, StoreError};
use crate::vjudge;

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error(transparent)]
    Judge(#[from] CodeforcesError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One page of a judge's list: the submissions it holds that can be kept, and how many rows the
/// judge sent. A page is the last one when the judge sent fewer rows than were asked for, so a row
/// that cannot be kept must not make a full page look short.
pub struct Page {
    pub rows: Vec<JudgeSubmission>,
    pub raw: usize,
}

/// A judge's list of a contest's submissions, newest first, read a page at a time.
pub trait Pages {
    /// `from` starts at 1.
    ///
    /// # Errors
    /// The judge cannot be asked or does not answer as expected.
    fn page(&self, contest_id: u64, from: u64, count: u64) -> Result<Page, CodeforcesError>;
}

impl Pages for Client {
    fn page(&self, contest_id: u64, from: u64, count: u64) -> Result<Page, CodeforcesError> {
        let rows = self.status(contest_id, from, count)?;
        let raw = rows.len();
        Ok(Page { rows, raw })
    }
}

impl Pages for vjudge::Client {
    fn page(&self, contest_id: u64, from: u64, count: u64) -> Result<Page, CodeforcesError> {
        self.list(contest_id, "", from, count)
    }
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
    client: &dyn Pages,
    store: &dyn Store,
    organization: &str,
    judge: JudgeKind,
    contest_id: u64,
    page_size: u64,
    pause: Duration,
) -> Result<usize, WatchError> {
    let mut total = 0;
    let mut from = 1;
    loop {
        let page = client.page(contest_id, from, page_size)?;
        let added = store.add_judge_submissions(
            organization,
            judge.key(),
            &page.rows,
            OffsetDateTime::now_utc(),
        )?;
        total += added;
        let short = page.raw < usize::try_from(page_size).unwrap_or(usize::MAX);
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
    client: &dyn Pages,
    store: &dyn Store,
    organization: &str,
    judge: JudgeKind,
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
            judge,
            contest_id,
            100,
            Duration::from_secs(2),
        ) {
            Ok(new) => {
                tracing::info!(contest_id, new, judge = judge.key(), "polled the judge");
                wait = interval;
            }
            Err(error) => {
                tracing::warn!(contest_id, %error, judge = judge.key(), "the judge poll failed");
                wait = (wait * 2).min(max_wait);
            }
        }
        sleep(wait);
    }
}
