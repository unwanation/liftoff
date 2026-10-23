-- One user per account of an OIDC provider: the `oidc` module logs a person in by `(source,
-- subject)`, so a second row of the same subject is a conflict. `local`
-- users have no subject.

CREATE UNIQUE INDEX users_source_subject ON users (source, subject) WHERE subject IS NOT NULL;
