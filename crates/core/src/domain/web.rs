//! Addresses of the web a browser is sent to: the origin of the panel
//! `liftoff-web` and the absolute URLs of its pages (the `web` row).
//!
//! Both come from modules, which are untrusted, or are built by
//! the core; either way they reach `Access-Control-Allow-Origin` and
//! `Location`, so they are parsed here once and trusted afterwards.

use super::names::string_newtype;
use super::refs::Host;
use crate::error::{Problem, ValidationError};

/// An origin of a web page, as browsers send it in `Origin`.
///
/// `https://<host>[:<port>]` in lower case, without a path, query,
/// fragment or user info. Plain `http://` is accepted only on
/// `localhost` and `127.0.0.1` (a panel in development): a cleartext
/// origin elsewhere would be given credentialed CORS and the
/// `SameSite=None` cookie, which a network attacker could then read
/// through script injected into it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin(String);

impl Origin {
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "https://<host>[:<port>] without a path, query, fragment or user info (http:// only on localhost or 127.0.0.1)";

    /// Parses an origin; the scheme and host may come in any case and
    /// are kept in lower case.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `origin` when `raw` is empty or not
    /// of [`Origin::PATTERN`].
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        if raw.is_empty() {
            return Err(ValidationError::new("origin", Problem::Empty));
        }
        let lower = raw.to_ascii_lowercase();
        let (secure, authority) = lower.strip_prefix("https://").map_or_else(
            || (false, lower.strip_prefix("http://").unwrap_or_default()),
            |rest| (true, rest),
        );
        let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
        let port_ok = authority == host
            || (port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port > 0));
        let host_ok = if secure {
            Host::new(host).is_ok()
        } else {
            matches!(host, "localhost" | "127.0.0.1")
        };
        if host_ok && port_ok {
            Ok(Self(lower))
        } else {
            Err(ValidationError::new(
                "origin",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            ))
        }
    }
}

string_newtype!(Origin);

/// An absolute URL a browser is sent to: an [`Origin`] followed by
/// nothing, or by a path, query or fragment of visible ASCII (no
/// whitespace or control characters, no relative or `javascript:`
/// target).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WebUrl(String);

impl WebUrl {
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "<origin>[/<path>][?<query>][#<fragment>] of visible ASCII, the origin https://<host>[:<port>] (http:// only on localhost or 127.0.0.1)";

    /// Parses an absolute URL; its origin is kept in lower case.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `url` when `raw` is empty or not of
    /// [`WebUrl::PATTERN`].
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        if raw.is_empty() {
            return Err(ValidationError::new("url", Problem::Empty));
        }
        let fail = || {
            ValidationError::new(
                "url",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            )
        };
        if !raw.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(fail());
        }
        let (origin, rest) = raw.split_at(origin_end(raw));
        let origin = Origin::new(origin).map_err(|_invalid| fail())?;
        Ok(Self(format!("{origin}{rest}")))
    }

    /// The URL's origin.
    #[must_use]
    pub fn origin(&self) -> Origin {
        let (origin, _rest) = self.0.split_at(origin_end(&self.0));
        Origin(origin.to_owned())
    }
}

string_newtype!(WebUrl);

/// Where the origin of `url` ends: at the first `/`, `?` or `#` after
/// its `scheme://`, else at its end.
fn origin_end(url: &str) -> usize {
    let start = url.find("://").map_or(0, |at| at.saturating_add(3));
    url.get(start..)
        .and_then(|rest| rest.find(['/', '?', '#']))
        .map_or(url.len(), |at| start.saturating_add(at))
}

#[cfg(test)]
mod tests {
    use super::*;

    mod origin {
        use super::*;

        // TC: WEB-01
        #[test]
        fn keeps_lower_case_when_origin_comes_in_any_case() {
            assert_eq!(
                Origin::new("HTTPS://Panel.Test:8443").map(|origin| origin.to_string()),
                Ok("https://panel.test:8443".to_owned())
            );
        }

        // TC: WEB-01 (no path, query, fragment, user info or bad port)
        #[test]
        fn refuses_when_origin_is_more_than_scheme_and_authority() {
            let accepted: Vec<&str> = [
                "https://panel.test/",
                "https://panel.test/path",
                "https://panel.test?x",
                "https://panel.test#x",
                "https://a@panel.test",
                "https://panel.test:0",
                "https://panel.test:http",
                "https://panel.test:+80",
                "https://",
                "null",
                "panel.test",
                "javascript:alert(1)",
                "",
            ]
            .into_iter()
            .filter(|raw| Origin::new(raw).is_ok())
            .collect();
            assert_eq!(accepted, Vec::<&str>::new());
        }

        // TC: WEB-01 (no cleartext origin but loopback, for development)
        #[test]
        fn refuses_http_when_host_is_not_loopback() {
            assert_eq!(
                (
                    Origin::new("http://panel.test").is_ok(),
                    Origin::new("http://localhost:5173").is_ok(),
                    Origin::new("http://127.0.0.1:8080").is_ok(),
                ),
                (false, true, true)
            );
        }
    }

    mod web_url {
        use super::*;

        // TC: WEB-03
        #[test]
        fn keeps_path_and_lowers_origin_when_url_is_absolute() {
            let url =
                WebUrl::new("HTTPS://Panel.Test/login?return=https%3A%2F%2Fa.test%2F").unwrap();
            assert_eq!(
                (url.as_str(), url.origin().as_str()),
                (
                    "https://panel.test/login?return=https%3A%2F%2Fa.test%2F",
                    "https://panel.test"
                )
            );
        }

        // TC: WEB-03 (no relative, script, cleartext or broken target)
        #[test]
        fn refuses_when_url_is_not_absolute_https() {
            let accepted: Vec<&str> = [
                "/login",
                "//evil.test/login",
                "javascript:alert(1)",
                "http://evil.test/login",
                "https://panel.test/a b",
                "https://panel.test/\r\nset-cookie:x",
                "https://a@panel.test/",
                "https://panel.tést/",
                "",
            ]
            .into_iter()
            .filter(|raw| WebUrl::new(raw).is_ok())
            .collect();
            assert_eq!(accepted, Vec::<&str>::new());
        }

        // TC: WEB-03
        #[test]
        fn origin_is_whole_url_when_url_has_no_path() {
            assert_eq!(
                WebUrl::new("https://panel.test?x=1").map(|url| url.origin().to_string()),
                Ok("https://panel.test".to_owned())
            );
        }
    }
}
