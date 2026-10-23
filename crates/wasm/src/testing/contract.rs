//! The checks of the module host contract (MOD-06..MOD-12).
//!
//! One function per rule against the built contract components, generic
//! over the host's store;
//! [`module_host_contract_tests!`](crate::module_host_contract_tests)
//! runs them all.
//!
//! Every check returns `Err` when it cannot be set up (a component is not
//! built, a fixture fails) and panics with the rule when the host breaks
//! it.

use core::time::Duration;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use liftoff_core::Error;
use liftoff_core::domain::{ModuleName, User, UserRole};
use liftoff_core::fakes::{FakePorts, Records, World};
use liftoff_core::module::{Caller, CoreBridge, CoreCalls, HookName, ModuleStatus};
use liftoff_core::ports::{Action, HookResult, ModuleStore, Resource, Store as _};

use super::{
    Checked, FailingStore, Failure, Host, Listener, Loaded, MEMORY_CAP, RecordingCore, component,
    limits, load, within,
};
use crate::{
    CompileStats, Limits, MAX_HOST_RESOURCES, ModuleConfig, ModuleError, ModuleHost, RouteRequest,
    TokioTimer, TracingLog,
};

/// A host of the contract over `store` within `limits`.
fn host_within<S: ModuleStore + 'static>(store: S, limits: Limits) -> Result<Host<S>, Failure> {
    Ok(ModuleHost::new(store, TokioTimer, TracingLog, limits)?)
}

/// A host of the contract over a fresh store.
fn host<S: ModuleStore + 'static>(new_store: &(impl Fn() -> S + Sync)) -> Result<Host<S>, Failure> {
    host_within(new_store(), limits())
}

/// The code and message of a module's own error.
const fn module_error(result: &Result<String, ModuleError>) -> Option<(&str, &str)> {
    match result {
        Err(ModuleError::Module { code, message }) => Some((code.as_str(), message.as_str())),
        _ => None,
    }
}

// ---- MOD-06 -----------------------------------------------------------------

/// The host reads `manifest()` at load through the core's strict parser.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn reads_declared_fields_when_allimports_loaded<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-06
    let module = load(&host(new_store)?, "allimports").await?;
    let manifest = module.manifest();
    assert_eq!(
        (
            manifest.name.as_str(),
            manifest.version.as_str(),
            manifest
                .http_allow
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            manifest.migrations,
            manifest.hooks.clone(),
        ),
        (
            "allimports",
            "0.1.0",
            vec!["127.0.0.1".to_owned()],
            1,
            vec![HookName::Authorize, HookName::BeforeDeploy],
        ),
        "the manifest of allimports as it declared it"
    );
    Ok(())
}

/// A manifest of name, version and migrations only is valid.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn reads_minimal_manifest_when_probe_loaded<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-06
    let module = load(&host(new_store)?, "probe").await?;
    assert_eq!(
        (
            module.manifest().name.as_str(),
            module.manifest().migrations,
            module.manifest().http_allow.is_empty(),
        ),
        ("probe", 1, true),
        "the minimal manifest of probe"
    );
    Ok(())
}

/// A manifest with an unknown field is refused at load, naming the field.
///
/// # Errors
///
/// When the component is not built or the host cannot be built.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn refuses_load_when_manifest_has_unknown_field<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-06
    let host = host(new_store)?;
    let loaded = host
        .load(
            ModuleName::new("badmanifest")?,
            &component("badmanifest")?,
            ModuleConfig::new(),
        )
        .await;
    assert!(
        matches!(&loaded, Err(ModuleError::Load { module, reason })
            if module == "badmanifest" && reason.contains("colour")),
        "a manifest with an unknown field does not load: {loaded:?}"
    );
    Ok(())
}

/// A refused module is not registered.
///
/// # Errors
///
/// When the component is not built or the host cannot be built.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn registers_nothing_when_manifest_is_refused<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-06
    let host = host(new_store)?;
    let name = ModuleName::new("badmanifest")?;
    let _refused = host
        .load(
            name.clone(),
            &component("badmanifest")?,
            ModuleConfig::new(),
        )
        .await;
    assert_eq!(
        host.registry().status(&name),
        None,
        "a refused module is not registered"
    );
    Ok(())
}

/// A loaded module is in the host's registry, enabled, with its manifest.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn registers_module_enabled_when_loaded<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-06, MOD-09
    let host = host(new_store)?;
    let module = load(&host, "allimports").await?;
    assert_eq!(
        (
            host.registry().manifest(module.name()).as_ref(),
            host.registry().status(module.name()),
        ),
        (
            Some(module.manifest()),
            Some(ModuleStatus {
                enabled: true,
                error: None
            })
        ),
        "the registry holds the loaded module"
    );
    Ok(())
}

// ---- MOD-07 -----------------------------------------------------------------

/// A host of the contract over a fresh store whose modules may reach
/// private addresses (the contract's listeners are on loopback).
fn open_host<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Result<Host<S>, Failure> {
    Ok(host(new_store)?.allowing_private_networks())
}

