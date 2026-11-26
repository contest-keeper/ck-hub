//! The submissions of a stick, and whether the judge saw the same ones.
//!
//! The stick reports each submit it saw. The judge is asked for what the people who logged in on
//! the stick submitted in the contests the stick took part in. A judge submission that no stick
//! event explains is a suspect: it was made from somewhere else with the same account.

use std::collections::HashMap;

use ck_protocol::EventKind;
use serde::Serialize;
use serde_json::Value;
use time::{Duration, OffsetDateTime};

use crate::codeforces::CodeforcesError;
use crate::judge::{EventKey, JudgeSubmission, SubmitEvent, submit_event};
use crate::matcher::{Verdict, match_submissions};
use crate::store::StoredEvent;

/// How far apart the stick's report and the judge's clock may be for the same submit.
pub(crate) const WINDOW: Duration = Duration::seconds(30);
/// The answer of a kept-rows `Ask` for a contest no watcher reads.
pub(crate) const NOTHING_KEPT: &str = "no judge submissions are kept for the contest";
/// How long the judge has to show a submit the stick reported: its pages and the watcher's polls
/// are not instant, so a fresh submit is not yet a problem.
pub(crate) const GRACE: Duration = Duration::seconds(120);
/// The judge allows about one call every two seconds, so only the latest contests and people are
/// asked about.
const MAX_CONTESTS: usize = 2;
const MAX_HANDLES: usize = 2;
const MAX_ROWS: usize = 200;

