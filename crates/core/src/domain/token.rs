//! Bearer secrets and their stored hashes.
//!
//! Secrets are shown once and never stored: the store keeps the SHA-256
//! [`TokenHash`]. `Debug` on every secret type is redacted so a secret cannot
//! reach a log line through `{:?}` (AUTH-12).

use core::fmt;

use sha2::{Digest as _, Sha256};

use crate::error::{Problem, ValidationError};

/// The random part of a secret: exactly 40 base62 characters.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBody(String);

impl SecretBody {
    /// Number of characters.
    pub const LEN: usize = 40;
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "40 characters of [0-9A-Za-z]";

    /// Validates 40 base62 characters.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `token` otherwise.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        if raw.len() == Self::LEN && raw.chars().all(|c| c.is_ascii_alphanumeric()) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(ValidationError::new(
                "token",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            ))
        }
    }

    /// Borrows the characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `n` zero-padded to 40 decimal digits, least significant first (valid
    /// base62; a `u64` has at most 20 digits). The counter leads, so the
    /// first characters tell two secrets apart, as random ones do. For
    /// deterministic test generators only.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn from_counter(n: u64) -> Self {
        Self(format!("{n:040}").chars().rev().collect())
    }
}

impl fmt::Debug for SecretBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBody(<redacted>)")
    }
}

macro_rules! prefixed_secret {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Every value starts with `", $prefix, "`.")]
            pub const PREFIX: &'static str = $prefix;

            /// Builds the secret from freshly generated random characters.
            #[must_use]
            pub fn from_body(body: &SecretBody) -> Self {
                Self(format!("{}{}", Self::PREFIX, body.as_str()))
            }

            #[doc = concat!("Parses `", $prefix, "` followed by 40 base62 characters.")]
            ///
            /// # Errors
            ///
            /// A [`ValidationError`] on field `token` otherwise.
            pub fn parse(raw: &str) -> Result<Self, ValidationError> {
                let body = raw.strip_prefix(Self::PREFIX).ok_or_else(|| {
                    ValidationError::new(
                        "token",
                        Problem::Pattern {
                            expected: concat!($prefix, " followed by 40 characters of [0-9A-Za-z]"),
                        },
                    )
                })?;
                Ok(Self::from_body(&SecretBody::new(body)?))
            }

            /// The full secret. Named so every place that reveals it is easy
            /// to find.
            #[must_use]
            pub fn expose(&self) -> &str {
                &self.0
            }

            /// The SHA-256 of the secret, which is what the store keeps.
            #[must_use]
            pub fn hash(&self) -> TokenHash {
                TokenHash::of(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}(<redacted>)", stringify!($name))
            }
        }
    };
}

prefixed_secret!(
    /// A repository's deploy token: `lf_` + 40 base62 characters.
    ///
    /// `Display` prints the full secret (it is what goes into the Forgejo
    /// Actions secret and, once, into an API response); `Debug` is redacted.
    ///
    /// ```
    /// use liftoff_core::domain::{DeployTokenSecret, SecretBody};
    ///
    /// let body = SecretBody::new(&"a".repeat(40))?;
    /// let secret = DeployTokenSecret::from_body(&body);
    /// assert_eq!(secret.to_string().len(), 43);
    /// assert!(!format!("{secret:?}").contains("aaaa"));
    /// # Ok::<(), liftoff_core::ValidationError>(())
    /// ```
    DeployTokenSecret,
    "lf_"
);

impl fmt::Display for DeployTokenSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

prefixed_secret!(
    /// A CLI session token: `lfs_` + 40 base62 characters.
    ///
    /// The distinct prefix lets authentication tell a session from a deploy
    /// token before touching the store. No `Display`: use
    /// [`SessionSecret::expose`] where the token must be sent.
    SessionSecret,
    "lfs_"
);

prefixed_secret!(
    /// A single-use login token: `lft_` + 40 base62 characters.
    ///
    /// `liftoffd setup` prints the first one, `lof admin token` mints the
    /// next; `POST /auth/token` trades it for a `cli` session once. The
    /// store keeps only its hash. No `Display`: use [`SetupSecret::expose`]
    /// where it must be shown.
    SetupSecret,
    "lft_"
);

