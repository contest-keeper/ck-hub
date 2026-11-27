//! What the Hub knows about a judge and about what sticks said they submitted.

use ck_protocol::EventKind;
use time::OffsetDateTime;

use crate::store::StoredEvent;

/// A submission as the judge reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeSubmission {
    pub submission_id: u64,
    pub contest_id: u64,
    /// Every member. A team submission has several; handles compare without regard to case.
    pub handles: Vec<String>,
    pub problem_index: String,
    pub submitted_at: OffsetDateTime,
}

/// A judge the Hub follows. Contest numbers repeat between judges, so everything that is kept or
/// compared is told apart by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JudgeKind {
    Codeforces,
    Vjudge,
}

impl JudgeKind {
    pub const ALL: [Self; 2] = [Self::Codeforces, Self::Vjudge];

    /// The word the stick puts in its events and the store keeps with a judge's submissions.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Codeforces => "codeforces",
            Self::Vjudge => "vjudge",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codeforces => "Codeforces",
            Self::Vjudge => "VJudge",
        }
    }

    /// Where the judge's pages are. The Hub page builds its links from this, so no address is
    /// written in the page itself.
    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::Codeforces => "https://codeforces.com",
            Self::Vjudge => "https://vjudge.net",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// The judge an event of a stick is about. An event without one is an old Codeforces one.
    #[must_use]
    pub fn of_payload(payload: &serde_json::Value) -> Option<Self> {
        match ["judge", "platform"]
            .iter()
            .find_map(|name| payload.get(*name).and_then(serde_json::Value::as_str))
        {
            Some(key) => Self::from_key(key),
            None => Some(Self::Codeforces),
        }
    }
}

/// Identifies a stick event: stick, boot and sequence number.
pub type EventKey = (String, String, u64);

/// A stick's report that a contestant submitted to a judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitEvent {
    pub key: EventKey,
    pub judge: JudgeKind,
    pub handle: String,
    pub contest_id: u64,
    pub problem_index: String,
    /// The judge's own time for the submit when the stick read it (`judge_time_ms`, `VJudge`), else
    /// the stick's clock.
    pub at: OffsetDateTime,
    pub submission_id: Option<u64>,
    /// `false` when the stick saw the site refuse the form, so this submit explains no judge one.
    pub accepted: bool,
}

/// Reads a submit event of a judge the Hub follows. Anything else, or a payload that lacks a field, is `None`:
/// the Hub keeps every event, but only well-formed ones can be matched.
#[must_use]
pub fn submit_event(stored: &StoredEvent) -> Option<SubmitEvent> {
    let event = &stored.event;
    if event.kind != EventKind::Submission {
        return None;
    }
    let payload = &event.payload;
    // The stick names the fields `platform`, `username` and `problem`; the plan called them
    // `judge`, `handle` and `problem_index`. Both are read.
    let text = |names: [&str; 2]| {
        names
            .iter()
            .find_map(|name| payload.get(*name).and_then(serde_json::Value::as_str))
    };
    let judge = JudgeKind::from_key(text(["judge", "platform"])?)?;
    // The contest id is a number, or a number written as text.
    let contest_id = match payload.get("contest_id")? {
        serde_json::Value::Number(number) => number.as_u64()?,
        serde_json::Value::String(text) => text.parse().ok()?,
        _ => return None,
    };
    Some(SubmitEvent {
        key: (
            event.stick_id.as_str().to_owned(),
            event.boot_id.clone(),
            event.seq,
        ),
        judge,
        handle: text(["handle", "username"])?.to_owned(),
        contest_id,
        problem_index: text(["problem_index", "problem"])?.to_owned(),
        at: payload
            .get("judge_time_ms")
            .and_then(serde_json::Value::as_i64)
            // Milliseconds since 1970; a smaller number is not a time.
            .filter(|ms| *ms > 1_000_000_000_000)
            .and_then(|ms| OffsetDateTime::from_unix_timestamp(ms / 1000).ok())
            .unwrap_or_else(|| event.sent_at.as_offset_date_time()),
        submission_id: payload
            .get("submission_id")
            .and_then(serde_json::Value::as_u64),
        accepted: payload.get("accepted").and_then(serde_json::Value::as_bool) != Some(false),
    })
}

#[cfg(test)]
mod tests {
    use ck_protocol::{Event, FIRST_PREV_HASH, StickId};
    use serde_json::{Value, json};

    use super::*;

    fn stored(kind: EventKind, payload: Value) -> StoredEvent {
        StoredEvent {
            received_at: OffsetDateTime::from_unix_timestamp(1_800_000_100).expect("time"),
            integrity: None,
            event: Event {
                stick_id: StickId::parse("03c9f556").expect("id"),
                boot_id: "boot-1".to_owned(),
                seq: 7,
                prev_hash: FIRST_PREV_HASH.to_owned(),
                sent_at: OffsetDateTime::from_unix_timestamp(1_800_000_000)
                    .expect("time")
                    .into(),
                kind,
                payload,
            },
        }
    }

    #[test]
    fn reads_a_full_submission() {
        let event = stored(
            EventKind::Submission,
            json!({"judge": "codeforces", "handle": "tourist", "contest_id": 1900,
                   "problem_index": "B", "submission_id": 245_000_123_u64}),
        );
        let read = submit_event(&event).expect("a submit event");
        assert_eq!(read.key, ("03c9f556".to_owned(), "boot-1".to_owned(), 7));
        assert_eq!(read.handle, "tourist");
        assert_eq!(read.contest_id, 1900);
        assert_eq!(read.problem_index, "B");
        assert_eq!(read.submission_id, Some(245_000_123));
    }

    #[test]
    fn reads_a_vjudge_submit_with_the_judges_own_time() {
        let event = stored(
            EventKind::Submission,
            json!({"platform": "vjudge", "username": "robbertp", "contest_id": "871412",
                   "problem": "C", "submission_id": 74_477_117_u64,
                   "judge_time_ms": 1_795_789_024_000_i64}),
        );
        let read = submit_event(&event).expect("a submit event");
        assert_eq!(read.judge, JudgeKind::Vjudge);
        assert_eq!(read.contest_id, 871_412);
        assert_eq!(read.submission_id, Some(74_477_117));
        assert_eq!(read.at.unix_timestamp(), 1_795_789_024);
        // Without the judge's time, or with one that is no time, the stick's clock is used.
        let plain = stored(
            EventKind::Submission,
            json!({"platform": "vjudge", "username": "a", "contest_id": 1, "problem": "A",
                   "judge_time_ms": 5}),
        );
        assert_eq!(
            submit_event(&plain).expect("event").at.unix_timestamp(),
            1_800_000_000
        );
    }

    #[test]
    fn ignores_logins_other_judges_and_incomplete_payloads() {
        let login = stored(
            EventKind::Login,
            json!({"judge": "codeforces", "handle": "a"}),
        );
        assert!(submit_event(&login).is_none());
        let other = stored(
            EventKind::Submission,
            json!({"judge": "kilonova", "handle": "a", "contest_id": 1, "problem_index": "A"}),
        );
        assert!(submit_event(&other).is_none());
        let incomplete = stored(
            EventKind::Submission,
            json!({"judge": "codeforces", "handle": "a"}),
        );
        assert!(submit_event(&incomplete).is_none());
    }
}
