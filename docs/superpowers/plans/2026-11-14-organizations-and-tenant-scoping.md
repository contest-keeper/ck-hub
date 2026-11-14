# Organizations and Tenant Scoping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Users belong to organizations through memberships with roles, and every stick, heartbeat and event belongs to one organization, kept apart by the store and by Postgres row-level security.

**Architecture:** Plan 1 of 4 for `docs/superpowers/specs/2026-11-14-accounts-and-organizations-design.md`. Accounts get an id and log in by email. A `memberships` table gives each user a role per organization (`owner`, `admin`, `viewer`). Tenant tables get `organization_id`; every store query filters on it explicitly, and row-level security (`ck.org_id` set per transaction) is a second guard. Stick ingestion finds a stick's organization from its id through one narrow lookup policy. Data from before this change moves into an organization with slug `default`.

**Tech Stack:** Rust 2024, axum 0.8, the sync `postgres` crate behind `Mutex<Client>`, Argon2id, plain SQL migrations in `crates/ck-hub/migrations/`.

**Later plans (not here):**
- Plan 2: registration, email verification, password reset, a mail interface.
- Plan 3: Google and Microsoft sign-in (OpenID Connect).
- Plan 4: invitations, `is_platform_staff` and the staff-only routes.
- Separate: Forge changes, provisioning of dedicated Hubs.

**Decisions this plan makes (the spec left them open or silent):**
- A pre-existing `admin` becomes an `owner` of the `default` organization; a `viewer` stays a `viewer`.
- The `default` organization is created by the migration only when the database already holds users or data. A fresh database starts with no organization. A Hub started with `--registry` creates `default` on first use, and the registry file's sticks belong to it.
- Pre-existing accounts keep the name they logged in with in the `email` column until plan 2 adds a way to change it. New accounts must have an email-shaped name.
- `is_platform_staff` is not added here: nothing uses it until plan 4.
- Row-level security does nothing for a superuser or a role with `BYPASSRLS`. The Hub warns at start when its database role is one (Task 4). The cloud deployment must connect as a plain role.
- No commit steps: the user asked not to commit. Project rules still apply when they do: branch from `develop`, `type: lowercase subject`, no attribution lines.

**Run from** `crates/ck-hub`. Checks used throughout:
`cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
For the Postgres tests start the throwaway database from `README.md` and set `CK_TEST_DATABASE_URL`.

---

## File structure

| File | Change |
|---|---|
| `migrations/0005_organizations_and_memberships.sql` | create: organizations, memberships, users by id, sessions by user id |
| `migrations/0006_organization_scoping.sql` | create: `organization_id` and row-level security on tenant tables |
| `src/store.rs` | modify: `Role::Owner`, `Organization`, `Membership`, `NewUser`, new `User`, trait methods, `MemoryStore` |
| `src/postgres_store.rs` | modify: migrations list, identity queries, `in_org`, org-scoped queries |
| `src/accounts.rs` | modify: login by email, `account`, `access` with an organization, `create_organization`, `add_member` |
| `src/hub.rs` | modify: carry the organization through heartbeat, event and stick listing |
| `src/register.rs` | modify: register into an organization |
| `src/routes.rs` | modify: organization-aware access, `/api/v1/orgs/{slug}/sticks`, `me` |
| `src/page.html` | modify: email label, organization switcher |
| `src/main.rs` | modify: `create-user`, new `create-org`, `add-member`, `import-registry --org`, RLS warning |
| `src/lib.rs` | modify: exports |
| `tests/store.rs`, `tests/accounts.rs`, `tests/api.rs`, `tests/sticks.rs` | modify: new contract and isolation tests, helpers |
| `README.md` | modify: accounts section |

---

## Task 1: Identity, organizations and memberships

Tenant data is not scoped yet. All routes keep their paths and resolve the caller's only organization.

**Files:** create `migrations/0005_organizations_and_memberships.sql`; modify `src/store.rs`, `src/postgres_store.rs`, `src/accounts.rs`, `src/routes.rs`, `src/main.rs`, `src/lib.rs`, `tests/store.rs`, `tests/accounts.rs`, `tests/api.rs`, `tests/sticks.rs`.

### Step 1.1: Write the failing store contract test

- [ ] In `tests/store.rs` add `Organization`/`NewUser` imports (`use ck_hub::{..., NewUser, ...}`), replace `accounts_and_sessions` with this, and make `the_store_contract` call `identity_contract(store);` where it called `accounts_and_sessions(store, &a);`:

```rust
fn identity_contract(store: &dyn Store) {
    let id = unique();
    let email = format!("{id}@example.org");
    let new = |email: &str, hash: &str| NewUser {
        email: email.to_owned(),
        password_hash: hash.to_owned(),
    };

    // Accounts: an email is taken once, capitals do not make another one, and it reads back whole.
    assert!(store.user_by_email(&email).expect("read").is_none());
    let user = store
        .create_user(new(&email, "hash-1"))
        .expect("write")
        .expect("a new account");
    assert!(
        store
            .create_user(new(&email, "hash-2"))
            .expect("write")
            .is_none(),
        "never replaced"
    );
    assert!(
        store
            .create_user(new(&email.to_uppercase(), "hash-3"))
            .expect("write")
            .is_none(),
        "capitals do not matter"
    );
    let kept = store
        .user_by_email(&email.to_uppercase())
        .expect("read")
        .expect("a user");
    assert_eq!(
        (kept.id.as_str(), kept.password_hash.as_str()),
        (user.id.as_str(), "hash-1")
    );
    assert_eq!(
        store.user(&user.id).expect("read").expect("a user").email,
        email
    );
    assert!(store.user("no-such-id").expect("read").is_none());

    // Organizations: a slug is taken once.
    let slug = format!("{id}-org");
    let org = store
        .create_organization(&slug, "Test organization")
        .expect("write")
        .expect("a new organization");
    assert!(
        store
            .create_organization(&slug, "Other")
            .expect("write")
            .is_none()
    );
    assert_eq!(
        store.organization_by_slug(&slug).expect("read"),
        Some(org.clone())
    );
    assert_eq!(store.ensure_organization(&slug, "ignored").expect("write"), org);
    assert!(store.organization_by_slug("no-such-slug").expect("read").is_none());

    // Memberships: one role per user and organization, never replaced here.
    assert_eq!(store.membership(&org.id, &user.id).expect("read"), None);
    assert!(
        store
            .add_member(&org.id, &user.id, Role::Admin)
            .expect("write")
    );
    assert!(
        !store
            .add_member(&org.id, &user.id, Role::Viewer)
            .expect("write"),
        "the role is not replaced"
    );
    assert_eq!(
        store.membership(&org.id, &user.id).expect("read"),
        Some(Role::Admin)
    );
    let all = store.memberships(&user.id).expect("read");
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].organization.clone(), all[0].role), (org, Role::Admin));

    // Sessions: found by their hash until they are deleted; deleting twice is fine.
    let session = format!("{id}-session");
    assert!(store.session(&session).expect("read").is_none());
    store
        .create_session(&session, &user.id, time(100))
        .expect("write");
    assert_eq!(
        store.session(&session).expect("read"),
        Some((user.id.clone(), time(100)))
    );
    store.delete_session(&session).expect("delete");
    store.delete_session(&session).expect("delete again");
    assert!(store.session(&session).expect("read").is_none());

    // Expired sessions are removed; the ones still running stay.
    let (old, current) = (format!("{id}-old"), format!("{id}-current"));
    store
        .create_session(&old, &user.id, time(50))
        .expect("write");
    store
        .create_session(&current, &user.id, time(500))
        .expect("write");
    store.delete_expired_sessions(time(100)).expect("delete");
    assert!(store.session(&old).expect("read").is_none());
    assert!(store.session(&current).expect("read").is_some());
    store.delete_session(&current).expect("delete");
}
```

Remove the now unused `Role` import only if the compiler says so (the test above uses it).

### Step 1.2: Run it and watch it fail

- [ ] Run: `cargo test --test store 2>&1 | tail -20`
- Expected: compile errors (`NewUser`, `user_by_email`, `create_organization` not found).

### Step 1.3: Add the migration

- [ ] Create `migrations/0005_organizations_and_memberships.sql`:

```sql
-- Organizations: a club or contest team. Staff create them; people belong to them by membership.
CREATE TABLE organizations (
    id         text PRIMARY KEY DEFAULT gen_random_uuid()::text,
    slug       text        NOT NULL UNIQUE
               CHECK (slug ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    name       text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- A database that already holds accounts or data gets one organization for them. A new database
-- starts with none.
INSERT INTO organizations (slug, name)
SELECT 'default', 'Default'
WHERE EXISTS (SELECT 1 FROM users)
   OR EXISTS (SELECT 1 FROM sticks)
   OR EXISTS (SELECT 1 FROM heartbeats)
   OR EXISTS (SELECT 1 FROM events);

-- Accounts get an id of their own. The old name column becomes the email: accounts made before
-- this keep the name they logged in with until they change it.
ALTER TABLE sessions DROP CONSTRAINT sessions_user_name_fkey;
ALTER TABLE users DROP CONSTRAINT users_pkey;
ALTER TABLE users ADD COLUMN id text NOT NULL DEFAULT gen_random_uuid()::text;
ALTER TABLE users ADD PRIMARY KEY (id);
ALTER TABLE users RENAME COLUMN name TO email;
CREATE UNIQUE INDEX users_email_lower ON users (lower(email));

ALTER TABLE sessions ADD COLUMN user_id text;
UPDATE sessions SET user_id = users.id FROM users WHERE users.email = sessions.user_name;
DELETE FROM sessions WHERE user_id IS NULL;
ALTER TABLE sessions ALTER COLUMN user_id SET NOT NULL;
ALTER TABLE sessions ADD FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE;
ALTER TABLE sessions DROP COLUMN user_name;

CREATE TABLE memberships (
    organization_id text        NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    user_id         text        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role            text        NOT NULL CHECK (role IN ('owner', 'admin', 'viewer')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (organization_id, user_id)
);

-- Whoever managed the Hub before is an owner of the default organization.
INSERT INTO memberships (organization_id, user_id, role)
SELECT organizations.id, users.id, CASE users.role WHEN 'admin' THEN 'owner' ELSE users.role END
FROM users, organizations
WHERE organizations.slug = 'default';

ALTER TABLE users DROP COLUMN role;
```

### Step 1.4: Types and trait in `src/store.rs`

- [ ] Replace the `Role` enum, `User` struct and the user/session part of the `Store` trait.

```rust
/// What a member of an organization may do. Each role may do everything the ones below it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Everything an admin may do, and invites admins.
    Owner,
    /// Manages sticks, and sees everything a viewer sees.
    Admin,
    /// Only looks at the dashboard.
    Viewer,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Viewer => "viewer",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Owner => 3,
            Self::Admin => 2,
            Self::Viewer => 1,
        }
    }

    /// Whether someone with this role may do what `needed` asks for.
    #[must_use]
    pub fn allows(self, needed: Role) -> bool {
        self.rank() >= needed.rank()
    }
}