prefixed_secret!(
    /// A one-time code of the gate: `lfc_` + 40 base62 characters. `/gate/start` sends it to the protected host's
    /// `/.liftoff/callback`, which trades it for a [`GateSecret`] once.
    GateCodeSecret,
    "lfc_"
);

prefixed_secret!(
    /// The secret of a session on one protected host, the value of the
    /// cookie `__Host-liftoff_gate`: `lfg_` + 40 base62 characters. Not a
    /// [`Bearer`]: it never authenticates a call of the API.
    GateSecret,
    "lfg_"
);

prefixed_secret!(
    /// The secret of a one-time link: `lfl_` + 40 base62 characters.
    ///
    /// It opens a
    /// `browser` session once, within ten minutes, for the target it was
    /// issued for; the store keeps only its hash (OTL-05). Not a
    /// [`Bearer`]: it never authenticates a call of the API.
    LinkSecret,
    "lfl_"
);

/// A bearer token as presented in `Authorization: Bearer ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bearer {
    /// A CLI session.
    Session(SessionSecret),
    /// A repository deploy token.
    Deploy(DeployTokenSecret),
}

impl Bearer {
    /// Classifies `raw` by prefix and validates it.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `token` when `raw` is neither kind.
    pub fn parse(raw: &str) -> Result<Self, ValidationError> {
        if raw.starts_with(SessionSecret::PREFIX) {
            SessionSecret::parse(raw).map(Self::Session)
        } else {
            DeployTokenSecret::parse(raw).map(Self::Deploy)
        }
    }

    /// The hash to look the token up by.
    #[must_use]
    pub fn hash(&self) -> TokenHash {
        match self {
            Self::Session(secret) => secret.hash(),
            Self::Deploy(secret) => secret.hash(),
        }
    }
}

