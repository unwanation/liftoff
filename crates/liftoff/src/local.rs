//! Local mode: what the CLI does when no server is chosen.
//!
//! The server of a run is `--server`, else `.liftoff`, else the config's
//! default, else none: then the CLI is local. A `.liftoff` (or
//! `--server`) naming a server the config does not have is a config error
//! (exit 78, LCL-08), never local mode.
//!
//! Locally the CLI runs the core services over its own adapters
//! ([`LocalPorts`]), each in the config home (`~/.config/liftoff/`, the
//! directory of the config file, `0700`):
//!
//! - the secrets in `secrets.db`, sealed with the core's `crypto` under the
//!   key in `key` (created `0600`; `lof doctor` warns when it is wider,
//!   LCL-02);
//! - the forge connections, their tokens sealed the same way (LCL-01),
//!   and the installed modules, in the same file;
//! - the `git.*` modules in the CLI's own module host ([`cli_host`]): the
//!   components the registry client downloads to `modules/` (`lof module
//!   install`, or `lof git connect` for the forge's module), their tables
//!   in `modules.db` ([`LocalModuleStore`]), no scheduler and no
//!   `core-call` (LCL-07).
//!
//! [`Local`] is what a run needs for that besides the config path: where
//! the module registry is and the key its releases are signed with, and
//! the environment of the `git` children.

mod files;
mod git;
mod host;
mod ports;
mod secrets;
mod store;
pub(crate) mod view;

#[cfg(feature = "test-support")]
use core::net::SocketAddr;
use std::sync::Arc;

use liftoff_core::domain::{Host, User};
use liftoff_core::module::ModuleGit;
pub use liftoff_core::module::RegistryKey;
use liftoff_core::ports::GitAccess;
use liftoff_core::service::{GitService, ModuleService, Settings};
use liftoff_wasm::HttpModuleSource;

pub(crate) use files::{Home, inside};
pub(crate) use git::{ForgeAuth, GitRunner, clone_url};
pub use host::{CliHost, LocalCoreCalls, MODULE_LIMITS, cli_host, local_host};
pub use ports::{LOCAL_BASE, LocalGit, LocalModuleSet, LocalPorts, LocalRuntime, Offline};
pub use secrets::{LocalRevision, LocalSecret, LocalSecretStore, local_user};
pub use store::LocalModuleStore;

use crate::error::CliError;

/// The module registry of a release build.
pub const DEFAULT_REGISTRY: &str =
    "https://api.github.com/repos/unwanation/liftoff-modules/releases";

/// The public key the registry's releases are signed with: the one the
/// daemon pins too (`liftoff_core::module::REGISTRY_PUBLIC_KEY`).
pub const REGISTRY_PUBLIC_KEY: &str = liftoff_core::module::REGISTRY_PUBLIC_KEY;

/// What local mode needs besides the config path.
#[derive(Debug, Clone)]
pub struct Local {
    /// The module registry: its releases API or its `index.json`.
    registry: String,
    /// The key its releases must be signed with.
    registry_key: RegistryKey,
    /// Variables set for every `git` child on top of the CLI's own
    /// environment.
    git_env: Vec<(String, String)>,
    /// `https://<authority>` of the modules' `wasi:http` sent to a
    /// loopback address instead.
    #[cfg(feature = "test-support")]
    https_routes: Vec<(String, SocketAddr)>,
}

impl Local {
    /// Local mode over the registry at `registry`, whose releases are
    /// signed with `registry_key`.
    #[must_use]
    pub fn new(registry: impl Into<String>, registry_key: RegistryKey) -> Self {
        Self {
            registry: registry.into(),
            registry_key,
            git_env: Vec::new(),
            #[cfg(feature = "test-support")]
            https_routes: Vec::new(),
        }
    }

    /// The registry of a release build and its pinned key.
    #[must_use]
    pub fn release() -> Self {
        Self::new(DEFAULT_REGISTRY, RegistryKey::new(REGISTRY_PUBLIC_KEY))
    }

    /// Sets `key` to `value` in the environment of every `git` child (a
    /// `GIT_CONFIG_GLOBAL` of its own, for one).
    #[must_use]
    pub fn with_git_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.git_env.push((key.into(), value.into()));
        self
    }

    /// Sends the modules' `wasi:http` requests to `https://<authority>`
    /// over plain HTTP to `to` instead (feature `test-support`; the
    /// module host's `routing_https_to`).
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn routing_https_to(mut self, authority: &str, to: SocketAddr) -> Self {
        self.https_routes.push((authority.to_owned(), to));
        self
    }

    /// How the `git` children run.
    pub(crate) fn git(&self) -> GitRunner {
        GitRunner::new(self.git_env.clone())
    }

    /// The host of local mode over `store`, keeping compiled components in
    /// `cache`.
    fn host(&self, store: LocalModuleStore, cache: &std::path::Path) -> Result<CliHost, CliError> {
        let host = local_host(store, cache)
            .map_err(|error| CliError::Local(format!("the module host: {error}")))?;
        #[cfg(feature = "test-support")]
        let host = self
            .https_routes
            .iter()
            .fold(host, |host, (authority, to)| {
                host.routing_https_to(authority, *to)
            });
        Ok(host)
    }
}

/// The core services of local mode over [`LocalPorts`], as the local user.
pub(crate) struct LocalMode {
    pub(crate) ports: Arc<LocalPorts>,
    pub(crate) settings: Arc<Settings>,
    pub(crate) actor: User,
}

impl LocalMode {
    /// Opens the local store, the module host and the registry client of
    /// the config home `home`, and loads every installed module.
    pub(crate) async fn open(home: &Home, local: &Local) -> Result<Self, CliError> {
        home.prepare()?;
        let store = LocalSecretStore::open(home)?;
        let modules_store = LocalModuleStore::open(home.modules_db())?;
        let host = local.host(modules_store, &home.cache())?;
        let runtime = LocalRuntime::new(host, home.modules());
        let git = ModuleGit::new(Arc::clone(runtime.host().registry()), runtime.set().clone());
        let registry = HttpModuleSource::new(local.registry.clone())?;
        let ports = Arc::new(LocalPorts {
            secrets: store.key(),
            store,
            git,
            registry,
            modules: runtime,
        });
        let settings = Arc::new(ports::settings(local.registry_key.clone())?);
        ModuleService::new(Arc::clone(&ports), Arc::clone(&settings))
            .restore()
            .await?;
        Ok(Self {
            ports,
            settings,
            actor: local_user(),
        })
    }

    /// The local connection on `host`, opened by the core
    /// ([`GitService::access`]) for the `git` children that clone and
    /// push; `None` when there is none.
    pub(crate) async fn forge_access(&self, host: &Host) -> Result<Option<GitAccess>, CliError> {
        Ok(GitService::new(Arc::clone(&self.ports))
            .access(&self.actor, host)
            .await?)
    }

    /// The module service of local mode.
    pub(crate) fn modules(&self) -> ModuleService<LocalPorts> {
        ModuleService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
    }
}
