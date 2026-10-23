#![cfg(test)]
//! The CLI's module host: `liftoff::local::cli_host` over `LocalModuleStore`, a SQLite file
//! (`modules.db` of the config home) in a tempdir per test.
//!
//! - LCL-06: the module host contract of `liftoff-wasm` (MOD-06…MOD-12,
//!   `module_host_contract_tests!`) over the CLI's store. The contract
//!   builds its own `ModuleHost` (with the daemon's scheduler and its own
//!   `core-call` bridge) over that store, so it covers the store, not
//!   `cli_host`. Each rule of the host itself therefore runs again
//!   through the CLI's host below (`cli_host_guards`): the manifest
//!   (MOD-06), `http_allow` (MOD-07), timeouts (MOD-08), traps (MOD-09),
//!   the memory cap (MOD-10, at the limits a run uses, `local_host`), the
//!   compile cache of a run (MOD-12, `local_host`); so do MOD-01 and
//!   MOD-02 (the store's rules, through the `probe` and `allimports`
//!   contract components) and the `ModuleStore` half of the core's
//!   `store_contract` (`module_store_contract`). MOD-11 through the CLI
//!   host is LCL-07 (`core_call`: its bridge serves no method). MOD-03
//!   (the scheduler) does not apply: the CLI host has none; MOD-04 and
//!   MOD-05 are rules of the core's `Authorizer`, not of a host.
//! - LCL-07: no scheduler (a module's `schedule` never sleeps on the timer
//!   and never ticks) and no `core-call` that changes an installation.
//!
//! The components are built by `just modules`;
//! `liftoff_wasm::testing::component` reads them from `WASM_MODULES_DIR`,
//! else from `modules/target/wasm32-wasip2/release/`.

use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::path::Path;
use std::sync::Arc;

use liftoff::local::{CliHost, LocalCoreCalls, LocalModuleStore, cli_host};
use liftoff_core::Error;
use liftoff_core::domain::{ModuleName, UserRole};
use liftoff_core::fakes::Records;
use liftoff_core::ports::ModuleStore;
use liftoff_wasm::testing::{component, limits};
use liftoff_wasm::{Module, ModuleConfig, ModuleError, Timer, TracingLog};

/// The CLI's store in a tempdir of its own, which lives as long as it.
#[derive(Debug)]
struct OnDisk {
    _dir: tempfile::TempDir,
    store: LocalModuleStore,
}

/// A fresh `modules.db`.
fn on_disk() -> OnDisk {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalModuleStore::open(dir.path().join("modules.db")).unwrap();
    OnDisk { _dir: dir, store }
}

impl ModuleStore for OnDisk {
    fn module_get(
        &self,
        module: &ModuleName,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        self.store.module_get(module, key)
    }

    fn module_set(
        &self,
        module: &ModuleName,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.store.module_set(module, key, value)
    }

    fn module_query(
        &self,
        module: &ModuleName,
        sql: &str,
        params: &[String],
    ) -> impl Future<Output = Result<String, Error>> + Send {
        self.store.module_query(module, sql, params)
    }

    fn module_purge(&self, module: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        self.store.module_purge(module)
    }
}

/// TC: LCL-06. The contract of the module host (MOD-06…MOD-12) over the
/// CLI's store.
mod contract {
    liftoff_wasm::module_host_contract_tests!(super::on_disk);
}

/// TC: LCL-06, MOD-01. The `ModuleStore` rules of the core's
/// `store_contract` over `modules.db`, as `crates/store` runs them.
mod module_store_contract {
    liftoff_core::store_contract_tests!(
        @rules super::on_disk;
        misses_key_of_other_module_when_only_one_set_it,
        keeps_module_keys_apart_when_two_modules_set_one_key,
        replaces_module_value_when_set_again,
        forgets_module_keys_when_purged,
        keeps_other_module_keys_when_one_is_purged,
    );
}

// ---- the CLI host -------------------------------------------------------------

/// A [`Timer`] that counts the sleeps asked of it and never wakes.
#[derive(Debug, Clone, Default)]
struct CountingTimer(Arc<AtomicUsize>);

