//! Follows a contest on the judge and keeps what it reports.

use std::thread::sleep;
use std::time::Duration;

use time::OffsetDateTime;

use crate::codeforces::{Client, CodeforcesError, SourceJudge};
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
        let added = store.add_judge_submissions(
            organization,
            "codeforces",
            &page,
            OffsetDateTime::now_utc(),
        )?;
        total += added;
        let short = page.len() < usize::try_from(page_size).unwrap_or(usize::MAX);
        if added == 0 || short {
            return Ok(total);
        }
        from += page_size;
        sleep(pause);
    }
}

/// Once the contest is over, reads the source of each kept submission that has no hash yet from the
/// judge's page and keeps its hash, at most `limit` per call. Returns how many were kept. Before the
/// end of the contest nothing is read: the code is not public then.
///
/// A submission whose page shows no source is tried again on the next call. A failure stops the pass
/// with an error; what was kept stays.
///
/// # Errors
/// The judge or the store failing.
pub fn hash_sources(
    judge: &dyn SourceJudge,
    store: &dyn Store,
    organization: &str,
    contest_id: u64,
    limit: usize,
    pause: Duration,
) -> Result<usize, WatchError> {
    if judge.phase(contest_id)? != "FINISHED" {
        return Ok(0);
    }
    let known = store.judge_source_hashes(organization, "codeforces", contest_id)?;
    let waiting = store
        .judge_submissions(organization, "codeforces", contest_id)?
        .into_iter()
        .filter(|submission| !known.contains_key(&submission.submission_id))
        .take(limit);
    let mut kept = 0;
    for submission in waiting {
        sleep(pause);
        if let Some(hash) = judge.source_hash(contest_id, submission.submission_id)? {
            store.add_judge_source_hash(
                organization,
                "codeforces",
                contest_id,
                submission.submission_id,
                &hash,
                OffsetDateTime::now_utc(),
            )?;
            kept += 1;
        }
    }
    Ok(kept)
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
        let pause = Duration::from_secs(2);
        let result =
            poll_once(client, store, organization, contest_id, 100, pause).and_then(|new| {
                // `shortcut:` 50 pages per round so the next poll is not far behind; raise it if a
                // big contest takes too many rounds to hash.
                let hashed = hash_sources(client, store, organization, contest_id, 50, pause)?;
                Ok((new, hashed))
            });
        match result {
            Ok((new, hashed)) => {
                tracing::info!(contest_id, new, hashed, "polled codeforces");
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::judge::JudgeSubmission;
    use crate::store::MemoryStore;

    struct FakeJudge {
        phase: &'static str,
        /// The submission whose page fails to load, if any.
        broken: Option<u64>,
        /// The submission whose page shows no source, if any.
        hidden: Option<u64>,
        calls: Cell<usize>,
    }

    impl SourceJudge for FakeJudge {
        fn phase(&self, _: u64) -> Result<String, CodeforcesError> {
            Ok(self.phase.to_owned())
        }

        fn source_hash(&self, _: u64, id: u64) -> Result<Option<String>, CodeforcesError> {
            self.calls.set(self.calls.get() + 1);
            if self.broken == Some(id) {
                return Err(CodeforcesError::Request("down".to_owned()));
            }
            Ok((self.hidden != Some(id)).then(|| format!("hash-{id}")))
        }
    }

    fn judge(phase: &'static str, broken: Option<u64>, hidden: Option<u64>) -> FakeJudge {
        FakeJudge {
            phase,
            broken,
            hidden,
            calls: Cell::new(0),
        }
    }

    fn store_with(ids: &[u64]) -> MemoryStore {
        let store = MemoryStore::default();
        let at = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time");
        let submissions: Vec<JudgeSubmission> = ids
            .iter()
            .map(|id| JudgeSubmission {
                submission_id: *id,
                contest_id: 7,
                handles: vec!["a".to_owned()],
                problem_index: "A".to_owned(),
                submitted_at: at,
            })
            .collect();
        store
            .add_judge_submissions("org", "codeforces", &submissions, at)
            .expect("kept");
        store
    }

    fn run_pass(judge: &FakeJudge, store: &MemoryStore) -> Result<usize, WatchError> {
        hash_sources(judge, store, "org", 7, 50, Duration::ZERO)
    }

    #[test]
    fn nothing_is_read_before_the_contest_is_over() {
        let store = store_with(&[1, 2]);
        let judge = judge("CODING", None, None);
        assert_eq!(run_pass(&judge, &store).expect("pass"), 0);
        assert_eq!(judge.calls.get(), 0);
    }

    #[test]
    fn a_finished_contest_is_hashed_once_and_a_hidden_page_is_tried_again() {
        let store = store_with(&[1, 2, 3]);
        let hidden = judge("FINISHED", None, Some(2));
        assert_eq!(run_pass(&hidden, &store).expect("pass"), 2);
        let kept = store
            .judge_source_hashes("org", "codeforces", 7)
            .expect("hashes");
        assert_eq!(kept.get(&1).map(String::as_str), Some("hash-1"));
        assert!(!kept.contains_key(&2));

        // The second pass asks only about the submission that had no source.
        let shown = judge("FINISHED", None, None);
        assert_eq!(run_pass(&shown, &store).expect("pass"), 1);
        assert_eq!(shown.calls.get(), 1);
    }

    #[test]
    fn a_failed_page_stops_the_pass_and_keeps_what_was_read() {
        let store = store_with(&[1, 2, 3]);
        let failing = judge("FINISHED", Some(2), None);
        assert!(run_pass(&failing, &store).is_err());
        let kept = store
            .judge_source_hashes("org", "codeforces", 7)
            .expect("hashes");
        assert_eq!(kept.len(), 1);

        let healed = judge("FINISHED", None, None);
        assert_eq!(run_pass(&healed, &store).expect("pass"), 2);
    }
}
