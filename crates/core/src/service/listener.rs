//! [`ListenerRoute`] and the forwarding rules of the function and tunnel
//! listeners.
//!
//! Caddy sends every request for a domain bound to a running function
//! (tunnel) environment to the daemon's listener with `Host` unchanged.
//! The listener adapter reads the request; the rules are here:
//!
//! - [`ListenerRoute::environment_of`]: `Host` → domain → binding →
//!   environment of the listener's kind; any other host (unknown, a
//!   domain bound to nothing or to an environment of another kind) is
//!   none of the listener's, which answers `404` (FN-11);
//! - what reaches the environment's code: never the client's `X-Liftoff-*` headers
//!   ([`forwards_header`]), never a cookie of liftoff's
//!   (`__Host-liftoff*`, [`forwarded_cookies`]), never an
//!   `Authorization` that carries a liftoff token (`lfs_…` and the other
//!   prefixes, [`is_liftoff_authorization`]); everything else passes as
//!   it came (FN-13).

use std::sync::Arc;

use crate::domain::{
    DeployTokenSecret, EnvKind, Environment, GateCodeSecret, GateSecret, Host, LinkSecret,
    SessionSecret, SetupSecret,
};
use crate::error::Error;
use crate::ports::{Ports, Store as _};

/// The prefix of every cookie of liftoff's (the API's browser session,
/// its login states, a host's session); case does not matter.
pub const LIFTOFF_COOKIE_PREFIX: &str = "__Host-liftoff";

/// The prefix of liftoff's own request headers (`X-Liftoff-User`, …);
/// case does not matter.
pub const LIFTOFF_HEADER_PREFIX: &str = "x-liftoff-";

/// See the [module documentation](self).
#[derive(Debug)]
pub struct ListenerRoute<P> {
    ports: Arc<P>,
}

impl<P> Clone for ListenerRoute<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
        }
    }
}

impl<P: Ports> ListenerRoute<P> {
    /// The routing over `ports`.
    #[must_use]
    pub const fn new(ports: Arc<P>) -> Self {
        Self { ports }
    }

    /// The environment of `kind` that `host` is an address of: the
    /// environment its domain is bound to, when that is of `kind`.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn environment_of(
        &self,
        host: &Host,
        kind: EnvKind,
    ) -> Result<Option<Environment>, Error> {
        let store = self.ports.store();
        let Some(domain) = store.domain_by_host(host).await? else {
            return Ok(None);
        };
        let Some(binding) = store.binding_by_domain(domain.id).await? else {
            return Ok(None);
        };
        Ok(store
            .environment(binding.environment_id)
            .await?
            .filter(|environment| environment.kind == kind))
    }
}

/// Whether the header `name` (as the client sent it) may reach the
/// environment's code: not one of liftoff's `X-Liftoff-*`.
///
/// `Cookie` and
/// `Authorization` pass here; [`forwarded_cookies`] and
/// [`is_liftoff_authorization`] decide their values.
#[must_use]
pub fn forwards_header(name: &str) -> bool {
    !name
        .get(..LIFTOFF_HEADER_PREFIX.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(LIFTOFF_HEADER_PREFIX))
}

/// The cookies of the `Cookie` header values `values` without liftoff's
/// (`__Host-liftoff*`), joined with `"; "` into one value.
///
/// They keep the order they came in; `None` when none is left. Pairs pass as they came (their
/// spacing trimmed); an empty pair is dropped.
#[must_use]
pub fn forwarded_cookies<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let kept: Vec<&str> = values
        .into_iter()
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .filter(|pair| !pair.is_empty() && !is_liftoff_cookie(pair))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

/// Whether the cookie pair `pair` (`name=value`) is one of liftoff's.
fn is_liftoff_cookie(pair: &str) -> bool {
    let name = pair.split_once('=').map_or(pair, |(name, _)| name).trim();
    name.get(..LIFTOFF_COOKIE_PREFIX.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(LIFTOFF_COOKIE_PREFIX))
}

/// Whether the `Authorization` value `value` carries a liftoff token.
///
/// A `cli` session, a deploy token, any other secret of liftoff's by its
/// prefix, valid or not: the environment's code never sees it. An
/// application's own credentials pass.
#[must_use]
pub fn is_liftoff_authorization(value: &str) -> bool {
    let token = value
        .split_once(' ')
        .map_or(value, |(_scheme, token)| token)
        .trim();
    [
        SessionSecret::PREFIX,
        DeployTokenSecret::PREFIX,
        SetupSecret::PREFIX,
        GateSecret::PREFIX,
        GateCodeSecret::PREFIX,
        LinkSecret::PREFIX,
    ]
    .iter()
    .any(|prefix| token.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::World;

    mod environment_of {
        use super::*;

        // TC: FN-11
        #[tokio::test]
        async fn environment_when_host_is_address_of_kind() {
            let world = World::new().await;
            let api = world.records.function(&world.shop, "api").unwrap();
            world.ports.store.insert_environment(&api).await.unwrap();
            world
                .add_address(&world.shop, &api, "api.apps.test")
                .await
                .unwrap();
            let found = ListenerRoute::new(Arc::clone(&world.ports))
                .environment_of(&Host::new("api.apps.test").unwrap(), EnvKind::Function)
                .await
                .unwrap();
            assert_eq!(found.map(|environment| environment.id), Some(api.id));
        }

        // TC: FN-11 (a compose environment's domain)
        #[tokio::test]
        async fn none_when_host_is_address_of_other_kind() {
            let world = World::new().await;
            let found = ListenerRoute::new(Arc::clone(&world.ports))
                .environment_of(&Host::new("shop.apps.test").unwrap(), EnvKind::Function)
                .await
                .unwrap();
            assert_eq!(found, None);
        }

        // TC: FN-11 (unknown host)
        #[tokio::test]
        async fn none_when_host_is_unknown() {
            let world = World::new().await;
            let found = ListenerRoute::new(Arc::clone(&world.ports))
                .environment_of(&Host::new("nobody.apps.test").unwrap(), EnvKind::Function)
                .await
                .unwrap();
            assert_eq!(found, None);
        }
    }

    mod forwarding {
        use super::*;

        // TC: FN-13
        #[test]
        fn header_passes_when_it_is_applications() {
            assert!(forwards_header("x-custom"));
        }

        // TC: FN-13
        #[test]
        fn header_stays_when_it_is_liftoffs_in_any_case() {
            assert!(!forwards_header("X-LIFTOFF-User"));
        }

        // TC: FN-13
        #[test]
        fn cookies_lose_liftoffs_when_mixed() {
            assert_eq!(
                forwarded_cookies(["a=1; __Host-liftoff_gate=x", "__host-LIFTOFF_session=y;b=2"]),
                Some("a=1; b=2".to_owned())
            );
        }

        // TC: FN-13
        #[test]
        fn no_cookie_when_only_liftoffs() {
            assert_eq!(forwarded_cookies(["__Host-liftoff_gate=x"]), None);
        }

        // TC: FN-13
        #[test]
        fn authorization_is_liftoffs_when_bearer_has_liftoff_prefix() {
            assert!(is_liftoff_authorization("Bearer lfs_abc"));
        }

        // TC: FN-13
        #[test]
        fn authorization_is_applications_when_token_is_foreign() {
            assert!(!is_liftoff_authorization("Bearer app-token-123"));
        }
    }
}
