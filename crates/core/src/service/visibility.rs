//! [`VisibilityService`] and [`Access`].
//!
//! - [`VisibilityService::share`] and [`VisibilityService::unshare`]
//!   (`POST /projects/{p}/environments/{e}/share|unshare`, `lof env
//!   share|unshare`, `lof dev --share`) turn an environment `shared` or
//!   back `private`; only its owner (a tunnel's `owner_user_id`) or an
//!   admin may, and the `authorize` hook is asked once (VIS-01, VIS-06);
//! - [`Access::check`] is what the function and tunnel listeners ask
//!   before a single byte reaches the environment. The host of the
//!   request must be an address of the target (a domain bound to its
//!   environment), else `403` whatever else the request carries: the
//!   `gate` hook judges the host, so a host of another project must not
//!   stand in for the target's. Then `shared` passes (VIS-02); `private`
//!   needs, of the owner or the admin (VIS-04), the session of this
//!   host in the cookie (`__Host-liftoff_gate`, a [`GateSession`] handed over from a `browser` session, never the API session) or a `cli` session as the bearer, else a
//!   browser goes to the login and a JSON client gets `401` (VIS-03);
//!   with the `tenants` module the `gate` hook decides who besides
//!   passes (VIS-05);
//! - an artifact has no domain: `shared` passes,
//!   `private` needs the API host's `browser` session or a `cli` bearer
//!   of its owner or an admin, and the `gate` hook is not asked.

use std::sync::Arc;

use super::artifacts::live;
use super::settings::Settings;
use super::support::{authorized_environment, owner_or_admin, private_refusal};
use crate::domain::{
    ArtifactId, Bearer, EnvName, Environment, EnvironmentId, GateSecret, GateSession, Host,
    LinkTarget, ProjectName, Session, SessionKind, SessionSecret, User, UserId, Visibility,
};
use crate::error::Error;
use crate::ports::{Action, Clock as _, HookRequest, Ports, Store as _};

/// See the [module documentation](self).
#[derive(Debug)]
pub struct VisibilityService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for VisibilityService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> VisibilityService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// `share`: the environment `env` of `project` opens to anybody.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` owns the environment or is an
    /// admin (VIS-06), or when the `authorize` hook denies;
    /// [`Error::NotFound`] for an unknown project or environment;
    /// whatever the store reports.
    pub async fn share(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
    ) -> Result<Environment, Error> {
        self.set(
            actor,
            project,
            env,
            Action::EnvironmentShare,
            Visibility::Shared,
        )
        .await
    }

    /// `unshare`: the environment `env` of `project` is private again.
    ///
    /// # Errors
    ///
    /// As [`VisibilityService::share`].
    pub async fn unshare(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
    ) -> Result<Environment, Error> {
        self.set(
            actor,
            project,
            env,
            Action::EnvironmentUnshare,
            Visibility::Private,
        )
        .await
    }

    /// `share` or `unshare`: `authorize` once about the environment, the
    /// owner rule, then `visibility` stored (nothing is written when it
    /// is already so).
    async fn set(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        action: Action,
        visibility: Visibility,
    ) -> Result<Environment, Error> {
        let ports = self.ports.as_ref();
        let (_project, environment) =
            authorized_environment(ports, actor, action, project, env).await?;
        owner_or_admin(actor, &environment, action)?;
        if environment.visibility == visibility {
            return Ok(environment);
        }
        let changed = Environment {
            visibility,
            ..environment
        };
        ports.store().update_environment(&changed).await?;
        tracing::info!(
            environment = %changed.name,
            %visibility,
            by = %actor.login,
            "changed the visibility of an environment"
        );
        Ok(changed)
    }
}

