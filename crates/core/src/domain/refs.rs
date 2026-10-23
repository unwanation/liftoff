//! References to things outside liftoff: hosts, Forgejo logins and
//! repositories, git branches, image tags and ports.

use core::fmt;
use core::ops::RangeInclusive;

use super::names::{Label, string_newtype};
use crate::error::{Problem, ValidationError};

/// A fully qualified, lower-case host name, e.g. `example-qa.example.com`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Host(String);

impl Host {
    /// Maximum length of a host name.
    pub const MAX_LEN: usize = 253;
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "lower-case DNS labels of [a-z0-9-] (1-63 chars, no leading or trailing dash) joined by dots, at least two labels";

    /// Validates a host name.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `host` when `raw` is not a lower-case
    /// FQDN with at least two labels.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let fail = |problem| Err(ValidationError::new("host", problem));
        if raw.is_empty() {
            return fail(Problem::Empty);
        }
        if raw.len() > Self::MAX_LEN {
            return fail(Problem::TooLong {
                max: Self::MAX_LEN,
                actual: raw.len(),
            });
        }
        let pattern = Problem::Pattern {
            expected: Self::PATTERN,
        };
        let labels: Vec<&str> = raw.split('.').collect();
        if labels.len() < 2 || !labels.iter().all(|l| is_dns_label(l)) {
            return fail(pattern);
        }
        Ok(Self(raw.to_owned()))
    }

    /// `<label>.<zone>`.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] when the result is longer than [`Host::MAX_LEN`].
    pub fn under(label: &Label, zone: &Self) -> Result<Self, ValidationError> {
        Self::new(&format!("{label}.{zone}"))
    }

    /// The left-most label when this host sits directly under `zone`.
    #[must_use]
    pub fn label_in(&self, zone: &Self) -> Option<Label> {
        let rest = self.0.strip_suffix(zone.as_str())?.strip_suffix('.')?;
        Label::new(rest).ok()
    }
}

fn is_dns_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

string_newtype!(Host);

/// The e-mail of the ACME account Caddy registers (`[proxy] acme_email`),
/// e.g. `ops@example.com`: a local part without spaces or `@`, then a
/// [`Host`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AcmeEmail(String);

impl AcmeEmail {
    /// Maximum length of the local part.
    pub const LOCAL_MAX_LEN: usize = 64;
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "<local>@<host>: a local part of 1-64 printable ASCII characters without spaces or '@', then a lower-case FQDN";

    /// Validates an ACME account e-mail.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `acme_email` when `raw` is empty or
    /// not `<local>@<host>`.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        if raw.is_empty() {
            return Err(ValidationError::new("acme_email", Problem::Empty));
        }
        let local_ok = |local: &str| {
            (1..=Self::LOCAL_MAX_LEN).contains(&local.len())
                && local.chars().all(|c| c.is_ascii_graphic() && c != '@')
        };
        match raw.split_once('@') {
            Some((local, host)) if local_ok(local) && Host::new(host).is_ok() => {
                Ok(Self(raw.to_owned()))
            }
            _ => Err(ValidationError::new(
                "acme_email",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            )),
        }
    }
}

string_newtype!(AcmeEmail);

/// An OIDC issuer, e.g. `https://auth.example.com`: `https://`, a
/// [`Host`], an optional port and an optional path, without query,
/// fragment or user info. Users of the provider are `oidc:<issuer>`.
///
/// Always `https://`: ID tokens and the keys that verify them never
/// travel in plain text. The fake providers of the tests, which serve
/// plain HTTP on loopback, go through [`OidcIssuer::loopback`], which
/// exists only with the `test-support` feature.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OidcIssuer(String);

impl OidcIssuer {
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str =
        "https://<host>[:<port>][/<path>]: a lower-case FQDN, no query, fragment or user info";

