//! `git.forgejo`: the module that serves the core's `Git` port for
//! Forgejo hosts, carried over
//! from the earlier adapter `ForgejoGit`: always the user's own
//! connection (`Authorization: token <credential>`, no `Sudo`).
//!
//! Its configuration: `hosts`, the Forgejo hosts it serves (a JSON array
//! of host names, e.g. `["git.example.com"]`). Its manifest declares them as
//! `git_provider.hosts` (`oauth: false`: tokens only) and as `http_allow`;
//! the API of a host is `https://<host>/api/v1`. The manifest is read when
//! the module loads, so a change of `hosts` takes effect when it is
//! loaded again (`lof module config` reloads it).
//!
//! The core's `ModuleGit` calls `handle-tool` without an actor, the
//! connection in the arguments (its credential in plaintext for that call
//! only; the module stores nothing):
//!
//! | Tool | Arguments | Forgejo | Answer |
//! |---|---|---|---|
//! | `validate_token` | `{host, token}` | `GET /user` | `{login}` |
//! | `repo_role` | `{host, login, token, repo}` | `GET /repos/{repo}` | `{role}`: `admin`, `write`, `read`, `none` (`403`/`404`) |
//! | `image_exists` | `{…, owner, name, tag}` | `GET /packages/{owner}/container/{name}/{tag}` | `{exists}` |
//! | `put_ci_secret` | `{…, repo, key, value}` | `PUT /repos/{repo}/actions/secrets/{key}` `{"data"}` | `{}` |
//! | `list_templates` | `{…, repos}` | `GET /repos/{repo}/contents/liftoff.toml` each | `{templates: [{repo, manifest}]}` |
//! | `generate_from_template` | `{…, template, owner, name, private}` | `POST /repos/{template}/generate` | `{repo}` |
//!
//! A `5xx` is sent once more, then the error is `upstream` (GITF-04); a
//! `401` of a stored connection is `forbidden` with a `lof git connect`
//! hint (GITF-05); a host it is not configured for is `validation`. The
//! tools are not in the manifest's `tools`: they take a plaintext
//! credential, so they are no MCP tools and only the core calls them.

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

mod forgejo;
mod http;

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error};
use serde_json::{Map, Value, json};

use crate::forgejo::{Forgejo, error};

/// The component's export.
struct GitForgejo;

/// The configured `hosts`, lower-cased: a JSON array of host names, or
/// (set by hand) names separated by spaces or commas.
fn hosts() -> Vec<String> {
    let Some(raw) = host::config_get("hosts") else {
        return Vec::new();
    };
    let names: Vec<String> = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => raw.split([' ', ',', '\n']).map(str::to_owned).collect(),
    };
    names
        .into_iter()
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

/// The arguments of a tool call.
struct Args(Map<String, Value>);

impl Args {
    fn parse(raw: &str) -> Result<Self, Error> {
        match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(fields)) => Ok(Self(fields)),
            _ => Err(error("validation", "the arguments must be a JSON object")),
        }
    }

    /// The string argument `name`.
    fn text(&self, name: &str) -> Result<&str, Error> {
        self.0
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| error("validation", format!("argument {name} is missing")))
    }

    /// The boolean argument `name`.
    fn flag(&self, name: &str) -> Result<bool, Error> {
        self.0
            .get(name)
            .and_then(Value::as_bool)
            .ok_or_else(|| error("validation", format!("argument {name} is missing")))
    }

    /// The array-of-strings argument `name`.
    fn texts(&self, name: &str) -> Result<Vec<&str>, Error> {
        self.0
            .get(name)
            .and_then(Value::as_array)
            .and_then(|items| items.iter().map(Value::as_str).collect())
            .ok_or_else(|| error("validation", format!("argument {name} is missing")))
    }

    /// The connection the call acts as, on a host the module serves.
    fn forge(&self) -> Result<Forgejo<'_>, Error> {
        let host = self.text("host")?;
        if !hosts()
            .iter()
            .any(|served| served.eq_ignore_ascii_case(host))
        {
            return Err(error(
                "validation",
                format!("git.forgejo does not serve {host}: add it to the module's hosts"),
            ));
        }
        Ok(Forgejo {
            host,
            token: self.text("token")?,
        })
    }
}

/// The tools of the module (see the table above).
const TOOLS: [&str; 6] = [
    "validate_token",
    "repo_role",
    "image_exists",
    "put_ci_secret",
    "list_templates",
    "generate_from_template",
];

/// Runs the tool `name` on `args`: its JSON answer.
fn tool(name: &str, args: &Args) -> Result<Value, Error> {
    if !TOOLS.contains(&name) {
        return Err(error("not_found", format!("tool {name}")));
    }
    let forge = args.forge()?;
    match name {
        "validate_token" => Ok(json!({ "login": forge.login()? })),
        "repo_role" => Ok(json!({ "role": forge.repo_role(args.text("repo")?)? })),
        "image_exists" => Ok(json!({
            "exists": forge.image_exists(args.text("owner")?, args.text("name")?, args.text("tag")?)?,
        })),
        "put_ci_secret" => {
            forge.put_ci_secret(
                args.text("login")?,
                args.text("repo")?,
                args.text("key")?,
                args.text("value")?,
            )?;
            Ok(json!({}))
        }
        "list_templates" => {
            let templates = args
                .texts("repos")?
                .into_iter()
                .map(|repo| Ok(json!({ "repo": repo, "manifest": forge.manifest(repo)? })))
                .collect::<Result<Vec<Value>, Error>>()?;
            Ok(json!({ "templates": templates }))
        }
        "generate_from_template" => Ok(json!({
            "repo": forge.generate(
                args.text("template")?,
                args.text("owner")?,
                args.text("name")?,
                args.flag("private")?,
            )?,
        })),
        _ => Err(error("not_found", format!("tool {name}"))),
    }
}

impl Guest for GitForgejo {
    fn manifest() -> String {
        let hosts = hosts();
        json!({
            "name": "git.forgejo",
            "version": env!("CARGO_PKG_VERSION"),
            "config_schema": {
                "type": "object",
                "required": ["hosts"],
                "additionalProperties": false,
                "properties": {
                    "hosts": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Forgejo hosts the module serves, e.g. [\"git.example.org\"]",
                    },
                },
            },
            "http_allow": hosts,
            "git_provider": { "hosts": hosts, "oauth": false },
        })
        .to_string()
    }

    fn migrate(_from: u32) -> Result<(), Error> {
        // No tables: the connections are the core's.
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        Response {
            status: 404,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: format!("no route {} {}", req.method, req.path).into_bytes(),
        }
    }

    fn handle_tool(name: String, args: String) -> Result<String, Error> {
        tool(&name, &Args::parse(&args)?).map(|answer| answer.to_string())
    }

    fn on_hook(_hook: String, _payload: String) -> Result<HookResult, Error> {
        Ok(HookResult::Continue)
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`GitForgejo`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::GitForgejo;

    crate::bindings::export!(GitForgejo with_types_in crate::bindings);
}
