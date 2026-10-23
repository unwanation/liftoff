-- The ten-minute cache of the template catalogue.
--
-- One row per `(user_id, host, repo)`: what that user's own connection
-- read from the repository's default branch. `manifest` is the
-- `liftoff.toml` text, NULL when the repository has none (the catalogue
-- warns about it without asking the forge again). The catalogue decides
-- from `fetched_at` whether the row is still fresh and replaces it in
-- place; rows go with their user.

CREATE TABLE cached_templates (
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    host       TEXT NOT NULL,
    repo       TEXT NOT NULL,
    manifest   TEXT,
    fetched_at TEXT NOT NULL,
    PRIMARY KEY (user_id, host, repo)
) STRICT;
