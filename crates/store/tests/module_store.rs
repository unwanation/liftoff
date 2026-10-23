#![cfg(test)]
//! MOD-01, MOD-02: the module storage of `store-get`, `store-set` and
//! `store-query` over SQLite. The key space is in the `Store` contract
//! (`liftoff_core::fakes::store_contract`); the SQL authorizer is SQLite's
//! own and is checked here.

use core::time::Duration;
use liftoff_core::Error;
use liftoff_core::domain::{ModuleName, UserRole};
use liftoff_core::fakes::Records;
use liftoff_core::ports::{ModuleStore as _, Store as _};

use liftoff_store::{ModuleQueryLimits, SqliteStore};

fn module(name: &str) -> ModuleName {
    ModuleName::new(name).unwrap()
}

fn no_params() -> Vec<String> {
    Vec::new()
}

/// A store with user `owner` and the table `m_probe_items (name TEXT)` of
/// module `probe`.
async fn store_with_probe_items() -> SqliteStore {
    let store = SqliteStore::open_in_memory().unwrap();
    let owner = Records::default().user("owner", UserRole::Admin).unwrap();
    store.insert_user(&owner).await.unwrap();
    store
        .module_query(
            &module("probe"),
            "CREATE TABLE m_probe_items (name TEXT NOT NULL)",
            &no_params(),
        )
        .await
        .unwrap();
    store
}

async fn probe_query(store: &SqliteStore, sql: &str) -> Result<String, Error> {
    store
        .module_query(&module("probe"), sql, &no_params())
        .await
}

mod module_query {
    use super::*;

    // TC: MOD-02
    #[tokio::test]
    async fn returns_rows_when_module_reads_own_table() {
        let store = store_with_probe_items().await;
        store
            .module_query(
                &module("probe"),
                "INSERT INTO m_probe_items (name) VALUES (?1)",
                &["apple".to_owned()],
            )
            .await
            .unwrap();
        assert_eq!(
            probe_query(&store, "SELECT name FROM m_probe_items")
                .await
                .unwrap(),
            r#"[{"name":"apple"}]"#
        );
    }

    // TC: MOD-02 (JSON rows: integers are numbers)
    #[tokio::test]
    async fn returns_integers_as_numbers_when_rows_counted() {
        let store = store_with_probe_items().await;
        probe_query(&store, "INSERT INTO m_probe_items (name) VALUES ('apple')")
            .await
            .unwrap();
        assert_eq!(
            probe_query(&store, "SELECT count(*) AS n FROM m_probe_items")
                .await
                .unwrap(),
            r#"[{"n":1}]"#
        );
    }

    // TC: MOD-02 (a statement without rows answers an empty array)
    #[tokio::test]
    async fn returns_empty_array_when_statement_has_no_rows() {
        let store = store_with_probe_items().await;
        assert_eq!(
            probe_query(&store, "DELETE FROM m_probe_items")
                .await
                .unwrap(),
            "[]"
        );
    }