    /// Validates an OIDC issuer URL.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `issuer` when `raw` is empty or not
    /// of [`OidcIssuer::PATTERN`].
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        Self::parse(raw, false)
    }

    /// Validates an OIDC issuer URL as [`OidcIssuer::new`] does, but also
    /// accepts `http://` on `127.0.0.1` or `localhost`: the fake provider
    /// of a test (test support only).
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `issuer` otherwise.
    #[cfg(any(test, feature = "test-support"))]
    pub fn loopback(raw: &str) -> Result<Self, ValidationError> {
        Self::parse(raw, true)
    }

    fn parse(raw: &str, loopback: bool) -> Result<Self, ValidationError> {
        if raw.is_empty() {
            return Err(ValidationError::new("issuer", Problem::Empty));
        }
        let (secure, rest) = raw.strip_prefix("https://").map_or_else(
            || {
                let plain = raw.strip_prefix("http://").filter(|_rest| loopback);
                (false, plain.unwrap_or_default())
            },
            |rest| (true, rest),
        );
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
        let port_ok = authority == host || port.parse::<u16>().is_ok_and(|port| port > 0);
        let path_ok = path
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '?' | '#'));
        let host_ok = if secure {
            Host::new(host).is_ok()
        } else {
            matches!(host, "127.0.0.1" | "localhost")
        };
        if host_ok && port_ok && path_ok {
            Ok(Self(raw.to_owned()))
        } else {
            Err(ValidationError::new(
                "issuer",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            ))
        }
    }
}

string_newtype!(OidcIssuer);

const fn is_forgejo_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// A Forgejo login (equal to the Zitadel `preferred_username`).
///
/// Forgejo compares logins case-insensitively, so the value is stored in
/// ASCII lower case: `Bob` and `bob` are the same login.
///
/// ```
/// use liftoff_core::domain::Login;
///
/// assert_eq!(Login::new("Bob").unwrap(), Login::new("bob").unwrap());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Login(String);

impl Login {
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str =
        "1-40 chars of [A-Za-z0-9_.-], starting and ending with a letter or digit";

    /// Validates a Forgejo login and lower-cases it.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `login` when `raw` is not of
    /// [`Login::PATTERN`].
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let fail = |problem| Err(ValidationError::new("login", problem));
        if raw.is_empty() {
            return fail(Problem::Empty);
        }
        if raw.len() > 40 {
            return fail(Problem::TooLong {
                max: 40,
                actual: raw.chars().count(),
            });
        }
        let edges_ok = raw.starts_with(|c: char| c.is_ascii_alphanumeric())
            && raw.ends_with(|c: char| c.is_ascii_alphanumeric());
        if edges_ok && raw.chars().all(is_forgejo_name_char) {
            Ok(Self(raw.to_ascii_lowercase()))
        } else {
            fail(Problem::Pattern {
                expected: Self::PATTERN,
            })
        }
    }
}

string_newtype!(Login);

/// A Forgejo repository, written `owner/name`.
///
/// Forgejo resolves owners and repository names case-insensitively, so
/// both parts are stored in ASCII lower case: a pin made with `Bob/App`
/// matches the `bob/app` CI sends, and the two can never hold separate
/// live deploy tokens.
///
/// ```
/// use liftoff_core::domain::RepoRef;
///
/// assert_eq!(RepoRef::new("Bob/App").unwrap().to_string(), "bob/app");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoRef {
    owner: String,
    name: String,
}

impl RepoRef {
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "owner/name, each 1-100 chars of [A-Za-z0-9_.-]";

    /// Parses `owner/name` and lower-cases both parts.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `repo` when `raw` is not exactly two
    /// valid parts separated by one slash.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let pattern = || {
            Err(ValidationError::new(
                "repo",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            ))
        };
        if raw.is_empty() {
            return Err(ValidationError::new("repo", Problem::Empty));
        }
        let Some((owner, name)) = raw.split_once('/') else {
            return pattern();
        };
        let part_ok = |p: &str| {
            (1..=100).contains(&p.len())
                && p != "."
                && p != ".."
                && p.chars().all(is_forgejo_name_char)
        };
        if part_ok(owner) && part_ok(name) {
            Ok(Self {
                owner: owner.to_ascii_lowercase(),
                name: name.to_ascii_lowercase(),
            })
        } else {
            pattern()
        }
    }

    /// The owning user or organisation.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The repository name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl core::str::FromStr for RepoRef {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// A git branch name, e.g. `feature/x` (the `ref` of a CI deploy).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Branch(String);

