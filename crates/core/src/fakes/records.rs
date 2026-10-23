//! [`Records`]: builders of valid domain records with deterministic ids.

use time::Duration;

use super::clock::{FixedClock, SeqRng};
use crate::domain::{
    Audit, AuditId, Binding, BindingId, Branch, ContainerPort, CredentialKind, Deploy, DeployId,
    DeployNumber, DeploySource, DeployStatus, DeployToken, DeployTokenId, DeployTokenSecret,
    Domain, DomainId, DomainKind, EnvKind, EnvName, EnvState, Environment, EnvironmentId,
    GateSecret, GateSession, GitConnection, GitConnectionId, GitProvider, Host, HostPort, ImageTag,
    Limits, Login, Pin, Project, ProjectId, ProjectName, RepoRef, SealedEnv, Secret, SecretId,
    SecretKey, SecretScope, ServiceName, Session, SessionId, SessionKind, SessionSecret,
    SetupSecret, SetupToken, Trigger, UpdatedBy, User, UserId, UserRole, UserSource, VersionNumber,
    Visibility,
};
use crate::error::Error;
use crate::ports::{Clock as _, Rng as _};

/// Builds valid records for tests. Ids come from a [`SeqRng`], timestamps
/// from a [`FixedClock`]; share them with [`FakePorts`](super::FakePorts) so
/// ids never collide with ones the services generate.
///
/// Every builder takes raw strings and validates them, failing with the
/// [`Error::Validation`] a typo deserves.
#[derive(Debug, Clone, Default)]
pub struct Records {
    rng: SeqRng,
    clock: FixedClock,
}

impl Records {
    /// A factory over `rng` and `clock`.
    #[must_use]
    pub const fn new(rng: SeqRng, clock: FixedClock) -> Self {
        Self { rng, clock }
    }

    /// A `local` user with `email = "<login>@example.com"`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid login.
    pub fn user(&self, login: &str, role: UserRole) -> Result<User, Error> {
        Ok(User {
            id: UserId::new(self.rng.ulid()),
            login: Login::new(login)?,
            email: format!("{login}@example.com"),
            role,
            source: UserSource::Local,
            subject: None,
            created_at: self.clock.now(),
        })
    }

    /// A `cli` session of `user` valid for 30 days, and its secret.
    #[must_use]
    pub fn session(&self, user: &User) -> (Session, SessionSecret) {
        let secret = SessionSecret::from_body(&self.rng.token_secret());
        let now = self.clock.now();
        let session = Session {
            id: SessionId::new(self.rng.ulid()),
            user_id: user.id,
            token_hash: secret.hash(),
            name: "test-machine".to_owned(),
            kind: SessionKind::Cli,
            expires_at: now.saturating_add(Duration::days(30)),
            last_used_at: now,
            created_at: now,
        };
        (session, secret)
    }

    /// A single-use login token of `user` minted now, and its secret.
    #[must_use]
    pub fn setup_token(&self, user: &User) -> (SetupToken, SetupSecret) {
        let secret = SetupSecret::from_body(&self.rng.token_secret());
        let token = SetupToken {
            token_hash: secret.hash(),
            user_id: user.id,
            created_at: self.clock.now(),
        };
        (token, secret)
    }

    /// A Forgejo token connection of `user` on `host` as the same login,
    /// with the credential `sealed`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid host.
    pub fn git_connection(
        &self,
        user: &User,
        host: &str,
        sealed: SealedEnv,
    ) -> Result<GitConnection, Error> {
        Ok(GitConnection {
            id: GitConnectionId::new(self.rng.ulid()),
            user_id: user.id,
            provider: GitProvider::Forgejo,
            host: Host::new(host)?,
            account_login: user.login.clone(),
            credential: sealed,
            kind: CredentialKind::Token,
            expires_at: None,
            created_at: self.clock.now(),
        })
    }

    /// A project without tenant, manifest or template.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid name, host or repository.
    pub fn project(
        &self,
        name: &str,
        git_host: &str,
        repo: &str,
        by: &User,
    ) -> Result<Project, Error> {
        Ok(Project {
            id: ProjectId::new(self.rng.ulid()),
            tenant_id: None,
            name: ProjectName::new(name)?,
            git_host: Host::new(git_host)?,
            repo: RepoRef::new(repo)?,
            manifest: None,
            template: None,
            created_by: by.id,
            created_at: self.clock.now(),
        })
    }

    /// An empty, unpinned, shared compose environment with default limits.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid name.
    pub fn environment(&self, project: &Project, name: &str) -> Result<Environment, Error> {
        Ok(Environment {
            id: EnvironmentId::new(self.rng.ulid()),
            project_id: project.id,
            name: EnvName::new(name)?,
            kind: EnvKind::Compose,
            pin: None,
            owner_user_id: None,
            limits: Limits::default(),
            state: EnvState::Empty,
            current_deploy_id: None,
            visibility: Visibility::Shared,
            protected: false,
            created_at: self.clock.now(),
        })
    }

    /// An empty, unpinned, private tunnel environment `name` of
    /// `project` leading to `owner`'s machine, as `GET /tunnel` creates
    /// one.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid name.
    pub fn tunnel(
        &self,
        project: &Project,
        name: &str,
        owner: &User,
    ) -> Result<Environment, Error> {
        Ok(Environment {
            kind: EnvKind::Tunnel,
            owner_user_id: Some(owner.id),
            visibility: EnvKind::Tunnel.default_visibility(),
            ..self.environment(project, name)?
        })
    }

