-- Which physical USB stick a stick id was written to, so the Hub can tell a stick was rewritten.
--
-- Keeper Forge sends a fingerprint of the disk it writes: a SHA-256 of the disk's serial number and
-- size, and the last characters of the serial so a person can match it to a stick in hand. Every
-- write of the same disk counts. `current_stick_id` is the stick id of its latest write. The rows
-- are kept when a stick is deleted, so the count survives it. The fingerprint is a bookkeeping aid
-- and not a security check: a serial can be copied.
CREATE TABLE stick_hardware (
    organization_id  text        NOT NULL REFERENCES organizations (id),
    fingerprint      text        NOT NULL,
    serial_hint      text        NOT NULL DEFAULT '',
    write_count      integer     NOT NULL,
    first_written_at timestamptz NOT NULL,
    last_written_at  timestamptz NOT NULL,
    current_stick_id text,
    PRIMARY KEY (organization_id, fingerprint)
);

CREATE INDEX stick_hardware_by_stick ON stick_hardware (organization_id, current_stick_id);

ALTER TABLE stick_hardware ENABLE ROW LEVEL SECURITY;
ALTER TABLE stick_hardware FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON stick_hardware
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));

-- The disk a code was minted for. The write is counted when the stick trades the code, not when
-- the code is made: a write that was cancelled or failed never reaches the Hub, so it never counts.
ALTER TABLE enrolment_codes ADD COLUMN hardware_fingerprint text;
ALTER TABLE enrolment_codes ADD COLUMN hardware_serial_hint text;
