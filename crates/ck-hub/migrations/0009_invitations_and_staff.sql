-- Platform staff make organizations and name their first owner; they do not see inside organizations.
ALTER TABLE users ADD COLUMN is_platform_staff boolean NOT NULL DEFAULT false;

-- A mailed invitation to join an organization. Only a hash of its token is kept, so a copy of this table
-- cannot be used.
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

-- One open invitation per organization and address: inviting again replaces it.
CREATE UNIQUE INDEX invitations_one_open ON invitations (organization_id, lower(email));
CREATE INDEX invitations_by_expiry ON invitations (expires_at);
