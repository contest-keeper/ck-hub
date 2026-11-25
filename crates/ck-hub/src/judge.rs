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
    /// By the stick's clock.
    pub at: OffsetDateTime,
    pub submission_id: Option<u64>,
    /// The hash of the source as the stick saw it, after `normalize_source`. Missing when the
    /// stick saw only a cut of the code.
    pub source_sha256_norm: Option<String>,
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
        at: event.sent_at.as_offset_date_time(),
        submission_id: payload
            .get("submission_id")
            .and_then(serde_json::Value::as_u64),
        source_sha256_norm: payload
            .get("source_sha256_norm")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
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
