# ck-hub

Keeper Hub is the Contest Keeper server. It receives reports from the contest machines, follows
the contest on the judge (Kilonova, Codeforces, vJudge) and flags submissions that did not come
from a contest machine.

Work in progress. Today the Hub receives signed heartbeats and events from sticks and shows
organizers every stick with its last heartbeat, who is logged in and what needs a look. It has
accounts (sign-up with mail confirmation, password reset), organizations with owner, admin and viewer
roles, invitations, platform staff, and mail through Azure Communication Services. It follows a
Codeforces and VJudge contest and matches the judge's submissions to the stick events; watching Kilonova is
not built.

To deploy it, see [ck-infra](https://github.com/contest-keeper/ck-infra) (Azure or Docker Compose).

## Run it

```sh
cargo run -p ck-hub -- --registry ck-registry.json --log hub.log
```

- `--registry` is the file `ck-forge` writes. It holds every stick's secret, so keep it private. A
  stick added to it later is known without restarting the Hub.
- `--listen` (or `CK_LISTEN`) defaults to `127.0.0.1:8088`; the container image sets `0.0.0.0:8088`. A QEMU test machine reaches the host as `10.0.2.2`.
- `--database-url` (or the environment variable `CK_DATABASE_URL`) is a PostgreSQL address such as
  `postgres://user:password@localhost/ckhub`. The Hub creates its tables itself on start. Without it
  everything is kept in memory and lost when the Hub stops, and it says so in its log. The connection
  is encrypted when the database offers TLS and always with `?sslmode=require` (the certificate is
  checked against the public web roots, which covers Azure Database for PostgreSQL). Add
  `?sslmode=disable` for a database on the same machine that has no TLS.
- `--mail console --public-url http://127.0.0.1:8088` (or `CK_MAIL` and `CK_PUBLIC_URL`) lets the Hub
  send mail, which sign-up and password reset need. `console` prints each mail, links included, to the
  terminal: use it on a developer's machine only. Without mail the two routes answer `503`.
- `--mail acs --public-url https://hub.example.org --mail-endpoint https://NAME.communication.azure.com
  --mail-from noreply@mail.example.org` (or `CK_MAIL`, `CK_PUBLIC_URL`, `CK_MAIL_ENDPOINT`,
  `CK_MAIL_FROM`) sends through Azure Communication Services Email. The sender must be an address of a
  domain verified in that service. The service's access key is read from the environment variable
  `CK_ACS_KEY` only, never from a flag, so it is not in a process list, and it is never logged. Mail is
  queued and sent from a background thread: a slow or unavailable Azure does not slow or fail a request,
  and a mail that cannot be sent is logged with its error (never its text) and dropped.
- `--codeforces-contest ID --codeforces-organization SLUG` makes the Hub poll that Codeforces contest and match its
  submissions to the organization's stick events. It works anonymously; for a gym or group contest set
  `CK_CODEFORCES_KEY` and `CK_CODEFORCES_SECRET` (environment only, never logged or stored).
- `--vjudge-contest ID --vjudge-organization SLUG` (or `CK_VJUDGE_CONTEST` and `CK_VJUDGE_ORGANIZATION`) does the
  same for a VJudge contest. VJudge's status list is public, so no key is needed. A stick's VJudge submit is
  matched by its run id and by VJudge's own submit time, which the stick reads from VJudge, so the stick's clock
  does not matter. Contest numbers are kept apart per judge; `GET .../contests/{id}/submissions?judge=vjudge`
  is the VJudge contest view.
- `--log` adds one line per heartbeat, accepted or refused. It never holds a secret or a signature.

| Route | What it does |
|---|---|
| `POST /api/v1/login` | A person logs in with `{"email", "password"}`. The answer sets the `ck_session` cookie (`HttpOnly; Secure; SameSite=Strict`, 12 hours). `401` for a wrong email or password, with the same answer for both, and `429` after 5 wrong passwords for one email (locked for 5 minutes) or 60 in a minute for all emails. |
| `POST /api/v1/register` | Makes an account from `{"email", "password"}` and mails a link to confirm the address. `202` with the same answer whether or not the address already has an account (an existing one is told by mail instead), `400` for a bad email or a password under 12 characters, `503` when the Hub cannot send mail. Logging in before the link is used answers `403`. |
| `POST /api/v1/verify-email` | Confirms an address with `{"token"}` from the link. `204`, or `400` for a link that was used, ended (24 hours) or never existed. |
| `POST /api/v1/password-reset` | Mails a reset link to `{"email"}` when it has an account. Always `202`, so it does not tell who has one. |
| `POST /api/v1/password-reset/confirm` | Sets a new password with `{"token", "password"}`. `204`, or `400` for a bad link (1 hour, once) or a short password, which leaves the link usable. It ends every session of the account and confirms the address. |
| `POST /api/v1/logout` | Ends the session on the server and clears the cookie. |
| `GET /api/v1/me` | Who the session belongs to and the organizations they are in, each with their role there (`role` is also given when there is exactly one). `401` without a valid session. |
| `GET /` | The page organizers watch: every stick with its status, last heartbeat, computer name, uptime and recorder. It reads the sticks of the chosen organization every 5 seconds, and shows a selector when the account is in several. It is one file with no outside resources, so it works with no internet. |
| `POST /api/v1/heartbeat` | A stick sends a heartbeat. The body is signed with the stick's secret (HMAC-SHA256, hex) in `X-CK-Signature`, and the stick id is in `X-CK-Stick`. `400` for a malformed message, `401` for an unknown stick or a bad signature (the two look the same), `409` for a sequence number that is not newer within the same boot. |
| `POST /api/v1/events` | A stick sends one event (a login, a submission), signed like a heartbeat. Events of one boot form a chain. A gap or a broken link does not refuse the event: it is kept with a note and shown as a warning. `409` for an event the Hub already has, which the stick counts as delivered. |
| `POST /api/v1/orgs/{slug}/sticks` | Admin or owner of the organization. Registers sticks: one stick object, or a whole registry file of the admin tool (`{"sticks": [...]}`, extra fields are ignored, up to 2 MB). Safe to send twice. A stick that is already known with a different secret is refused with `409` and left as it was; a secret is never replaced. Nothing is registered if any stick in the request is invalid. A stick id already used by another organization is refused like a different secret. The answer says how many were added and never contains a secret. |
| `GET /api/v1/orgs/{slug}/sticks` | A member of the organization (any role). Every stick of the organization, with its last heartbeat and whether it is online (a heartbeat in the last 60 seconds). `404` for an unknown organization and for one the account is not in, with the same answer for both. |
| `POST /api/v1/orgs/{slug}/invitations` | Admin or owner. Invites `{"email", "role"}` by mail with a link that works once for 7 days. An owner may invite admins and viewers, an admin viewers: only roles below one's own, and nobody invites an owner. `202` with the same answer for an address with an account, one without and one already in the organization, `403`, `404`, `400` for a bad email or role, `503` without mail. Inviting the same address again replaces the open invitation. |
| `GET /api/v1/orgs/{slug}/invitations` | Admin or owner. The open invitations: id, email, role, when the link ends. |
| `DELETE /api/v1/orgs/{slug}/invitations/{id}` | Admin or owner, for roles below their own. `204`, `403` or `404`. |
| `POST /api/v1/invitations/accept` | Logged in. `{"token"}` from the link. The account's email must be the invited one: `403` for another account, which leaves the invitation open; `400` for a link that was used, ended or never existed; `200` with the organization and the role. |
| `GET /api/v1/orgs/{slug}/members` | Admin or owner. `[{user_id, email, role}]`. |
| `DELETE /api/v1/orgs/{slug}/members/{user_id}` | Anyone may remove themselves; otherwise only a higher role removes a lower one. `409` for the last owner, who stays. `204`, `403`, `404`. |
| `POST` and `GET /api/v1/staff/organizations` | Platform staff only (`404` for everyone else, `401` without a session). `POST {"slug", "name", "owner_email"}` makes an organization and mails its first owner an invitation (`202`, `400`, `409` for a taken slug); `GET` lists slugs and names. Staff see nothing inside an organization and are not members of the ones they make. |
| `GET /api/v1/profiles`, `GET`, `PUT` and `DELETE /api/v1/profiles/{id}`, `GET /api/v1/profiles/{id}/wallpaper` | Keeper Forge contest profiles of the caller's organization (a session of an admin in exactly one organization, like the route above; `401`, `403` for a viewer, `409` for several organizations). A profile of another organization is `404`. `PUT {"name", "settings": <json>, "wallpaper": {"name", "data_base64"} | null}` creates or replaces the profile with the id the client made (1 to 64 of `A-Za-z0-9_-`): `204`, `400` for a bad id, name (1 to 200 characters), wallpaper name (a plain `.png`, `.jpg` or `.jpeg` name, no `/` or `\`, not `.` or `..`) or a picture that is not what its name says, `413` for settings over 256 KiB or a picture over 5 MB. `GET` answers `{"id", "name", "settings", "wallpaper": {"name"} | null, "updated_at"}`; the list answers `[{"id", "name", "updated_at"}]`; the wallpaper route answers the bytes with `image/png` or `image/jpeg`. The wallpaper bytes are kept in the profile's row (`bytea`), so a profile is one row. |


## Tests with PostgreSQL

The shared store tests always run on memory. They run on PostgreSQL too when `CK_TEST_DATABASE_URL`
is set, for example against a throwaway local container:

```sh
docker run -d --name ck-hub-test-db -e POSTGRES_PASSWORD=ck-test-only -e POSTGRES_DB=ckhub   -p 127.0.0.1:54329:5432 postgres:17-alpine
CK_TEST_DATABASE_URL=postgres://postgres:ck-test-only@127.0.0.1:54329/ckhub cargo test
```

Without the variable those tests print `skipped: ...` (run `cargo test -- --nocapture` to see it) and
pass. Set `CK_TEST_DATABASE_REQUIRED=1` to make a missing database a failure; CI does.

Events in the database are append-only: the database itself refuses to change or delete them, so
every test uses ids no earlier run used.

## Accounts and organizations

People belong to organizations. The page and the stick list show one organization at a time.
Accounts need the database. Staff make them from the command line:

```sh
ck-hub create-user ana@example.org --database-url postgres://user:password@localhost/ckhub
ck-hub create-org rcpc --name "RCPC" --owner ana@example.org --database-url postgres://...
ck-hub add-member rcpc bo@example.org --role viewer --database-url postgres://...
```

The password (at least 12 characters) is asked for at a prompt without echo, twice. It is never taken
from an argument or the environment, so it is not left in a shell history or a process list. The
roles are `owner`, `admin` and `viewer`; an owner may do everything an admin may. Only an Argon2id
hash of the password is stored, and only a hash of each session token, so a copy of the database
cannot be used to log in.

Everything a stick reports belongs to one organization. Row-level security keeps organizations apart
in the database, on top of the filters of every query. It does not apply to a superuser or a role
with `BYPASSRLS`, so after it migrated the Hub switches the connection to the plain role `ck_hub_app`
(`SET ROLE`, made by migration 0008, also after every reconnect), whoever logged in: your own server
admin may be the account in `--database-url`. That role cannot log in, change the schema or the
migration record, or rewrite the event log. The Hub warns at start if the switch did not leave row
security in force. Migrations still need the account that logged in, so it keeps rights to change the
schema; it is the Hub's own queries that run as the plain role.

Platform staff are accounts with a flag, set from the command line:

```sh
ck-hub make-staff ana@example.org --database-url postgres://...
ck-hub revoke-staff ana@example.org --database-url postgres://...
```

A staff account sees a Platform panel on the page where it makes an organization and invites its
first owner by mail.

Owners and admins invite people from the People panel under the stick list (or the routes above).
The invited person opens the link, logs in or signs up with the address the invitation was sent to,
and joins with the invited role. The link carries its token after the `#`, works once, ends after 7
days and is kept only as a hash. Invitation mail counts against the same mail limits as other mail.

Anyone can sign up on the page when the Hub can send mail, but an account has no access until an
owner invites it or staff add it to an organization (`ck-hub add-member`); the page says so. Links in the mail
carry their token after the `#`, so it never reaches a server log and a mail scanner that opens the
link does not use it up. A link works once. At most 3 mails go to one address in 5 minutes and 120
in a minute from the whole Hub; over that, nothing is sent and the answer stays the same. Accounts
nobody confirmed are removed after 48 hours. Accounts made from the command line, and every account
that existed before sign-up, count as confirmed.

A database from before organizations is moved into an organization with the slug `default`: former
admins become its owners, former viewers stay viewers, and they log in with the name they had. The
sticks of a `--registry` file belong to `default` too.

The cookie is `Secure`, so run the Hub behind HTTPS (the browser also accepts it on `localhost`).
Sticks do not use accounts: their heartbeats and events are signed with their own secret.
A Hub started without a database has no accounts, so nobody can look at it.

## Registering sticks

A stick is known to the Hub when its id and secret are in the database or in the `--registry` file
(or both). The normal way is that the admin tool sends its registry over HTTPS with an admin
account (`ck-forge registry push`, or the Hub page of the window; both use `POST /api/v1/orgs/{slug}/sticks`).
The fallback, for an admin with no connection, is to bring the registry file to the Hub:

```sh
ck-hub import-registry ck-registry.json --org rcpc --database-url postgres://...
```

or to send the file to the same endpoint as an admin. Both are safe to repeat.

The database holds each stick's secret, because the Hub checks the signatures with it. Treat the
database like the registry file: private, backed up, and not shared. A later step replaces the
secrets with public keys, so the Hub no longer holds anything that can sign for a stick.
