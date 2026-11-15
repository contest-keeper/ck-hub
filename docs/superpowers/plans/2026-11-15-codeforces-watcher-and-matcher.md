# Codeforces watcher and matcher (Hub side) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Hub reads a Codeforces contest's submissions, keeps them, and works out which ones did not come from a stick.

**Architecture:** Four small modules in `crates/ck-hub/src`: `judge.rs` (shared types and the stick event reader), `matcher.rs` (pure matching), `codeforces.rs` (API parsing, signing and HTTP client) and `watcher.rs` (polling loop). Judge submissions go in a new append-only, organization-scoped table. Verdicts are not stored: they are computed from the stored rows each time, so a stick event that arrives late still matches.

**Tech Stack:** Rust, `ureq` (already a dependency), `sha2` (already), `postgres`, `chrono`, `serde_json`. No new crates.

**Scope:** Hub side only. The stick capture proxy (`ck-os`) and the payload contract in `ck-protocol` are separate plans. Kilonova and vJudge reuse `judge.rs`, `matcher.rs` and the store methods later.

## Facts found while planning (they correct the spec)

- `ck-protocol` (an external crate pinned by git rev) already defines `EventKind::Login` and `EventKind::Submission`, and `Event.payload` is a free `serde_json::Value`. So there are no new event types. What is missing is the **payload shape**, which the code comment says is "fixed with the capture rules (B.3, D.2)" in the plan that is not in this checkout. This plan defines the shape below and reads it defensively. Check it against B.3/D.2 before merging, and record it in `ck-protocol`.
- Tables are organization-scoped with row level security (`ck.org_id`, see `migrations/0006`). The new table must follow that, and store access goes through `in_org`.
- Verdicts are computed, not stored (spec said "recorded"). Reason: an event can reach the Hub after the judge submission because sticks queue offline, so "unmatched" is not final.
- The table drops the `raw jsonb` column from the spec. Nothing reads it.

## Payload shape (what the Hub reads)

`EventKind::Submission` payload:

```json
{ "judge": "codeforces", "handle": "tourist", "contest_id": 1900, "problem_index": "B", "submission_id": 245000123 }
```