/// An account. The password is only ever kept as an Argon2 hash. `email` is what the person logs
/// in with; accounts made before organizations existed keep their old name there.
#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
    pub password_hash: String,
}

/// What a store needs to make an account. The store picks the id.
#[derive(Debug, Clone)]
pub struct NewUser {
    pub email: String,
    pub password_hash: String,
}

/// A club or contest team. Everything sticks report belongs to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    pub id: String,
    pub slug: String,
    pub name: String,
}

/// An organization a user belongs to, and the role they have in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub organization: Organization,
    pub role: Role,
}
```

Trait: replace the `create_user`, `user`, `create_session`, `session` methods and add the rest (keep the delete methods as they are):

```rust
    /// Adds an account. Returns `None`, and changes nothing, when the email is taken. Capitals
    /// do not matter.
    fn create_user(&self, user: NewUser) -> Result<Option<User>, StoreError>;

    /// The account with this email, whatever its capitals.
    fn user_by_email(&self, email: &str) -> Result<Option<User>, StoreError>;

    fn user(&self, id: &str) -> Result<Option<User>, StoreError>;

    /// Adds an organization. Returns `None`, and changes nothing, when the slug is taken.
    fn create_organization(&self, slug: &str, name: &str)
    -> Result<Option<Organization>, StoreError>;

    /// The organization with this slug, made if there is none.
    fn ensure_organization(&self, slug: &str, name: &str) -> Result<Organization, StoreError>;

    fn organization_by_slug(&self, slug: &str) -> Result<Option<Organization>, StoreError>;

    /// Makes `user_id` a member. Returns false, and changes nothing, for a member already.
    fn add_member(
        &self,
        organization_id: &str,
        user_id: &str,
        role: Role,
    ) -> Result<bool, StoreError>;

    fn membership(&self, organization_id: &str, user_id: &str) -> Result<Option<Role>, StoreError>;

    /// Every organization the user belongs to, ordered by slug.
    fn memberships(&self, user_id: &str) -> Result<Vec<Membership>, StoreError>;

    /// Starts a session. `token_hash` is a hash of the token the browser holds, never the token.
    fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// The user id a session belongs to and when it ends, if there is such a session.
    fn session(&self, token_hash: &str) -> Result<Option<(String, DateTime<Utc>)>, StoreError>;
```

`MemoryStore`: change the fields and implement the new methods.

```rust
// fields (replace users and sessions, add the rest); add `use std::sync::atomic::{AtomicU64, Ordering};`
    users: Mutex<Vec<User>>,
    sessions: Mutex<HashMap<String, (String, DateTime<Utc>)>>,
    organizations: Mutex<Vec<Organization>>,
    members: Mutex<Vec<(String, String, Role)>>,
    next_id: AtomicU64,
```

```rust
impl MemoryStore {
    fn id(&self, kind: &str) -> String {
        format!("{kind}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}
```

and in `impl Store for MemoryStore`:

```rust
    fn create_user(&self, user: NewUser) -> Result<Option<User>, StoreError> {
        let mut users = self.users.lock().expect("users lock");
        if users
            .iter()
            .any(|known| known.email.eq_ignore_ascii_case(&user.email))
        {
            return Ok(None);
        }
        let user = User {
            id: self.id("user"),
            email: user.email,
            password_hash: user.password_hash,
        };
        users.push(user.clone());
        Ok(Some(user))
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        let users = self.users.lock().expect("users lock");
        Ok(users
            .iter()
            .find(|user| user.email.eq_ignore_ascii_case(email))
            .cloned())
    }

    fn user(&self, id: &str) -> Result<Option<User>, StoreError> {
        let users = self.users.lock().expect("users lock");
        Ok(users.iter().find(|user| user.id == id).cloned())
    }

    fn create_organization(
        &self,
        slug: &str,
        name: &str,
    ) -> Result<Option<Organization>, StoreError> {
        let mut organizations = self.organizations.lock().expect("organizations lock");
        if organizations.iter().any(|known| known.slug == slug) {
            return Ok(None);
        }
        let organization = Organization {
            id: self.id("org"),
            slug: slug.to_owned(),
            name: name.to_owned(),
        };
        organizations.push(organization.clone());
        Ok(Some(organization))
    }

    fn ensure_organization(&self, slug: &str, name: &str) -> Result<Organization, StoreError> {
        if let Some(found) = self.organization_by_slug(slug)? {
            return Ok(found);
        }
        // Another thread may have made it between the two steps.
        match self.create_organization(slug, name)? {
            Some(made) => Ok(made),
            None => self
                .organization_by_slug(slug)?
                .ok_or_else(|| StoreError("an organization vanished".to_owned())),
        }
    }

    fn organization_by_slug(&self, slug: &str) -> Result<Option<Organization>, StoreError> {
        let organizations = self.organizations.lock().expect("organizations lock");
        Ok(organizations.iter().find(|o| o.slug == slug).cloned())
    }

    fn add_member(
        &self,
        organization_id: &str,
        user_id: &str,
        role: Role,
    ) -> Result<bool, StoreError> {
        let mut members = self.members.lock().expect("members lock");
        if members
            .iter()
            .any(|(org, user, _)| org == organization_id && user == user_id)
        {
            return Ok(false);
        }
        members.push((organization_id.to_owned(), user_id.to_owned(), role));
        Ok(true)
    }

    fn membership(&self, organization_id: &str, user_id: &str) -> Result<Option<Role>, StoreError> {
        let members = self.members.lock().expect("members lock");
        Ok(members
            .iter()
            .find(|(org, user, _)| org == organization_id && user == user_id)
            .map(|(_, _, role)| *role))
    }

    fn memberships(&self, user_id: &str) -> Result<Vec<Membership>, StoreError> {
        let members = self.members.lock().expect("members lock");
        let organizations = self.organizations.lock().expect("organizations lock");
        let mut found: Vec<Membership> = members
            .iter()
            .filter(|(_, user, _)| user == user_id)
            .filter_map(|(org, _, role)| {
                organizations
                    .iter()
                    .find(|o| &o.id == org)
                    .map(|organization| Membership {
                        organization: organization.clone(),
                        role: *role,
                    })
            })
            .collect();
        found.sort_by(|a, b| a.organization.slug.cmp(&b.organization.slug));
        Ok(found)
    }
```

`create_session` / `session` keep their bodies; only the parameter is now a user id.

### Step 1.5: Postgres identity queries

- [ ] In `src/postgres_store.rs`: add the migration to `MIGRATIONS` (`[(&str, &str); 5]`):

```rust
    (
        "0005_organizations_and_memberships",
        include_str!("../migrations/0005_organizations_and_memberships.sql"),
    ),
```

Update the `use crate::store::{...}` line to import `Membership, NewUser, Organization`. Replace `create_user`, `user`, `create_session`, `session` and add the new methods:

```rust
    fn create_user(&self, user: NewUser) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "INSERT INTO users (email, password_hash) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING RETURNING id, email, password_hash",
                &[&user.email, &user.password_hash],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, email, password_hash FROM users WHERE lower(email) = lower($1)",
                &[&email],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn user(&self, id: &str) -> Result<Option<User>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, email, password_hash FROM users WHERE id = $1",
                &[&id],
            )
        })?;
        Ok(row.as_ref().map(user_from))
    }

    fn create_organization(
        &self,
        slug: &str,
        name: &str,
    ) -> Result<Option<Organization>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "INSERT INTO organizations (slug, name) VALUES ($1, $2)
                 ON CONFLICT (slug) DO NOTHING RETURNING id, slug, name",
                &[&slug, &name],
            )
        })?;
        Ok(row.as_ref().map(organization_from))
    }

    fn ensure_organization(&self, slug: &str, name: &str) -> Result<Organization, StoreError> {
        let row = self.run(|client| {
            client.execute(
                "INSERT INTO organizations (slug, name) VALUES ($1, $2)
                 ON CONFLICT (slug) DO NOTHING",
                &[&slug, &name],
            )?;
            client.query_one(
                "SELECT id, slug, name FROM organizations WHERE slug = $1",
                &[&slug],
            )
        })?;
        Ok(organization_from(&row))
    }

    fn organization_by_slug(&self, slug: &str) -> Result<Option<Organization>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT id, slug, name FROM organizations WHERE slug = $1",
                &[&slug],
            )
        })?;
        Ok(row.as_ref().map(organization_from))
    }

    fn add_member(
        &self,
        organization_id: &str,
        user_id: &str,
        role: Role,
    ) -> Result<bool, StoreError> {
        let added = self.run(|client| {
            client.execute(
                "INSERT INTO memberships (organization_id, user_id, role) VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
                &[&organization_id, &user_id, &role.as_str()],
            )
        })?;
        Ok(added == 1)
    }

    fn membership(&self, organization_id: &str, user_id: &str) -> Result<Option<Role>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT role FROM memberships WHERE organization_id = $1 AND user_id = $2",
                &[&organization_id, &user_id],
            )
        })?;
        row.map(|row| role_from(&row.get::<_, String>(0))).transpose()
    }

    fn memberships(&self, user_id: &str) -> Result<Vec<Membership>, StoreError> {
        let rows = self.run(|client| {
            client.query(
                "SELECT organizations.id, organizations.slug, organizations.name, memberships.role
                 FROM memberships JOIN organizations ON organizations.id = memberships.organization_id
                 WHERE memberships.user_id = $1 ORDER BY organizations.slug",
                &[&user_id],
            )
        })?;
        rows.iter()
            .map(|row| {
                Ok(Membership {
                    organization: organization_from(row),
                    role: role_from(&row.get::<_, String>(3))?,
                })
            })
            .collect()
    }

    fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.run(|client| {
            client.execute(
                "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)",
                &[&token_hash, &user_id, &expires_at],
            )
        })?;
        Ok(())
    }

    fn session(&self, token_hash: &str) -> Result<Option<(String, DateTime<Utc>)>, StoreError> {
        let row = self.run(|client| {
            client.query_opt(
                "SELECT user_id, expires_at FROM sessions WHERE token_hash = $1",
                &[&token_hash],
            )
        })?;
        Ok(row.map(|row| (row.get(0), row.get(1))))
    }
