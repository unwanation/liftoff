-- The gate in front of protected environments (the v1 SSO spec, section 6.3).
--
-- `environments.protected`: the flag `lof protect` sets (only the
-- `tenants` module writes it, through `core-call
-- environments.set_protected`). `gate_codes`: the one-time codes
-- `/gate/start` issues, ten minutes, one use. `gate_sessions`: the
-- sessions on one protected host (the cookie `__Host-liftoff_gate`). Both
-- go with the browser session they were handed over from (logout, `lof
-- admin sessions revoke`).

ALTER TABLE environments ADD COLUMN protected INTEGER NOT NULL DEFAULT 0 CHECK (protected IN (0, 1));

CREATE TABLE gate_codes (
    code_hash  BLOB PRIMARY KEY CHECK (length(code_hash) = 32),
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    host       TEXT NOT NULL,
    rd         TEXT NOT NULL,
    expires_at TEXT NOT NULL
) STRICT;

CREATE INDEX gate_codes_session ON gate_codes (session_id);

CREATE TABLE gate_sessions (
    token_hash BLOB PRIMARY KEY CHECK (length(token_hash) = 32),
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    host       TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL
) STRICT;

CREATE INDEX gate_sessions_session ON gate_sessions (session_id);
