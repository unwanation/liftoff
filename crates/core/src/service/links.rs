//! [`LinksService`]: one-time links (11).
//!
//! - [`LinksService::issue`] (`POST /auth/links {target}`, `lof dev`,
//!   `lof artifact open`): a link on the API host,
//!   `https://<api host>/api/v1/auth/links/<token>`, that lives
//!   [`LINK_TTL`] (OTL-01); the store keeps only the token's hash
//!   (OTL-05);
//! - [`LinksService::redeem`] (`GET /auth/links/{token}` and the login page): once (OTL-02), within its ten minutes (OTL-01), for
//!   its own target only (OTL-03), it opens a `browser` session of 24 hours (OTL-04)
//!   whose secret the browser keeps as the API host's cookie;
//! - [`LinksService::open`] (`GET /auth/links/{token}`, the login page):
//!   [`LinksService::redeem`], then the gate's one-time code that hands
//!   the new session over to the target's host, which cannot see the
//!   API host's cookie (v1 SSO, `handover_code`); the listener of that
//!   host redeems it for the host's own session (OTL-06, OTL-07).

use std::sync::Arc;

use time::Duration;

use super::artifacts::live;
use super::auth::AuthService;
use super::gate_listener::handover_code;
use super::settings::Settings;
use super::support::{authorized, owner_or_admin};
use crate::domain::{
    ArtifactId, Domain, DomainKind, Environment, GateCodeSecret, Host, LinkSecret, LinkTarget,
    OneTimeLink, ReturnPath, Session, SessionId, SessionKind, SessionSecret, Timestamp, User,
};
use crate::error::Error;
use crate::ports::{Action, Clock as _, Ports, Resource, Rng as _, Store as _};

/// How long a one-time link can be redeemed.
pub const LINK_TTL: Duration = Duration::minutes(10);

/// A link [`LinksService::issue`] made: shown once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedLink {
    /// `https://<api host>/api/v1/auth/links/<token>`.
    pub url: String,
    /// Until when it can be redeemed.
    pub expires_at: Timestamp,
}

/// What [`LinksService::redeem`] opened: a `browser` session for the
/// link's user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSession {
    /// The session secret, the value of the cookie; only its hash is
    /// stored.
    pub token: SessionSecret,
    /// The session as stored (`kind = browser`, 24 hours).
    pub session: Session,
    /// What the link opened: where the browser goes next.
    pub target: LinkTarget,
}

/// What [`LinksService::open`] did with a link.
///
/// The `browser` session it opened, for the API host's cookie, and the
/// session handover to the target's own host, which cannot see that
/// cookie (the gate's one-time code, v1 SSO).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedLink {
    /// The `browser` session the link opened (the API host's cookie
    /// `__Host-liftoff_session`).
    pub browser: BrowserSession,
    /// The address of the target the browser goes to:
    /// `https://<host>/.liftoff/callback?code=<code>`.
    pub host: Host,
    /// The gate's one-time code for `host` (ten minutes, one use), which
    /// carries the path to return to; the listener of `host` redeems it
    /// for the host's session `__Host-liftoff_gate`.
    pub code: GateCodeSecret,
}

