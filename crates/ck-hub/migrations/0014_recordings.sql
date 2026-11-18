-- Recordings a stick uploads (screen video segments and snapshots). The bytes live in a private blob
-- container under `<organization_id>/<stick_id>/<id>`; this table only describes them, so the
-- Hub can resume an upload, show a list and delete what has expired.
--
-- `next_chunk` is the number of chunks the blob store has accepted so far (chunks go in order), so
-- an interrupted stick continues from there. A recording is complete once `completed_at` is set,
-- and then `expires_at` says when the clean-up job removes it. `file` is the stick's own name for
-- the file: it is unique per stick, so asking to start an upload twice is the same upload.
CREATE TABLE recordings (
    organization_id text        NOT NULL REFERENCES organizations (id),
    id              text        NOT NULL,
    stick_id        text        NOT NULL,
    file            text        NOT NULL,
    kind            text        NOT NULL CHECK (kind IN ('video', 'snapshot')),
    size            bigint      NOT NULL CHECK (size > 0),
    sha256          text        NOT NULL,
    started_at      timestamptz,
    duration_s      bigint,
    next_chunk      integer     NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL,
    completed_at    timestamptz,
    expires_at      timestamptz,
    PRIMARY KEY (organization_id, id),
    UNIQUE (organization_id, stick_id, file)
);

CREATE INDEX recordings_by_stick ON recordings (organization_id, stick_id, created_at DESC);
CREATE INDEX recordings_by_expiry ON recordings (organization_id, expires_at);

ALTER TABLE recordings ENABLE ROW LEVEL SECURITY;
ALTER TABLE recordings FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON recordings
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));
