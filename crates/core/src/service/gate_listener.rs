//! [`GateListener`]: the gate in front of protected environments.
//!
//! The v1 SSO gate with the session handover by one-time code, over the
//! core's browser sessions and the `gate` hook.
//!
//! Caddy asks it with `forward_auth` for every request to a domain bound
//! to a protected environment ([`Environment::protected`](crate::domain::Environment::protected)).
//! The core owns the sessions (modules never see a cookie); who
//! may pass is the `gate` hook's decision (the `tenants` module: members
//! of the environment's tenant and the admin).
//!
//! The HTTP adapter (`crates/rest`, `routes/gate.rs`) reads the cookies,
//! headers and query, hands them over as domain types and renders the
//! [`GateAnswer`]; the rules are here:
//!
//! - [`GateListener::check`] (`GET /gate/check` on `[gate] bind`, the host
//!   from `X-Liftoff-Host`): no live session on this host →
//!   [`GateAnswer::Start`]; the hook denies → [`GateAnswer::Refuse`];
//!   allowed → [`GateAnswer::Pass`];
//! - [`GateListener::redeem`] (`/.liftoff/callback?code=`): a one-time
//!   code issued for this host (10 minutes, one use) whose browser
//!   session is alive → [`GateAnswer::Enter`] with the host session; any
//!   other code → [`GateAnswer::Stale`];
//! - [`GateListener::logout`] (`/.liftoff/logout`): ends the host session
//!   with the browser session it came from;
//! - [`GateListener::start`] (`GET /gate/start?host&rd` on the API): a
//!   host that is neither a domain of a protected environment, a
//!   private address of a function or tunnel
//!   nor `[artifacts] host` (there the browser session or the login, then a code, whoever it is) →
//!   [`Error::NotFound`] (no open redirect; `rd` is a
//!   [`ReturnPath`], which the adapter parses, `/` for anything that is
//!   not a path); no live browser session → [`GateAnswer::Login`] for
//!   a protected environment (it exists only with `tenants`, which
//!   requires `oidc`) or when a module replaces the login page
//!   (`render_page(login)`, the `oidc` module), else
//!   [`GateAnswer::LoginPage`], the daemon's page where a one-time link
//!   is pasted; the admin, the environment's owner and
//!   whom the hook lets through may go on, anybody else →
//!   [`GateAnswer::Refuse`] naming the login ("signed in as …");
//!   allowed → [`GateAnswer::Callback`] with a code (the `rd` travels in
//!   the code, not in the URL);
//! - `handover_code`: that code, which a one-time link opens too
//!   ([`LinksService::open`](super::LinksService::open));
//! - [`GateListener::page`]: the page a module renders in place of the
//!   daemon's; [`GateListener::login_page`] the login's, which names
//!   the host and the return address only for a host served here;
//! - [`GateListener::prune`]: deletes expired codes and host sessions (v1
//!   SSO-13; the daemon calls it at start and every hour).

use std::sync::Arc;

use time::Duration;

use super::settings::Settings;
use super::support::{gate_refusal, private_refusal};
use crate::domain::{
    EnvKind, Environment, GateCode, GateCodeSecret, GateSecret, GateSession, Host, Login,
    ReturnPath, Session, SessionKind, SessionSecret, User, Visibility,
};
use crate::error::Error;
use crate::ports::{
    Clock as _, HookRequest, Hooks as _, ModulePage, PageKind, PageRequest, Ports, Rng as _,
    Store as _,
};

/// How long a one-time code can be redeemed (one-time links live ten minutes).
const CODE_TTL: Duration = Duration::minutes(10);

