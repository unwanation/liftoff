//! Typed ULID identifiers, one per entity, so a `UserId` cannot be passed
//! where a `DomainId` is expected.

use core::fmt;
use core::str::FromStr;

use ulid::Ulid;

use crate::error::{Problem, ValidationError};

macro_rules! ulid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Ulid);

        impl $name {
            /// Wraps a freshly generated ULID.
            #[must_use]
            pub const fn new(ulid: Ulid) -> Self {
                Self(ulid)
            }

            /// The underlying ULID.
            #[must_use]
            pub const fn ulid(self) -> Ulid {
                self.0
            }

            /// Parses the 26-character Crockford base32 form.
            ///
            /// # Errors
            ///
            /// A [`ValidationError`] on field `id` otherwise.
            pub fn parse(raw: &str) -> Result<Self, ValidationError> {
                Ulid::from_string(raw).map(Self).map_err(|_| {
                    ValidationError::new(
                        "id",
                        Problem::Pattern {
                            expected: "a 26-character ULID",
                        },
                    )
                })
            }
        }

        impl From<Ulid> for $name {
            fn from(ulid: Ulid) -> Self {
                Self(ulid)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = ValidationError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
    };
}

ulid_id!(
    /// Identifies a user.
    UserId
);
ulid_id!(
    /// Identifies a domain.
    DomainId
);
ulid_id!(
    /// Identifies a binding.
    BindingId
);
ulid_id!(
    /// Identifies a deploy.
    DeployId
);
ulid_id!(
    /// Identifies a deploy token.
    DeployTokenId
);
ulid_id!(
    /// Identifies a session.
    SessionId
);
ulid_id!(
    /// Identifies a project.
    ProjectId
);
ulid_id!(
    /// Identifies an environment of a project.
    EnvironmentId
);
ulid_id!(
    /// Identifies a secret (one key in one scope; its versions share it).
    SecretId
);
ulid_id!(
    /// Identifies an audit entry.
    AuditId
);
ulid_id!(
    /// Identifies a user's connection to a git forge.
    GitConnectionId
);
ulid_id!(
    /// Identifies a tenant of the `tenants` module; the core only carries
    /// it (`projects.tenant_id`, `domains.tenant_id`).
    TenantId
);

#[cfg(test)]
mod tests {
    use super::*;

    mod parse {
        use super::*;

        #[test]
        fn round_trips_when_displayed_and_parsed() {
            let id = DeployId::new(Ulid::from_parts(1, 2));
            assert_eq!(DeployId::parse(&id.to_string()).unwrap(), id);
        }

        #[test]
        fn rejects_when_not_ulid() {
            assert_eq!(UserId::parse("nope").unwrap_err().field, "id");
        }
    }
}
