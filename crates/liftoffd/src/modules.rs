//! The daemon's side of installing modules.
//!
//! It names the pinned registry key (the core's) and the module runtime
//! (`liftoff-wasm`'s [`HostRuntime`](liftoff_wasm::HostRuntime) over the
//! files in `paths.data/modules/`), binds the modules' `core-call` to the
//! core's bridge, and writes the status of modules back periodically. The
//! registry client over HTTPS is `liftoff-wasm`'s
//! [`HttpModuleSource`](liftoff_wasm::HttpModuleSource).
//!
//! Every rule of installing lives in the core's [`ModuleService`].

use core::pin::Pin;
use core::time::Duration;
use std::sync::{Arc, OnceLock, Weak};

use liftoff_core::Error;
use liftoff_core::domain::{ModuleName, User};
use liftoff_core::module::{Caller, CoreBridge, CoreCalls};
use liftoff_core::ports::Ports;
use liftoff_core::service::{ModuleService, Settings};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// The minisign public key the module registry's releases are signed
/// with: the core's, which the CLI's local mode pins too.
pub use liftoff_core::module::REGISTRY_PUBLIC_KEY;

/// A module's `core-call` with what it borrows owned: the module, its
/// `replaces`, the actor, then `service`, `method` and `args`.
#[derive(Debug)]
pub struct OwnedCall {
    /// The calling module.
    pub module: ModuleName,
    /// Its manifest's `replaces`.
    pub replaces: Vec<String>,
    /// The actor of its call in progress.
    pub actor: Option<User>,
    /// The core service.
    pub service: String,
    /// Its method.
    pub method: String,
    /// The JSON arguments.
    pub args: String,
}

impl OwnedCall {
    /// The [`Caller`] of this call.
    #[must_use]
    pub fn caller(&self) -> Caller<'_> {
        Caller {
            module: &self.module,
            replaces: &self.replaces,
            actor: self.actor.as_ref(),
        }
    }
}

/// What [`LateCore`] runs a call with.
pub type CoreFuture = Pin<Box<dyn Future<Output = Result<String, Error>> + Send>>;

/// The body bound into a [`LateCore`].
type CoreFn = Box<dyn Fn(OwnedCall) -> CoreFuture + Send + Sync>;

/// The daemon's `core-call`: the core's bridge over the daemon's ports.
///
/// The module host is part of those ports, so it is built over an
/// unbound `LateCore` and the composition root binds the bridge once the
/// ports exist (as [`LateRunner`](crate::queue::LateRunner) for deploys).
/// Until then every call is [`Error::Unavailable`].
#[derive(Clone, Default)]
pub struct LateCore(Arc<OnceLock<CoreFn>>);

impl core::fmt::Debug for LateCore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LateCore")
            .field("bound", &self.0.get().is_some())
            .finish()
    }
}

impl LateCore {
    /// Sets what a call runs. Returns `false` (and keeps the first body)
    /// when it was bound before.
    pub fn bind(&self, call: impl Fn(OwnedCall) -> CoreFuture + Send + Sync + 'static) -> bool {
        self.0.set(Box::new(call)).is_ok()
    }
}

impl CoreCalls for LateCore {
    fn call(
        &self,
        caller: Caller<'_>,
        service: &str,
        method: &str,
        args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        self.0.get().map_or_else(
            || -> CoreFuture {
                Box::pin(core::future::ready(Err(Error::unavailable(
                    "the core is not wired to the module host yet",
                ))))
            },
            |call| {
                call(OwnedCall {
                    module: caller.module.clone(),
                    replaces: caller.replaces.to_vec(),
                    actor: caller.actor.cloned(),
                    service: service.to_owned(),
                    method: method.to_owned(),
                    args: args.to_owned(),
                })
            },
        )
    }
}

/// Binds the modules' `core-call` in `core` to the core's bridge over
/// `ports` configured by `settings`.
///
/// The daemon does this once its ports exist (MOD-11; `auth.create_session` of the `oidc` module). The bridge
/// holds the ports weakly, since the module host lives inside
/// them: a call while the daemon is going away is
/// [`Error::Unavailable`].
pub fn bind_core<P: Ports + 'static>(core: &LateCore, ports: &Arc<P>, settings: &Arc<Settings>) {
    let settings = Arc::clone(settings);
    bind_bridge(core, ports, move |ports| {
        CoreBridge::new(ports, Arc::clone(&settings))
    });
}

