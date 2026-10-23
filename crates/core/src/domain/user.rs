//! Users and their connections to git forges.

use core::fmt;

use super::ids::{GitConnectionId, UserId};
use super::refs::{Host, Login, OidcIssuer};
use super::sealed::SealedEnv;
use super::spelled::spelled_enum;
use super::timestamp::Timestamp;
use crate::error::{Problem, ValidationError};

/// Global role of a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserRole {
    /// Sees and changes everything. The single user of the single-user
    /// mode is one.
    Admin,
    /// Acts only where a module (`tenants`) lets them.
    User,
}

spelled_enum!(UserRole { Admin => "admin", User => "user" });

/// Where a user comes from: `local` (single-user mode) or
/// `oidc:<issuer>` (the `oidc` module, v1 import).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UserSource {
    /// Created by `liftoffd setup`.
    Local,
    /// Logged in through an OIDC provider of this issuer.
    Oidc(OidcIssuer),
}

impl UserSource {
    /// The stored spelling: `local` or `oidc:<issuer>`.
    #[must_use]
    pub fn as_string(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Oidc(issuer) => format!("oidc:{issuer}"),
        }
    }

    /// Parses `local` or `oidc:<issuer>` with an [`OidcIssuer`]. With the
    /// feature `test-support` the issuer may also be a plain-HTTP
    /// loopback one ([`OidcIssuer::loopback`]): the fake provider of a
    /// test whose users a real store keeps and reads back.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `source` otherwise.
    pub fn parse(raw: &str) -> Result<Self, ValidationError> {
        if raw == "local" {
            return Ok(Self::Local);
        }
        raw.strip_prefix("oidc:")
            .and_then(|issuer| {
                #[cfg(any(test, feature = "test-support"))]
                if let Ok(loopback) = OidcIssuer::loopback(issuer) {
                    return Some(loopback);
                }
                OidcIssuer::new(issuer).ok()
            })
            .map(Self::Oidc)
            .ok_or_else(|| {
                ValidationError::new(
                    "source",
                    Problem::Pattern {
                        expected: "local or oidc:<issuer>",
                    },
                )
            })
    }
}

impl fmt::Display for UserSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

/// A person who can act on liftoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// Identifier.
    pub id: UserId,
    /// Login (unique, lower case).
    pub login: Login,
    /// E-mail address.
    pub email: String,
    /// Global role.
    pub role: UserRole,
    /// Where the user comes from.
    pub source: UserSource,
    /// The OIDC subject; `None` for `local` users.
    pub subject: Option<String>,
    /// Creation time.
    pub created_at: Timestamp,
}

impl User {
    /// Whether the user is a liftoff admin.
    #[must_use]
    pub fn is_admin(&self) -> bool {
        self.role == UserRole::Admin
    }
}

/// A git forge provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitProvider {
    /// GitHub (module `git.github`).
    Github,
    /// GitLab (module `git.gitlab`).
    Gitlab,
    /// Forgejo (in the core, module `git.forgejo`).
    Forgejo,
}

spelled_enum!(GitProvider {
    Github => "github",
    Gitlab => "gitlab",
    Forgejo => "forgejo",
});

impl GitProvider {
    /// Whether a compose project's CI on this forge needs a registry
    /// login from liftoff to push its image: Forgejo's
    /// automatic token cannot push packages, GitHub's `GITHUB_TOKEN` and
    /// GitLab's `CI_REGISTRY_*` can.
    #[must_use]
    pub const fn ci_needs_registry_login(self) -> bool {
        matches!(self, Self::Forgejo)
    }
}

/// How a git connection was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialKind {
    /// An OAuth device flow.
    Oauth,
    /// A personal access token.
    Token,
}

spelled_enum!(CredentialKind { Oauth => "oauth", Token => "token" });

/// A user's credential on one forge host; unique by `(user_id, host)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitConnection {
    /// Identifier.
    pub id: GitConnectionId,
    /// The user it belongs to.
    pub user_id: UserId,
    /// The forge.
    pub provider: GitProvider,
    /// The forge host, e.g. `git.example.com`.
    pub host: Host,
    /// The user's account on the forge.
    pub account_login: Login,
    /// The token, sealed by the `Secrets` port.
    pub credential: SealedEnv,
    /// OAuth or token.
    pub kind: CredentialKind,
    /// When the credential stops working, if the forge said.
    pub expires_at: Option<Timestamp>,
    /// Creation time.
    pub created_at: Timestamp,
}

