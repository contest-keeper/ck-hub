-- One event of a boot has one sequence number. The Hub already refuses a repeat, but it checks and
-- writes in two steps, so two Hub processes that overlap for a moment (a new revision starting
-- while the old one runs) could both write the same event. The index makes the database refuse it.
--
-- Copies written before this migration are removed first, keeping the earliest of each. Events are
-- append-only, so the delete goes through the same door as `purge_stick_events`: the trigger lets it
-- pass for one stick, and row security needs the organization named.
DO $$
DECLARE
    org   text;
    stick text;
BEGIN
    FOR org IN SELECT id FROM organizations LOOP
        PERFORM set_config('ck.org_id', org, true);
        FOR stick IN
            SELECT DISTINCT d.stick_id FROM (
                SELECT stick_id FROM events
                WHERE organization_id = org
                GROUP BY stick_id, boot_id, seq
                HAVING count(*) > 1
            ) d
        LOOP
            PERFORM set_config('ck.purging_stick', stick, true);
            DELETE FROM events e USING events keep
            WHERE e.organization_id = org AND e.stick_id = stick
              AND keep.organization_id = e.organization_id AND keep.stick_id = e.stick_id
              AND keep.boot_id = e.boot_id AND keep.seq = e.seq AND keep.id < e.id;
        END LOOP;
    END LOOP;
    PERFORM set_config('ck.purging_stick', '', true);
    PERFORM set_config('ck.org_id', '', true);
END $$;

CREATE UNIQUE INDEX events_once ON events (organization_id, stick_id, boot_id, seq);
