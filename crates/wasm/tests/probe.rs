#![cfg(test)]
//! MOD-01, MOD-03, MOD-10 and the host's limits: the module host drives the built
//! `probe` component (`modules/testing/probe`) against the core's
//! `InMemoryStore` (its key space) and a fake timer. `store-query` through
//! the host needs SQLite and lives with the composition root
//! (`crates/liftoffd/tests/modules.rs`, MOD-02): adapters do not depend on
//! each other.
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`): `cargo build --target wasm32-wasip2 --release` in `modules/`. The
//! tests read it from the directory in `WASM_MODULES_DIR`, which the
//! just recipes set, else from `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use liftoff_core::Error;
use liftoff_core::domain::ModuleName;
use liftoff_core::fakes::InMemoryStore;
use liftoff_core::ports::ModuleStore as _;
use liftoff_wasm::{
    Limits, LogLevel, MAX_KEY_BYTES, MAX_LOG_BYTES, MAX_LOG_LINES_PER_CALL, MAX_SCHEDULES,
    MAX_VALUE_BYTES, MIN_PERIOD_SECS, Module, ModuleConfig, ModuleError, ModuleHost, ModuleLog,
    Timer,
};
use tokio::sync::oneshot;

// ---- fakes ------------------------------------------------------------------

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Default)]
struct Clock {
    now: Duration,
    sleepers: Vec<(Duration, oneshot::Sender<()>)>,
}

/// A [`Timer`] that only moves when told to: [`ManualTimer::advance`]
/// wakes every sleeper whose deadline has come.
#[derive(Debug, Clone, Default)]
struct ManualTimer(Arc<Mutex<Clock>>);

impl ManualTimer {
    fn advance(&self, by: Duration) {
        let due = {
            let mut clock = lock(&self.0);
            clock.now = clock.now.saturating_add(by);
            let now = clock.now;
            let (due, waiting): (Vec<_>, Vec<_>) = clock
                .sleepers
                .drain(..)
                .partition(|(deadline, _)| *deadline <= now);
            clock.sleepers = waiting;
            due
        };
        for (_, wake) in due {
            let _gone = wake.send(());
        }
    }

    /// How many sleeps are pending (dropped sleeps do not count).
    fn sleepers(&self) -> usize {
        lock(&self.0)
            .sleepers
            .iter()
            .filter(|(_, wake)| !wake.is_closed())
            .count()
    }
}

impl Timer for ManualTimer {
    fn sleep(&self, period: Duration) -> impl Future<Output = ()> + Send {
        let (wake, woken) = oneshot::channel();
        {
            let mut clock = lock(&self.0);
            let deadline = clock.now.saturating_add(period);
            clock.sleepers.push((deadline, wake));
        }
        async move {
            let _woken = woken.await;
        }
    }
}

/// A [`ModuleLog`] that keeps every line; with [`RecordingLog::advancing`]
/// it also moves a timer whenever a line ends in ` begin`, i.e. while a
/// tick of the probe runs.
/// A logged line: module, level, message.
type Line = (String, LogLevel, String);

#[derive(Debug, Clone, Default)]
struct RecordingLog {
    lines: Arc<Mutex<Vec<Line>>>,
    during_tick: Option<(ManualTimer, Duration)>,
}

impl RecordingLog {
    fn advancing(timer: &ManualTimer, by: Duration) -> Self {
        Self {
            lines: Arc::default(),
            during_tick: Some((timer.clone(), by)),
        }
    }

    fn lines(&self) -> Vec<Line> {
        lock(&self.lines).clone()
    }
}

impl ModuleLog for RecordingLog {
    fn log(&self, module: &ModuleName, level: LogLevel, message: &str) {
        lock(&self.lines).push((module.to_string(), level, message.to_owned()));
        if let Some((timer, by)) = &self.during_tick
            && message.ends_with(" begin")
        {
            timer.advance(*by);
        }
    }
}

// ---- helpers ----------------------------------------------------------------

type Host = ModuleHost<InMemoryStore, ManualTimer, RecordingLog>;
type Probe = Module<InMemoryStore, RecordingLog>;

const PERIOD: Duration = Duration::from_secs(60);

fn component(name: &str) -> Vec<u8> {
    let dir = std::env::var_os("WASM_MODULES_DIR").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../modules/target/wasm32-wasip2/release")
        },
        PathBuf::from,
    );
    let path = dir.join(format!("{name}.wasm"));
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; build the modules first with `just modules`",
            path.display()
        )
    })
}

