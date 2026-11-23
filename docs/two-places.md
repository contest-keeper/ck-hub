# Flagging one stick heard from two places (R2.10, Hub half)

The stick's secret can be read offline, so a copied disk or secret is a stick that exists twice. The
Hub cannot stop that, but it can say so.

**Signal.** Every boot of a stick has its own `boot_id`. A reboot ends the old boot before the new
one starts, so the first-to-last heartbeat times of the two boots do not overlap. A second machine
using the same secret has its own boot, and its times overlap the first machine's.

**What is kept.** `heartbeat_boots` (one row per stick and boot) gets `first_seen` and `last_seen`
(migration 0021), set by the same statement that already records the sequence. Rows from before the
migration have no times and are ignored.

**Rule.** When the stick list is built, the boots last heard from in the past five minutes are read
(`Store::boot_spans`). Two of them whose spans overlap (`a.first < b.last` and `b.first < a.last`)
add one warning to the stick's `warnings`, which the dashboard already shows. The warning goes by
itself five minutes after one of the two stops.

**Not done.** No heartbeat is refused and no credential is retired: a wrong guess must not take a
working stick offline during a contest. Retiring stays an organizer's decision. The Hub does not
store client addresses, so the warning cannot say where the second machine is.

**Limits.** The check needs two different boot ids, so a copy resumed from a running snapshot (same
boot id) is not caught by it; its heartbeats only collide on sequence numbers. A boot that was already
running when migration 0021 was applied starts its span at its first heartbeat after it. The warning
is not stored: it shows while the overlap is recent and then goes. The stick list asks the store once
per stick; an organization-wide query is the next step if that list gets slow.