impl Branch {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 255;
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "a git branch name (no spaces, control characters, `..`, `~^:?*[\\`, leading `-` or `/`, trailing `/`, `.` or `.lock`)";

    /// Validates a branch name with the rules of `git check-ref-format`.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `branch` when `raw` is not a usable
    /// branch name.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let fail = |problem| Err(ValidationError::new("branch", problem));
        if raw.is_empty() {
            return fail(Problem::Empty);
        }
        if raw.len() > Self::MAX_LEN {
            return fail(Problem::TooLong {
                max: Self::MAX_LEN,
                actual: raw.len(),
            });
        }
        let bad_char = raw.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        });
        let bad_shape = raw.starts_with(['-', '/', '.'])
            || raw.ends_with(['/', '.'])
            || raw.rsplit_once('.').is_some_and(|(_, ext)| ext == "lock")
            || raw.contains("..")
            || raw.contains("//")
            || raw.contains("/.")
            || raw.contains("@{")
            || raw == "@";
        if bad_char || bad_shape {
            return fail(Problem::Pattern {
                expected: Self::PATTERN,
            });
        }
        Ok(Self(raw.to_owned()))
    }
}

string_newtype!(Branch);

/// A container image tag, e.g. `sha-1a2b3c`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageTag(String);

impl ImageTag {
    /// The shape the OCI distribution spec allows for tags.
    pub const PATTERN: &'static str = "^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$";

    /// Validates a tag.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `tag` when `raw` does not match
    /// [`ImageTag::PATTERN`].
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let fail = |problem| Err(ValidationError::new("tag", problem));
        if raw.is_empty() {
            return fail(Problem::Empty);
        }
        if raw.len() > 128 {
            return fail(Problem::TooLong {
                max: 128,
                actual: raw.chars().count(),
            });
        }
        let mut chars = raw.chars();
        let head_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let tail_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if head_ok && tail_ok {
            Ok(Self(raw.to_owned()))
        } else {
            fail(Problem::Pattern {
                expected: Self::PATTERN,
            })
        }
    }
}

string_newtype!(ImageTag);

/// A loopback port on the host that a binding publishes, 20000-29999.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostPort(u16);

impl HostPort {
    /// Lowest port liftoff hands out.
    pub const MIN: u16 = 20_000;
    /// Highest port liftoff hands out.
    pub const MAX: u16 = 29_999;

    /// Validates that `port` lies in [`HostPort::MIN`]..=[`HostPort::MAX`].
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `host_port` otherwise.
    pub fn new(port: u16) -> Result<Self, ValidationError> {
        if (Self::MIN..=Self::MAX).contains(&port) {
            Ok(Self(port))
        } else {
            Err(ValidationError::new(
                "host_port",
                Problem::OutOfRange {
                    min: Self::MIN.into(),
                    max: Self::MAX.into(),
                },
            ))
        }
    }

    /// Every port of the range, lowest first.
    pub fn all() -> impl Iterator<Item = Self> {
        Self::each_in(&Self::full_range())
    }

    /// [`HostPort::MIN`]..=[`HostPort::MAX`], the default of
    /// `[domains] port_range`.
    #[must_use]
    pub const fn full_range() -> RangeInclusive<Self> {
        RangeInclusive::new(Self(Self::MIN), Self(Self::MAX))
    }

    /// Validates a configured `[domains] port_range = [low, high]`: both
    /// ends are host ports and `low <= high`.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `host_port` for an end outside
    /// [`HostPort::MIN`]..=[`HostPort::MAX`], or on field `port_range`
    /// when `low > high`.
    pub fn range(low: u16, high: u16) -> Result<RangeInclusive<Self>, ValidationError> {
        let (low, high) = (Self::new(low)?, Self::new(high)?);
        if low > high {
            return Err(ValidationError::new(
                "port_range",
                Problem::Invalid("the low end must not exceed the high end".to_owned()),
            ));
        }
        Ok(low..=high)
    }

