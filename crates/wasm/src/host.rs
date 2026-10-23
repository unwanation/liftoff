//! [`ModuleHost`]: the engine every module of the daemon runs in, and
//! loading a component into a [`Module`].

use core::sync::atomic::{AtomicUsize, Ordering};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use liftoff_core::domain::ModuleName;
use liftoff_core::module::{CoreCalls, ModuleManifest, ModuleRegistry, NoCoreCalls};
use liftoff_core::ports::ModuleStore;
use tokio::sync::Mutex;
use wasmtime::component::{Component, Linker};
use wasmtime::{Cache, Engine};

use crate::engine::{engine, linker};
use crate::error::{ModuleError, load_error};
use crate::http::Resolver;
use crate::imports::HostState;
use crate::limits::Limits;
use crate::log::ModuleLog;
use crate::module::{Inner, Instance, Module, Recipe, Slot};
use crate::scheduler::{self, Schedules, Timer};

/// The configuration of a module (`config-get`): key → value, secrets
/// already decrypted.
pub type ModuleConfig = BTreeMap<String, String>;

/// What the compile cache did for one host so far (MOD-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompileStats {
    /// Components compiled.
    pub compiled: usize,
    /// Components loaded from the cache without compiling.
    pub cached: usize,
}

/// What every module of one host shares: the engine, the linker with the
/// host imports, and the ports the imports reach.
pub(crate) struct Shared<S: 'static, L: 'static, C: 'static> {
    pub(crate) engine: Engine,
    pub(crate) linker: Linker<HostState<S, L, C>>,
    pub(crate) store: Arc<S>,
    pub(crate) log: Arc<L>,
    pub(crate) core: Arc<C>,
    pub(crate) limits: Limits,
    pub(crate) cache_dir: Option<PathBuf>,
    pub(crate) cache: Option<Cache>,
    /// Components compiled or taken from the cache so far.
    pub(crate) loads: AtomicUsize,
}

/// The engine every module of the daemon runs in, with the ports the
/// `host` imports reach: storage `S`, scheduler clock `T`, log `L` and
/// the core services of `core-call` `C`.
///
/// It registers every module it loads in its [`ModuleRegistry`]
/// ([`ModuleHost::registry`]) and reports the outcome of every call
/// there, which disables a module that fails three calls in a row
/// (MOD-09).
pub struct ModuleHost<S: 'static, T, L: 'static, C: 'static = NoCoreCalls> {
    pub(crate) shared: Arc<Shared<S, L, C>>,
    timer: Arc<T>,
    registry: Arc<ModuleRegistry>,
    /// Whether `wasi:http` may reach private addresses (MOD-07).
    private_networks: bool,
    /// How `wasi:http` resolves a host (MOD-07).
    pub(crate) resolver: Resolver,
    /// Whether a loaded module gets a scheduler task (MOD-03).
    scheduler: bool,
}

impl<S: 'static, T, L: 'static, C> Clone for ModuleHost<S, T, L, C> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            timer: Arc::clone(&self.timer),
            registry: Arc::clone(&self.registry),
            private_networks: self.private_networks,
            resolver: self.resolver.clone(),
            scheduler: self.scheduler,
        }
    }
}

impl<S: 'static, T, L: 'static, C> core::fmt::Debug for ModuleHost<S, T, L, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ModuleHost")
            .field("limits", &self.shared.limits)
            .field("cache_dir", &self.shared.cache_dir)
            .field("private_networks", &self.private_networks)
            .field("scheduler", &self.scheduler)
            .finish_non_exhaustive()
    }
}

impl<S: ModuleStore + 'static, T: Timer, L: ModuleLog> ModuleHost<S, T, L> {
    /// A host whose modules store through `store`, schedule on `timer`
    /// and log to `log`, each call within `limits`, registered in a
    /// registry of its own ([`ModuleHost::with_registry`]); `core-call`
    /// answers `unavailable` until [`ModuleHost::with_core_calls`], and
    /// `wasi:http` reaches public addresses only until
    /// [`ModuleHost::allowing_private_networks`].
    ///
    /// # Errors
    ///
    /// [`ModuleError::Load`] when the wasmtime engine cannot be built.
    pub fn new(store: S, timer: T, log: L, limits: Limits) -> Result<Self, ModuleError> {
        let (engine, cache) = engine(None)?;
        Ok(Self {
            shared: Arc::new(Shared {
                linker: linker(&engine)?,
                engine,
                store: Arc::new(store),
                log: Arc::new(log),
                core: Arc::new(NoCoreCalls),
                limits,
                cache_dir: None,
                cache,
                loads: AtomicUsize::new(0),
            }),
            timer: Arc::new(timer),
            registry: Arc::new(ModuleRegistry::new()),
            private_networks: false,
            resolver: Resolver::System,
            scheduler: true,
        })
    }
}