/// What the gate decided; the HTTP adapter renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateAnswer {
    /// No live session on `host`: the browser goes to `/gate/start` on
    /// the API for `host`, to come back to `rd`.
    Start {
        /// The protected host.
        host: Host,
        /// Where to go back to on `host`.
        rd: ReturnPath,
    },
    /// No live browser session at `/gate/start`: the browser login of
    /// the module that replaces the login page, which comes back to
    /// `/gate/start` for `host` and `rd`.
    Login {
        /// The protected host.
        host: Host,
        /// Where to go back to on `host`.
        rd: ReturnPath,
    },
    /// No live browser session at `/gate/start` and no module that
    /// replaces the login page: the daemon's login page for `host` and
    /// `rd`, where a one-time link is pasted.
    LoginPage {
        /// The private host.
        host: Host,
        /// Where to go back to on `host`.
        rd: ReturnPath,
    },
    /// `/gate/start` let the browser through: to `host`'s
    /// `/.liftoff/callback` with `code`.
    Callback {
        /// The protected host.
        host: Host,
        /// The one-time code for it.
        code: GateCodeSecret,
    },
    /// A code redeemed: the host session `secret` as the host's cookie
    /// for `max_age` seconds, then `rd`.
    Enter {
        /// The secret of the new host session.
        secret: GateSecret,
        /// Where to go on the host.
        rd: ReturnPath,
        /// Seconds the cookie lives: until the browser session ends.
        max_age: i64,
    },
    /// The request may pass: the user the application sees.
    Pass {
        /// The user's login.
        login: Login,
        /// The user's address; empty when unknown.
        email: String,
    },
    /// The request may not pass; the reason is safe to show.
    Refuse(String),
    /// The host session ended (or there was none): the cookie is cleared.
    SignedOut,
    /// A code that is not one of this host, expired, used or of a session
    /// that ended.
    Stale,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct GateListener<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for GateListener<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> GateListener<P> {
    /// The gate over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// The API host, where `/gate/start` and the login are.
    #[must_use]
    pub fn api_host(&self) -> &Host {
        &self.settings.proxy.api_host
    }

    /// `/gate/check` for `host`: the host session of `gate`, then the
    /// hook, which sees `request` without its credentials. `back` is
    /// where the browser returns to after `/gate/start`: the original
    /// path and query (`X-Forwarded-Uri`).
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn check(
        &self,
        host: &Host,
        gate: Option<&GateSecret>,
        back: ReturnPath,
        request: &HookRequest,
    ) -> Result<GateAnswer, Error> {
        let Some(gate) = self.host_session(host, gate).await? else {
            return Ok(GateAnswer::Start {
                host: host.clone(),
                rd: back,
            });
        };
        let Some(user) = self.ports.store().user(gate.user_id).await? else {
            return Ok(GateAnswer::Refuse("forbidden".to_owned()));
        };
        if let Some(refused) = self.refused(host, &user, request).await {
            return Ok(refused);
        }
        Ok(GateAnswer::Pass {
            login: user.login,
            email: user.email,
        })
    }

    /// The live session on `host` of `secret`.
    async fn host_session(
        &self,
        host: &Host,
        secret: Option<&GateSecret>,
    ) -> Result<Option<GateSession>, Error> {
        let Some(secret) = secret else {
            return Ok(None);
        };
        let now = self.ports.clock().now();
        Ok(self
            .ports
            .store()
            .gate_session(&secret.hash())
            .await?
            .filter(|gate| &gate.host == host && gate.expires_at > now))
    }

    /// The refusal when the `gate` hook does not let `user` through to
    /// `host` ([`gate_refusal`]); `None` when it lets them through.
    async fn refused(&self, host: &Host, user: &User, request: &HookRequest) -> Option<GateAnswer> {
        gate_refusal(self.ports.as_ref(), host, user, request)
            .await
            .map(GateAnswer::Refuse)
    }

    /// `/.liftoff/callback?code=` on `host`: the code of this host
    /// becomes its session.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn redeem(
        &self,
        host: &Host,
        code: Option<&GateCodeSecret>,
    ) -> Result<GateAnswer, Error> {
        let Some(code) = code else {
            return Ok(GateAnswer::Stale);
        };
        let store = self.ports.store();
        let now = self.ports.clock().now();
        let Some(code) = store
            .take_gate_code(&code.hash())
            .await?
            .filter(|code| &code.host == host && code.expires_at > now)
        else {
            return Ok(GateAnswer::Stale);
        };
        let Some(session) = store
            .sessions_of_user(code.user_id)
            .await?
            .into_iter()
            .find(|session| session.id == code.session_id && session.expires_at > now)
        else {
            return Ok(GateAnswer::Stale);
        };
        let secret = GateSecret::from_body(&self.ports.rng().token_secret());
        store
            .insert_gate_session(&GateSession {
                token_hash: secret.hash(),
                session_id: session.id,
                user_id: session.user_id,
                host: host.clone(),
                expires_at: session.expires_at,
                created_at: now,
            })
            .await?;
        let max_age = session
            .expires_at
            .unix_timestamp()
            .saturating_sub(now.unix_timestamp())
            .max(0);
        Ok(GateAnswer::Enter {
            secret,
            rd: code.rd,
            max_age,
        })
    }

    /// `/.liftoff/logout` on `host`: a host session of this host ends
    /// with the browser session it came from (and every other host
    /// session of it, v1 SSO-11). Anything else counts as signed out
    /// already.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn logout(
        &self,
        host: &Host,
        gate: Option<&GateSecret>,
    ) -> Result<GateAnswer, Error> {
        if let Some(gate) = self.host_session(host, gate).await? {
            match self.ports.store().delete_session(gate.session_id).await {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(GateAnswer::SignedOut)
    }

    /// `/gate/start?host&rd` on the API with the browser session
    /// `session`: no open redirect, the browser session or the login, who
    /// may open the host (the admin, the environment's owner, whom the
    /// hook lets through; the hook sees `request` without its
    /// credentials), then a code for the host. `[artifacts] host` gets
    /// the code without the question (`start_artifacts`).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when `host` is missing or neither a domain of
    /// a protected environment, a private address of a function or
    /// tunnel nor `[artifacts] host`; whatever the store reports.
    pub async fn start(
        &self,
        host: Option<&Host>,
        rd: ReturnPath,
        session: Option<&SessionSecret>,
        request: &HookRequest,
    ) -> Result<GateAnswer, Error> {
        let not_found = || Error::not_found("no protected or private host of that name");
        let host = host.ok_or_else(not_found)?;
        if self.settings.artifacts.host.as_ref() == Some(host) {
            return self.start_artifacts(host, rd, session).await;
        }
        let environment = self.gated(host).await?.ok_or_else(not_found)?;
        let Some(session) = self.browser_session(session).await? else {
            // A protected environment exists only with `tenants`, which
            // requires `oidc` (TEN-01): its login is the module's, unless
            // the panel takes the login page over.
            let module_login = self
                .module_login(host, &rd)
                .await
                .unwrap_or(environment.protected);
            let host = host.clone();
            return Ok(if module_login {
                GateAnswer::Login { host, rd }
            } else {
                GateAnswer::LoginPage { host, rd }
            });
        };
        let Some(user) = self.ports.store().user(session.user_id).await? else {
            return Ok(GateAnswer::Refuse("forbidden".to_owned()));
        };
        // A protected host keeps the gate's rule, which `/gate/check`
        // applies too: the hook decides first, even for the admin. A
        // private address that is not protected follows its listener's
        // ([`Access::check`](super::Access::check)).
        let refused = if environment.protected {
            gate_refusal(self.ports.as_ref(), host, &user, request).await
        } else {
            private_refusal(self.ports.as_ref(), &environment, host, &user, request).await
        };
        if let Some(reason) = refused {
            return Ok(GateAnswer::Refuse(reason));
        }
        let code = handover_code(self.ports.as_ref(), &session, host, rd).await?;
        Ok(GateAnswer::Callback {
            host: host.clone(),
            code,
        })
    }

    /// [`GateListener::start`] for `[artifacts] host`: the API host's `browser` session or the login, then a
    /// code for that host. Who may open which artifact is not asked
    /// here: [`Access::check`](super::Access::check) decides on every
    /// request there, with the host session the code becomes.
    async fn start_artifacts(
        &self,
        host: &Host,
        rd: ReturnPath,
        session: Option<&SessionSecret>,
    ) -> Result<GateAnswer, Error> {
        let Some(session) = self.browser_session(session).await? else {
            let module_login = self.module_login(host, &rd).await.unwrap_or(false);
            let host = host.clone();
            return Ok(if module_login {
                GateAnswer::Login { host, rd }
            } else {
                GateAnswer::LoginPage { host, rd }
            });
        };
        let code = handover_code(self.ports.as_ref(), &session, host, rd).await?;
        Ok(GateAnswer::Callback {
            host: host.clone(),
            code,
        })
    }

    /// The page a module renders in place of the daemon's
    /// (`render_page`); `None` keeps the daemon's own.
    pub async fn page(&self, page: &PageRequest) -> Option<ModulePage> {
        self.ports.hooks().render_page(page).await
    }

    /// The login page of the API host (`/login?host&rd`) as a module
    /// renders it in place of the daemon's. It knows `host` and the
    /// address to return to only when the daemon serves `host` behind a
    /// session ([`GateListener::start`]'s hosts): any other host is
    /// asked without either, so the panel never gets a return address
    /// the daemon does not serve (no open redirect through it).
    pub async fn login_page(&self, host: Option<&Host>, rd: &ReturnPath) -> Option<ModulePage> {
        let served = match host {
            Some(host) if self.settings.artifacts.host.as_ref() == Some(host) => Some(host),
            Some(host) => match self.gated(host).await {
                Ok(Some(_environment)) => Some(host),
                Ok(None) => None,
                Err(error) => {
                    tracing::error!(%host, %error, "the login page could not look its host up");
                    None
                }
            },
            None => None,
        };
        let page = served.map_or_else(
            || PageRequest::of(PageKind::Login),
            |host| PageRequest::at(PageKind::Login, host, rd.as_str()),
        );
        self.page(&page).await
    }

    /// Whether the login of `host` is a module's own page (`oidc`'s
    /// HTML, which starts its provider's login): `Some(true)`. A module
    /// that sends the browser elsewhere (the panel `liftoff-web`) is not, `Some(false)`: the browser goes to the
    /// API host's `/login`, which sends it on with where it was going.
    /// `None` when no module renders the login.
    async fn module_login(&self, host: &Host, rd: &ReturnPath) -> Option<bool> {
        match self
            .page(&PageRequest::at(PageKind::Login, host, rd.as_str()))
            .await?
        {
            ModulePage::Html(_) => Some(true),
            ModulePage::Redirect(_) => Some(false),
        }
    }

    /// Deletes the codes and host sessions that expired (v1 SSO-13); how
    /// many of each.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn prune(&self) -> Result<(usize, usize), Error> {
        let now = self.ports.clock().now();
        self.ports.store().prune_gate_records(now).await
    }

    /// The environment `host` is a domain of, when the browser needs a
    /// session there: a protected environment (the gate), or a private
    /// function or tunnel (their listeners).
    async fn gated(&self, host: &Host) -> Result<Option<Environment>, Error> {
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
            .filter(|environment| {
                environment.protected
                    || (matches!(environment.kind, EnvKind::Function | EnvKind::Tunnel)
                        && environment.visibility == Visibility::Private)
            }))
    }

    /// The live `browser` session of `secret`.
    async fn browser_session(
        &self,
        secret: Option<&SessionSecret>,
    ) -> Result<Option<Session>, Error> {
        let Some(secret) = secret else {
            return Ok(None);
        };
        let now = self.ports.clock().now();
        Ok(self
            .ports
            .store()
            .session_by_hash(&secret.hash())
            .await?
            .filter(|session| session.kind == SessionKind::Browser && session.expires_at > now))
    }
}

/// A one-time code that hands the `browser` session `session` over to
/// `host` and returns to `rd` there: ten minutes, one use, redeemed by
/// [`GateListener::redeem`] on that host (v1 SSO). `/gate/start` and a
/// one-time link ([`LinksService::open`](super::LinksService::open))
/// issue it alike.
///
/// # Errors
///
/// Whatever the store reports.
pub(super) async fn handover_code<P: Ports>(
    ports: &P,
    session: &Session,
    host: &Host,
    rd: ReturnPath,
) -> Result<GateCodeSecret, Error> {
    let code = GateCodeSecret::from_body(&ports.rng().token_secret());
    ports
        .store()
        .insert_gate_code(&GateCode {
            code_hash: code.hash(),
            session_id: session.id,
            user_id: session.user_id,
            host: host.clone(),
            rd,
            expires_at: ports.clock().now().saturating_add(CODE_TTL),
        })
        .await?;
    Ok(code)
}
