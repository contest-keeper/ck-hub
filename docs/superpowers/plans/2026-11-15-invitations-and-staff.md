# Invitations, Members and Platform Staff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Organizations grow by invitation, with no command line: an owner or admin invites a person by email, the person signs up or logs in and accepts, and platform staff create organizations and name their first owner through the Hub. This is piece 4 of `docs/superpowers/specs/2026-11-14-accounts-and-organizations-design.md`.

**Architecture:** A new `invitations` table keeps a hash of a mailed one-time token, the organization, the invited email, the role and an expiry. Accepting needs a logged-in account whose email is the invited one, so a forwarded link alone proves nothing. Members are listed and removed through routes under the organization; a rule keeps at least one owner. Platform staff are accounts with `is_platform_staff` set (`ck-hub make-staff`); they create organizations and see only names and slugs, never data inside. All of it reuses what pieces 1 to 2 built: `Hub::access` for roles, `Mailer` and the per-address mail limit for the mail, and the `#fragment` link pattern of the verification mails.

**Tech Stack:** Rust 2024, axum 0.8, the sync `postgres` client behind the `Store` trait (memory and PostgreSQL), plain SQL migrations.

## Decisions made here (change them before starting if you disagree)

- **Who may invite whom** (as in the spec): owners invite admins and viewers, admins invite viewers. Nobody invites an owner: the first owner comes from staff, and more owners come from staff too. Nobody can raise their own role.
- **Accepting needs the matching account.** The logged-in account's email (case-insensitive) must equal the invitation's. A person with no account registers first (piece 2), confirms the email, and then accepts. Someone else holding the link learns nothing and changes nothing.
- **The answer to an invitation never says whether the address has an account.** Always `202`, with the same text. The mail differs only in its words.
- **One open invitation per organization and email.** Inviting again replaces it (new token, new expiry) and counts against the mail limit. Invitations last 7 days and work once.
- **An organization keeps at least one owner.** Removing the last owner is refused with `409`. An owner may remove admins and viewers, admins may remove viewers, and anyone may remove themselves unless they are the last owner.
- **Staff see no organization data.** The staff routes return slugs, names and creation facts only. A staff account is not a member of anything unless it was invited like anyone else. `ck-hub make-staff` and `revoke-staff` are the only ways to change the flag.
- **Staff create an organization with an invitation, not a membership:** `POST /api/v1/staff/organizations {slug, name, owner_email}` makes the organization and invites the first owner by email (role `owner`, the one place an owner is invited). The command line `create-org` keeps working for an account that exists.
- **Small interface.** The page gets an accept step for `#invite=` links and a "People" panel for owners and admins (members, invite form, remove). Nothing for staff beyond the routes.

## File Structure

- Create `crates/ck-hub/migrations/0009_invitations_and_staff.sql`.
- Modify `crates/ck-hub/src/store.rs`, `postgres_store.rs`: types and trait methods, memory and PostgreSQL implementations.
- Create `crates/ck-hub/src/invite.rs`: `Hub::invite`, `accept_invitation`, `invitations_of`, `revoke_invitation`, `members_of`, `remove_member`, `staff_create_organization`, `staff_organizations`.
- Modify `crates/ck-hub/src/accounts.rs` (`Account.staff`, `AccessError::NotStaff` is `Forbidden`), `routes.rs`, `main.rs` (`make-staff`, `revoke-staff`), `page.html`, `lib.rs`, `README.md`.
- Create `crates/ck-hub/tests/invitations.rs`; extend `tests/store.rs`.

---

### Task 1: Store: invitations, members and the staff flag

**Files:** `migrations/0009_invitations_and_staff.sql`; `store.rs`; `postgres_store.rs`; test `tests/store.rs`.

- [ ] **Step 1: Migration.**

```sql
ALTER TABLE users ADD COLUMN is_platform_staff boolean NOT NULL DEFAULT false;

CREATE TABLE invitations (
    id              text PRIMARY KEY DEFAULT gen_random_uuid()::text,
    organization_id text        NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    email           text        NOT NULL,
    role            text        NOT NULL CHECK (role IN ('owner', 'admin', 'viewer')),
    token_hash      text        NOT NULL UNIQUE,
    invited_by      text        REFERENCES users (id) ON DELETE SET NULL,
    expires_at      timestamptz NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);
-- One open invitation per organization and address; inviting again replaces it.
CREATE UNIQUE INDEX invitations_one_open ON invitations (organization_id, lower(email));
CREATE INDEX invitations_by_expiry ON invitations (expires_at);
```

The Hub's own role already has rights on new tables through the default privileges of migration 0008.

