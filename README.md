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
- `--log` adds one line per heartbeat, accepted or refused. It never holds a secret or a signature.

| Route | What it does |
|---|---|
| `POST /api/v1/heartbeat` | A stick sends a heartbeat. The body is signed with the stick's secret (HMAC-SHA256, hex) in `X-CK-Signature`, and the stick id is in `X-CK-Stick`. `400` for a malformed message, `401` for an unknown stick or a bad signature (the two look the same), `409` for a sequence number that is not newer within the same boot. |
| `GET /api/v1/sticks` | Every stick the admin made, with its last heartbeat and whether it is online (a heartbeat in the last 60 seconds). |

