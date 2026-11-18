# Enrolment codes (#24): Hub side

A new stick should get its own secret without one being written to its disk beforehand. An admin
makes a one-time code for one stick; the stick trades it once for its id and secret. The Forge and
agent sides are lane B task B4 (#24); this note and `crates/ck-hub/src/enrol.rs` are the Hub side.

## Flow

1. An admin or owner asks the Hub for a code, optionally naming what the stick is for:
   `POST /api/v1/orgs/{slug}/enrolment-codes` (and `/api/v1/enrolment-codes` for the caller's only
   organization) with `{ "team", "site", "contest", "machine_number", "lifetime_minutes" }`, all optional.
   Answer `201 { "code": "K7QM-2XPD-9TVH-C4RB-8WNE", "expires_at": "..." }`. The code is shown once.
2. The code goes to the stick (typed, or written by Forge into the config partition, lane B).
3. The stick calls `POST /api/v1/enrol` with `{ "code": "..." }`. No login: the code is the proof.
   Answer `200 { "stick_id", "token", "team", "site", "contest", "machine_number" }`, `no-store`.
   The Hub made the id and the secret; the stick keeps them and signs everything with the secret from
   then on, exactly as a stick registered the old way.

## Decisions

| Question | Decision |
|---|---|
| Who may make a code | `admin` or `owner` of the organization. 30 codes a minute per person. |
| The code | 20 characters from 32 letters and digits without I, O, 0, 1 (100 bits), written in groups of four. Typed in any case, with or without dashes or spaces. |
| At rest | Only SHA-256 of the normalized code. The code is that random, so a plain hash is enough and the table cannot be turned back into codes. The secret handed out is kept only as the stick's token, as before. |
| Single use | One `UPDATE ... WHERE used_at IS NULL AND expires_at > now` marks the code used and, in the same transaction, the stick is registered in the code's organization. Two sticks racing for one code: exactly one wins (tested on PostgreSQL). |
| A taken stick id | The transaction is rolled back, so the code is not used up; the Hub tries another id (three times). |
| Expiry | 5 minutes to 24 hours, default 60 minutes. Codes made more than a day ago are removed when a new one is made. |
| Same answer for every failure | An unknown, a used and an expired code all answer `401 the code is not valid`, so the answer tells nothing about which codes exist. |
| Rate limit | A flood of failed attempts (30 a minute from everyone together) is answered `429` until the minute ends. The code is far too random to guess; this only protects the database from a flood. There is no per-address limit because the Hub is behind a proxy and does not see addresses. |
| Never logged | The code and the secret are not logged, not in errors and not kept (`MintedCode` and `Enrolment` have no `Debug`). |
| Isolation | Row-level security on `enrolment_codes`: the usual per-organization policy, plus a policy that lets the redeeming statement read and change exactly the one code it names (`ck.code_hash`), like the one-stick lookup of `sticks`. |

## For lane B (B4)

Forge asks the Hub for a code per stick (it already holds an organizer session), writes the code to the
config partition, and the agent calls `/api/v1/enrol` once at first boot and keeps the id and secret.
The old way (Forge makes the secret and registers it) keeps working, so both can run side by side.

## Not in scope

Listing or revoking unused codes (an unused code simply expires), codes that make several sticks.