-- The sticks the admin made, registered by the admin tool or imported from its registry file.
-- The secret has to be kept as it is, because the Hub checks the HMAC signatures with it.
CREATE TABLE sticks (
    stick_id       text PRIMARY KEY,
    token          text        NOT NULL,
    team           text        NOT NULL DEFAULT '',
    site           text        NOT NULL DEFAULT '',
    contest        text        NOT NULL DEFAULT '',
    machine_number integer,
    created_at     timestamptz NOT NULL DEFAULT now()
);
