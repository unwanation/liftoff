//! The records of the gate in front of protected environments (the v1 SSO spec, section 6.3): the one-time code
//! that hands a browser session over to one protected host, and the
//! session on that host.

use super::names::string_newtype;
use super::{Host, SessionId, Timestamp, TokenHash, UserId};
use crate::error::{Problem, ValidationError};

/// Where the gate sends a browser back to on a protected host (`rd`, the
/// `X-Forwarded-Uri` of the request it stopped): a path on that same
/// host, never another site (no open redirect, v1 SSO-04).
///
/// The HTTP adapter parses it from the query or the header and uses
/// [`ReturnPath::root`] for anything that is not one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReturnPath(String);

impl ReturnPath {
    /// The longest path kept, in bytes .
    pub const MAX_LEN: usize = 2048;
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str =
        "a path on the same host: '/' first, not '//' or '/\\', no control character";

    /// Validates a path to return to.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `rd` when `raw` is empty, longer
    /// than [`ReturnPath::MAX_LEN`] bytes, does not start with `/`,
    /// starts with `//` or `/\` (another host) or holds a control
    /// character.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        if raw.is_empty() {
            return Err(ValidationError::new("rd", Problem::Empty));
        }
        if raw.len() > Self::MAX_LEN {
            return Err(ValidationError::new(
                "rd",
                Problem::TooLong {
                    max: Self::MAX_LEN,
                    actual: raw.len(),
                },
            ));
        }
        let path = raw.starts_with('/')
            && !raw.starts_with("//")
            && !raw.starts_with("/\\")
            && !raw.chars().any(char::is_control);
        if path {
            Ok(Self(raw.to_owned()))
        } else {
            Err(ValidationError::new(
                "rd",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            ))
        }
    }

    /// `/`, the root of the host.
    #[must_use]
    pub fn root() -> Self {
        Self("/".to_owned())
    }
}

string_newtype!(ReturnPath);

/// A one-time code `/gate/start` issued: it opens a [`GateSession`] on
/// `host` for the browser session `session_id` once, within ten minutes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateCode {
    /// SHA-256 of the [`GateCodeSecret`](super::GateCodeSecret) (unique).
    pub code_hash: TokenHash,
    /// The browser session it was issued from; deleting that session
    /// deletes the code.
    pub session_id: SessionId,
    /// The user of that session.
    pub user_id: UserId,
    /// The protected host it opens.
    pub host: Host,
    /// Where the browser goes once it is redeemed: a path on `host`.
    pub rd: ReturnPath,
    /// Until when it can be redeemed.
    pub expires_at: Timestamp,
}

/// A session on one protected host: the cookie `__Host-liftoff_gate`.
///
/// It is not a [`Session`](super::Session): its secret
/// ([`GateSecret`](super::GateSecret)) reaches the application behind the
/// host with every request, so it opens that host and nothing else, never
/// the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateSession {
    /// SHA-256 of the [`GateSecret`](super::GateSecret) (unique).
    pub token_hash: TokenHash,
    /// The browser session it was handed over from; deleting that session
    /// (logout, `lof admin sessions revoke`) deletes it.
    pub session_id: SessionId,
    /// The user of that session.
    pub user_id: UserId,
    /// The one host it opens.
    pub host: Host,
    /// Its end (that of the browser session).
    pub expires_at: Timestamp,
    /// Creation time.
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod return_path {
        use super::*;

        // TC: TEN-07 (v1 SSO-04: `rd` is only a path)
        #[test]
        fn keeps_path_when_rd_is_a_path() {
            assert_eq!(
                ReturnPath::new("/admin?x=1").map(|rd| rd.to_string()),
                Ok("/admin?x=1".to_owned())
            );
        }

        // TC: TEN-07 (v1 SSO-04: another host, a scheme, nothing at all, a
        // control character or a path too long is no path)
        #[test]
        fn rejects_rd_when_it_is_not_a_path() {
            let long = format!("/{}", "a".repeat(ReturnPath::MAX_LEN));
            let rejected: Vec<bool> = [
                "//evil.example",
                "/\\evil.example",
                "https://evil.example",
                "",
                "/a\nb",
                long.as_str(),
            ]
            .iter()
            .map(|rd| ReturnPath::new(rd).is_err())
            .collect();
            assert_eq!(rejected, vec![true; 6]);
        }
    }
}
