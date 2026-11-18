-- Commands the Hub sends to a stick (a message, a screen lock, a shutdown). A command is queued
-- here, carried to the stick in the reply to its heartbeat and closed by the stick's ack. The row
-- keeps who sent it and how it went, so the organization can see what was done to its sticks.
--
-- `signed_command` is exactly what is sent, so the signed bytes never change after queueing. The
-- state is queued, delivered, done, refused or failed; a command that ends without an ack is shown
-- as expired when it is read, so no job has to touch it. The sender's email is kept as it was, so
-- the history stays readable after an account is renamed or removed.
CREATE TABLE commands (
    organization_id text        NOT NULL REFERENCES organizations (id),
    id              text        NOT NULL,
    stick_id        text        NOT NULL,
    sent_by_user_id text        NOT NULL,
    sent_by_email   text        NOT NULL,
    action_type     text        NOT NULL,
    signed_command  jsonb       NOT NULL,
    state           text        NOT NULL
        CHECK (state IN ('queued', 'delivered', 'done', 'refused', 'failed')),
    refusal         text,
    created_at      timestamptz NOT NULL,
    delivered_at    timestamptz,
    finished_at     timestamptz,
    expires_at      timestamptz NOT NULL,
    PRIMARY KEY (organization_id, id)
);

CREATE INDEX commands_by_stick ON commands (organization_id, stick_id, created_at DESC);

ALTER TABLE commands ENABLE ROW LEVEL SECURITY;
ALTER TABLE commands FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON commands
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));