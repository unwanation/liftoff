//! Migration 1 (TEN-01): the tables (kept as they are when a
//! `remove` without `--purge` left them); the tenants a v1 import
//! put aside (`import_v1_tenants`, with their members, projects and
//! domains, keeping their ids); and the tenant `default`, owned by the
//! single user (or, with several, by the admins), which gets every
//! project and domain outside a tenant.
//!
//! It runs without an actor, so it reaches the core only through the
//! `tenancy.*` calls, and the v1 tables through the store's read-only
//! access the store gives `tenants` alone to `import_v1_tenant*`.

use serde::Deserialize;

use crate::bindings::liftoff::module::host::Error;
use crate::{core, db, ulid};

/// The slug of the tenant of everything that was there before.
pub(crate) const DEFAULT: &str = "default";

/// Runs migration 1. Over the tables a `remove` without `--purge` kept
/// (a reinstall), the tenants they hold stay as they are: the v1 import
/// is not replayed and `default` is not made again.
pub(crate) fn first() -> Result<(), Error> {
    db::create_tables()?;
    if !db::tenants()?.is_empty() {
        return Ok(());
    }
    let imported = import_v1()?;
    default_tenant(imported)
}

/// Adopts the v1 tenants; whether there were any.
fn import_v1() -> Result<bool, Error> {
    #[derive(Deserialize)]
    struct Owned {
        tenant_id: String,
        id: String,
    }
    db::execute(
        "INSERT INTO m_tenants_tenants (id, slug, name, max_projects, env_cpus, env_memory_mb, created_by) \
         SELECT id, slug, name, max_targets, cpus_hundredths, memory_mb, created_by FROM import_v1_tenants",
        &[],
    )?;
    db::execute(
        "INSERT INTO m_tenants_memberships (tenant_id, user_id, role) \
         SELECT tenant_id, user_id, role FROM import_v1_tenant_members",
        &[],
    )?;
    let owned = |sql: &str| -> Result<Vec<Owned>, Error> {
        db::query(sql, &[])?
            .into_iter()
            .map(|row| {
                serde_json::from_value(row).map_err(|why| Error {
                    code: "internal".to_owned(),
                    message: format!("unexpected row of the v1 import: {why}"),
                })
            })
            .collect()
    };
    let projects = owned("SELECT tenant_id, project_id AS id FROM import_v1_tenant_projects")?;
    let domains = owned("SELECT tenant_id, domain_id AS id FROM import_v1_tenant_domains")?;
    let tenants = db::tenants()?;
    for tenant in &tenants {
        let of = |rows: &[Owned]| -> Vec<String> {
            rows.iter()
                .filter(|row| row.tenant_id == tenant.id)
                .map(|row| row.id.clone())
                .collect()
        };
        core::assign(&tenant.id, &of(&projects), &of(&domains))?;
    }
    Ok(!tenants.is_empty())
}

/// The tenant `default` with what no tenant holds. After a v1 import it
/// is kept only when it got something.
fn default_tenant(imported: bool) -> Result<(), Error> {
    if db::tenant_by_slug(DEFAULT)?.is_some() {
        return Ok(());
    }
    let id = ulid::new().ok_or_else(|| Error {
        code: "internal".to_owned(),
        message: "no randomness for a tenant id".to_owned(),
    })?;
    let users = core::users()?;
    let owners: Vec<&core::User> = if users.len() == 1 {
        users.iter().collect()
    } else {
        users.iter().filter(|user| user.is_admin()).collect()
    };
    let creator = owners.first().map_or("", |owner| owner.id.as_str());
    db::insert_tenant(&id, DEFAULT, "Default", creator)?;
    for owner in &owners {
        db::put_member(&id, &owner.id, "owner")?;
    }
    let (projects, domains) = core::adopt(&id)?;
    if imported && projects == 0 && domains == 0 {
        db::delete_tenant(&id)?;
    }
    Ok(())
}
