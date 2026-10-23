//! Secrets (scopes).

use core::fmt;

use super::ids::{EnvironmentId, ProjectId, SecretId, TenantId, UserId};
use super::names::{EnvName, ProjectName};
use super::sealed::SealedEnv;
use super::spelled::spelled_enum;
use super::timestamp::Timestamp;
use crate::error::{Problem, ValidationError};

/// A secret's name, an env variable: `^[A-Z][A-Z0-9_]{0,63}$`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretKey(String);

impl SecretKey {
    /// The shape of a key.
    pub const PATTERN: &'static str = "^[A-Z][A-Z0-9_]{0,63}$";
    /// Maximum length of a key.
    pub const MAX_LEN: usize = 64;

    /// Validates a key; errors name the field `key`.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] when `raw` is empty, longer than 64
    /// characters or not of [`SecretKey::PATTERN`].
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let fail = |problem| Err(ValidationError::new("key", problem));
        if raw.is_empty() {
            return fail(Problem::Empty);
        }
        let len = raw.chars().count();
        if len > Self::MAX_LEN {
            return fail(Problem::TooLong {
                max: Self::MAX_LEN,
                actual: len,
            });
        }
        let mut chars = raw.chars();
        let head_ok = chars.next().is_some_and(|c| c.is_ascii_uppercase());
        let tail_ok = chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if head_ok && tail_ok {
            Ok(Self(raw.to_owned()))
        } else {
            fail(Problem::Pattern {
                expected: Self::PATTERN,
            })
        }
    }
}

super::names::string_newtype!(SecretKey);

/// A secret's plaintext value. `Debug` is redacted; read it with
/// [`SecretValue::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps a plaintext value.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    /// The plaintext. Named so every place that reveals it is easy to find.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

/// The kind of a secret scope, as stored in `secrets.scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ScopeKind {
    /// Every environment of every project of a tenant (the `tenants`
    /// module puts projects into tenants).
    Tenant,
    /// Every environment of a project.
    Project,
    /// One environment.
    Environment,
}

spelled_enum!(ScopeKind { Tenant => "tenant", Project => "project", Environment => "environment" });

/// Where a secret applies; the lower scope overrides the upper one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum SecretScope {
    /// Every environment of every project of a tenant.
    Tenant(TenantId),
    /// Every environment of a project.
    Project(ProjectId),
    /// One environment.
    Environment(EnvironmentId),
}

impl SecretScope {
    /// The stored kind (`secrets.scope`).
    #[must_use]
    pub const fn kind(self) -> ScopeKind {
        match self {
            Self::Tenant(_) => ScopeKind::Tenant,
            Self::Project(_) => ScopeKind::Project,
            Self::Environment(_) => ScopeKind::Environment,
        }
    }

    /// The stored id (`secrets.scope_id`).
    #[must_use]
    pub const fn id(self) -> ulid::Ulid {
        match self {
            Self::Tenant(id) => id.ulid(),
            Self::Project(id) => id.ulid(),
            Self::Environment(id) => id.ulid(),
        }
    }
}

/// A secret scope addressed by name, as the CLI and the API name it
/// (`--project` or the environment of the project directory). The service
/// resolves it to a [`SecretScope`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ScopeRef {
    /// Every environment of every project of the tenant the project is in
    /// (`--scope tenant` in the project directory); a project outside
    /// every tenant (no `tenants` module) has none.
    Tenant(ProjectName),
    /// Every environment of the project.
    Project(ProjectName),
    /// One environment of the project.
    Environment(ProjectName, EnvName),
}

/// The env an environment runs with: the merged secrets of its
/// scopes followed by the system variables. The order is part of the
/// value: two resolutions of the same state are equal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Env(Vec<(SecretKey, SecretValue)>);

impl Env {
    /// Wraps `vars` in the order given.
    #[must_use]
    pub const fn new(vars: Vec<(SecretKey, SecretValue)>) -> Self {
        Self(vars)
    }

    /// `(name, value)` in order.
    #[must_use]
    pub fn vars(&self) -> &[(SecretKey, SecretValue)] {
        &self.0
    }
}

/// A version number of a secret, from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VersionNumber(u32);

impl VersionNumber {
    /// The first version.
    pub const FIRST: Self = Self(1);

    /// Validates a positive number.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `version` for zero.
    pub fn new(number: u32) -> Result<Self, ValidationError> {
        if number == 0 {
            Err(ValidationError::new(
                "version",
                Problem::OutOfRange {
                    min: 1,
                    max: u32::MAX.into(),
                },
            ))
        } else {
            Ok(Self(number))
        }
    }

    /// The version after this one, or `None` on overflow.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for VersionNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Who wrote a secret version (`updated_by`): a user, CI through the
/// deprecated `env` body field or the v1 import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdatedBy {
    /// A user.
    User(UserId),
    /// `POST /deploys` with `env` (stored as `ci`).
    Ci,
    /// `liftoffd setup --import` (stored as `import`).
    Import,
}

