-- Core settings, one row per key.
--
-- `setup_token`: the live single-use login token as `<user id>:<hex
-- SHA-256>` (`liftoffd setup` and `lof admin token` write it, the first
-- `POST /auth/token` deletes it). The store checks the user exists when it
-- writes the row; `updated_at` is when the token was minted.

CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;
