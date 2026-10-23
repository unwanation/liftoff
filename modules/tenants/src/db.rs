//! The module's tables: `m_tenants_tenants` (`slug`, `name`,
//! `max_projects`, `env_cpus` in hundredths of a CPU, `env_memory_mb`,
//! `created_by`) and `m_tenants_memberships` (`tenant_id`, `user_id`,
//! `role` `owner` or `member`), through `store-query`.

use serde::Deserialize;
use serde_json::Value;

use crate::bindings::liftoff::module::host::{self, Error};

/// A tenant as its row holds it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Tenant {
    pub(crate) id: String,
    pub(crate) slug: String,
    pub(crate) name: String,
    pub(crate) max_projects: Option<u32>,
    pub(crate) env_cpus: Option<u32>,
    pub(crate) env_memory_mb: Option<u32>,
}

/// A membership.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Membership {
    pub(crate) user_id: String,
    pub(crate) role: String,
}

/// `sql` with `params`: its rows.
pub(crate) fn query(sql: &str, params: &[&str]) -> Result<Vec<Value>, Error> {
    let params: Vec<String> = params.iter().map(|param| (*param).to_owned()).collect();
    let rows = host::store_query(sql, &params)?;
    serde_json::from_str(&rows).map_err(|why| Error {
        code: "internal".to_owned(),
        message: format!("store-query answered no rows: {why}"),
    })
}

/// `sql` with `params`, its rows as `T`.
fn rows<T: for<'de> Deserialize<'de>>(sql: &str, params: &[&str]) -> Result<Vec<T>, Error> {
    query(sql, params)?
        .into_iter()
        .map(|row| {
            serde_json::from_value(row).map_err(|why| Error {
                code: "internal".to_owned(),
                message: format!("unexpected row: {why}"),
            })
        })
        .collect()
}

/// `sql` with `params`, for its effect.
pub(crate) fn execute(sql: &str, params: &[&str]) -> Result<(), Error> {
    query(sql, params).map(drop)
}

/// Creates the tables (migration 1).
pub(crate) fn create_tables() -> Result<(), Error> {
    execute(
        "CREATE TABLE IF NOT EXISTS m_tenants_tenants (\
         id TEXT PRIMARY KEY NOT NULL, \
         slug TEXT NOT NULL UNIQUE, \
         name TEXT NOT NULL, \
         max_projects INTEGER, \
         env_cpus INTEGER, \
         env_memory_mb INTEGER, \
         created_by TEXT)",
        &[],
    )?;
    execute(
        "CREATE TABLE IF NOT EXISTS m_tenants_memberships (\
         tenant_id TEXT NOT NULL REFERENCES m_tenants_tenants (id) ON DELETE CASCADE, \
         user_id TEXT NOT NULL, \
         role TEXT NOT NULL CHECK (role IN ('owner', 'member')), \
         PRIMARY KEY (tenant_id, user_id))",
        &[],
    )
}

const TENANT_COLUMNS: &str = "id, slug, name, max_projects, env_cpus, env_memory_mb";

/// The tenant of slug `slug`.
pub(crate) fn tenant_by_slug(slug: &str) -> Result<Option<Tenant>, Error> {
    Ok(rows(
        &format!("SELECT {TENANT_COLUMNS} FROM m_tenants_tenants WHERE slug = ?1"),
        &[slug],
    )?
    .into_iter()
    .next())
}

/// The tenant of id `id`.
pub(crate) fn tenant_by_id(id: &str) -> Result<Option<Tenant>, Error> {
    Ok(rows(
        &format!("SELECT {TENANT_COLUMNS} FROM m_tenants_tenants WHERE id = ?1"),
        &[id],
    )?
    .into_iter()
    .next())
}

/// Every tenant, by slug.
pub(crate) fn tenants() -> Result<Vec<Tenant>, Error> {
    rows(
        &format!("SELECT {TENANT_COLUMNS} FROM m_tenants_tenants ORDER BY slug"),
        &[],
    )
}

