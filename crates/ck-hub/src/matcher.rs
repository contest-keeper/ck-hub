//! Decides which judge submissions came from a stick.
//!
//! First by submission id, then, for events that did not read an id, by handle, problem and time. A stick event explains at most one
//! judge submission, so one submit cannot cover two.

use serde::Serialize;
use time::Duration;

use crate::judge::{EventKey, JudgeSubmission, SubmitEvent};

/// Whether the code on the judge is the code the stick saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    Same,
    Different,
    /// The stick saw the code, but the judge's page has not been read yet (the contest is not over
    /// or the page shows no source).
    NotReadYet,
    /// The stick has no whole-source hash (it saw only a cut of the code), so there is nothing to
    /// compare.
    NotComparable,
}

/// Compares the hash the stick made of its source with the hash made of the judge's page.
#[must_use]
pub fn code_check(event: &SubmitEvent, judge_hash: Option<&str>) -> Code {
    match (event.source_sha256_norm.as_deref(), judge_hash) {
        (None, _) => Code::NotComparable,
        (Some(_), None) => Code::NotReadYet,
        (Some(stick), Some(judge)) if stick.eq_ignore_ascii_case(judge) => Code::Same,
        (Some(_), Some(_)) => Code::Different,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Came from the stick event with this key.
    Matched(EventKey),
    /// No stick reported it. This is what the Hub flags.
    Unmatched,
}

/// One verdict per judge submission, in the order given.
#[must_use]
pub fn match_submissions(
    judge: &[JudgeSubmission],
    events: &[SubmitEvent],
    window: Duration,
) -> Vec<Verdict> {
    let mut verdicts = vec![Verdict::Unmatched; judge.len()];
    let mut claimed = vec![false; events.len()];

    // By id first, so a precise match is never taken by a looser one.
    for (i, submission) in judge.iter().enumerate() {
        for (e, event) in events.iter().enumerate() {
            if !claimed[e]
                && event.submission_id == Some(submission.submission_id)
                && event.contest_id == submission.contest_id
            {
                claimed[e] = true;
                verdicts[i] = Verdict::Matched(event.key.clone());
                break;
            }
        }
    }

    // The rest by handle, problem and time, oldest submission first, closest event first. An event
    // that names a submission is for that one only: another submission close in time is not it.
    let mut rest: Vec<usize> = (0..judge.len())
        .filter(|i| verdicts[*i] == Verdict::Unmatched)
        .collect();
    rest.sort_by_key(|i| judge[*i].submitted_at);
    for i in rest {
        let submission = &judge[i];
        let mut best: Option<(usize, Duration)> = None;
        for (e, event) in events.iter().enumerate() {
            if claimed[e]
                || event.submission_id.is_some()
                || event.contest_id != submission.contest_id
                || event.problem_index != submission.problem_index
                || !submission
                    .handles
                    .iter()
                    .any(|handle| handle.eq_ignore_ascii_case(&event.handle))
            {
                continue;
            }
            let gap = (event.at - submission.submitted_at).abs();
            if gap <= window && best.is_none_or(|(_, closest)| gap < closest) {
                best = Some((e, gap));
            }
        }
        if let Some((e, _)) = best {
            claimed[e] = true;
            verdicts[i] = Verdict::Matched(events[e].key.clone());
        }
    }
    verdicts
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).expect("time")
    }

    fn judge(id: u64, handle: &str, problem: &str, seconds: i64) -> JudgeSubmission {
        JudgeSubmission {
            submission_id: id,
            contest_id: 1,
            handles: vec![handle.to_owned()],
            problem_index: problem.to_owned(),
            submitted_at: at(seconds),
        }
    }

    fn event(seq: u64, handle: &str, problem: &str, seconds: i64, id: Option<u64>) -> SubmitEvent {
        SubmitEvent {
            key: ("stick".to_owned(), "boot".to_owned(), seq),
            handle: handle.to_owned(),
            contest_id: 1,
            problem_index: problem.to_owned(),
            at: at(seconds),
            submission_id: id,
            source_sha256_norm: None,
            accepted: true,
        }
    }

    #[test]
    fn the_code_is_compared_only_when_both_sides_have_it() {
        let mut seen = event(1, "a", "A", 0, None);
        assert_eq!(code_check(&seen, Some("h")), Code::NotComparable);
        seen.source_sha256_norm = Some("AB".to_owned());
        assert_eq!(code_check(&seen, None), Code::NotReadYet);
        assert_eq!(code_check(&seen, Some("ab")), Code::Same);
        assert_eq!(code_check(&seen, Some("cd")), Code::Different);
    }

    fn key(seq: u64) -> Verdict {
        Verdict::Matched(("stick".to_owned(), "boot".to_owned(), seq))
    }

    const WINDOW: Duration = Duration::seconds(10);

    #[test]
    fn matches_by_id_even_far_apart_in_time() {
        let verdicts = match_submissions(
            &[judge(99, "a", "A", 0)],
            &[event(1, "a", "A", 600, Some(99))],
            WINDOW,
        );
        assert_eq!(verdicts, vec![key(1)]);
    }

    #[test]
    fn falls_back_to_handle_problem_and_window() {
        let verdicts = match_submissions(
            &[judge(99, "Tourist", "B", 0)],
            &[event(1, "tourist", "B", 4, None)],
            WINDOW,
        );
        assert_eq!(verdicts, vec![key(1)]);
    }

    #[test]
    fn an_event_that_names_another_submission_is_not_matched_by_time() {
        // The judge list may only lack submission 7 for now; 99 was not made on that stick.
        let verdicts = match_submissions(
            &[judge(99, "a", "A", 0)],
            &[event(1, "a", "A", 2, Some(7))],
            WINDOW,
        );
        assert_eq!(verdicts, vec![Verdict::Unmatched]);
    }

    #[test]
    fn outside_the_window_or_for_another_problem_is_unmatched() {
        let verdicts = match_submissions(
            &[judge(1, "a", "A", 0), judge(2, "a", "B", 100)],
            &[event(1, "a", "A", 11, None), event(2, "a", "C", 100, None)],
            WINDOW,
        );
        assert_eq!(verdicts, vec![Verdict::Unmatched, Verdict::Unmatched]);
    }

    #[test]
    fn one_event_cannot_explain_two_submissions() {
        let verdicts = match_submissions(
            &[judge(1, "a", "A", 0), judge(2, "a", "A", 3)],
            &[event(1, "a", "A", 1, None)],
            WINDOW,
        );
        assert_eq!(verdicts, vec![key(1), Verdict::Unmatched]);
    }

    #[test]
    fn an_id_match_is_not_taken_by_a_looser_match() {
        // Submission 1 is earlier and would take event 1 by time, but event 1 names submission 2.
        let verdicts = match_submissions(
            &[judge(1, "a", "A", 0), judge(2, "a", "A", 5)],
            &[event(1, "a", "A", 5, Some(2))],
            WINDOW,
        );
        assert_eq!(verdicts, vec![Verdict::Unmatched, key(1)]);
    }

    #[test]
    fn any_member_of_a_team_matches() {
        let mut team = judge(1, "x", "A", 0);
        team.handles.push("y".to_owned());
        let verdicts = match_submissions(&[team], &[event(1, "Y", "A", 1, None)], WINDOW);
        assert_eq!(verdicts, vec![key(1)]);
    }

    #[test]
    fn no_events_means_everything_is_unmatched() {
        assert_eq!(
            match_submissions(&[judge(1, "a", "A", 0)], &[], WINDOW),
            vec![Verdict::Unmatched]
        );
    }
}