/// SHA-256 of a secret; the only form in which tokens are stored.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Hashes `secret`.
    #[must_use]
    pub fn of(secret: &str) -> Self {
        Self(Sha256::digest(secret.as_bytes()).into())
    }

    /// Wraps raw digest bytes (e.g. read back from the store).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses 64 lower-case hex characters.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `token_hash` otherwise.
    pub fn from_hex(raw: &str) -> Result<Self, ValidationError> {
        let invalid = || {
            ValidationError::new(
                "token_hash",
                Problem::Pattern {
                    expected: "64 lower-case hex characters",
                },
            )
        };
        if raw.len() != 64 {
            return Err(invalid());
        }
        let nibble = |c: u8| match c {
            b'0'..=b'9' => Some(c.wrapping_sub(b'0')),
            b'a'..=b'f' => Some(c.wrapping_sub(b'a').wrapping_add(10)),
            _ => None,
        };
        let mut bytes = [0_u8; 32];
        let (pairs, _rest) = raw.as_bytes().as_chunks::<2>();
        for (byte, &[hi, lo]) in bytes.iter_mut().zip(pairs) {
            let (hi, lo) = nibble(hi).zip(nibble(lo)).ok_or_else(invalid)?;
            *byte = (hi << 4_u8) | lo;
        }
        Ok(Self(bytes))
    }

    /// The digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for TokenHash {
    /// Lower-case hex.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenHash({self})")
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const BODY: &str = "0123456789abcdefghijABCDEFGHIJ0123456789";
    const BODY_39: &str = "0123456789abcdefghijABCDEFGHIJ012345678";

    mod secret_body {
        use super::*;

        #[test]
        fn accepts_when_40_base62() {
            assert_eq!(SecretBody::new(BODY).unwrap().as_str(), BODY);
        }

        #[rstest]
        #[case::short(BODY_39)]
        #[case::long(&format!("{BODY}a"))]
        #[case::dash(&format!("{BODY_39}-"))]
        fn rejects_when_not_40_base62(#[case] raw: &str) {
            assert_eq!(SecretBody::new(raw).unwrap_err().field, "token");
        }
    }

    mod deploy_token_secret {
        use super::*;

        // TC: v1 AUTH-11 (format lf_ + 40 base62)
        #[test]
        fn lf_prefix_and_body_when_displayed() {
            let secret = DeployTokenSecret::from_body(&SecretBody::new(BODY).unwrap());
            assert_eq!(secret.to_string(), format!("lf_{BODY}"));
        }

        #[test]
        fn round_trips_when_display_parsed() {
            let raw = format!("lf_{BODY}");
            assert_eq!(DeployTokenSecret::parse(&raw).unwrap().expose(), raw);
        }

        #[rstest]
        #[case::no_prefix(BODY)]
        #[case::session_prefix(&format!("lfs_{BODY}"))]
        #[case::short_body("lf_abc")]
        fn parse_rejects_when_not_deploy_token(#[case] raw: &str) {
            assert!(
                DeployTokenSecret::parse(raw).is_err(),
                "{raw:?} must not parse"
            );
        }

        // TC: v1 AUTH-12 (secrets never reach logs through Debug)
        #[test]
        fn redacted_when_debug_formatted() {
            let secret = DeployTokenSecret::from_body(&SecretBody::new(BODY).unwrap());
            let shown = format!("{secret:?}");
            assert!(!shown.contains("lf_") && !shown.contains(BODY), "{shown}");
        }

        // TC: v1 AUTH-01 (only the SHA-256 is stored)
        #[test]
        fn sha256_of_full_secret_when_hashed() {
            let secret = DeployTokenSecret::parse(&format!("lf_{BODY}")).unwrap();
            assert_eq!(secret.hash(), TokenHash::of(&format!("lf_{BODY}")));
        }
    }

    mod session_secret {
        use super::*;

        #[test]
        fn lfs_prefix_and_body_when_exposed() {
            let secret = SessionSecret::from_body(&SecretBody::new(BODY).unwrap());
            assert_eq!(secret.expose(), format!("lfs_{BODY}"));
        }

        // TC: v1 AUTH-12
        #[test]
        fn redacted_when_debug_formatted() {
            let secret = SessionSecret::from_body(&SecretBody::new(BODY).unwrap());
            assert!(
                !format!("{secret:?}").contains(BODY),
                "debug leaks the body"
            );
        }
    }

    mod bearer {
        use super::*;

        #[test]
        fn parses_session_when_lfs_prefix() {
            assert!(matches!(
                Bearer::parse(&format!("lfs_{BODY}")).unwrap(),
                Bearer::Session(_)
            ));
        }

        // TC: v1 AUTH-11
        #[test]
        fn parses_deploy_token_when_lf_prefix() {
            assert!(matches!(
                Bearer::parse(&format!("lf_{BODY}")).unwrap(),
                Bearer::Deploy(_)
            ));
        }

        // the cookie of a target host
        // (and a one-time link) never authenticates a call of the API.
        #[rstest]
        #[case::host_session("lfg_")]
        #[case::one_time_link("lfl_")]
        fn rejects_when_secret_is_not_a_bearer(#[case] prefix: &str) {
            let raw = format!("{prefix}{BODY}");
            assert!(Bearer::parse(&raw).is_err(), "{prefix} must not parse");
        }

        // TC: v1 AUTH-12
        #[rstest]
        #[case::garbage("hunter2")]
        #[case::empty("")]
        fn rejects_when_unknown_shape(#[case] raw: &str) {
            assert!(Bearer::parse(raw).is_err(), "{raw:?} must not parse");
        }
    }

    mod token_hash {
        use super::*;

        #[test]
        fn matches_known_vector_when_sha256_of_abc() {
            assert_eq!(
                TokenHash::of("abc").to_string(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
        }

        #[test]
        fn round_trips_when_hex_encoded() {
            let hash = TokenHash::of("x");
            assert_eq!(TokenHash::from_hex(&hash.to_string()).unwrap(), hash);
        }

        #[rstest]
        #[case::short("abc")]
        #[case::upper(&"A".repeat(64))]
        #[case::non_hex(&"g".repeat(64))]
        fn from_hex_rejects_when_not_lower_hex(#[case] raw: &str) {
            assert!(TokenHash::from_hex(raw).is_err(), "{raw:?} must not parse");
        }
    }
}
