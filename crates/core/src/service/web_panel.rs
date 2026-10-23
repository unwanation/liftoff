//! [`WebPanelService`]: what the panel `liftoff-web` changes for browsers
//! (the `web` row).
//!
//! The panel is a project of its own on another origin that talks to the
//! REST API. While the module named [`WEB_MODULE`] keeps a panel
//! ([`Hooks::web_panel`](crate::ports::Hooks::web_panel)):
//!
//! - `/api/v1` answers CORS with credentials for the panel's origin
//!   alone ([`WebPanelService::origin`]: compared whole, scheme and host
//!   in any case, never `null`); any other origin gets no CORS header;
//! - the browser session's cookie on the API host is `SameSite=None`
//!   ([`WebPanelService::same_site`]): the panel's requests are
//!   cross-site, and a `Lax` cookie would not go with them;
//! - a request from the panel's origin may authenticate with that cookie
//!   (the HTTP adapter hands it over as the session's bearer); one from
//!   any other origin may not, so the cookie that now goes with every
//!   cross-site request opens nothing for another site.
//!
//! The question is asked on each request, so installing, reconfiguring
//! or removing the module takes effect without a restart. A module that
//! fails to answer keeps no panel (fail closed: no CORS, `Lax`).

use std::sync::Arc;

use crate::domain::{Origin, SessionSecret};
use crate::ports::{Hooks as _, Ports, WebPanel};

/// The module that keeps the panel: [`Hooks::web_panel`](crate::ports::Hooks::web_panel)
/// is asked of it alone.
pub const WEB_MODULE: &str = "web";

/// The `SameSite` attribute of the browser session's cookie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// Without a panel.
    Lax,
    /// While the panel `liftoff-web` is kept (it needs `Secure`).
    None,
}

impl SameSite {
    /// The attribute's value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}

/// What a request's `Origin` is to the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelOrigin {
    /// No module keeps a panel (or it failed to answer).
    NoPanel,
    /// The panel's origin: CORS with credentials, the cookie
    /// authenticates.
    Panel(WebPanel),
    /// Another origin while there is a panel: no CORS, but the answer
    /// varies on `Origin`.
    Other,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct WebPanelService<P> {
    ports: Arc<P>,
}

impl<P> Clone for WebPanelService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
        }
    }
}

impl<P: Ports> WebPanelService<P> {
    /// The service over `ports`.
    #[must_use]
    pub const fn new(ports: Arc<P>) -> Self {
        Self { ports }
    }

    /// The panel kept now, if any; a module that fails to answer keeps
    /// none (the dispatcher logs why).
    pub async fn panel(&self) -> Option<WebPanel> {
        self.ports.hooks().web_panel().await.ok().flatten()
    }

    /// What the request's `origin` (the `Origin` header's value) is to
    /// the panel: the panel's when it parses as the same [`Origin`]
    /// (scheme and host in any case); `null`, an empty value or any
    /// other origin is not.
    pub async fn origin(&self, origin: &str) -> PanelOrigin {
        let Some(panel) = self.panel().await else {
            return PanelOrigin::NoPanel;
        };
        if Origin::new(origin.trim()).is_ok_and(|origin| origin == panel.origin) {
            PanelOrigin::Panel(panel)
        } else {
            PanelOrigin::Other
        }
    }

    /// The `SameSite` of the browser session's cookie now.
    pub async fn same_site(&self) -> SameSite {
        if self.panel().await.is_some() {
            SameSite::None
        } else {
            SameSite::Lax
        }
    }
}

/// The `Set-Cookie` value of the browser session `token` on the API host
/// (`__Host-liftoff_session`) for `max_age` seconds: `Path=/`, `Secure`,
/// `HttpOnly`, `SameSite=<same_site>`.
#[must_use]
pub fn browser_session_cookie(token: &SessionSecret, max_age: i64, same_site: SameSite) -> String {
    format!(
        "{}={}; Path=/; Secure; HttpOnly; SameSite={}; Max-Age={max_age}",
        crate::ports::ModuleRequest::SESSION_COOKIE,
        token.expose(),
        same_site.as_str()
    )
}