    // TC: MOD-02
    #[tokio::test]
    async fn forbidden_when_module_reads_users() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "SELECT login FROM users").await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (writes to a core table are refused before they run)
    #[tokio::test]
    async fn keeps_users_when_module_deletes_them() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "DELETE FROM users").await;
        assert_eq!(
            (
                matches!(refused, Err(Error::Forbidden(_))),
                store.users().await.unwrap().len()
            ),
            (true, 1),
            "{refused:?}"
        );
    }

    // TC: TEN-01 (`tenants` reads what a v1 import put aside for it)
    #[tokio::test]
    async fn returns_rows_when_tenants_reads_v1_import_tables() {
        let store = store_with_probe_items().await;
        let mut answers = Vec::new();
        for table in [
            "import_v1_tenants",
            "import_v1_tenant_members",
            "import_v1_tenant_projects",
            "import_v1_tenant_domains",
        ] {
            answers.push(
                store
                    .module_query(
                        &module("tenants"),
                        &format!("SELECT count(*) AS n FROM {table}"),
                        &no_params(),
                    )
                    .await
                    .map_err(|error| error.to_string()),
            );
        }
        assert!(answers.iter().all(Result::is_ok), "{answers:?}");
    }

    // TC: MOD-01 (the v1 import is a core table; only `tenants` reads it, no other module sees the v1 membership graph)
    #[tokio::test]
    async fn forbidden_when_other_module_reads_v1_import_tables() {
        let store = store_with_probe_items().await;
        let mut refused = Vec::new();
        for table in [
            "import_v1_tenants",
            "import_v1_tenant_members",
            "import_v1_tenant_projects",
            "import_v1_tenant_domains",
        ] {
            let answer = probe_query(&store, &format!("SELECT count(*) AS n FROM {table}")).await;
            refused.push(matches!(answer, Err(Error::Forbidden(_))));
        }
        assert_eq!(refused, [true, true, true, true]);
    }

    // TC: MOD-01 (another module's tables are as closed as the core's)
    #[tokio::test]
    async fn forbidden_when_module_reads_table_of_other_module() {
        let store = store_with_probe_items().await;
        store
            .module_query(
                &module("other"),
                "CREATE TABLE m_other_items (name TEXT)",
                &no_params(),
            )
            .await
            .unwrap();
        let refused = probe_query(&store, "SELECT name FROM m_other_items").await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (outside its namespace a module creates nothing)
    #[tokio::test]
    async fn forbidden_when_module_creates_table_outside_its_namespace() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "CREATE TABLE items (name TEXT)").await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (ATTACH is refused explicitly)
    #[tokio::test]
    async fn forbidden_when_module_attaches_database() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "ATTACH DATABASE ':memory:' AS side").await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (PRAGMA is refused explicitly, reading ones too)
    #[tokio::test]
    async fn forbidden_when_module_reads_pragma() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "PRAGMA table_info(users)").await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (PRAGMA is refused explicitly, writing ones too)
    #[tokio::test]
    async fn forbidden_when_module_sets_pragma() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "PRAGMA foreign_keys = OFF").await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (the schema is not the module's: no table names leak)
    #[tokio::test]
    async fn forbidden_when_module_reads_schema() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "SELECT name FROM sqlite_master").await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (one statement per call: nothing hides behind a `;`)
    #[tokio::test]
    async fn invalid_when_sql_has_two_statements() {
        let store = store_with_probe_items().await;
        let refused =
            probe_query(&store, "SELECT name FROM m_probe_items; DELETE FROM users").await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: MOD-02 (the daemon lives on: a refusal leaves the store usable)
    #[tokio::test]
    async fn answers_own_query_when_module_was_refused_before() {
        let store = store_with_probe_items().await;
        let _refused = probe_query(&store, "SELECT login FROM users").await;
        assert_eq!(
            probe_query(&store, "SELECT name FROM m_probe_items")
                .await
                .unwrap(),
            "[]"
        );
    }

    // TC: MOD-02 (the core keeps its own access after a module's query)
    #[tokio::test]
    async fn lets_core_read_users_when_module_was_refused_before() {
        let store = store_with_probe_items().await;
        let _refused = probe_query(&store, "SELECT login FROM users").await;
        assert_eq!(store.users().await.unwrap().len(), 1);
    }
    // TC: MOD-02 (the authorizer does not see the new name of a rename;
    // the schema check after the statement does)
    #[tokio::test]
    async fn forbidden_and_kept_when_module_renames_own_table_out_of_namespace() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "ALTER TABLE m_probe_items RENAME TO items").await;
        let kept = probe_query(&store, "SELECT count(*) AS n FROM m_probe_items").await;
        assert!(
            matches!(refused, Err(Error::Forbidden(_))) && kept.is_ok(),
            "{refused:?} {kept:?}"
        );
    }

    // TC: MOD-02 (a module evolves its own tables)
    #[tokio::test]
    async fn alters_and_indexes_own_table_when_module_migrates() {
        let store = store_with_probe_items().await;
        for sql in [
            "ALTER TABLE m_probe_items ADD COLUMN size INTEGER",
            "ALTER TABLE m_probe_items RENAME COLUMN size TO weight",
            "CREATE INDEX m_probe_itemsbyname ON m_probe_items (name)",
            "ALTER TABLE m_probe_items RENAME TO m_probe_things",
            "DROP INDEX m_probe_itemsbyname",
            "DROP TABLE m_probe_things",
        ] {
            probe_query(&store, sql)
                .await
                .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        }
    }

    // TC: MOD-02 (the schema stays closed inside a CREATE ... AS SELECT)
    #[tokio::test]
    async fn forbidden_when_module_copies_schema_into_own_table() {
        let store = store_with_probe_items().await;
        let refused = probe_query(
            &store,
            "CREATE TABLE m_probe_names AS SELECT name FROM sqlite_master",
        )
        .await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (the connection is the daemon's: no transactions, views,
    // triggers or temporary tables)
    #[tokio::test]
    async fn forbidden_when_module_reaches_for_the_connection() {
        let store = store_with_probe_items().await;
        let mut admitted = Vec::new();
        for sql in [
            "BEGIN",
            "SAVEPOINT side",
            "CREATE VIEW m_probe_users AS SELECT login FROM users",
            "CREATE VIEW m_probe_view AS SELECT name FROM m_probe_items",
            "CREATE TRIGGER m_probe_t AFTER INSERT ON m_probe_items BEGIN DELETE FROM users; END",
            "CREATE TEMP TABLE m_probe_tmp (x)",
            "SELECT * FROM pragma_table_info('users')",
            "DETACH DATABASE side",
        ] {
            let refused = probe_query(&store, sql).await;
            if !matches!(refused, Err(Error::Forbidden(_))) {
                admitted.push((sql, refused));
            }
        }
        assert!(admitted.is_empty(), "{admitted:#?}");
    }

    // TC: MOD-02 (VACUUM never reaches the authorizer: `VACUUM INTO`
    // would copy the whole database to a file)
    #[tokio::test]
    async fn forbidden_when_module_vacuums_into_file() {
        let store = store_with_probe_items().await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("copy.db");
        let mut admitted = Vec::new();
        for sql in [
            format!("VACUUM INTO '{}'", target.display()),
            "VACUUM".to_owned(),
            "  /* note */ -- line\n vacuum main".to_owned(),
        ] {
            let refused = probe_query(&store, &sql).await;
            if !matches!(refused, Err(Error::Forbidden(_))) {
                admitted.push((sql, refused));
            }
        }
        assert!(admitted.is_empty() && !target.exists(), "{admitted:#?}");
    }

    // TC: MOD-02 (a missing name outside the module's prefix is answered
    // like an existing one: the answer does not tell which core tables
    // exist)
    #[tokio::test]
    async fn forbidden_when_module_reads_missing_table_outside_its_namespace() {
        let store = store_with_probe_items().await;
        let mut admitted = Vec::new();
        for sql in [
            "SELECT * FROM nosuch",
            "SELECT nosuch FROM users",
            "DELETE FROM nosuch",
            "DROP TABLE nosuch",
            "DROP INDEX nosuch",
        ] {
            let refused = probe_query(&store, sql).await;
            if !matches!(&refused, Err(Error::Forbidden(_))) {
                admitted.push((sql, refused));
            }
        }
        assert!(admitted.is_empty(), "{admitted:#?}");
    }

    // TC: MOD-02 (a missing table of its own is the module's error)
    #[tokio::test]
    async fn invalid_when_module_reads_missing_own_table() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "SELECT * FROM m_probe_missing").await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: MOD-02 (a module's error is its own; the store answers on)
    #[tokio::test]
    async fn invalid_when_sql_does_not_parse() {
        let store = store_with_probe_items().await;
        let refused = probe_query(&store, "SELECT name FROM WHERE").await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }
}

mod module_tables {
    use super::*;

    // TC: MOD-01 (a name that begins another's prefix does not reach the
    // other's tables: `dns` and `dns.cloudflare`)
    #[tokio::test]
    async fn forbidden_when_module_reaches_table_of_module_its_name_begins() {
        let store = store_with_probe_items().await;
        store
            .module_query(
                &module("dns.cloudflare"),
                "CREATE TABLE m_dns_cloudflare_records (host TEXT)",
                &no_params(),
            )
            .await
            .unwrap();
        let mut admitted = Vec::new();
        for sql in [
            "SELECT host FROM m_dns_cloudflare_records",
            "DELETE FROM m_dns_cloudflare_records",
            "DROP TABLE m_dns_cloudflare_records",
            "CREATE TABLE m_dns_cloudflare_zones (id TEXT)",
        ] {
            let refused = store.module_query(&module("dns"), sql, &no_params()).await;
            if !matches!(refused, Err(Error::Forbidden(_))) {
                admitted.push((sql, refused));
            }
        }
        assert!(admitted.is_empty(), "{admitted:#?}");
    }

    // TC: MOD-01 (an object name is the prefix and one word: an index
    // named like a dotted module's table is not the module's)
    #[tokio::test]
    async fn forbidden_when_module_names_index_with_two_words() {
        let store = store_with_probe_items().await;
        let refused = probe_query(
            &store,
            "CREATE INDEX m_probe_items_name ON m_probe_items (name)",
        )
        .await;
        assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
    }

    // TC: MOD-02 (a foreign key is a dependency of the core's table on the
    // module: SQLite then refuses the core's deletes with a "foreign key
    // mismatch"; the authorizer does not see `REFERENCES`)
    #[tokio::test]
    async fn forbidden_when_module_references_table_outside_its_namespace() {
        let store = store_with_probe_items().await;
        store
            .module_query(
                &module("other"),
                "CREATE TABLE m_other_items (name TEXT PRIMARY KEY)",
                &no_params(),
            )
            .await
            .unwrap();
        let mut admitted = Vec::new();
        for sql in [
            "CREATE TABLE m_probe_refs (user TEXT REFERENCES sessions (user_id))",
            "CREATE TABLE m_probe_refs (user TEXT REFERENCES users (id))",
            "CREATE TABLE m_probe_refs (user TEXT REFERENCES Users)",
            "CREATE TABLE m_probe_refs (user TEXT, FOREIGN KEY (user) REFERENCES users (id))",
            "CREATE TABLE m_probe_refs (name TEXT REFERENCES m_other_items (name))",
            "CREATE TABLE m_probe_refs (name TEXT REFERENCES nosuch (name))",
            "ALTER TABLE m_probe_items ADD COLUMN owner TEXT REFERENCES users (login)",
        ] {
            let refused = probe_query(&store, sql).await;
            if !matches!(refused, Err(Error::Forbidden(_))) {
                admitted.push((sql, refused));
            }
        }
        assert!(admitted.is_empty(), "{admitted:#?}");
    }

    // TC: MOD-02 (the core's deletes keep working after a module tried to
    // reference its table)
    #[tokio::test]
    async fn lets_core_delete_sessions_when_module_referenced_them() {
        let store = store_with_probe_items().await;
        let owner = store.users().await.unwrap().remove(0);
        let _refused = probe_query(
            &store,
            "CREATE TABLE m_probe_refs (user TEXT REFERENCES sessions (user_id))",
        )
        .await;
        let deleted = store.delete_sessions_of_user(owner.id).await;
        assert!(deleted.is_ok(), "{deleted:?}");
    }

    // TC: MOD-02 (keys and foreign keys between its own tables are the
    // module's: their automatic indexes are its own too)
    #[tokio::test]
    async fn creates_linked_tables_when_module_references_its_own() {
        let store = store_with_probe_items().await;
        for sql in [
            "CREATE TABLE m_probe_kinds (name TEXT PRIMARY KEY, code TEXT UNIQUE)",
            "CREATE TABLE m_probe_tags (kind TEXT REFERENCES m_probe_kinds (name))",
            "INSERT INTO m_probe_kinds (name, code) VALUES ('fruit', 'f')",
            "INSERT INTO m_probe_tags (kind) VALUES ('fruit')",
            "DROP TABLE m_probe_tags",
        ] {
            probe_query(&store, sql)
                .await
                .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        }
    }
}

mod deferred_keys {
    use super::*;

    /// Module `probe` with a parent table and a child whose foreign key
    /// is checked at commit (`DEFERRABLE INITIALLY DEFERRED`).
    async fn with_deferred_key(store: &SqliteStore) {
        for sql in [
            "CREATE TABLE m_probe_parents (id INTEGER PRIMARY KEY)",
            "CREATE TABLE m_probe_children (parent INTEGER REFERENCES m_probe_parents (id) DEFERRABLE INITIALLY DEFERRED)",
        ] {
            probe_query(store, sql)
                .await
                .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        }
    }

    // TC: MOD-02 (a deferred key the statement breaks is refused and
    // undone)
    #[tokio::test]
    async fn invalid_and_undone_when_statement_breaks_deferred_key() {
        let store = store_with_probe_items().await;
        with_deferred_key(&store).await;
        let refused = probe_query(&store, "INSERT INTO m_probe_children VALUES (42)").await;
        let kept = probe_query(&store, "SELECT count(*) AS n FROM m_probe_children").await;
        assert_eq!(
            (matches!(refused, Err(Error::Validation(_))), kept.ok()),
            (true, Some(r#"[{"n":0}]"#.to_owned())),
            "{refused:?}"
        );
    }

    // TC: MOD-02 (the daemon's connection is never left inside a
    // transaction: the core's next write is committed and survives a
    // reopen of the file)
    #[tokio::test]
    async fn keeps_core_write_after_reopen_when_statement_broke_deferred_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("liftoff.db");
        {
            let store = SqliteStore::open(&path).unwrap();
            with_deferred_key(&store).await;
            let _refused = probe_query(&store, "INSERT INTO m_probe_children VALUES (42)").await;
            let owner = Records::default().user("owner", UserRole::Admin).unwrap();
            store.insert_user(&owner).await.unwrap();
        }
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.users().await.unwrap().len(), 1);
    }
}

mod module_query_limits {
    use super::*;

    const LIMITS: ModuleQueryLimits = ModuleQueryLimits {
        timeout: Duration::from_millis(200),
        max_result_bytes: 1024,
    };

    async fn limited_store() -> SqliteStore {
        let store = SqliteStore::open_in_memory()
            .unwrap()
            .with_module_query_limits(LIMITS);
        let owner = Records::default().user("owner", UserRole::Admin).unwrap();
        store.insert_user(&owner).await.unwrap();
        store
    }

    const RUNAWAY: &str = "WITH RECURSIVE m_probe_c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM m_probe_c) SELECT count(*) FROM m_probe_c";

    // TC: MOD-02 (a statement that runs past its time is cut off: it holds
    // the daemon's connection)
    #[tokio::test]
    async fn invalid_when_statement_runs_past_its_time() {
        let store = limited_store().await;
        let refused = probe_query(&store, RUNAWAY).await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: MOD-02 (the daemon lives on after a cut-off statement: the core
    // and the module are answered, without the module's time limit)
    #[tokio::test]
    async fn answers_core_and_module_when_statement_was_cut_off() {
        let store = limited_store().await;
        let _refused = probe_query(&store, RUNAWAY).await;
        let core = store.users().await.map(|users| users.len());
        let own = probe_query(&store, "SELECT 1 AS one").await;
        assert_eq!(
            (core.ok(), own.ok()),
            (Some(1), Some(r#"[{"one":1}]"#.to_owned()))
        );
    }

    // TC: MOD-02 (an answer has a size limit)
    #[tokio::test]
    async fn invalid_when_rows_exceed_result_limit() {
        let store = limited_store().await;
        let refused = probe_query(
            &store,
            "WITH RECURSIVE m_probe_c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM m_probe_c LIMIT 1000) SELECT x FROM m_probe_c",
        )
        .await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: MOD-02 (one value has the same size limit while it is computed)
    #[tokio::test]
    async fn invalid_when_value_exceeds_result_limit() {
        let store = limited_store().await;
        let refused = probe_query(&store, "SELECT length(zeroblob(1000000)) AS n").await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: MOD-02 (the limit holds for one wide row too: every value is
    // under it, their sum is not)
    #[tokio::test]
    async fn invalid_when_one_row_of_values_under_limit_exceeds_it() {
        let store = limited_store().await;
        let columns: Vec<String> = (0..16)
            .map(|index| format!("zeroblob(500) AS c{index}"))
            .collect();
        let refused = probe_query(&store, &format!("SELECT {}", columns.join(", "))).await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: MOD-02 (a statement has few columns: one row cannot make the
    // daemon hold thousands of values at once)
    #[tokio::test]
    async fn invalid_when_statement_has_too_many_columns() {
        let store = limited_store().await;
        let columns: Vec<String> = (0..=liftoff_store::MAX_MODULE_COLUMNS)
            .map(|index| format!("NULL AS c{index}"))
            .collect();
        let refused = probe_query(&store, &format!("SELECT {}", columns.join(", "))).await;
        assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: MOD-02 (the column limit is the module's only)
    #[tokio::test]
    async fn answers_widest_allowed_row_when_columns_are_at_limit() {
        let store = limited_store().await;
        let columns: Vec<String> = (0..liftoff_store::MAX_MODULE_COLUMNS)
            .map(|index| format!("NULL AS c{index}"))
            .collect();
        let answered = probe_query(&store, &format!("SELECT {}", columns.join(", "))).await;
        assert!(answered.is_ok(), "{answered:?}");
    }

    // TC: MOD-02 (the core's values are not bound by the module's limit)
    #[tokio::test]
    async fn keeps_core_values_unbounded_when_module_query_ran() {
        let store = SqliteStore::open_in_memory()
            .unwrap()
            .with_module_query_limits(LIMITS);
        let _ran = probe_query(&store, "SELECT 1 AS one").await;
        let mut big = Records::default().user("big", UserRole::User).unwrap();
        big.email = "x".repeat(4096);
        let inserted = store.insert_user(&big).await;
        assert!(inserted.is_ok(), "{inserted:?}");
    }
}

mod module_keys {
    use super::*;

    // TC: MOD-01 (the key space survives a reopen of the file)
    #[tokio::test]
    async fn keeps_value_when_file_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("liftoff.db");
        SqliteStore::open(&path)
            .unwrap()
            .module_set(&module("probe"), "greeting", b"hi")
            .await
            .unwrap();
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(
            reopened
                .module_get(&module("probe"), "greeting")
                .await
                .unwrap(),
            Some(b"hi".to_vec())
        );
    }
}

mod module_purge {
    use super::*;

    // TC: REG-07 (`remove --purge` drops the module's tables)
    #[tokio::test]
    async fn drops_module_tables_when_purged() {
        let store = store_with_probe_items().await;
        store.module_purge(&module("probe")).await.unwrap();
        let read = probe_query(&store, "SELECT name FROM m_probe_items").await;
        assert!(read.is_err(), "the table is gone: {read:?}");
    }

    // TC: REG-07 (a purged module can create its tables again: a fresh
    // install migrates from 0)
    #[tokio::test]
    async fn lets_module_create_its_tables_again_when_purged() {
        let store = store_with_probe_items().await;
        store.module_purge(&module("probe")).await.unwrap();
        let created = probe_query(&store, "CREATE TABLE m_probe_items (name TEXT NOT NULL)").await;
        assert_eq!(created.ok(), Some("[]".to_owned()));
    }

    // TC: REG-07 (another module's tables stay)
    #[tokio::test]
    async fn keeps_other_module_tables_when_one_is_purged() {
        let store = store_with_probe_items().await;
        store
            .module_query(
                &module("other"),
                "CREATE TABLE m_other_items (name TEXT NOT NULL)",
                &no_params(),
            )
            .await
            .unwrap();
        store.module_purge(&module("probe")).await.unwrap();
        let read = store
            .module_query(
                &module("other"),
                "SELECT name FROM m_other_items",
                &no_params(),
            )
            .await;
        assert_eq!(read.ok(), Some("[]".to_owned()));
    }

    // TC: REG-07 (the core's tables stay)
    #[tokio::test]
    async fn keeps_core_tables_when_module_is_purged() {
        let store = store_with_probe_items().await;
        store.module_purge(&module("probe")).await.unwrap();
        assert_eq!(store.users().await.map(|users| users.len()).ok(), Some(1));
    }
}