/// The tenants `user` belongs to and their role there, by slug.
pub(crate) fn tenants_of(user: &str) -> Result<Vec<(Tenant, String)>, Error> {
    #[derive(Deserialize)]
    struct Row {
        id: String,
        slug: String,
        name: String,
        max_projects: Option<u32>,
        env_cpus: Option<u32>,
        env_memory_mb: Option<u32>,
        role: String,
    }
    let found: Vec<Row> = rows(
        "SELECT t.id, t.slug, t.name, t.max_projects, t.env_cpus, t.env_memory_mb, m.role \
         FROM m_tenants_tenants t JOIN m_tenants_memberships m ON m.tenant_id = t.id \
         WHERE m.user_id = ?1 ORDER BY t.slug",
        &[user],
    )?;
    Ok(found
        .into_iter()
        .map(|row| {
            (
                Tenant {
                    id: row.id,
                    slug: row.slug,
                    name: row.name,
                    max_projects: row.max_projects,
                    env_cpus: row.env_cpus,
                    env_memory_mb: row.env_memory_mb,
                },
                row.role,
            )
        })
        .collect())
}

/// Inserts a tenant.
pub(crate) fn insert_tenant(
    id: &str,
    slug: &str,
    name: &str,
    created_by: &str,
) -> Result<(), Error> {
    execute(
        "INSERT INTO m_tenants_tenants (id, slug, name, created_by) VALUES (?1, ?2, ?3, NULLIF(?4, ''))",
        &[id, slug, name, created_by],
    )
}

/// Deletes a tenant and its memberships.
pub(crate) fn delete_tenant(id: &str) -> Result<(), Error> {
    execute(
        "DELETE FROM m_tenants_memberships WHERE tenant_id = ?1",
        &[id],
    )?;
    execute("DELETE FROM m_tenants_tenants WHERE id = ?1", &[id])
}

/// Sets one quota column of a tenant (`None` clears it).
pub(crate) fn set_quota(id: &str, column: Quota, value: Option<u32>) -> Result<(), Error> {
    let column = match column {
        Quota::MaxProjects => "max_projects",
        Quota::EnvCpus => "env_cpus",
        Quota::EnvMemoryMb => "env_memory_mb",
    };
    let value = value.map(|value| value.to_string()).unwrap_or_default();
    execute(
        &format!("UPDATE m_tenants_tenants SET {column} = NULLIF(?1, '') WHERE id = ?2"),
        &[&value, id],
    )
}

/// A quota column.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Quota {
    MaxProjects,
    EnvCpus,
    EnvMemoryMb,
}

/// The role of `user` in `tenant`.
pub(crate) fn role(tenant: &str, user: &str) -> Result<Option<String>, Error> {
    Ok(rows::<Membership>(
        "SELECT user_id, role FROM m_tenants_memberships WHERE tenant_id = ?1 AND user_id = ?2",
        &[tenant, user],
    )?
    .into_iter()
    .next()
    .map(|membership| membership.role))
}

/// The memberships of `tenant`.
pub(crate) fn members(tenant: &str) -> Result<Vec<Membership>, Error> {
    rows(
        "SELECT user_id, role FROM m_tenants_memberships WHERE tenant_id = ?1",
        &[tenant],
    )
}

/// Makes `user` a member of `tenant` with `role`, or changes their role.
pub(crate) fn put_member(tenant: &str, user: &str, role: &str) -> Result<(), Error> {
    execute(
        "INSERT INTO m_tenants_memberships (tenant_id, user_id, role) VALUES (?1, ?2, ?3) \
         ON CONFLICT (tenant_id, user_id) DO UPDATE SET role = excluded.role",
        &[tenant, user, role],
    )
}

/// Ends the membership of `user` in `tenant`.
pub(crate) fn delete_member(tenant: &str, user: &str) -> Result<(), Error> {
    execute(
        "DELETE FROM m_tenants_memberships WHERE tenant_id = ?1 AND user_id = ?2",
        &[tenant, user],
    )
}
