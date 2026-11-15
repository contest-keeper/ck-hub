-- Contest profiles of Keeper Forge: the settings JSON and an optional wallpaper picture. They
-- belong to the organization, so every admin of a team sees and loads the same ones. The client
-- makes the id, so the key is the organization and the id: two organizations may use one id and
-- never see each other's profile.
--
-- The wallpaper bytes live in this row (`bytea`, TOAST keeps them out of the way of listings):
-- a picture is at most a few megabytes, and a profile then moves, backs up and deletes as one row.
CREATE TABLE profiles (
    organization_id text        NOT NULL REFERENCES organizations (id),
    id              text        NOT NULL,
    name            text        NOT NULL,
    settings        jsonb       NOT NULL,
    wallpaper_name  text,
    wallpaper_data  bytea,
    updated_at      timestamptz NOT NULL,
    PRIMARY KEY (organization_id, id),
    CHECK ((wallpaper_name IS NULL) = (wallpaper_data IS NULL))
);

ALTER TABLE profiles ENABLE ROW LEVEL SECURITY;
ALTER TABLE profiles FORCE ROW LEVEL SECURITY;
CREATE POLICY in_organization ON profiles
    USING (organization_id = current_setting('ck.org_id', true))
    WITH CHECK (organization_id = current_setting('ck.org_id', true));
