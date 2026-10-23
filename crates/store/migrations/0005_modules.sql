-- Installed modules.
-- The component itself is the file `paths.data/modules/<name>-<version>.wasm`.
-- `config` is a JSON object by top-level field: `{"plain": <value>}` or,
-- for a field the schema marks `"format": "secret"`, `{"sealed":
-- "<ciphertext>"}` (REG-05). Installation order is `rowid` order: the
-- order hooks run in. `sha256` is the hex SHA-256 of the component as
-- verified at install; the file is checked against it before every load.

CREATE TABLE modules (
    name         TEXT PRIMARY KEY,
    version      TEXT NOT NULL,
    sha256       TEXT NOT NULL,
    enabled      INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    config       TEXT NOT NULL,
    error        TEXT,
    installed_at TEXT NOT NULL
) STRICT;