`submission_id` is optional. Time is `Event.sent_at` (the stick's clock). Clock drift between stick and judge is the main risk for the window rule, so the window is configurable.

## File structure

- Create `crates/ck-hub/src/judge.rs`: `JudgeSubmission`, `SubmitEvent`, `submit_event()`.
- Create `crates/ck-hub/src/matcher.rs`: `Verdict`, `match_submissions()`.
- Create `crates/ck-hub/src/codeforces.rs`: `parse_status()`, `signed_query()`, `Client`.
- Create `crates/ck-hub/src/watcher.rs`: `poll_once()`, `run()`.
- Create `crates/ck-hub/migrations/0010_judge_submissions.sql`.
- Modify `crates/ck-hub/src/store.rs`: three trait methods and the `MemoryStore` impl.
- Modify `crates/ck-hub/src/postgres_store.rs`: the same three methods.
- Modify `crates/ck-hub/src/lib.rs`: modules and exports.
- Modify `crates/ck-hub/src/main.rs`: flags and the watcher thread.
- Create `crates/ck-hub/tests/codeforces_watcher.rs`.
- Modify `crates/ck-hub/tests/store.rs`: judge submission checks shared by both stores.

All commands run from `/Users/robertpopa/projects/ck-wt/ck-hub`.

---

### Task 0: Branch and spec fix

**Files:**
- Modify: `docs/superpowers/specs/2026-11-15-codeforces-capture-and-matching-design.md`

- [ ] **Step 1: Branch from develop** (the working tree holds only untracked docs)

```bash
git switch -c feature/codeforces-matcher develop
```

Expected: `Switched to a new branch 'feature/codeforces-matcher'`. If git refuses because of the untracked files, they do not conflict with develop; stop and report.

- [ ] **Step 2: Fix the spec.** In section 1 replace the two event types with: "No new event types: `EventKind::Login` and `EventKind::Submission` already exist in `ck-protocol`. This spec fixes the Submission payload: `judge`, `handle`, `contest_id`, `problem_index`, optional `submission_id`." In section 3 remove `raw jsonb` and replace "The result ... is recorded per judge submission" with "Verdicts are computed from stored rows, not stored, so a late stick event still matches."

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers
git commit -m "docs: add codeforces matcher spec and plan"
```

---

### Task 1: Shared types and the stick event reader

**Files:**
- Create: `crates/ck-hub/src/judge.rs`
- Modify: `crates/ck-hub/src/lib.rs`

- [ ] **Step 1: Write the file with its tests** (tests first in the file; the function body is a stub that fails them)

```rust
//! What the Hub knows about a judge and about what sticks said they submitted.

use chrono::{DateTime, Utc};
use ck_protocol::EventKind;

use crate::store::StoredEvent;

/// A submission as the judge reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeSubmission {
    pub submission_id: u64,
    pub contest_id: u64,
    /// Every member. A team submission has several; handles compare without regard to case.
    pub handles: Vec<String>,
    pub problem_index: String,
    pub submitted_at: DateTime<Utc>,
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
    pub at: DateTime<Utc>,
    pub submission_id: Option<u64>,
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
    if payload.get("judge")?.as_str()? != "codeforces" {
        return None;
    }
    Some(SubmitEvent {
        key: (
            event.stick_id.as_str().to_owned(),
            event.boot_id.clone(),
            event.seq,
        ),
        handle: payload.get("handle")?.as_str()?.to_owned(),
        contest_id: payload.get("contest_id")?.as_u64()?,
        problem_index: payload.get("problem_index")?.as_str()?.to_owned(),
        at: event.sent_at,
        submission_id: payload.get("submission_id").and_then(serde_json::Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use ck_protocol::{Event, FIRST_PREV_HASH, StickId};
    use serde_json::{Value, json};

    use super::*;

    fn stored(kind: EventKind, payload: Value) -> StoredEvent {
        StoredEvent {
            received_at: DateTime::from_timestamp(1_800_000_100, 0).expect("time"),
            integrity: None,
            event: Event {
                stick_id: StickId::parse("03c9f556").expect("id"),
                boot_id: "boot-1".to_owned(),
                seq: 7,
                prev_hash: FIRST_PREV_HASH.to_owned(),
                sent_at: DateTime::from_timestamp(1_800_000_000, 0).expect("time"),
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
    fn the_submission_id_is_optional() {
        let event = stored(
            EventKind::Submission,
            json!({"judge": "codeforces", "handle": "a", "contest_id": 1, "problem_index": "A"}),
        );
        assert_eq!(submit_event(&event).expect("event").submission_id, None);
    }

    #[test]
    fn ignores_logins_other_judges_and_incomplete_payloads() {
        let login = stored(EventKind::Login, json!({"judge": "codeforces", "handle": "a"}));
        assert!(submit_event(&login).is_none());
        let other = stored(
            EventKind::Submission,
            json!({"judge": "kilonova", "handle": "a", "contest_id": 1, "problem_index": "A"}),
        );
        assert!(submit_event(&other).is_none());
        let incomplete = stored(EventKind::Submission, json!({"judge": "codeforces", "handle": "a"}));
        assert!(submit_event(&incomplete).is_none());
    }
}
```

- [ ] **Step 2: Add the module.** In `src/lib.rs` add `mod judge;` (alphabetical, after `invite`) and `pub use judge::{EventKey, JudgeSubmission, SubmitEvent, submit_event};`.

- [ ] **Step 3: Run**

```bash
cargo test -p ck-hub --lib judge
```

Expected: 3 passed. If `event.stick_id.as_str()` does not exist, use whatever `StickId` exposes (`postgres_store.rs` uses `as_str()`, so it does).

- [ ] **Step 4: Commit**

```bash
git add crates/ck-hub/src/judge.rs crates/ck-hub/src/lib.rs
git commit -m "feat: read codeforces submit events"
```

---

### Task 2: The matcher

**Files:**
- Create: `crates/ck-hub/src/matcher.rs`
- Modify: `crates/ck-hub/src/lib.rs`

- [ ] **Step 1: Write the file with tests**

```rust
//! Decides which judge submissions came from a stick.
//!
//! First by submission id, then by handle, problem and time. A stick event explains at most one
//! judge submission, so one submit cannot cover two.

use chrono::TimeDelta;

use crate::judge::{EventKey, JudgeSubmission, SubmitEvent};

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
    window: TimeDelta,
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

    // The rest by handle, problem and time, oldest submission first, closest event first.
    let mut rest: Vec<usize> = (0..judge.len())
        .filter(|i| verdicts[*i] == Verdict::Unmatched)
        .collect();
    rest.sort_by_key(|i| judge[*i].submitted_at);
    for i in rest {
        let submission = &judge[i];
        let mut best: Option<(usize, TimeDelta)> = None;
        for (e, event) in events.iter().enumerate() {
            if claimed[e]
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
    use chrono::{DateTime, Utc};

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("time")
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
        }
    }

    fn key(seq: u64) -> Verdict {
        Verdict::Matched(("stick".to_owned(), "boot".to_owned(), seq))
    }

    const WINDOW: TimeDelta = TimeDelta::seconds(10);

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
    fn a_wrong_id_falls_through_to_the_window() {
        let verdicts = match_submissions(
            &[judge(99, "a", "A", 0)],
            &[event(1, "a", "A", 2, Some(7))],
            WINDOW,
        );
        assert_eq!(verdicts, vec![key(1)]);
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
```

- [ ] **Step 2: Add the module.** In `src/lib.rs` add `mod matcher;` and `pub use matcher::{Verdict, match_submissions};`.

- [ ] **Step 3: Run**

```bash
cargo test -p ck-hub --lib matcher
```

Expected: 8 passed. (`Option::is_none_or` needs Rust 1.82+; the workspace has a `rust-toolchain.toml`, check it. If older, write `best.map_or(true, |(_, closest)| gap < closest)` and expect clippy to prefer `is_none_or`.)

- [ ] **Step 4: Commit**

```bash
git add crates/ck-hub/src/matcher.rs crates/ck-hub/src/lib.rs
git commit -m "feat: match judge submissions to stick events"
```

---

### Task 3: Codeforces response parsing and request signing

**Files:**
- Create: `crates/ck-hub/src/codeforces.rs`
- Modify: `crates/ck-hub/src/lib.rs`

- [ ] **Step 1: Write the pure part with tests** (the HTTP client comes in Task 5)

```rust
//! Talking to the Codeforces API.

use chrono::DateTime;
use serde::Deserialize;
use sha2::{Digest, Sha512};

use crate::judge::JudgeSubmission;

#[derive(Debug, thiserror::Error)]
pub enum CodeforcesError {
    #[error("the answer is not what Codeforces sends: {0}")]
    Malformed(String),
    #[error("Codeforces refused the request: {0}")]
    Refused(String),
    #[error("the request failed: {0}")]
    Request(String),
}

#[derive(Deserialize)]
struct Answer {
    status: String,
    comment: Option<String>,
    #[serde(default)]
    result: Vec<Row>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    id: u64,
    contest_id: u64,
    creation_time_seconds: i64,
    problem: Problem,
    author: Author,
}

#[derive(Deserialize)]
struct Problem {
    index: String,
}

#[derive(Deserialize)]
struct Author {
    members: Vec<Member>,
}

#[derive(Deserialize)]
struct Member {
    handle: String,
}

/// Reads the answer of `contest.status`. Codeforces lists the newest submission first.
///
/// # Errors
/// [`CodeforcesError::Refused`] when Codeforces says `FAILED`, [`CodeforcesError::Malformed`]
/// when the body is not the expected JSON.
pub fn parse_status(body: &str) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
    let answer: Answer =
        serde_json::from_str(body).map_err(|error| CodeforcesError::Malformed(error.to_string()))?;
    if answer.status != "OK" {
        return Err(CodeforcesError::Refused(answer.comment.unwrap_or(answer.status)));
    }
    let mut submissions = Vec::with_capacity(answer.result.len());
    for row in answer.result {
        let submitted_at = DateTime::from_timestamp(row.creation_time_seconds, 0)
            .ok_or_else(|| CodeforcesError::Malformed("a submission time is out of range".to_owned()))?;
        submissions.push(JudgeSubmission {
            submission_id: row.id,
            contest_id: row.contest_id,
            handles: row.author.members.into_iter().map(|m| m.handle).collect(),
            problem_index: row.problem.index,
            submitted_at,
        });
    }
    Ok(submissions)
}

/// The query string for a signed call: the parameters, `apiKey`, `time` and `apiSig`, with the
/// parameters in the order the signature covers. `rand` is six characters the caller picks.
#[must_use]
pub fn signed_query(
    method: &str,
    params: &[(&str, String)],
    key: &str,
    secret: &str,
    time: i64,
    rand: &str,
) -> String {
    let mut all: Vec<(String, String)> = params
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect();
    all.push(("apiKey".to_owned(), key.to_owned()));
    all.push(("time".to_owned(), time.to_string()));
    all.sort();
    let joined = all
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let digest = Sha512::digest(format!("{rand}/{method}?{joined}#{secret}").as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{joined}&apiSig={rand}{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{"status":"OK","result":[
        {"id":20,"contestId":1900,"creationTimeSeconds":1800000100,
         "problem":{"contestId":1900,"index":"B","name":"x"},
         "author":{"members":[{"handle":"tourist"},{"handle":"petr"}]},"verdict":"OK"},
        {"id":10,"contestId":1900,"creationTimeSeconds":1800000000,
         "problem":{"contestId":1900,"index":"A","name":"y"},
         "author":{"members":[{"handle":"petr"}]}}]}"#;

    #[test]
    fn reads_submissions_newest_first() {
        let submissions = parse_status(BODY).expect("parsed");
        assert_eq!(submissions.len(), 2);
        assert_eq!(submissions[0].submission_id, 20);
        assert_eq!(submissions[0].handles, vec!["tourist", "petr"]);
        assert_eq!(submissions[0].problem_index, "B");
        assert_eq!(submissions[1].submitted_at.timestamp(), 1_800_000_000);
    }

    #[test]
    fn a_failed_status_is_a_refusal_with_the_comment() {
        let error = parse_status(r#"{"status":"FAILED","comment":"contestId: not found"}"#)
            .expect_err("refused");
        assert!(error.to_string().contains("contestId: not found"));
    }

    #[test]
    fn garbage_is_malformed() {
        assert!(matches!(parse_status("<html>"), Err(CodeforcesError::Malformed(_))));
    }

    #[test]
    fn the_signature_covers_sorted_parameters_and_the_secret() {
        let query = signed_query(
            "contest.status",
            &[("contestId", "1".to_owned()), ("count", "5".to_owned())],
            "k",
            "s",
            100,
            "abcdef",
        );
        let signed = "abcdef/contest.status?apiKey=k&contestId=1&count=5&time=100#s";
        let digest = Sha512::digest(signed.as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            query,
            format!("apiKey=k&contestId=1&count=5&time=100&apiSig=abcdef{hex}")
        );
        assert!(!query.contains('s') || !query.contains("#s"), "the secret is never sent");
    }
}
```

- [ ] **Step 2: Add the module.** `mod codeforces;` and `pub use codeforces::{CodeforcesError, parse_status, signed_query};` in `lib.rs`.

- [ ] **Step 3: Run**

```bash
cargo test -p ck-hub --lib codeforces
```

Expected: 4 passed. Replace the last weak assertion with `assert!(!query.contains("#s"))` if clippy complains about it.

- [ ] **Step 4: Commit**

```bash
git add crates/ck-hub/src/codeforces.rs crates/ck-hub/src/lib.rs
git commit -m "feat: parse and sign codeforces api calls"
```

---

### Task 4: Store judge submissions

**Files:**
- Create: `crates/ck-hub/migrations/0010_judge_submissions.sql`
- Modify: `crates/ck-hub/src/store.rs`, `crates/ck-hub/src/postgres_store.rs`, `crates/ck-hub/tests/store.rs`

- [ ] **Step 1: Migration**

```sql
-- What a judge says was submitted. Never changed or deleted, like events. The Hub works out which
-- of them came from a stick whenever it is asked, so a late stick event still matches.
CREATE TABLE judge_submissions (
    id              bigserial PRIMARY KEY,
    organization_id text        NOT NULL REFERENCES organizations (id),
    judge           text        NOT NULL,
    contest_id      bigint      NOT NULL,
    submission_id   bigint      NOT NULL,
    handles         text[]      NOT NULL,
    problem_index   text        NOT NULL,
    submitted_at    timestamptz NOT NULL,
    received_at     timestamptz NOT NULL,
    UNIQUE (organization_id, judge, contest_id, submission_id)
);

ALTER TABLE judge_submissions ENABLE ROW LEVEL SECURITY;
ALTER TABLE judge_submissions FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON judge_submissions
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));

CREATE FUNCTION judge_submissions_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'judge submissions are append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER judge_submissions_never_change
    BEFORE UPDATE OR DELETE ON judge_submissions
    FOR EACH ROW EXECUTE FUNCTION judge_submissions_are_append_only();
```

Check how migrations are loaded (`grep -n "migrations" crates/ck-hub/src/postgres_store.rs`); if they are listed by hand, add `0010`.

- [ ] **Step 2: Write the failing shared test** in `tests/store.rs`, following how the file runs one check against `MemoryStore` and, when `CK_TEST_DATABASE_URL` is set, `PostgresStore` (copy the shape of an existing check such as the one for events, including how it makes an organization). Check to add:

```rust
fn check_judge_submissions(store: &dyn Store, organization: &str) {
    let first = ck_hub::JudgeSubmission {
        submission_id: 10,
        contest_id: 1900,
        handles: vec!["tourist".to_owned()],
        problem_index: "A".to_owned(),
        submitted_at: time(0),
    };
    let second = ck_hub::JudgeSubmission { submission_id: 20, ..first.clone() };

    let added = store
        .add_judge_submissions(organization, "codeforces", &[first.clone(), second], time(5))
        .expect("added");
    assert_eq!(added, 2);

    // Seeing the same submission again changes nothing.
    let again = store
        .add_judge_submissions(organization, "codeforces", &[first.clone()], time(9))
        .expect("added");
    assert_eq!(again, 0);

    let kept = store.judge_submissions(organization, "codeforces", 1900).expect("kept");
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0], first);
    assert!(store.judge_submissions(organization, "codeforces", 1).expect("kept").is_empty());
    assert!(store.judge_submissions(organization, "kilonova", 1900).expect("kept").is_empty());
}
```

and a second check that `submission_events` returns the Codeforces `Submission` events of every stick of the organization, in the order received, and none of another organization's: append two events for two sticks plus a `Login` event and an event with `"judge": "kilonova"`, then assert only the two Codeforces submit events come back as `SubmitEvent`s.

- [ ] **Step 3: Run, expect a compile failure** (`no method named add_judge_submissions`)

```bash
cargo test -p ck-hub --test store
```

- [ ] **Step 4: Trait.** Add to `Store` in `store.rs`, after `events`:

```rust
    /// Keeps the submissions the judge reported that are not kept yet. Returns how many were new.
    fn add_judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        submissions: &[JudgeSubmission],
        received_at: DateTime<Utc>,
    ) -> Result<usize, StoreError>;

    /// Every kept submission of one contest, oldest first.
    fn judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        contest_id: u64,
    ) -> Result<Vec<JudgeSubmission>, StoreError>;

    /// What every stick of the organization reported as submitted to Codeforces, in the order
    /// the Hub received it. Events that are not well-formed Codeforces submits are left out.
    fn submission_events(&self, organization: &str) -> Result<Vec<SubmitEvent>, StoreError>;
```

- [ ] **Step 5: `MemoryStore`.** Add a field `judge_submissions: Mutex<Vec<(String, String, JudgeSubmission)>>` (organization, judge, submission). Implement:
  - `add_judge_submissions`: lock; for each submission skip when an entry with the same organization, judge, contest id and submission id exists, otherwise push; count the pushes.
  - `judge_submissions`: filter on organization, judge and contest id, sort by `submitted_at`, clone.
  - `submission_events`: lock `events`; for every `(org, _)` key equal to `organization` collect all `StoredEvent`s, sort by `received_at`, map through `crate::judge::submit_event`, `flatten`.

  Add `use crate::judge::{JudgeSubmission, SubmitEvent, submit_event};` at the top of `store.rs`.

- [ ] **Step 6: `PostgresStore`.** Convert ids to `i64` before the closure (the closure can only return `postgres::Error`; use `.map_err(failed)` as `append_event` does):

```rust
    fn add_judge_submissions(
        &self,
        organization: &str,
        judge: &str,
        submissions: &[JudgeSubmission],
        received_at: DateTime<Utc>,
    ) -> Result<usize, StoreError> {
        let mut rows = Vec::with_capacity(submissions.len());
        for s in submissions {
            rows.push((
                i64::try_from(s.contest_id).map_err(failed)?,
                i64::try_from(s.submission_id).map_err(failed)?,
            ));
        }
        self.in_org(organization, |transaction| {
            let mut added = 0;
            for (s, (contest, id)) in submissions.iter().zip(&rows) {
                added += transaction.execute(
                    "INSERT INTO judge_submissions
                         (organization_id, judge, contest_id, submission_id, handles,
                          problem_index, submitted_at, received_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (organization_id, judge, contest_id, submission_id) DO NOTHING",
                    &[&organization, &judge, contest, id, &s.handles, &s.problem_index,
                      &s.submitted_at, &received_at],
                )?;
            }
            Ok(usize::try_from(added).unwrap_or(0))
        })
    }
```

`judge_submissions`: `SELECT submission_id, handles, problem_index, submitted_at FROM judge_submissions WHERE organization_id = $1 AND judge = $2 AND contest_id = $3 ORDER BY submitted_at, id`, convert `i64` back with `u64::try_from(..).map_err(failed)?` outside the closure as `events` does. `submission_events`: `SELECT event, received_at, integrity FROM events WHERE organization_id = $1 AND event->>'kind' = 'submission' ORDER BY id`, build `StoredEvent` exactly as `events` does, then `submit_event` and `flatten`. Check the JSON key for the kind (`EventKind` serializes as `submission` by `rename_all = "snake_case"`, field name `kind`).

- [ ] **Step 7: Run both stores.** Start a throwaway PostgreSQL as the README describes, then:

```bash
CK_TEST_DATABASE_URL=postgres://postgres:password@127.0.0.1:54329/ckhub cargo test -p ck-hub --test store
```

Expected: pass on memory and PostgreSQL. Also try an `UPDATE judge_submissions ...` through `psql` to see the append-only trigger refuse it; the `postgres` role the tests use may bypass RLS, so do not treat RLS as tested by this run.

- [ ] **Step 8: Commit**

```bash
git add crates/ck-hub/migrations crates/ck-hub/src crates/ck-hub/tests/store.rs
git commit -m "feat: keep judge submissions per organization"
```

---

### Task 5: Codeforces client and polling

**Files:**
- Modify: `crates/ck-hub/src/codeforces.rs`
- Create: `crates/ck-hub/src/watcher.rs`, `crates/ck-hub/tests/codeforces_watcher.rs`
- Modify: `crates/ck-hub/src/lib.rs`

- [ ] **Step 1: Client.** Append to `codeforces.rs`. Look at `acs_mail.rs` first and mirror how it calls `ureq` (the version is 3, whose error and body API differ from 2):

```rust
/// A Codeforces API client. With credentials every call is signed, which also covers gyms and
/// group contests; without them only public contests can be read.
pub struct Client {
    base: String,
    credentials: Option<(String, String)>,
}

impl Client {
    #[must_use]
    pub fn new(base: &str, credentials: Option<(String, String)>) -> Self {
        Self { base: base.trim_end_matches('/').to_owned(), credentials }
    }

    /// One page of `contest.status`, newest first. `from` starts at 1.
    ///
    /// # Errors
    /// [`CodeforcesError::Request`] when the call fails, otherwise what [`parse_status`] says.
    pub fn status(
        &self,
        contest_id: u64,
        from: u64,
        count: u64,
    ) -> Result<Vec<JudgeSubmission>, CodeforcesError> {
        let params = [
            ("contestId", contest_id.to_string()),
            ("from", from.to_string()),
            ("count", count.to_string()),
        ];
        let query = match &self.credentials {
            Some((key, secret)) => {
                let now = chrono::Utc::now();
                let rand = format!("{:06}", now.timestamp_subsec_nanos() % 1_000_000);
                signed_query("contest.status", &params, key, secret, now.timestamp(), &rand)
            }
            None => params.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("&"),
        };
        let url = format!("{}/api/contest.status?{query}", self.base);
        // Codeforces explains a refusal in a JSON body with an error status, so read the body
        // whatever the status is.
        let mut response = ureq::get(&url)
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .map_err(|error| CodeforcesError::Request(error.to_string()))?;
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| CodeforcesError::Request(error.to_string()))?;
        parse_status(&body)
    }
}
```

The error text must never include the URL (it carries `apiKey`), so keep `.to_string()` of the `ureq` error only if it does not print the URL; check by pointing the client at a closed port and reading the message. If it prints the URL, replace the message with a fixed "the request failed".

- [ ] **Step 2: Write the failing watcher test** `tests/codeforces_watcher.rs`. A tiny HTTP server on a thread: bind `TcpListener` to `127.0.0.1:0`, answer each request with a canned JSON body chosen from the request line (the `from=` value), and record the request lines.

```rust
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

/// Serves `pages[n]` for `from = n * count + 1` and records every request line.
fn serve(pages: Vec<String>, count: u64) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("connection");
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).expect("request line");
            log.lock().expect("log").push(line.clone());
            let from: u64 = line
                .split("from=").nth(1).and_then(|s| s.split(['&', ' ']).next())
                .and_then(|s| s.parse().ok()).unwrap_or(1);
            let page = usize::try_from((from - 1) / count).expect("page");
            let body = pages.get(page).cloned()
                .unwrap_or_else(|| r#"{"status":"OK","result":[]}"#.to_owned());
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("answer");
        }
    });
    (base, seen)
}