    /// An empty, unpinned function environment `name` of `project`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid name.
    pub fn function(&self, project: &Project, name: &str) -> Result<Environment, Error> {
        Ok(Environment {
            kind: EnvKind::Function,
            ..self.environment(project, name)?
        })
    }

    /// A `browser` session of `user` valid for 24 hours, and its secret
    /// (what a one-time link or a login module opens).
    #[must_use]
    pub fn browser_session(&self, user: &User) -> (Session, SessionSecret) {
        let (session, secret) = self.session(user);
        let session = Session {
            kind: SessionKind::Browser,
            expires_at: session.created_at.saturating_add(Duration::hours(24)),
            ..session
        };
        (session, secret)
    }

    /// A session on `host` handed over from the browser session
    /// `session` (the cookie `__Host-liftoff_gate`), and its secret.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid host.
    pub fn host_session(
        &self,
        session: &Session,
        host: &str,
    ) -> Result<(GateSession, GateSecret), Error> {
        let secret = GateSecret::from_body(&self.rng.token_secret());
        let gate = GateSession {
            token_hash: secret.hash(),
            session_id: session.id,
            user_id: session.user_id,
            host: Host::new(host)?,
            expires_at: session.expires_at,
            created_at: self.clock.now(),
        };
        Ok((gate, secret))
    }

    /// A pin of `branch` by `by`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid branch.
    pub fn pin(branch: &str, by: &User) -> Result<Pin, Error> {
        Ok(Pin {
            branch: Branch::new(branch)?,
            pinned_by: by.id,
        })
    }

    /// A managed, unproxied, unverified domain of `project`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid host.
    pub fn domain(&self, project: &Project, host: &str) -> Result<Domain, Error> {
        Ok(Domain {
            id: DomainId::new(self.rng.ulid()),
            tenant_id: None,
            project_id: project.id,
            host: Host::new(host)?,
            kind: DomainKind::Managed,
            verified_at: None,
            record_lost_at: None,
            proxied: false,
            created_at: self.clock.now(),
        })
    }

    /// A binding of `service:port` on `environment` to `domain` at
    /// `host_port`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid service or a port out of range.
    pub fn binding(
        &self,
        environment: &Environment,
        domain: &Domain,
        service: &str,
        port: u16,
        host_port: u16,
    ) -> Result<Binding, Error> {
        Ok(Binding {
            id: BindingId::new(self.rng.ulid()),
            environment_id: environment.id,
            domain_id: domain.id,
            service: ServiceName::new(service)?,
            container_port: ContainerPort::new(port)?,
            host_port: HostPort::new(host_port)?,
        })
    }

    /// A CLI deploy of tag `sha-<number>` with a one-service compose file.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] when `number` is zero.
    pub fn deploy(
        &self,
        environment: &Environment,
        number: u32,
        status: DeployStatus,
        by: &User,
    ) -> Result<Deploy, Error> {
        let finished = status.is_terminal().then(|| self.clock.now());
        Ok(Deploy {
            id: DeployId::new(self.rng.ulid()),
            environment_id: environment.id,
            number: DeployNumber::new(number)?,
            source: DeploySource::Cli,
            git_ref: None,
            tag: ImageTag::new(&format!("sha-{number}"))?,
            compose: "services:\n  web:\n    image: nginx:alpine\n".to_owned(),
            component_hash: None,
            status,
            log: String::new(),
            error: None,
            rolled_back_to: None,
            triggered_by: Trigger::User(by.id),
            started_at: finished,
            finished_at: finished,
        })
    }

    /// A live deploy token of `project` and its secret.
    #[must_use]
    pub fn deploy_token(&self, project: &Project, by: &User) -> (DeployToken, DeployTokenSecret) {
        let secret = DeployTokenSecret::from_body(&self.rng.token_secret());
        let token = DeployToken {
            id: DeployTokenId::new(self.rng.ulid()),
            project_id: project.id,
            token_hash: secret.hash(),
            created_by: by.id,
            created_at: self.clock.now(),
            revoked_at: None,
        };
        (token, secret)
    }

    /// Version 1 of secret `key` in `scope` with the value `sealed`,
    /// written by `by`, not synced to CI.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for an invalid key.
    pub fn secret(
        &self,
        scope: SecretScope,
        key: &str,
        sealed: SealedEnv,
        by: &User,
    ) -> Result<Secret, Error> {
        let now = self.clock.now();
        Ok(Secret {
            id: SecretId::new(self.rng.ulid()),
            scope,
            key: SecretKey::new(key)?,
            value: sealed,
            version: VersionNumber::FIRST,
            ci: false,
            updated_by: UpdatedBy::User(by.id),
            updated_at: now,
            created_at: now,
        })
    }

    /// The next version of `secret` with the value `sealed`.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the version number overflows.
    pub fn revise(&self, secret: &Secret, sealed: SealedEnv) -> Result<Secret, Error> {
        let version = secret
            .version
            .next()
            .ok_or_else(|| Error::internal_msg("secret version overflow"))?;
        Ok(Secret {
            value: sealed,
            version,
            updated_at: self.clock.now(),
            ..secret.clone()
        })
    }

    /// An audit entry of `actor`.
    #[must_use]
    pub fn audit(&self, actor: Option<&User>, action: &str, resource: &str) -> Audit {
        Audit {
            id: AuditId::new(self.rng.ulid()),
            actor_user_id: actor.map(|user| user.id),
            action: action.to_owned(),
            resource: resource.to_owned(),
            at: self.clock.now(),
        }
    }
}
