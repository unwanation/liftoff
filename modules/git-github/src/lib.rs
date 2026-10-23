//! `git.github`: the module that serves the core's `Git` port for
//! `github.com`, always as
//! the user's own connection (`Authorization: Bearer <credential>`).
//!
//! Its configuration: `client_id` (a secret, optional), the OAuth
//! application of the GitHub device flow. With it the manifest declares
//! `git_provider.oauth: true` and `lof git connect github` runs the device
//! flow; without it the module connects by token only and the CLI asks
//! for one. The manifest is read when the module loads, so a change of
//! `client_id` takes effect when it is loaded again (`lof module config`
//! reloads it). It serves `github.com` only (GitHub Enterprise is not
//! this module's), and its `http_allow` is the REST API, the OAuth host
//! and the container registry.
//!
//! The core's `ModuleGit` calls `handle-tool` without an actor, the
//! connection in the arguments (its credential in plaintext for that call
//! only; the module stores nothing):
//!
//! | Tool | Arguments | GitHub | Answer |
//! |---|---|---|---|
//! | `validate_token` | `{host, token}` | `GET api.github.com/user`; `X-OAuth-Scopes` must name `repo` and `read:packages` | `{login}` |
//! | `repo_role` | `{host, login, token, repo}` | `GET /repos/{repo}` `permissions` | `{role}`: `admin`, `write`, `read`, `none` (`403`/`404`) |
//! | `image_exists` | `{…, owner, name, tag}` | `GET ghcr.io/token?scope=repository:{owner}/{name}:pull&service=ghcr.io` (Basic `login:token`), then `HEAD ghcr.io/v2/{owner}/{name}/manifests/{tag}` | `{exists}` |
//! | `put_ci_secret` | `{…, repo, key, value}` | `GET /repos/{repo}/actions/secrets/public-key`, then `PUT /repos/{repo}/actions/secrets/{key}` `{"encrypted_value","key_id"}` | `{}` |
//! | `list_templates` | `{…, repos}` | `GET /repos/{repo}/contents/liftoff.toml` each | `{templates: [{repo, manifest}]}` |
//! | `generate_from_template` | `{…, template, owner, name, private}` | `POST /repos/{template}/generate` `{owner, name, private, include_all: true}` | `{repo}` |
//! | `device_start` | `{host}` | `POST github.com/login/device/code` `client_id`, `scope=repo read:packages` | `{device_code, user_code, verification_uri, interval, expires_in}` |
//! | `device_poll` | `{host, device_code}` | `POST github.com/login/oauth/access_token` | `{status: "pending"}`, `{status: "slow_down", interval?}` or `{status: "granted", token}` |
//!
//! Choices:
//!
//! - `image_exists` asks the registry (`HEAD` of the tag's manifest with
//!   a pull token exchanged for the connection), not `GET
//!   /user/packages/container/{pkg}/versions`: the packages API differs
//!   between users and organisations, is paginated, and answers what the
//!   account published rather than what it can pull.
//! - A token without `X-OAuth-Scopes` (a fine-grained token, whose
//!   permissions are per repository) is accepted; each later call answers
//!   for what it may do.
//! - `put_ci_secret` seals the value in a libsodium sealed box
//!   (`crypto_box`, pure Rust) with randomness from `wasi:random`
//!   (`seal.rs`); the plan's live check of is the
//!   `wasm32-wasip2` build of `just modules`, which passes, and GIT-05
//!   opens the box with the repository's private key.
//!
//! A `5xx` of an idempotent request (and of the device flow's `POST`s) is
//! sent once more, then the error is `upstream` (GIT-08); `POST
//! /repos/{t}/generate` is sent once, since GitHub may have created the
//! repository before failing. A `401` of a stored connection is
//! `forbidden` with a `lof git connect` hint; a host other than
//! `github.com` is `validation`. Without `client_id` the device flow is
//! `validation` with a hint to connect with `--token` (GIT-01); an
//! expired device code is `gone`, a denied one `forbidden`. The tools
//! are not in the manifest's `tools`: they take a plaintext credential,
//! so they are no MCP tools and only the core calls them.

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

