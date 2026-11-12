-- The organizers who may look at the Hub. Only a hash of the password is kept.
CREATE TABLE users (
    name          text PRIMARY KEY,
    password_hash text        NOT NULL,
    role          text        NOT NULL CHECK (role IN ('admin', 'viewer')),
    created_at    timestamptz NOT NULL DEFAULT now()
);

-- Logged-in browsers. Only a hash of the session token is kept, so a copy of this table cannot be
-- used to log in.
CREATE TABLE sessions (
    token_hash text PRIMARY KEY,
    user_name  text        NOT NULL REFERENCES users (name) ON DELETE CASCADE,
    expires_at timestamptz NOT NULL
);

CREATE INDEX sessions_by_expiry ON sessions (expires_at);