```

Row helpers at the bottom of the file:

```rust
fn user_from(row: &postgres::Row) -> User {
    User {
        id: row.get(0),
        email: row.get(1),
        password_hash: row.get(2),
    }
}

fn organization_from(row: &postgres::Row) -> Organization {
    Organization {
        id: row.get(0),
        slug: row.get(1),
        name: row.get(2),
    }
}

fn role_from(text: &str) -> Result<Role, StoreError> {
    Role::parse(text).ok_or_else(|| failed(format!("unknown role {text}")))
}
```

Remove the old `user` row-building code that used `Role::parse(&role)`.

### Step 1.6: Export the new types

- [ ] `src/lib.rs`: change the store re-export line to

```rust
pub use store::{
    MemoryStore, Membership, NewUser, Organization, Role, SeenHeartbeat, Stick, Store, StoreError,
    StoredEvent, User,
};
```

### Step 1.7: Store tests pass (the Hub layer still fails to compile)

- [ ] The library does not build until Step 1.9. Skip running tests here; continue.

### Step 1.8: Write the failing Hub-level tests

- [ ] In `tests/accounts.rs`, replace the `setup()` account lines (the two `hub.create_user(..., Role::...)` calls) with:

```rust
    hub.create_user("ana@example.org", PASSWORD).expect("ana");
    hub.create_user("vera@example.org", PASSWORD).expect("vera");
    hub.create_organization("default", "Default", "ana@example.org")
        .expect("organization");
    hub.add_member("default", "vera@example.org", Role::Viewer)
        .expect("vera is a viewer");
    hub.create_organization("other", "Other", "ana@example.org")
        .expect("another organization");
```

then update every other call in the file mechanically:
- `"ana"` / `"vera"` as login names become `"ana@example.org"` / `"vera@example.org"`.
- `hub.access(&token, Role::X)` becomes `hub.access(&token, "default", Role::X)`.
- `CreateUserError::BadName` becomes `CreateUserError::BadEmail`.
- The `create_user` error tests use the new two-argument form: `setup.hub.create_user("bo@example.org", "short")`, `create_user("  ", PASSWORD)`, `create_user("a\nb", PASSWORD)`, `create_user("no-at-sign", PASSWORD)` (expect `BadEmail`), and a repeated `ana@EXAMPLE.org` (expect `EmailTaken`).
- In the last test in the file (`hub.create_user("vera", PASSWORD, Role::Viewer)` near line 283): `hub.create_user("vera@example.org", PASSWORD)` plus `hub.create_organization("default", "Default", "vera@example.org")`.

Add these tests:

```rust
#[test]
fn access_is_checked_per_organization() {
    let setup = setup();
    let (viewer, _) = setup
        .hub
        .login("vera@example.org", PASSWORD)
        .expect("log in");
    assert!(setup.hub.access(&viewer, "default", Role::Viewer).is_ok());
    assert_eq!(
        setup.hub.access(&viewer, "default", Role::Admin).err(),
        Some(AccessError::Forbidden)
    );
    // Not a member of "other", and no such organization, look the same.
    assert_eq!(
        setup.hub.access(&viewer, "other", Role::Viewer).err(),
        Some(AccessError::NotFound)
    );
    assert_eq!(
        setup.hub.access(&viewer, "nowhere", Role::Viewer).err(),
        Some(AccessError::NotFound)
    );
    assert_eq!(
        setup.hub.access("made-up", "default", Role::Viewer).err(),
        Some(AccessError::Unauthorized)
    );
}

#[test]
fn an_owner_may_do_everything_and_the_only_organization_is_found_without_a_slug() {
    let setup = setup();
    let (owner, _) = setup.hub.login("ana@example.org", PASSWORD).expect("log in");
    assert!(setup.hub.access(&owner, "default", Role::Owner).is_ok());
    let (viewer, _) = setup
        .hub
        .login("vera@example.org", PASSWORD)
        .expect("log in");
    let access = setup.hub.access_sole(&viewer, Role::Viewer).expect("sole");
    assert_eq!(access.organization.slug, "default");
    // Ana belongs to two organizations, so there is no "the" organization.
    assert_eq!(
        setup.hub.access_sole(&owner, Role::Viewer).err(),
        Some(AccessError::Ambiguous)
    );
}

#[test]
fn organizations_have_checked_slugs_and_owners() {
    let setup = setup();
    let make = |slug: &str, owner: &str| setup.hub.create_organization(slug, "Name", owner);
    assert_eq!(
        make("Bad Slug", "ana@example.org").err(),
        Some(OrganizationError::BadSlug)
    );
    assert_eq!(
        make("-bad", "ana@example.org").err(),
        Some(OrganizationError::BadSlug)
    );
    assert_eq!(
        make("default", "ana@example.org").err(),
        Some(OrganizationError::SlugTaken)
    );
    assert_eq!(
        make("fresh", "nobody@example.org").err(),
        Some(OrganizationError::NoSuchUser)
    );
    assert!(make("fresh", "ana@example.org").is_ok());
    assert_eq!(
        setup
            .hub
            .add_member("fresh", "ana@example.org", Role::Viewer)
            .err(),
        Some(OrganizationError::AlreadyMember)
    );
}
```

Add `OrganizationError` to the `use ck_hub::{...}` list.

In `tests/sticks.rs` (lines ~40-42) and `tests/api.rs` (line ~74) do the same setup change: users `ana@example.org` (owner of `default`), `vera@example.org` (viewer of `default`); api.rs uses `organizer@example.org` as a viewer of `default` (so create it as the owner's guest: create `ana` first, or create the organizer and give them `Role::Viewer` after an owner exists). Change login bodies from `"organizer"`, `"ana"`, `"vera"` to the email forms.

### Step 1.9: Implement the account layer in `src/accounts.rs`

- [ ] Replace the types, `create_user` and the `impl Hub` block. Keep `Accounts`, `PasswordCost`, `hash`, `token_hash`, `unavailable`, the lockout constants and the unit test (change `"a"`, `"b"` names are fine as they are).

Constants: rename `MAX_NAME_LENGTH` to `MAX_EMAIL_LENGTH = 254` and use it in `login`.

Types:

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum AccessError {
    /// No session, or one that ended.
    Unauthorized,
    /// No such organization, or the user is not a member. The two look the same.
    NotFound,
    /// A member whose role is not allowed to do this.
    Forbidden,
    /// No organization was named and the user belongs to several.
    Ambiguous,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreateUserError {
    BadEmail,
    PasswordTooShort,
    EmailTaken,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OrganizationError {
    BadSlug,
    SlugTaken,
    NoSuchOrganization,
    NoSuchUser,
    AlreadyMember,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub email: String,
}

/// Who is asking, in which organization, and with what role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    pub account: Account,
    pub organization: Organization,
    pub role: Role,
}
```

Imports: `use crate::store::{Membership, NewUser, Organization, Role, Store, StoreError, User};`

`impl Hub`:

```rust
impl Hub {
    pub fn create_user(&self, email: &str, password: &str) -> Result<User, CreateUserError> {
        create_user(self.store.as_ref(), self.accounts.cost, email, password)
    }

    pub fn create_organization(
        &self,
        slug: &str,
        name: &str,
        owner_email: &str,
    ) -> Result<Organization, OrganizationError> {
        create_organization(self.store.as_ref(), slug, name, owner_email)
    }

    pub fn add_member(
        &self,
        slug: &str,
        email: &str,
        role: Role,
    ) -> Result<(), OrganizationError> {
        add_member(self.store.as_ref(), slug, email, role)
    }

    /// Checks an email and password. On success returns the session token for the browser's cookie.
    pub fn login(&self, email: &str, password: &str) -> Result<(String, Account), LoginError> {
        // (the body of today's login, with these changes)
        //  - `name` is now `email`; the lockout key is `email.to_lowercase()`
        //  - `self.store.user(name)` becomes `self.store.user_by_email(email)`
        //  - create_session gets `&user.id`
        //  - the result is `Account { id: user.id, email: user.email }`
    }

    /// The account a session token belongs to.
    pub fn account(&self, token: &str) -> Result<Account, AccessError> {
        let hash = token_hash(token);
        let store_failed = |error: StoreError| {
            unavailable(&error);
            AccessError::Unavailable
        };
        let (user_id, ends) = self
            .store
            .session(&hash)
            .map_err(store_failed)?
            .ok_or(AccessError::Unauthorized)?;
        if ends <= (self.clock)() {
            self.store.delete_session(&hash).map_err(store_failed)?;
            return Err(AccessError::Unauthorized);
        }
        let user = self
            .store
            .user(&user_id)
            .map_err(store_failed)?
            .ok_or(AccessError::Unauthorized)?;
        Ok(Account {
            id: user.id,
            email: user.email,
        })
    }

    /// The organizations the account belongs to.
    pub fn organizations_of(&self, account: &Account) -> Result<Vec<Membership>, AccessError> {
        self.store.memberships(&account.id).map_err(|error| {
            unavailable(&error);
            AccessError::Unavailable
        })
    }

    /// The account, if it is a member of the organization with this slug and its role may do what
    /// `needed` asks for.
    pub fn access(&self, token: &str, slug: &str, needed: Role) -> Result<Access, AccessError> {
        let account = self.account(token)?;
        let store_failed = |error: StoreError| {
            unavailable(&error);
            AccessError::Unavailable
        };
        let organization = self
            .store
            .organization_by_slug(slug)
            .map_err(store_failed)?
            .ok_or(AccessError::NotFound)?;
        // The role is read now, so a changed role applies to a session that is already open.
        let role = self
            .store
            .membership(&organization.id, &account.id)
            .map_err(store_failed)?
            .ok_or(AccessError::NotFound)?;
        if !role.allows(needed) {
            return Err(AccessError::Forbidden);
        }
        Ok(Access {
            account,
            organization,
            role,
        })
    }

    /// For the routes without an organization in the address: the one the account belongs to.
    pub fn access_sole(&self, token: &str, needed: Role) -> Result<Access, AccessError> {
        let account = self.account(token)?;
        let mut memberships = self.organizations_of(&account)?;
        let membership = match memberships.len() {
            0 => return Err(AccessError::NotFound),
            1 => memberships.remove(0),
            _ => return Err(AccessError::Ambiguous),
        };
        if !membership.role.allows(needed) {
            return Err(AccessError::Forbidden);
        }
        Ok(Access {
            account,
            organization: membership.organization,
            role: membership.role,
        })
    }

    /// Ends a session. A token that is not a session is ignored.
    pub fn logout(&self, token: &str) {
        if let Err(error) = self.store.delete_session(&token_hash(token)) {
            unavailable(&error);
        }
    }
}
```

Free functions (replace the old `create_user`):

```rust
fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    email.chars().count() <= MAX_EMAIL_LENGTH
        && !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !email.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn valid_slug(slug: &str) -> bool {
    (1..=63).contains(&slug.len())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
}

/// Adds an account to `store`: checks the email and the password, and keeps only a hash of the
/// password. `ck-hub create-user` uses this without starting a Hub.
pub fn create_user(
    store: &dyn Store,
    cost: PasswordCost,
    email: &str,
    password: &str,
) -> Result<User, CreateUserError> {
    if !valid_email(email) {
        return Err(CreateUserError::BadEmail);
    }
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(CreateUserError::PasswordTooShort);
    }
    let password_hash = hash(cost, password).map_err(|error| {
        tracing::error!("cannot hash a password: {error}");
        CreateUserError::Unavailable
    })?;
    let created = store
        .create_user(NewUser {
            email: email.to_owned(),
            password_hash,
        })
        .map_err(|error| {
            unavailable(&error);
            CreateUserError::Unavailable
        })?;
    created.ok_or(CreateUserError::EmailTaken)
}

/// Makes an organization whose first owner is the account with `owner_email`.
pub fn create_organization(
    store: &dyn Store,
    slug: &str,
    name: &str,
    owner_email: &str,
) -> Result<Organization, OrganizationError> {
    let store_failed = |error: StoreError| {
        unavailable(&error);
        OrganizationError::Unavailable
    };
    if !valid_slug(slug) || name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(OrganizationError::BadSlug);
    }
    let owner = store
        .user_by_email(owner_email)
        .map_err(store_failed)?
        .ok_or(OrganizationError::NoSuchUser)?;
    let organization = store
        .create_organization(slug, name)
        .map_err(store_failed)?
        .ok_or(OrganizationError::SlugTaken)?;
    store
        .add_member(&organization.id, &owner.id, Role::Owner)
        .map_err(store_failed)?;
    Ok(organization)
}

/// Makes the account with `email` a member of the organization with this slug.
pub fn add_member(
    store: &dyn Store,
    slug: &str,
    email: &str,
    role: Role,
) -> Result<(), OrganizationError> {
    let store_failed = |error: StoreError| {
        unavailable(&error);
        OrganizationError::Unavailable
    };
    let organization = store
        .organization_by_slug(slug)
        .map_err(store_failed)?
        .ok_or(OrganizationError::NoSuchOrganization)?;
    let user = store
        .user_by_email(email)
        .map_err(store_failed)?
        .ok_or(OrganizationError::NoSuchUser)?;
    if store
        .add_member(&organization.id, &user.id, role)
        .map_err(store_failed)?
    {
        Ok(())
    } else {
        Err(OrganizationError::AlreadyMember)
    }
}
```

The name-validation test for control characters (`"a\nb"`) is covered by `valid_email` (it has no `@`, so `BadEmail` either way).

`src/lib.rs`: change the accounts re-export to

```rust
pub use accounts::{
    Access, AccessError, Account, CreateUserError, LoginError, MAX_FAILED_LOGINS,
    MIN_PASSWORD_LENGTH, NAME_LOCKOUT_SECONDS, OrganizationError, PasswordCost,
    SESSION_LIFETIME_HOURS, add_member, create_organization, create_user,
};
```

### Step 1.10: Routes

- [ ] `src/routes.rs`:

Imports: `use crate::accounts::{Access, AccessError, Account, LoginError, SESSION_LIFETIME_HOURS};` and `use crate::store::{Membership, Role};`.

Login body accepts both field names, so the page and Forge keep working until they are changed:

```rust
#[derive(Deserialize)]
struct LoginBody {
    #[serde(alias = "username")]
    email: String,
    password: String,
}
```

and `hub.login(&body.email, &body.password)`; the 401 text becomes `"wrong email or password"`.

Replace `account_json` and `me`:

```rust
fn account_json(account: &Account, memberships: &[Membership]) -> serde_json::Value {
    let organizations: Vec<_> = memberships
        .iter()
        .map(|m| json!({ "slug": m.organization.slug, "name": m.organization.name, "role": m.role.as_str() }))
        .collect();
    let mut answer = json!({
        "username": account.email,
        "email": account.email,
        "organizations": organizations,
    });
    // The role of an account with one organization, as the page and Forge read it today.
    if let [only] = memberships {
        answer["role"] = json!(only.role.as_str());
    }
    answer
}

async fn me(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    let Some(token) = session_token(&headers) else {
        return denied((StatusCode::UNAUTHORIZED, "log in first"));
    };
    let result = tokio::task::spawn_blocking(move || {
        let account = hub.account(&token)?;
        let memberships = hub.organizations_of(&account)?;
        Ok::<_, AccessError>((account, memberships))
    })
    .await;
    match result {
        Ok(Ok((account, memberships))) => Json(account_json(&account, &memberships)).into_response(),
        Ok(Err(reason)) => denied(denied_for(&reason)),
        Err(_) => denied(denied_for(&AccessError::Unavailable)),
    }
}
```

In `login`, the success body is `account_json(&account, &[])`-less: replace `Json(account_json(&account))` by `Json(json!({ "username": account.email, "email": account.email }))`.

Replace `require` with these two (the denial text lives in one place):

```rust
fn denied_for(reason: &AccessError) -> Denied {
    match reason {
        AccessError::Unauthorized => (StatusCode::UNAUTHORIZED, "log in first"),
        AccessError::NotFound => (StatusCode::NOT_FOUND, "no such organization"),
        AccessError::Forbidden => (StatusCode::FORBIDDEN, "this needs a different role"),
        AccessError::Ambiguous => (
            StatusCode::CONFLICT,
            "you belong to several organizations: use /api/v1/orgs/{slug}/...",
        ),
        AccessError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "the Hub cannot read its data right now",
        ),
    }
}

/// The caller's access to the only organization they belong to.
async fn require(hub: &Arc<Hub>, headers: &HeaderMap, needed: Role) -> Result<Access, Denied> {
    let Some(token) = session_token(headers) else {
        return Err(denied_for(&AccessError::Unauthorized));
    };
    let hub = Arc::clone(hub);
    match tokio::task::spawn_blocking(move || hub.access_sole(&token, needed)).await {
        Ok(result) => result.map_err(|reason| denied_for(&reason)),
        Err(_) => Err(denied_for(&AccessError::Unavailable)),
    }
}
```

