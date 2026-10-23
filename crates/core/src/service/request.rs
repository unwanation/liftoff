//! What every HTTP adapter reads of a request the same way.
//!
//! The REST API, the gate listener, the function and tunnel listeners
//! read the session cookie of a host, a liftoff bearer, and the request
//! as a module's hook sees it. The adapters only hand over the raw
//! header values; the rules are here, once (modules are untrusted).

use crate::domain::{Bearer, GateSecret, Host};
use crate::ports::HookRequest;

use super::listener::is_liftoff_authorization;
use super::visibility::AccessRequest;

/// The cookie of the session on one host (`__Host-liftoff_gate`): the
/// gate sets it after the session handover, [`Access::check`] reads it.
///
/// [`Access::check`]: super::Access::check
pub const HOST_SESSION_COOKIE: &str = "__Host-liftoff_gate";

/// The `Set-Cookie` value of the host session `secret` for `max_age`
/// seconds.
///
/// `__Host-` (`Path=/`, no `Domain`), `Secure`, `HttpOnly`,
/// `SameSite=Lax`. The gate listener and the function and
/// tunnel listeners set it alike after the session handover.
#[must_use]
pub fn host_session_cookie(secret: &GateSecret, max_age: i64) -> String {
    format!(
        "{HOST_SESSION_COOKIE}={}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age}",
        secret.expose()
    )
}

/// The `Set-Cookie` value that clears the host session's cookie
/// (`/.liftoff/logout`).
#[must_use]
pub fn host_session_cleared() -> String {
    format!("{HOST_SESSION_COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Whether the header `name: value` is kept from a module's hook.
///
/// Kept are a cookie, `Proxy-Authorization`, and an `Authorization` that is not
/// `<scheme> <token>` or carries a liftoff token (any of its prefixes,
/// valid or not, [`is_liftoff_authorization`]). A foreign bearer passes,
/// since an `oidc` module needs it. Case of `name` does not matter.
#[must_use]
pub fn hidden_from_hooks(name: &str, value: &str) -> bool {
    if name.eq_ignore_ascii_case("cookie") || name.eq_ignore_ascii_case("proxy-authorization") {
        return true;
    }
    name.eq_ignore_ascii_case("authorization")
        && (!value.contains(' ') || is_liftoff_authorization(value))
}

/// The request as a hook sees it: `method`, `path` (with its query, as
/// the client sent it) and every header of `headers` that is not
/// [hidden](hidden_from_hooks), names in lower case.
#[must_use]
pub fn hook_request<'a>(
    method: &str,
    path: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> HookRequest {
    HookRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        headers: headers
            .into_iter()
            .filter(|(name, value)| !hidden_from_hooks(name, value))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
            .collect(),
    }
}

/// The liftoff token of the `Authorization` value `authorization` when
/// it is `Bearer <token>` (scheme in any case) and the token has a
/// liftoff shape; anything else is no bearer.
#[must_use]
pub fn bearer_of(authorization: Option<&str>) -> Option<Bearer> {
    let (scheme, token) = authorization?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Bearer::parse(token).ok()
}

/// The value of the cookie `name` in the `Cookie` header values
/// `values` (the first one of that name); spacing around pairs, names
/// and values is trimmed.
#[must_use]
pub fn cookie_value<'a>(values: impl IntoIterator<Item = &'a str>, name: &str) -> Option<&'a str> {
    values
        .into_iter()
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim())
}

impl AccessRequest {
    /// What a listener read of a request for `host`: its `Cookie` header
    /// values `cookies` (the [host session](HOST_SESSION_COOKIE) is taken
    /// when it has its shape), its `Authorization` (a [liftoff
    /// bearer](bearer_of)), its `Accept`, and the request as the `gate`
    /// hook sees it.
    #[must_use]
    pub fn read<'a>(
        host: Host,
        cookies: impl IntoIterator<Item = &'a str>,
        authorization: Option<&str>,
        accept: Option<&str>,
        request: HookRequest,
    ) -> Self {
        Self {
            host,
            cookie: cookie_value(cookies, HOST_SESSION_COOKIE)
                .and_then(|raw| GateSecret::parse(raw).ok()),
            session: None,
            bearer: bearer_of(authorization),
            accept: accept.map(ToOwned::to_owned),
            request,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod hidden_from_hooks {
        use super::*;

        #[test]
        fn hidden_when_header_is_cookie_in_any_case() {
            assert!(hidden_from_hooks("Cookie", "a=1"));
        }

        #[test]
        fn hidden_when_header_is_proxy_authorization() {
            assert!(hidden_from_hooks("proxy-authorization", "Basic abc"));
        }

        #[test]
        fn hidden_when_bearer_is_session() {
            assert!(hidden_from_hooks("authorization", "Bearer lfs_abc"));
        }

        #[test]
        fn hidden_when_bearer_is_host_session() {
            assert!(hidden_from_hooks("authorization", "Bearer lfg_abc"));
        }

        #[test]
        fn hidden_when_authorization_has_no_scheme() {
            assert!(hidden_from_hooks("authorization", "app-token"));
        }

        #[test]
        fn shown_when_bearer_is_foreign() {
            assert!(!hidden_from_hooks("authorization", "Bearer eyJhbGciOi"));
        }

        #[test]
        fn shown_when_header_is_applications() {
            assert!(!hidden_from_hooks("x-custom", "lfs_abc"));
        }
    }

    mod bearer_of {
        use super::*;

        #[test]
        fn none_when_scheme_is_basic() {
            assert_eq!(bearer_of(Some("Basic abc")), None);
        }

        #[test]
        fn none_when_token_is_foreign() {
            assert_eq!(bearer_of(Some("Bearer app-token")), None);
        }

        #[test]
        fn none_when_token_is_empty() {
            assert_eq!(bearer_of(Some("Bearer ")), None);
        }
    }

    mod cookie_value {
        use super::*;

        #[test]
        fn value_when_cookie_is_among_others() {
            assert_eq!(
                cookie_value(
                    ["a=1", " __Host-liftoff_gate = lfg_x ; b=2"],
                    HOST_SESSION_COOKIE
                ),
                Some("lfg_x")
            );
        }

        #[test]
        fn none_when_cookie_is_absent() {
            assert_eq!(cookie_value(["a=1; b=2"], HOST_SESSION_COOKIE), None);
        }
    }
}
