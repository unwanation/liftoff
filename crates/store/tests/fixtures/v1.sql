-- A liftoff v1 database (branch `main`, liftoffd 0.2.0) for the import
-- tests of the import (IMP-01..06).
--
-- The schema is the v1 migrations verbatim (`git show
-- main:crates/store/migrations/000{1,2,3}_*.sql`); the rows after it are
-- representative data. Env files are sealed the way `FakeSecrets` seals
-- (`sealed:` + hex of the plaintext), so the tests open them with the
-- fake; a real v1 database seals them with the master key.
--
-- Ids encode what they are (all valid ULIDs): users ...A00n, tenants
-- ...B00n, targets ...C00n, domains ...D00n, bindings ...E00n, deploy
-- tokens ...F00n, deploys of acme/prod ...G0nnn, of acme/qa ...H0nnn, of
-- beta/prod ...J0nnn, sessions ...K00n.

-- ==== v1 0001_init.sql ========================================================
-- liftoff schema, spec section 4.
--
-- Ids are ULID strings, timestamps RFC 3339 text, token hashes 32-byte blobs.
-- Foreign keys restrict by default; only the cascades the Store contract
-- names are spelled out (tenant -> memberships, target -> bindings/deploys).

CREATE TABLE users (
    id         TEXT PRIMARY KEY,
    sub        TEXT NOT NULL UNIQUE,
    login      TEXT NOT NULL UNIQUE,
    email      TEXT NOT NULL,
    role       TEXT NOT NULL CHECK (role IN ('admin', 'user')),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE tenants (
    id                     TEXT PRIMARY KEY,
    slug                   TEXT NOT NULL UNIQUE,
    name                   TEXT NOT NULL,
    created_by             TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    max_targets            INTEGER NOT NULL,
    target_cpus_hundredths INTEGER NOT NULL,
    target_memory_mb       INTEGER NOT NULL,
    created_at             TEXT NOT NULL
) STRICT;

CREATE TABLE memberships (
    tenant_id TEXT NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id   TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    role      TEXT NOT NULL CHECK (role IN ('owner', 'member')),
    PRIMARY KEY (tenant_id, user_id)
) STRICT;

CREATE INDEX memberships_user ON memberships (user_id);

CREATE TABLE targets (
    id                TEXT PRIMARY KEY,
    tenant_id         TEXT NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    name              TEXT NOT NULL,
    repo              TEXT,
    branch            TEXT,
    pinned_by         TEXT REFERENCES users (id) ON DELETE RESTRICT,
    cpus_hundredths   INTEGER NOT NULL,
    memory_mb         INTEGER NOT NULL,
    state             TEXT NOT NULL CHECK (state IN ('empty', 'running', 'failed', 'stopped')),
    -- Not a foreign key: deploys reference their target, and a pruned or
    -- cascaded deploy must not be blocked by this column.
    current_deploy_id TEXT,
    created_at        TEXT NOT NULL,
    UNIQUE (tenant_id, name),
    -- A pin is repo, branch and pinned_by together or not at all.
    CHECK ((repo IS NULL) = (branch IS NULL) AND (repo IS NULL) = (pinned_by IS NULL))
) STRICT;

CREATE INDEX targets_pin ON targets (repo, branch) WHERE repo IS NOT NULL;

CREATE TABLE domains (
    id           TEXT PRIMARY KEY,
    tenant_id    TEXT NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    host         TEXT NOT NULL UNIQUE,
    kind         TEXT NOT NULL CHECK (kind IN ('managed', 'external')),
    proxied      INTEGER NOT NULL CHECK (proxied IN (0, 1)),
    cf_record_id TEXT,
    verified_at  TEXT,
    created_at   TEXT NOT NULL
) STRICT;

CREATE INDEX domains_tenant ON domains (tenant_id);

CREATE TABLE bindings (
    id             TEXT PRIMARY KEY,
    target_id      TEXT NOT NULL REFERENCES targets (id) ON DELETE CASCADE,
    service        TEXT NOT NULL,
    container_port INTEGER NOT NULL,
    -- One binding per domain, one binding per host port.
    domain_id      TEXT NOT NULL UNIQUE REFERENCES domains (id) ON DELETE RESTRICT,
    host_port      INTEGER NOT NULL UNIQUE
) STRICT;

CREATE INDEX bindings_target ON bindings (target_id);

-- The host port of the last inserted binding: allocation goes round the
-- range after it instead of reusing a port that was just freed.
CREATE TABLE port_cursor (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    host_port INTEGER NOT NULL
) STRICT;

CREATE TABLE deploys (
    id             TEXT PRIMARY KEY,
    target_id      TEXT NOT NULL REFERENCES targets (id) ON DELETE CASCADE,
    number         INTEGER NOT NULL,
    source         TEXT NOT NULL CHECK (source IN ('ci', 'cli', 'redeploy')),
    repo           TEXT,
    git_ref        TEXT,
    tag            TEXT NOT NULL,
    compose        TEXT NOT NULL,
    env            TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    log            TEXT NOT NULL,
    error          TEXT,
    rolled_back_to INTEGER,
    -- A user id or a deploy token id, so no foreign key.
    trigger_kind   TEXT NOT NULL CHECK (trigger_kind IN ('user', 'token')),
    trigger_id     TEXT NOT NULL,
    started_at     TEXT,
    finished_at    TEXT,
    UNIQUE (target_id, number)
) STRICT;

CREATE INDEX deploys_status ON deploys (status, id);

CREATE TABLE deploy_tokens (
    id         TEXT PRIMARY KEY,
    repo       TEXT NOT NULL,
    token_hash BLOB NOT NULL CHECK (length(token_hash) = 32),
    created_by TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL,
    revoked_at TEXT
) STRICT;

-- At most one live (not revoked) token per repository.
CREATE UNIQUE INDEX deploy_tokens_live_repo ON deploy_tokens (repo) WHERE revoked_at IS NULL;
CREATE INDEX deploy_tokens_hash ON deploy_tokens (token_hash);

CREATE TABLE sessions (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    token_hash   BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    name         TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    last_used_at TEXT NOT NULL
) STRICT;

CREATE INDEX sessions_user ON sessions (user_id);

-- ==== v1 0002_target_sso.sql ==================================================
ALTER TABLE targets ADD COLUMN sso INTEGER NOT NULL DEFAULT 0 CHECK (sso IN (0, 1));

-- ==== v1 0003_gate.sql ========================================================
-- The SSO gate's records, spec "SSO gate" section 3.
--
-- Same conventions as 0001: ULID text ids, RFC 3339 text timestamps,
-- 32-byte blob hashes. Foreign keys are on for every connection, so the
-- cascades below are the database's: a user's gate sessions go with the
-- user, a central session takes its host sessions and codes with it.

-- No expires_at indexes: delete_expired_gate compares expiry in Rust, because
-- RFC 3339 text does not sort, so no query could use them.

CREATE TABLE gate_logins (
    id            TEXT PRIMARY KEY,
    state_hash    BLOB NOT NULL UNIQUE CHECK (length(state_hash) = 32),
    nonce         TEXT NOT NULL,
    pkce_verifier TEXT NOT NULL,
    host          TEXT NOT NULL,
    rd            TEXT NOT NULL,
    expires_at    TEXT NOT NULL
) STRICT;

CREATE TABLE gate_sessions (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    kind       TEXT NOT NULL CHECK (kind IN ('central', 'host')),
    host       TEXT,
    parent_id  TEXT REFERENCES gate_sessions (id) ON DELETE CASCADE,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    -- A central session has neither host nor parent; a host session both.
    CHECK (
        (kind = 'central' AND host IS NULL AND parent_id IS NULL)
        OR (kind = 'host' AND host IS NOT NULL AND parent_id IS NOT NULL)
    )
) STRICT;

CREATE INDEX gate_sessions_user ON gate_sessions (user_id);
CREATE INDEX gate_sessions_parent ON gate_sessions (parent_id);

CREATE TABLE gate_codes (
    code_hash  BLOB PRIMARY KEY CHECK (length(code_hash) = 32),
    session_id TEXT NOT NULL REFERENCES gate_sessions (id) ON DELETE CASCADE,
    host       TEXT NOT NULL,
    rd         TEXT NOT NULL,
    expires_at TEXT NOT NULL
) STRICT;

CREATE INDEX gate_codes_session ON gate_codes (session_id);

-- rusqlite_migration keeps the schema version here.
PRAGMA user_version = 3;

-- ==== rows ====================================================================

-- alice: admin, owner of acme. bob: user, member of acme, owner of beta.
INSERT INTO users (id, sub, login, email, role, created_at) VALUES
    ('01HV000000000000000000A001', 'sub-alice', 'alice', 'alice@example.com', 'admin', '2026-09-01T10:00:00Z'),
    ('01HV000000000000000000A002', 'sub-bob',   'bob',   'bob@example.com',   'user',  '2026-09-02T10:00:00Z');

INSERT INTO tenants (id, slug, name, created_by, max_targets, target_cpus_hundredths, target_memory_mb, created_at) VALUES
    ('01HV000000000000000000B001', 'acme', 'Acme Inc.', '01HV000000000000000000A001', 10, 200, 2048, '2026-09-03T10:00:00Z'),
    ('01HV000000000000000000B002', 'beta', 'Beta Team', '01HV000000000000000000A002', 3,  100, 1024, '2026-09-04T10:00:00Z');

INSERT INTO memberships (tenant_id, user_id, role) VALUES
    ('01HV000000000000000000B001', '01HV000000000000000000A001', 'owner'),
    ('01HV000000000000000000B001', '01HV000000000000000000A002', 'member'),
    ('01HV000000000000000000B002', '01HV000000000000000000A002', 'owner');

-- Repository acme/shop has two targets (prod on main, qa on dev); beta/shop
-- one. Both repositories are named `shop`, so the project names clash.
INSERT INTO targets (id, tenant_id, name, repo, branch, pinned_by, cpus_hundredths, memory_mb, state, current_deploy_id, created_at, sso) VALUES
    ('01HV000000000000000000C001', '01HV000000000000000000B001', 'prod', 'acme/shop', 'main', '01HV000000000000000000A001', 50,  512,  'running', '01HV00000000000000000G0021', '2026-09-05T10:00:00Z', 0),
    ('01HV000000000000000000C002', '01HV000000000000000000B001', 'qa',   'acme/shop', 'dev',  '01HV000000000000000000A001', 25,  256,  'running', '01HV00000000000000000H0001', '2026-09-06T10:00:00Z', 1),
    ('01HV000000000000000000C003', '01HV000000000000000000B002', 'prod', 'beta/shop', 'main', '01HV000000000000000000A002', 100, 1024, 'failed',  NULL,                         '2026-09-07T10:00:00Z', 0);

INSERT INTO domains (id, tenant_id, host, kind, proxied, cf_record_id, verified_at, created_at) VALUES
    ('01HV000000000000000000D001', '01HV000000000000000000B001', 'shop.apps.test',    'managed',  1, 'cf-record-1', NULL,                   '2026-09-05T11:00:00Z'),
    ('01HV000000000000000000D002', '01HV000000000000000000B001', 'shop-qa.apps.test', 'managed',  1, 'cf-record-2', NULL,                   '2026-09-06T11:00:00Z'),
    ('01HV000000000000000000D003', '01HV000000000000000000B002', 'www.beta.example',  'external', 0, NULL,          '2026-09-07T12:00:00Z', '2026-09-07T11:00:00Z');

INSERT INTO bindings (id, target_id, service, container_port, domain_id, host_port) VALUES
    ('01HV000000000000000000E001', '01HV000000000000000000C001', 'web', 3000, '01HV000000000000000000D001', 20000),
    ('01HV000000000000000000E002', '01HV000000000000000000C002', 'web', 3000, '01HV000000000000000000D002', 20001),
    ('01HV000000000000000000E003', '01HV000000000000000000C003', 'app', 8080, '01HV000000000000000000D003', 20002);

INSERT INTO port_cursor (id, host_port) VALUES (1, 20002);

-- Secrets (SHA-256 of the token text):
--   F001 live    lf_AcmeShopDeployToken000000000000000000000
--   F002 revoked lf_AcmeShopRevokedToken00000000000000000000
--   F003 live    lf_BetaShopDeployToken000000000000000000000
INSERT INTO deploy_tokens (id, repo, token_hash, created_by, created_at, revoked_at) VALUES
    ('01HV000000000000000000F002', 'acme/shop', X'8d972e0641e91726373c398ea487fcc2bfba5b3b05700ee77f7b3f141b3d9b21', '01HV000000000000000000A001', '2026-09-05T12:00:00Z', '2026-09-08T12:00:00Z'),
    ('01HV000000000000000000F001', 'acme/shop', X'd2643f6da9baa6f3f49fa4b6389ace9cd7932095eb3dc301c8e7d7fc1e550cd7', '01HV000000000000000000A001', '2026-09-08T12:00:00Z', NULL),
    ('01HV000000000000000000F003', 'beta/shop', X'd2bffba5a5b65fe6be82c559e68de9e84963a5a57dbb07f0280e1db773e9ff59', '01HV000000000000000000A002', '2026-09-07T12:00:00Z', NULL);

-- acme/prod: 22 deploys by CI. 1..20 succeeded with an old env, 21
-- succeeded with the current env (the current deploy), 22 failed with an
-- env that must not become secrets.
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 22)
INSERT INTO deploys (id, target_id, number, source, repo, git_ref, tag, compose, env, status, log, error, rolled_back_to, trigger_kind, trigger_id, started_at, finished_at)
SELECT
    '01HV00000000000000000G' || printf('%04d', i),
    '01HV000000000000000000C001',
    i,
    'ci',
    'acme/shop',
    'main',
    'sha-' || i,
    'services:' || char(10) || '  web:' || char(10) || '    image: git.example.com/acme/shop:${IMAGE_TAG}' || char(10),
    'sealed:' || lower(hex(CASE
        WHEN i = 21 THEN 'DATABASE_URL=postgres://db/acme' || char(10) || 'LOG_LEVEL=info' || char(10)
        WHEN i = 22 THEN 'DATABASE_URL=postgres://db/broken' || char(10)
        ELSE 'DATABASE_URL=postgres://db/old' || char(10)
    END)),
    CASE WHEN i = 22 THEN 'failed' ELSE 'succeeded' END,
    'deploy ' || i || char(10),
    CASE WHEN i = 22 THEN 'container web is unhealthy' ELSE NULL END,
    NULL,
    'token',
    '01HV000000000000000000F001',
    '2026-09-10T10:' || printf('%02d', i) || ':00Z',
    '2026-09-10T10:' || printf('%02d', i) || ':30Z'
