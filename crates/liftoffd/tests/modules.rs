#![cfg(test)]
//! MOD-02 through the module host: the built `probe` component
//! (`modules/testing/probe`) runs `store-query` against the daemon's SQLite
//! store. It lives with the composition root because it joins two
//! adapters (`liftoff-wasm` and `liftoff-store`), which never depend on
//! each other; the host's own tests use the core's fakes
//! (`crates/wasm/tests/probe.rs`).
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); the tests read it from `WASM_MODULES_DIR`, which the just recipes
//! set, else from `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;
use std::path::PathBuf;

use liftoff_core::domain::{ModuleName, UserRole};
use liftoff_core::fakes::Records;
use liftoff_core::ports::Store as _;
use liftoff_store::SqliteStore;
use liftoff_wasm::{Limits, Module, ModuleConfig, ModuleError, ModuleHost, TokioTimer, TracingLog};

type Probe = Module<SqliteStore, TracingLog>;

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

/// An in-memory store holding user `owner`.
async fn store() -> SqliteStore {
    let store = SqliteStore::open_in_memory().unwrap();
    let owner = Records::default().user("owner", UserRole::Admin).unwrap();
    store.insert_user(&owner).await.unwrap();
    store
}

/// The probe loaded as `probe` over `store` and migrated from 0 (its
/// table `m_probe_items` exists).
async fn migrated(store: &SqliteStore) -> Probe {
    let limits = Limits {
        max_memory_bytes: 64 * 1024 * 1024,
        call_timeout: Duration::from_secs(10),
    };
    let host = ModuleHost::new(store.clone(), TokioTimer, TracingLog, limits).unwrap();
    let probe = host
        .load(
            ModuleName::new("probe").unwrap(),
            &component("probe"),
            ModuleConfig::new(),
        )
        .await
        .unwrap();
    probe.migrate(0).await.unwrap();
    probe
}

async fn query(probe: &Probe, sql: &str) -> Result<String, ModuleError> {
    let args = serde_json::json!({ "sql": sql, "params": [] }).to_string();
    probe.handle_tool("query", &args).await
}

fn forbidden(result: &Result<String, ModuleError>) -> bool {
    matches!(result, Err(ModuleError::Module { code, .. }) if code == "forbidden")
}

mod store_query {
    use super::*;

    // TC: MOD-02
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_rows_when_probe_reads_own_table() {
        let probe = migrated(&store().await).await;
        probe
            .handle_tool("items-add", r#"{"name":"apple"}"#)
            .await
            .unwrap();
        assert_eq!(
            probe.handle_tool("items-list", "{}").await.unwrap(),
            r#"[{"name":"apple"}]"#
        );
    }

    // TC: MOD-02
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_forbidden_to_module_when_probe_reads_users() {
        let probe = migrated(&store().await).await;
        let refused = probe.handle_tool("read-users", "{}").await;
        assert!(forbidden(&refused), "{refused:?}");
    }

    // TC: MOD-02 (ATTACH is refused explicitly)
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_forbidden_to_module_when_probe_attaches_database() {
        let probe = migrated(&store().await).await;
        let refused = query(&probe, "ATTACH DATABASE ':memory:' AS side").await;
        assert!(forbidden(&refused), "{refused:?}");
    }

    // TC: MOD-02 (PRAGMA is refused explicitly)
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_forbidden_to_module_when_probe_runs_pragma() {
        let probe = migrated(&store().await).await;
        let refused = query(&probe, "PRAGMA table_info(users)").await;
        assert!(forbidden(&refused), "{refused:?}");
    }

    // TC: MOD-02 (a foreign key into a core table is refused)
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_forbidden_to_module_when_probe_references_sessions() {
        let probe = migrated(&store().await).await;
        let refused = query(
            &probe,
            "CREATE TABLE m_probe_refs (user TEXT REFERENCES sessions (user_id))",
        )
        .await;
        assert!(forbidden(&refused), "{refused:?}");
    }

    // TC: MOD-02 (the module lives on after a refusal)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_serving_probe_when_it_was_refused() {
        let probe = migrated(&store().await).await;
        let _refused = probe.handle_tool("read-users", "{}").await;
        assert_eq!(probe.handle_tool("items-list", "{}").await.unwrap(), "[]");
    }

    // TC: MOD-02 (a write to a core table never runs; the store lives on)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_users_when_probe_drops_their_table() {
        let store = store().await;
        let probe = migrated(&store).await;
        let refused = query(&probe, "DROP TABLE users").await;
        assert_eq!(
            (forbidden(&refused), store.users().await.unwrap().len()),
            (true, 1),
            "{refused:?}"
        );
    }

    // TC: MOD-02 (a runaway statement is cut off and answered to the
    // module; the core is answered afterwards)
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_validation_to_module_when_probe_runs_away() {
        let store = store().await;
        let probe = migrated(&store).await;
        let refused = query(
            &probe,
            "WITH RECURSIVE m_probe_c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM m_probe_c) \
             SELECT count(*) FROM m_probe_c",
        )
        .await;
        assert_eq!(
            (
                matches!(&refused, Err(ModuleError::Module { code, .. }) if code == "validation"),
                store.users().await.map(|users| users.len()).ok()
            ),
            (true, Some(1)),
            "{refused:?}"
        );
    }
}