impl CountingTimer {
    fn sleeps(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Timer for CountingTimer {
    fn sleep(&self, _period: Duration) -> impl Future<Output = ()> + Send {
        self.0.fetch_add(1, Ordering::SeqCst);
        core::future::pending()
    }
}

type Loaded = Module<LocalModuleStore, TracingLog, LocalCoreCalls>;

/// The CLI host over the store at `path`, scheduling (never) on `timer`.
fn host_at(path: &Path, timer: &CountingTimer) -> CliHost<CountingTimer> {
    cli_host(
        LocalModuleStore::open(path).unwrap(),
        timer.clone(),
        limits(),
    )
    .unwrap()
}

/// The contract component `component` loaded as the module `name`.
async fn load<T: Timer>(host: &CliHost<T>, name: &str, component_name: &str) -> Loaded {
    host.load(
        ModuleName::new(name).unwrap(),
        &component(component_name).unwrap(),
        ModuleConfig::new(),
    )
    .await
    .unwrap()
}

/// A CLI host in `dir` with `probe` loaded and migrated.
async fn probe_in(dir: &Path) -> (CliHost<CountingTimer>, Loaded, CountingTimer) {
    let timer = CountingTimer::default();
    let host = host_at(&dir.join("modules.db"), &timer);
    let probe = load(&host, "probe", "probe").await;
    probe.migrate(0).await.unwrap();
    (host, probe, timer)
}

fn kv_set_args(key: &str, value: &str) -> String {
    serde_json::json!({"key": key, "value": value}).to_string()
}

fn kv_get_args(key: &str) -> String {
    serde_json::json!({"key": key}).to_string()
}

fn query_args(sql: &str) -> String {
    serde_json::json!({"sql": sql, "params": []}).to_string()
}

/// The code of a module's own error.
fn code(result: &Result<String, ModuleError>) -> Option<&str> {
    match result {
        Err(ModuleError::Module { code, .. }) => Some(code.as_str()),
        _ => None,
    }
}

/// TC: MOD-01 on the CLI host: each module's key space is its own, and it
/// survives the CLI process (the file).
mod store_keys {
    use super::*;

    // TC: MOD-01
    #[tokio::test(flavor = "multi_thread")]
    async fn misses_key_of_other_module_when_only_one_set_it() {
        let dir = tempfile::tempdir().unwrap();
        let (host, probe, _timer) = probe_in(dir.path()).await;
        let other = load(&host, "allimports", "allimports").await;
        probe
            .handle_tool("kv-set", &kv_set_args("greeting", "hi"))
            .await
            .unwrap();
        assert_eq!(
            other.handle_tool("kv-get", &kv_get_args("greeting")).await,
            Ok("null".to_owned())
        );
    }

    // TC: MOD-01 (`modules.db` keeps a module's keys
    // between two runs of the CLI)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_value_when_modules_db_is_opened_again() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (_host, probe, _timer) = probe_in(dir.path()).await;
            probe
                .handle_tool("kv-set", &kv_set_args("greeting", "hi"))
                .await
                .unwrap();
        }
        // The next run loads the installed module without migrating it
        // again, as `ModuleService::restore` does.
        let timer = CountingTimer::default();
        let host = host_at(&dir.path().join("modules.db"), &timer);
        let probe = load(&host, "probe", "probe").await;
        assert_eq!(
            probe.handle_tool("kv-get", &kv_get_args("greeting")).await,
            Ok("\"hi\"".to_owned())
        );
    }
}

/// TC: MOD-02 on the CLI host: `store-query` reaches the module's own
/// tables only, and a refusal is an error to the module.
mod store_query {
    use super::*;

    // TC: MOD-02
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_rows_when_module_reads_own_table() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, probe, _timer) = probe_in(dir.path()).await;
        probe
            .handle_tool("items-add", r#"{"name":"apple"}"#)
            .await
            .unwrap();
        assert_eq!(
            probe.handle_tool("items-list", "{}").await,
            Ok(r#"[{"name":"apple"}]"#.to_owned())
        );
    }

    // TC: MOD-02 (`users` is no table of the module)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_module_reads_users() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, probe, _timer) = probe_in(dir.path()).await;
        let refused = probe.handle_tool("read-users", "{}").await;
        assert_eq!(code(&refused), Some("forbidden"), "{refused:?}");
    }

    // TC: MOD-02 (and the host answers the next call)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_own_query_when_module_was_refused_before() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, probe, _timer) = probe_in(dir.path()).await;
        let _refused = probe.handle_tool("read-users", "{}").await;
        assert_eq!(
            probe.handle_tool("items-list", "{}").await,
            Ok("[]".to_owned())
        );
    }

    // TC: MOD-02 (`ATTACH`, `PRAGMA` and the schema are refused explicitly,
    // as in the daemon's store: one authorizer for both)
    #[rstest::rstest]
    #[case::attach("ATTACH DATABASE 'other.db' AS other")]
    #[case::pragma("PRAGMA table_info(m_probe_items)")]
    #[case::schema("SELECT name FROM sqlite_master")]
    #[case::other_module("CREATE TABLE m_allimports_items (name TEXT)")]
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_statement_reaches_outside_module_tables(#[case] sql: &str) {
        let dir = tempfile::tempdir().unwrap();
        let (_host, probe, _timer) = probe_in(dir.path()).await;
        let refused = probe.handle_tool("query", &query_args(sql)).await;
        assert_eq!(code(&refused), Some("forbidden"), "{refused:?}");
    }
}

