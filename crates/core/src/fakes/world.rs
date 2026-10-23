//! [`World`]: the shared fixture of the test-case catalogue.

use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::time::Duration;
use std::sync::Arc;

use super::FakePorts;
use super::log::CallLog;
use super::records::Records;
use crate::domain::{
    AcmeEmail, Binding, ComposeProject, Deploy, DeployStatus, DeployTokenSecret, Domain, EnvState,
    Environment, GateSecret, GitConnection, GitProvider, Host, HostPort, Label, Project, Secret,
    SecretScope, SecretValue, SessionSecret, SetupSecret, User, UserRole,
};
use crate::error::Error;
use crate::module::RegistryKey;
use crate::ports::{GitAccess, ProjectFile, ProjectFiles as _, RepoRole, Secrets as _, Store as _};
use crate::service::{
    Access, ArtifactSettings, ArtifactsService, AuthService, BindingService, CreateService,
    DeployService, DomainService, EnvironmentService, LinksService, ModuleService, ModuleSettings,
    ProjectService, ProxySettings, SecretSync, SecretsService, Settings, TlsMode, TunnelsService,
    VisibilityService,
};

/// The world every suite starts from:
///
/// - user `owner`: admin, source `local`, with a Forgejo token connection
///   on `git.example.com` whose account `owner` is admin of `owner/shop`;
/// - project `shop` on `git.example.com` (a host the fake forge serves as
///   Forgejo), repository `owner/shop`;
/// - environments `prod` (pinned to `main` by `owner`) and `qa` (pinned
///   to `dev` by `owner`), both compose and empty;
/// - managed domain `shop.apps.test` bound to `web:3000` on `prod` at host
///   port 20000;
/// - secrets of `shop`: `DATABASE_URL = postgres://db/shop`,
///   `LOG_LEVEL = info`; of `qa`: `DATABASE_URL = postgres://db/shop_qa`
///   (values sealed with the fake `Secrets`).
///
/// Its [`Settings`]: base `apps.test`, public IP `203.0.113.10`, reserved
/// label `liftoff`, host ports 20000-29999, wait timeout 120 s, deploy
/// history 20, a lost tunnel offline after 30 s; API site
/// `liftoff.test` on `127.0.0.1:7000`, TLS `acme` with the account
/// `ops@example.com`, the gate listener on `127.0.0.1:7001`; modules: the fake registry's key pinned, `liftoffd`
/// [`World::LIFTOFFD_VERSION`]; no template repositories (a catalogue
/// test sets its own `templates`); artifacts: TTL 5 hours, 16 MiB, 50
/// versions, served under `https://liftoff.test/a/`.
///
/// The call log is empty when [`World::new`] returns. The `add_*`
/// helpers write through the fakes (and so into the log); call
/// `world.log().clear()` after arranging and before acting.
#[derive(Debug, Clone)]
pub struct World {
    /// Every fake.
    pub ports: Arc<FakePorts>,
    /// Record builder sharing the fakes' rng and clock.
    pub records: Records,
    /// The single admin.
    pub owner: User,
    /// `owner`'s connection on `git.example.com`.
    pub connection: GitConnection,
    /// Project `shop`.
    pub shop: Project,
    /// `shop/prod`, pinned to `main`.
    pub prod: Environment,
    /// `shop/qa`, pinned to `dev`.
    pub qa: Environment,
    /// `shop.apps.test`.
    pub domain: Domain,
    /// `shop.apps.test` → `web:3000` on `prod`.
    pub binding: Binding,
    /// The project secrets of `shop`, by key.
    pub project_secrets: Vec<Secret>,
    /// The environment secrets of `qa`, by key.
    pub qa_secrets: Vec<Secret>,
    /// The installation's configuration.
    pub settings: Arc<Settings>,
}