/// `cookie` (a `Set-Cookie` value) with its `SameSite` attribute set to
/// `same_site`, added when it has none: how the browser login module's
/// session cookie follows the panel too.
#[must_use]
pub fn with_same_site(cookie: &str, same_site: SameSite) -> String {
    let wanted = format!("SameSite={}", same_site.as_str());
    let mut found = false;
    let mut parts: Vec<String> = cookie
        .split(';')
        .map(|part| {
            let trimmed = part.trim();
            let is_same_site = trimmed
                .split_once('=')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("samesite"));
            if is_same_site {
                found = true;
                wanted.clone()
            } else {
                trimmed.to_owned()
            }
        })
        .collect();
    if !found {
        parts.push(wanted);
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::fakes::World;

    const PANEL: &str = "https://panel.test";

    fn panel() -> WebPanel {
        WebPanel {
            origin: Origin::new(PANEL).unwrap(),
        }
    }

    async fn service(
        answer: Result<Option<WebPanel>, Error>,
    ) -> WebPanelService<crate::fakes::FakePorts> {
        let world = World::new().await;
        world.ports.hooks.keep_panel(answer);
        WebPanelService::new(Arc::clone(&world.ports))
    }

    mod origin {
        use super::*;

        // TC: WEB-01
        #[tokio::test]
        async fn panel_when_origin_matches_in_any_case() {
            let web = service(Ok(Some(panel()))).await;
            assert_eq!(
                (
                    web.origin(PANEL).await,
                    web.origin("HTTPS://Panel.Test").await
                ),
                (PanelOrigin::Panel(panel()), PanelOrigin::Panel(panel()))
            );
        }

        // TC: WEB-01
        #[tokio::test]
        async fn other_when_origin_is_null_or_extends_panel() {
            let web = service(Ok(Some(panel()))).await;
            assert_eq!(
                (
                    web.origin("null").await,
                    web.origin("https://panel.test.evil.test").await,
                    web.origin("https://panel.test:8443").await,
                ),
                (PanelOrigin::Other, PanelOrigin::Other, PanelOrigin::Other)
            );
        }

        // TC: WEB-01 (fail closed)
        #[tokio::test]
        async fn no_panel_when_module_fails() {
            let web = service(Err(Error::unavailable("module web failed"))).await;
            assert_eq!(web.origin(PANEL).await, PanelOrigin::NoPanel);
        }
    }

    mod same_site {
        use super::*;

        // TC: WEB-04
        #[tokio::test]
        async fn none_when_panel_kept() {
            assert_eq!(
                service(Ok(Some(panel()))).await.same_site().await,
                SameSite::None
            );
        }

        // TC: WEB-04, WEB-05
        #[tokio::test]
        async fn lax_when_no_panel_or_module_fails() {
            assert_eq!(
                (
                    service(Ok(None)).await.same_site().await,
                    service(Err(Error::unavailable("down")))
                        .await
                        .same_site()
                        .await,
                ),
                (SameSite::Lax, SameSite::Lax)
            );
        }
    }

    mod with_same_site {
        use super::*;

        // TC: WEB-04 (the login module's cookie follows the panel)
        #[test]
        fn replaces_attribute_when_cookie_has_one() {
            assert_eq!(
                with_same_site(
                    "__Host-liftoff_session=lfs_x; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=60",
                    SameSite::None
                ),
                "__Host-liftoff_session=lfs_x; Path=/; Secure; HttpOnly; SameSite=None; Max-Age=60"
            );
        }

        #[test]
        fn adds_attribute_when_cookie_has_none() {
            assert_eq!(
                with_same_site("a=b; Path=/", SameSite::None),
                "a=b; Path=/; SameSite=None"
            );
        }
    }
}
