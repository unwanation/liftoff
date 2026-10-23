-- liftoff v2 core schema.
--
-- Ids are ULID strings, timestamps RFC 3339 text, token hashes 32-byte
-- blobs. Foreign keys restrict by default; only the cascades the Store
-- contract names are spelled out (environment -> bindings, deploys;
-- secret -> versions). Secrets point at their scope polymorphically
-- (`scope`, `scope_id`), so the store checks that parent and deletes an
-- environment's secrets itself, in the same transaction.

CREATE TABLE users (
    id         TEXT PRIMARY KEY,
    login      TEXT NOT NULL UNIQUE,
    email      TEXT NOT NULL,
    role       TEXT NOT NULL CHECK (role IN ('admin', 'user')),
    -- `local` or `oidc:<issuer>`.
    source     TEXT NOT NULL CHECK (source = 'local' OR source LIKE 'oidc:_%'),
    subject    TEXT,
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE sessions (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    token_hash   BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('cli', 'browser', 'mcp')),
    expires_at   TEXT NOT NULL,
    last_used_at TEXT NOT NULL,
    created_at   TEXT NOT NULL
) STRICT;

CREATE INDEX sessions_user ON sessions (user_id);

CREATE TABLE git_connections (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    provider      TEXT NOT NULL CHECK (provider IN ('github', 'gitlab', 'forgejo')),
    host          TEXT NOT NULL,
    account_login TEXT NOT NULL,
    credential    TEXT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('oauth', 'token')),
    expires_at    TEXT,
    created_at    TEXT NOT NULL,
    UNIQUE (user_id, host)
) STRICT;

CREATE TABLE projects (
    id             TEXT PRIMARY KEY,
    -- A tenant of the `tenants` module; NULL without it. Not a foreign key:
    -- module tables live in their own namespace.
    tenant_id      TEXT,
    name           TEXT NOT NULL,
    git_host       TEXT NOT NULL,
    repo           TEXT NOT NULL,
    manifest       TEXT,
    template_group TEXT,
    template_name  TEXT,
    created_by     TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at     TEXT NOT NULL,
    UNIQUE (git_host, repo),
    CHECK ((template_group IS NULL) = (template_name IS NULL))
) STRICT;

-- `(tenant_id, name)` is unique also without a tenant: NULLs would not
-- clash in a plain unique index.
CREATE UNIQUE INDEX projects_tenant_name ON projects (coalesce(tenant_id, ''), name);

CREATE TABLE environments (
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    name              TEXT NOT NULL,
    kind              TEXT NOT NULL CHECK (kind IN ('compose', 'function', 'tunnel')),
    branch            TEXT,
    pinned_by         TEXT REFERENCES users (id) ON DELETE RESTRICT,
    owner_user_id     TEXT REFERENCES users (id) ON DELETE RESTRICT,
    cpus_hundredths   INTEGER NOT NULL,
    memory_mb         INTEGER NOT NULL,
    state             TEXT NOT NULL
        CHECK (state IN ('empty', 'running', 'failed', 'stopped', 'offline')),
    -- Not a foreign key: deploys reference their environment, and a pruned
    -- or cascaded deploy must not be blocked by this column.
    current_deploy_id TEXT,
    visibility        TEXT NOT NULL CHECK (visibility IN ('private', 'shared')),
    created_at        TEXT NOT NULL,
    UNIQUE (project_id, name),
    -- A pin is branch and pinned_by together or not at all.
    CHECK ((branch IS NULL) = (pinned_by IS NULL))
) STRICT;

CREATE INDEX environments_branch ON environments (project_id, branch) WHERE branch IS NOT NULL;

CREATE TABLE domains (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT,
    project_id  TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    host        TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL CHECK (kind IN ('managed', 'external')),
    verified_at TEXT,
    -- Since when the daily re-verification finds an external record gone
    -- (a warning only: the route stays).
    record_lost_at TEXT,
    proxied     INTEGER NOT NULL CHECK (proxied IN (0, 1)),
    created_at  TEXT NOT NULL
) STRICT;

CREATE INDEX domains_project ON domains (project_id);

CREATE TABLE bindings (
    id             TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    -- One binding per domain, one binding per host port.
    domain_id      TEXT NOT NULL UNIQUE REFERENCES domains (id) ON DELETE RESTRICT,
    service        TEXT NOT NULL,
    container_port INTEGER NOT NULL,
    host_port      INTEGER NOT NULL UNIQUE
) STRICT;

CREATE INDEX bindings_environment ON bindings (environment_id);

-- The host port of the last inserted binding: allocation goes round the
-- range after it instead of reusing a port that was just freed.
CREATE TABLE port_cursor (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    host_port INTEGER NOT NULL
) STRICT;

CREATE TABLE deploys (
    id             TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    number         INTEGER NOT NULL CHECK (number > 0),
    source         TEXT NOT NULL CHECK (source IN ('ci', 'cli', 'redeploy')),
    ref            TEXT,
    tag            TEXT NOT NULL,
    compose        TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    log            TEXT NOT NULL,
    error          TEXT,
    rolled_back_to INTEGER,
    -- `user:<id>` or `token:<id>`, so no foreign key.
    triggered_by   TEXT NOT NULL CHECK (triggered_by LIKE 'user:_%' OR triggered_by LIKE 'token:_%'),
    started_at     TEXT,
    finished_at    TEXT,
    UNIQUE (environment_id, number)
) STRICT;

CREATE INDEX deploys_status ON deploys (status, id);

CREATE TABLE deploy_tokens (
    id         TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE RESTRICT,
    token_hash BLOB NOT NULL CHECK (length(token_hash) = 32),
    created_by TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL,
    revoked_at TEXT
) STRICT;

-- At most one live (not revoked) token per project.
CREATE UNIQUE INDEX deploy_tokens_live_project ON deploy_tokens (project_id) WHERE revoked_at IS NULL;
CREATE INDEX deploy_tokens_hash ON deploy_tokens (token_hash);

CREATE TABLE secrets (
    id         TEXT PRIMARY KEY,
    -- `tenant` arrives with the `tenants` module.
    scope      TEXT NOT NULL CHECK (scope IN ('tenant', 'project', 'environment')),
    scope_id   TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    version    INTEGER NOT NULL CHECK (version > 0),
    ci         INTEGER NOT NULL CHECK (ci IN (0, 1)),
    -- A user id, `ci` or `import`.
    updated_by TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (scope, scope_id, key)
) STRICT;

CREATE TABLE secret_versions (
    secret_id  TEXT NOT NULL REFERENCES secrets (id) ON DELETE CASCADE,
    version    INTEGER NOT NULL CHECK (version > 0),
    value      TEXT NOT NULL,
    updated_by TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (secret_id, version)
) STRICT;

CREATE TABLE audit (
    id            TEXT PRIMARY KEY,
    -- Not a foreign key: the log outlives what it mentions.
    actor_user_id TEXT,
    action        TEXT NOT NULL,
    resource      TEXT NOT NULL,
    at            TEXT NOT NULL
) STRICT;
