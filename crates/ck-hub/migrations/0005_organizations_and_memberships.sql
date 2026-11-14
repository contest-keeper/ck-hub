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
