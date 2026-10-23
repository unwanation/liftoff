//! [`FakeFunctions`] and [`FakeComponentFiles`].

use core::future::ready;

use futures_core::Stream;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::log::{Call, CallLog, Faults, FunctionCall, lock, scriptable};
use crate::domain::{ComponentHash, Env, EnvironmentId};
use crate::error::Error;
use crate::ports::{ComponentFiles, ComponentsLock, FunctionHost};

/// The first eight bytes of a WebAssembly component (the magic, version
/// `0x0d` and layer `1`), which is all [`FakeFunctions`] looks at.
pub const COMPONENT_PREAMBLE: &[u8] = b"\0asm\x0d\x00\x01\x00";

#[derive(Debug, Default)]
struct HostState {
    /// The active component of each environment.
    active: BTreeMap<EnvironmentId, ComponentHash>,
    /// The `wasi:config` each environment's component was activated with.
    configs: BTreeMap<EnvironmentId, Env>,
    /// Every component precompiled, in order.
    precompiled: Vec<ComponentHash>,
    /// The lines each environment's function wrote.
    logs: BTreeMap<EnvironmentId, Vec<String>>,
}

/// The function runtime in memory, behind the same [`FunctionHost`] as
/// the daemon's.
///
/// A component here is any bytes that start with
/// [`COMPONENT_PREAMBLE`] ([`FakeFunctions::component`] makes one);
/// anything else is refused by `check`, `precompile` and `activate` with
/// [`Error::Validation`] on field `component`. Every call is logged
/// ([`FunctionCall`]) and scriptable by its name (`fail_next("activate",
/// …)` is a warm-up that traps: nothing is activated). The active
/// component and the `wasi:config` of each environment can be inspected.
#[derive(Debug, Clone)]
pub struct FakeFunctions {
    log: CallLog,
    faults: Faults,
    state: Arc<Mutex<HostState>>,
}

scriptable!(FakeFunctions);

impl FakeFunctions {
    /// A runtime with nothing active, logging to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            faults: Faults::default(),
            state: Arc::default(),
        }
    }

    /// A component this fake accepts, told apart from others by `label`.
    #[must_use]
    pub fn component(label: &str) -> Vec<u8> {
        let mut bytes = COMPONENT_PREAMBLE.to_vec();
        bytes.extend_from_slice(label.as_bytes());
        bytes
    }

    /// The hash of the component active for `environment`, if any.
    #[must_use]
    pub fn active(&self, environment: EnvironmentId) -> Option<ComponentHash> {
        lock(&self.state).active.get(&environment).cloned()
    }

    /// The `wasi:config` the active component of `environment` was
    /// activated with.
    #[must_use]
    pub fn config(&self, environment: EnvironmentId) -> Option<Env> {
        lock(&self.state).configs.get(&environment).cloned()
    }

    /// Every component precompiled so far, in order.
    #[must_use]
    pub fn precompiled(&self) -> Vec<ComponentHash> {
        lock(&self.state).precompiled.clone()
    }

    /// Makes `component` active for `environment` behind the deploy's
    /// back (what an earlier deploy left).
    pub fn set_active(&self, environment: EnvironmentId, component: &[u8]) {
        let _previous = lock(&self.state)
            .active
            .insert(environment, ComponentHash::of(component));
    }

    /// Adds `line` to what the function of `environment` wrote.
    pub fn write_log(&self, environment: EnvironmentId, line: &str) {
        lock(&self.state)
            .logs
            .entry(environment)
            .or_default()
            .push(line.to_owned());
    }

    fn record(&self, call: FunctionCall) {
        self.log.push(Call::Function(call));
    }
}

/// `Ok` for bytes that start with [`COMPONENT_PREAMBLE`].
fn accepted(component: &[u8]) -> Result<(), Error> {
    if component.starts_with(COMPONENT_PREAMBLE) {
        Ok(())
    } else {
        Err(Error::invalid(
            "component",
            "it is not a wasi:http component",
        ))
    }
}