/// The current value of one key in one scope; unique by `(scope, key)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Secret {
    /// Identifier, shared by its versions.
    pub id: SecretId,
    /// Where it applies.
    pub scope: SecretScope,
    /// The env variable.
    pub key: SecretKey,
    /// The current value, sealed by the `Secrets` port.
    pub value: SealedEnv,
    /// The current version.
    pub version: VersionNumber,
    /// Whether it is synced into the forge's CI secrets.
    pub ci: bool,
    /// Who wrote the current version.
    pub updated_by: UpdatedBy,
    /// When the current version was written.
    pub updated_at: Timestamp,
    /// Creation time (version 1).
    pub created_at: Timestamp,
}

/// One entry of a secret's history (`secret_versions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretVersion {
    /// The secret.
    pub secret_id: SecretId,
    /// Its number.
    pub version: VersionNumber,
    /// The value, sealed.
    pub value: SealedEnv,
    /// Who wrote it.
    pub updated_by: UpdatedBy,
    /// When.
    pub updated_at: Timestamp,
}

/// Where a secret is synced to (`secret_syncs.target`): only
/// the forge's CI secrets of the project's repository so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum SyncTarget {
    /// The CI secrets (GitHub and Forgejo Actions secrets, GitLab CI
    /// variables) of the project's repository.
    Ci,
}

spelled_enum!(SyncTarget { Ci => "ci" });

/// What the last sync of a secret to a target did (`secret_syncs`): one row per `(secret_id, target)`,
/// replaced by every attempt.
///
/// `version` is an addition to the spec's columns: the version the
/// target holds, so a sync with nothing changed sends nothing (SYNC-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSyncRecord {
    /// The secret.
    pub secret_id: SecretId,
    /// Where it went.
    pub target: SyncTarget,
    /// The version the target holds: the last one sent successfully;
    /// `None` while no attempt has succeeded.
    pub version: Option<VersionNumber>,
    /// When `version` was sent; `None` while no attempt has succeeded.
    pub synced_at: Option<Timestamp>,
    /// Why the last attempt failed; `None` when it succeeded.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod secret_key {
        use super::*;

        // TC: SEC-01
        #[rstest]
        #[case::catalogue("DATABASE_URL")]
        #[case::single_letter("A")]
        #[case::digits("S3_BUCKET")]
        #[case::trailing_underscore("TOKEN_")]
        #[case::max_len("A234567890123456789012345678901234567890123456789012345678901234")]
        fn accepts_when_shape_matches(#[case] raw: &str) {
            assert_eq!(
                SecretKey::new(raw).map(|key| key.to_string()),
                Ok(raw.to_owned()),
                "{raw:?} should be a valid key"
            );
        }

        // TC: SEC-01
        #[rstest]
        #[case::catalogue_lower_case("database_url")]
        #[case::digit_first("1ABC")]
        #[case::underscore_first("_A")]
        #[case::dash("A-B")]
        #[case::space("A B")]
        #[case::mixed_case("Database_URL")]
        #[case::non_ascii("ÄB")]
        fn rejects_pattern_when_shape_differs(#[case] raw: &str) {
            assert_eq!(
                SecretKey::new(raw),
                Err(ValidationError::new(
                    "key",
                    Problem::Pattern {
                        expected: "^[A-Z][A-Z0-9_]{0,63}$"
                    }
                ))
            );
        }

        // TC: SEC-01
        #[test]
        fn rejects_too_long_when_65_chars() {
            assert_eq!(
                SecretKey::new(&"A".repeat(65)),
                Err(ValidationError::new(
                    "key",
                    Problem::TooLong {
                        max: 64,
                        actual: 65
                    }
                ))
            );
        }

        // TC: SEC-01
        #[test]
        fn rejects_empty_when_blank() {
            assert_eq!(
                SecretKey::new(""),
                Err(ValidationError::new("key", Problem::Empty))
            );
        }

        // TC: SEC-01
        #[test]
        fn converts_into_validation_error_when_lower_case() {
            let error: crate::Error = SecretKey::new("database_url").unwrap_err().into();
            assert!(
                matches!(&error, crate::Error::Validation(v) if v.field == "key"),
                "a bad key is Error::Validation on `key`, got {error:?}"
            );
        }
    }

    mod secret_value {
        use super::*;

        // TC: v1 AUTH-12 (secrets never reach logs through Debug)
        #[test]
        fn redacted_when_debug_formatted() {
            let value = SecretValue::new("hunter2-plaintext".to_owned());
            let shown = format!("{value:?}");
            assert!(!shown.contains("hunter2-plaintext"), "{shown}");
        }
    }
}
