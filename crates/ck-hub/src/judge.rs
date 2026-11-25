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

/// Identifies a stick event: stick, boot and sequence number.
pub type EventKey = (String, String, u64);

/// A stick's report that a contestant submitted to Codeforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitEvent {
    pub key: EventKey,
    pub handle: String,
    pub contest_id: u64,
    pub problem_index: String,
    /// By the stick's clock: when it saw the submit (`submitted_at`), else when it queued the event.
    pub at: OffsetDateTime,
    pub submission_id: Option<u64>,
    /// The hash of the source as the stick saw it, after `normalize_source`. Missing when the
    /// stick saw only a cut of the code.
    pub source_sha256_norm: Option<String>,
    /// `false` when the stick saw the site refuse the form, so this submit explains no judge one.
    pub accepted: bool,
}

/// Reads a Codeforces submit event. Anything else, or a payload that lacks a field, is `None`:
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
    if text(["judge", "platform"])? != "codeforces" {
        return None;
    }
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
        handle: text(["handle", "username"])?.to_owned(),
        contest_id,
        problem_index: text(["problem_index", "problem"])?.to_owned(),
        at: payload
            .get("submitted_at")
            .and_then(serde_json::Value::as_i64)
            // A stick that could not read its clock sends 0; that is no time.
            .filter(|seconds| *seconds > 1_000_000_000)
            .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
            .unwrap_or_else(|| event.sent_at.as_offset_date_time()),
        submission_id: payload
            .get("submission_id")
            .and_then(serde_json::Value::as_u64),
        source_sha256_norm: payload
            .get("source_sha256_norm")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        accepted: payload.get("accepted").and_then(serde_json::Value::as_bool) != Some(false),
    })
}

/// The stick tells a submit at once and the judge's id for it in a second event with the same
/// `submitted_at`. This gives the id to the first and drops the second, so one submit is one event.
/// A second event whose first is not there stays: it carries the id and is a report on its own.
#[must_use]
pub fn join_follow_ups(events: Vec<SubmitEvent>) -> Vec<SubmitEvent> {
    let mut joined: Vec<SubmitEvent> = Vec::with_capacity(events.len());
    for event in events {
        let first = match event.submission_id {
            Some(_) => joined.iter_mut().find(|first| {
                first.submission_id.is_none()
                    && first.key.0 == event.key.0
                    && first.key.1 == event.key.1
                    && first.contest_id == event.contest_id
                    && first.problem_index == event.problem_index
                    && first.handle == event.handle
                    && first.at == event.at
            }),
            None => None,
        };
        match first {
            Some(first) => first.submission_id = event.submission_id,
            None => joined.push(event),
        }
    }
    joined
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
                   "problem_index": "B", "submission_id": 245_000_123_u64,
                   "source_sha256_norm": "abc"}),
        );
        let read = submit_event(&event).expect("a submit event");
        assert_eq!(read.key, ("03c9f556".to_owned(), "boot-1".to_owned(), 7));
        assert_eq!(read.handle, "tourist");
        assert_eq!(read.contest_id, 1900);
        assert_eq!(read.problem_index, "B");
        assert_eq!(read.submission_id, Some(245_000_123));
        assert_eq!(read.source_sha256_norm.as_deref(), Some("abc"));
    }

    #[test]
    fn the_time_is_when_the_stick_saw_the_submit_and_the_id_joins_the_first_event() {
        let first = submit_event(&stored(
            EventKind::Submission,
            json!({"platform": "codeforces", "username": "a", "contest_id": 1,
                   "problem": "A", "submitted_at": 1_799_999_990_u64}),
        ))
        .expect("first");
        assert_eq!(first.at.unix_timestamp(), 1_799_999_990);
        let mut follow = first.clone();
        follow.key.2 = 8;
        follow.submission_id = Some(55);
        let joined = join_follow_ups(vec![first.clone(), follow.clone()]);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].submission_id, Some(55));
        assert_eq!(joined[0].key, first.key);
        // Another time, or no first event: the second stays as it is.
        follow.at = first.at + time::Duration::seconds(5);
        assert_eq!(join_follow_ups(vec![first, follow]).len(), 2);
        // A clock that read 0 is no time.
        let none = submit_event(&stored(
            EventKind::Submission,
            json!({"platform": "codeforces", "username": "a", "contest_id": 1,
                   "problem": "A", "submitted_at": 0}),
        ))
        .expect("event");
        assert_eq!(none.at.unix_timestamp(), 1_800_000_000);
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