    /// Every port of `range`, lowest first.
    pub fn each_in(range: &RangeInclusive<Self>) -> impl Iterator<Item = Self> + use<> {
        (range.start().0..=range.end().0).map(Self)
    }

    /// The port number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The port a service listens on inside its container, 1-65535.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContainerPort(u16);

impl ContainerPort {
    /// Validates that `port` is not zero.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `port` when `port` is zero.
    pub fn new(port: u16) -> Result<Self, ValidationError> {
        if port == 0 {
            Err(ValidationError::new(
                "port",
                Problem::OutOfRange {
                    min: 1,
                    max: u16::MAX.into(),
                },
            ))
        } else {
            Ok(Self(port))
        }
    }

    /// The port number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for ContainerPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod host {
        use super::*;

        #[rstest]
        #[case("example.com")]
        #[case("example-qa.example.com")]
        #[case("a.b.c")]
        fn accepts_when_lowercase_fqdn(#[case] raw: &str) {
            assert!(Host::new(raw).is_ok(), "{raw:?} should be valid");
        }

        #[rstest]
        #[case::single_label("localhost")]
        #[case::uppercase("Example.example.com")]
        #[case::empty_label("a..dev")]
        #[case::trailing_dot("a.dev.")]
        #[case::leading_dash("-a.dev")]
        #[case::underscore("a_b.dev")]
        #[case::label_too_long(&format!("{}.dev", "a".repeat(64)))]
        fn rejects_when_not_fqdn(#[case] raw: &str) {
            assert_eq!(Host::new(raw).unwrap_err().field, "host");
        }

        #[test]
        fn rejects_when_longer_than_253() {
            let raw = format!("{}.dev", vec!["a".repeat(60); 5].join("."));
            assert!(matches!(
                Host::new(&raw).unwrap_err().problem,
                Problem::TooLong { max: 253, .. }
            ));
        }

        #[test]
        fn joins_label_and_zone_when_under_zone() {
            let host = Host::under(
                &Label::new("example-qa").unwrap(),
                &Host::new("example.com").unwrap(),
            )
            .unwrap();
            assert_eq!(host.as_str(), "example-qa.example.com");
        }

        #[test]
        fn label_in_zone_returns_left_label_when_direct_child() {
            let zone = Host::new("example.com").unwrap();
            let host = Host::new("example-qa.example.com").unwrap();
            assert_eq!(
                host.label_in(&zone),
                Some(Label::new("example-qa").unwrap())
            );
        }

        #[test]
        fn label_in_zone_is_none_when_nested_deeper() {
            let zone = Host::new("example.com").unwrap();
            assert_eq!(Host::new("a.b.example.com").unwrap().label_in(&zone), None);
        }
    }

    mod acme_email {
        use super::*;

        #[rstest]
        #[case("ops@example.com")]
        #[case("ops+acme@mail.example.com")]
        fn accepts_when_local_part_at_host(#[case] raw: &str) {
            assert_eq!(AcmeEmail::new(raw).unwrap().as_str(), raw);
        }

        #[test]
        fn rejects_as_empty_when_empty() {
            let error = AcmeEmail::new("").unwrap_err();
            assert_eq!((error.field, error.problem), ("acme_email", Problem::Empty));
        }

        #[rstest]
        #[case::no_at("ops.example.com")]
        #[case::empty_local("@example.com")]
        #[case::empty_host("ops@")]
        #[case::two_ats("ops@acme@example.com")]
        #[case::space_in_local("o ps@example.com")]
        #[case::host_not_fqdn("ops@localhost")]
        #[case::host_uppercase("ops@Example.com")]
        #[case::local_too_long(&format!("{}@example.com", "a".repeat(65)))]
        fn rejects_as_pattern_when_not_local_at_host(#[case] raw: &str) {
            let error = AcmeEmail::new(raw).unwrap_err();
            assert_eq!(
                (error.field, error.problem),
                (
                    "acme_email",
                    Problem::Pattern {
                        expected: AcmeEmail::PATTERN
                    }
                )
            );
        }
    }