impl<S: ModuleStore + 'static, T: Timer, L: ModuleLog, C: CoreCalls> ModuleHost<S, T, L, C> {
    /// The same host answering `core-call` through `core` with the actor
    /// of the module call in progress (MOD-11).
    ///
    /// # Errors
    ///
    /// [`ModuleError::Load`] when the host imports do not link.
    pub fn with_core_calls<D: CoreCalls>(
        self,
        core: D,
    ) -> Result<ModuleHost<S, T, L, D>, ModuleError> {
        let shared = &self.shared;
        Ok(ModuleHost {
            shared: Arc::new(Shared {
                engine: shared.engine.clone(),
                linker: linker(&shared.engine)?,
                store: Arc::clone(&shared.store),
                log: Arc::clone(&shared.log),
                core: Arc::new(core),
                limits: shared.limits,
                cache_dir: shared.cache_dir.clone(),
                cache: shared.cache.clone(),
                loads: AtomicUsize::new(0),
            }),
            timer: self.timer,
            registry: self.registry,
            private_networks: self.private_networks,
            resolver: self.resolver,
            scheduler: self.scheduler,
        })
    }

    /// The same host registering its modules in `registry` (the daemon's,
    /// which REST and the hooks read).
    #[must_use]
    pub fn with_registry(self, registry: Arc<ModuleRegistry>) -> Self {
        Self { registry, ..self }
    }

    /// The same host without a scheduler: a module's `schedule` is kept
    /// as the module asked (the WIT `schedule` has no error) but nothing
    /// ever sleeps on the host's timer or calls `tick` (the CLI's host,
    /// a module of local mode lives for one command; LCL-07).
    #[must_use]
    pub fn without_scheduler(self) -> Self {
        Self {
            scheduler: false,
            ..self
        }
    }

    /// The same host letting `wasi:http` reach private addresses:
    /// loopback, private, link-local, shared and unspecified ones (spec
    /// 11 keeps the daemon's REST and the Caddy admin API on loopback, so
    /// by default a module never reaches them). For tests and for
    /// deployments that run their services on a private network.
    #[must_use]
    pub fn allowing_private_networks(self) -> Self {
        Self {
            private_networks: true,
            ..self
        }
    }

    /// The same host sending its modules' `wasi:http` requests to
    /// `https://<authority>` (a host, or `host:port`) over plain HTTP to
    /// `to` instead (feature `test-support`).
    ///
    /// The request is still checked against the module's `http_allow` by
    /// its own authority; only the connection changes: `to` is neither
    /// resolved nor vetted, and no TLS is spoken. The fake upstream of a
    /// module whose `http_allow` names a fixed public host
    /// (`dns.cloudflare` and `api.cloudflare.com`), served by the test on
    /// loopback. Called again, it routes one more authority (`git.github`
    /// reaches `api.github.com`, `github.com` and `ghcr.io`).
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn routing_https_to(self, authority: &str, to: core::net::SocketAddr) -> Self {
        let mut routes = if let Resolver::Routed(routes) = &self.resolver {
            routes.to_vec()
        } else {
            Vec::new()
        };
        routes.push(crate::http::HttpsRoute {
            authority: authority.to_owned(),
            to,
        });
        Self {
            resolver: Resolver::Routed(Arc::from(routes)),
            ..self
        }
    }

    /// The store its modules' imports reach (what `remove --purge` drops a module's storage in).
    #[must_use]
    pub fn store(&self) -> &S {
        &self.shared.store
    }

    /// The registry its modules are registered in.
    #[must_use]
    pub const fn registry(&self) -> &Arc<ModuleRegistry> {
        &self.registry
    }

    /// The same host keeping compiled components in `dir`
    /// (`paths.data/cache/`), keyed by the component's hash and the
    /// wasmtime version (wasmtime's own compile cache): a component
    /// compiled once, by any host over `dir`, is not compiled again
    /// (MOD-12).
    ///
    /// # Errors
    ///
    /// [`ModuleError::Load`] when the cache cannot be set up in `dir`.
    pub fn with_compile_cache(self, dir: impl Into<PathBuf>) -> Result<Self, ModuleError> {
        let dir = dir.into();
        let (engine, cache) = engine(Some(&dir))?;
        let shared = &self.shared;
        Ok(Self {
            shared: Arc::new(Shared {
                linker: linker(&engine)?,
                engine,
                store: Arc::clone(&shared.store),
                log: Arc::clone(&shared.log),
                core: Arc::clone(&shared.core),
                limits: shared.limits,
                cache_dir: Some(dir),
                cache,
                loads: AtomicUsize::new(0),
            }),
            timer: self.timer,
            registry: self.registry,
            private_networks: self.private_networks,
            resolver: self.resolver,
            scheduler: self.scheduler,
        })
    }

    /// What the compile cache of this host did so far; without a cache
    /// every load compiles.
    #[must_use]
    pub fn compile_stats(&self) -> CompileStats {
        let loads = self.shared.loads.load(Ordering::SeqCst);
        let cached = self.shared.cache.as_ref().map_or(0, Cache::cache_hits);
        CompileStats {
            compiled: loads.saturating_sub(cached),
            cached,
        }
    }

    /// Compiles (or takes from the compile cache) and instantiates
    /// `component` as the module `name` with no configuration and reads
    /// its manifest, without registering or keeping anything: what an
    /// install checks before it keeps the file. Nothing of
    /// the module runs but `manifest()`.
    ///
    /// # Errors
    ///
    /// [`ModuleError::Load`] when the bytes are not a component of the
    /// world, it does not instantiate, or its manifest is refused
    /// (MOD-06).
    pub async fn inspect(
        &self,
        name: ModuleName,
        component: &[u8],
    ) -> Result<ModuleManifest, ModuleError> {
        let (_recipe, _instance, manifest) =
            self.prepare(name, component, ModuleConfig::new()).await?;
        Ok(manifest)
    }

    /// Compiles (or takes from the compile cache) and instantiates
    /// `component` as the module `name` with `config`, reads its manifest
    /// and registers it, enabled, in the host's registry. Its scheduler,
    /// one tokio task, starts with it (unless
    /// [`ModuleHost::without_scheduler`]) and stops when the last clone of
    /// the [`Module`] is dropped. The compile runs on a blocking thread, off
    /// the async workers.
    ///
    /// # Errors
    ///
    /// [`ModuleError::Load`] when the bytes are not a component of the
    /// world, it does not instantiate, or its manifest is refused
    /// (MOD-06); nothing is registered then.
    pub async fn load(
        &self,
        name: ModuleName,
        component: &[u8],
        config: ModuleConfig,
    ) -> Result<Module<S, L, C>, ModuleError> {
        let (recipe, instance, manifest) = self.prepare(name.clone(), component, config).await?;
        let schedules = Arc::clone(&recipe.schedules);
        self.registry.register(name.clone(), manifest.clone());
        let slot = Arc::new(Mutex::new(Slot {
            recipe,
            instance: Some(instance),
            registry: Arc::clone(&self.registry),
        }));
        let ticker = self.scheduler.then(|| {
            tokio::spawn(scheduler::run(
                name.clone(),
                Arc::clone(&self.timer),
                schedules,
                Arc::clone(&slot),
            ))
        });
        Ok(Module {
            inner: Arc::new(Inner {
                name,
                manifest,
                registry: Arc::clone(&self.registry),
                slot,
                scheduler: ticker,
            }),
        })
    }

    /// The compiled and instantiated `component` with its manifest, its
    /// `wasi:http` allow-list in place.
    async fn prepare(
        &self,
        name: ModuleName,
        component: &[u8],
        config: ModuleConfig,
    ) -> Result<(Recipe<S, L, C>, Instance<S, L, C>, ModuleManifest), ModuleError> {
        let engine = self.shared.engine.clone();
        let bytes = component.to_vec();
        let component = tokio::task::spawn_blocking(move || Component::new(&engine, &bytes))
            .await
            .map_err(|joined| ModuleError::Load {
                module: name.to_string(),
                reason: format!("the compile did not finish: {joined}"),
            })?
            .map_err(|error| load_error(&name, &error))?;
        self.shared.loads.fetch_add(1, Ordering::SeqCst);
        let mut recipe = Recipe {
            shared: Arc::clone(&self.shared),
            component,
            name: name.clone(),
            config,
            schedules: Arc::new(Schedules::default()),
            http_allow: Arc::from(Vec::new()),
            replaces: Arc::from(Vec::new()),
            private_networks: self.private_networks,
            resolver: self.resolver.clone(),
        };
        let mut instance = recipe.instantiate().await?;
        let manifest = instance
            .manifest(&name, self.shared.limits.call_timeout)
            .await?;
        recipe.http_allow = Arc::from(manifest.http_allow.clone());
        recipe.replaces = Arc::from(manifest.replaces.clone());
        let state = instance.store.data_mut();
        state.http_hooks = recipe.http_hooks();
        state.replaces = Arc::clone(&recipe.replaces);
        Ok((recipe, instance, manifest))
    }
}
