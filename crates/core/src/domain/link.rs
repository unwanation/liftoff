//! One-time links (11): what `lof dev` and `lof artifact open` print
//! so a browser without OIDC gets a `browser` session.

use super::artifact::ArtifactId;
use super::ids::{EnvironmentId, UserId};
use super::timestamp::Timestamp;
use super::token::TokenHash;
use crate::error::{Problem, ValidationError};

/// The stored prefix of an environment target.
const ENVIRONMENT: &str = "environment:";
/// The stored prefix of an artifact target.
const ARTIFACT: &str = "artifact:";

/// What a one-time link opens, and what a private address is (the
/// target of [`Access`](crate::service::Access)): an environment or an
/// artifact.
///
/// The store spells it `environment:<id>` or `artifact:<id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LinkTarget {
    /// The domains of an environment (a tunnel's dev address, a function).
    Environment(EnvironmentId),
    /// An artifact, `/a/<id>`.
    Artifact(ArtifactId),
}

impl LinkTarget {
    /// The stored spelling: `environment:<id>` or `artifact:<id>`.
    #[must_use]
    pub fn as_stored(self) -> String {
        match self {
            Self::Environment(id) => format!("{ENVIRONMENT}{id}"),
            Self::Artifact(id) => format!("{ARTIFACT}{id}"),
        }
    }

    /// Parses the stored spelling (also what `POST /auth/links` takes as
    /// `target`).
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `target` for anything but
    /// `environment:` followed by a ULID or `artifact:` followed by an
    /// [`ArtifactId`].
    pub fn parse(raw: &str) -> Result<Self, ValidationError> {
        let invalid = || {
            ValidationError::new(
                "target",
                Problem::Pattern {
                    expected: "environment:<ULID> or artifact:<id>",
                },
            )
        };
        if let Some(id) = raw.strip_prefix(ARTIFACT) {
            return ArtifactId::parse(id)
                .map(Self::Artifact)
                .map_err(|_not_an_id| invalid());
        }
        let id = raw.strip_prefix(ENVIRONMENT).ok_or_else(invalid)?;
        EnvironmentId::parse(id)
            .map(Self::Environment)
            .map_err(|_not_a_ulid| invalid())
    }
}

/// A one-time link: ten minutes, one use, bound to its target. The token itself is never stored, only its hash (OTL-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneTimeLink {
    /// SHA-256 of the [`LinkSecret`](super::LinkSecret) (unique; the
    /// key of the row).
    pub token_hash: TokenHash,
    /// Whose `browser` session it opens.
    pub user_id: UserId,
    /// What it opens.
    pub target: LinkTarget,
    /// Until when it can be redeemed.
    pub expires_at: Timestamp,
    /// When it was redeemed; `None` while it is unused.
    pub used_at: Option<Timestamp>,
    /// Creation time.
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::*;

    mod link_target {
        use super::*;

        fn prod() -> LinkTarget {
            LinkTarget::Environment(EnvironmentId::new(Ulid::from_parts(1, 2)))
        }

        // TC: OTL-03 (the target is stored in its canonical spelling)
        #[test]
        fn spells_environment_and_id_when_stored() {
            assert_eq!(
                prod().as_stored(),
                format!("environment:{}", Ulid::from_parts(1, 2))
            );
        }

        // TC: OTL-03
        #[test]
        fn reads_back_target_when_parsed_from_stored() {
            assert_eq!(LinkTarget::parse(&prod().as_stored()), Ok(prod()));
        }

        fn report() -> LinkTarget {
            LinkTarget::Artifact(ArtifactId::parse("aB3dE6gH9jK2").unwrap())
        }

        // TC: ART-01 (the login link of a private artifact names it in
        // its canonical spelling)
        #[test]
        fn spells_artifact_and_id_when_stored() {
            assert_eq!(report().as_stored(), "artifact:aB3dE6gH9jK2");
        }

        // TC: ART-01
        #[test]
        fn reads_back_artifact_target_when_parsed_from_stored() {
            assert_eq!(LinkTarget::parse("artifact:aB3dE6gH9jK2"), Ok(report()));
        }

        // TC: ART-01 (an artifact target needs a well-formed id)
        #[test]
        fn rejects_artifact_target_when_id_malformed() {
            let rejected: Vec<bool> = ["artifact:", "artifact:short", "artifact:aB3dE6gH9jK2X"]
                .into_iter()
                .map(|raw| LinkTarget::parse(raw).is_err())
                .collect();
            assert_eq!(rejected, vec![true; 3]);
        }

        // TC: OTL-03 (a target that is no environment is refused)
        #[test]
        fn rejects_target_when_kind_unknown_or_id_malformed() {
            let rejected: Vec<bool> = [
                "project:01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "environment:",
                "environment:not-a-ulid",
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "",
            ]
            .into_iter()
            .map(|raw| LinkTarget::parse(raw).is_err())
            .collect();
            assert_eq!(rejected, vec![true; 5]);
        }
    }
}
