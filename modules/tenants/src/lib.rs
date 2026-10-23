//! `tenants`: the module of tenants with owners and members, their quotas
//! and the `protect` gate on environments.
//!
//! Its tables are in `db.rs`; migration 1 (`migrate.rs`) adopts the tenants
//! of a v1 import and puts everything else in the tenant `default`
//! (TEN-01). It requires `oidc` (people log in through it, the gate
//! sends browsers to its login).
//!
//! Its routes below `/api/v1/m/tenants`, one per command, all behind a
//! session, JSON in and out (errors are `application/problem+json` with a
//! `detail`):
//!
//! | Command | Route | Body | Who |
//! |---|---|---|---|
//! | `lof tenant create` | `POST /tenant/create` | `{"slug","name"}` | anyone; becomes its owner; the tenant gets the v1 quotas (3 projects, 1 CPU, 1024 MiB) until the admin changes them |
//! | `lof tenant ls` | `POST /tenant/ls` | `{}` | their tenants; every one for the admin |
//! | `lof tenant rm` | `POST /tenant/rm` | `{"tenant"}` | owner, admin; `409` while it holds projects (TEN-10) |
//! | `lof tenant members add` | `POST /tenant/members/add` | `{"tenant","login","owner"}` | owner, admin (TEN-03) |
//! | `lof tenant members rm` | `POST /tenant/members/rm` | `{"tenant","login"}` | owner, admin (TEN-03) |
//! | `lof tenant members ls` | `POST /tenant/members/ls` | `{"tenant"}` | members, admin |
//! | `lof protect`, `lof unprotect` | `POST /protect`, `POST /unprotect` | `{"project","env"}` | members of the project's tenant, admin (TEN-06, TEN-09) |
//! | `lof admin tenant set` | `POST /admin/tenant/set` | `{"tenant","max_projects","env_cpus","env_memory_mb"}` | admin (TEN-04, TEN-05) |
//!
//! `tenant` is a slug. Its hooks:
//!
//! - `authorize`: the admin may do everything; anyone lists projects,
//!   domains and modules and manages their own forge connections (the
//!   core then asks about every listed project and domain, TEN-02); an
//!   action on a project, environment, domain or secret scope is allowed
//!   to the members of its tenant and denied to everyone else; a new
//!   project in a tenant (resource `{"type":"tenant"}`) to its members;
//!   anything else is left to the core (the admin only);
//! - `before_deploy`: denies an environment whose limits exceed its
//!   tenant's `env_cpus` or `env_memory_mb` (TEN-05);
//! - `gate`: lets the admin and the members of the host's tenant through
//!   (TEN-07, TEN-08);
//! - `tenant_quotas`: the tenant's `max_projects`, `env_cpus` and
//!   `env_memory_mb`, which the core applies (TEN-04, TEN-05);
//! - `project_tenant`: the tenant of a new project, the one the actor
//!   names by slug or else their only one (every project is in a tenant).

#[expect(
    clippy::error_impl_error,
    clippy::mem_forget,
    clippy::same_length_and_capacity,
    reason = "code wit-bindgen generates"
)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "liftoff:module/liftoff-module",
        generate_all,
    });
}

mod core;
mod db;
mod hooks;
mod migrate;
mod routes;
mod ulid;

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::Error;
use serde_json::{Value, json};

/// The component's export.
struct Tenants;

/// The commands and their routes: `lof <words>` is `POST /<words joined
/// by />`.
fn commands() -> Value {
    let arg = |name: &str, kind: &str, required: bool, hint: &str| json!({"name": name, "type": kind, "required": required, "hint": hint});
    let tenant = arg("tenant", "string", true, "Tenant slug");
    // `project` is the CLI's to fill from the working tree (`lof protect --env prod` in a project's directory).
    let environment = [
        arg("project", "project", true, "Project"),
        arg("env", "string", true, "Environment"),
    ];
    json!([
        {"name": "tenant create", "description": "Creates a tenant you own",
         "args": [arg("slug", "string", true, "Tenant slug"), arg("name", "string", false, "Display name")]},
        {"name": "tenant ls", "description": "Lists your tenants"},
        {"name": "tenant rm", "description": "Removes a tenant without projects", "args": [tenant]},
        {"name": "tenant members add", "description": "Adds a member to a tenant",
         "args": [tenant, arg("login", "string", true, "Login"), arg("owner", "boolean", false, "As an owner")]},
        {"name": "tenant members rm", "description": "Removes a member from a tenant",
         "args": [tenant, arg("login", "string", true, "Login")]},
        {"name": "tenant members ls", "description": "Lists the members of a tenant", "args": [tenant]},
        {"name": "protect", "description": "Puts an environment behind the gate: only members of its tenant open it",
         "args": environment},
        {"name": "unprotect", "description": "Takes an environment from behind the gate", "args": environment},
        {"name": "admin tenant set", "description": "Sets the quotas of a tenant (admins only)",
         "args": [tenant,
                  arg("max_projects", "integer", false, "Most projects"),
                  arg("env_cpus", "string", false, "CPUs of each new environment, e.g. 0.5"),
                  arg("env_memory_mb", "integer", false, "MiB of each new environment")]},
    ])
}

impl Guest for Tenants {
    fn manifest() -> String {
        let routes: Vec<Value> = commands()
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|command| command.get("name").and_then(Value::as_str))
            .map(|name| json!({"method": "POST", "path": routes::path_of(name)}))
            .collect();
        json!({
            "name": "tenants",
            "version": env!("CARGO_PKG_VERSION"),
            "requires": ["oidc"],
            "commands": commands(),
            "routes": routes,
            "migrations": 1,
            "hooks": ["authorize", "before_deploy", "gate", "tenant_quotas", "project_tenant"],
        })
        .to_string()
    }

    fn migrate(from: u32) -> Result<(), Error> {
        if from == 0 {
            migrate::first()?;
        }
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        if req.method == "POST" && req.path == "/.hook/tenant_quotas" {
            return hooks::tenant_quotas(&req.body);
        }
        if req.method == "POST" && req.path == "/.hook/project_tenant" {
            return hooks::project_tenant(&req.body);
        }
        routes::handle(&req)
    }

    fn handle_tool(name: String, _args: String) -> Result<String, Error> {
        Err(Error {
            code: "not_found".to_owned(),
            message: format!("tool {name}"),
        })
    }

    fn on_hook(hook: String, payload: String) -> Result<HookResult, Error> {
        let payload: Value = serde_json::from_str(&payload).map_err(|why| Error {
            code: "validation".to_owned(),
            message: format!("hook payload: {why}"),
        })?;
        match hook.as_str() {
            "authorize" => hooks::authorize(&payload),
            "before_deploy" => hooks::before_deploy(&payload),
            "gate" => hooks::gate(&payload),
            _ => Ok(HookResult::Continue),
        }
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`Tenants`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Tenants;

    crate::bindings::export!(Tenants with_types_in crate::bindings);
}
