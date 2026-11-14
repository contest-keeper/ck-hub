-- An account made by sign-up is unverified until its owner opens the link we mail. Every account
-- that exists now was made by staff or the command line, which vouch for the address.
ALTER TABLE users ADD COLUMN email_verified_at timestamptz;
UPDATE users SET email_verified_at = now();

-- One-time links for confirming an email and for resetting a password. Only a hash of the token
-- is kept, so a copy of this table cannot be used.
CREATE TABLE email_tokens (
    token_hash text PRIMARY KEY,
    user_id    text        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose    text        NOT NULL CHECK (purpose IN ('verify', 'reset')),
    expires_at timestamptz NOT NULL
);
CREATE INDEX email_tokens_by_user ON email_tokens (user_id, purpose);
CREATE INDEX email_tokens_by_expiry ON email_tokens (expires_at);
