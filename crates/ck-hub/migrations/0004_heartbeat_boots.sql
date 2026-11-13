-- The highest heartbeat sequence seen in every boot of every stick, so a heartbeat of an older boot
-- that was recorded earlier cannot be replayed.
CREATE TABLE heartbeat_boots (
    stick_id text   NOT NULL,
    boot_id  text   NOT NULL,
    seq      bigint NOT NULL,
    PRIMARY KEY (stick_id, boot_id)
);

INSERT INTO heartbeat_boots (stick_id, boot_id, seq)
SELECT stick_id, boot_id, seq FROM heartbeats;
