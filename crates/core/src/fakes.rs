//! Hand-written fakes of every port, for tests (feature `test-support`).
//!
//! No mocking crates: each fake is a small in-memory implementation that
//! records what it was asked and can be scripted to fail.
//!
//! - **One ordered log.** Every recording fake built from the same
//!   [`CallLog`] appends to it, so a test can assert the order of calls
//!   across systems. [`FakePorts::new`] wires all fakes to one log.
//! - **Scripted failures.** Every fake has `fail_next(op, error)` and
//!   `fail_always(op, error)`, where `op` is the port method name
//!   (`"compose_up"`, `"set_slice_limits"`, ...). Failed calls are still
//!   logged. Docker commands that *run* but exit non-zero are scripted with
//!   [`RecordingDocker::respond_next`] / [`RecordingDocker::fail_next_up`]
//!   instead.
//! - **Concurrency.** [`RecordingDocker::hold`] keeps an op's commands
//!   pending until [`Gate::release`]; `in_flight`/`max_in_flight` count
//!   overlap and `panic_next` makes a command panic (QUE-01..QUE-04).
//! - **State you can inspect.** `RecordingFiles::file`,
//!   `RecordingSystemd::limits`, `RecordingProxy::applied`,
//!   `FakeGit::ci_secret` show the resulting state.
//!
//! - **Store.** [`InMemoryStore`] is held to the same contract as the
//!   SQLite store: [`store_contract`] and
//!   [`store_contract_tests!`](crate::store_contract_tests).
//! - **Blobs.** [`FakeBlobs`] is held to the same contract as the blobs
//!   on disk: [`blobs_contract`] and
//!   [`blobs_contract_tests!`](crate::blobs_contract_tests).
//! - **World.** [`World`] is the shared fixture of the test-case catalogue.
//! - **Real proxy.** [`WithProxy`] swaps a real proxy adapter in for
//!   [`RecordingProxy`] (the adapter's end-to-end tests); [`WithStore`]
//!   swaps a real store in for [`InMemoryStore`]; [`WithHooks`] swaps
//!   other hooks in for [`FakeHooks`] (the module dispatcher over [`FakeModules`]); [`WithGit`] swaps another
//!   forge in for [`FakeGit`] (the `git.*` modules);
//!   [`WithModules`] swaps a real store and module runtime in.
//! - **Modules.** [`FakeModuleSource`] is the registry, serving the
//!   [`FakeRelease`]s published to it signed with its own minisign key;
//!   [`FakeModuleRuntime`] is the module host, whose components are their
//!   manifests' JSON.

mod blobs;
pub mod blobs_contract;
mod clock;
mod docker;
mod functions;
mod git;
mod hooks;
mod log;
mod modules;
mod proxy;
mod records;
mod registry;
mod resolver;
mod runtime;
mod store;
pub mod store_contract;
mod systemd;
mod with_git;
mod with_hooks;
mod with_modules;
mod with_proxy;
mod with_store;
mod world;

use std::sync::Arc;

pub use blobs::FakeBlobs;
pub use clock::{FakeSecrets, FixedClock, RecordingQueue, SeqRng};
pub use docker::{DockerReply, Gate, LineStream, RecordingDocker, RecordingFiles};
pub use functions::{COMPONENT_PREAMBLE, FakeComponentFiles, FakeFunctions};
pub use git::FakeGit;
pub use hooks::FakeHooks;
pub use log::{
    BlobCall, Call, CallLog, DockerCall, Faults, FilesCall, FunctionCall, GitCall, HookCall,
    RegistryCall, ResolveCall, RuntimeCall, SystemdCall,
};
pub use modules::{FakeModules, ModuleCall};
pub use proxy::RecordingProxy;
pub use records::Records;
pub use registry::{FAKE_LIFTOFFD_MIN, FakeModuleSource, FakeRelease, PROBE_MANIFEST, sha256_hex};
pub use resolver::FakeResolver;
pub use runtime::FakeModuleRuntime;
pub use store::InMemoryStore;
pub use systemd::RecordingSystemd;
pub use with_git::WithGit;
pub use with_hooks::WithHooks;
pub use with_modules::WithModules;
pub use with_proxy::WithProxy;
pub use with_store::WithStore;
pub use world::World;