fn limits() -> Limits {
    Limits {
        max_memory_bytes: 64 * 1024 * 1024,
        call_timeout: Duration::from_secs(10),
    }
}

fn module(name: &str) -> ModuleName {
    ModuleName::new(name).unwrap()
}

/// The module key space of the core's fake store.
#[expect(clippy::unused_async, reason = "keeps the call sites of a store")]
async fn store() -> InMemoryStore {
    InMemoryStore::new()
}

fn host(store: &InMemoryStore, timer: &ManualTimer, log: &RecordingLog) -> Host {
    ModuleHost::new(store.clone(), timer.clone(), log.clone(), limits()).unwrap()
}

/// The probe component loaded as module `name`.
async fn load(host: &Host, name: &str) -> Probe {
    host.load(module(name), &component("probe"), ModuleConfig::new())
        .await
        .unwrap()
}

async fn kv_set(module: &Probe, key: &str, value: &str) {
    let args = format!(r#"{{"key":"{key}","value":"{value}"}}"#);
    module.handle_tool("kv-set", &args).await.unwrap();
}

async fn kv_get(module: &Probe, key: &str) -> String {
    module
        .handle_tool("kv-get", &format!(r#"{{"key":"{key}"}}"#))
        .await
        .unwrap()
}

async fn schedule(module: &Probe, name: &str, every_secs: u32) {
    module
        .handle_tool(
            "schedule",
            &format!(r#"{{"name":"{name}","every_secs":{every_secs}}}"#),
        )
        .await
        .unwrap();
}

async fn schedule_sweep(module: &Probe) {
    schedule(module, "sweep", 60).await;
}

/// Waits (real time, at most 10 s) until `done` holds.
async fn eventually(what: &str, mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..1000 {
        if done().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting until {what}");
}

/// How many times the probe's `tick(sweep)` ran, from its own key.
async fn sweeps(store: &InMemoryStore) -> Option<String> {
    store
        .module_get(&module("probe"), "ticks:sweep")
        .await
        .unwrap()
        .map(|bytes| String::from_utf8(bytes).unwrap())
}

/// The probe scheduled `sweep` every 60 s and its scheduler sleeps.
async fn sweeping(store: &InMemoryStore, timer: &ManualTimer, log: &RecordingLog) -> Probe {
    let probe = load(&host(store, timer, log), "probe").await;
    schedule_sweep(&probe).await;
    eventually("the scheduler sleeps", async || timer.sleepers() == 1).await;
    probe
}

fn trapped(result: &Result<String, ModuleError>) -> bool {
    matches!(result, Err(ModuleError::Trap { .. }))
}

/// The probe's log lines (messages only).
fn probe_lines(log: &RecordingLog) -> Vec<String> {
    log.lines()
        .into_iter()
        .filter(|(module, _, _)| module == "probe")
        .map(|(_, _, message)| message)
        .collect()
}

/// Real time for the scheduler task to act on what it was told.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

// ---- MOD-01 -----------------------------------------------------------------

mod store_keys {
    use super::*;

    // TC: MOD-01 (`store-set` then `store-get` in one module)
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_back_value_when_probe_set_it() {
        let host = host(
            &store().await,
            &ManualTimer::default(),
            &RecordingLog::default(),
        );
        let probe = load(&host, "probe").await;
        kv_set(&probe, "greeting", "hi").await;
        assert_eq!(kv_get(&probe, "greeting").await, r#""hi""#);
    }

    // TC: MOD-01
    #[tokio::test(flavor = "multi_thread")]
    async fn misses_key_of_other_module_when_only_probe_set_it() {
        let host = host(
            &store().await,
            &ManualTimer::default(),
            &RecordingLog::default(),
        );
        let probe = load(&host, "probe").await;
        let other = load(&host, "other").await;
        kv_set(&probe, "greeting", "hi").await;
        assert_eq!(kv_get(&other, "greeting").await, "null");
    }

    // TC: MOD-01
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_values_apart_when_two_modules_set_one_key() {
        let host = host(
            &store().await,
            &ManualTimer::default(),
            &RecordingLog::default(),
        );
        let probe = load(&host, "probe").await;
        let other = load(&host, "other").await;
        kv_set(&probe, "greeting", "hi").await;
        kv_set(&other, "greeting", "bye").await;
        assert_eq!(
            (
                kv_get(&probe, "greeting").await,
                kv_get(&other, "greeting").await
            ),
            (r#""hi""#.to_owned(), r#""bye""#.to_owned())
        );
    }
}

// ---- MOD-03 -----------------------------------------------------------------

mod scheduler {
    use super::*;

    // TC: MOD-03
    #[tokio::test(flavor = "multi_thread")]
    async fn ticks_once_when_period_elapses() {
        let (store, timer, log) = (
            store().await,
            ManualTimer::default(),
            RecordingLog::default(),
        );
        let _probe = sweeping(&store, &timer, &log).await;
        timer.advance(PERIOD);
        eventually("sweep ran once", async || {
            sweeps(&store).await.as_deref() == Some("1")
        })
        .await;
    }

    // TC: MOD-03
    #[tokio::test(flavor = "multi_thread")]
    async fn does_not_tick_when_period_has_not_elapsed() {
        let (store, timer, log) = (
            store().await,
            ManualTimer::default(),
            RecordingLog::default(),
        );
        let _probe = sweeping(&store, &timer, &log).await;
        timer.advance(PERIOD.saturating_sub(Duration::from_secs(1)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!((sweeps(&store).await, timer.sleepers()), (None, 1));
    }

    // TC: MOD-03
    #[tokio::test(flavor = "multi_thread")]
    async fn ticks_again_when_next_period_elapses() {
        let (store, timer, log) = (
            store().await,
            ManualTimer::default(),
            RecordingLog::default(),
        );
        let _probe = sweeping(&store, &timer, &log).await;
        timer.advance(PERIOD);
        eventually("sweep ran once and sleeps again", async || {
            sweeps(&store).await.as_deref() == Some("1") && timer.sleepers() == 1
        })
        .await;
        timer.advance(PERIOD);
        eventually("sweep ran twice", async || {
            sweeps(&store).await.as_deref() == Some("2")
        })
        .await;
    }

    // TC: MOD-03 (no parallel or piled-up calls of one name: two more
    // periods pass while a tick runs, and that tick is still the only one;
    // the next period starts when it returned)
    #[tokio::test(flavor = "multi_thread")]
    async fn ticks_once_when_two_periods_pass_during_tick() {
        let (store, timer) = (store().await, ManualTimer::default());
        let log = RecordingLog::advancing(&timer, PERIOD.saturating_mul(2));
        let _probe = sweeping(&store, &timer, &log).await;
        timer.advance(PERIOD);
        eventually("sweep ran and sleeps again", async || {
            sweeps(&store).await.is_some() && timer.sleepers() == 1
        })
        .await;
        settle().await;
        assert_eq!(
            (sweeps(&store).await, probe_lines(&log), timer.sleepers()),
            (
                Some("1".to_owned()),
                vec!["tick sweep begin".to_owned(), "tick sweep end".to_owned()],
                1
            )
        );
    }

    // TC: MOD-03 (scheduling a name again replaces its period: one sleep
    // per name, however often it is scheduled)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_one_sleep_when_name_is_rescheduled() {
        let (store, timer, log) = (
            store().await,
            ManualTimer::default(),
            RecordingLog::default(),
        );
        let probe = sweeping(&store, &timer, &log).await;
        for _ in 0..5 {
            schedule_sweep(&probe).await;
        }
        settle().await;
        assert_eq!(timer.sleepers(), 1);
    }

    // TC: MOD-03 (`every-secs = 0` stops a name)
    #[tokio::test(flavor = "multi_thread")]
    async fn stops_ticking_when_rescheduled_with_zero() {
        let (store, timer, log) = (
            store().await,
            ManualTimer::default(),
            RecordingLog::default(),
        );
        let probe = sweeping(&store, &timer, &log).await;
        schedule(&probe, "sweep", 0).await;
        eventually("the sleep is gone", async || timer.sleepers() == 0).await;
        timer.advance(PERIOD);
        settle().await;
        assert_eq!(sweeps(&store).await, None);
    }

    // TC: MOD-03 (a module keeps at most `MAX_SCHEDULES` names: the daemon
    // holds nothing more for it)
    #[tokio::test(flavor = "multi_thread")]
    async fn ignores_new_names_when_module_has_max_schedules() {
        let (store, timer, log) = (
            store().await,
            ManualTimer::default(),
            RecordingLog::default(),
        );
        let probe = load(&host(&store, &timer, &log), "probe").await;
        for index in 0..=MAX_SCHEDULES {
            schedule(&probe, &format!("n{index}"), 60).await;
        }
        eventually("the names sleep", async || {
            timer.sleepers() == MAX_SCHEDULES
        })
        .await;
        settle().await;
        assert_eq!(timer.sleepers(), MAX_SCHEDULES);
    }

    // TC: MOD-03 (no tick storm: a period under `MIN_PERIOD_SECS` is
    // ignored)
    #[tokio::test(flavor = "multi_thread")]
    async fn ignores_schedule_when_period_is_below_minimum() {
        let (store, timer, log) = (
            store().await,
            ManualTimer::default(),
            RecordingLog::default(),
        );
        let probe = load(&host(&store, &timer, &log), "probe").await;
        schedule(&probe, "sweep", MIN_PERIOD_SECS.saturating_sub(1)).await;
        settle().await;
        assert_eq!(timer.sleepers(), 0);
    }

    // TC: none (hardening: a module's `log` reaches the daemon
    // tagged with the module)
    #[tokio::test(flavor = "multi_thread")]
    async fn logs_tick_lines_tagged_with_module_when_period_elapses() {
        let (store, timer, log) = (
            store().await,
            ManualTimer::default(),
            RecordingLog::default(),
        );
        let _probe = sweeping(&store, &timer, &log).await;
        timer.advance(PERIOD);
        eventually("sweep ran once", async || sweeps(&store).await.is_some()).await;
        eventually("sweep logged its end", async || log.lines().len() == 2).await;
        assert_eq!(
            log.lines(),
            vec![
                (
                    "probe".to_owned(),
                    LogLevel::Info,
                    "tick sweep begin".to_owned()
                ),
                (
                    "probe".to_owned(),
                    LogLevel::Info,
                    "tick sweep end".to_owned()
                ),
            ]
        );
    }
}

// ---- traps and limits -------------------------------------------------------

mod traps {
    use super::*;

    fn host_of(store: &InMemoryStore) -> Host {
        host(store, &ManualTimer::default(), &RecordingLog::default())
    }

    // TC: none (hardening: a trap fails its call only, the next call
    // gets a fresh instance; MOD-09's disabling after three is)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_next_call_when_previous_call_trapped() {
        let store = store().await;
        let probe = load(&host_of(&store), "probe").await;
        kv_set(&probe, "greeting", "hi").await;
        let trap = probe.handle_tool("trap", "{}").await;
        assert_eq!(
            (trapped(&trap), kv_get(&probe, "greeting").await),
            (true, r#""hi""#.to_owned()),
            "{trap:?}"
        );
    }

    // TC: none (hardening: a failing store traps `store-get`, which
    // has no error, as a failure of the host, not of the module; the
    // module is not disabled by it)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_next_call_when_store_failed_once() {
        let store = store().await;
        let probe = load(&host_of(&store), "probe").await;
        kv_set(&probe, "greeting", "hi").await;
        store.fail_next("module_get", Error::internal_msg("disk gone"));
        let failed = probe.handle_tool("kv-get", r#"{"key":"greeting"}"#).await;
        assert_eq!(
            (
                matches!(failed, Err(ModuleError::HostFailed { .. })),
                kv_get(&probe, "greeting").await
            ),
            (true, r#""hi""#.to_owned()),
            "{failed:?}"
        );
    }

    // TC: MOD-10 (the memory cap holds: growing past it traps, the module
    // lives on)
    #[tokio::test(flavor = "multi_thread")]
    async fn traps_when_module_grows_past_memory_cap() {
        let store = store().await;
        let probe = load(&host_of(&store), "probe").await;
        let over = limits().max_memory_bytes.saturating_mul(2);
        let grown = probe
            .handle_tool("grow", &format!(r#"{{"bytes":{over}}}"#))
            .await;
        let small = probe.handle_tool("grow", r#"{"bytes":1048576}"#).await;
        assert_eq!(
            (trapped(&grown), small),
            (true, Ok("null".to_owned())),
            "{grown:?}"
        );
    }

    // TC: none (hardening: a value over `MAX_VALUE_BYTES` traps and is
    // not stored)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_nothing_when_value_is_over_cap() {
        let store = store().await;
        let probe = load(&host_of(&store), "probe").await;
        let value = "x".repeat(MAX_VALUE_BYTES.saturating_add(1));
        let refused = probe
            .handle_tool("kv-set", &format!(r#"{{"key":"big","value":"{value}"}}"#))
            .await;
        assert_eq!(
            (trapped(&refused), kv_get(&probe, "big").await),
            (true, "null".to_owned())
        );
    }

    // TC: none (hardening: a key over `MAX_KEY_BYTES` traps)
    #[tokio::test(flavor = "multi_thread")]
    async fn traps_when_key_is_over_cap() {
        let store = store().await;
        let probe = load(&host_of(&store), "probe").await;
        let key = "k".repeat(MAX_KEY_BYTES.saturating_add(1));
        let refused = probe
            .handle_tool("kv-set", &format!(r#"{{"key":"{key}","value":"v"}}"#))
            .await;
        assert!(trapped(&refused), "{refused:?}");
    }

    // TC: none (hardening: the caller gets the trap's short reason,
    // not wasmtime's chain with its backtrace)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_short_reason_when_module_traps() {
        let store = store().await;
        let probe = load(&host_of(&store), "probe").await;
        let trap = probe.handle_tool("trap", "{}").await;
        let short = matches!(
            &trap,
            Err(ModuleError::Trap { module, reason })
                if module == "probe" && reason.starts_with("wasm trap") && !reason.contains('\n')
        );
        assert!(short, "{trap:?}");
    }

    // TC: none (hardening: a host trap answers the host's message
    // only)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_host_message_when_store_fails() {
        let store = store().await;
        let probe = load(&host_of(&store), "probe").await;
        store.fail_next("module_get", Error::internal_msg("disk gone"));
        let failed = probe.handle_tool("kv-get", r#"{"key":"greeting"}"#).await;
        assert_eq!(
            failed,
            Err(ModuleError::HostFailed {
                module: "probe".to_owned(),
                reason: "the module store failed".to_owned()
            })
        );
    }
}

// ---- the `log` import -------------------------------------------------------

mod logs {
    use super::*;

    /// The probe's `log` tool called once; `message` is JSON-escaped
    /// already.
    async fn log_from_probe(log: &RecordingLog, message: &str, times: usize) {
        let probe = load(&host(&store().await, &ManualTimer::default(), log), "probe").await;
        let args = format!(r#"{{"message":"{message}","times":{times}}}"#);
        probe.handle_tool("log", &args).await.unwrap();
    }

    // TC: none (hardening: a module cannot forge lines of the daemon's
    // log with line breaks)
    #[tokio::test(flavor = "multi_thread")]
    async fn escapes_control_characters_when_module_logs_them() {
        let log = RecordingLog::default();
        log_from_probe(&log, r"a\nb\rc\u001b", 1).await;
        assert_eq!(probe_lines(&log), vec![r"a\nb\rc\u{1b}".to_owned()]);
    }

    // TC: none (hardening: a line has a size cap)
    #[tokio::test(flavor = "multi_thread")]
    async fn cuts_line_when_message_is_over_cap() {
        let log = RecordingLog::default();
        log_from_probe(&log, &"x".repeat(MAX_LOG_BYTES.saturating_add(10)), 1).await;
        assert_eq!(
            probe_lines(&log),
            vec![format!("{}…", "x".repeat(MAX_LOG_BYTES))]
        );
    }

    // TC: none (hardening: one call writes at most
    // `MAX_LOG_LINES_PER_CALL` lines)
    #[tokio::test(flavor = "multi_thread")]
    async fn drops_lines_past_cap_when_call_logs_in_loop() {
        let log = RecordingLog::default();
        log_from_probe(&log, "again", MAX_LOG_LINES_PER_CALL.saturating_add(5)).await;
        assert_eq!(probe_lines(&log).len(), MAX_LOG_LINES_PER_CALL);
    }

    // TC: none (hardening: the line cap is per call)
    #[tokio::test(flavor = "multi_thread")]
    async fn logs_again_when_next_call_begins() {
        let log = RecordingLog::default();
        let probe = load(
            &host(&store().await, &ManualTimer::default(), &log),
            "probe",
        )
        .await;
        let flood = format!(r#"{{"message":"again","times":{MAX_LOG_LINES_PER_CALL}}}"#);
        probe.handle_tool("log", &flood).await.unwrap();
        probe
            .handle_tool("log", r#"{"message":"next"}"#)
            .await
            .unwrap();
        assert_eq!(probe_lines(&log).last().map(String::as_str), Some("next"));
    }
}
