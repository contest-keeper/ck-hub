# ck-hub

Keeper Hub is the Contest Keeper server. It receives reports from the contest machines, follows
the contest on the judge (Kilonova, Codeforces, vJudge) and flags submissions that did not come
from a contest machine.

Work in progress. Today the Hub receives signed heartbeats and events from sticks and shows
organizers every stick with its last heartbeat, who is logged in and what needs a look. Watching
the judge and matching submissions are not built yet.

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
  is encrypted when the database offers TLS and always with `?sslmode=require` (the certificate is
  checked against the public web roots, which covers Azure Database for PostgreSQL). Add
  `?sslmode=disable` for a database on the same machine that has no TLS.
- `--log` adds one line per heartbeat, accepted or refused. It never holds a secret or a signature.

| Route | What it does |
|---|---|
| `POST /api/v1/login` | An organizer logs in with `{"username", "password"}`. The answer sets the `ck_session` cookie (`HttpOnly; Secure; SameSite=Strict`, 12 hours). `401` for a wrong name or password, with the same answer for both, and `429` after 5 wrong passwords for one name (locked for 5 minutes) or 60 in a minute for all names. |
| `POST /api/v1/logout` | Ends the session on the server and clears the cookie. |
| `GET /api/v1/me` | Who the session belongs to and their role. `401` without a valid session. |
| `GET /` | The page organizers watch: every stick with its status, last heartbeat, computer name, uptime and recorder. It reads `/api/v1/sticks` every 5 seconds. It is one file with no outside resources, so it works with no internet. |
| `POST /api/v1/heartbeat` | A stick sends a heartbeat. The body is signed with the stick's secret (HMAC-SHA256, hex) in `X-CK-Signature`, and the stick id is in `X-CK-Stick`. `400` for a malformed message, `401` for an unknown stick or a bad signature (the two look the same), `409` for a sequence number that is not newer within the same boot. |
| `POST /api/v1/events` | A stick sends one event (a login, a submission), signed like a heartbeat. Events of one boot form a chain. A gap or a broken link does not refuse the event: it is kept with a note and shown as a warning. `409` for an event the Hub already has, which the stick counts as delivered. |
| `POST /api/v1/sticks` | Admin only. Registers sticks: one stick object, or a whole registry file of the admin tool (`{"sticks": [...]}`, extra fields are ignored, up to 2 MB). Safe to send twice. A stick that is already known with a different secret is refused with `409` and left as it was; a secret is never replaced. Nothing is registered if any stick in the request is invalid. The answer says how many were added and never contains a secret. |
| `GET /api/v1/sticks` | Needs a session (any role). Every stick the admin made, with its last heartbeat and whether it is online (a heartbeat in the last 60 seconds). |


## Tests with PostgreSQL

The shared store tests always run on memory. They run on PostgreSQL too when `CK_TEST_DATABASE_URL`
is set, for example against a throwaway local container:

```sh
docker run -d --name ck-hub-test-db -e POSTGRES_PASSWORD=ck-test-only -e POSTGRES_DB=ckhub   -p 127.0.0.1:54329:5432 postgres:17-alpine
CK_TEST_DATABASE_URL=postgres://postgres:ck-test-only@127.0.0.1:54329/ckhub cargo test
```

Events in the database are append-only: the database itself refuses to change or delete them, so
every test uses ids no earlier run used.

## Organizer accounts

The page and the stick list are only for organizers. Accounts need the database:

```sh
ck-hub create-user ana --database-url postgres://user:password@localhost/ckhub
```

The password (at least 12 characters) is asked for at a prompt without echo, twice. It is never taken
from an argument or the environment, so it is not left in a shell history or a process list. `--role`
is `admin` (default) or `viewer`. Only an Argon2id hash of the password is stored, and only a hash of
each session token, so a copy of the database cannot be used to log in.

The cookie is `Secure`, so run the Hub behind HTTPS (the browser also accepts it on `localhost`).
Sticks do not use accounts: their heartbeats and events are signed with their own secret.
A Hub started without a database has no accounts, so nobody can look at it.

## Registering sticks

A stick is known to the Hub when its id and secret are in the database or in the `--registry` file
(or both). The normal way is that the admin tool sends its registry over HTTPS with an admin
account (`ck-forge registry push`, or the Hub page of the window; both use `POST /api/v1/sticks`).
The fallback, for an admin with no connection, is to bring the registry file to the Hub:

```sh
ck-hub import-registry ck-registry.json --database-url postgres://...
```

or to send the file to the same endpoint as an admin. Both are safe to repeat.

The database holds each stick's secret, because the Hub checks the signatures with it. Treat the
database like the registry file: private, backed up, and not shared. A later step replaces the
secrets with public keys, so the Hub no longer holds anything that can sign for a stick.