/// TC: LCL-07. A module in the CLI host has no scheduler.
mod schedule {
    use super::*;

    // TC: LCL-07 (`schedule` has no error in the WIT: the call answers)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_call_when_module_schedules() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, probe, _timer) = probe_in(dir.path()).await;
        assert_eq!(
            probe
                .handle_tool("schedule", r#"{"name":"sweep","every_secs":60}"#)
                .await,
            Ok("null".to_owned())
        );
    }

    // TC: LCL-07 (no scheduler: nothing ever sleeps toward a `tick`)
    #[tokio::test(flavor = "multi_thread")]
    async fn never_sleeps_on_timer_when_module_schedules() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, probe, timer) = probe_in(dir.path()).await;
        let _answered = probe
            .handle_tool("schedule", r#"{"name":"sweep","every_secs":60}"#)
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(timer.sleeps(), 0);
    }
}

/// TC: LCL-07. A module in the CLI host makes no `core-call` that changes
/// an installation: there is none behind the CLI.
mod core_call {
    use super::*;

    /// `allimports`' `core-call` of `service.method` as `owner`.
    async fn core_call(service: &str, method: &str) -> Result<String, ModuleError> {
        let dir = tempfile::tempdir().unwrap();
        let timer = CountingTimer::default();
        let host = host_at(&dir.path().join("modules.db"), &timer);
        let module = load(&host, "allimports", "allimports").await;
        let owner = Records::default().user("owner", UserRole::Admin).unwrap();
        let args = serde_json::json!({"service": service, "method": method, "args": "{}"});
        module
            .handle_tool_as(&owner, "core-call", &args.to_string())
            .await
    }

    // TC: LCL-07 (the mutating methods of the core's table)
    #[rstest::rstest]
    #[case::create_session("auth", "create_session")]
    #[case::set_protected("environments", "set_protected")]
    #[case::assign("tenancy", "assign")]
    #[case::adopt("tenancy", "adopt")]
    #[tokio::test(flavor = "multi_thread")]
    async fn unavailable_when_module_calls_mutating_method(
        #[case] service: &str,
        #[case] method: &str,
    ) {
        let refused = core_call(service, method).await;
        assert_eq!(code(&refused), Some("unavailable"), "{refused:?}");
    }

    // TC: LCL-07 (the error says what to do: connect a server)
    #[tokio::test(flavor = "multi_thread")]
    async fn points_to_server_connect_when_module_calls_mutating_method() {
        let refused = core_call("environments", "set_protected").await;
        let message = match &refused {
            Err(ModuleError::Module { message, .. }) => message.clone(),
            other => format!("{other:?}"),
        };
        assert!(message.contains("lof server connect"), "{message:?}");
    }

    // TC: MOD-11 (a method outside the core's table is refused as by the
    // daemon's bridge)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_module_calls_method_outside_table() {
        let refused = core_call("store", "delete_user").await;
        assert_eq!(code(&refused), Some("forbidden"), "{refused:?}");
    }
}

/// TC: LCL-06. The host's own rules through `cli_host` and `local_host`
/// themselves (the contract above builds a host of its own).
mod cli_host_guards {
    use liftoff::local::{MODULE_LIMITS, local_host};
    use liftoff_wasm::testing::Listener;
    use liftoff_wasm::{CompileStats, Limits};

    use super::*;

    /// The host a run of local mode uses, over `dir/modules.db`, its
    /// compiled components in `dir/cache`.
    fn run_host(dir: &Path) -> CliHost {
        local_host(
            LocalModuleStore::open(dir.join("modules.db")).unwrap(),
            &dir.join("cache"),
        )
        .unwrap()
    }

