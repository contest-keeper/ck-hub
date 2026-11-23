# Why the Hub stays at one replica

The Hub runs as one copy (`max_replicas = 1` in ck-infra). Moving the login lockouts into PostgreSQL
would not be enough to raise it, so that change was not made on its own: half of the state would
then be shared and half not.

State that lives only in the memory of one Hub process:

| What | Where | What goes wrong with two copies |
|---|---|---|
| Login lockouts, mail limits | `accounts.rs` `failures` | Each copy counts separately: an attacker gets the limit once per copy. |
| Codes Keeper Forge trades for a session | `accounts.rs` `forge_codes` | A code issued by one copy is unknown to the other, so the trade fails. |
| Rate limits for heartbeats, commands, signing, enrolment, chunks | `hub.rs` `rates`, `command_rates`, `sign_rates`, `enrol_mints`, `enrol_failures`, `chunk_rates` | Limits multiply by the number of copies. |
| The stick registry | `hub.rs` `registry`, read from `registry.json` on disk | Each copy has its own file. |
| Judge result cache | `hub.rs` `judged` | Only a cost, not a fault. |

Everything else (sticks, events, heartbeats, users, sessions, recordings metadata) is in PostgreSQL
already and is safe to share.

To go beyond one replica, one change has to move all of the rows above that matter (limits, forge
codes, the registry) into PostgreSQL, with tests on both store backends. Until then the Container App
keeps `max_replicas = 1`. The 500-stick capacity run in `capacity.md` was done with one copy.
