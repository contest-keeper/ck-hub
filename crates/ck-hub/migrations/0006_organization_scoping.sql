-- Everything sticks report belongs to one organization. Data that exists already goes to the
-- default organization (the previous migration made it when there was any).
DO $$
DECLARE
    t           text;
    default_org text;
BEGIN
    SELECT id INTO default_org FROM organizations WHERE slug = 'default';
    FOREACH t IN ARRAY ARRAY['sticks', 'heartbeats', 'heartbeat_boots', 'events'] LOOP
        IF default_org IS NULL THEN
            EXECUTE format(
                'ALTER TABLE %I ADD COLUMN organization_id text NOT NULL REFERENCES organizations (id)',
                t);
        ELSE
            -- A constant default fills the old rows without updating them, so the append-only
            -- trigger of events is not involved.
            EXECUTE format(
                'ALTER TABLE %I ADD COLUMN organization_id text NOT NULL DEFAULT %L REFERENCES organizations (id)',
                t, default_org);
            EXECUTE format('ALTER TABLE %I ALTER COLUMN organization_id DROP DEFAULT', t);
        END IF;

        -- A request names its organization for the transaction (`ck.org_id`). Without it, or with
        -- another, no row of these tables is visible or writable. Superusers and roles with
        -- BYPASSRLS are not subject to this: the Hub must not connect as one.
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY in_organization ON %I
                 USING (organization_id = current_setting(''ck.org_id'', true))
                 WITH CHECK (organization_id = current_setting(''ck.org_id'', true))',
            t);
    END LOOP;
END $$;

-- A stick that sends a heartbeat does not say which organization it is in. The Hub finds out with
-- this lookup of exactly one stick (`ck.stick_id`), which can read and change nothing else.
CREATE POLICY lookup_one_stick ON sticks FOR SELECT
    USING (stick_id = current_setting('ck.stick_id', true));
