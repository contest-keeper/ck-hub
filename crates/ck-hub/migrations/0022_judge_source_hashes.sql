-- The hash of the source of a judge submission, read from the judge's own page once the contest is
-- over. Only the hash is kept, never the code. Never changed or deleted, like the submissions.
CREATE TABLE judge_source_hashes (
    id              bigserial PRIMARY KEY,
    organization_id text        NOT NULL REFERENCES organizations (id),
    judge           text        NOT NULL,
    contest_id      bigint      NOT NULL,
    submission_id   bigint      NOT NULL,
    source_sha256   text        NOT NULL,
    fetched_at      timestamptz NOT NULL,
    UNIQUE (organization_id, judge, contest_id, submission_id)
);

ALTER TABLE judge_source_hashes ENABLE ROW LEVEL SECURITY;
ALTER TABLE judge_source_hashes FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON judge_source_hashes
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));

CREATE FUNCTION judge_source_hashes_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'judge source hashes are append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER judge_source_hashes_never_change
    BEFORE UPDATE OR DELETE ON judge_source_hashes
    FOR EACH ROW EXECUTE FUNCTION judge_source_hashes_are_append_only();

REVOKE UPDATE, DELETE ON judge_source_hashes FROM ck_hub_app;