/// What [`LinksService::open_own`] did with a link opened by its own
/// URL (`GET /auth/links/{token}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenedOwn {
    /// A link to an environment: as [`LinksService::open`] without an
    /// address of its own.
    Address(OpenedLink),
    /// A link to an artifact (`login_url` of a publish, `lof artifact
    /// open`): the `browser` session alone. The artifact is served on
    /// the API host, which sees that session's cookie; on `[artifacts]
    /// host` the gate hands it over.
    Artifact {
        /// The `browser` session the link opened.
        browser: BrowserSession,
        /// The artifact the browser goes to.
        artifact: ArtifactId,
    },
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct LinksService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for LinksService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> LinksService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// A one-time link of `actor` to `target`.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` may open `target` (the
    /// `authorize` hook is asked once, [`Action::LinkIssue`]);
    /// [`Error::NotFound`] for a target that does not exist; whatever the
    /// store reports.
    pub async fn issue(&self, actor: &User, target: &LinkTarget) -> Result<IssuedLink, Error> {
        let ports = self.ports.as_ref();
        if let LinkTarget::Artifact(id) = target {
            // `lof artifact open`: its owner or
            // an admin, as for every operation on an artifact.
            let found = live(ports, *id).await;
            let artifact = authorized(ports, actor, Action::LinkIssue, found, |artifact| {
                Resource::Artifact(artifact.id)
            })
            .await?;
            if artifact.owner_user_id != actor.id && !actor.is_admin() {
                return Err(Error::forbidden(format!(
                    "{} may not {}: artifact {id} is another user's",
                    actor.login,
                    Action::LinkIssue
                )));
            }
            return self.issue_checked(actor, target).await;
        }
        let found = self.environment_of(target).await;
        let environment = authorized(ports, actor, Action::LinkIssue, found, |environment| {
            Resource::Environment(environment.id)
        })
        .await?;
        owner_or_admin(actor, &environment, Action::LinkIssue)?;
        self.issue_checked(actor, target).await
    }

    /// The link of [`LinksService::issue`] once `actor` may have it: an
    /// operation that already asked about `target` (`tunnel.open` for the
    /// owner's own tunnel) issues it without asking again.
    pub(super) async fn issue_checked(
        &self,
        actor: &User,
        target: &LinkTarget,
    ) -> Result<IssuedLink, Error> {
        let ports = self.ports.as_ref();
        let secret = LinkSecret::from_body(&ports.rng().token_secret());
        let now = ports.clock().now();
        let link = OneTimeLink {
            token_hash: secret.hash(),
            user_id: actor.id,
            target: *target,
            expires_at: now.saturating_add(LINK_TTL),
            used_at: None,
            created_at: now,
        };
        ports.store().insert_one_time_link(&link).await?;
        tracing::info!(
            target = %link.target.as_stored(),
            by = %actor.login,
            "issued a one-time link"
        );
        Ok(IssuedLink {
            url: format!(
                "https://{}/api/v1/auth/links/{}",
                self.settings.proxy.api_host,
                secret.expose()
            ),
            expires_at: link.expires_at,
        })
    }

    /// `GET /auth/links/{token}` (`at` is `None`, `rd` the root) and a
    /// link pasted on the login page of the private address `at`, which
    /// came from `rd` there: [`LinksService::redeem`] (with the environment `at` is
    /// an address of as the expected target when `at` is given), then the
    /// gate's one-time code of the new `browser` session for the target's
    /// address (the host `at`, else the target's managed domain), which
    /// returns to `rd` (OTL-06, OTL-07).
    ///
    /// # Errors
    ///
    /// As [`LinksService::redeem`]: [`Error::Gone`] for a link that is
    /// unknown, expired or used, [`Error::Forbidden`] for a link of
    /// another target than the address `at`; [`Error::NotFound`] for a
    /// target without an address; whatever the store reports. The link
    /// stays unused on every error but the store's.
    pub async fn open(
        &self,
        token: &LinkSecret,
        at: Option<&Host>,
        rd: ReturnPath,
    ) -> Result<OpenedLink, Error> {
        let (target, host) = if let Some(host) = at {
            (self.target_at(host).await?, host.clone())
        } else {
            let target = self.unused_target(token).await?;
            let host = self.address_of(&target).await?;
            (target, host)
        };
        let browser = self.redeem(token, Some(&target)).await?;
        let code = handover_code(self.ports.as_ref(), &browser.session, &host, rd).await?;
        Ok(OpenedLink {
            browser,
            host,
            code,
        })
    }

    /// `GET /auth/links/{token}`: the link opened by its own URL, for an
    /// environment as [`LinksService::open`] (no address `at`, back to
    /// the root), for an artifact the `browser` session alone
    /// ([`OpenedOwn::Artifact`]).
    ///
    /// # Errors
    ///
    /// As [`LinksService::open`].
    pub async fn open_own(&self, token: &LinkSecret) -> Result<OpenedOwn, Error> {
        match self.unused_target(token).await? {
            target @ LinkTarget::Artifact(artifact) => {
                let browser = self.redeem(token, Some(&target)).await?;
                Ok(OpenedOwn::Artifact { browser, artifact })
            }
            LinkTarget::Environment(_) => self
                .open(token, None, ReturnPath::root())
                .await
                .map(OpenedOwn::Address),
        }
    }

    /// The target of the environment `host` is an address of; a host
    /// that is none is a link for another address ([`Error::Forbidden`]).
    async fn target_at(&self, host: &Host) -> Result<LinkTarget, Error> {
        let store = self.ports.store();
        let binding = match store.domain_by_host(host).await? {
            Some(domain) => store.binding_by_domain(domain.id).await?,
            None => None,
        };
        binding
            .map(|binding| LinkTarget::Environment(binding.environment_id))
            .ok_or_else(|| Error::forbidden("the link is for another address"))
    }

    /// The target of the link `token` while it can be redeemed, without
    /// redeeming it.
    async fn unused_target(&self, token: &LinkSecret) -> Result<LinkTarget, Error> {
        let now = self.ports.clock().now();
        self.ports
            .store()
            .one_time_link(&token.hash())
            .await?
            .filter(|link| link.used_at.is_none() && link.expires_at > now)
            .map(|link| link.target)
            .ok_or_else(|| Error::gone("the link expired or was used"))
    }

    /// The address the browser opens `target` at: its managed domain,
    /// else its first external one (the oldest of either kind).
    async fn address_of(&self, target: &LinkTarget) -> Result<Host, Error> {
        let store = self.ports.store();
        let environment = self.environment_of(target).await?;
        let mut domains: Vec<Domain> = Vec::new();
        for binding in store.bindings_of_environment(environment.id).await? {
            if let Some(domain) = store.domain(binding.domain_id).await? {
                domains.push(domain);
            }
        }
        domains
            .into_iter()
            .min_by_key(|domain| (domain.kind != DomainKind::Managed, domain.created_at))
            .map(|domain| domain.host)
            .ok_or_else(|| {
                Error::not_found(format!("an address of environment {}", environment.name))
            })
    }

    /// Deletes the links that expired, used or not (OTL-01); how many.
    /// The daemon runs it hourly with the gate's pruning.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn prune(&self) -> Result<usize, Error> {
        let now = self.ports.clock().now();
        self.ports.store().prune_one_time_links(now).await
    }

    /// The environment `target` names.
    async fn environment_of(&self, target: &LinkTarget) -> Result<Environment, Error> {
        match target {
            LinkTarget::Environment(id) => self
                .ports
                .store()
                .environment(*id)
                .await?
                .ok_or_else(|| Error::not_found(format!("environment {id}"))),
            // An artifact has no environment and no address of its own
            // to hand a session over to: `lof artifact open`
            // and its redemption on the API host are their own branch.
            LinkTarget::Artifact(id) => Err(Error::invalid(
                "target",
                format!("artifact:{id} is not an environment"),
            )),
        }
    }

    /// Redeems `token`: a new `browser` session of the link's user.
    /// `expected` is the target the browser is on its way to when it
    /// knows one (the login page of a private address, where a link is
    /// pasted); `None` takes the link's own (`GET /auth/links/{token}`).
    ///
    /// # Errors
    ///
    /// [`Error::Gone`] for a link that is unknown, expired or used
    /// (OTL-01, OTL-02); [`Error::Forbidden`] for a link of another
    /// target than `expected` (OTL-03), which stays unused; whatever the
    /// store reports.
    pub async fn redeem(
        &self,
        token: &LinkSecret,
        expected: Option<&LinkTarget>,
    ) -> Result<BrowserSession, Error> {
        let ports = self.ports.as_ref();
        let store = ports.store();
        let hash = token.hash();
        let now = ports.clock().now();
        let gone = || Error::gone("the link expired or was used");
        let link = store
            .one_time_link(&hash)
            .await?
            .filter(|link| link.used_at.is_none() && link.expires_at > now)
            .ok_or_else(gone)?;
        if let Some(expected) = expected
            && &link.target != expected
        {
            tracing::warn!(
                link = %link.target.as_stored(),
                asked = %expected.as_stored(),
                "refused a one-time link for another target"
            );
            return Err(Error::forbidden("the link is for another address"));
        }
        // The store marks it used once: of two redeems racing, one wins.
        let link = store
            .use_one_time_link(&hash, now)
            .await?
            .ok_or_else(gone)?;
        let secret = SessionSecret::from_body(&ports.rng().token_secret());
        let session = Session {
            id: SessionId::new(ports.rng().ulid()),
            user_id: link.user_id,
            token_hash: secret.hash(),
            name: "browser".to_owned(),
            kind: SessionKind::Browser,
            expires_at: now.saturating_add(AuthService::<P>::BROWSER_SESSION_TTL),
            last_used_at: now,
            created_at: now,
        };
        store.insert_session(&session).await?;
        tracing::info!(session = %session.id, target = %link.target.as_stored(), "redeemed a one-time link");
        Ok(BrowserSession {
            token: secret,
            session,
            target: link.target,
        })
    }
}

#[cfg(test)]
mod tests;