- [ ] **Step 2: Failing contract test** `invitations_contract(store)` in `tests/store.rs`, called from the identity contract like `verification_contract`. It pins: `put_invitation` replaces the open one for the same organization and email (capitals do not matter); `take_invitation(hash, now)` returns it once, only before it ended, and removes it either way; `invitations_of(org)` lists the open ones with who invited; `delete_invitation(org, id)`; `delete_expired_invitations(now)`; `members_of(org)` returns `(user id, email, role)` ordered by email; `remove_member(org, user)` removes and says whether it did; `count_owners(org)`; `set_staff(user_id, bool)` and `User.is_platform_staff` read back; `users_by_staff` is not needed.
- [ ] **Step 3: Implement.** Types in `store.rs`:

```rust
/// An invitation to join an organization. `token_hash` is never given back, only what is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    pub id: String,
    pub organization_id: String,
    pub email: String,
    pub role: Role,
    pub invited_by: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: String,
    pub email: String,
    pub role: Role,
}
```

`User` gets `is_platform_staff: bool`; `NewUser` stays (staff are made by `set_staff`). Trait methods: `put_invitation(&self, organization_id, email, role, token_hash, invited_by: Option<&str>, expires_at) -> Result<Invitation, StoreError>`; `take_invitation(&self, token_hash, now) -> Result<Option<Invitation>, StoreError>`; `invitations_of`, `delete_invitation`, `delete_expired_invitations`, `members_of`, `remove_member`, `count_owners`, `set_staff`, `organizations(&self) -> Result<Vec<Organization>, StoreError>` (ordered by slug, for staff). PostgreSQL: `put_invitation` is one transaction (delete the open one for `lower(email)`, insert); `take_invitation` is `DELETE ... RETURNING`, expiry checked in Rust like `take_email_token`. Every user select gains `is_platform_staff`.
- [ ] **Step 4:** run on memory and PostgreSQL. **Step 5: Commit** `feat: keep invitations, members and the staff flag in the store`.

### Task 2: Inviting, accepting, members (Hub logic)

**Files:** create `src/invite.rs`; modify `accounts.rs`, `lib.rs`; test `tests/invitations.rs`.

- [ ] **Step 1: Failing tests** (memory store, `MemoryMailer`, cheap hashing, a clock that advances), each with the name of the rule it pins:
  - `an_owner_invites_an_admin_and_an_admin_invites_a_viewer_and_nobody_invites_more_than_their_own_level`: owner→admin ok, owner→viewer ok, owner→owner `Forbidden`, admin→viewer ok, admin→admin `Forbidden`, viewer→anyone `Forbidden`, a non-member gets `NotFound`.
  - `an_invitation_is_accepted_only_by_the_account_with_that_address_and_only_once`: wrong account `Forbidden`-like error and the invitation stays; the right one becomes a member with the invited role; a second use `Invalid`; an expired one (7 days) `Invalid`.
  - `inviting_again_replaces_the_open_invitation_and_the_old_link_stops_working`.
  - `inviting_an_unknown_address_and_a_member_look_the_same_to_the_caller` (both `Ok`, nothing about accounts in the result); the mail of a member already in the organization says so and carries no link.
  - `invitation_mail_is_limited_like_other_mail` (reuse `mail_allowed`).
  - `members_are_listed_and_removed_by_the_rules_and_the_last_owner_stays`: admin removes viewer, not admin; owner removes admin; self-removal ok; the last owner `LastOwner`.
  - `invitations_can_be_listed_and_revoked_by_those_who_may_make_them`.
- [ ] **Step 2: Implement.**

```rust
pub const INVITATION_LIFETIME_DAYS: i64 = 7;

#[derive(Debug, PartialEq, Eq)]
pub enum InviteError { NoMail, BadEmail, Forbidden, NotFound, Unavailable }
#[derive(Debug, PartialEq, Eq)]
pub enum AcceptError { Unauthorized, WrongAccount, Invalid, Unavailable }
#[derive(Debug, PartialEq, Eq)]
pub enum MemberError { Forbidden, NotFound, LastOwner, Unavailable }

impl Role {
    /// The roles a member of this role may invite or remove.
    pub fn may_manage(self, other: Role) -> bool { self.rank() > other.rank() }
}
```

`Role::rank` exists (owner > admin > viewer); "owners invite admins and viewers, admins invite viewers" is exactly `rank(me) > rank(them)`, so an owner cannot invite an owner and nobody outranks themselves. `Hub::invite(token, slug, email, role)`: `access(token, slug, Role::Admin)` (admins and owners may open the panel), refuse `!inviter.role.may_manage(role)`; validate the email with `valid_email`; delete expired invitations; generate a token like the verification ones, store its hash with a 7-day expiry through `put_invitation`; send the mail through `mail_allowed` and `send` (same helpers as `signup.rs`, make them `pub(crate)`), with the link `{base}/#invite={token}` and the organization's name in the text; if the address already is a member, send a notice without a link instead. `accept_invitation(session_token, invite_token)`: `account(session)?`; `take_invitation` (consumed only if the account's email equals the invitation's, case-insensitively: check first with a peek by hash, so a wrong account does not use it up, then take); `add_member` (a member already is fine, the invitation is used up). `members_of`, `remove_member` with the rules above and `count_owners`.
- [ ] **Step 3:** run `cargo test -p ck-hub --test invitations`. **Step 4: Commit** `feat: invite people to an organization and let them accept`.

