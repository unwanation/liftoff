-- Artifacts, their versions and the files of each version.
--
-- `artifacts.id`: 12 base62 characters drawn at random, as in
-- the standalone artifacts server. A `shared` artifact has no `expires_at`; a `private`
-- one expires `[artifacts] ttl` after its latest version or its
-- `unshare`. A version is unique per `(artifact_id, number)` and goes
-- with its artifact; its files (`artifact_files`: normalised relative
-- path, SHA-256 of the bytes, size, in the order the core gave them,
-- `index.html` first) go with their version. The bytes themselves are
-- content-addressed blobs outside the database (the `Blobs` port), so
-- `hash` is no foreign key. Users are never deleted: the owner and the
-- author of a version are `RESTRICT`.

CREATE TABLE artifacts (
    id              TEXT PRIMARY KEY
                    CHECK (length(id) = 12 AND id NOT GLOB '*[^0-9A-Za-z]*'),
    owner_user_id   TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    title           TEXT NOT NULL,
    visibility      TEXT NOT NULL CHECK (visibility IN ('private', 'shared')),
    current_version INTEGER NOT NULL CHECK (current_version >= 1),
    expires_at      TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    CHECK (visibility = 'private' OR expires_at IS NULL)
) STRICT;

CREATE INDEX artifacts_by_owner ON artifacts (owner_user_id);
CREATE INDEX artifacts_by_expiry ON artifacts (expires_at) WHERE visibility = 'private';

CREATE TABLE artifact_versions (
    artifact_id TEXT NOT NULL REFERENCES artifacts (id) ON DELETE CASCADE,
    number      INTEGER NOT NULL CHECK (number >= 1),
    title       TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL CHECK (size_bytes >= 0),
    created_by  TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (artifact_id, number)
) STRICT, WITHOUT ROWID;

CREATE TABLE artifact_files (
    artifact_id TEXT NOT NULL,
    number      INTEGER NOT NULL,
    position    INTEGER NOT NULL CHECK (position >= 0),
    name        TEXT NOT NULL CHECK (name <> ''),
    hash        TEXT NOT NULL
                CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    size        INTEGER NOT NULL CHECK (size >= 0),
    PRIMARY KEY (artifact_id, number, name),
    UNIQUE (artifact_id, number, position),
    FOREIGN KEY (artifact_id, number)
        REFERENCES artifact_versions (artifact_id, number) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE INDEX artifact_files_by_hash ON artifact_files (hash);
