//! [`ModuleStore`] over SQLite: the key space of
//! every module (table `module_keys`) and `store-query` behind the
//! authorizer, both `liftoff-module-sql`'s, on the daemon's one
//! connection.
//!
//! The rules live in `liftoff-module-sql`, so the
//! CLI's local module store holds modules to the same ones. What only the
//! daemon's database has is named here: the tables a v1 import put aside
//! for one module ([`V1_IMPORT_TABLES`]), which that module alone may read.

use liftoff_core::Error;
use liftoff_core::domain::ModuleName;
use liftoff_core::ports::ModuleStore;
use liftoff_module_sql::{QueryRules, SharedTable};

use crate::SqliteStore;

/// The tables a v1 import (`liftoffd setup --import`) fills for a module,
/// with the one module that may read each: `import_v1_tenants` and its
/// `import_v1_tenant_*` rows are the `tenants` module's (modules do not touch the core's tables; these hold the v1 membership graph, which no other module may see).
const V1_IMPORT_TABLES: &[SharedTable] = &[
    SharedTable {
        table: "import_v1_tenants",
        reader: "tenants",
    },
    SharedTable {
        table: "import_v1_tenant_members",
        reader: "tenants",
    },
    SharedTable {
        table: "import_v1_tenant_projects",
        reader: "tenants",
    },
    SharedTable {
        table: "import_v1_tenant_domains",
        reader: "tenants",
    },
];

impl ModuleStore for SqliteStore {
    async fn module_get(&self, module: &ModuleName, key: &str) -> Result<Option<Vec<u8>>, Error> {
        let module = module.clone();
        let key = key.to_owned();
        self.run(move |conn| liftoff_module_sql::get(conn, &module, &key))
            .await
    }

    async fn module_set(&self, module: &ModuleName, key: &str, value: &[u8]) -> Result<(), Error> {
        let module = module.clone();
        let key = key.to_owned();
        let value = value.to_vec();
        self.run(move |conn| liftoff_module_sql::set(conn, &module, &key, &value))
            .await
    }

    async fn module_query(
        &self,
        module: &ModuleName,
        sql: &str,
        params: &[String],
    ) -> Result<String, Error> {
        let module = module.clone();
        let sql = sql.to_owned();
        let params = params.to_vec();
        let rules = QueryRules {
            limits: self.module_limits,
            shared: V1_IMPORT_TABLES,
        };
        self.run(move |conn| liftoff_module_sql::query(conn, &module, &sql, &params, rules))
            .await
    }

    /// Drops every table of `module` and its keys in one transaction;
    /// their indexes go with them.
    async fn module_purge(&self, module: &ModuleName) -> Result<(), Error> {
        let module = module.clone();
        self.run(move |conn| liftoff_module_sql::purge(conn, &module))
            .await
    }
}
