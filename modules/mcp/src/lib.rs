//! `mcp`: the module that turns on MCP over Streamable HTTP (8.3).
//!
//! The transport itself is the daemon's (`POST|GET|DELETE /mcp` and `GET
//! /.well-known/oauth-protected-resource` at the root of the API host,
//! `424` until this module is installed): the module requires `oidc`,
//! whose `verify_token` checks the access tokens, and keeps the OAuth
//! protected resource in its configuration:
//!
//! | Field | Meaning |
//! |---|---|
//! | `resource` | the URL of the resource, `https://<api>/mcp` |
//! | `allowed_audiences` | the `aud` values an access token may carry (at least one) |
//! | `client_id` | optional: the client pre-registered at the provider, for clients without dynamic registration |
//!
//! Its one hook, `mcp_resource`, answers data (a `handle-route` call on
//! `POST /.hook/mcp_resource`, like `tenant_quotas`): `200`
//! `{"resource","audiences"}` from the configuration, `404` while
//! `resource` is not set. No routes, no tables, no outgoing HTTP.

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

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error};
use serde_json::{Value, json};

/// The component's export.
struct Mcp;

/// The JSON Schema of the configuration.
fn schema() -> Value {
    json!({
        "type": "object",
        "required": ["resource", "allowed_audiences"],
        "additionalProperties": false,
        "properties": {
            "resource": {
                "type": "string",
                "pattern": "^https?://",
                "description": "URL of the protected resource, https://<api host>/mcp",
            },
            "allowed_audiences": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 1,
                "description": "Audiences (aud) an access token of /mcp may carry",
            },
            "client_id": {
                "type": "string",
                "description": "OAuth client pre-registered at the provider, for MCP clients without dynamic client registration",
            },
        },
    })
}

/// A problem document (RFC 9457) with `detail`.
fn problem(status: u16, title: &str, detail: &str) -> Response {
    Response {
        status,
        headers: vec![(
            "content-type".to_owned(),
            "application/problem+json".to_owned(),
        )],
        body: json!({"type": "about:blank", "title": title, "status": status, "detail": detail})
            .to_string()
            .into_bytes(),
    }
}

/// `allowed_audiences` as the host hands it over: the JSON text of the
/// list (a list value is passed as JSON), or one audience, or several
/// separated by commas.
fn audiences(raw: &str) -> Vec<String> {
    let listed: Vec<String> = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => raw.split(',').map(|one| one.trim().to_owned()).collect(),
    };
    listed
        .into_iter()
        .filter(|audience| !audience.is_empty())
        .collect()
}

/// `mcp_resource()`: `200 {"resource","audiences"}` of the
/// configuration, `404` while either is not set.
fn resource() -> Response {
    let resource = host::config_get("resource").filter(|value| !value.is_empty());
    let audiences = host::config_get("allowed_audiences")
        .map(|raw| audiences(&raw))
        .unwrap_or_default();
    match resource {
        Some(resource) if !audiences.is_empty() => Response {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: json!({"resource": resource, "audiences": audiences})
                .to_string()
                .into_bytes(),
        },
        _ => problem(
            404,
            "Not Found",
            "the mcp module has no resource or allowed_audiences configured",
        ),
    }
}

impl Guest for Mcp {
    fn manifest() -> String {
        json!({
            "name": "mcp",
            "version": env!("CARGO_PKG_VERSION"),
            "requires": ["oidc"],
            "config_schema": schema(),
            "hooks": ["mcp_resource"],
        })
        .to_string()
    }

    fn migrate(_from: u32) -> Result<(), Error> {
        // No tables.
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/.hook/mcp_resource") => resource(),
            _ => problem(404, "Not Found", "no such route"),
        }
    }

    fn handle_tool(name: String, _args: String) -> Result<String, Error> {
        Err(Error {
            code: "not_found".to_owned(),
            message: format!("tool {name}"),
        })
    }

    fn on_hook(_hook: String, _payload: String) -> Result<HookResult, Error> {
        Ok(HookResult::Continue)
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`Mcp`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Mcp;

    crate::bindings::export!(Mcp with_types_in crate::bindings);
}