    // TC: LCL-06, MOD-06
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_load_when_manifest_has_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_at(&dir.path().join("modules.db"), &CountingTimer::default());
        let loaded = host
            .load(
                ModuleName::new("badmanifest").unwrap(),
                &component("badmanifest").unwrap(),
                ModuleConfig::new(),
            )
            .await;
        assert!(
            matches!(&loaded, Err(ModuleError::Load { reason, .. }) if reason.contains("colour")),
            "{:?}",
            loaded.err()
        );
    }

    // TC: LCL-06, MOD-07
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_request_when_host_is_not_in_http_allow() {
        let listener = Listener::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let host = host_at(&dir.path().join("modules.db"), &CountingTimer::default())
            .allowing_private_networks();
        let config = ModuleConfig::from([(
            "http_allow".to_owned(),
            format!("127.0.0.1:{}", listener.port),
        )]);
        let module = host
            .load(
                ModuleName::new("badhost").unwrap(),
                &component("badhost").unwrap(),
                config,
            )
            .await
            .unwrap();
        let fetch = |url: String| {
            let module = &module;
            async move {
                let args = serde_json::json!({ "url": url }).to_string();
                module.handle_tool("fetch", &args).await
            }
        };
        let allowed = fetch(format!("http://127.0.0.1:{}/", listener.port)).await;
        let refused = fetch(format!("http://localhost:{}/", listener.port)).await;
        assert_eq!(
            (allowed, code(&refused), listener.accepted()),
            (Ok("200".to_owned()), Some("http"), 1)
        );
    }

    // TC: LCL-06, MOD-10 (at the limits a run of local mode uses)
    #[tokio::test(flavor = "multi_thread")]
    async fn traps_call_when_module_grows_past_memory_cap_of_run() {
        let dir = tempfile::tempdir().unwrap();
        let host = run_host(dir.path());
        let module = load(&host, "trap", "trap").await;
        let over = MODULE_LIMITS.max_memory_bytes.saturating_mul(2).to_string();
        let grown = module.handle_tool("grow", &over).await;
        assert!(matches!(grown, Err(ModuleError::Trap { .. })), "{grown:?}");
    }

    // TC: LCL-06, MOD-12 (the second run takes the component from the
    // cache of the config home)
    #[tokio::test(flavor = "multi_thread")]
    async fn skips_compiling_when_second_run_loads_same_component() {
        let dir = tempfile::tempdir().unwrap();
        let first = run_host(dir.path());
        let _probe = load(&first, "probe", "probe").await;
        drop(first);
        let second = run_host(dir.path());
        let _again = load(&second, "probe", "probe").await;
        assert_eq!(
            second.compile_stats(),
            CompileStats {
                compiled: 0,
                cached: 1
            }
        );
    }

    // TC: LCL-06, MOD-09
    #[tokio::test(flavor = "multi_thread")]
    async fn disables_module_when_three_calls_in_a_row_trap() {
        let dir = tempfile::tempdir().unwrap();
        let timer = CountingTimer::default();
        let host = host_at(&dir.path().join("modules.db"), &timer);
        let module = load(&host, "trap", "trap").await;
        for _ in 0..3 {
            let trapped = module.handle_tool("panic", "").await;
            assert!(
                matches!(trapped, Err(ModuleError::Trap { .. })),
                "{trapped:?}"
            );
        }
        assert!(!module.status().enabled);
    }

    // TC: LCL-06, MOD-08
    #[tokio::test(flavor = "multi_thread")]
    async fn interrupts_call_and_answers_next_when_call_runs_past_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let host = cli_host(
            LocalModuleStore::open(dir.path().join("modules.db")).unwrap(),
            CountingTimer::default(),
            Limits {
                call_timeout: Duration::from_millis(200),
                ..limits()
            },
        )
        .unwrap();
        let module = load(&host, "loop", "loop").await;
        let answers = tokio::time::timeout(Duration::from_secs(10), async {
            let spun = module.handle_tool("spin", "{}").await;
            let pinged = module.handle_tool("ping", "{}").await;
            (spun, pinged)
        })
        .await;
        assert_eq!(
            answers,
            Ok((
                Err(ModuleError::Timeout {
                    module: "loop".to_owned()
                }),
                Ok("\"pong\"".to_owned())
            ))
        );
    }
}