use crate::ports::Ports;

/// Every fake, wired to one shared [`CallLog`].
///
/// Fields are public so a test can script and inspect each fake directly
/// (`ports.docker.fail_next_up(["boom"])`). Clones share all state.
#[derive(Debug, Clone)]
pub struct FakePorts {
    /// The log every recording fake writes to.
    pub log: CallLog,
    /// Every record.
    pub store: InMemoryStore,
    /// `docker compose`.
    pub docker: RecordingDocker,
    /// Compose project files.
    pub files: RecordingFiles,
    /// Slices.
    pub systemd: RecordingSystemd,
    /// The reverse proxy.
    pub proxy: RecordingProxy,
    /// Git forges.
    pub git: FakeGit,
    /// Public DNS.
    pub resolver: FakeResolver,
    /// Clock.
    pub clock: FixedClock,
    /// Ids and secrets.
    pub rng: SeqRng,
    /// Encryption with the master key.
    pub secrets: FakeSecrets,
    /// Deploy queue.
    pub queue: RecordingQueue,
    /// Extension points.
    pub hooks: FakeHooks,
    /// The module registry.
    pub registry: FakeModuleSource,
    /// The module host as installation drives it.
    pub modules: FakeModuleRuntime,
    /// The runtime of functions.
    pub functions: FakeFunctions,
    /// The kept `.wasm` files of function deploys.
    pub components: FakeComponentFiles,
    /// The files of artifact versions.
    pub blobs: FakeBlobs,
}

impl FakePorts {
    /// Empty fakes sharing one log.
    #[must_use]
    pub fn new() -> Self {
        let log = CallLog::new();
        Self {
            store: InMemoryStore::with_log(log.clone()),
            docker: RecordingDocker::with_log(log.clone()),
            files: RecordingFiles::with_log(log.clone()),
            systemd: RecordingSystemd::with_log(log.clone()),
            proxy: RecordingProxy::with_log(log.clone()),
            git: FakeGit::with_log(log.clone()),
            resolver: FakeResolver::with_log(log.clone()),
            clock: FixedClock::default(),
            rng: SeqRng::new(),
            secrets: FakeSecrets::new(),
            queue: RecordingQueue::with_log(log.clone()),
            hooks: FakeHooks::with_log(log.clone()),
            registry: FakeModuleSource::with_log(log.clone()),
            modules: FakeModuleRuntime::with_log(log.clone()),
            functions: FakeFunctions::with_log(log.clone()),
            components: FakeComponentFiles::with_log(log.clone()),
            blobs: FakeBlobs::with_log(log.clone()),
            log,
        }
    }

    /// Wraps the fakes for a service constructor.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl Default for FakePorts {
    fn default() -> Self {
        Self::new()
    }
}

impl Ports for FakePorts {
    type Store = InMemoryStore;
    type Docker = RecordingDocker;
    type Files = RecordingFiles;
    type Systemd = RecordingSystemd;
    type Proxy = RecordingProxy;
    type Git = FakeGit;
    type Resolver = FakeResolver;
    type Clock = FixedClock;
    type Rng = SeqRng;
    type Secrets = FakeSecrets;
    type Queue = RecordingQueue;
    type Hooks = FakeHooks;
    type Registry = FakeModuleSource;
    type Modules = FakeModuleRuntime;
    type Functions = FakeFunctions;
    type Components = FakeComponentFiles;
    type Blobs = FakeBlobs;

