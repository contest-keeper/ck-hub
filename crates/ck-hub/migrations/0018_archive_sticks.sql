-- Archiving and deleting sticks.
--
-- An archived stick keeps all its data but its credential stops working; `delete_after` says when
-- the Hub removes it for good (or an admin deletes it earlier). Archiving is not retirement:
-- retirement is one way, archiving can be undone.
--
-- A deleted stick keeps only a tombstone row in `sticks` (`deleted_at` set, the secret and the
-- labels blanked, `retired_at` set). The tombstone keeps the id from being registered again by a
-- registry push or the registry file, and from being shown.
ALTER TABLE sticks ADD COLUMN archived_at timestamptz;
ALTER TABLE sticks ADD COLUMN delete_after timestamptz;
ALTER TABLE sticks ADD COLUMN deleted_at timestamptz;
ALTER TABLE sticks ADD CONSTRAINT sticks_archive_pair
    CHECK ((archived_at IS NULL) = (delete_after IS NULL));

-- Events are append-only for the Hub's role: it has no DELETE right on them and a trigger refuses
-- changes. Deleting a stick has to remove its events, so the trigger lets a delete through only
-- when it comes from `purge_stick_events` below, which runs with its owner's rights.
CREATE OR REPLACE FUNCTION events_are_append_only() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE' AND OLD.stick_id = current_setting('ck.purging_stick', true) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'events are append-only';
END;
$$ LANGUAGE plpgsql;

CREATE FUNCTION purge_stick_events(org text, stick text) RETURNS bigint
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
DECLARE
    removed bigint;
BEGIN
    -- The function runs with its owner's rights, so it checks for itself what the Hub role may do:
    -- only the events of an archived stick of the organization the request works for.
    IF org IS DISTINCT FROM current_setting('ck.org_id', true) THEN
        RAISE EXCEPTION 'events can only be deleted inside their own organization';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM sticks
        WHERE organization_id = org AND stick_id = stick
          AND archived_at IS NOT NULL AND deleted_at IS NULL
    ) THEN
        RAISE EXCEPTION 'only the events of an archived stick can be deleted';
    END IF;
    PERFORM set_config('ck.purging_stick', stick, true);
    DELETE FROM events WHERE organization_id = org AND stick_id = stick;
    GET DIAGNOSTICS removed = ROW_COUNT;
    PERFORM set_config('ck.purging_stick', '', true);
    RETURN removed;
END;
$$;

REVOKE ALL ON FUNCTION purge_stick_events(text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION purge_stick_events(text, text) TO ck_hub_app;