/// `badhost` on `host` with `http_allow` set to `allow`.
async fn badhost<S: ModuleStore + 'static>(
    host: &Host<S>,
    allow: &str,
) -> Result<Loaded<S>, Failure> {
    let config = ModuleConfig::from([("http_allow".to_owned(), allow.to_owned())]);
    Ok(host
        .load(ModuleName::new("badhost")?, &component("badhost")?, config)
        .await?)
}

/// The tool `fetch {url}` of `badhost`.
async fn fetch<S: ModuleStore + 'static, C: CoreCalls>(
    module: &Loaded<S, C>,
    url: &str,
) -> Result<String, ModuleError> {
    let args = serde_json::json!({ "url": url }).to_string();
    module.handle_tool("fetch", &args).await
}

/// A host in `http_allow` is reached.
///
/// # Errors
///
/// When the component is not built or does not load, or the listener
/// does not start.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn reaches_host_when_it_is_in_http_allow<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-07
    let listener = Listener::start().await?;
    let allow = format!("127.0.0.1:{}", listener.port);
    let module = badhost(&open_host(new_store)?, &allow).await?;
    let fetched = fetch(&module, &format!("http://127.0.0.1:{}/", listener.port)).await;
    assert_eq!(
        (fetched, listener.accepted()),
        (Ok("200".to_owned()), 1),
        "an allowed host is reached once"
    );
    Ok(())
}

/// A host outside `http_allow` is an error to the module and the request
/// never leaves: `localhost` reaches the same listener, which still saw
/// one connection only, the allowed one.
///
/// # Errors
///
/// When the component is not built or does not load, or the listener
/// does not start.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn refuses_request_when_host_is_not_in_http_allow<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-07
    let listener = Listener::start().await?;
    let allow = format!("127.0.0.1:{}", listener.port);
    let module = badhost(&open_host(new_store)?, &allow).await?;
    let allowed = fetch(&module, &format!("http://127.0.0.1:{}/", listener.port)).await;
    let refused = fetch(&module, &format!("http://localhost:{}/", listener.port)).await;
    assert_eq!(
        (
            allowed,
            module_error(&refused).map(|(code, _)| code),
            listener.accepted()
        ),
        (Ok("200".to_owned()), Some("http"), 1),
        "a host outside http_allow is refused before connecting"
    );
    Ok(())
}

/// The module learns why: the request was denied.
///
/// # Errors
///
/// When the component is not built or does not load, the listener does
/// not start or the allowed request fails.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn names_denial_when_host_is_not_in_http_allow<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-07
    let listener = Listener::start().await?;
    let allow = format!("127.0.0.1:{}", listener.port);
    let module = badhost(&open_host(new_store)?, &allow).await?;
    let _allowed = fetch(&module, &format!("http://127.0.0.1:{}/", listener.port)).await?;
    let refused = fetch(&module, "http://example.com/").await;
    assert!(
        module_error(&refused).is_some_and(|(_, message)| message.contains("Denied")),
        "the module's error names the denial"
    );
    Ok(())
}

/// An allowed host on a port its entry does not name is refused, and
/// nothing connects.
///
/// # Errors
///
/// When the component is not built or does not load, or a listener does
/// not start.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn refuses_request_when_port_is_not_in_http_allow<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-07
    let allowed = Listener::start().await?;
    let other = Listener::start().await?;
    let module = badhost(
        &open_host(new_store)?,
        &format!("127.0.0.1:{}", allowed.port),
    )
    .await?;
    let refused = fetch(&module, &format!("http://127.0.0.1:{}/", other.port)).await;
    assert_eq!(
        (
            module_error(&refused).map(|(code, _)| code),
            other.accepted()
        ),
        (Some("http"), 0),
        "another port of an allowed host is refused before connecting"
    );
    Ok(())
}

/// An entry without a port allows the scheme's default port only.
///
/// # Errors
///
/// When the component is not built or does not load, or the listener
/// does not start.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn refuses_request_when_entry_has_no_port_and_port_is_not_default<
    S: ModuleStore + 'static,
>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-07
    let listener = Listener::start().await?;
    let module = badhost(&open_host(new_store)?, "127.0.0.1").await?;
    let refused = fetch(&module, &format!("http://127.0.0.1:{}/", listener.port)).await;
    assert_eq!(
        (
            module_error(&refused).map(|(code, _)| code),
            listener.accepted()
        ),
        (Some("http"), 0),
        "a bare entry does not open every port"
    );
    Ok(())
}

/// By default a module reaches public addresses only: an allowed
/// loopback literal is refused, and nothing connects.
///
/// # Errors
///
/// When the component is not built or does not load, or the listener
/// does not start.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn refuses_request_when_allowed_host_is_private<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-07
    let listener = Listener::start().await?;
    let allow = format!("127.0.0.1:{}", listener.port);
    let module = badhost(&host(new_store)?, &allow).await?;
    let refused = fetch(&module, &format!("http://127.0.0.1:{}/", listener.port)).await;
    assert_eq!(
        (
            module_error(&refused).map(|(code, _)| code),
            listener.accepted()
        ),
        (Some("http"), 0),
        "a private address is refused before connecting"
    );
    Ok(())
}