fn row(id: u64, seconds: i64) -> String {
    format!(r#"{{"id":{id},"contestId":1900,"creationTimeSeconds":{},"problem":{{"index":"A"}},"author":{{"members":[{{"handle":"a"}}]}}}}"#, 1_800_000_000 + seconds)
}

fn page(rows: &[String]) -> String {
    format!(r#"{{"status":"OK","result":[{}]}}"#, rows.join(","))
}
```

Tests (each builds a `MemoryStore`, an organization via `store.ensure_organization("org", "Org")`, a `Client::new(&base, None)`):

1. `keeps_every_page_until_the_last_short_one`: page size 2, pages `[row(4), row(3)]`, `[row(2), row(1)]`, `[]`; `poll_once(&client, &store, &org.id, "codeforces", 1900, 2, Duration::ZERO)` returns `Ok(4)` and the store holds ids 1 to 4.
2. `a_second_poll_stops_at_what_it_already_has`: run again with the same server; returns `Ok(0)` and the request log shows only one request (the first page is all known, so it stops).
3. `a_refusal_keeps_nothing_new_and_is_an_error`: server answers `{"status":"FAILED","comment":"contestId: not found"}`; `poll_once` returns `Err` containing the comment and the store is empty.
4. `a_signed_client_sends_the_key_and_signature_but_not_the_secret`: `Client::new(&base, Some(("KEY".into(), "SECRET".into())))`; the request line contains `apiKey=KEY` and `apiSig=` and does not contain `SECRET`.

- [ ] **Step 3: Run, expect a compile failure** (`poll_once` not found)

```bash
cargo test -p ck-hub --test codeforces_watcher
```

- [ ] **Step 4: `watcher.rs`**

```rust
//! Follows a contest on the judge and keeps what it reports.

use std::thread::sleep;
use std::time::Duration;

use chrono::Utc;

use crate::codeforces::{Client, CodeforcesError};
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
    judge: &str,
    contest_id: u64,
    page_size: u64,
    pause: Duration,
) -> Result<usize, WatchError> {
    let mut total = 0;
    let mut from = 1;
    loop {
        let page = client.status(contest_id, from, page_size)?;
        let added = store.add_judge_submissions(organization, judge, &page, Utc::now())?;
        total += added;
        let last = page.len() < usize::try_from(page_size).unwrap_or(usize::MAX);
        if added == 0 || last {
            return Ok(total);
        }
        from += page_size;
        sleep(pause);
    }
}

/// Polls forever: every `interval`, and after a failure the wait doubles up to `max_wait`, then
/// goes back to `interval` after a success. Failures are logged without the request.
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
        match poll_once(client, store, organization, "codeforces", contest_id, 100, Duration::from_secs(2)) {
            Ok(new) => {
                tracing::info!(contest_id, new, "polled codeforces");
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
```

Note the stop rule: a page whose submissions are all known ends the poll. If the very newest page is all known the poll costs one call, which is the steady state.

- [ ] **Step 5: Exports.** `mod watcher;` and `pub use watcher::{WatchError, poll_once, run as watch};` and `pub use codeforces::Client as CodeforcesClient;` in `lib.rs` (rename the first export in the test file's imports to match).

- [ ] **Step 6: Run**

```bash
cargo test -p ck-hub --test codeforces_watcher
```

Expected: 4 passed.

- [ ] **Step 7: Commit**

```bash
git add crates/ck-hub
git commit -m "feat: poll codeforces contests"
```

---

### Task 6: Start the watcher with the Hub

**Files:**
- Modify: `crates/ck-hub/src/main.rs`

- [ ] **Step 1:** Read how `main.rs` starts the server (the `Serve`-like args struct near the `CK_LISTEN` flag, how it builds the store, and how it resolves an organization from a slug). Add to that struct, next to the other options:

```rust
    /// Follow this Codeforces contest. Needs `--codeforces-organization`.
    #[arg(long, env = "CK_CODEFORCES_CONTEST", requires = "codeforces_organization")]
    codeforces_contest: Option<u64>,

    /// The slug of the organization whose contest it is.
    #[arg(long, env = "CK_CODEFORCES_ORGANIZATION")]
    codeforces_organization: Option<String>,

    /// Codeforces API key, for gyms and group contests. Comes with the secret or not at all.
    #[arg(long, env = "CK_CODEFORCES_KEY", hide_env_values = true, requires = "codeforces_secret")]
    codeforces_key: Option<String>,

    #[arg(long, env = "CK_CODEFORCES_SECRET", hide_env_values = true, requires = "codeforces_key")]
    codeforces_secret: Option<String>,
```

- [ ] **Step 2:** After the store is built and before the server runs, when `codeforces_contest` is set: look up the organization with `store.organization_by_slug(..)` (exit with a clear error if missing), build `CodeforcesClient::new("https://codeforces.com", key.zip(secret))`, and `std::thread::spawn` a thread calling `ck_hub::watch(&client, &*store, &organization.id, contest, Duration::from_secs(30), Duration::from_secs(600))`. Clone the `Arc<dyn Store>` for the thread. Never print the key or secret, and do not add them to any log line or startup message.

- [ ] **Step 3: Check everything**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

Expected: all pass. Fix clippy findings in the new files (the workspace lints are strict; do not allow them away).

- [ ] **Step 4: Run it by hand against a public contest**

```bash
cargo run -p ck-hub -- serve --codeforces-organization default --codeforces-contest 1900
```

(use the real subcommand name from `main.rs`). Expected: a log line `polled codeforces` with `new` greater than 0 after a few seconds, then `new=0`. Stop it with Ctrl-C.

- [ ] **Step 5: Commit**

```bash
git add crates/ck-hub/src/main.rs
git commit -m "feat: follow a codeforces contest from the hub"
```

---

## After this plan

Not in this plan, in this order: (1) record the Submission payload shape in `ck-protocol` and check it against capture rules B.3/D.2; (2) the `ck-os` capture proxy plan; (3) a dashboard or API view that shows verdicts, with a grace period before a fresh submission counts as flagged; (4) Kilonova and vJudge watchers on the same store methods.

## Self-review notes

- Spec coverage: id match, window fallback, one event per submission, wrong id falls through, anonymous and keyed polling, secrets from environment only, backoff, cursor never skips a failed page, append-only table, store tests on memory and PostgreSQL. Capture is deliberately a separate plan.
- Not verified while planning: the exact `ureq` 3 call shape, the migration loader, the `StickId` accessors and the `main.rs` start-up structure. Each task says where to check.
