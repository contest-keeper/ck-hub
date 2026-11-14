# Accounts and organizations on the Hub

Date: 2026-11-14. Status: draft, waiting for review.

## Goal

Move Keeper Hub from "each organizer runs their own Hub, accounts made by CLI" to a cloud Hub
(`hub.contest-keeper.com`) that many organizations share. Users register and log in, belong to
organizations, and all sticks, heartbeats and events are stored in the cloud, separated by organization.

An organization that wants more privacy gets a dedicated Hub (for example `rcpc.contest-keeper.com`):
a separate deployment with its own database. It runs the same image and the same schema as the shared
Hub, with one organization in it. There is no separate dedicated mode in the code.

## Decisions

- Tenancy: one shared database, an `organization_id` on every tenant row, filtering in the store layer,
  and Postgres row-level security as a second guard. Dedicated Hubs use their own database.
- Sign-up: email and password with email verification, plus Google and Microsoft sign-in.
- Organizations are created by platform staff, who name the first owner. After that, membership is by
  invitation only. A user without an organization has no access.
- Roles: platform staff (global), and per organization `owner`, `admin`, `viewer`. A user may belong to
  several organizations.
- Platform staff do not see inside organizations. They create organizations and manage accounts, not data.

## Scope

In: accounts, organizations, memberships, invitations, sign-in, scoping all Hub data by organization,
migration of existing data.

Out (later pieces): Forge changes (login per organization, SSO users, API tokens), provisioning and DNS
for dedicated Hubs, billing, moving an organization between Hubs, the dashboard UI for invitations and
members beyond what login and organization switching need.

## Data model

| Table | Columns and rules |
|---|---|
| `users` | `id` uuid, `email` unique (case-insensitive), `password_hash` nullable (SSO-only users have none), `email_verified_at`, `is_platform_staff`, `created_at` |
| `identities` | `user_id`, `provider` (`google` or `microsoft`), `subject`; `(provider, subject)` unique. Linked to an existing account only when the provider reports the same verified email |
| `organizations` | `id`, `slug` unique (later the subdomain label of a dedicated Hub), `name`, `created_at` |
| `memberships` | `(organization_id, user_id)` primary key, `role` in `owner`, `admin`, `viewer`. Every organization keeps at least one owner |
| `invitations` | `id`, `organization_id`, `email`, `role`, `token_hash`, `expires_at` (7 days), `accepted_at`, `invited_by` |
| `sessions` | `token_hash`, `user_id`, `expires_at`. The working organization is chosen per request and checked against memberships, not stored in the session |
| `sticks`, `heartbeats`, `events` | add `organization_id NOT NULL`. A stick id stays globally unique, so the Hub finds the organization from the id when a signed heartbeat arrives |

Row-level security: each request runs in a transaction that sets the organization (`SET LOCAL`). Policies
allow only rows of that organization. Stick ingestion looks the stick up through one narrow function that
returns its organization, then continues inside that organization.

## Flows and API

- Register: `POST /api/v1/register` (email, password) sends a verification link. No login until verified.
- SSO: standard OpenID Connect at `/api/v1/auth/{provider}/start` and `/callback`. Only verified emails
  are accepted.
- Login: `POST /api/v1/login` takes email and password. The `ck_session` cookie, Argon2id hashing and the
  lockout rules stay as today, with the lockout keyed on email.
- Password reset: `POST /api/v1/password-reset` and a confirm route. Short-lived token, stored hashed.
- Email goes through a small mail interface: a real provider in the cloud, console output in tests and
  local runs.
- Staff: `ck-hub create-org <slug> --owner <email>` (and a staff-only route) creates the organization
  and invites its first owner. `ck-hub make-staff <email>` marks a staff account.
- Invitations: `POST /api/v1/orgs/{slug}/invitations` and `POST /api/v1/invitations/accept`. Owners invite
  admins and viewers, admins invite viewers. A person without an account signs up with the invited email,
  then accepts.
- Scoped routes: sticks and events under `/api/v1/orgs/{slug}/...`. The Hub checks membership and role,
  then runs the request in that organization's row-level security context.
- Sticks are unchanged: signed heartbeats and events go to the same endpoints.
- A non-member and an unknown organization both get `404`, so organizations cannot learn about each other.

## Migration of existing Hubs

A migration creates one default organization and moves existing sticks, heartbeats and events into it.
Existing users have no email, so they are asked to add one at first sign-in and become members of the
default organization with their current role (`admin` becomes `owner` or `admin`, to be decided when the
migration is written).

## Testing

As today: memory store always, Postgres when `CK_TEST_DATABASE_URL` is set. New tests cover registration
and verification, login lockout, SSO with a fake provider, invitations and role limits, and the
staff-only organization routes. The key tests are isolation: a member of organization A gets `404` for
organization B's sticks and events, and a query with no organization context returns nothing under
row-level security.

## Open items

- Which mail provider the cloud deployment uses.
- Whether current `admin` users become `owner` in the default organization.
