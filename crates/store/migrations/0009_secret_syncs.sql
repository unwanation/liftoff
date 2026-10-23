-- What the last sync of a secret to a target did.
--
-- One row per `(secret_id, target)`, replaced by every attempt. `version`
-- is an addition to the spec's columns: the version the target holds (the
-- last one sent successfully), so a sync with nothing changed sends
-- nothing (SYNC-03). `version` and `synced_at` are NULL while no attempt
-- has succeeded; `error` holds why the last attempt failed, NULL when it
-- succeeded. Rows go with their secret, and so with its environment or
-- project.

CREATE TABLE secret_syncs (
    secret_id TEXT NOT NULL REFERENCES secrets (id) ON DELETE CASCADE,
    target    TEXT NOT NULL CHECK (target IN ('ci')),
    version   INTEGER CHECK (version > 0),
    synced_at TEXT,
    error     TEXT,
    PRIMARY KEY (secret_id, target)
) STRICT;