### Task 3: Platform staff

**Files:** `invite.rs`, `accounts.rs`, `main.rs`; tests in `tests/invitations.rs`.

- [ ] **Step 1: Failing tests:** `staff_make_an_organization_and_invite_its_first_owner` (`staff_create_organization(session, slug, name, owner_email)` returns `Ok`, the organization exists, an invitation with role owner is mailed, and the staff account is **not** a member); `only_staff_may` (a normal account gets `Forbidden`, no session `Unauthorized`); `staff_see_slugs_and_names_but_no_sticks` (`staff_organizations` returns names; `access(token, slug, Viewer)` for a staff account that is not a member is still `NotFound`); `a_bad_slug_or_a_taken_slug_is_refused` (reuse `valid_slug` and the store's `None`).
- [ ] **Step 2: Implement.** `Account` gets `staff: bool`, read from the user at `account()`. `Hub::staff_create_organization` checks `account.staff`, calls the existing `create_organization` machinery without an owner (a store call that makes only the organization: `store.create_organization`), then `put_invitation` with role `owner` and `invited_by` the staff account, and mails it. `ck-hub make-staff <email>` and `revoke-staff <email>` call `set_staff` through the `with_store` helper, with the same error style as `add-member`.
- [ ] **Step 3: Commit** `feat: let platform staff make organizations and invite their first owner`.

### Task 4: Routes

**Files:** `routes.rs`; HTTP tests in `tests/invitations.rs`.

| Route | Who | Answer |
|---|---|---|
| `POST /api/v1/orgs/{slug}/invitations` `{email, role}` | admin, owner (and only roles below theirs) | `202 {"status":"invitation sent"}` always, `403`, `404`, `400` bad email or role, `503` without mail |
| `GET /api/v1/orgs/{slug}/invitations` | admin, owner | open invitations: id, email, role, expires |
| `DELETE /api/v1/orgs/{slug}/invitations/{id}` | admin, owner (for roles below theirs) | `204` / `404` |
| `POST /api/v1/invitations/accept` `{token}` | logged in | `200 {"organization":{slug,name},"role"}`, `400` link not valid, `403` another account's invitation, `401` |
| `GET /api/v1/orgs/{slug}/members` | any member | `[{user_id,email,role}]` |
| `DELETE /api/v1/orgs/{slug}/members/{user_id}` | by the rules above or oneself | `204`, `403`, `404`, `409` last owner |
| `POST /api/v1/staff/organizations` `{slug,name,owner_email}` | staff | `202`, `400`, `409` slug taken, `403`, `404`-free: a non-staff gets `404` so the route is not advertised |
| `GET /api/v1/staff/organizations` | staff | `[{slug,name}]`, `404` for non-staff |

`me` also returns `"staff": true` for staff. Error bodies use the existing `Denied` helpers. `POST .../invitations` bodies are small (`MAX_BODY_BYTES`). Tests drive the real router with a session cookie, like `tests/accounts.rs`.
- [ ] **Commit** `feat: add the invitation, member and staff routes`.

### Task 5: The page

**Files:** `page.html`; the page test in `tests/api.rs`.

- [ ] An `#invite=TOKEN` link: after the existing fragment handling, if logged in, `POST /api/v1/invitations/accept` and show the result (the organization name or the error); if not logged in, keep the token in memory, show the login/register form with a line "Log in or register with the address the invitation was sent to", and accept right after login. Strip the fragment like the other links.
- [ ] A "People" panel for owners and admins under the stick list: a table of members (email, role, a Remove button where the rules allow), pending invitations (email, role, a Revoke button), and a form (email, role select limited to the roles below theirs). It reloads after each action and shows the server's `error` text.
- [ ] Page assertions in `tests/api.rs` for the new strings; check in a browser with two accounts. **Commit** `feat: add accepting invitations and a people panel to the page`.

### Task 6: Docs and checks

- [ ] README: routes, the invitation flow, staff (`make-staff`, `revoke-staff`, `POST /api/v1/staff/organizations`), the rules (who invites whom, last owner, 7 days, once, matching account). `cargo fmt --all`, clippy, tests on memory and PostgreSQL, a mutation check (let any account accept an invitation: the matching-account test must fail), an upgrade check of a populated database at 0008, PR into `develop` titled `feat: invite people, list members and let staff make organizations`.

## Self-review against the spec

- Invitations and the accept route, roles and who invites whom, 7 days, hashed token, `invited_by`: Tasks 1 to 4.
- "Platform staff do not see inside organizations": staff routes return names only, `access` is unchanged for staff.
- Staff create organizations and invite the first owner, `make-staff`: Task 3.
- Not here: Google and Microsoft sign-in (the other piece), role changes of existing members (remove and invite again), bulk invitations, audit log of invitations.
