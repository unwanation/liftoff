-- The key spaces of modules (8.4 `store-get`/`store-set`): one
-- row per module and key. A module's own tables (`m_<module>_*`) are
-- created by the module itself through `store-query`.

CREATE TABLE module_keys (
    module TEXT NOT NULL,
    key    TEXT NOT NULL,
    value  BLOB NOT NULL,
    PRIMARY KEY (module, key)
) STRICT, WITHOUT ROWID;
