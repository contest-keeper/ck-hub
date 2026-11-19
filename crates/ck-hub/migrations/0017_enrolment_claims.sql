ALTER TABLE enrolment_codes
    ADD COLUMN claimed_stick_id text CHECK (claimed_stick_id IS NULL OR used_at IS NOT NULL);