/// An allowed name that resolves to a private address is refused too.
///
/// # Errors
///
/// When the component is not built or does not load, or the listener
/// does not start.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn refuses_request_when_allowed_name_resolves_to_private_address<
    S: ModuleStore + 'static,
>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-07
    let listener = Listener::start().await?;
    let allow = format!("localhost:{}", listener.port);
    let module = badhost(&host(new_store)?, &allow).await?;
    let refused = fetch(&module, &format!("http://localhost:{}/", listener.port)).await;
    assert_eq!(
        (
            module_error(&refused).map(|(code, _)| code),
            listener.accepted()
        ),
        (Some("http"), 0),
        "a name resolving to loopback is refused before connecting"
    );
    Ok(())
}

// ---- MOD-08 -----------------------------------------------------------------

/// Limits with a call timeout of 200 ms.
const fn short_timeout() -> Limits {
    Limits {
        call_timeout: Duration::from_millis(200),
        ..limits()
    }
}

/// How long a timeout check waits for the host to interrupt the call.
const TIMEOUT_CHECK_LIMIT: Duration = Duration::from_secs(10);

/// How long a call of [`short_timeout`] that wasm code keeps busy may
/// take in all: the epoch interrupts it at 200 ms, well before the host
/// would drop it (`call_timeout` + 1 s).
const EPOCH_INTERRUPT_BOUND: Duration = Duration::from_millis(900);

/// How long a call of [`short_timeout`] that waits on the host may take
/// in all: the host drops it 1 s after `call_timeout`.
const HOST_WAIT_BOUND: Duration = Duration::from_secs(3);

/// What `spin` answered and how long it took, and the call after it if
/// one was made.
type Spun = (
    Result<String, ModuleError>,
    Duration,
    Option<Result<String, ModuleError>>,
);

/// `spin` of `loop` under [`short_timeout`], then `then` on the same
/// module, on a thread of its own.
fn spin_then<S: ModuleStore + 'static>(
    store: S,
    then: Option<&'static str>,
) -> Result<Spun, Failure> {
    within(TIMEOUT_CHECK_LIMIT, async move || {
        let module = load(&host_within(store, short_timeout())?, "loop").await?;
        let started = Instant::now();
        let spun = module.handle_tool("spin", "{}").await;
        let took = started.elapsed();
        let next = match then {
            Some(next) => Some(module.handle_tool(next, "{}").await),
            None => None,
        };
        Ok::<_, Failure>((spun, took, next))
    })?
}

/// An endless call is interrupted at `call_timeout`, by the epoch: well
/// before the host would drop a call waiting on it.
///
/// # Errors
///
/// When the component is not built or does not load, or the call does
/// not return within 10 s.
///
/// # Panics
///
/// When the host breaks the rule.
pub fn interrupts_call_when_it_runs_past_call_timeout<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-08
    let (spun, took, _) = spin_then(new_store(), None)?;
    assert_eq!(
        (spun, took < EPOCH_INTERRUPT_BOUND),
        (
            Err(ModuleError::Timeout {
                module: "loop".to_owned()
            }),
            true
        ),
        "an endless call times out at its call timeout ({took:?})"
    );
    Ok(())
}

/// A call waiting on the host (here an HTTP request to a server that
/// never answers), which the epoch cannot interrupt, is dropped shortly
/// after `call_timeout`.
///
/// # Errors
///
/// When the component is not built or does not load, the listener does
/// not start, or the call does not return within 10 s.
///
/// # Panics
///
/// When the host breaks the rule.
pub fn interrupts_call_when_host_request_hangs<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-08
    let store = new_store();
    let (fetched, took) = within(TIMEOUT_CHECK_LIMIT, async move || {
        let silent = Listener::silent().await?;
        let host = host_within(store, short_timeout())?.allowing_private_networks();
        let module = badhost(&host, &format!("127.0.0.1:{}", silent.port)).await?;
        let started = Instant::now();
        let fetched = fetch(&module, &format!("http://127.0.0.1:{}/", silent.port)).await;
        Ok::<_, Failure>((fetched, started.elapsed()))
    })??;
    assert_eq!(
        (fetched, took < HOST_WAIT_BOUND),
        (
            Err(ModuleError::Timeout {
                module: "badhost".to_owned()
            }),
            true
        ),
        "a call stuck on the host is dropped after its call timeout ({took:?})"
    );
    Ok(())
}

/// The core sees the timeout as `Internal`.
///
/// # Errors
///
/// As [`interrupts_call_when_it_runs_past_call_timeout`].
///
/// # Panics
///
/// When the host breaks the rule.
pub fn reports_internal_when_call_timed_out<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-08
    let (spun, _, _) = spin_then(new_store(), None)?;
    let error = spun.map_err(Error::from);
    assert!(
        matches!(error, Err(Error::Internal(_))),
        "a timeout is Internal to the core"
    );
    Ok(())
}

