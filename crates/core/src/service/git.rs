//! `GitService`: a user's own forge connections.
//!
//! Forgejo is known by personal access token only: the token is checked
//! with one call ([`Git::account`](crate::ports::Git::account), the
//! `validate_token` tool of the `git.forgejo` module, `GET /user`),
//! sealed by the [`Secrets`](crate::ports::Secrets) port and stored as the
//! user's connection on that host, replacing an earlier one.
//! GitHub and GitLab are `git.*` modules too: GitHub by token
//! or, when its module has an OAuth application, by the forge's device
//! flow ([`GitService::device_start`], [`GitService::device_poll`]), whose
//! token is checked and stored the same way (kind `oauth`). A connection
//! is never shown with its credential.

use std::sync::Arc;

use time::Duration;

use super::Authorizer;
use super::support::open_connection;
use crate::domain::{
    CredentialKind, DeviceCode, GitConnection, GitConnectionId, GitProvider, Host, SecretValue,
    User,
};
use crate::error::Error;
use crate::ports::{
    Action, Clock as _, DeviceGrant, DeviceStart, Git as _, GitAccess, Ports, Resource, Rng as _,
    Secrets as _, Store as _,
};

/// One poll of [`GitService::device_poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePolled {
    /// The person has not confirmed yet: poll again after the same
    /// interval.
    Pending,
    /// The person has not confirmed yet, and the forge asks to poll less
    /// often (RFC 8628 3.5 `slow_down`): see
    /// [`DeviceGrant::SlowDown`].
    SlowDown {
        /// The interval the forge asks for, if it said.
        interval: Option<u64>,
    },
    /// Confirmed: the connection stored.
    Connected(GitConnection),
}

/// A token to store as a connection: how it was obtained, and how many
/// seconds it stays valid when the forge said.
#[derive(Debug)]
struct Credential {
    token: SecretValue,
    kind: CredentialKind,
    expires_in: Option<u64>,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct GitService<P> {
    ports: Arc<P>,
}

impl<P> Clone for GitService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
        }
    }
}

impl<P: Ports> GitService<P> {
    /// A service over `ports`.
    #[must_use]
    pub const fn new(ports: Arc<P>) -> Self {
        Self { ports }
    }