    mod oidc_issuer {
        use super::*;

        #[rstest]
        #[case("https://auth.example.com")]
        #[case("https://auth.example.com:8443")]
        #[case("https://login.example.com/realms/acme")]
        fn accepts_when_https_host_port_and_path(#[case] raw: &str) {
            assert_eq!(OidcIssuer::new(raw).unwrap().as_str(), raw);
        }

        #[test]
        fn rejects_as_empty_when_empty() {
            let error = OidcIssuer::new("").unwrap_err();
            assert_eq!((error.field, error.problem), ("issuer", Problem::Empty));
        }

        #[rstest]
        #[case::no_scheme("auth.example.com")]
        #[case::http("http://auth.example.com")]
        #[case::http_private_address("http://10.0.0.1")]
        #[case::http_loopback("http://127.0.0.1:8080")]
        #[case::http_localhost("http://localhost:5556/dex")]
        #[case::no_host("https://")]
        #[case::host_not_fqdn("https://localhost")]
        #[case::bad_port("https://auth.example.com:x")]
        #[case::empty_port("https://auth.example.com:")]
        #[case::user_info("https://me@auth.example.com")]
        #[case::query("https://auth.example.com/?a=1")]
        #[case::fragment("https://auth.example.com/#a")]
        #[case::space_in_path("https://auth.example.com/a b")]
        fn rejects_as_pattern_when_not_https_url(#[case] raw: &str) {
            let error = OidcIssuer::new(raw).unwrap_err();
            assert_eq!(
                (error.field, error.problem),
                (
                    "issuer",
                    Problem::Pattern {
                        expected: OidcIssuer::PATTERN
                    }
                )
            );
        }

        // The fake providers of the tests serve plain HTTP on loopback.
        #[rstest]
        #[case::loopback("http://127.0.0.1:8080")]
        #[case::localhost("http://localhost:5556/dex")]
        #[case::https("https://auth.example.com")]
        fn accepts_for_tests_when_loopback_http(#[case] raw: &str) {
            assert_eq!(OidcIssuer::loopback(raw).unwrap().as_str(), raw);
        }

        #[rstest]
        #[case::http("http://auth.example.com")]
        #[case::http_private_address("http://10.0.0.1")]
        #[case::query("http://127.0.0.1:8080/?a=1")]
        fn rejects_for_tests_when_http_is_not_loopback(#[case] raw: &str) {
            assert!(OidcIssuer::loopback(raw).is_err());
        }
    }

    mod login {
        use super::*;

        #[rstest]
        #[case("bob")]
        #[case("alice")]
        #[case("a.b-c_d9")]
        fn accepts_when_forgejo_shape(#[case] raw: &str) {
            assert!(Login::new(raw).is_ok(), "{raw:?} should be valid");
        }

        #[rstest]
        #[case::empty("")]
        #[case::leading_dot(".bob")]
        #[case::trailing_dash("bob-")]
        #[case::slash("b/ob")]
        #[case::too_long(&"a".repeat(41))]
        fn rejects_when_not_forgejo_shape(#[case] raw: &str) {
            assert_eq!(Login::new(raw).unwrap_err().field, "login");
        }

        #[test]
        fn equals_lower_case_when_spelled_with_capitals() {
            assert_eq!(Login::new("BoB").unwrap(), Login::new("bob").unwrap());
        }
    }

    mod repo_ref {
        use super::*;

        #[test]
        fn splits_owner_and_name_when_parsed() {
            let repo = RepoRef::new("bob/app").unwrap();
            assert_eq!((repo.owner(), repo.name()), ("bob", "app"));
        }

        #[test]
        fn equals_lower_case_when_spelled_with_capitals() {
            let mixed = RepoRef::new("Bob/My.App").unwrap();
            assert_eq!(mixed, RepoRef::new("bob/my.app").unwrap());
        }