/// The module lives on: the next call answers.
///
/// # Errors
///
/// As [`interrupts_call_when_it_runs_past_call_timeout`].
///
/// # Panics
///
/// When the host breaks the rule.
pub fn answers_next_call_when_previous_call_timed_out<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-08
    let (_, _, pinged) = spin_then(new_store(), Some("ping"))?;
    assert_eq!(
        pinged,
        Some(Ok(r#""pong""#.to_owned())),
        "the call after a timeout answers"
    );
    Ok(())
}

/// Timeouts count toward the three in a row of MOD-09: an endless loop is
/// the module's fault.
///
/// # Errors
///
/// When the component is not built or does not load, or the calls do not
/// return within 10 s.
///
/// # Panics
///
/// When the host breaks the rule.
pub fn disables_module_when_three_calls_in_a_row_time_out<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-08, MOD-09
    let store = new_store();
    let status = within(TIMEOUT_CHECK_LIMIT, async move || {
        let module = load(&host_within(store, short_timeout())?, "loop").await?;
        for _ in 0..3 {
            let _spun = module.handle_tool("spin", "{}").await;
        }
        Ok::<_, Failure>(module.status())
    })??;
    assert!(
        !status.enabled,
        "three timeouts in a row disable the module"
    );
    Ok(())
}

/// A call whose caller goes away (a REST client that disconnects) costs
/// the module nothing: the next call answers and the dropped call does not
/// count toward disabling it.
///
/// # Errors
///
/// When the component is not built or does not load, or the calls do not
/// return within 10 s.
///
/// # Panics
///
/// When the host breaks the rule.
pub fn answers_next_call_when_previous_call_was_dropped<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let store = new_store();
    let (pings, status) = within(TIMEOUT_CHECK_LIMIT, async move || {
        let module = load(&host_within(store, limits())?, "loop").await?;
        let mut pings = Vec::new();
        for _ in 0..3 {
            let dropped =
                tokio::time::timeout(Duration::from_millis(50), module.handle_tool("spin", "{}"))
                    .await;
            assert!(dropped.is_err(), "spin is still running when it is dropped");
            pings.push(module.handle_tool("ping", "{}").await);
        }
        Ok::<_, Failure>((pings, module.status()))
    })??;
    assert_eq!(
        (pings, status),
        (
            vec![Ok(r#""pong""#.to_owned()); 3],
            ModuleStatus {
                enabled: true,
                error: None
            }
        ),
        "a dropped call leaves the module answering and enabled"
    );
    Ok(())
}

// ---- MOD-09, MOD-10 ---------------------------------------------------------

/// `times` calls of `panic`, each a trap.
async fn panics<S: ModuleStore + 'static>(module: &Loaded<S>, times: usize) {
    for _ in 0..times {
        let trapped = module.handle_tool("panic", "").await;
        assert!(
            matches!(trapped, Err(ModuleError::Trap { .. })),
            "a panic of the module is a trap"
        );
    }
}

/// Three traps in a row disable the module, with its error.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn disables_module_when_three_calls_in_a_row_trap<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    panics(&module, 3).await;
    let status = module.status();
    assert_eq!(
        (status.enabled, status.error.is_some()),
        (false, true),
        "three traps in a row disable the module"
    );
    Ok(())
}

/// A disabled module is not entered.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn refuses_call_when_module_is_disabled<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    panics(&module, 3).await;
    let refused = module.handle_tool("ok", "").await;
    assert!(
        matches!(&refused, Err(ModuleError::Disabled { module, .. }) if module == "trap"),
        "a disabled module refuses calls"
    );
    Ok(())
}

/// In a row: an answered call starts the count again.
///
/// # Errors
///
/// When the component is not built or does not load, or `ok` fails.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn keeps_module_enabled_when_answered_call_breaks_trap_run<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    panics(&module, 2).await;
    module.handle_tool("ok", "").await?;
    panics(&module, 1).await;
    assert_eq!(
        module.status(),
        ModuleStatus {
            enabled: true,
            error: None
        },
        "an answered call breaks the run of traps"
    );
    Ok(())
}

/// Two traps leave the module enabled.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn keeps_module_enabled_when_two_calls_trap<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    panics(&module, 2).await;
    assert!(
        module.status().enabled,
        "two traps leave the module enabled"
    );
    Ok(())
}

/// The host's registry has the disabled module, with its error, and its
/// routes answer `ModuleRequired` (424) there.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn reports_disabled_status_in_registry_when_three_calls_trap<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let host = host(new_store)?;
    let module = load(&host, "trap").await?;
    panics(&module, 3).await;
    let status = host.registry().status(module.name());
    let route = host.registry().route(module.name(), "GET", "/ok");
    assert!(
        status.as_ref().is_some_and(|status| !status.enabled
            && status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("trapped")))
            && matches!(&route, Err(Error::ModuleRequired(name)) if name == "trap"),
        "the registry shows the module disabled with its error: {status:?}, {route:?}"
    );
    Ok(())
}

