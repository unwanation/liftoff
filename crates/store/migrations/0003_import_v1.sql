-- What `liftoffd setup --import` keeps of a v1 database beyond the core
-- tables (IMP-05): the v1 tenants, their
-- memberships, and which imported projects and domains belonged to which
-- tenant, for the `tenants` module to adopt. The
-- core tables keep `tenant_id` empty until then.
--
-- The rows point at core rows with cascading foreign keys, so they never
-- hold a user, project or domain back from being deleted.

CREATE TABLE import_v1_tenants (
    id              TEXT PRIMARY KEY,
    slug            TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL,
    created_by      TEXT NOT NULL,
    max_targets     INTEGER NOT NULL,
    cpus_hundredths INTEGER NOT NULL,
    memory_mb       INTEGER NOT NULL,
    created_at      TEXT NOT NULL
) STRICT;

CREATE TABLE import_v1_tenant_members (
    tenant_id TEXT NOT NULL REFERENCES import_v1_tenants (id) ON DELETE CASCADE,
    user_id   TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role      TEXT NOT NULL CHECK (role IN ('owner', 'member')),
    PRIMARY KEY (tenant_id, user_id)
) STRICT;

CREATE TABLE import_v1_tenant_projects (
    tenant_id  TEXT NOT NULL REFERENCES import_v1_tenants (id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    PRIMARY KEY (tenant_id, project_id)
) STRICT;

-- `cf_record_id` is the v1 Cloudflare record of the domain, for the
-- `dns.cloudflare` module.
CREATE TABLE import_v1_tenant_domains (
    tenant_id    TEXT NOT NULL REFERENCES import_v1_tenants (id) ON DELETE CASCADE,
    domain_id    TEXT NOT NULL PRIMARY KEY REFERENCES domains (id) ON DELETE CASCADE,
    cf_record_id TEXT
) STRICT;
