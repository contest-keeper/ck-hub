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

/// What one pass over a contest's pages needs.
struct Scan<'a> {
    client: &'a dyn Pages,
    store: &'a dyn Store,
    organization: &'a str,
    judge: JudgeKind,
    contest_id: u64,
    page_size: u64,
    pause: Duration,
    /// Read to the oldest page even when a page has nothing new.
    full: bool,
}

fn scan(job: &Scan) -> Result<usize, WatchError> {
    let mut total = 0;
    let mut from = 1;
    loop {
        let page = job.client.page(job.contest_id, from, job.page_size)?;
        let added = job.store.add_judge_submissions(
            job.organization,
            job.judge.key(),
            &page.rows,
            OffsetDateTime::now_utc(),
        )?;
        total += added;
        let short = page.raw < usize::try_from(job.page_size).unwrap_or(usize::MAX);
        // A page of usable rows that the Hub already has marks where an earlier poll got to. A page
        // with no usable rows says nothing about that, so it never ends the poll.
        let known = added == 0 && !page.rows.is_empty();
        if short || (known && !job.full) {
            return Ok(total);
        }
        from += job.page_size;
        sleep(job.pause);
    }
}

/// Reads pages from the newest submission backwards and keeps them, until a page has nothing the
/// Hub did not already have, or is short (the oldest page). Returns how many were new.
///
/// A failed page stops the poll with an error: what was kept stays, and the next poll starts
/// again from the newest page. After a failure use [`backfill`] once, because a page that is
/// already kept would end this poll before the pages that failed. `pause` sits between pages
/// because Codeforces allows about one call every two seconds.
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
    scan(&Scan {
        client,
        store,
        organization,
        judge,
        contest_id,
        page_size,
        pause,
        full: false,
    })
}

/// [`poll_once`] that reads every page to the oldest one, for the first poll and for the one after
/// a failure, when pages the Hub has may sit in front of pages it does not.
///
/// # Errors
/// The judge or the store failing.
pub fn backfill(
    client: &dyn Pages,
    store: &dyn Store,
    organization: &str,
    judge: JudgeKind,
    contest_id: u64,
    page_size: u64,
    pause: Duration,
) -> Result<usize, WatchError> {
    scan(&Scan {
        client,
        store,
        organization,
        judge,
        contest_id,
        page_size,
        pause,
        full: true,
    })
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
    // The first poll, and the one after a failure, read every page.
    let mut caught_up = false;
    loop {
        let poll = if caught_up { poll_once } else { backfill };
        match poll(
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
                caught_up = true;
            }
            Err(error) => {
                tracing::warn!(contest_id, %error, judge = judge.key(), "the judge poll failed");
                wait = (wait * 2).min(max_wait);
                caught_up = false;
            }
        }
        sleep(wait);
    }
}
