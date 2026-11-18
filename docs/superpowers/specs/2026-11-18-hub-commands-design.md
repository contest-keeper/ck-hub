# Hub commands to a stick (#1): Hub side

The data types, the signature and the checks the stick makes are fixed in `ck-protocol`
(`command.rs`, `docs/commands.md`, release v0.2.0) and `ck-crypto`. This note covers what the Hub
does: who may send a command, where it waits, how it reaches the stick, and what is remembered.

## Decisions

| Question | Decision |
|---|---|
| Who may send | A member of the stick's organization with role `admin` or `owner`. A viewer may read the history but not send. A stick of another organization is a 404, never a 403 (it does not reveal that the stick exists). |
| Where it waits | A `commands` table (and the same in the in-memory store): one row per command, with its state. |
| How it reaches the stick | In the reply to the stick's heartbeat, as `commands: [SignedCommand]`, next to the existing `accepted` field. Nothing is pushed: sticks are behind the venue network. |
| Signing | HMAC-SHA256 with the stick's token, through `ck_crypto::sign_command`. The Hub signs when it queues the command and keeps only the signed form, so the signed bytes never change. When per-contest keys (H.4) arrive only the key changes. |
| Lifetime | The sender may ask for 30 seconds to 1 hour; the default is 5 minutes. `expires_at` is `issued_at` plus that. The Hub never sends an expired command. |
| Audit | Every command keeps who sent it (user id and email at that time), when, the stick, the action type and its states with times. Rows are kept for 90 days, then removed by the same clean-up that removes old sessions. |
| Message text | Stored with the command so the history can show what was sent. It is never written to the Hub log, and the stick's ack does not repeat it. |

## States

```
queued --(in a heartbeat reply)--> delivered --(ack ok)--------> done
   |                                   |      --(ack refused)--> refused (reason kept)
   |                                   |      --(ack failed)---> failed
   +----------(expires_at passes)------+----> expired
```

- `queued`: created, not yet sent.
- `delivered`: sent at least once. It is sent again in every heartbeat reply until an ack arrives or it
  expires, because a reply can be lost. The stick runs each id once, so a repeat only produces a
  `replayed` refusal that the Hub ignores for a command that is already `done`.
- An ack for a command that is not the stick's, or is unknown, is dropped and counted in the log
  (id only, no text).
- The first ack decides the state; a later one never changes it.

## API

All answers are JSON. Errors use the existing `{ "error": "..." }` shape.

- `POST /api/v1/orgs/{slug}/sticks/{id}/commands` (and `POST /api/v1/sticks/{id}/commands` for the
  only organization of the caller, like the other stick routes).
  Body: `{ "action": { "type": "message", "text": "..." } | { "type": "lock_screen" } | { "type": "shutdown" }, "lifetime_seconds": 300 }`.
  Answer `201` with the command (id, state `queued`, times). `400` for a bad action or lifetime,
  `403` for a viewer, `404` for an unknown stick, `409` when the stick already has 10 commands that
  are neither finished nor expired, `429` past 20 commands a minute for one user.
- `GET /api/v1/orgs/{slug}/sticks/{id}/commands` (and the short form): the newest 50 commands with
  state, sender, times and the ack result. Any member may read.
- `GET /api/v1/orgs/{slug}/commands/{command_id}`: one command, for polling after a send.

Shutdown and lock are destructive for a contestant. The Hub page asks for a confirmation before it
calls the API (see below); the API itself needs no confirm flag, because it is already limited to
admins and owners.

## Heartbeat reply

`accept_heartbeat` answers `{ "accepted": seq, "commands": [...] }`. For the authenticated stick it
loads the commands that are `queued` or `delivered` and not expired, marks the `queued` ones
`delivered`, and returns them signed. The field is left out when there is nothing to send. An older
stick ignores it (the type is `#[serde(default)]`). The reply never carries commands of another stick.

The `command_ack` event is accepted by `accept_event` like other events (same signature check and
sequence rules) and is the only way a command leaves `delivered`.

## Storage

```
commands(
  id text primary key,            -- the command id (UUID), also inside the signed command
  organization_id text not null,
  stick_id text not null,
  sent_by_user_id text not null,
  sent_by_email text not null,    -- kept as it was at that time
  action_type text not null,
  signed_command jsonb not null,  -- what is sent, exactly
  state text not null,            -- queued | delivered | done | refused | failed | expired
  refusal text,                   -- set when state = refused
  created_at, delivered_at, finished_at, expires_at timestamptz
)
index on (stick_id, state, expires_at), index on (organization_id, created_at desc)
```

A migration adds it. The `Store` trait gets `queue_command`, `pending_commands(stick, now)`,
`mark_delivered`, `record_ack`, `commands_of(stick, limit)` and `delete_commands_before(time)`.
The memory store does the same in a map, so the API tests need no database.

## The Hub page

Per stick: buttons Message, Lock and Shutdown, each with a confirm step that names the stick and the
action (Message also asks for the text). Below, the command history with state and sender. The page
polls the history while a command is `queued` or `delivered`.

## Order of work

1. This note.
2. Move the Hub to ck-protocol v0.2.0 (it is on an older revision). With C6 the shared time type
   changes at the same moment, so do the two together.
3. Store trait, in-memory and PostgreSQL implementations, migration, tests (against a real database).
4. API routes and the heartbeat reply, with tests: viewer refused, other organization 404, expiry,
   repeat until ack, ack closes it, ack for another stick's command ignored.
5. Hub page.
6. Check with a VM stick (lane B, B3) that a command arrives and the ack shows in the history.

## Not in scope

Per-contest keys (H.4), scheduled commands, commands to many sticks at once (the page may call the
API once per stick), a command that changes settings.