/// What a listener knows of a request when it asks [`Access::check`]:
/// the adapter reads it from the HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRequest {
    /// The host the request is for (the `gate` hook sees it); it must be
    /// an address of the target.
    pub host: Host,
    /// The session of `host` in its cookie (`__Host-liftoff_gate`), if
    /// any: a host session, never the API's `browser` session, so the
    /// secret the listener might let through opens this host only (an artifact reads it on `[artifacts] host` alone).
    pub cookie: Option<GateSecret>,
    /// The API host's `browser` session in its cookie
    /// (`__Host-liftoff_session`), if any: what a private artifact on
    /// the API host is opened with. The
    /// listeners of other hosts never see it and leave it `None`.
    pub session: Option<SessionSecret>,
    /// `Authorization: Bearer`, if any.
    pub bearer: Option<Bearer>,
    /// The `Accept` header, if any: `application/json` asks for `401`
    /// instead of the login page.
    pub accept: Option<String>,
    /// The request as the `gate` hook sees it, without its credentials.
    pub request: HookRequest,
}

/// What [`Access::check`] decided; the listener renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    /// The request passes; `user` is who it is (`None` for a `shared`
    /// target without a session).
    Allow {
        /// The person behind the request, when it carried a session.
        user: Option<User>,
    },
    /// A browser without a session: the login page (`302`, VIS-03).
    Login,
    /// A JSON client without a session (`401`, VIS-03).
    Unauthenticated,
    /// A session of someone who may not open the target (`403`); the
    /// reason is safe to show.
    Forbidden(String),
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct Access<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for Access<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> Access<P> {
    /// The check over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// Whether `request` may open `target`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a target that does not exist; whatever the
    /// store reports. A host that is not an address of `target` is
    /// [`AccessDecision::Forbidden`], not an error.
    pub async fn check(
        &self,
        request: &AccessRequest,
        target: &LinkTarget,
    ) -> Result<AccessDecision, Error> {
        let store = self.ports.store();
        let environment = match target {
            LinkTarget::Environment(id) => store
                .environment(*id)
                .await?
                .ok_or_else(|| Error::not_found(format!("environment {id}")))?,
            LinkTarget::Artifact(id) => return self.check_artifact(request, *id).await,
        };
        if !self.is_address_of(&request.host, environment.id).await? {
            return Ok(AccessDecision::Forbidden(format!(
                "{} is not an address of environment {}",
                request.host, environment.name
            )));
        }
        if environment.visibility == Visibility::Shared {
            return Ok(AccessDecision::Allow { user: None });
        }
        let Some(user) = self.user_of(request).await? else {
            return Ok(if wants_json(request.accept.as_deref()) {
                AccessDecision::Unauthenticated
            } else {
                AccessDecision::Login
            });
        };
        let refusal = private_refusal(
            self.ports.as_ref(),
            &environment,
            &request.host,
            &user,
            &request.request,
        )
        .await;
        Ok(refusal.map_or(
            AccessDecision::Allow { user: Some(user) },
            AccessDecision::Forbidden,
        ))
    }

    /// [`Access::check`] of an artifact: it
    /// lives on the API host (or `[artifacts] host`), so the `gate` hook,
    /// which judges the addresses of environments, is not asked; `shared`
    /// passes (with the viewer's user when a live session came along, so the wrapper knows its owner), `private` needs a `browser`
    /// session of the API host, a session of `[artifacts] host` on that
    /// host, or a `cli` bearer of its owner or an admin. An expired one is gone
    /// (ART-08).
    async fn check_artifact(
        &self,
        request: &AccessRequest,
        id: ArtifactId,
    ) -> Result<AccessDecision, Error> {
        let artifact = live(self.ports.as_ref(), id).await?;
        let user = self.artifact_viewer(request).await?;
        if artifact.visibility == Visibility::Shared {
            return Ok(AccessDecision::Allow { user });
        }
        let Some(user) = user else {
            return Ok(if wants_json(request.accept.as_deref()) {
                AccessDecision::Unauthenticated
            } else {
                AccessDecision::Login
            });
        };
        if user.id == artifact.owner_user_id || user.is_admin() {
            Ok(AccessDecision::Allow { user: Some(user) })
        } else {
            Ok(AccessDecision::Forbidden(format!(
                "artifact {id} is another user's"
            )))
        }
    }

    /// Whose live session a request for an artifact carries: the API
    /// host's `browser` session in its cookie, else, on `[artifacts]
    /// host` alone, that host's session (`__Host-liftoff_gate`, handed over from the API host as the gate does), else a `cli`
    /// session as the bearer; `None` for anything else.
    async fn artifact_viewer(&self, request: &AccessRequest) -> Result<Option<User>, Error> {
        let from_cookie = match &request.session {
            Some(secret) => self
                .session_of_kind(secret, SessionKind::Browser)
                .await?
                .map(|session| session.user_id),
            None => None,
        };
        let on_artifacts_host = self.settings.artifacts.host.as_ref() == Some(&request.host);
        let from_cookie = match (from_cookie, &request.cookie) {
            (Some(user_id), _) => Some(user_id),
            (None, Some(secret)) if on_artifacts_host => self
                .host_session(&request.host, secret)
                .await?
                .map(|gate| gate.user_id),
            (None, _) => None,
        };
        let user_id = match (from_cookie, &request.bearer) {
            (Some(user_id), _) => Some(user_id),
            (None, Some(Bearer::Session(secret))) => self
                .cli_session(secret)
                .await?
                .map(|session| session.user_id),
            (None, Some(Bearer::Deploy(_)) | None) => None,
        };
        self.user(user_id).await
    }

    /// Whether `host` is a domain bound to the environment `id`.
    async fn is_address_of(&self, host: &Host, id: EnvironmentId) -> Result<bool, Error> {
        let store = self.ports.store();
        let Some(domain) = store.domain_by_host(host).await? else {
            return Ok(false);
        };
        Ok(store
            .binding_by_domain(domain.id)
            .await?
            .is_some_and(|binding| binding.environment_id == id))
    }

    /// Whose live session `request` carries: the session of its host in
    /// the cookie, else a `cli` session as the bearer. Anything else (no
    /// credential, an unknown or expired one, a session of another host
    /// or kind) counts as none. Nothing is written: the check runs for
    /// every request of a private address.
    async fn user_of(&self, request: &AccessRequest) -> Result<Option<User>, Error> {
        let from_cookie = match &request.cookie {
            Some(secret) => self
                .host_session(&request.host, secret)
                .await?
                .map(|gate| gate.user_id),
            None => None,
        };
        let user_id = match (from_cookie, &request.bearer) {
            (Some(user_id), _) => Some(user_id),
            (None, Some(Bearer::Session(secret))) => self
                .cli_session(secret)
                .await?
                .map(|session| session.user_id),
            (None, Some(Bearer::Deploy(_)) | None) => None,
        };
        self.user(user_id).await
    }

    /// The user `id`, if any.
    async fn user(&self, id: Option<UserId>) -> Result<Option<User>, Error> {
        match id {
            Some(id) => self.ports.store().user(id).await,
            None => Ok(None),
        }
    }

    /// The live session on `host` whose secret is `secret`.
    async fn host_session(
        &self,
        host: &Host,
        secret: &GateSecret,
    ) -> Result<Option<GateSession>, Error> {
        let now = self.ports.clock().now();
        Ok(self
            .ports
            .store()
            .gate_session(&secret.hash())
            .await?
            .filter(|gate| &gate.host == host && gate.expires_at > now))
    }

    /// The live `cli` session whose secret is `secret`.
    async fn cli_session(&self, secret: &SessionSecret) -> Result<Option<Session>, Error> {
        self.session_of_kind(secret, SessionKind::Cli).await
    }

    /// The live session of `kind` whose secret is `secret`.
    async fn session_of_kind(
        &self,
        secret: &SessionSecret,
        kind: SessionKind,
    ) -> Result<Option<Session>, Error> {
        let now = self.ports.clock().now();
        Ok(self
            .ports
            .store()
            .session_by_hash(&secret.hash())
            .await?
            .filter(|session| session.kind == kind && session.expires_at > now))
    }
}

/// Whether a client that sent `accept` wants `401` rather than the login
/// page: it asks for `application/json` and not for HTML (a browser asks
/// for `text/html`).
fn wants_json(accept: Option<&str>) -> bool {
    accept.is_some_and(|accept| {
        let accept = accept.to_ascii_lowercase();
        accept.contains("application/json") && !accept.contains("text/html")
    })
}

#[cfg(test)]
mod tests;