The `sticks` and `register` handlers keep calling `require(&hub, &headers, Role::X)` and ignore the returned value for now (`if let Err(reason) = ...` still compiles).

### Step 1.11: CLI

- [ ] `src/main.rs`:

Imports: replace `CreateUserError` list with `CreateUserError, OrganizationError, add_member, create_organization`.

Commands:

```rust
    /// Make an account. The password is asked for here and never taken from an argument
    CreateUser(CreateUserArgs),
    /// Make an organization and its first owner, who must have an account
    CreateOrg(CreateOrgArgs),
    /// Make an existing account a member of an organization
    AddMember(AddMemberArgs),
```

```rust
#[derive(Args)]
struct CreateUserArgs {
    /// The email the person logs in with
    email: String,
    #[command(flatten)]
    database: DatabaseArgs,
}

#[derive(Args)]
struct CreateOrgArgs {
    /// The short name used in addresses: lowercase letters, digits and hyphens
    slug: String,
    /// The name people see. Defaults to the slug
    #[arg(long)]
    name: Option<String>,
    /// The email of the account that becomes the owner
    #[arg(long)]
    owner: String,
    #[command(flatten)]
    database: DatabaseArgs,
}

#[derive(Args)]
struct AddMemberArgs {
    slug: String,
    email: String,
    /// `owner`, `admin` or `viewer`
    #[arg(long, default_value = "viewer")]
    role: String,
    #[command(flatten)]
    database: DatabaseArgs,
}
```

In `make_user`: remove the role parse; prompt text uses `args.email`; call `create_user(&store, PasswordCost::default(), &email, &password)` and drop the returned user; log `tracing::info!(email = %args.email, "the account was made")`; map `BadEmail => "that is not an email address"`, `EmailTaken => "an account with that email already exists"`.

New helper and commands (leave `import_registry` and `run` as they are in this task):

```rust
/// Runs `work` against the database on a thread where blocking is fine.
async fn with_store<T: Send + 'static>(
    database: DatabaseArgs,
    work: impl FnOnce(&PostgresStore) -> T + Send + 'static,
) -> Result<T, String> {
    let url = database
        .database_url
        .ok_or("this needs a database: pass --database-url or set CK_DATABASE_URL")?;
    let store = connect(url).await?;
    tokio::task::spawn_blocking(move || {
        let result = work(&store);
        // Dropped on this blocking thread, where closing the connection is safe.
        drop(store);
        result
    })
    .await
    .map_err(|e| e.to_string())
}

fn organization_problem(error: &OrganizationError) -> String {
    match error {
        OrganizationError::BadSlug => {
            "the slug must be lowercase letters, digits and hyphens (not at the ends), at most 63 characters, and the name must not be empty".to_owned()
        }
        OrganizationError::SlugTaken => "an organization with that slug exists".to_owned(),
        OrganizationError::NoSuchOrganization => "there is no organization with that slug".to_owned(),
        OrganizationError::NoSuchUser => "there is no account with that email".to_owned(),
        OrganizationError::AlreadyMember => "that account is already a member".to_owned(),
        OrganizationError::Unavailable => "the database could not be written".to_owned(),
    }
}

async fn make_org(args: CreateOrgArgs) -> Result<(), String> {
    let name = args.name.unwrap_or_else(|| args.slug.clone());
    let (slug, owner) = (args.slug, args.owner);
    let made = {
        let (slug, owner) = (slug.clone(), owner.clone());
        with_store(args.database, move |store| {
            create_organization(store, &slug, &name, &owner)
        })
        .await?
    };
    match made {
        Ok(_) => {
            tracing::info!(slug = %slug, owner = %owner, "the organization was made");
            Ok(())
        }
        Err(error) => Err(organization_problem(&error)),
    }
}

async fn add_to_org(args: AddMemberArgs) -> Result<(), String> {
    let role = Role::parse(&args.role).ok_or("the role must be owner, admin or viewer")?;
    let (slug, email) = (args.slug, args.email);
    let added = {
        let (slug, email) = (slug.clone(), email.clone());
        with_store(args.database, move |store| add_member(store, &slug, &email, role)).await?
    };
    match added {
        Ok(()) => {
            tracing::info!(slug = %slug, email = %email, role = %args.role, "the member was added");
            Ok(())
        }
        Err(error) => Err(organization_problem(&error)),
    }
}
```

and in `main`: `Some(Command::CreateOrg(args)) => make_org(args).await, Some(Command::AddMember(args)) => add_to_org(args).await,`.

### Step 1.12: Run everything

- [ ] Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
- Expected: all pass (Postgres tests skip without `CK_TEST_DATABASE_URL`).
- [ ] Run with the database: `CK_TEST_DATABASE_URL=postgres://postgres:ck-test-only@127.0.0.1:54329/ckhub cargo test`
- Expected: all pass, including `the_postgres_store_keeps_its_contract` (this runs migration 0005 against a database that may already hold users and sessions).

---

## Task 2: Scope the data by organization

**Files:** create `migrations/0006_organization_scoping.sql`; modify `src/store.rs`, `src/postgres_store.rs`, `src/hub.rs`, `src/register.rs`, `src/main.rs`, `src/routes.rs`, `tests/store.rs`, `tests/api.rs`, `tests/sticks.rs`.

### Step 2.1: Write the failing isolation test

- [ ] In `tests/store.rs` add (and call it from both `the_memory_store_keeps_its_contract` and `the_postgres_store_keeps_its_contract`, after `the_store_contract(...)`):

```rust
/// Organization two never sees what organization one stored, whichever way it asks.
fn organizations_are_kept_apart(store: &dyn Store) {
    let id = unique();
    let make = |tag: &str| {
        store
            .create_organization(&format!("{id}-{tag}"), tag)
            .expect("write")
            .expect("a new organization")
    };
    let (one, two) = (make("one"), make("two"));
    let stick = format!("{id}-s");
    let stick_id = StickId::parse(&stick).expect("id");
    let new = ck_hub::Stick {
        stick_id: stick_id.clone(),
        token: Token::new("a-secret-of-some-length"),
        team: "Alpha".to_owned(),
        site: "Lab 1".to_owned(),
        contest: "Spring".to_owned(),
        machine_number: None,
    };

    assert!(store.add_stick(&one.id, new.clone()).expect("write"));
    assert!(
        !store.add_stick(&two.id, new).expect("write"),
        "a stick id is taken for every organization"
    );
    let (owner, _) = store.stick(&stick_id).expect("read").expect("a stick");
    assert_eq!(owner, one.id, "the lookup names the organization");
    store
        .record_heartbeat(&one.id, heartbeat(&stick, "boot-1", 1), time(1))
        .expect("write");
    store
        .append_event(&one.id, event(&stick, 1, "ana"), time(2), None)
        .expect("write");

    assert_eq!(store.sticks(&one.id).expect("read").len(), 1);
    assert!(store.sticks(&two.id).expect("read").is_empty());
    assert!(
        store
            .last_heartbeat(&one.id, &stick_id)
            .expect("read")
            .is_some()
    );
    assert!(
        store
            .last_heartbeat(&two.id, &stick_id)
            .expect("read")
            .is_none()
    );
    assert_eq!(
        store
            .heartbeat_seq_of_boot(&two.id, &stick_id, "boot-1")
            .expect("read"),
        None
    );
    assert_eq!(store.events(&one.id, &stick_id).expect("read").len(), 1);
    assert!(store.events(&two.id, &stick_id).expect("read").is_empty());
    assert!(
        store
            .last_event_of_boot(&two.id, &stick_id, "boot-1")
            .expect("read")
            .is_none()
    );
}
```

The existing `the_store_contract` must make its own organization and pass it as the first argument to every tenant call. At its top add:

```rust
    let org = store
        .create_organization(&format!("{id}-contract"), "Contract")
        .expect("write")
        .expect("a new organization")
        .id;
```

and rewrite each call: `store.record_heartbeat(&org, heartbeat(...), time(..))`, `store.last_heartbeat(&org, &a_id)`, `store.heartbeat_seq_of_boot(&org, &a_id, "boot-1")`, `store.append_event(&org, event(...), time(..), None)`, `store.last_event_of_boot(&org, &a_id, ...)`, `store.events(&org, &a_id)`, `store.add_stick(&org, new(...))`, `store.sticks(&org)`. `store.stick(&a_id)` now returns `Option<(String, Stick)>`: write `let (owner, kept) = store.stick(&a_id).expect("read").expect("a stick"); assert_eq!(owner, org);` and use `kept` as before. The other tests in this file that call these methods (`postgres_keeps_data_across_a_reconnect...`, `postgres_never_changes_or_deletes_an_event`, `a_restarted_hub_on_postgres_remembers_what_it_accepted`, `a_hub_on_postgres_answers_real_http_requests`) get the same treatment: create an organization first and pass its id.

### Step 2.2: Write the failing row-level security test

- [ ] Add to `tests/store.rs`:

```rust
/// The policies themselves, with the Hub's queries out of the way: a role that cannot bypass row
/// security sees only the organization it names, or the one stick it looks up.
#[test]
fn postgres_row_security_hides_other_organizations() {
    let Some(url) = database_url() else { return };
    let store = PostgresStore::connect(&url).expect("connect");
    let id = unique();
    let make = |tag: &str| {
        store
            .create_organization(&format!("{id}-{tag}"), tag)
            .expect("write")
            .expect("a new organization")
    };
    let (one, two) = (make("one"), make("two"));
    let stick = format!("{id}-rls");
    store
        .add_stick(
            &one.id,
            ck_hub::Stick {
                stick_id: StickId::parse(&stick).expect("id"),
                token: Token::new("a-secret-of-some-length"),
                team: String::new(),
                site: String::new(),
                contest: String::new(),
                machine_number: None,
            },
        )
        .expect("write");

    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    client
        .batch_execute(
            "DO $$ BEGIN
                 IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'ck_rls_test') THEN
                     CREATE ROLE ck_rls_test NOSUPERUSER NOBYPASSRLS;
                 END IF;
             END $$;
             GRANT SELECT ON ALL TABLES IN SCHEMA public TO ck_rls_test;",
        )
        .expect("a role that row security applies to");
    let mut tx = client.transaction().expect("transaction");
    tx.batch_execute("SET LOCAL ROLE ck_rls_test").expect("switch role");
    let visible = |tx: &mut postgres::Transaction<'_>| -> i64 {
        tx.query_one("SELECT count(*) FROM sticks WHERE stick_id = $1", &[&stick])
            .expect("count")
            .get(0)
    };

    assert_eq!(visible(&mut tx), 0, "no organization named");
    tx.execute("SELECT set_config('ck.org_id', $1, true)", &[&two.id])
        .expect("set");
    assert_eq!(visible(&mut tx), 0, "the other organization");
    tx.execute("SELECT set_config('ck.org_id', $1, true)", &[&one.id])
        .expect("set");
    assert_eq!(visible(&mut tx), 1, "its own organization");
    tx.execute("SELECT set_config('ck.org_id', '', true)", &[])
        .expect("clear");
    tx.execute("SELECT set_config('ck.stick_id', $1, true)", &[&stick])
        .expect("set");
    assert_eq!(visible(&mut tx), 1, "the lookup of that one stick");
}
```

### Step 2.3: Run and watch both fail

- [ ] Run: `cargo test --test store 2>&1 | tail -30`
- Expected: compile errors for the new method signatures.

### Step 2.4: The migration

- [ ] Create `migrations/0006_organization_scoping.sql`:

```sql
-- Everything sticks report belongs to one organization. Data that exists already goes to the
-- default organization (the previous migration made it when there was any).
DO $$
DECLARE
    t           text;
    default_org text;
BEGIN
    SELECT id INTO default_org FROM organizations WHERE slug = 'default';
    FOREACH t IN ARRAY ARRAY['sticks', 'heartbeats', 'heartbeat_boots', 'events'] LOOP
        IF default_org IS NULL THEN
            EXECUTE format(
                'ALTER TABLE %I ADD COLUMN organization_id text NOT NULL REFERENCES organizations (id)', t);
        ELSE
            -- A constant default fills the old rows without updating them, so the append-only
            -- trigger of events is not involved.
            EXECUTE format(
                'ALTER TABLE %I ADD COLUMN organization_id text NOT NULL DEFAULT %L REFERENCES organizations (id)',
                t, default_org);
            EXECUTE format('ALTER TABLE %I ALTER COLUMN organization_id DROP DEFAULT', t);
        END IF;

        -- A request names its organization for the transaction (`ck.org_id`). Without it, or with
        -- another, no row of these tables is visible or writable. Superusers and roles with
        -- BYPASSRLS are not subject to this: the Hub must not connect as one.
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY in_organization ON %I
                 USING (organization_id = current_setting(''ck.org_id'', true))
                 WITH CHECK (organization_id = current_setting(''ck.org_id'', true))', t);
    END LOOP;
END $$;

-- A stick that sends a heartbeat does not say which organization it is in. The Hub finds out with
-- this lookup of exactly one stick (`ck.stick_id`), which can read and change nothing else.
CREATE POLICY lookup_one_stick ON sticks FOR SELECT
    USING (stick_id = current_setting('ck.stick_id', true));
```

### Step 2.5: Store trait and `MemoryStore`

- [ ] In `src/store.rs` change the tenant methods of the trait (everything else stays):

```rust
    fn record_heartbeat(&self, organization: &str, heartbeat: Heartbeat, received_at: DateTime<Utc>) -> Result<(), StoreError>;
    fn last_heartbeat(&self, organization: &str, stick: &StickId) -> Result<Option<SeenHeartbeat>, StoreError>;
    fn heartbeat_seq_of_boot(&self, organization: &str, stick: &StickId, boot: &str) -> Result<Option<u64>, StoreError>;
    fn append_event(&self, organization: &str, event: Event, received_at: DateTime<Utc>, integrity: Option<String>) -> Result<(), StoreError>;
    fn last_event_of_boot(&self, organization: &str, stick: &StickId, boot: &str) -> Result<Option<StoredEvent>, StoreError>;
    fn events(&self, organization: &str, stick: &StickId) -> Result<Vec<StoredEvent>, StoreError>;

    /// Adds a stick to an organization. Returns false, and changes nothing, when its id is already
    /// known to any organization: a stick's secret is never replaced, and an id belongs to one.
    fn add_stick(&self, organization: &str, stick: Stick) -> Result<bool, StoreError>;

    /// The stick with this id and the organization it belongs to.
    fn stick(&self, id: &StickId) -> Result<Option<(String, Stick)>, StoreError>;

    /// Every stick of the organization.
    fn sticks(&self, organization: &str) -> Result<Vec<Stick>, StoreError>;
```

(Write them in the multi-line rustfmt style the file already uses; `cargo fmt` will reflow.)

`MemoryStore`: key the maps by organization.

```rust
    heartbeats: Mutex<HashMap<(String, StickId), SeenHeartbeat>>,
    boot_seqs: Mutex<HashMap<(String, StickId, String), u64>>,
    events: Mutex<HashMap<(String, StickId), Vec<StoredEvent>>>,
    sticks: Mutex<HashMap<StickId, (String, Stick)>>,
```

Each method adds `organization` to its key: for example

```rust
    fn last_heartbeat(&self, organization: &str, stick: &StickId) -> Result<Option<SeenHeartbeat>, StoreError> {
        Ok(self
            .heartbeats
            .lock()
            .expect("heartbeats lock")
            .get(&(organization.to_owned(), stick.clone()))
            .cloned())
    }
```

`add_stick`, `stick`, `sticks`:

```rust
    fn add_stick(&self, organization: &str, stick: Stick) -> Result<bool, StoreError> {
        let mut sticks = self.sticks.lock().expect("sticks lock");
        if sticks.contains_key(&stick.stick_id) {
            return Ok(false);
        }
        sticks.insert(stick.stick_id.clone(), (organization.to_owned(), stick));
        Ok(true)
    }

    fn stick(&self, id: &StickId) -> Result<Option<(String, Stick)>, StoreError> {
        Ok(self.sticks.lock().expect("sticks lock").get(id).cloned())
    }

    fn sticks(&self, organization: &str) -> Result<Vec<Stick>, StoreError> {
        Ok(self
            .sticks
            .lock()
            .expect("sticks lock")
            .values()
            .filter(|(org, _)| org == organization)
            .map(|(_, stick)| stick.clone())
            .collect())
    }
```

### Step 2.6: `PostgresStore`

- [ ] Add the migration to `MIGRATIONS` (`[(&str, &str); 6]`): `("0006_organization_scoping", include_str!("../migrations/0006_organization_scoping.sql"))`.

Add next to `run`:

```rust
    /// Like [`run`](Self::run), inside a transaction that names the organization, so row-level
    /// security shows only its rows. Every query below also names the organization itself: that
    /// is the rule, and row security is the second guard for a query that forgets it.
    fn in_org<T>(
        &self,
        organization: &str,
        query: impl FnOnce(&mut postgres::Transaction<'_>) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute("SELECT set_config('ck.org_id', $1, true)", &[&organization])?;
            let result = query(&mut transaction)?;
            transaction.commit()?;
            Ok(result)
        })
    }
```

