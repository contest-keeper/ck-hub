-- What a judge says was submitted. Never changed or deleted, like events. The Hub works out which
-- of them came from a stick whenever it is asked, so a late stick event still matches.
CREATE TABLE judge_submissions (
    id              bigserial PRIMARY KEY,
    organization_id text        NOT NULL REFERENCES organizations (id),
    judge           text        NOT NULL,
    contest_id      bigint      NOT NULL,
    submission_id   bigint      NOT NULL,
    handles         text[]      NOT NULL,
    problem_index   text        NOT NULL,
    submitted_at    timestamptz NOT NULL,
    received_at     timestamptz NOT NULL,
    UNIQUE (organization_id, judge, contest_id, submission_id)
);

ALTER TABLE judge_submissions ENABLE ROW LEVEL SECURITY;
ALTER TABLE judge_submissions FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON judge_submissions
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));

CREATE FUNCTION judge_submissions_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'judge submissions are append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER judge_submissions_never_change
    BEFORE UPDATE OR DELETE ON judge_submissions
    FOR EACH ROW EXECUTE FUNCTION judge_submissions_are_append_only();

-- Like the event log, the Hub's role may only add to it.
REVOKE UPDATE, DELETE ON judge_submissions FROM ck_hub_app;