/// A failure of the daemon's store under a call is not the module's: it
/// fails the call, `Internal` to the core, but never disables the module.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn keeps_module_enabled_when_store_fails_three_times<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let failing = host_within(FailingStore::new(new_store()), limits())?;
    let module = load(&failing, "allimports").await?;
    let mut internal = 0_usize;
    for _ in 0..3 {
        let set = module
            .handle_tool("kv-set", r#"{"key":"k","value":"v"}"#)
            .await;
        if matches!(set, Err(ModuleError::HostFailed { .. }))
            && matches!(set.map_err(Error::from), Err(Error::Internal(_)))
        {
            internal = internal.saturating_add(1);
        }
    }
    assert_eq!(
        (internal, module.status().enabled),
        (3, true),
        "store failures fail the calls but keep the module enabled"
    );
    Ok(())
}

/// REST of a disabled module answers 424: the core sees
/// `ModuleRequired`, which REST maps to 424.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn reports_module_required_when_module_is_disabled<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    panics(&module, 3).await;
    let refused = module.handle_tool("ok", "").await.map_err(Error::from);
    assert_eq!(
        refused.map_err(|error| error.to_string()),
        Err(Error::ModuleRequired("trap".to_owned()).to_string()),
        "a disabled module is ModuleRequired to the core"
    );
    Ok(())
}

/// Memory past the cap traps the call, and the core sees `Internal`, not
/// an OOM of the daemon.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn reports_internal_trap_when_module_grows_past_memory_cap<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-10
    let module = load(&host(new_store)?, "trap").await?;
    let over = MEMORY_CAP.saturating_mul(2).to_string();
    let grown = module.handle_tool("grow", &over).await;
    let trapped = matches!(grown, Err(ModuleError::Trap { .. }));
    let core = grown.map_err(Error::from);
    assert!(
        trapped && matches!(core, Err(Error::Internal(_))),
        "growing past the cap is a trap, Internal to the core"
    );
    Ok(())
}

// ---- MOD-09: routes and hooks ----------------------------------------------

/// `GET path` with an empty body.
fn get(path: &str) -> RouteRequest {
    RouteRequest {
        method: "GET".to_owned(),
        path: path.to_owned(),
        headers: Vec::new(),
        body: Vec::new(),
    }
}

/// `handle-route` gets the request and the call's actor, and its answer
/// comes back.
///
/// # Errors
///
/// When the component is not built or does not load, or the route fails.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn answers_route_with_call_actor_when_module_handles_it<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    let carol = user("carol", UserRole::User)?;
    let answered = module.handle_route(Some(&carol), get("/ok")).await?;
    assert_eq!(
        (answered.status, answered.body),
        (200, carol.id.to_string().into_bytes()),
        "the route answers with the call's actor"
    );
    Ok(())
}

/// Three routes in a row that trap disable the module, and the next
/// route is `ModuleRequired` to the core.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn disables_module_when_three_routes_in_a_row_trap<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    for _ in 0..3 {
        let _trapped = module.handle_route(None, get("/panic")).await;
    }
    let refused = module
        .handle_route(None, get("/ok"))
        .await
        .map_err(Error::from);
    assert!(
        !module.status().enabled && matches!(refused, Err(Error::ModuleRequired(_))),
        "three trapped routes disable the module"
    );
    Ok(())
}

/// `on-hook` gets the hook's name and payload, and its decision comes
/// back.
///
/// # Errors
///
/// When the component is not built or does not load, or the hook fails.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn answers_hook_result_when_module_hooks<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    let denied = module.on_hook(None, HookName::Authorize, "deny").await?;
    let allowed = module.on_hook(None, HookName::Authorize, "allow").await?;
    let other = module.on_hook(None, HookName::Authorize, "{}").await?;
    assert_eq!(
        (denied, allowed, other),
        (
            HookResult::Deny("denied by trap".to_owned()),
            HookResult::Allow,
            HookResult::Continue
        ),
        "the module's decisions come back"
    );
    Ok(())
}

/// Three hooks in a row that trap disable the module.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn disables_module_when_three_hooks_in_a_row_trap<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-09
    let module = load(&host(new_store)?, "trap").await?;
    for _ in 0..3 {
        let _trapped = module.on_hook(None, HookName::Authorize, "panic").await;
    }
    let refused = module.on_hook(None, HookName::Authorize, "allow").await;
    assert!(
        !module.status().enabled && matches!(refused, Err(ModuleError::Disabled { .. })),
        "three trapped hooks disable the module"
    );
    Ok(())
}

/// Handles a module makes the host hold (here `wasi:http` field sets) are
/// capped like its memory: past [`MAX_HOST_RESOURCES`] the call traps
/// instead of growing the daemon, and the module answers again.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn traps_call_when_module_holds_too_many_host_resources<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-10
    let module = badhost(&host(new_store)?, "127.0.0.1").await?;
    let hoard = |count: usize| serde_json::json!({ "count": count }).to_string();
    let within_cap = module.handle_tool("hoard", &hoard(100)).await;
    let past_cap = module
        .handle_tool("hoard", &hoard(MAX_HOST_RESOURCES.saturating_mul(4)))
        .await;
    let after = module.handle_tool("hoard", &hoard(100)).await;
    assert!(
        within_cap == Ok("100".to_owned())
            && matches!(past_cap, Err(ModuleError::Trap { .. }))
            && after == Ok("100".to_owned()),
        "too many host handles trap the call only: {within_cap:?}, {past_cap:?}, {after:?}"
    );
    Ok(())
}