#[derive(Debug, Serialize, PartialEq)]
pub struct SubmissionsView {
    /// What the stick reported, newest first.
    pub submissions: Vec<StickSubmission>,
    /// What the judge says these people submitted since they logged in on the stick, newest first.
    pub judge: Vec<JudgeRow>,
    /// Judge submissions that are a problem: the stick did not report them, the code differs, or
    /// the time is off.
    pub suspect: usize,
    /// Submissions the stick reported and the judge never showed.
    pub not_on_judge: usize,
    pub contests: Vec<u64>,
    pub handles: Vec<String>,
    /// Why the judge's side is missing or incomplete, when it is.
    pub judge_note: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct StickSubmission {
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
    pub handle: Option<String>,
    pub contest_id: Option<u64>,
    pub problem: Option<String>,
    pub language: Option<String>,
    /// The start of the source's hash: enough to tell two sources apart, not to read one.
    pub source_sha256: Option<String>,
    pub source_bytes: Option<u64>,
    pub source_cut: bool,
    /// `false` when the stick saw the form refused, for example while the site asked for a check.
    pub accepted: bool,
    /// The judge submission that is this one, when the judge has it.
    pub judge_id: Option<u64>,
    /// The judge was asked and does not show it, long enough after the submit.
    pub not_on_judge: bool,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct JudgeRow {
    pub submission_id: u64,
    pub contest_id: u64,
    pub problem: String,
    pub handles: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub submitted_at: OffsetDateTime,
    pub from_stick: bool,
    /// Seconds between the stick's clock and the judge's, when a stick reported it.
    pub time_gap_s: Option<i64>,
    /// A stick reported it, but its time is further from the judge's than [`WINDOW`].
    pub time_off: bool,
    pub suspect: bool,
}

/// Handles are letters, digits and a few signs. Anything else is not asked about or linked to.
fn is_handle(text: &str) -> bool {
    (1..=24).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn text(payload: &Value, names: [&str; 2]) -> Option<String> {
    names
        .iter()
        .find_map(|name| payload.get(*name).and_then(Value::as_str))
        .map(|value| value.chars().take(64).collect())
}

fn contest_id(payload: &Value) -> Option<u64> {
    match payload.get("contest_id")? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// The contest in a Codeforces path like `/contest/2285/problem/A`.
fn contest_of_path(path: &str) -> Option<u64> {
    let rest = path.strip_prefix("/contest/")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn stick_submission(
    item: &StoredEvent,
    judge_id: Option<u64>,
    not_on_judge: bool,
) -> StickSubmission {
    let payload = &item.event.payload;
    StickSubmission {
        received_at: item.received_at,
        handle: text(payload, ["handle", "username"]),
        contest_id: contest_id(payload),
        problem: text(payload, ["problem_index", "problem"]),
        language: text(payload, ["language", "language"]),
        source_sha256: text(payload, ["source_sha256", "source_sha256"])
            .map(|hash| hash.chars().take(12).collect()),
        source_bytes: payload.get("source_bytes").and_then(Value::as_u64),
        source_cut: payload.get("source_cut").and_then(Value::as_bool) == Some(true),
        accepted: payload.get("accepted").and_then(Value::as_bool) != Some(false),
        judge_id,
        not_on_judge,
    }
}

/// Who logged in on the stick, with when they first did, and the contests it was used in, the most
/// recent last.
fn who_and_where(stored: &[StoredEvent]) -> (Vec<(String, OffsetDateTime)>, Vec<u64>) {
    let mut handles: Vec<(String, OffsetDateTime)> = Vec::new();
    let mut contests: Vec<u64> = Vec::new();
    let mut seen = |contest: u64| {
        contests.retain(|known| *known != contest);
        contests.push(contest);
    };
    for item in stored {
        let payload = &item.event.payload;
        match item.event.kind {
            EventKind::Login | EventKind::Submission => {
                if let Some(handle) = text(payload, ["handle", "username"])
                    && is_handle(&handle)
                {
                    match handles.iter_mut().find(|(known, _)| *known == handle) {
                        // The first time a person was seen is what later submissions are compared from.
                        Some(entry) => entry.1 = entry.1.min(item.received_at),
                        None => handles.push((handle, item.received_at)),
                    }
                }
                if item.event.kind == EventKind::Submission
                    && let Some(contest) = contest_id(payload)
                {
                    seen(contest);
                }
            }
            EventKind::Network => {
                let Some(entries) = payload.get("entries").and_then(Value::as_array) else {
                    continue;
                };
                for entry in entries {
                    let host = entry.get("host").and_then(Value::as_str).unwrap_or("");
                    let path = entry.get("path").and_then(Value::as_str).unwrap_or("");
                    if host.starts_with("codeforces.com")
                        && let Some(contest) = contest_of_path(path)
                    {
                        seen(contest);
                    }
                }
            }
            _ => {}
        }
    }
    (handles, contests)
}

/// Asks the judge for the submissions of a person in a contest.
pub type Ask<'a> = dyn Fn(u64, &str) -> Result<Vec<JudgeSubmission>, CodeforcesError> + 'a;

/// What the judge was asked and answered for: the contests, the people and when.
#[derive(Clone, Copy)]
struct Asked<'a> {
    contests: &'a [u64],
    handles: &'a [(String, OffsetDateTime)],
    now: OffsetDateTime,
}

/// The submissions the stick reported, one per submit. `asked` is what the judge was asked and
/// answered for; a submit is "not on the judge" only inside it.
fn stick_submissions(
    stored: &[StoredEvent],
    events: &[SubmitEvent],
    explained: &HashMap<EventKey, u64>,
    asked: Option<Asked>,
) -> Vec<StickSubmission> {
    let mut submissions = Vec::new();
    for item in stored
        .iter()
        .filter(|s| s.event.kind == EventKind::Submission)
    {
        let key = (
            item.event.stick_id.as_str().to_owned(),
            item.event.boot_id.clone(),
            item.event.seq,
        );
        // An event the Hub cannot read as a submit (no contest, say) is listed without a verdict.
        let Some(event) = events.iter().find(|event| event.key == key) else {
            if submit_event(item).is_none() {
                submissions.push(stick_submission(item, None, false));
            }
            continue;
        };
        let judge_id = explained.get(&key).copied();
        let not_on_judge = judge_id.is_none()
            && event.accepted
            && asked.is_some_and(|asked| {
                asked.contests.contains(&event.contest_id)
                    && asked
                        .handles
                        .iter()
                        .any(|(handle, _)| handle.eq_ignore_ascii_case(&event.handle))
                    && asked.now - event.at > WINDOW + GRACE
            });
        submissions.push(stick_submission(item, judge_id, not_on_judge));
    }
    submissions
}

/// Asks the judge for each person in each contest. The note says why the answer is missing or
/// incomplete, when it is.
fn ask_judge(
    ask: Option<&Ask>,
    handles: &[(String, OffsetDateTime)],
    contests: &[u64],
) -> (Vec<JudgeSubmission>, Option<String>) {
    let mut note: Option<String> = None;
    let mut judged: Vec<JudgeSubmission> = Vec::new();
    match ask {
        None => {
            note = Some("The judge is not set up on this Hub, so nothing is compared.".to_owned());
        }
        Some(_) if handles.is_empty() => {
            note = Some("No one has logged in on this stick yet.".to_owned());
        }
        Some(_) if contests.is_empty() => {
            note = Some(
                "No contest is known for this stick yet: it has not opened or submitted to one."
                    .to_owned(),
            );
        }
        Some(ask) => {
            for contest in contests {
                for (handle, since) in handles {
                    match ask(*contest, handle) {
                        Ok(found) => {
                            // Only what was submitted after the person logged in on this stick.
                            let from = *since - Duration::seconds(120);
                            judged.extend(found.into_iter().filter(|s| {
                                s.submitted_at >= from
                                    && s.handles.iter().any(|h| h.eq_ignore_ascii_case(handle))
                            }));
                        }
                        Err(error) => {
                            // The stick list asks for every stick at once; no watcher is not news.
                            if !matches!(&error, CodeforcesError::Request(why) if why == NOTHING_KEPT)
                            {
                                tracing::warn!(contest, %error, "the judge could not be asked");
                            }
                            note = Some("Codeforces could not be asked just now.".to_owned());
                        }
                    }
                }
            }
        }
    }
    (judged, note)
}

/// What the stick reported and what the judge says, side by side. `ask` gets the submissions of a
/// person in a contest from the judge; `None` when no judge is set up.
pub fn view(stored: &[StoredEvent], ask: Option<&Ask>, now: OffsetDateTime) -> SubmissionsView {
    let (mut handles, mut contests) = who_and_where(stored);
    // The latest few, newest people and contests first.
    handles.reverse();
    handles.truncate(MAX_HANDLES);
    contests.reverse();
    contests.truncate(MAX_CONTESTS);

    let (mut judged, note) = ask_judge(ask, &handles, &contests);
    judged.sort_by_key(|s| (s.contest_id, s.submission_id));
    judged.dedup_by_key(|s| (s.contest_id, s.submission_id));

    let all_events: Vec<SubmitEvent> = stored.iter().filter_map(submit_event).collect();
    // A submission the stick saw refused cannot explain one on the judge.
    let events: Vec<SubmitEvent> = all_events
        .iter()
        .filter(|event| event.accepted)
        .cloned()
        .collect();
    let verdicts = match_submissions(&judged, &events, WINDOW);
    let by_key: HashMap<&EventKey, &SubmitEvent> =
        events.iter().map(|event| (&event.key, event)).collect();
    let mut explained: HashMap<EventKey, u64> = HashMap::new();
    let mut rows: Vec<JudgeRow> = Vec::new();
    for (submission, verdict) in judged.iter().zip(&verdicts) {
        if let Verdict::Matched(key) = verdict {
            explained.insert(key.clone(), submission.submission_id);
        }
        let from_stick = matches!(verdict, Verdict::Matched(_));
        let time_gap_s = match verdict {
            Verdict::Matched(key) => by_key
                .get(key)
                .map(|event| (event.at - submission.submitted_at).whole_seconds()),
            Verdict::Unmatched => None,
        };
        // An id that matches does not excuse a time that does not: both are checked.
        let time_off = time_gap_s.is_some_and(|gap| gap.abs() > WINDOW.whole_seconds());
        rows.push(JudgeRow {
            submission_id: submission.submission_id,
            contest_id: submission.contest_id,
            problem: submission.problem_index.clone(),
            handles: submission.handles.clone(),
            submitted_at: submission.submitted_at,
            from_stick,
            time_gap_s,
            time_off,
            suspect: !from_stick || time_off,
        });
    }
    let suspect = rows.iter().filter(|r| r.suspect).count();
    rows.sort_by_key(|row| std::cmp::Reverse(row.submitted_at));
    rows.truncate(MAX_ROWS);

    let judge_answered = ask.is_some() && note.is_none();
    let mut submissions = stick_submissions(
        stored,
        &all_events,
        &explained,
        judge_answered.then_some(Asked {
            contests: &contests,
            handles: &handles,
            now,
        }),
    );
    let not_on_judge = submissions.iter().filter(|s| s.not_on_judge).count();
    submissions.reverse();
    submissions.truncate(MAX_ROWS);

    SubmissionsView {
        submissions,
        judge: rows,
        suspect,
        not_on_judge,
        contests,
        handles: handles.into_iter().map(|(handle, _)| handle).collect(),
        judge_note: note,
    }
}

#[cfg(test)]
mod tests {
    use ck_protocol::{Event, FIRST_PREV_HASH, StickId};
    use serde_json::json;

    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).expect("time")
    }

    fn stored(seq: u64, seconds: i64, kind: EventKind, payload: Value) -> StoredEvent {
        StoredEvent {
            received_at: at(seconds),
            integrity: None,
            event: Event {
                stick_id: StickId::parse("03c9f556").expect("id"),
                boot_id: "boot-1".to_owned(),
                seq,
                prev_hash: FIRST_PREV_HASH.to_owned(),
                sent_at: at(seconds).into(),
                kind,
                payload,
            },
        }
    }

    fn login(seconds: i64) -> StoredEvent {
        stored(
            1,
            seconds,
            EventKind::Login,
            json!({"platform": "codeforces", "username": "ana"}),
        )
    }

    fn submit(seq: u64, seconds: i64, problem: &str, accepted: bool) -> StoredEvent {
        stored(
            seq,
            seconds,
            EventKind::Submission,
            json!({"platform": "codeforces", "username": "ana", "contest_id": "2285",
                   "problem": problem, "language": "91", "source_sha256": "abcdef0123456789abcdef",
                   "source_bytes": 120, "accepted": accepted}),
        )
    }

    fn judged(id: u64, problem: &str, seconds: i64) -> JudgeSubmission {
        JudgeSubmission {
            submission_id: id,
            contest_id: 2285,
            handles: vec!["Ana".to_owned()],
            problem_index: problem.to_owned(),
            submitted_at: at(seconds),
        }
    }

    #[test]
    fn a_judge_submission_with_no_stick_report_is_a_suspect_and_a_matched_one_is_not() {
        let stored = vec![
            login(0),
            submit(2, 100, "A", true),
            submit(3, 110, "A", false),
        ];
        let ask = |contest: u64, handle: &str| {
            assert_eq!((contest, handle), (2285, "ana"));
            Ok(vec![
                judged(30, "C", 400),
                judged(20, "A", 102),
                judged(5, "A", -3600),
            ])
        };
        let seen = view(&stored, Some(&ask), at(10_000));
        assert_eq!(seen.handles, ["ana"]);
        assert_eq!(seen.contests, [2285]);
        assert_eq!(
            seen.judge.len(),
            2,
            "what came before the login is not compared"
        );
        assert_eq!(seen.judge[0].submission_id, 30);
        assert!(seen.judge[0].suspect);
        assert_eq!(seen.judge[1].submission_id, 20);
        assert!(seen.judge[1].from_stick);
        assert_eq!(seen.suspect, 1);
        assert_eq!(seen.judge_note, None);

        // The stick's list: newest first, the refused one has no judge counterpart.
        assert_eq!(seen.submissions.len(), 2);
        assert!(!seen.submissions[0].accepted);
        assert_eq!(seen.submissions[0].judge_id, None);
        assert_eq!(seen.submissions[1].judge_id, Some(20));
        assert_eq!(
            seen.submissions[1].source_sha256.as_deref(),
            Some("abcdef012345")
        );
        assert_eq!(seen.submissions[1].contest_id, Some(2285));
    }

    fn submit_with_id(seq: u64, seconds: i64, problem: &str, id: Option<u64>) -> StoredEvent {
        stored(
            seq,
            seconds,
            EventKind::Submission,
            json!({"platform": "codeforces", "username": "ana", "contest_id": "2285",
                   "problem": problem, "accepted": true, "submission_id": id}),
        )
    }

    #[test]
    fn an_id_that_matches_with_a_time_that_does_not_is_a_problem() {
        let stored = vec![login(0), submit_with_id(2, 100, "A", Some(20))];
        let ask = |_: u64, _: &str| Ok(vec![judged(20, "A", 100 + 300)]);
        let seen = view(&stored, Some(&ask), at(10_000));
        assert!(seen.judge[0].from_stick, "the id matched");
        assert!(seen.judge[0].time_off);
        assert_eq!(seen.judge[0].time_gap_s, Some(-300));
        assert!(seen.judge[0].suspect);
        assert_eq!(seen.suspect, 1);
    }

    #[test]
    fn a_submit_the_judge_never_shows_is_a_problem_only_after_the_grace() {
        let stored = vec![login(0), submit_with_id(2, 100, "A", None)];
        let ask = |_: u64, _: &str| Ok(vec![]);
        let fresh = view(&stored, Some(&ask), at(100 + 60));
        assert_eq!(fresh.not_on_judge, 0, "the judge has had no time yet");
        let late = view(&stored, Some(&ask), at(10_000));
        assert_eq!(late.not_on_judge, 1);
        assert!(late.submissions[0].not_on_judge);
        // When the judge could not be asked, nothing is claimed about it.
        let down = |_: u64, _: &str| Err(CodeforcesError::Request(NOTHING_KEPT.to_owned()));
        let unknown = view(&stored, Some(&down), at(10_000));
        assert_eq!(unknown.not_on_judge, 0);
    }

    #[test]
    fn the_contest_is_found_in_the_pages_opened_and_a_login_alone_is_enough_to_ask() {
        let pages = stored(
            2,
            50,
            EventKind::Network,
            json!({"entries": [{"host": "codeforces.com:443", "path": "/contest/1900/problem/B"},
                               {"host": "example.org", "path": "/contest/77/x"}]}),
        );
        let ask = |contest: u64, _: &str| {
            assert_eq!(contest, 1900);
            Ok(vec![])
        };
        let seen = view(&[login(0), pages], Some(&ask), at(10_000));
        assert_eq!(seen.contests, [1900]);
        assert_eq!(seen.submissions.len(), 0);
    }

    #[test]
    fn it_says_why_nothing_is_compared() {
        let none = view(&[login(0)], None, at(10_000));
        assert!(none.judge_note.expect("a note").contains("not set up"));
        let ask = |_: u64, _: &str| Ok(vec![]);
        let nobody = view(&[], Some(&ask), at(10_000));
        assert!(
            nobody
                .judge_note
                .expect("a note")
                .contains("No one has logged in")
        );
        let no_contest = view(&[login(0)], Some(&ask), at(10_000));
        assert!(
            no_contest
                .judge_note
                .expect("a note")
                .contains("No contest")
        );
        let failing = |_: u64, _: &str| Err(CodeforcesError::Request("down".to_owned()));
        let down = view(
            &[login(0), submit(2, 5, "A", true)],
            Some(&failing),
            at(10_000),
        );
        assert!(
            down.judge_note
                .expect("a note")
                .contains("could not be asked")
        );
        assert_eq!(down.submissions.len(), 1);
    }

    #[test]
    fn only_plain_handles_are_asked_about() {
        let odd = stored(1, 0, EventKind::Login, json!({"username": "a b&c=d"}));
        let ask = |_: u64, _: &str| -> Result<Vec<JudgeSubmission>, CodeforcesError> {
            panic!("must not be asked")
        };
        let seen = view(&[odd], Some(&ask), at(10_000));
        assert_eq!(seen.handles.len(), 0);
    }
}
