# ck-hub

Keeper Hub is the Contest Keeper server. It receives reports from the contest machines, follows
the contest on the judge (Kilonova, Codeforces, vJudge) and flags submissions that did not come
from a contest machine.

Work in progress. Today the Hub receives signed heartbeats from sticks and lists the sticks with their
last heartbeat.

## Run it

```sh
cargo run -p ck-hub -- --registry ck-registry.json --log hub.log
```

- `--registry` is the file `ck-forge` writes. It holds every stick's secret, so keep it private. A
  stick added to it later is known without restarting the Hub.
- `--listen` defaults to `127.0.0.1:8088`. A QEMU test machine reaches the host as `10.0.2.2`.
- `--database-url` (or the environment variable `CK_DATABASE_URL`) is a PostgreSQL address such as
  `postgres://user:password@localhost/ckhub`. The Hub creates its tables itself on start. Without it
  everything is kept in memory and lost when the Hub stops, and it says so in its log. The connection
  is not encrypted, so run the database next to the Hub.
- `--log` adds one line per heartbeat, accepted or refused. It never holds a secret or a signature.

| Route | What it does |
|---|---|
| `GET /` | The page organizers watch: every stick with its status, last heartbeat, computer name, uptime and recorder. It reads `/api/v1/sticks` every 5 seconds. It is one file with no outside resources, so it works with no internet. |
| `POST /api/v1/heartbeat` | A stick sends a heartbeat. The body is signed with the stick's secret (HMAC-SHA256, hex) in `X-CK-Signature`, and the stick id is in `X-CK-Stick`. `400` for a malformed message, `401` for an unknown stick or a bad signature (the two look the same), `409` for a sequence number that is not newer within the same boot. |
| `GET /api/v1/sticks` | Every stick the admin made, with its last heartbeat and whether it is online (a heartbeat in the last 60 seconds). |


## Tests with PostgreSQL

The shared store tests always run on memory. They run on PostgreSQL too when `CK_TEST_DATABASE_URL`
is set, for example against a throwaway local container:

```sh
docker run -d --name ck-hub-test-db -e POSTGRES_PASSWORD=ck-test-only -e POSTGRES_DB=ckhub   -p 127.0.0.1:54329:5432 postgres:17-alpine
CK_TEST_DATABASE_URL=postgres://postgres:ck-test-only@127.0.0.1:54329/ckhub cargo test
```

Events in the database are append-only: the database itself refuses to change or delete them, so
every test uses ids no earlier run used.