// ---- MOD-11 -----------------------------------------------------------------

/// A user of the catalogue's records.
fn user(login: &str, role: UserRole) -> Result<User, Failure> {
    Ok(Records::default().user(login, role)?)
}

/// The arguments of the tool `core-call` of `allimports`.
fn core_call_args(service: &str, method: &str, args: &str) -> String {
    serde_json::json!({ "service": service, "method": method, "args": args }).to_string()
}

/// `allimports` over `core`.
async fn allimports<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
    core: &RecordingCore,
) -> Result<Loaded<S, RecordingCore>, Failure> {
    load(
        &host(new_store)?.with_core_calls(core.clone())?,
        "allimports",
    )
    .await
}

/// The core sees the actor of the module call.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn passes_call_actor_when_module_calls_core<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11
    let core = RecordingCore::default();
    let module = allimports(new_store, &core).await?;
    let carol = user("carol", UserRole::User)?;
    let _answered = module
        .handle_tool_as(
            &carol,
            "core-call",
            &core_call_args("projects", "list", "{}"),
        )
        .await;
    assert_eq!(
        core.calls(),
        vec![(
            Some(carol.id),
            "projects".to_owned(),
            "list".to_owned(),
            "{}".to_owned()
        )],
        "the core is called as the call's actor"
    );
    Ok(())
}

/// The module gets the core's JSON answer.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn answers_core_result_when_core_answers<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11
    let core = RecordingCore::default();
    core.answer(Ok(r#"[{"name":"shop"}]"#.to_owned()));
    let module = allimports(new_store, &core).await?;
    let answered = module
        .handle_tool_as(
            &user("carol", UserRole::User)?,
            "core-call",
            &core_call_args("projects", "list", "{}"),
        )
        .await;
    assert_eq!(
        answered,
        Ok(r#"[{"name":"shop"}]"#.to_owned()),
        "the module gets the core's answer"
    );
    Ok(())
}

/// A refusal of the core reaches the module as its error.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn hands_refusal_to_module_when_core_refuses<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11
    let core = RecordingCore::default();
    core.answer(Err(Error::forbidden("not your tenant")));
    let module = allimports(new_store, &core).await?;
    let refused = module
        .handle_tool_as(
            &user("carol", UserRole::User)?,
            "core-call",
            &core_call_args("projects", "list", "{}"),
        )
        .await;
    assert_eq!(
        module_error(&refused),
        Some(("forbidden", "not your tenant")),
        "the core's refusal reaches the module"
    );
    Ok(())
}

/// A call without an actor calls the core without one.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn passes_no_actor_when_call_has_none<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11
    let core = RecordingCore::default();
    let module = allimports(new_store, &core).await?;
    let _answered = module
        .handle_tool("core-call", &core_call_args("projects", "list", "{}"))
        .await;
    assert_eq!(
        core.calls().first().map(|(actor, ..)| *actor),
        Some(None),
        "a call without an actor calls the core without one"
    );
    Ok(())
}

/// The actor is the current call's: it does not outlive it.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn forgets_actor_when_next_call_has_none<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11
    let core = RecordingCore::default();
    let module = allimports(new_store, &core).await?;
    let args = core_call_args("projects", "list", "{}");
    let _first = module
        .handle_tool_as(&user("carol", UserRole::User)?, "core-call", &args)
        .await;
    let _second = module.handle_tool("core-call", &args).await;
    assert_eq!(
        core.calls().get(1).map(|(actor, ..)| *actor),
        Some(None),
        "the actor of a call is gone in the next"
    );
    Ok(())
}

/// `allimports` over the core's services of the [`World`].
async fn bridged<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
    world: &World,
) -> Result<Loaded<S, CoreBridge<FakePorts>>, Failure> {
    let bridge = CoreBridge::new(Arc::clone(&world.ports), Arc::clone(&world.settings));
    load(&host(new_store)?.with_core_calls(bridge)?, "allimports").await
}

/// Through the core's bridge: an allowed method passes `authorize` with
/// the call's actor.
///
/// # Errors
///
/// When the component is not built or does not load, or the world cannot
/// be arranged.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn asks_authorize_with_call_actor_when_bridge_lists_projects<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11
    let world = World::new().await;
    let carol = world.records.user("carol", UserRole::User)?;
    world.ports.store.insert_user(&carol).await?;
    world
        .ports
        .hooks
        .answer_next("authorize", HookResult::Allow);
    let module = bridged(new_store, &world).await?;
    world.log().clear();
    let _listed = module
        .handle_tool_as(
            &carol,
            "core-call",
            &core_call_args("projects", "list", "{}"),
        )
        .await;
    let asked = world.log().authorized();
    assert_eq!(
        (
            asked.first().cloned(),
            asked.iter().all(|(actor, _, _)| *actor == carol.id)
        ),
        (Some((carol.id, Action::ProjectList, Resource::Admin)), true),
        "the core asks authorize for the call's actor, first about the list: {asked:?}"
    );
    Ok(())
}

