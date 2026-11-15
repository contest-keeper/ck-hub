-- The Hub does its work as this plain role, never as the account that logs in (a superuser, or the server's
-- admin on a managed database). Row-level security binds a plain role, so the second wall between
-- organizations holds. The role cannot log in, cannot change the schema, and cannot rewrite the event log.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'ck_hub_app') THEN
        CREATE ROLE ck_hub_app NOLOGIN NOSUPERUSER NOBYPASSRLS;
    END IF;
END $$;

-- The account that logs in may switch to it (SET ROLE), and only to it.
GRANT ck_hub_app TO CURRENT_USER;

GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO ck_hub_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO ck_hub_app;

-- Tables made by later migrations are open to it the same way.
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO ck_hub_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO ck_hub_app;

-- The migration record is the migrator's, and events are only ever added (a trigger refuses changes as well).
REVOKE ALL ON schema_migrations FROM ck_hub_app;
REVOKE UPDATE, DELETE ON events FROM ck_hub_app;