FROM n;

-- acme/qa: deploy 1 succeeded (CI, current), deploy 2 failed (alice from
-- the CLI) with an env that adds DEBUG.
INSERT INTO deploys (id, target_id, number, source, repo, git_ref, tag, compose, env, status, log, error, rolled_back_to, trigger_kind, trigger_id, started_at, finished_at) VALUES
    ('01HV00000000000000000H0001', '01HV000000000000000000C002', 1, 'ci', 'acme/shop', 'dev', 'sha-qa1',
     'services:' || char(10) || '  web:' || char(10) || '    image: git.example.com/acme/shop:${IMAGE_TAG}' || char(10),
     'sealed:' || lower(hex('DATABASE_URL=postgres://db/acme_qa' || char(10))),
     'succeeded', 'ok' || char(10), NULL, NULL, 'token', '01HV000000000000000000F001', '2026-09-11T10:00:00Z', '2026-09-11T10:01:00Z'),
    ('01HV00000000000000000H0002', '01HV000000000000000000C002', 2, 'cli', NULL, NULL, 'sha-qa2',
     'services:' || char(10) || '  web:' || char(10) || '    image: git.example.com/acme/shop:${IMAGE_TAG}' || char(10),
     'sealed:' || lower(hex('DATABASE_URL=postgres://db/qa_broken' || char(10) || 'DEBUG=1' || char(10))),
     'failed', 'boom' || char(10), 'image not found', NULL, 'user', '01HV000000000000000000A001', '2026-09-12T10:00:00Z', '2026-09-12T10:01:00Z');

-- beta/prod: one failed deploy, nothing ever succeeded.
INSERT INTO deploys (id, target_id, number, source, repo, git_ref, tag, compose, env, status, log, error, rolled_back_to, trigger_kind, trigger_id, started_at, finished_at) VALUES
    ('01HV00000000000000000J0001', '01HV000000000000000000C003', 1, 'ci', 'beta/shop', 'main', 'sha-b1',
     'services:' || char(10) || '  app:' || char(10) || '    image: git.example.com/beta/shop:${IMAGE_TAG}' || char(10),
     'sealed:' || lower(hex('SECRET_KEY=never-succeeded' || char(10))),
     'failed', 'boom' || char(10), 'container app is unhealthy', NULL, 'token', '01HV000000000000000000F003', '2026-09-13T10:00:00Z', '2026-09-13T10:01:00Z');

-- A CLI session of alice: v1 sessions are not imported (users log in again).
INSERT INTO sessions (id, user_id, token_hash, name, expires_at, last_used_at) VALUES
    ('01HV000000000000000000K001', '01HV000000000000000000A001', X'0000000000000000000000000000000000000000000000000000000000000001', 'laptop', '2026-11-01T10:00:00Z', '2026-10-01T10:00:00Z');