    fn store(&self) -> &Self::Store {
        &self.store
    }
    fn docker(&self) -> &Self::Docker {
        &self.docker
    }
    fn files(&self) -> &Self::Files {
        &self.files
    }
    fn systemd(&self) -> &Self::Systemd {
        &self.systemd
    }
    fn proxy(&self) -> &Self::Proxy {
        &self.proxy
    }
    fn git(&self) -> &Self::Git {
        &self.git
    }
    fn resolver(&self) -> &Self::Resolver {
        &self.resolver
    }
    fn clock(&self) -> &Self::Clock {
        &self.clock
    }
    fn rng(&self) -> &Self::Rng {
        &self.rng
    }
    fn secrets(&self) -> &Self::Secrets {
        &self.secrets
    }
    fn queue(&self) -> &Self::Queue {
        &self.queue
    }
    fn hooks(&self) -> &Self::Hooks {
        &self.hooks
    }
    fn registry(&self) -> &Self::Registry {
        &self.registry
    }
    fn modules(&self) -> &Self::Modules {
        &self.modules
    }
    fn functions(&self) -> &Self::Functions {
        &self.functions
    }
    fn components(&self) -> &Self::Components {
        &self.components
    }
    fn blobs(&self) -> &Self::Blobs {
        &self.blobs
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;
    use crate::domain::{ComposeProject, EnvName, ImageTag, Login, ProjectName, RepoRef};
    use crate::error::{Error, System};
    use crate::ports::{Docker as _, Systemd as _};

    fn project() -> ComposeProject {
        ComposeProject::new(
            &ProjectName::new("shop").unwrap(),
            &EnvName::new("qa").unwrap(),
        )
    }

    mod fake_ports {
        use super::*;
        use crate::domain::SliceName;

        #[tokio::test]
        async fn keeps_cross_fake_order_when_log_shared() {
            let ports = FakePorts::new();
            ports.docker.compose_pull(&project()).await.unwrap();
            let slice = SliceName::new(
                &ProjectName::new("shop").unwrap(),
                &EnvName::new("qa").unwrap(),
            );
            ports.systemd.stop_slice(&slice).await.unwrap();
            assert_eq!(
                ports.log.calls(),
                vec![
                    Call::Docker(DockerCall::Pull(project())),
                    Call::Systemd(SystemdCall::StopSlice(slice)),
                ]
            );
        }
    }

    mod recording_docker {
        use super::*;

        // TC: v1 DEP-09 (scripted compose_up failure with stderr)
        #[tokio::test]
        async fn fails_once_then_succeeds_when_fail_next_up() {
            let docker = RecordingDocker::new();
            docker.fail_next_up(["pulling", "container web is unhealthy"]);
            let failed = docker
                .compose_up(&project(), Duration::from_secs(120))
                .await
                .unwrap();
            let next = docker
                .compose_up(&project(), Duration::from_secs(120))
                .await
                .unwrap();
            assert_eq!(
                (failed.success(), failed.last_stderr_line(), next.success()),
                (false, Some("container web is unhealthy"), true)
            );
        }

        #[tokio::test]
        async fn err_when_fail_next() {
            let docker = RecordingDocker::new();
            docker.fail_next("compose_pull", Error::upstream(System::Docker, "no daemon"));
            assert!(
                docker.compose_pull(&project()).await.is_err(),
                "scripted Err"
            );
        }
    }

    mod fake_git {
        use super::*;
        use crate::domain::{GitProvider, Host, SecretKey, SecretValue};
        use crate::ports::{Git as _, GitAccess, RepoRole};

        fn repo() -> RepoRef {
            RepoRef::new("bob/app").unwrap()
        }

        fn access(login: &str) -> GitAccess {
            GitAccess {
                provider: GitProvider::Forgejo,
                host: Host::new("git.example.com").unwrap(),
                login: Login::new(login).unwrap(),
                token: SecretValue::new("token".to_owned()),
            }
        }

        fn key() -> SecretKey {
            SecretKey::new("LIFTOFF_TOKEN").unwrap()
        }

        #[tokio::test]
        async fn follows_role_table_when_asked_for_role() {
            let git = FakeGit::new();
            git.grant(&Login::new("carol").unwrap(), &repo(), RepoRole::Write);
            assert_eq!(
                (
                    git.repo_role(&access("carol"), &repo()).await.unwrap(),
                    git.repo_role(&access("dave"), &repo()).await.unwrap(),
                ),
                (RepoRole::Write, RepoRole::None)
            );
        }

        // TC: v1 PIN-04
        #[tokio::test]
        async fn forbidden_when_put_ci_secret_without_admin() {
            let git = FakeGit::new();
            git.grant(&Login::new("carol").unwrap(), &repo(), RepoRole::Write);
            let result = git
                .put_ci_secret(
                    &access("carol"),
                    &repo(),
                    &key(),
                    &SecretValue::new("v".to_owned()),
                )
                .await;
            assert!(
                matches!(result, Err(Error::Forbidden(_)))
                    && git.ci_secret(&repo(), &key()).is_none(),
                "Write is not enough and nothing is stored, got {result:?}"
            );
        }

        #[tokio::test]
        async fn stores_value_when_put_ci_secret_as_admin() {
            let git = FakeGit::new();
            git.grant(&Login::new("carol").unwrap(), &repo(), RepoRole::Admin);
            git.put_ci_secret(
                &access("carol"),
                &repo(),
                &key(),
                &SecretValue::new("v".to_owned()),
            )
            .await
            .unwrap();
            assert_eq!(
                git.ci_secret(&repo(), &key())
                    .map(|v| v.expose().to_owned()),
                Some("v".to_owned())
            );
        }

        // TC: v1 DEP-08
        #[tokio::test]
        async fn visible_only_when_image_allowed_for_login() {
            let git = FakeGit::new();
            let image = crate::ports::PackageRef {
                owner: "bob".to_owned(),
                name: "app".to_owned(),
                tag: ImageTag::new("sha-1").unwrap(),
            };
            git.allow_image(&Login::new("bob").unwrap(), &image);
            assert_eq!(
                (
                    git.image_exists(&access("bob"), &image).await.unwrap(),
                    git.image_exists(&access("carol"), &image).await.unwrap(),
                ),
                (true, false)
            );
        }

        #[tokio::test]
        async fn fails_once_and_logs_when_fail_next() {
            let git = FakeGit::new();
            git.fail_next("repo_role", Error::upstream(System::Forge, "down"));
            let failed = git.repo_role(&access("carol"), &repo()).await;
            let next = git.repo_role(&access("carol"), &repo()).await;
            assert_eq!(
                (failed.is_err(), next.ok(), git.calls().len()),
                (true, Some(RepoRole::None), 2)
            );
        }
    }

    mod recording_proxy {
        use super::*;
        use crate::domain::Host;
        use crate::ports::Proxy as _;
        use crate::proxy_config::{ApiSite, ProxyConfig, Tls};

        /// A configuration with no domain site.
        fn config() -> ProxyConfig {
            ProxyConfig {
                tls: Tls::Edge {
                    listen: "127.0.0.1:7002".parse().unwrap(),
                },
                api: ApiSite {
                    host: Host::new("liftoff.test").unwrap(),
                    upstream: "127.0.0.1:7000".parse().unwrap(),
                },
                artifacts: None,
                sites: Vec::new(),
            }
        }

        #[tokio::test]
        async fn keeps_last_good_config_when_apply_fails() {
            let proxy = RecordingProxy::new();
            proxy.apply(config()).await.unwrap();
            proxy.fail_next("apply", Error::upstream(System::Caddy, "invalid"));
            let failed = proxy.apply(config()).await;
            assert_eq!(
                (failed.is_err(), proxy.applied(), proxy.apply_count()),
                (true, Some(config()), 2)
            );
        }

        #[tokio::test]
        async fn nothing_applied_when_only_call_failed() {
            let proxy = RecordingProxy::new();
            proxy.fail_next("apply", Error::upstream(System::Caddy, "invalid"));
            let failed = proxy.apply(config()).await;
            assert_eq!(
                (failed.is_err(), proxy.applied(), proxy.apply_count()),
                (true, None, 1)
            );
        }
    }

    mod fake_resolver {
        use std::net::Ipv4Addr;

        use super::*;
        use crate::domain::Host;
        use crate::ports::Resolver as _;

        fn host(name: &str) -> Host {
            Host::new(name).unwrap()
        }

        #[tokio::test]
        async fn answers_table_or_empty_when_queried() {
            let resolver = FakeResolver::new();
            resolver.set_cname(&host("www.shop.test"), vec![host("apps.test")]);
            resolver.set_a(&host("shop.test"), vec![Ipv4Addr::new(203, 0, 113, 7)]);
            assert_eq!(
                (
                    resolver.cname(&host("www.shop.test")).await.unwrap(),
                    resolver.a(&host("shop.test")).await.unwrap(),
                    resolver.a(&host("none.test")).await.unwrap(),
                ),
                (
                    vec![host("apps.test")],
                    vec![Ipv4Addr::new(203, 0, 113, 7)],
                    vec![]
                )
            );
        }

        #[tokio::test]
        async fn empty_when_records_removed() {
            let resolver = FakeResolver::new();
            resolver.set_cname(&host("www.shop.test"), vec![host("apps.test")]);
            resolver.remove(&host("www.shop.test"));
            assert_eq!(
                resolver.cname(&host("www.shop.test")).await.unwrap(),
                Vec::<Host>::new()
            );
        }

        #[tokio::test]
        async fn fails_once_and_logs_when_fail_next() {
            let log = CallLog::default();
            let resolver = FakeResolver::with_log(log.clone());
            resolver.fail_next("cname", Error::internal_msg("SERVFAIL"));
            let failed = resolver.cname(&host("www.shop.test")).await;
            let next = resolver.cname(&host("www.shop.test")).await;
            assert_eq!(
                (failed.is_err(), next.ok(), log.calls().len()),
                (true, Some(vec![]), 2)
            );
        }
    }

    mod fake_hooks {
        use super::*;
        use crate::domain::DeployStatus;
        use crate::ports::{
            AuthResult, HookRequest, HookResult, Hooks as _, ModulePage, PageKind, PageRequest,
        };

        // TC: DEP-08 (the scripted Deny is used once)
        #[tokio::test]
        async fn falls_back_to_continue_when_answer_used() {
            let world = World::new().await;
            let hooks = FakeHooks::new();
            hooks.answer_next("before_deploy", HookResult::Deny("frozen".to_owned()));
            let deploy = world
                .records
                .deploy(&world.prod, 1, DeployStatus::Queued, &world.owner)
                .unwrap();
            let first = hooks.before_deploy(&world.prod, &deploy).await;
            let second = hooks.before_deploy(&world.prod, &deploy).await;
            let after = hooks.after_deploy(&world.prod, &deploy).await;
            assert_eq!(
                (first, second, after, hooks.calls().len()),
                (
                    HookResult::Deny("frozen".to_owned()),
                    HookResult::Continue,
                    HookResult::Continue,
                    3
                )
            );
        }

        // TC: LOGN-05
        #[tokio::test]
        async fn falls_back_to_continue_when_actor_used() {
            let world = World::new().await;
            let hooks = FakeHooks::new();
            hooks.authenticate_next(AuthResult::Actor(world.owner.id));
            let request = HookRequest::default();
            assert_eq!(
                (
                    hooks.authenticate(&request).await,
                    hooks.authenticate(&request).await,
                ),
                (AuthResult::Actor(world.owner.id), AuthResult::Continue)
            );
        }

        #[tokio::test]
        async fn replaces_only_scripted_page_when_rendering() {
            let hooks = FakeHooks::new();
            hooks.render(PageKind::Offline, "<p>dev offline</p>");
            assert_eq!(
                (
                    hooks.render_page(&PageRequest::of(PageKind::Offline)).await,
                    hooks
                        .render_page(&PageRequest::of(PageKind::Unavailable))
                        .await,
                ),
                (
                    Some(ModulePage::Html("<p>dev offline</p>".to_owned())),
                    None
                )
            );
        }
    }
}
