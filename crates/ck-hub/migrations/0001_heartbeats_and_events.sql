-- The latest heartbeat of every stick, and how many have arrived.
CREATE TABLE heartbeats (
    stick_id    text PRIMARY KEY,
    boot_id     text        NOT NULL,
    seq         bigint      NOT NULL,
    received_at timestamptz NOT NULL,
    count       bigint      NOT NULL,
    heartbeat   jsonb       NOT NULL
);

-- Every event a stick reported, in the order the Hub received them. Never changed or deleted:
-- anything else about a stick can be worked out again from these rows.
CREATE TABLE events (
    id          bigserial PRIMARY KEY,
    stick_id    text        NOT NULL,
    boot_id     text        NOT NULL,
    seq         bigint      NOT NULL,
    received_at timestamptz NOT NULL,
    integrity   text,
    event       jsonb       NOT NULL
);

CREATE INDEX events_by_stick ON events (stick_id, id);

CREATE FUNCTION events_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'events are append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER events_never_change
    BEFORE UPDATE OR DELETE ON events
    FOR EACH ROW EXECUTE FUNCTION events_are_append_only();
