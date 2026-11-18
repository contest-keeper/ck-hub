-- One-time enrolment codes. An admin mints a code for one new stick; the stick trades it once for
-- its own secret, so no long-lived secret has to be written to the stick's disk in advance.
--
-- Only a SHA-256 of the code is kept (the code itself has about 100 bits of randomness, so a plain
-- hash is enough), and never the code or the secret that was handed out. `used_at` is set by the
-- one statement that redeems a code, so a code works once even when two requests race.
CREATE TABLE enrolment_codes (
    code_hash          text        PRIMARY KEY,
    organization_id    text        NOT NULL REFERENCES organizations (id),
    created_by_user_id text        NOT NULL,
    team               text        NOT NULL DEFAULT '',
    site               text        NOT NULL DEFAULT '',
    contest            text        NOT NULL DEFAULT '',
    machine_number     integer,
    created_at         timestamptz NOT NULL,
    expires_at         timestamptz NOT NULL,
    used_at            timestamptz
);

CREATE INDEX enrolment_codes_by_organization ON enrolment_codes (organization_id, created_at);

ALTER TABLE enrolment_codes ENABLE ROW LEVEL SECURITY;
ALTER TABLE enrolment_codes FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON enrolment_codes
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));

-- A stick that enrols does not say which organization it is in: the code does. Redeeming looks up
-- exactly the one code it holds (`ck.code_hash`), which can read and change nothing else.
CREATE POLICY redeem_one_code_read ON enrolment_codes FOR SELECT
    USING (code_hash = current_setting('ck.code_hash', true));
CREATE POLICY redeem_one_code_use ON enrolment_codes FOR UPDATE
    USING (code_hash = current_setting('ck.code_hash', true))
    WITH CHECK (code_hash = current_setting('ck.code_hash', true));