//! Every judge submission of one contest, each with the stick that explains it.
//!
//! The same comparison as the page of one stick, but across all sticks of the organization: a judge
//! submission is a suspect when no stick reported it, or when a stick did but its time is not the
//! judge's.

use std::collections::HashMap;

use serde::Serialize;
use time::{Duration, OffsetDateTime};

use crate::judge::{EventKey, JudgeSubmission, SubmitEvent};
use crate::matcher::{Verdict, match_submissions};
use crate::submissions::WINDOW;

#[derive(Debug, Serialize, PartialEq)]
pub struct ContestView {
    pub contest_id: u64,
    /// Where the judge's pages are; the Hub sets it, the page builds its links from it.
    pub site: &'static str,
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
    /// A stick reported it, but its time is further from the judge's than the window.
    pub time_off: bool,
    pub suspect: bool,
}

/// `judge` and `events` are those of one contest, `events` without the submits the stick saw
/// refused.
#[must_use]
pub fn view(contest_id: u64, judge: &[JudgeSubmission], events: &[SubmitEvent]) -> ContestView {
    let verdicts = match_submissions(judge, events, WINDOW);
    let by_key: HashMap<&EventKey, &SubmitEvent> =
        events.iter().map(|event| (&event.key, event)).collect();
    let mut rows = Vec::with_capacity(judge.len());
    for (submission, verdict) in judge.iter().zip(&verdicts) {
        let explained = match verdict {
            Verdict::Matched(key) => by_key.get(key).copied(),
            Verdict::Unmatched => None,
        };
        let gap = explained.map(|event| event.at - submission.submitted_at);
        let time_gap_s = gap.map(Duration::whole_seconds);
        // An id that matches does not excuse a time that does not: both are checked, on the whole
        // duration, as the matcher does, not on the seconds shown.
        let time_off = gap.is_some_and(|gap| gap.abs() > WINDOW);
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
            time_gap_s,
            time_off,
            suspect: explained.is_none() || time_off,
        });
    }
    rows.sort_by_key(|row| std::cmp::Reverse((row.submitted_at, row.submission_id)));
    ContestView {
        contest_id,
        site: "",
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

    fn event(stick: &str, handle: &str, seconds: i64, id: Option<u64>) -> SubmitEvent {
        SubmitEvent {
            judge: crate::judge::JudgeKind::Codeforces,
            key: (stick.to_owned(), "boot".to_owned(), 1),
            handle: handle.to_owned(),
            contest_id: 1,
            problem_index: "A".to_owned(),
            at: at(seconds),
            submission_id: id,
            accepted: true,
        }
    }

    #[test]
    fn id_time_and_a_stranger_are_told_apart() {
        let submissions = [
            judge(1, "ana", 0),   // by id, same time
            judge(2, "bob", 100), // by time, 3 s apart
            judge(3, "cy", 200),  // no stick reported it: suspect
            judge(4, "dan", 300), // by id, but the stick's time is 5 minutes off: suspect
        ];
        let events = [
            event("s1", "ana", 0, Some(1)),
            event("s2", "bob", 103, None),
            event("s3", "dan", 600, Some(4)),
        ];
        let view = view(1, &submissions, &events);

        let row = |id: u64| {
            view.submissions
                .iter()
                .find(|r| r.submission_id == id)
                .unwrap()
        };
        assert_eq!(row(1).matched_by, Some(MatchedBy::Id));
        assert!(!row(1).suspect);
        assert_eq!(row(2).matched_by, Some(MatchedBy::Time));
        assert_eq!(row(2).time_gap_s, Some(3));
        assert!(!row(2).suspect);
        assert_eq!(row(3).stick_id, None);
        assert!(row(3).suspect);
        assert_eq!(row(4).matched_by, Some(MatchedBy::Id));
        assert!(row(4).time_off && row(4).suspect);
        assert_eq!(view.matched, 3);
        assert_eq!(view.suspect, 2);
        assert_eq!(view.submissions[0].submission_id, 4, "newest first");
    }

    #[test]
    fn a_gap_of_half_a_second_over_the_window_is_off_though_it_shows_as_the_window() {
        let mut seen = event("s1", "ana", 130, Some(1));
        seen.at += Duration::milliseconds(500);
        let view = view(1, &[judge(1, "ana", 100)], &[seen]);
        assert_eq!(view.submissions[0].time_gap_s, Some(30));
        assert!(view.submissions[0].time_off);
    }
}