impl FunctionHost for FakeFunctions {
    fn check(&self, component: &[u8]) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(FunctionCall::Check(ComponentHash::of(component)));
        ready(
            self.faults
                .check("check")
                .and_then(|()| accepted(component)),
        )
    }

    fn precompile(&self, component: &[u8]) -> impl Future<Output = Result<(), Error>> + Send {
        let hash = ComponentHash::of(component);
        self.record(FunctionCall::Precompile(hash.clone()));
        let result = self
            .faults
            .check("precompile")
            .and_then(|()| accepted(component));
        if result.is_ok() {
            lock(&self.state).precompiled.push(hash);
        }
        ready(result)
    }

    fn activate(
        &self,
        environment: EnvironmentId,
        component: &[u8],
        config: Env,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        let hash = ComponentHash::of(component);
        self.record(FunctionCall::Activate {
            environment,
            component: hash.clone(),
        });
        let result = self
            .faults
            .check("activate")
            .and_then(|()| accepted(component));
        if result.is_ok() {
            let mut state = lock(&self.state);
            let _previous = state.active.insert(environment, hash);
            let _earlier = state.configs.insert(environment, config);
        }
        ready(result)
    }

    /// Whether [`FakeFunctions::active`] has a component; a read, not
    /// logged.
    fn is_active(&self, environment: EnvironmentId) -> impl Future<Output = bool> + Send {
        ready(self.active(environment).is_some())
    }

    /// The lines [`FakeFunctions::write_log`] wrote, the last `tail` of
    /// them; the stream ends after them, `follow` or not.
    fn logs(
        &self,
        environment: EnvironmentId,
        tail: Option<u32>,
        follow: bool,
    ) -> impl Future<
        Output = Result<impl Stream<Item = Result<String, Error>> + Send + 'static, Error>,
    > + Send {
        self.record(FunctionCall::Logs {
            environment,
            tail,
            follow,
        });
        ready(self.faults.check("logs").map(|()| {
            let lines = lock(&self.state)
                .logs
                .get(&environment)
                .cloned()
                .unwrap_or_default();
            let skip = tail.map_or(0, |tail| {
                lines
                    .len()
                    .saturating_sub(usize::try_from(tail).unwrap_or(usize::MAX))
            });
            futures_util::stream::iter(lines.into_iter().skip(skip).map(Ok))
        }))
    }
}

/// The kept `.wasm` files in memory, behind the same [`ComponentFiles`]
/// as the daemon's.
///
/// `save` and `remove` are logged ([`FunctionCall::Save`],
/// [`FunctionCall::Remove`]) and every call is scriptable by its name;
/// the kept files can be inspected.
#[derive(Debug, Clone)]
pub struct FakeComponentFiles {
    log: CallLog,
    faults: Faults,
    files: Arc<Mutex<BTreeMap<ComponentHash, Vec<u8>>>>,
    guard: Arc<ComponentsLock>,
}

scriptable!(FakeComponentFiles);

impl FakeComponentFiles {
    /// No files, logging to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            faults: Faults::default(),
            files: Arc::default(),
            guard: Arc::default(),
        }
    }

    /// The kept file of `hash`, if any.
    #[must_use]
    pub fn file(&self, hash: &ComponentHash) -> Option<Vec<u8>> {
        lock(&self.files).get(hash).cloned()
    }

    /// The hashes of every kept file, sorted.
    #[must_use]
    pub fn hashes(&self) -> Vec<ComponentHash> {
        lock(&self.files).keys().cloned().collect()
    }

    /// Keeps `component` behind the deploy's back (what an earlier deploy
    /// left); returns its hash.
    #[must_use]
    pub fn put(&self, component: &[u8]) -> ComponentHash {
        let hash = ComponentHash::of(component);
        let _previous = lock(&self.files).insert(hash.clone(), component.to_vec());
        hash
    }
}

impl ComponentFiles for FakeComponentFiles {
    fn lock(&self) -> &ComponentsLock {
        &self.guard
    }

    fn save(
        &self,
        hash: &ComponentHash,
        component: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.log
            .push(Call::Function(FunctionCall::Save(hash.clone())));
        let result = self.faults.check("save");
        if result.is_ok() {
            let _kept = lock(&self.files)
                .entry(hash.clone())
                .or_insert_with(|| component.to_vec());
        }
        ready(result)
    }

    fn read(
        &self,
        hash: &ComponentHash,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        ready(self.faults.check("read").map(|()| self.file(hash)))
    }

    fn list(&self) -> impl Future<Output = Result<Vec<ComponentHash>, Error>> + Send {
        ready(self.faults.check("list").map(|()| self.hashes()))
    }

    fn remove(&self, hash: &ComponentHash) -> impl Future<Output = Result<(), Error>> + Send {
        self.log
            .push(Call::Function(FunctionCall::Remove(hash.clone())));
        let result = self.faults.check("remove");
        if result.is_ok() {
            let _removed = lock(&self.files).remove(hash);
        }
        ready(result)
    }
}