/// The device code of a forge's OAuth device flow (RFC 8628 3.2): what
/// the flow is polled with.
///
/// While the flow is pending it works as a credential (the OAuth
/// application's `client_id` is public, and the device flow needs no
/// client secret), so `Debug` never shows it.
///
/// ```
/// use liftoff_core::domain::DeviceCode;
///
/// let code = DeviceCode::new("dc-3584d835").unwrap();
/// assert_eq!(
///     (code.expose(), format!("{code:?}").as_str()),
///     ("dc-3584d835", "DeviceCode(<redacted>)"));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCode(String);

impl DeviceCode {
    /// The longest device code accepted, in bytes.
    pub const MAX_LEN: usize = 512;
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "visible ASCII characters, no spaces";

    /// Validates a device code a forge handed out.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `device_code` when `raw` is empty,
    /// longer than [`DeviceCode::MAX_LEN`] or not of
    /// [`DeviceCode::PATTERN`].
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let fail = |problem| Err(ValidationError::new("device_code", problem));
        if raw.is_empty() {
            return fail(Problem::Empty);
        }
        if raw.len() > Self::MAX_LEN {
            return fail(Problem::TooLong {
                max: Self::MAX_LEN,
                actual: raw.chars().count(),
            });
        }
        if raw.bytes().all(|byte| byte.is_ascii_graphic()) {
            Ok(Self(raw.to_owned()))
        } else {
            fail(Problem::Pattern {
                expected: Self::PATTERN,
            })
        }
    }

    /// The code. Named so every place that reveals it is easy to find.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceCode(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod device_code {
        use super::*;

        #[test]
        fn hides_code_when_debug_printed() {
            let code = DeviceCode::new("dc-3584d83530557fdd").unwrap();
            assert!(!format!("{code:?}").contains("dc-"));
        }

        #[test]
        fn rejects_as_empty_when_empty() {
            let error = DeviceCode::new("").unwrap_err();
            assert_eq!(
                (error.field, error.problem),
                ("device_code", Problem::Empty)
            );
        }

        #[test]
        fn rejects_as_too_long_when_longer_than_max() {
            let error = DeviceCode::new(&"a".repeat(DeviceCode::MAX_LEN + 1)).unwrap_err();
            assert_eq!(
                error.problem,
                Problem::TooLong {
                    max: DeviceCode::MAX_LEN,
                    actual: DeviceCode::MAX_LEN + 1,
                }
            );
        }

        #[test]
        fn accepts_when_exactly_max_long() {
            assert!(DeviceCode::new(&"a".repeat(DeviceCode::MAX_LEN)).is_ok());
        }

        #[rstest::rstest]
        #[case::space("dc 1")]
        #[case::newline("dc\n1")]
        #[case::non_ascii("dcé")]
        fn rejects_as_pattern_when_not_visible_ascii(#[case] raw: &str) {
            let error = DeviceCode::new(raw).unwrap_err();
            assert_eq!(
                error.problem,
                Problem::Pattern {
                    expected: DeviceCode::PATTERN
                }
            );
        }
    }

    mod user_source {
        use super::*;

        #[test]
        fn round_trips_when_oidc_issuer_valid() {
            let source = UserSource::parse("oidc:https://auth.example.com").unwrap();
            assert_eq!(
                (source.as_string(), source),
                (
                    "oidc:https://auth.example.com".to_owned(),
                    UserSource::Oidc(OidcIssuer::new("https://auth.example.com").unwrap())
                )
            );
        }

        #[test]
        fn rejects_as_pattern_when_issuer_not_an_oidc_issuer() {
            let error = UserSource::parse("oidc:http://auth.example.com").unwrap_err();
            assert_eq!(
                (error.field, error.problem),
                (
                    "source",
                    Problem::Pattern {
                        expected: "local or oidc:<issuer>"
                    }
                )
            );
        }
    }
}