impl World {
    /// The base domain of the world's installation.
    pub const BASE: &'static str = "apps.test";
    /// The forge host of `shop`.
    pub const GIT_HOST: &'static str = "git.example.com";
    /// The plaintext of `owner`'s Forgejo token.
    pub const OWNER_TOKEN: &'static str = "forgejo-token-of-owner";
    /// The public IP of the world's installation.
    pub const PUBLIC_IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 10);
    /// The reserved labels of the world's installation.
    pub const RESERVED: &'static [&'static str] = &["liftoff"];
    /// The host of the world's `[api] public_url`.
    pub const API_HOST: &'static str = "liftoff.test";
    /// The world's `[api] bind`.
    pub const API_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7000);
    /// The world's `[gate] bind`.
    pub const GATE_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7001);
    /// The world's `[functions] bind`.
    pub const FUNCTIONS_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7003);
    /// The world's `[tunnel] bind`.
    pub const TUNNEL_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7004);
    /// The world's `[tunnel] offline_after`.
    pub const TUNNEL_OFFLINE_AFTER: Duration = Duration::from_secs(30);
    /// The world's `[proxy] acme_email`.
    pub const ACME_EMAIL: &'static str = "ops@example.com";
    /// The world's `[artifacts] ttl`.
    pub const ARTIFACT_TTL: Duration = Duration::from_hours(5);
    /// The world's `[artifacts] max_size_mb`, 16 MiB, in bytes.
    pub const ARTIFACT_MAX_BYTES: u64 = 16 * 1024 * 1024;
    /// The world's `[artifacts] versions`.
    pub const ARTIFACT_VERSIONS: usize = 50;
    /// The version of the world's `liftoffd` (modules' `liftoffd_min`).
    pub const LIFTOFFD_VERSION: &'static str = "2.0.0";

    /// Builds the world.
    ///
    /// # Panics
    ///
    /// Only if the fixture itself is broken.
    #[expect(
        clippy::expect_used,
        reason = "a broken fixture is a bug in the fixture; every test would fail"
    )]
    pub async fn new() -> Self {
        Self::try_new().await.expect("the shared test world builds")
    }

    async fn try_new() -> Result<Self, Error> {
        let ports = Arc::new(FakePorts::new());
        let records = Records::new(ports.rng.clone(), ports.clock.clone());
        let store = &ports.store;

        let owner = records.user("owner", UserRole::Admin)?;
        store.insert_user(&owner).await?;
        let connection = records.git_connection(
            &owner,
            Self::GIT_HOST,
            ports.secrets.encrypt(Self::OWNER_TOKEN)?,
        )?;
        store.put_git_connection(&connection).await?;

        let shop = records.project("shop", Self::GIT_HOST, "owner/shop", &owner)?;
        store.insert_project(&shop).await?;
        let mut prod = records.environment(&shop, "prod")?;
        prod.pin = Some(Records::pin("main", &owner)?);
        let mut qa = records.environment(&shop, "qa")?;
        qa.pin = Some(Records::pin("dev", &owner)?);
        store.insert_environment(&prod).await?;
        store.insert_environment(&qa).await?;

        let domain = records.domain(&shop, "shop.apps.test")?;
        store.insert_domain(&domain).await?;
        let binding = records.binding(&prod, &domain, "web", 3000, 20_000)?;
        store.insert_binding(&binding).await?;

        let mut project_secrets = Vec::new();
        for (key, value) in [
            ("DATABASE_URL", "postgres://db/shop"),
            ("LOG_LEVEL", "info"),
        ] {
            let secret = records.secret(
                SecretScope::Project(shop.id),
                key,
                ports.secrets.encrypt(value)?,
                &owner,
            )?;
            store.put_secret(&secret).await?;
            project_secrets.push(secret);
        }
        let qa_secret = records.secret(
            SecretScope::Environment(qa.id),
            "DATABASE_URL",
            ports.secrets.encrypt("postgres://db/shop_qa")?,
            &owner,
        )?;
        store.put_secret(&qa_secret).await?;

        ports.git.grant(&owner.login, &shop.repo, RepoRole::Admin);
        ports.git.serve(&shop.git_host, connection.provider);

        let settings = Arc::new(Self::settings(&ports.registry.public_key())?);

        let world = Self {
            ports,
            records,
            owner,
            connection,
            shop,
            prod,
            qa,
            domain,
            binding,
            project_secrets,
            qa_secrets: vec![qa_secret],
            settings,
        };
        world.log().clear();
        Ok(world)
    }

    /// The installation's configuration (see the type documentation).
    fn settings(registry_key: &str) -> Result<Settings, Error> {
        Ok(Settings {
            base: Host::new(Self::BASE)?,
            public_ip: Self::PUBLIC_IP,
            reserved: Self::RESERVED
                .iter()
                .map(|label| Label::new(label))
                .collect::<Result<_, _>>()?,
            port_range: HostPort::full_range(),
            wait_timeout: Duration::from_secs(120),
            deploy_history: 20,
            tunnel_offline_after: Self::TUNNEL_OFFLINE_AFTER,
            proxy: ProxySettings {
                api_host: Host::new(Self::API_HOST)?,
                api_bind: Self::API_BIND,
                tls: TlsMode::Acme {
                    email: AcmeEmail::new(Self::ACME_EMAIL)?,
                },
                gate_bind: Self::GATE_BIND,
                functions_bind: Self::FUNCTIONS_BIND,
                tunnel_bind: Self::TUNNEL_BIND,
            },
            modules: ModuleSettings {
                registry_key: RegistryKey::new(registry_key),
                liftoffd_version: Self::LIFTOFFD_VERSION.to_owned(),
            },
            templates: Vec::new(),
            artifacts: ArtifactSettings {
                ttl: Self::ARTIFACT_TTL,
                max_size_bytes: Self::ARTIFACT_MAX_BYTES,
                versions: Self::ARTIFACT_VERSIONS,
                host: None,
            },
        })
    }

    /// The shared call log.
    #[must_use]
    pub fn log(&self) -> &CallLog {
        &self.ports.log
    }

    /// `owner`'s connection as the [`Git`](crate::ports::Git) port sees it.
    #[must_use]
    pub fn owner_access(&self) -> GitAccess {
        GitAccess {
            provider: GitProvider::Forgejo,
            host: self.connection.host.clone(),
            login: self.connection.account_login.clone(),
            token: SecretValue::new(Self::OWNER_TOKEN.to_owned()),
        }
    }

    /// Stores a new user.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a bad login; whatever the store reports.
    pub async fn add_user(&self, login: &str, role: UserRole) -> Result<User, Error> {
        let user = self.records.user(login, role)?;
        self.ports.store.insert_user(&user).await?;
        Ok(user)
    }

    /// Stores a new `user` named `login` with a Forgejo token connection
    /// on [`World::GIT_HOST`] whose account is `login` (no role on any
    /// repository until [`FakeGit::grant`](super::FakeGit::grant)).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a bad login; whatever the store or the
    /// secrets report.
    pub async fn add_connected_user(&self, login: &str) -> Result<User, Error> {
        let user = self.add_user(login, UserRole::User).await?;
        let sealed = self
            .ports
            .secrets
            .encrypt(&format!("forgejo-token-of-{login}"))?;
        let connection = self.records.git_connection(&user, Self::GIT_HOST, sealed)?;
        self.ports.store.put_git_connection(&connection).await?;
        Ok(user)
    }

    /// [`ProjectService`] over the world's fakes and settings.
    #[must_use]
    pub fn projects(&self) -> ProjectService<FakePorts> {
        ProjectService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// [`EnvironmentService`] over the world's fakes and settings.
    #[must_use]
    pub fn environments(&self) -> EnvironmentService<FakePorts> {
        EnvironmentService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// [`DomainService`] over the world's fakes and settings.
    #[must_use]
    pub fn domains(&self) -> DomainService<FakePorts> {
        DomainService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// [`BindingService`] over the world's fakes and settings.
    #[must_use]
    pub fn bindings(&self) -> BindingService<FakePorts> {
        BindingService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// [`DeployService`] over the world's fakes and settings.
    #[must_use]
    pub fn deploys(&self) -> DeployService<FakePorts> {
        DeployService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// Stores a live deploy token of `project`, issued by `owner`; returns
    /// its secret (what CI sends as the bearer).
    ///
    /// # Errors
    ///
    /// Whatever the store reports (a second live token is a conflict).
    pub async fn deploy_token(&self, project: &Project) -> Result<DeployTokenSecret, Error> {
        let (token, secret) = self.records.deploy_token(project, &self.owner);
        self.ports.store.insert_deploy_token(&token).await?;
        Ok(secret)
    }

    /// [`AuthService`] over the world's fakes.
    #[must_use]
    pub fn auth(&self) -> AuthService<FakePorts> {
        AuthService::new(Arc::clone(&self.ports))
    }

    /// [`SecretsService`] over the world's fakes.
    #[must_use]
    pub fn secrets(&self) -> SecretsService<FakePorts> {
        SecretsService::new(Arc::clone(&self.ports))
    }

    /// [`SecretSync`] over the world's fakes.
    #[must_use]
    pub fn secret_sync(&self) -> SecretSync<FakePorts> {
        SecretSync::new(Arc::clone(&self.ports))
    }

    /// [`CreateService`] over the world's fakes and settings.
    #[must_use]
    pub fn create(&self) -> CreateService<FakePorts> {
        CreateService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// The module service over the world.
    #[must_use]
    pub fn modules(&self) -> ModuleService<FakePorts> {
        ModuleService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// Stores project `name` of `repo` on [`World::GIT_HOST`], created by
    /// `owner`, whose connection is made admin of `repo`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a bad name or repository; whatever the
    /// store reports.
    pub async fn add_project(&self, name: &str, repo: &str) -> Result<Project, Error> {
        let project = self
            .records
            .project(name, Self::GIT_HOST, repo, &self.owner)?;
        self.ports.store.insert_project(&project).await?;
        self.ports
            .git
            .grant(&self.owner.login, &project.repo, RepoRole::Admin);
        Ok(project)
    }

    /// Stores an empty, unpinned compose environment `name` of `project`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a bad name; whatever the store reports.
    pub async fn add_environment(
        &self,
        project: &Project,
        name: &str,
    ) -> Result<Environment, Error> {
        let environment = self.records.environment(project, name)?;
        self.ports.store.insert_environment(&environment).await?;
        Ok(environment)
    }

    /// Stores an unbound managed domain `host` of `project`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a bad host; whatever the store reports.
    pub async fn add_domain(&self, project: &Project, host: &str) -> Result<Domain, Error> {
        let domain = self.records.domain(project, host)?;
        self.ports.store.insert_domain(&domain).await?;
        Ok(domain)
    }

    /// Stores deploy `number` of `environment` in `status`, by `owner`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for number zero; whatever the store reports.
    pub async fn add_deploy(
        &self,
        environment: &Environment,
        number: u32,
        status: DeployStatus,
    ) -> Result<Deploy, Error> {
        let deploy = self
            .records
            .deploy(environment, number, status, &self.owner)?;
        self.ports.store.insert_deploy(&deploy).await?;
        Ok(deploy)
    }

    /// Makes `environment` look deployed: a succeeded deploy (one `web`
    /// service, `nginx:alpine`) becomes its current deploy, its
    /// `compose.yaml` is on disk and its state is `state`. Returns the
    /// environment as stored.
    ///
    /// # Errors
    ///
    /// Whatever the store or the files report.
    pub async fn deployed(
        &self,
        environment: &Environment,
        state: EnvState,
    ) -> Result<Environment, Error> {
        let store = &self.ports.store;
        let number = store.next_deploy_number(environment.id).await?;
        let deploy = self
            .add_deploy(environment, number.get(), DeployStatus::Succeeded)
            .await?;
        let project = store
            .project(environment.project_id)
            .await?
            .ok_or_else(|| Error::internal_msg("the environment's project is stored"))?;
        self.ports
            .files
            .write(
                &ComposeProject::new(&project.name, &environment.name),
                ProjectFile::Compose,
                &deploy.compose,
            )
            .await?;
        let mut deployed = environment.clone();
        deployed.state = state;
        deployed.current_deploy_id = Some(deploy.id);
        store.update_environment(&deployed).await?;
        Ok(deployed)
    }

    /// Stores a single-use login token of `user` as the live setup token
    /// (what `liftoffd setup` leaves behind); returns its secret.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn setup_token(&self, user: &User) -> Result<SetupSecret, Error> {
        let (token, secret) = self.records.setup_token(user);
        self.ports.store.put_setup_token(&token).await?;
        Ok(secret)
    }

    /// [`VisibilityService`] over the world's fakes and settings.
    #[must_use]
    pub fn visibility(&self) -> VisibilityService<FakePorts> {
        VisibilityService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// [`Access`] over the world's fakes and settings.
    #[must_use]
    pub fn access(&self) -> Access<FakePorts> {
        Access::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// [`ArtifactsService`] over the world's fakes and settings.
    #[must_use]
    pub fn artifacts(&self) -> ArtifactsService<FakePorts> {
        ArtifactsService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// [`LinksService`] over the world's fakes and settings.
    #[must_use]
    pub fn links(&self) -> LinksService<FakePorts> {
        LinksService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// Stores an empty, private tunnel environment `name` of `project`
    /// owned by `owner`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a bad name; whatever the store reports.
    pub async fn add_tunnel(
        &self,
        project: &Project,
        name: &str,
        owner: &User,
    ) -> Result<Environment, Error> {
        let environment = self.records.tunnel(project, name, owner)?;
        self.ports.store.insert_environment(&environment).await?;
        Ok(environment)
    }

    /// Stores a `browser` session of `user` (24 hours); returns its
    /// secret, the value of the cookie.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn browser_login(&self, user: &User) -> Result<SessionSecret, Error> {
        let (session, secret) = self.records.browser_session(user);
        self.ports.store.insert_session(&session).await?;
        Ok(secret)
    }

    /// Stores a `browser` session of `user` (24 hours) and the session on
    /// `host` handed over from it; returns the latter's secret, the value
    /// of the cookie on `host`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a bad host; whatever the store reports.
    pub async fn host_login(&self, user: &User, host: &str) -> Result<GateSecret, Error> {
        let (session, _api_cookie) = self.records.browser_session(user);
        let (gate, secret) = self.records.host_session(&session, host)?;
        self.ports.store.insert_session(&session).await?;
        self.ports.store.insert_gate_session(&gate).await?;
        Ok(secret)
    }

    /// Stores the address `host` of `environment` (a function's or a
    /// tunnel's): a managed domain of `project` bound to it. The binding's service and ports mean nothing for
    /// these kinds; one such address per world (its host port is fixed).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a bad host; whatever the store reports.
    pub async fn add_address(
        &self,
        project: &Project,
        environment: &Environment,
        host: &str,
    ) -> Result<Domain, Error> {
        let domain = self.add_domain(project, host).await?;
        let binding = self
            .records
            .binding(environment, &domain, "web", 80, 20_100)?;
        self.ports.store.insert_binding(&binding).await?;
        Ok(domain)
    }

    /// [`TunnelsService`] over the world's fakes and settings. Each call is a service of its own: a test that
    /// loses a connection and expires it keeps one.
    #[must_use]
    pub fn tunnels(&self) -> TunnelsService<FakePorts> {
        TunnelsService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }

    /// Stores a `cli` session of `user`; returns its secret.
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn login(&self, user: &User) -> Result<SessionSecret, Error> {
        let (session, secret) = self.records.session(user);
        self.ports.store.insert_session(&session).await?;
        Ok(secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Branch, EnvKind, EnvState, Host, Login, ProjectName, RepoRef, SecretKey, UserSource,
    };
    use crate::ports::Git as _;

    // Fixture: World v2; its criterion is that
    // `World::new()` builds.
    mod new {
        use super::*;

        #[tokio::test]
        async fn stores_owner_as_local_admin_when_built() {
            let world = World::new().await;
            let owner = world
                .ports
                .store
                .user_by_login(&Login::new("owner").unwrap())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (owner.role, owner.source),
                (UserRole::Admin, UserSource::Local)
            );
        }

        #[tokio::test]
        async fn resolves_shop_when_repo_is_owner_shop_on_git_example_com() {
            let world = World::new().await;
            let shop = world
                .ports
                .store
                .project_by_repo(
                    &Host::new("git.example.com").unwrap(),
                    &RepoRef::new("owner/shop").unwrap(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(shop.name, ProjectName::new("shop").unwrap());
        }

        #[tokio::test]
        async fn has_prod_on_main_and_qa_on_dev_when_built() {
            let world = World::new().await;
            let environments: Vec<_> = world
                .ports
                .store
                .environments_of_project(world.shop.id)
                .await
                .unwrap()
                .into_iter()
                .map(|e| {
                    (
                        e.name.to_string(),
                        e.kind,
                        e.state,
                        e.pin.map(|pin| (pin.branch, pin.pinned_by)),
                    )
                })
                .collect();
            let by_owner = |branch: &str| Some((Branch::new(branch).unwrap(), world.owner.id));
            assert_eq!(
                environments,
                vec![
                    (
                        "prod".to_owned(),
                        EnvKind::Compose,
                        EnvState::Empty,
                        by_owner("main")
                    ),
                    (
                        "qa".to_owned(),
                        EnvKind::Compose,
                        EnvState::Empty,
                        by_owner("dev")
                    ),
                ]
            );
        }

        #[tokio::test]
        async fn binds_shop_apps_test_to_web_3000_on_prod_when_built() {
            let world = World::new().await;
            let store = &world.ports.store;
            let domain = store
                .domain_by_host(&Host::new("shop.apps.test").unwrap())
                .await
                .unwrap()
                .unwrap();
            let binding = store.binding_by_domain(domain.id).await.unwrap().unwrap();
            assert_eq!(
                (
                    binding.environment_id,
                    binding.service.as_str(),
                    binding.container_port.get()
                ),
                (world.prod.id, "web", 3000)
            );
        }

        #[tokio::test]
        async fn has_secrets_at_project_and_environment_scope_when_built() {
            let world = World::new().await;
            let store = &world.ports.store;
            let keys = |secrets: Vec<Secret>| -> Vec<String> {
                secrets.iter().map(|s| s.key.to_string()).collect()
            };
            let project = store
                .secrets_of_scope(&SecretScope::Project(world.shop.id))
                .await
                .unwrap();
            let qa = store
                .secrets_of_scope(&SecretScope::Environment(world.qa.id))
                .await
                .unwrap();
            assert_eq!(
                (keys(project), keys(qa)),
                (
                    vec!["DATABASE_URL".to_owned(), "LOG_LEVEL".to_owned()],
                    vec!["DATABASE_URL".to_owned()]
                )
            );
        }

        #[tokio::test]
        async fn seals_qa_database_url_when_built() {
            let world = World::new().await;
            let secret = world
                .ports
                .store
                .secret(
                    &SecretScope::Environment(world.qa.id),
                    &SecretKey::new("DATABASE_URL").unwrap(),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                world.ports.secrets.decrypt(&secret.value).unwrap(),
                "postgres://db/shop_qa"
            );
        }

        #[tokio::test]
        async fn connects_owner_as_shop_admin_when_built() {
            let world = World::new().await;
            let stored = world
                .ports
                .store
                .git_connection(world.owner.id, &Host::new("git.example.com").unwrap())
                .await
                .unwrap();
            let role = world
                .ports
                .git
                .repo_role(&world.owner_access(), &world.shop.repo)
                .await
                .unwrap();
            assert_eq!(
                (stored, role),
                (Some(world.connection.clone()), RepoRole::Admin)
            );
        }

        #[tokio::test]
        async fn starts_with_empty_log_when_built() {
            let world = World::new().await;
            assert_eq!(world.log().calls(), vec![]);
        }
    }
}