/// Through the core's bridge: a method not in the table is an error to
/// the module.
///
/// # Errors
///
/// When the component is not built or does not load.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn refuses_method_when_bridge_has_no_such_method<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11
    let world = World::new().await;
    let module = bridged(new_store, &world).await?;
    let refused = module
        .handle_tool_as(
            &world.owner,
            "core-call",
            &core_call_args("store", "delete_user", "{}"),
        )
        .await;
    assert_eq!(
        module_error(&refused).map(|(code, _)| code),
        Some("forbidden"),
        "a method outside the table is forbidden"
    );
    Ok(())
}

/// A [`CoreCalls`] that, the way the hook dispatcher of will
/// raise `authorize` for every core service, calls `on-hook(authorize)`
/// of the module it serves while that module's `core-call` is in
/// progress, and answers the hook's decision as JSON.
struct HookingCore<S: 'static> {
    module: Hooked<S>,
    /// Calls a tool of the module instead of raising a hook.
    tool: bool,
}

/// The module a [`HookingCore`] serves, once it is loaded.
type Hooked<S> = Arc<std::sync::Mutex<Option<Loaded<S, HookingCore<S>>>>>;

impl<S: 'static> Clone for HookingCore<S> {
    fn clone(&self) -> Self {
        Self {
            module: Arc::clone(&self.module),
            tool: self.tool,
        }
    }
}

impl<S: ModuleStore + 'static> CoreCalls for HookingCore<S> {
    fn call(
        &self,
        caller: Caller<'_>,
        _service: &str,
        _method: &str,
        _args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        let module = self
            .module
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let actor = caller.actor.cloned();
        let tool = self.tool;
        async move {
            let module = module.ok_or_else(|| Error::internal_msg("no module to hook"))?;
            if tool {
                return module
                    .handle_tool("config-get", r#"{"key":"x"}"#)
                    .await
                    .map_err(Error::from);
            }
            let decision = module
                .on_hook(actor.as_ref(), HookName::Authorize, "{}")
                .await
                .map_err(Error::from)?;
            Ok(serde_json::json!(format!("{decision:?}")).to_string())
        }
    }
}

/// A hook raised by a module's own `core-call` does not reach that module
/// (it waits on no lock the call holds): the module has no opinion on it,
/// and the call answers instead of timing out.
///
/// # Errors
///
/// When the component is not built or does not load, or the call does
/// not return within 10 s.
///
/// # Panics
///
/// When the host breaks the rule.
pub fn skips_module_when_its_own_core_call_raises_hook<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11 (`authorize` is a hook modules subscribe to)
    let (answered, status) = reentered(new_store(), false)?;
    assert_eq!(
        (answered, status.enabled),
        (Ok(r#""Continue""#.to_owned()), true),
        "the module's own hook is skipped"
    );
    Ok(())
}

/// A tool of a module called from inside its own `core-call` is refused
/// at once rather than waiting on the lock its call holds; the refusal
/// does not count toward disabling the module.
///
/// # Errors
///
/// As [`skips_module_when_its_own_core_call_raises_hook`].
///
/// # Panics
///
/// When the host breaks the rule.
pub fn refuses_tool_when_module_own_core_call_calls_it<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-11
    let (answered, status) = reentered(new_store(), true)?;
    assert_eq!(
        (
            module_error(&answered).map(|(code, _)| code),
            status.enabled
        ),
        (Some("internal"), true),
        "a re-entered tool is refused without timing out"
    );
    Ok(())
}

/// What `allimports`' `core-call` answered when its core reached back
/// into it (a hook, or a tool when `tool`), and its status after.
fn reentered<S: ModuleStore + 'static>(
    store: S,
    tool: bool,
) -> Result<(Result<String, ModuleError>, ModuleStatus), Failure> {
    within(TIMEOUT_CHECK_LIMIT, async move || {
        let core = HookingCore {
            module: Arc::new(std::sync::Mutex::new(None)),
            tool,
        };
        let host = host_within(store, short_timeout())?.with_core_calls(core.clone())?;
        let module = load(&host, "allimports").await?;
        let hooked = |module| {
            *core
                .module
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = module;
        };
        hooked(Some(module.clone()));
        let answered = module
            .handle_tool_as(
                &user("carol", UserRole::User)?,
                "core-call",
                &core_call_args("projects", "list", "{}"),
            )
            .await;
        let status = module.status();
        // The module holds its core, which holds the module: break the
        // cycle so its scheduler stops.
        hooked(None);
        Ok::<_, Failure>((answered, status))
    })?
}

// ---- MOD-12 -----------------------------------------------------------------

/// A host over a fresh store with its compile cache in `dir`.
fn cached_host<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
    dir: &Path,
) -> Result<Host<S>, Failure> {
    Ok(host(new_store)?.with_compile_cache(dir)?)
}