        #[test]
        fn owner_slash_name_when_displayed() {
            assert_eq!(
                RepoRef::new("bob/my.app").unwrap().to_string(),
                "bob/my.app"
            );
        }

        #[rstest]
        #[case::no_slash("bobapp")]
        #[case::two_slashes("bob/app/x")]
        #[case::empty_owner("/app")]
        #[case::empty_name("bob/")]
        #[case::dot_dot("bob/..")]
        #[case::space("bob/my app")]
        fn rejects_when_not_owner_slash_name(#[case] raw: &str) {
            assert_eq!(RepoRef::new(raw).unwrap_err().field, "repo");
        }
    }

    mod branch {
        use super::*;

        #[rstest]
        #[case("main")]
        #[case("feature/x")]
        #[case("release-1.2")]
        fn accepts_when_valid_ref(#[case] raw: &str) {
            assert!(Branch::new(raw).is_ok(), "{raw:?} should be valid");
        }

        #[rstest]
        #[case::empty("")]
        #[case::space("my branch")]
        #[case::double_dot("a..b")]
        #[case::leading_dash("-x")]
        #[case::trailing_slash("x/")]
        #[case::lock_suffix("x.lock")]
        #[case::colon("a:b")]
        #[case::control("a\u{7}")]
        fn rejects_when_invalid_ref(#[case] raw: &str) {
            assert_eq!(Branch::new(raw).unwrap_err().field, "branch");
        }
    }

    mod image_tag {
        use super::*;

        #[rstest]
        #[case("sha-1")]
        #[case("latest")]
        #[case("_v1.2.3-rc")]
        fn accepts_when_oci_tag(#[case] raw: &str) {
            assert!(ImageTag::new(raw).is_ok(), "{raw:?} should be valid");
        }

        #[rstest]
        #[case::empty("")]
        #[case::leading_dot(".x")]
        #[case::colon("a:b")]
        #[case::too_long(&"a".repeat(129))]
        fn rejects_when_not_oci_tag(#[case] raw: &str) {
            assert_eq!(ImageTag::new(raw).unwrap_err().field, "tag");
        }
    }

    mod host_port {
        use super::*;

        // TC: v1 BND-01 (range 20000-29999)
        #[rstest]
        #[case(20_000)]
        #[case(29_999)]
        fn accepts_when_on_range_edge(#[case] port: u16) {
            assert_eq!(HostPort::new(port).unwrap().get(), port);
        }

        // TC: v1 BND-01
        #[rstest]
        #[case(19_999)]
        #[case(30_000)]
        #[case(0)]
        fn rejects_when_outside_range(#[case] port: u16) {
            assert_eq!(
                HostPort::new(port).unwrap_err().problem,
                Problem::OutOfRange {
                    min: 20_000,
                    max: 29_999
                }
            );
        }

        #[test]
        fn range_accepts_when_low_not_above_high() {
            let range = HostPort::range(20_010, 20_012).unwrap();
            let ports: Vec<u16> = HostPort::each_in(&range).map(HostPort::get).collect();
            assert_eq!(ports, vec![20_010, 20_011, 20_012]);
        }

        #[test]
        fn range_rejects_when_low_above_high() {
            assert_eq!(
                HostPort::range(20_002, 20_001).unwrap_err().field,
                "port_range"
            );
        }

        #[test]
        fn range_rejects_when_end_outside_host_ports() {
            assert_eq!(
                HostPort::range(20_000, 30_000).unwrap_err().field,
                "host_port"
            );
        }

        // TC: v1 BND-06 (10000 ports in total)
        #[test]
        fn yields_ten_thousand_ports_when_all() {
            assert_eq!(HostPort::all().count(), 10_000);
        }
    }

    mod container_port {
        use super::*;

        #[test]
        fn rejects_when_zero() {
            assert_eq!(ContainerPort::new(0).unwrap_err().field, "port");
        }

        #[test]
        fn accepts_when_max() {
            assert_eq!(ContainerPort::new(u16::MAX).unwrap().get(), u16::MAX);
        }
    }
}
