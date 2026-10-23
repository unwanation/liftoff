-- One-time links, the key-value store of functions and the component of
-- a function deploy.
--
-- `one_time_links`: keyed by the SHA-256 of the token, which is never
-- stored (OTL-05); ten minutes, one use (`used_at`), bound to its
-- `target` (`environment:<id>` now, `artifact:<id>`, so no
-- foreign key). `kv`: one value per `(environment_id, key)`, gone with
-- its environment. `deploys.component_hash`: the SHA-256 of the `.wasm`
-- of a function deploy, 64 lower-case hex characters; NULL for compose.
-- `environments.kind`, `owner_user_id` and `visibility` exist since
-- `0001_init.sql`.

ALTER TABLE deploys ADD COLUMN component_hash TEXT
    CHECK (component_hash IS NULL
           OR (length(component_hash) = 64 AND component_hash NOT GLOB '*[^0-9a-f]*'));

CREATE TABLE one_time_links (
    token_hash BLOB PRIMARY KEY CHECK (length(token_hash) = 32),
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    target     TEXT NOT NULL CHECK (target LIKE 'environment:_%' OR target LIKE 'artifact:_%'),
    expires_at TEXT NOT NULL,
    used_at    TEXT,
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE kv (
    environment_id TEXT NOT NULL REFERENCES environments (id) ON DELETE CASCADE,
    key            TEXT NOT NULL,
    value          BLOB NOT NULL,
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (environment_id, key)
) STRICT, WITHOUT ROWID;
