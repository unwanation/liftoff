-- The links of artifact versions to their blobs and the count of links
-- per blob, kept by `FileBlobs`.
--
-- The bytes are files under `paths.data/artifacts/`: `blobs/<hash>` and
-- `<artifact>/<n>/<name>`, a hard link to (or copy of) the blob. Neither
-- records which blob a version's file is, so `artifact_blob_links` does:
-- one row per file of a version. `artifact_blobs.refs` counts the rows
-- naming each blob; the row goes, and the blob's file with it, when the
-- count would reach zero (BLOB-03). A blob put but never linked has no
-- row. The links are no foreign key of `artifact_versions`: the blobs
-- are written after the version's rows and removed before them (`rm`,
-- the sweeper) or after them (pruning), and the sweep at start removes
-- the links and directories of versions that have no row.

CREATE TABLE artifact_blobs (
    hash TEXT PRIMARY KEY
         CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    refs INTEGER NOT NULL CHECK (refs >= 1)
) STRICT, WITHOUT ROWID;

CREATE TABLE artifact_blob_links (
    artifact_id TEXT NOT NULL,
    number      INTEGER NOT NULL CHECK (number >= 1),
    name        TEXT NOT NULL CHECK (name <> ''),
    hash        TEXT NOT NULL REFERENCES artifact_blobs (hash),
    PRIMARY KEY (artifact_id, number, name)
) STRICT, WITHOUT ROWID;

CREATE INDEX artifact_blob_links_by_hash ON artifact_blob_links (hash);