/// [`bind_core`] with the bridge `make` builds for each call.
///
/// The daemon's is [`CoreBridge::new`]; a test builds one that takes its
/// fake provider's plain-HTTP issuer
/// (`CoreBridge::allowing_loopback_issuers`).
pub fn bind_bridge<P, F>(core: &LateCore, ports: &Arc<P>, make: F)
where
    P: Ports + 'static,
    F: Fn(Arc<P>) -> CoreBridge<P> + Send + Sync + 'static,
{
    let ports: Weak<P> = Arc::downgrade(ports);
    let bound = core.bind(move |call: OwnedCall| -> CoreFuture {
        let bridge = ports.upgrade().map(&make);
        Box::pin(async move {
            let bridge = bridge.ok_or_else(|| Error::unavailable("the daemon is stopping"))?;
            bridge
                .call(call.caller(), &call.service, &call.method, &call.args)
                .await
        })
    });
    if !bound {
        tracing::error!("the modules' core-call was bound twice");
    }
}

/// The module runtime of the daemon: `liftoff-wasm`'s over the host and
/// `paths.data/modules/` (moved there, so the
/// CLI's local mode keeps its modules the same way).
pub use liftoff_wasm::HostRuntime as DaemonModules;

/// How often the daemon writes the modules the host disabled into
/// `modules` ([`sync_status_every`]).
pub const SYNC_STATUS_EVERY: Duration = Duration::from_secs(30);

/// Runs [`ModuleService::sync_status`] every `period` until `stop`: a
/// module the host disabled after failing calls in a row stays disabled
/// across a restart (MOD-09).
pub async fn sync_status_every<P: Ports>(
    modules: ModuleService<P>,
    period: Duration,
    stop: CancellationToken,
) {
    let mut every = tokio::time::interval(period);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _instant = every.tick() => {}
        }
        if let Err(error) = modules.sync_status().await {
            tracing::error!(?error, "writing the status of modules failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    mod late_core {
        use super::*;

        fn oidc() -> ModuleName {
            ModuleName::new("oidc").unwrap()
        }

        /// A call of `oidc`, which replaces `login`, without an actor.
        async fn call(core: &LateCore) -> Result<String, Error> {
            let replaces = ["login".to_owned()];
            core.call(
                Caller {
                    module: &oidc(),
                    replaces: &replaces,
                    actor: None,
                },
                "auth",
                "create_session",
                "{}",
            )
            .await
        }

        // TC: MOD-11 (before the daemon binds it, every call is refused)
        #[tokio::test]
        async fn answers_unavailable_when_unbound() {
            let refused = call(&LateCore::default()).await;
            assert!(matches!(refused, Err(Error::Unavailable(_))), "{refused:?}");
        }

        // TC: MOD-11, OIDC-01 (the bound body gets the module, its
        // `replaces`, the actor and the call as they were)
        #[tokio::test]
        async fn forwards_caller_and_call_when_bound() {
            let core = LateCore::default();
            let seen = Arc::new(Mutex::new(None));
            let recorded = Arc::clone(&seen);
            let bound = core.bind(move |call: OwnedCall| -> CoreFuture {
                *recorded.lock().unwrap() = Some((
                    call.module.to_string(),
                    call.replaces.clone(),
                    call.actor.is_some(),
                    format!("{}.{} {}", call.service, call.method, call.args),
                ));
                Box::pin(core::future::ready(Ok("granted".to_owned())))
            });
            let answered = call(&core).await;
            assert_eq!(
                (bound, answered.ok(), seen.lock().unwrap().clone()),
                (
                    true,
                    Some("granted".to_owned()),
                    Some((
                        "oidc".to_owned(),
                        vec!["login".to_owned()],
                        false,
                        "auth.create_session {}".to_owned()
                    ))
                )
            );
        }

        // TC: MOD-11 (a second bind keeps the first body)
        #[tokio::test]
        async fn keeps_first_body_when_bound_twice() {
            let core = LateCore::default();
            let first = core.bind(|_call| Box::pin(core::future::ready(Ok("first".to_owned()))));
            let second = core.bind(|_call| Box::pin(core::future::ready(Ok("second".to_owned()))));
            assert_eq!(
                (first, second, call(&core).await.ok()),
                (true, false, Some("first".to_owned()))
            );
        }
    }
}
