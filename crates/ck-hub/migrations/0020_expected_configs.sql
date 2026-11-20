-- The settings Forge wrote to a stick: the SHA-256 of the exact `config.json` bytes. A stick reports
-- the hash of the settings file it read in every heartbeat, so the Hub can warn when they differ
-- (the file was edited, or the stick was written by something else).
--
-- It is kept by stick id and not on the `sticks` row, because Forge tells the Hub before an enrolling
-- stick has a row. A stick without a row here is not checked.
CREATE TABLE expected_configs (
    organization_id text        NOT NULL REFERENCES organizations (id),
    stick_id        text        NOT NULL,
    sha256          text        NOT NULL,
    recorded_at     timestamptz NOT NULL,
    PRIMARY KEY (organization_id, stick_id)
);

ALTER TABLE expected_configs ENABLE ROW LEVEL SECURITY;
ALTER TABLE expected_configs FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON expected_configs
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));
