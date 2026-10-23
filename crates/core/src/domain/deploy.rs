//! Deploys and deploy tokens.

use core::fmt;

use sha2::{Digest as _, Sha256};

use super::ids::{DeployId, DeployTokenId, EnvironmentId, ProjectId, UserId};
use super::refs::{Branch, ImageTag};
use super::spelled::spelled_enum;
use super::timestamp::Timestamp;
use super::token::TokenHash;
use crate::error::{Problem, ValidationError};

/// The per-environment sequence number of a deploy (`#12`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeployNumber(u32);

impl DeployNumber {
    /// The first deploy of an environment.
    pub const FIRST: Self = Self(1);

    /// Validates a positive number.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `number` for zero.
    pub fn new(number: u32) -> Result<Self, ValidationError> {
        if number == 0 {
            Err(ValidationError::new(
                "number",
                Problem::OutOfRange {
                    min: 1,
                    max: u32::MAX.into(),
                },
            ))
        } else {
            Ok(Self(number))
        }
    }

    /// The number after this one, or `None` on overflow.
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

impl fmt::Display for DeployNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What started a deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeploySource {
    /// `POST /deploys` with a deploy token.
    Ci,
    /// `lof deploy` with a session.
    Cli,
    /// A redeploy or an automatic rollback.
    Redeploy,
}

spelled_enum!(DeploySource { Ci => "ci", Cli => "cli", Redeploy => "redeploy" });

/// Where a deploy is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeployStatus {
    /// Waiting in the queue.
    Queued,
    /// Being executed.
    Running,
    /// Finished; containers are up.
    Succeeded,
    /// Finished with an error.
    Failed,
}

spelled_enum!(DeployStatus {
    Queued => "queued",
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
});

impl DeployStatus {
    /// Whether the deploy has finished.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// Who triggered a deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// A logged-in user.
    User(UserId),
    /// A project's deploy token (CI).
    Token(DeployTokenId),
}

/// The largest `.wasm` component a function deploy takes: 50 MiB
/// (the field `component` of `POST /deploys`; FDEP-01).
pub const MAX_COMPONENT_BYTES: usize = 50 * 1024 * 1024;

/// The SHA-256 of a function's `.wasm` component.
///
/// 64 lower-case hex characters: the file `paths.data/functions/<hash>.wasm` and the key of the
/// precompiled cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentHash(String);

impl ComponentHash {
    /// The hash of the component `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        // The same lower-case hex a token hash prints.
        Self(TokenHash::from_bytes(Sha256::digest(bytes).into()).to_string())
    }

    /// Parses 64 lower-case hex characters (read back from the store).
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `component_hash` otherwise.
    pub fn parse(raw: &str) -> Result<Self, ValidationError> {
        let hex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
        if raw.len() == 64 && raw.bytes().all(hex) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(ValidationError::new(
                "component_hash",
                Problem::Pattern {
                    expected: "64 lower-case hex characters",
                },
            ))
        }
    }

    /// The 64 hex characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 hex characters, as a deploy's log and `tag` show the
    /// component.
    #[must_use]
    pub fn short(&self) -> &str {
        self.0.get(..12).unwrap_or(&self.0)
    }
}

impl fmt::Display for ComponentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One execution of an environment's compose project. The env is not
/// stored: it is assembled from secrets at execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deploy {
    /// Identifier.
    pub id: DeployId,
    /// The environment.
    pub environment_id: EnvironmentId,
    /// Sequence number within the environment.
    pub number: DeployNumber,
    /// What started it.
    pub source: DeploySource,
    /// Branch the image came from.
    pub git_ref: Option<Branch>,
    /// Image tag.
    pub tag: ImageTag,
    /// The compose file as sent.
    pub compose: String,
    /// The component a function deploy activates; `None` for compose.
    pub component_hash: Option<ComponentHash>,
    /// Lifecycle state.
    pub status: DeployStatus,
    /// Step output, including docker's.
    pub log: String,
    /// Short reason of a failure.
    pub error: Option<String>,
    /// For automatic rollbacks: the deploy whose content was redeployed.
    pub rolled_back_to: Option<DeployNumber>,
    /// Who triggered it.
    pub triggered_by: Trigger,
    /// When execution started.
    pub started_at: Option<Timestamp>,
    /// When execution finished.
    pub finished_at: Option<Timestamp>,
}

/// A project's CI credential; at most one live token per project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployToken {
    /// Identifier.
    pub id: DeployTokenId,
    /// The project it deploys into.
    pub project_id: ProjectId,
    /// SHA-256 of the secret.
    pub token_hash: TokenHash,
    /// Who caused it to be issued.
    pub created_by: UserId,
    /// Issue time.
    pub created_at: Timestamp,
    /// Revocation time; `None` while live.
    pub revoked_at: Option<Timestamp>,
}

impl DeployToken {
    /// Whether the token has not been revoked.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.revoked_at.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod component_hash {
        use super::*;

        /// SHA-256 of `abc` (FIPS 180-2, appendix B.1).
        const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        // TC: FDEP-05 (the file and the cache are keyed by the SHA-256)
        #[test]
        fn gives_lower_case_sha256_hex_when_hashing_bytes() {
            assert_eq!(ComponentHash::of(b"abc").as_str(), ABC);
        }

        // TC: FDEP-05 (what the store reads back is the hash `of` gives)
        #[test]
        fn round_trips_when_parsing_what_of_gives() {
            let hash = ComponentHash::of(b"\0asm component");
            assert_eq!(ComponentHash::parse(hash.as_str()).ok(), Some(hash));
        }

        // TC: FDEP-05
        #[test]
        fn refuses_when_upper_case() {
            assert!(ComponentHash::parse(&ABC.to_ascii_uppercase()).is_err());
        }

        // TC: FDEP-05
        #[test]
        fn refuses_when_63_characters() {
            assert!(ComponentHash::parse(ABC.strip_suffix('d').unwrap()).is_err());
        }

        // TC: FDEP-05
        #[test]
        fn refuses_when_65_characters() {
            assert!(ComponentHash::parse(&format!("{ABC}0")).is_err());
        }

        // TC: FDEP-05
        #[test]
        fn refuses_when_not_hex() {
            let raw = ABC.replacen('b', "g", 1);
            let refused = ComponentHash::parse(&raw).map_err(|error| error.field);
            assert_eq!(refused, Err("component_hash"));
        }
    }
}
