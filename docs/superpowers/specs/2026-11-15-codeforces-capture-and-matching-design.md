# Codeforces capture, watcher and matcher: design

Date: 2026-11-15. Scope: Codeforces only. Kilonova and vJudge follow later and reuse the same shape.

## Goal

Flag Codeforces submissions that did not come from a contest stick. The stick reports what the
contestant did, the Hub reports what the judge saw, and the matcher compares the two.

## Parts and build order

1. Event contract (ck-os and ck-hub).
2. Capture on the stick (ck-os).
3. Watcher and matcher on the Hub (ck-hub).

Parts 1 and 3 can be built and tested without a stick, so they go first. Each part gets its own plan.

## 1. Event contract

No new event types: `EventKind::Login` and `EventKind::Submission` already exist in `ck-protocol`.
This spec fixes the Submission payload: `judge`, `handle`, `contest_id`, `problem_index` and an
optional `submission_id`. The time is the event's `sent_at`. No event carries a password, cookie,
session id or token.

## 2. Capture on the stick (ck-os)

New crate `ck-agent-capture`: a local TLS proxy that intercepts `codeforces.com` only.

- Parses the login request for the handle, and the submit request for contest and problem.
- Credential material (password, cookies, CSRF and session tokens) is dropped at the point of
  parsing. It is never stored, logged or queued.
- Tries to read the new submission id. Codeforces redirects to the "my submissions" page after a
  submit, so the id may only appear in a later response. The id is best-effort.
- Hands events to the existing `ck-agent-uplink` queue.
- The browser must trust the proxy CA. The CA is installed by the image patch, so this part also
  touches the image.

## 3. Watcher and matcher (ck-hub)

### Watcher

- Polls `contest.status` for the contest id.
- Anonymous by default: public contests only.
- Keyed mode: an optional Codeforces API key and secret signs requests, which covers gyms and
  group contests. The key and secret are read from environment configuration only. They are not
  stored in the database and never logged.
- Backs off on API errors and rate limits. The cursor never moves past a failed page.
- Stores each judge submission in a new append-only table `judge_submissions`
  (judge, contest_id, submission_id, handle, problem_index, submitted_at), unique on
  (judge, contest_id, submission_id). A repeated poll is a no-op.

### Matcher

Runs in this order for each judge submission:

1. Id match: a stick `submit` event whose `submission_id` equals the judge submission id.
2. Fallback: same handle and problem, with `at` within a time window of the judge submission time.
   Default ±10 s, configurable.
3. Each stick event matches at most one judge submission.
4. Anything left unmatched is flagged.

Verdicts are computed from the stored rows, not stored, so a stick event that arrives late still
matches. An id
that is present but wrong falls through to the window rule.

## Testing

- Watcher against a fake Codeforces HTTP server: paging, repeated polls, rate-limit backoff,
  keyed signing.
- Matcher as a pure function: id match, fallback match, wrong id falls through, one event offered
  to two submissions, no event at all.
- Capture: parser tests on recorded request fixtures, including a check that no credential
  material reaches the queue.
- Store tests run on memory, and on PostgreSQL when `CK_TEST_DATABASE_URL` is set.

## Rules that apply

- No `Co-Authored-By` trailers or tool attribution in commits or PRs.
- Work branches from `develop`, never commit to `main` or `develop`.
- Never store, log or send passwords, cookies, session ids or tokens.