mod github;
mod http;
mod seal;

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error};
use serde_json::{Map, Value, json};

use crate::github::{API, DeviceFlow, GHCR, Github, HOST, error};

/// The component's export.
struct GitGithub;

/// The configured OAuth application, if any.
fn client_id() -> Option<String> {
    host::config_get("client_id")
        .map(|raw| raw.trim().to_owned())
        .filter(|id| !id.is_empty())
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

    /// The `host` argument, which must be `github.com`.
    fn served(&self) -> Result<(), Error> {
        let host = self.text("host")?;
        if host.eq_ignore_ascii_case(HOST) {
            Ok(())
        } else {
            Err(error(
                "validation",
                format!("git.github serves {HOST} only, not {host}"),
            ))
        }
    }

    /// The connection the call acts as.
    fn github(&self) -> Result<Github<'_>, Error> {
        self.served()?;
        Ok(Github {
            token: self.text("token")?,
        })
    }
}

/// The tools of the module (see the table above).
const TOOLS: [&str; 8] = [
    "validate_token",
    "repo_role",
    "image_exists",
    "put_ci_secret",
    "list_templates",
    "generate_from_template",
    "device_start",
    "device_poll",
];

/// The device flow of the configured OAuth application, else the hint to
/// connect with a token (GIT-01).
fn device_flow(client_id: Option<&str>) -> Result<DeviceFlow<'_>, Error> {
    client_id.map(|client_id| DeviceFlow { client_id }).ok_or_else(|| {
        error(
            "validation",
            format!(
                "git.github has no OAuth application (its config client_id): connect with a token instead, `lof git connect github --token <token>` (scopes {})",
                github::SCOPES
            ),
        )
    })
}

/// Runs the tool `name` on `args`: its JSON answer.
fn tool(name: &str, args: &Args) -> Result<Value, Error> {
    if !TOOLS.contains(&name) {
        return Err(error("not_found", format!("tool {name}")));
    }
    match name {
        "device_start" => {
            args.served()?;
            let client_id = client_id();
            device_flow(client_id.as_deref())?.start()
        }
        "device_poll" => {
            args.served()?;
            let client_id = client_id();
            device_flow(client_id.as_deref())?.poll(args.text("device_code")?)
        }
        "validate_token" => Ok(json!({ "login": args.github()?.login()? })),
        "repo_role" => Ok(json!({ "role": args.github()?.repo_role(args.text("repo")?)? })),
        "image_exists" => Ok(json!({
            "exists": args.github()?.image_exists(
                args.text("login")?,
                args.text("owner")?,
                args.text("name")?,
                args.text("tag")?,
            )?,
        })),
        "put_ci_secret" => {
            args.github()?.put_ci_secret(
                args.text("login")?,
                args.text("repo")?,
                args.text("key")?,
                args.text("value")?,
            )?;
            Ok(json!({}))
        }
        "list_templates" => {
            let github = args.github()?;
            let templates = args
                .texts("repos")?
                .into_iter()
                .map(|repo| Ok(json!({ "repo": repo, "manifest": github.manifest(repo)? })))
                .collect::<Result<Vec<Value>, Error>>()?;
            Ok(json!({ "templates": templates }))
        }
        "generate_from_template" => Ok(json!({
            "repo": args.github()?.generate(
                args.text("template")?,
                args.text("owner")?,
                args.text("name")?,
                args.flag("private")?,
            )?,
        })),
        _ => Err(error("not_found", format!("tool {name}"))),
    }
}

impl Guest for GitGithub {
    fn manifest() -> String {
        json!({
            "name": "git.github",
            "version": env!("CARGO_PKG_VERSION"),
            "config_schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "client_id": {
                        "type": "string",
                        "format": "secret",
                        "minLength": 1,
                        "description": "Client ID of a GitHub OAuth application with the device flow enabled; without it, connections are made by token",
                    },
                },
            },
            "http_allow": [API, HOST, GHCR],
            "git_provider": { "hosts": [HOST], "oauth": client_id().is_some() },
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

/// The exported symbols `wit-bindgen` generates for [`GitGithub`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::GitGithub;

    crate::bindings::export!(GitGithub with_types_in crate::bindings);
}
