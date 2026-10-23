//! `denier`: a contract component of the hook dispatcher (HOOK-01…06): a `tenants`-like module whose answers its
//! configuration sets.
//!
//! The hooks reach it as the host's `ModuleSet` sends them (see
//! `liftoff_wasm::ModuleSet`):
//!
//! | Call | Answer |
//! |---|---|
//! | `on-hook("authorize", {"actor","action","resource"})` | `deny("project belongs to another tenant")` when `resource` is `{"type":"project","id":<config foreign_project>}`, else `continue` |
//! | `on-hook("before_deploy", …)` | `deny(<config deny_deploys>)` when it is set, else `continue` |
//! | any other `on-hook` | `continue` |
//! | `handle-route(POST /.hook/authenticate)` | `403` with `<config deny_auth>` when it is set, else `200` with `<config actor>` (a user ID) when it is set, else `204` |
//! | `handle-route(GET /.hook/render_page/<kind>)` | `200` with `<config <kind>_page>` when it is set, else `404` |
//! | `handle-route(GET /whoami)` (behind a session), `GET /hello` (public) | `200` with the request's actor, or `anonymous` |
//! | any other route | `404` |

#[expect(
    clippy::error_impl_error,
    clippy::mem_forget,
    clippy::same_length_and_capacity,
    reason = "code wit-bindgen generates"
)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "liftoff:module/liftoff-module",
        generate_all,
    });
}

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error};
use serde::Deserialize;

/// The component's export.
struct Denier;

/// Subscribed to the hooks it answers; `/whoami` needs a session,
/// `/hello` does not.
const MANIFEST: &str = r#"{
  "name": "denier",
  "version": "0.1.0",
  "routes": [{"method": "GET", "path": "/whoami"},
             {"method": "GET", "path": "/hello", "public": true}],
  "hooks": ["authenticate", "authorize", "before_deploy", "render_page"]
}"#;

/// The reason `authorize` gives for the foreign project.
const FOREIGN: &str = "project belongs to another tenant";

/// The part of the `authorize` payload the module reads.
#[derive(Deserialize)]
struct Authorize {
    resource: Resource,
}

/// A resource of the `authorize` payload.
#[derive(Deserialize)]
struct Resource {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
}

fn respond(status: u16, body: impl Into<Vec<u8>>) -> Response {
    Response {
        status,
        headers: Vec::new(),
        body: body.into(),
    }
}

impl Guest for Denier {
    fn manifest() -> String {
        MANIFEST.to_owned()
    }

    fn migrate(_from: u32) -> Result<(), Error> {
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        if req.method == "POST" && req.path == "/.hook/authenticate" {
            if let Some(reason) = host::config_get("deny_auth") {
                return respond(403, reason);
            }
            return host::config_get("actor")
                .map_or_else(|| respond(204, ""), |actor| respond(200, actor));
        }
        if let Some(kind) = req.path.strip_prefix("/.hook/render_page/") {
            return host::config_get(&format!("{kind}_page"))
                .map_or_else(|| respond(404, ""), |html| respond(200, html));
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/whoami" | "/hello") => {
                respond(200, req.actor.unwrap_or_else(|| "anonymous".to_owned()))
            }
            _ => respond(404, ""),
        }
    }

    fn handle_tool(name: String, _args: String) -> Result<String, Error> {
        Err(Error {
            code: "not_found".to_owned(),
            message: format!("tool {name}"),
        })
    }

    fn on_hook(hook: String, payload: String) -> Result<HookResult, Error> {
        match hook.as_str() {
            "authorize" => {
                let Authorize { resource } =
                    serde_json::from_str(&payload).map_err(|why| Error {
                        code: "validation".to_owned(),
                        message: why.to_string(),
                    })?;
                let foreign = host::config_get("foreign_project");
                if resource.kind == "project" && resource.id.is_some() && resource.id == foreign {
                    return Ok(HookResult::Deny(FOREIGN.to_owned()));
                }
                Ok(HookResult::Continue)
            }
            "before_deploy" => {
                Ok(host::config_get("deny_deploys").map_or(HookResult::Continue, HookResult::Deny))
            }
            _ => Ok(HookResult::Continue),
        }
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`Denier`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Denier;

    crate::bindings::export!(Denier with_types_in crate::bindings);
}