    /// The connections of `actor`, by host.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn list(&self, actor: &User) -> Result<Vec<GitConnection>, Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::GitList, &Resource::Admin)
            .await?;
        self.ports.store().git_connections_of_user(actor.id).await
    }

    /// Checks `token` on `host` and stores it as `actor`'s connection
    /// there, replacing an earlier one.
    ///
    /// # Errors
    ///
    /// - [`Error::ModuleRequired`] when the forge's `git.*` module is not
    ///   installed (or disabled);
    /// - [`Error::Forbidden`] when the forge rejects the token (nothing is
    ///   stored);
    /// - whatever the forge, the secrets port or the store report.
    pub async fn connect(
        &self,
        actor: &User,
        provider: GitProvider,
        host: Host,
        token: SecretValue,
    ) -> Result<GitConnection, Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::GitConnect, &Resource::Admin)
            .await?;
        let credential = Credential {
            token,
            kind: CredentialKind::Token,
            expires_in: None,
        };
        self.store(actor, provider, host, credential).await
    }

    /// Checks the credential's token with the forge (one call, `/user`)
    /// and stores it sealed as `actor`'s connection on `host`, replacing
    /// an earlier one; a token valid for `expires_in` seconds expires
    /// then.
    async fn store(
        &self,
        actor: &User,
        provider: GitProvider,
        host: Host,
        Credential {
            token,
            kind,
            expires_in,
        }: Credential,
    ) -> Result<GitConnection, Error> {
        let account_login = self.ports.git().account(provider, &host, &token).await?;
        let now = self.ports.clock().now();
        let lifetime = |seconds: u64| Duration::seconds(i64::try_from(seconds).unwrap_or(i64::MAX));
        let connection = GitConnection {
            id: GitConnectionId::new(self.ports.rng().ulid()),
            user_id: actor.id,
            provider,
            host,
            account_login,
            credential: self.ports.secrets().encrypt(token.expose())?,
            kind,
            expires_at: expires_in.map(|seconds| now.saturating_add(lifetime(seconds))),
            created_at: now,
        };
        self.ports.store().put_git_connection(&connection).await?;
        tracing::info!(
            login = %actor.login,
            host = %connection.host,
            account = %connection.account_login,
            kind = connection.kind.as_str(),
            "stored a git connection"
        );
        Ok(connection)
    }

    /// Starts the OAuth device flow of `provider` on `host` for `actor`
    /// (`POST /git/connections/{host}/device`).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] with a hint to connect with a token when the
    /// forge's module has no OAuth application (GIT-01); whatever the
    /// forge reports.
    pub async fn device_start(
        &self,
        actor: &User,
        provider: GitProvider,
        host: Host,
    ) -> Result<DeviceStart, Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::GitConnect, &Resource::Admin)
            .await?;
        self.ports.git().device_start(provider, &host).await
    }

    /// Polls the device flow `device_code` once: [`DevicePolled::Pending`]
    /// (or [`DevicePolled::SlowDown`]) while the person has not
    /// confirmed; then checks the token as [`Self::connect`] does
    /// and stores it as `actor`'s connection on `host` (kind `oauth`),
    /// replacing an earlier one (`POST /git/connections/{host}/device/poll`).
    ///
    /// # Errors
    ///
    /// Whatever [`Self::connect`] and the forge report:
    /// [`Error::Forbidden`] when the person denied the access or the
    /// granted token lacks what liftoff needs (nothing is stored),
    /// [`Error::Gone`] when the device code expired.
    pub async fn device_poll(
        &self,
        actor: &User,
        provider: GitProvider,
        host: Host,
        device_code: &DeviceCode,
    ) -> Result<DevicePolled, Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::GitConnect, &Resource::Admin)
            .await?;
        match self
            .ports
            .git()
            .device_poll(provider, &host, device_code)
            .await?
        {
            DeviceGrant::Pending => Ok(DevicePolled::Pending),
            DeviceGrant::SlowDown { interval } => Ok(DevicePolled::SlowDown { interval }),
            DeviceGrant::Granted { token, expires_in } => self
                .store(
                    actor,
                    provider,
                    host,
                    Credential {
                        token,
                        kind: CredentialKind::Oauth,
                        expires_in,
                    },
                )
                .await
                .map(DevicePolled::Connected),
        }
    }

    /// What `actor`'s connection on `host` lets a `git` child do: the
    /// login and the unsealed token, opened as every service of the core
    /// opens a connection (an expired one is refused); `None` when there
    /// is none. Local mode's `git` children clone and push with it.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when the connection expired; whatever the
    /// store or the secrets port report.
    pub async fn access(&self, actor: &User, host: &Host) -> Result<Option<GitAccess>, Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::GitList, &Resource::Admin)
            .await?;
        let Some(connection) = self.ports.store().git_connection(actor.id, host).await? else {
            return Ok(None);
        };
        open_connection(self.ports.as_ref(), &connection).map(Some)
    }

    /// Deletes `actor`'s connection on `host`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] naming the host when there is none; whatever the
    /// store reports.
    pub async fn disconnect(&self, actor: &User, host: &Host) -> Result<(), Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::GitDisconnect, &Resource::Admin)
            .await?;
        let store = self.ports.store();
        if store.git_connection(actor.id, host).await?.is_none() {
            return Err(Error::not_found(format!("connection to {host}")));
        }
        store.delete_git_connection(actor.id, host).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Login;
    use crate::fakes::World;

    fn code() -> Host {
        Host::new("code.example.org").unwrap()
    }

    fn service(world: &World) -> GitService<crate::fakes::FakePorts> {
        GitService::new(Arc::clone(&world.ports))
    }

    mod connect {
        use super::*;

        // TC: REST-02 (the token is sealed, never stored plain)
        #[tokio::test]
        async fn stores_sealed_token_with_account_when_forge_accepts_it() {
            let world = World::new().await;
            let login = Login::new("owner-on-code").unwrap();
            world.ports.git.accept_token(&code(), "tok", &login);
            let stored = service(&world)
                .connect(
                    &world.owner,
                    GitProvider::Forgejo,
                    code(),
                    SecretValue::new("tok".to_owned()),
                )
                .await
                .unwrap();
            let again = world
                .ports
                .store
                .git_connection(world.owner.id, &code())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    again.account_login,
                    again.credential.as_str() == "tok",
                    world.ports.secrets.decrypt(&again.credential).unwrap(),
                    again.id
                ),
                (login, false, "tok".to_owned(), stored.id)
            );
        }

        // TC: REST-02
        #[tokio::test]
        async fn forbidden_and_nothing_stored_when_forge_rejects_token() {
            let world = World::new().await;
            let error = service(&world)
                .connect(
                    &world.owner,
                    GitProvider::Forgejo,
                    code(),
                    SecretValue::new("nope".to_owned()),
                )
                .await
                .unwrap_err();
            let stored = world
                .ports
                .store
                .git_connection(world.owner.id, &code())
                .await
                .unwrap();
            assert!(
                matches!(error, Error::Forbidden(_)) && stored.is_none(),
                "{error:?} {stored:?}"
            );
        }

        // TC: GIT-02 (GitHub is a `git.*` module like Forgejo, so its token goes to the `Git` port, which answers `ModuleRequired` itself when `git.github` is not installed)
        #[tokio::test]
        async fn stores_github_connection_when_forge_accepts_token() {
            let world = World::new().await;
            let github = Host::new("github.com").unwrap();
            world
                .ports
                .git
                .accept_token(&github, "ghp_token", &Login::new("alice").unwrap());
            let stored = service(&world)
                .connect(
                    &world.owner,
                    GitProvider::Github,
                    github,
                    SecretValue::new("ghp_token".to_owned()),
                )
                .await
                .unwrap();
            assert_eq!(
                (
                    stored.provider,
                    stored.host.as_str(),
                    stored.account_login.as_str(),
                    stored.kind
                ),
                (
                    GitProvider::Github,
                    "github.com",
                    "alice",
                    CredentialKind::Token
                )
            );
        }
    }

    mod device_poll {
        use super::*;

        fn github() -> Host {
            Host::new("github.com").unwrap()
        }

        /// `github.com` offering a device flow whose polls answer `grants`.
        fn offering(world: &World, grants: Vec<DeviceGrant>) {
            world.ports.git.offer_device_flow(
                &github(),
                DeviceStart {
                    device_code: DeviceCode::new("dc").unwrap(),
                    user_code: "WDJB-MJHT".to_owned(),
                    verification_uri: "https://github.com/login/device".to_owned(),
                    interval: 5,
                    expires_in: 900,
                },
                grants,
            );
        }

        async fn poll(world: &World) -> Result<DevicePolled, Error> {
            service(world)
                .device_poll(
                    &world.owner,
                    GitProvider::Github,
                    github(),
                    &DeviceCode::new("dc").unwrap(),
                )
                .await
        }

        // TC: GIT-01 (pending: nothing is checked nor stored)
        #[tokio::test]
        async fn pending_and_nothing_stored_when_person_has_not_confirmed() {
            let world = World::new().await;
            offering(&world, vec![DeviceGrant::Pending]);
            let polled = poll(&world).await.unwrap();
            let stored = world
                .ports
                .store
                .git_connection(world.owner.id, &github())
                .await
                .unwrap();
            assert_eq!((polled, stored), (DevicePolled::Pending, None));
        }

        // TC: GIT-01 (RFC 8628 3.5 `slow_down`: the forge's interval is
        // passed on, nothing is stored)
        #[tokio::test]
        async fn slow_down_with_forge_interval_when_forge_asks_to_slow_down() {
            let world = World::new().await;
            offering(&world, vec![DeviceGrant::SlowDown { interval: Some(10) }]);
            let polled = poll(&world).await.unwrap();
            let stored = world
                .ports
                .store
                .git_connection(world.owner.id, &github())
                .await
                .unwrap();
            assert_eq!(
                (polled, stored),
                (DevicePolled::SlowDown { interval: Some(10) }, None)
            );
        }

        // TC: GIT-01 (the granted token is stored sealed, kind `oauth`)
        #[tokio::test]
        async fn stores_oauth_connection_when_person_confirmed() {
            let world = World::new().await;
            offering(
                &world,
                vec![DeviceGrant::Granted {
                    token: SecretValue::new("gho_device".to_owned()),
                    expires_in: None,
                }],
            );
            world
                .ports
                .git
                .accept_token(&github(), "gho_device", &Login::new("alice").unwrap());
            let DevicePolled::Connected(polled) = poll(&world).await.unwrap() else {
                panic!("expected the stored connection");
            };
            let stored = world
                .ports
                .store
                .git_connection(world.owner.id, &github())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    polled.kind,
                    stored.account_login.as_str(),
                    world.ports.secrets.decrypt(&stored.credential).unwrap(),
                ),
                (CredentialKind::Oauth, "alice", "gho_device".to_owned())
            );
        }

        // TC: GIT-01 (a token the forge grants for `expires_in` seconds, as GitLab's, is stored with its expiry; one without a lifetime has none)
        #[tokio::test]
        async fn stores_expiry_of_granted_token_when_forge_names_its_lifetime() {
            let world = World::new().await;
            offering(
                &world,
                vec![DeviceGrant::Granted {
                    token: SecretValue::new("gho_device".to_owned()),
                    expires_in: Some(7200),
                }],
            );
            world
                .ports
                .git
                .accept_token(&github(), "gho_device", &Login::new("alice").unwrap());
            let _polled = poll(&world).await.unwrap();
            let stored = world
                .ports
                .store
                .git_connection(world.owner.id, &github())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                stored.expires_at,
                Some(stored.created_at.saturating_add(Duration::seconds(7200)))
            );
        }

        // TC: GIT-02 (the granted token is checked like a pasted one)
        #[tokio::test]
        async fn forbidden_and_nothing_stored_when_forge_rejects_granted_token() {
            let world = World::new().await;
            offering(
                &world,
                vec![DeviceGrant::Granted {
                    token: SecretValue::new("gho_device".to_owned()),
                    expires_in: None,
                }],
            );
            let error = poll(&world).await.unwrap_err();
            let stored = world
                .ports
                .store
                .git_connection(world.owner.id, &github())
                .await
                .unwrap();
            assert!(
                matches!(error, Error::Forbidden(_)) && stored.is_none(),
                "{error:?} {stored:?}"
            );
        }
    }

    mod access {
        use super::*;

        fn forgejo() -> Host {
            Host::new(World::GIT_HOST).unwrap()
        }

        // TC: LCL-03 (local mode clones and pushes with the actor's own
        // connection, opened as the core opens it)
        #[tokio::test]
        async fn opens_token_of_connection_when_actor_is_connected() {
            let world = World::new().await;
            let access = service(&world)
                .access(&world.owner, &forgejo())
                .await
                .unwrap()
                .map(|access| (access.host, access.token.expose().to_owned()));
            assert_eq!(access, Some((forgejo(), World::OWNER_TOKEN.to_owned())));
        }

        // TC: LCL-04 (no connection: nothing to push with)
        #[tokio::test]
        async fn none_when_actor_has_no_connection_on_host() {
            let world = World::new().await;
            let access = service(&world).access(&world.owner, &code()).await;
            assert!(matches!(access, Ok(None)), "{access:?}");
        }
    }
}
