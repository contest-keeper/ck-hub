# Hub capacity (C8)

Measured with `crates/ck-hub/examples/loadsim.rs` (a test tool, not shipped): N simulated sticks, each with its
own secret, send a signed heartbeat every 15 s and a network event of 20 requests every 10 s. Target contest
size given by the owner: **up to 500 sticks**.

## What was measured, and what it is not

- Hub release build on the developer PC (Windows), PostgreSQL 17 in Docker limited to 1 CPU, simulator on the same
  machine, 10 minutes per level (not 30), default logging (one line per message).
- This is **not Azure**. The Azure Hub has 0.25 vCPU / 0.5 GiB and a burstable `B1ms` database in the same region;
  here the database is a container reached through Docker's network. Latency per SQL statement differs, so the
  absolute numbers do not carry over. The shape does: where the Hub bends and why.
- Burst credits of `B1ms` are not modelled. Not run: 1000 sticks (not needed for the 500 target), Azure itself.

## Results

| Sticks | Offered load | Heartbeat p50 / p95 / p99 | Event p50 / p95 / p99 | Errors | Hub CPU | Hub memory |
|---|---|---|---|---|---|---|
| 100 | 17 msg/s | 26 / 38 / 59 ms | 18 / 34 / 48 ms | 0 | 0.06 core | 14 MB |
| 300 | 50 msg/s | 7.2 / 9.0 / 10.4 s | 7.1 / 8.9 / 10.2 s | 1 (simulator ran out of local ports) | 0.18 core | 44 MB |
| 500 | 83 msg/s | 14.5 / 19.5 / 22.9 s | 14.4 / 19.4 / 21.7 s | 0 | 0.16 core | 64 MB |

## Where it bends

- It bends **between 100 and 300 sticks**, at about **40 messages per second** served (300 sticks managed 42/s,
  500 sticks 36/s, while 50 and 83 were offered). Past that point requests queue and latency grows without bound
  (seconds); the heartbeat timeout of a real stick is shorter than that, so sticks would show as offline.
- The Hub is **not CPU-bound** (0.16 to 0.18 core, memory small). The limit is serial work: one PostgreSQL
  connection behind a `Mutex` in `postgres_store.rs`, plus the Hub's `write` lock that lets one heartbeat or event
  be checked and stored at a time. At about 25 ms per message (the 100-stick median) one connection can do about
  40 messages per second, which is exactly what was seen. On Azure the per-message time will be lower (same-region
  database), so the ceiling is higher, but it is the same single lane and is not measured.
- Memory is not a concern at 500 sticks (64 MB of 512 MB).

## Recommendation for 500 sticks (not built yet)

1. **Do not run 500 sticks on the current Hub.** 83 messages per second is about twice what one connection served.
2. Cheapest fixes, in order:
   1. A connection pool (for example 8 connections) and take the global `write` lock out of the path: the per-stick
      sequence check only needs a per-stick guard. Expected effect: roughly the pool size times more throughput
      until the database is the limit.
   2. Fewer messages: `heartbeat_s` 30 s and a network event every 20 s for large contests (settings only) halves
      the load: 500 sticks then offer about 42 messages per second.
   3. Batch event inserts (one statement per event today) and keep the heartbeat to one statement.
   4. A non-burstable database tier on contest days. Sampling or summarising network history on the stick
      (request to lane B) cuts event volume further.
3. Re-run this test on the Azure develop Hub after 2.1, at 100, 300, 500 sticks for 30 minutes each, and replace the
   table above with those numbers.

## How to repeat

```text
cargo build --release --bin ck-hub --example loadsim
loadsim registry --sticks 500 --out registry.json
CK_DATABASE_URL=postgres://... ck-hub --registry registry.json --listen 127.0.0.1:8099
loadsim run --hub http://127.0.0.1:8099 --registry registry.json --minutes 30
```

Sample the Hub's CPU and memory from outside (Task Manager, `docker stats`). The simulator opens a new
connection per message, so on Windows a very long run can exhaust local ports (the one error at 300 sticks).