Rewrite the tenant methods. The SQL gets `organization_id` in the `INSERT` column list and `AND organization_id = $n` in every `WHERE`; the closure parameter is a `Transaction` (`tx.execute`, `tx.query_opt`, `tx.query` have the same shape as `client`'s). The complete queries:

```rust
    // record_heartbeat (two statements in one transaction now)
    self.in_org(organization, |tx| {
        tx.execute(
            "INSERT INTO heartbeat_boots (organization_id, stick_id, boot_id, seq)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (stick_id, boot_id)
             DO UPDATE SET seq = GREATEST(heartbeat_boots.seq, EXCLUDED.seq)
             WHERE heartbeat_boots.organization_id = EXCLUDED.organization_id",
            &[&organization, &heartbeat.stick_id.as_str(), &heartbeat.boot_id, &seq],
        )?;
        tx.execute(
            "INSERT INTO heartbeats (organization_id, stick_id, boot_id, seq, received_at, count, heartbeat)
             VALUES ($1, $2, $3, $4, $5, 1, $6)
             ON CONFLICT (stick_id) DO UPDATE SET
                 boot_id = EXCLUDED.boot_id, seq = EXCLUDED.seq,
                 received_at = EXCLUDED.received_at, heartbeat = EXCLUDED.heartbeat,
                 count = heartbeats.count + 1
             WHERE heartbeats.organization_id = EXCLUDED.organization_id",
            &[&organization, &heartbeat.stick_id.as_str(), &heartbeat.boot_id, &seq, &received_at, &json],
        )
    })?;

    // last_heartbeat
    "SELECT heartbeat, received_at, count FROM heartbeats
     WHERE organization_id = $1 AND stick_id = $2"   // params: [&organization, &stick.as_str()]

    // heartbeat_seq_of_boot
    "SELECT seq FROM heartbeat_boots WHERE organization_id = $1 AND stick_id = $2 AND boot_id = $3"

    // last_event_of_boot
    "SELECT event, received_at, integrity FROM events
     WHERE organization_id = $1 AND stick_id = $2 AND boot_id = $3 ORDER BY id DESC LIMIT 1"

    // append_event
    "INSERT INTO events (organization_id, stick_id, boot_id, seq, received_at, integrity, event)
     VALUES ($1, $2, $3, $4, $5, $6, $7)"

    // events
    "SELECT event, received_at, integrity FROM events
     WHERE organization_id = $1 AND stick_id = $2 ORDER BY id"

    // add_stick: the conflict is on the stick id alone, so an id of another organization is refused
    "INSERT INTO sticks (organization_id, stick_id, token, team, site, contest, machine_number)
     VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (stick_id) DO NOTHING"

    // sticks
    "SELECT stick_id, token, team, site, contest, machine_number FROM sticks
     WHERE organization_id = $1"
```

The `stick` lookup is the one place that runs without an organization:

```rust
    fn stick(&self, id: &StickId) -> Result<Option<(String, Stick)>, StoreError> {
        let row = self.run(|client| {
            let mut transaction = client.transaction()?;
            transaction.execute("SELECT set_config('ck.stick_id', $1, true)", &[&id.as_str()])?;
            let row = transaction.query_opt(
                "SELECT stick_id, token, team, site, contest, machine_number, organization_id
                 FROM sticks WHERE stick_id = $1",
                &[&id.as_str()],
            )?;
            transaction.commit()?;
            Ok(row)
        })?;
        row.as_ref()
            .map(|row| Ok((row.get::<_, String>(6), stick_from(row)?)))
            .transpose()
    }
```

`stick_from` stays as it is (it reads columns 0 to 5).

### Step 2.7: The Hub carries the organization

- [ ] `src/hub.rs`:

Add the field `default_org: std::sync::OnceLock<String>` (initialise with `std::sync::OnceLock::new()` in `Hub::new`) and:

```rust
    /// The organization the registry file's sticks belong to, made on first use.
    fn default_org_id(&self) -> Result<String, StoreError> {
        if let Some(id) = self.default_org.get() {
            return Ok(id.clone());
        }
        let organization = self.store.ensure_organization("default", "Default")?;
        Ok(self.default_org.get_or_init(|| organization.id).clone())
    }
```

`authenticate` returns `(String, StickId)`:

```rust
        let from_file = {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            registry.find(&id).map(|entry| entry.token.clone())
        };
        // A stick is known from the registry file or from the database, whichever has it.
        let (organization, token) = match from_file {
            Some(token) => (self.default_org_id()?, token),
            None => match self.store.stick(&id)? {
                Some((organization, stick)) => (organization, stick.token),
                None => return Err(Rejection::Unauthorized),
            },
        };
        if !ck_crypto::verify(&token, body, signature) {
            return Err(Rejection::Unauthorized);
        }
        Ok((organization, id))
```

`check` and `check_event` start with `let (organization, id) = self.authenticate(...)?;` and pass `&organization` as the first argument of `heartbeat_seq_of_boot`, `record_heartbeat`, `last_event_of_boot`, `append_event`.

`sticks` takes the organization:

```rust
    pub fn sticks(&self, organization: &str) -> Result<Vec<StickView>, StoreError> {
        let now = (self.clock)();
        // The registry file's sticks belong to the default organization.
        let mut entries: Vec<Stick> = if organization == self.default_org_id()? {
            let mut registry = self.registry.lock().expect("registry lock");
            registry.refresh();
            registry.all().cloned().collect()
        } else {
            Vec::new()
        };
        for stick in self.store.sticks(organization)? {
            // (as today)
        }
        // ... and in the loop:
        //   self.store.last_heartbeat(organization, &entry.stick_id)?
        //   self.store.events(organization, &entry.stick_id)?
```

### Step 2.8: Registering into an organization

- [ ] `src/register.rs`: the function now knows which organization it registers into and who owns a known id.

```rust
pub fn register_sticks(
    store: &dyn Store,
    organization: &str,
    in_file: &dyn Fn(&StickId) -> Option<(String, Token)>,
    sticks: Vec<NewStick>,
) -> Result<Registered, RegisterError> {
```

`in_file` returns the organization the file's sticks belong to together with the secret. The known-stick block becomes:

```rust
        let known = match in_file(&new.stick_id) {
            Some(known) => Some(known),
            None => store
                .stick(&new.stick_id)
                .map_err(unavailable)?
                .map(|(owner, stick)| (owner, stick.token)),
        };
        if let Some((owner, known)) = known {
            // An id that belongs to another organization is reported like a different secret, so
            // nothing tells this organization which ids exist elsewhere.
            if owner == organization && known == token {
                result.already_known += 1;
            } else {
                result.conflicts.push(new.stick_id.to_string());
            }
            continue;
        }
```

and `store.add_stick(organization, stick)`.

`Hub::register_sticks`:

```rust
    pub fn register_sticks(&self, organization: &str, sticks: Vec<NewStick>) -> Result<Registered, RegisterError> {
        let file_org = self.default_org_id().map_err(|error| {
            tracing::error!("{error}");
            RegisterError::Unavailable
        })?;
        let registry = self.registry.lock().expect("registry lock");
        register_sticks(
            self.store.as_ref(),
            organization,
            &|id| {
                registry
                    .find(id)
                    .map(|stick| (file_org.clone(), stick.token.clone()))
            },
            sticks,
        )
    }
```

`default_org_id` is private to `hub.rs`; make it `pub(crate)`.

### Step 2.9: Routes and CLI pass the organization

- [ ] `src/routes.rs`: in `sticks` use the access from `require`:

```rust
    let access = match require(&hub, &headers, Role::Viewer).await {
        Ok(access) => access,
        Err(reason) => return denied(reason),
    };
    match tokio::task::spawn_blocking(move || hub.sticks(&access.organization.id)).await {
```

and in `register`: `let access = match require(..., Role::Admin).await {...}` then `hub.register_sticks(&access.organization.id, sticks)`.

- [ ] `src/main.rs` `import_registry`: add `#[arg(long)] org: String` ("the slug of the organization the sticks go to") to `ImportRegistryArgs`; resolve it with `store.organization_by_slug(&slug)` (error `"there is no organization with that slug"`), then `register_sticks(&store, &organization.id, &|_| None, file.sticks)`.

### Step 2.10: Update the remaining tests

- [ ] `tests/sticks.rs` and `tests/api.rs`: every call of `register_sticks` or a store method takes the organization; get it with `hub`-independent code: `store.organization_by_slug("default")` or by `ensure_organization("default", "Default")`. The Hub tests that start with `setup()` already created `default` through `create_organization("default", ...)`.
- [ ] Add to `tests/sticks.rs` an end-to-end isolation test over HTTP:

```rust
#[tokio::test]
async fn sticks_and_heartbeats_of_one_organization_are_invisible_to_another() {
    // setup(): ana owns "default" and "other"; vera is a viewer of "default" only
    // 1. log in as ana; POST a stick registry to /api/v1/orgs/default/sticks -> 200 added 1
    // 2. GET /api/v1/orgs/default/sticks -> 1 stick; GET /api/v1/orgs/other/sticks -> []
    // 3. POST the same stick to /api/v1/orgs/other/sticks -> 409 with the id in `conflicts`
    // 4. a signed heartbeat from the stick is accepted, and shows under "default" only
    // 5. log in as vera: GET /api/v1/orgs/other/sticks -> 404
}
```

Write it with the helpers that file already has for signing and posting (the sibling tests show the pattern); this test needs Task 3's routes, so write it now and expect it to fail until Task 3.

### Step 2.11: Run everything

- [ ] Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
- Expected: all pass except the new HTTP isolation test (needs Task 3).
- [ ] Run with the database: `CK_TEST_DATABASE_URL=... cargo test`
- Expected: `postgres_row_security_hides_other_organizations` passes; `the_postgres_store_keeps_its_contract` passes on a database that already holds events from older runs (migration 0006 fills them into `default`, or fails the test run if the events trigger blocks it: if so, stop and report it).

---

## Task 3: Organization routes and the page

**Files:** modify `src/routes.rs`, `src/page.html`, `tests/sticks.rs`, `tests/api.rs`.

### Step 3.1: Routes with the organization in the address

- [ ] `src/routes.rs`: add `use axum::extract::Path;` and replace the `/api/v1/sticks` route with both:

```rust
        .route(
            "/api/v1/sticks",
            get(sticks)
                .post(register)
                .layer(DefaultBodyLimit::max(MAX_REGISTRY_BYTES)),
        )
        .route(
            "/api/v1/orgs/{slug}/sticks",
            get(sticks_of).post(register_in).layer(DefaultBodyLimit::max(MAX_REGISTRY_BYTES)),
        )
```

Split the handlers so the body of each is shared. Add:

```rust
/// The caller's access to the organization named in the address.
async fn require_in(
    hub: &Arc<Hub>,
    headers: &HeaderMap,
    slug: String,
    needed: Role,
) -> Result<Access, Denied> {
    let Some(token) = session_token(headers) else {
        return Err(denied_for(&AccessError::Unauthorized));
    };
    let hub = Arc::clone(hub);
    match tokio::task::spawn_blocking(move || hub.access(&token, &slug, needed)).await {
        Ok(result) => result.map_err(|reason| denied_for(&reason)),
        Err(_) => Err(denied_for(&AccessError::Unavailable)),
    }
}

async fn sticks(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    match require(&hub, &headers, Role::Viewer).await {
        Ok(access) => list_sticks(hub, access).await,
        Err(reason) => denied(reason),
    }
}

async fn sticks_of(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Viewer).await {
        Ok(access) => list_sticks(hub, access).await,
        Err(reason) => denied(reason),
    }
}

async fn list_sticks(hub: Arc<Hub>, access: Access) -> Response {
    match tokio::task::spawn_blocking(move || hub.sticks(&access.organization.id)).await {
        Ok(Ok(sticks)) => Json(sticks).into_response(),
        Ok(Err(error)) => {
            tracing::error!("{error}");
            unavailable()
        }
        Err(_) => unavailable(),
    }
}

async fn register(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    match require(&hub, &headers, Role::Admin).await {
        Ok(access) => register_into(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}

async fn register_in(
    State(hub): State<Arc<Hub>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match require_in(&hub, &headers, slug, Role::Admin).await {
        Ok(access) => register_into(hub, access, body).await,
        Err(reason) => denied(reason),
    }
}
```

`register_into(hub, access, body)` is the old body of `register` after its permission check (parse with `read_sticks`, then `spawn_blocking(move || hub.register_sticks(&access.organization.id, sticks))`, same answers).

### Step 3.2: Run the isolation test from Task 2

- [ ] Run: `cargo test --test sticks sticks_and_heartbeats_of_one_organization 2>&1 | tail -20`
- Expected: PASS.
- [ ] Add one more case to `tests/api.rs`: `GET /api/v1/sticks` for a user with two organizations answers `409`, and `GET /api/v1/me` lists both organizations and has no `role`.

### Step 3.3: The page

- [ ] `src/page.html`:

Login field (line ~179) label and key:

```html
    <div><label for="user">Email</label><input id="user" name="email" type="email" autocomplete="username" required></div>
```

and in the submit handler `body: JSON.stringify({ email: document.getElementById("user").value, password: ... })`.

In the header's `#who` span add the switcher before the name: `<select id="org" aria-label="Organization" hidden></select>`.

Replace `refresh()`:

```js
let org = null;

async function refresh() {
  const problem = document.getElementById("problem");
  try {
    const me = await fetch("/api/v1/me", { cache: "no-store" });
    if (me.status === 401) { showLogin(true); problem.style.display = "none"; return; }
    if (!me.ok) throw new Error("HTTP " + me.status);
    const account = await me.json();
    const orgs = account.organizations;
    const select = document.getElementById("org");
    if (!orgs.some((o) => o.slug === org)) org = orgs.length ? orgs[0].slug : null;
    if (select.options.length !== orgs.length) {
      select.replaceChildren(...orgs.map((o) => new Option(o.name, o.slug)));
    }
    select.value = org || "";
    select.hidden = orgs.length < 2;
    const current = orgs.find((o) => o.slug === org);
    document.getElementById("who-name").textContent =
      account.email + (current ? " (" + current.role + ")" : "");
    showLogin(false);
    if (!org) {
      sticks = [];
      render();
      problem.textContent = "You are not a member of any organization yet. Ask an owner to add you.";
      problem.style.display = "block";
      return;
    }
    const response = await fetch("/api/v1/orgs/" + encodeURIComponent(org) + "/sticks", { cache: "no-store" });
    if (!response.ok) throw new Error("HTTP " + response.status);
    sticks = await response.json();
    render();
    document.getElementById("live-text").textContent = "Live · updated " + new Date().toLocaleTimeString();
    problem.style.display = "none";
  } catch (error) {
    problem.textContent = "The Hub cannot be reached. The list below is the last one received.";
    problem.style.display = "block";
  }
}

document.getElementById("org").addEventListener("change", (event) => {
  org = event.target.value;
  refresh();
});
```

(The old `refresh` fetched `/api/v1/sticks` and `/api/v1/me` separately; this one does `/me` first. Keep `showLogin`, the timers and the other handlers unchanged.)

### Step 3.4: Look at it in a browser

- [ ] Start a Hub on a throwaway Postgres: `cargo run -- --database-url postgres://postgres:ck-test-only@127.0.0.1:54329/ckhub`, in another shell `cargo run -- create-user ana@example.org`, `cargo run -- create-org demo --owner ana@example.org`, open `http://localhost:8088`, log in, check the list loads, the organization selector is hidden with one organization, and appears after `cargo run -- create-org second --owner ana@example.org`. Report what you saw.

### Step 3.5: Run everything

- [ ] Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
- Expected: all pass.

---

## Task 4: Warn when row-level security cannot protect, and update the docs

**Files:** modify `src/postgres_store.rs`, `README.md`, `tests/store.rs`.

### Step 4.1: Failing test

- [ ] `tests/store.rs`:

```rust
#[test]
fn postgres_says_whether_row_security_applies_to_the_hub_role() {
    let Some(url) = database_url() else { return };
    let store = PostgresStore::connect(&url).expect("connect");
    // The test database user is a superuser, so row security does not apply to it.
    assert!(!store.row_security_applies().expect("read"));
}
```

### Step 4.2: Implement

- [ ] `src/postgres_store.rs`:

```rust
    /// Whether row-level security binds the role the Hub connects as. It does not for a superuser
    /// or a role with BYPASSRLS: then only the store's own filters keep organizations apart.
    pub fn row_security_applies(&self) -> Result<bool, StoreError> {
        let row = self.run(|client| {
            client.query_one(
                "SELECT NOT (rolsuper OR rolbypassrls) FROM pg_roles WHERE rolname = current_user",
                &[],
            )
        })?;
        Ok(row.get(0))
    }
```

In `main.rs` `run`, after connecting:

```rust
        let store = connect(url).await?;
        if !store.row_security_applies().map_err(|e| e.to_string())? {
            tracing::warn!(
                "the database role is a superuser or bypasses row security: organizations are kept apart \
                 only by the Hub's own queries. Connect as a plain role in production"
            );
        }
        Arc::new(store)
```

Run: `cargo test --test store postgres_says 2>&1 | tail`; expected PASS with a superuser URL.

### Step 4.3: Docs

- [ ] `README.md`, section "Organizer accounts": replace it with:

````markdown
## Accounts and organizations

People belong to organizations. The page and the stick list show one organization at a time.
Accounts need the database. Staff make them from the command line:

```sh
ck-hub create-user ana@example.org --database-url postgres://user:password@localhost/ckhub
ck-hub create-org rcpc --name "RCPC" --owner ana@example.org --database-url postgres://...
ck-hub add-member rcpc bo@example.org --role viewer --database-url postgres://...
```

The password (at least 12 characters) is asked for at a prompt without echo, twice. It is never taken
from an argument or the environment. The roles are `owner`, `admin` and `viewer`. Only an Argon2id
hash of the password is stored, and only a hash of each session token.

Everything a stick reports belongs to one organization. Row-level security keeps organizations apart
in the database, but it does not apply to a superuser or a role with `BYPASSRLS`: connect the Hub as a
plain role. The Hub warns at start when it is not.

A database from before organizations is moved into an organization with the slug `default`: former
admins become its owners, former viewers stay viewers, and they log in with the name they had.
````

Also fix the mention of `create-user` in the "Registering sticks" paragraph so it says an admin account of the organization, and `ck-hub import-registry` gets `--org <slug>` in its example.

- [ ] The setup repository (`../ck-hub-setup`, `README.md` and `docs/azure.md`) tells people to run `create-user admin`. Note that it needs `create-user admin@<domain>` followed by `create-org main --owner admin@<domain>`. That repository is outside this change: list the needed edits in the final report instead of making them.

### Step 4.4: Final check

- [ ] Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`, then again with `CK_TEST_DATABASE_URL` set.
- Expected: all pass.

---

## Self-review against the spec

- Data model: `users`, `organizations`, `memberships`, `sessions` by user id, `organization_id` on `sticks`, `heartbeats`, `heartbeat_boots`, `events`: Tasks 1 and 2. Left for later plans: `identities` (plan 3), `invitations` and `is_platform_staff` (plan 4), `email_verified_at` (plan 2).
- Row-level security with a narrow stick lookup: Task 2 (`lookup_one_stick`), tested directly in `postgres_row_security_hides_other_organizations`.
- Login by email with the existing lockout, cookie and hashing: Task 1.
- `404` for a non-member and an unknown organization: `AccessError::NotFound`, Task 1 and Task 3.
- Migration of existing data and users: migrations 0005 and 0006; the open item "admin becomes owner" is decided above.
- `ck-hub create-org <slug> --owner <email>`: Task 1. The staff-only HTTP route and `make-staff` are plan 4.
- Sticks unchanged on the wire: the heartbeat and event endpoints keep their paths, headers and signing.