/// Every file below `dir`.
fn files_below(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .map(|path| if path.is_dir() { files_below(&path) } else { 1 })
            .sum()
    })
}

/// An empty cache compiles.
///
/// # Errors
///
/// When the component is not built or does not load, or the cache
/// directory cannot be made.
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn compiles_component_when_cache_is_empty<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-12
    let dir = tempfile::tempdir()?;
    let first = cached_host(new_store, dir.path())?;
    let _probe = load(&first, "probe").await?;
    assert_eq!(
        first.compile_stats(),
        CompileStats {
            compiled: 1,
            cached: 0
        },
        "an empty cache compiles"
    );
    Ok(())
}

/// The second start does not compile.
///
/// # Errors
///
/// As [`compiles_component_when_cache_is_empty`].
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn skips_compiling_when_second_host_loads_same_component<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-12
    let dir = tempfile::tempdir()?;
    let _first = load(&cached_host(new_store, dir.path())?, "probe").await?;
    let second = cached_host(new_store, dir.path())?;
    let _again = load(&second, "probe").await?;
    assert_eq!(
        second.compile_stats(),
        CompileStats {
            compiled: 0,
            cached: 1
        },
        "the second start takes the component from the cache"
    );
    Ok(())
}

/// Keyed by the component: another one compiles.
///
/// # Errors
///
/// As [`compiles_component_when_cache_is_empty`].
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn compiles_again_when_component_differs<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-12
    let dir = tempfile::tempdir()?;
    let _first = load(&cached_host(new_store, dir.path())?, "probe").await?;
    let second = cached_host(new_store, dir.path())?;
    let _trap = load(&second, "trap").await?;
    assert_eq!(
        second.compile_stats(),
        CompileStats {
            compiled: 1,
            cached: 0
        },
        "another component compiles"
    );
    Ok(())
}

/// A component from the cache runs.
///
/// # Errors
///
/// As [`compiles_component_when_cache_is_empty`].
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn answers_calls_when_component_comes_from_cache<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-12
    let dir = tempfile::tempdir()?;
    let _first = load(&cached_host(new_store, dir.path())?, "trap").await?;
    let again = load(&cached_host(new_store, dir.path())?, "trap").await?;
    assert_eq!(
        again.handle_tool("ok", "").await,
        Ok(r#""ok""#.to_owned()),
        "a cached component answers"
    );
    Ok(())
}

/// The compiled component is kept in the cache directory.
///
/// # Errors
///
/// As [`compiles_component_when_cache_is_empty`].
///
/// # Panics
///
/// When the host breaks the rule.
pub async fn keeps_compiled_component_in_cache_dir_when_loaded<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-12
    let dir = tempfile::tempdir()?;
    let _probe = load(&cached_host(new_store, dir.path())?, "probe").await?;
    assert!(
        files_below(dir.path()) > 0,
        "the compiled component is in the cache directory"
    );
    Ok(())
}

/// The compile cache holds native code wasmtime runs without checking
/// it, so the host makes a missing cache directory private to the daemon.
///
/// # Errors
///
/// When the host cannot be built or the directories cannot be made.
///
/// # Panics
///
/// When the host breaks the rule.
pub fn makes_cache_dir_private_when_it_is_missing<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-12 (nothing else may write code the daemon runs)
    use std::os::unix::fs::PermissionsExt as _;
    let data = tempfile::tempdir()?;
    let dir = data.path().join("cache");
    let _host = cached_host(new_store, &dir)?;
    let mode = std::fs::metadata(&dir)?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "the cache directory is the daemon's only");
    Ok(())
}

/// A cache directory of the daemon's that others may write to is made
/// private before anything is cached in it.
///
/// # Errors
///
/// When the host cannot be built or the directory cannot be made.
///
/// # Panics
///
/// When the host breaks the rule.
pub fn narrows_cache_dir_when_others_may_write_to_it<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-12
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))?;
    let _host = cached_host(new_store, dir.path())?;
    let mode = std::fs::metadata(dir.path())?.permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "the cache directory is the daemon's only");
    Ok(())
}

/// A cache directory that is a symlink is refused: the host does not
/// start on it.
///
/// # Errors
///
/// When the host cannot be built or the directories cannot be made.
///
/// # Panics
///
/// When the host breaks the rule.
pub fn refuses_cache_when_dir_is_a_symlink<S: ModuleStore + 'static>(
    new_store: &(impl Fn() -> S + Sync),
) -> Checked {
    // TC: MOD-12
    let data = tempfile::tempdir()?;
    let target = data.path().join("elsewhere");
    std::fs::create_dir(&target)?;
    let link = data.path().join("cache");
    std::os::unix::fs::symlink(&target, &link)?;
    let refused = host(new_store)?.with_compile_cache(&link);
    assert!(
        matches!(&refused, Err(ModuleError::Load { reason, .. }) if reason.contains("not a directory")),
        "a symlinked cache is refused: {:?}",
        refused.err()
    );
    Ok(())
}
