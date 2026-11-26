//! Every judge submission of one contest, each with the stick that explains it.
//!
//! The same comparison as the page of one stick, but across all sticks of the organization: a judge
//! submission is a suspect when no stick reported it, or when a stick did but the code on the judge
//! is not the code the stick saw.

use std::collections::HashMap;

use serde::Serialize;
use time::OffsetDateTime;

use crate::judge::{EventKey, JudgeSubmission, SubmitEvent};
use crate::matcher::{Code, Verdict, code_check, match_submissions};
use crate::submissions::WINDOW;

#[derive(Debug, Serialize, PartialEq)]
pub struct ContestView {
    pub contest_id: u64,
    /// Newest first.
    pub submissions: Vec<ContestRow>,
    pub matched: usize,
    pub suspect: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// The stick read the judge's id for its submission.
    Id,
    /// Same person, problem and time, because the stick did not know the id.
    Time,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ContestRow {
    pub submission_id: u64,
    pub problem: String,
    pub handles: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub submitted_at: OffsetDateTime,
    /// The stick that reported it; `None` when none did.
    pub stick_id: Option<String>,
    pub matched_by: Option<MatchedBy>,
    /// Seconds between the stick's clock and the judge's, when a stick reported it.
    pub time_gap_s: Option<i64>,
    pub code: Option<Code>,
    pub suspect: bool,
}

/// `judge` and `events` are those of one contest, `events` without the submits the stick saw
/// refused. `hashes` are the kept source hashes of the judge's submissions, by id.
#[must_use]
pub fn view(
    contest_id: u64,
    judge: &[JudgeSubmission],
    events: &[SubmitEvent],
    hashes: &HashMap<u64, String>,
) -> ContestView {
    let verdicts = match_submissions(judge, events, WINDOW);
    let by_key: HashMap<&EventKey, &SubmitEvent> =
        events.iter().map(|event| (&event.key, event)).collect();
    let mut rows = Vec::with_capacity(judge.len());
    for (submission, verdict) in judge.iter().zip(&verdicts) {
        let explained = match verdict {
            Verdict::Matched(key) => by_key.get(key).copied(),
            Verdict::Unmatched => None,
        };
        let code = explained.map(|event| {
            code_check(
                event,
                hashes.get(&submission.submission_id).map(String::as_str),
            )
        });
        rows.push(ContestRow {
            submission_id: submission.submission_id,
            problem: submission.problem_index.clone(),
            handles: submission.handles.clone(),
            submitted_at: submission.submitted_at,
            stick_id: explained.map(|event| event.key.0.clone()),
            matched_by: explained.map(|event| {
                if event.submission_id == Some(submission.submission_id) {
                    MatchedBy::Id
                } else {
                    MatchedBy::Time
                }
            }),
            time_gap_s: explained.map(|event| (event.at - submission.submitted_at).whole_seconds()),
            suspect: explained.is_none() || code == Some(Code::Different),
            code,
        });
    }
    rows.sort_by_key(|row| std::cmp::Reverse((row.submitted_at, row.submission_id)));
    ContestView {
        contest_id,
        matched: rows.iter().filter(|row| row.stick_id.is_some()).count(),
        suspect: rows.iter().filter(|row| row.suspect).count(),
        submissions: rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).expect("time")
    }

    fn judge(id: u64, handle: &str, seconds: i64) -> JudgeSubmission {
        JudgeSubmission {
            submission_id: id,
            contest_id: 1,
            handles: vec![handle.to_owned()],
            problem_index: "A".to_owned(),
            submitted_at: at(seconds),
        }
    }

    fn event(
        stick: &str,
        handle: &str,
        seconds: i64,
        id: Option<u64>,
        hash: Option<&str>,
    ) -> SubmitEvent {
        SubmitEvent {
            key: (stick.to_owned(), "boot".to_owned(), 1),
            handle: handle.to_owned(),
            contest_id: 1,
            problem_index: "A".to_owned(),
            at: at(seconds),
            submission_id: id,
            source_sha256_norm: hash.map(str::to_owned),
            accepted: true,
        }
    }

    fn judge_hashes(pairs: &[(u64, &str)]) -> HashMap<u64, String> {
        pairs
            .iter()
            .map(|(id, hash)| (*id, (*hash).to_owned()))
            .collect()
    }

    #[test]
    fn id_time_code_and_a_stranger_are_told_apart() {
        let submissions = [
            judge(1, "ana", 0),   // by id, same code
            judge(2, "bob", 100), // by time, other code: suspect
            judge(3, "cy", 200),  // no stick reported it: suspect
            judge(4, "dan", 300), // by id, stick saw a cut: cannot compare
            judge(5, "eve", 400), // by id, the judge's code is not read yet
        ];
        let events = [
            event("s1", "ana", 0, Some(1), Some("h1")),
            event("s2", "bob", 103, None, Some("hx")),
            event("s3", "dan", 300, Some(4), None),
            event("s4", "eve", 400, Some(5), Some("h5")),
        ];
        let hashes = judge_hashes(&[(1, "h1"), (2, "h2"), (4, "h4")]);
        let view = view(1, &submissions, &events, &hashes);

        let row = |id: u64| {
            view.submissions
                .iter()
                .find(|r| r.submission_id == id)
                .unwrap()
        };
        assert_eq!(row(1).matched_by, Some(MatchedBy::Id));
        assert_eq!(row(1).code, Some(Code::Same));
        assert!(!row(1).suspect);
        assert_eq!(row(2).matched_by, Some(MatchedBy::Time));
        assert_eq!(row(2).time_gap_s, Some(3));
        assert_eq!(row(2).code, Some(Code::Different));
        assert!(row(2).suspect);
        assert_eq!(row(3).stick_id, None);
        assert!(row(3).suspect);
        assert_eq!(row(4).code, Some(Code::NotComparable));
        assert!(!row(4).suspect);
        assert_eq!(row(5).code, Some(Code::NotReadYet));
        assert_eq!(view.matched, 4);
        assert_eq!(view.suspect, 2);
        assert_eq!(view.submissions[0].submission_id, 5, "newest first");
    }
}
