//! `git.gitlab`: the module that serves the core's `Git` port for
//! GitLab (`gitlab.com` or one self-hosted GitLab), always as the user's own connection
//! (`Authorization: Bearer <credential>`).
//!
//! Its configuration: `host` (optional, `gitlab.com` by default), the
//! GitLab it serves, and `client_id` (a secret, optional), the OAuth
//! application of GitLab's device flow (GitLab 17.2 or later). The
//! manifest declares `host` as its only `git_provider.hosts` and its only
//! `http_allow` (HTTPS on port 443: a GitLab on another port or on a
//! private address is not reachable), and `git_provider.oauth: true` with
//! `client_id`. The manifest is read when the module loads, so a change
//! of the config takes effect when it is loaded again (`lof module
//! config` reloads it). One instance serves one GitLab.
//!
//! The core's `ModuleGit` calls `handle-tool` without an actor, the
//! connection in the arguments (its credential in plaintext for that call
//! only; the module stores nothing). `{id}` is the project's URL-encoded
//! path (`acme%2Fweb`):
//!
//! | Tool | Arguments | GitLab (below `/api/v4`) | Answer |
//! |---|---|---|---|
//! | `validate_token` | `{host, token}` | `GET /user`; `401` / `403` → forbidden naming the scope `api` | `{login}` |
//! | `repo_role` | `{host, login, token, repo}` | `GET /projects/{id}`: the higher of `permissions.project_access` and `group_access` `access_level`, ≥ 40 admin, 30 write, 20 (or a public or internal project) read | `{role}` (`404` → `none`) |
//! | `image_exists` | `{…, owner, name, tag}` | `GET /projects/{id}/registry/repositories?tags=true`: the repository whose `path` is `owner/name` has the tag | `{exists}` |
//! | `put_ci_secret` | `{…, repo, key, value}` | `POST /projects/{id}/variables` `{key, value, masked: true, protected: true}`; on `400` "has already been taken" `PUT /projects/{id}/variables/{key}` | `{}` |
//! | `list_templates` | `{…, repos}` | `GET /projects/{id}/repository/files/liftoff.toml/raw?ref=HEAD` each | `{templates: [{repo, manifest}]}` |
//! | `generate_from_template` | `{…, template, owner, name, private}` | `POST /projects/{template}/fork` `{namespace_path, name, path, visibility}`, `GET /projects/{new}` until its `import_status` is `finished`, then `DELETE /projects/{new}/fork` | `{repo}` |
//! | `device_start` | `{host}` | `POST /oauth/authorize_device` `client_id`, `scope=api` | `{device_code, user_code, verification_uri, interval, expires_in}` |
//! | `device_poll` | `{host, device_code}` | `POST /oauth/token` | `{status: "pending"}`, `{status: "slow_down"}` or `{status: "granted", token, expires_in}` |
//!
//! Choices:
//!
//! - `generate_from_template` forks and renames, as designed
//!   (GitLab has no template repositories), rather than `POST /projects`
//!   with an `import_url`: an import of a private template would put the
//!   user's token into the URL. The fork relation is removed afterwards,
//!   so the new project is not a fork of the template. GitLab copies a
//!   fork in the background and the CLI clones it right away, so
//!   the module answers once `import_status` is `finished` (a look a
//!   second, six at most, within the default call timeout); `failed` or
//!   a longer copy is `upstream` naming the project, which is then not
//!   unlinked. How long a real fork takes is the plan's live check of
//!
//! - `GET /user` does not tell a token's scopes, so a token that reads
//!   the user but lacks `api` passes the check and its later calls are
//!   refused: GitLab's `403 insufficient_scope` is `forbidden` naming the
//!   scope, never a silent "no access".
//! - CI variables are masked and protected. GitLab masks only a value of
//!   at least 8 characters from its maskable set and refuses any other
//!   (`validation` saying so; it is never stored unmasked), and only the
//!   pipelines of protected branches and tags receive a protected one.
//! - GitLab's OAuth tokens expire after two hours: the grant passes
//!   `expires_in` on, so the core records when the connection expires,
//!   but it stores no refresh token, and the connection is then refused
//!   with the hint to connect again (a personal access token lasts
//!   longer).
//!
//! A `5xx` of an idempotent request (and of the device flow's `POST`s and
//! the variable's `POST`) is sent once more, then the error is `upstream`;
//! the fork is sent once, since GitLab may have created the project
//! before failing (GIT-14). A `401` of a stored connection is `forbidden`
//! with a `lof git connect gitlab` hint; a host other than the
//! configured one is `validation`. Without `client_id` the core refuses
//! the device flow before it reaches the module, which answers the same
//! `validation` with a hint to connect with `--token`; an expired device
//! code is `gone`, a denied one `forbidden`. The tools are not in the
//! manifest's `tools`: they take a plaintext credential, so they are no
//! MCP tools and only the core calls them.

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

mod gitlab;
mod http;

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error};
use serde_json::{Map, Value, json};

use crate::gitlab::{DEFAULT_HOST, DeviceFlow, Gitlab, SCOPE, error};

/// The component's export.
struct GitGitlab;

/// The GitLab of the module's config `host`, `gitlab.com` without one,
/// lower-cased as the core writes hosts.
fn served_host() -> String {
    host::config_get("host")
        .map(|raw| raw.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| DEFAULT_HOST.to_owned())
}

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

    /// The `host` argument, which must be `served`.
    fn served(&self, served: &str) -> Result<(), Error> {
        let host = self.text("host")?;
        if host.eq_ignore_ascii_case(served) {
            Ok(())
        } else {
            Err(error(
                "validation",
                format!("git.gitlab serves {served} only, not {host}"),
            ))
        }
    }

    /// The connection the call acts as, on `served`.
    fn gitlab<'a>(&'a self, served: &'a str) -> Result<Gitlab<'a>, Error> {
        self.served(served)?;
        Ok(Gitlab {
            host: served,
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

/// The device flow of the configured OAuth application on `host`, else
/// the hint to connect with a token.
fn device_flow<'a>(host: &'a str, client_id: Option<&'a str>) -> Result<DeviceFlow<'a>, Error> {
    client_id
        .map(|client_id| DeviceFlow { host, client_id })
        .ok_or_else(|| {
            error(
                "validation",
                format!(
                    "git.gitlab has no OAuth application (its config client_id): connect with a token instead, `lof git connect gitlab --host {host} --token <token>` (scope {SCOPE})"
                ),
            )
        })
}

/// Runs the tool `name` on `args`: its JSON answer.
fn tool(name: &str, args: &Args) -> Result<Value, Error> {
    if !TOOLS.contains(&name) {
        return Err(error("not_found", format!("tool {name}")));
    }
    let served = served_host();
    match name {
        "device_start" => {
            args.served(&served)?;
            let client_id = client_id();
            device_flow(&served, client_id.as_deref())?.start()
        }
        "device_poll" => {
            args.served(&served)?;
            let client_id = client_id();
            device_flow(&served, client_id.as_deref())?.poll(args.text("device_code")?)
        }
        "validate_token" => Ok(json!({ "login": args.gitlab(&served)?.login()? })),
        "repo_role" => Ok(json!({ "role": args.gitlab(&served)?.repo_role(args.text("repo")?)? })),
        "image_exists" => Ok(json!({
            "exists": args.gitlab(&served)?.image_exists(
                args.text("owner")?,
                args.text("name")?,
                args.text("tag")?,
            )?,
        })),
        "put_ci_secret" => {
            args.gitlab(&served)?.put_ci_secret(
                args.text("login")?,
                args.text("repo")?,
                args.text("key")?,
                args.text("value")?,
            )?;
            Ok(json!({}))
        }
        "list_templates" => {
            let gitlab = args.gitlab(&served)?;
            let templates = args
                .texts("repos")?
                .into_iter()
                .map(|repo| Ok(json!({ "repo": repo, "manifest": gitlab.manifest(repo)? })))
                .collect::<Result<Vec<Value>, Error>>()?;
            Ok(json!({ "templates": templates }))
        }
        "generate_from_template" => Ok(json!({
            "repo": args.gitlab(&served)?.fork(
                args.text("template")?,
                args.text("owner")?,
                args.text("name")?,
                args.flag("private")?,
            )?,
        })),
        _ => Err(error("not_found", format!("tool {name}"))),
    }
}

impl Guest for GitGitlab {
    fn manifest() -> String {
        let host = served_host();
        json!({
            "name": "git.gitlab",
            "version": env!("CARGO_PKG_VERSION"),
            "config_schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "host": {
                        "type": "string",
                        "minLength": 1,
                        "pattern": "^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$",
                        "description": "The GitLab the module serves, a host name reached over HTTPS on port 443; gitlab.com without it",
                    },
                    "client_id": {
                        "type": "string",
                        "format": "secret",
                        "minLength": 1,
                        "description": "Application ID of a GitLab OAuth application (scope api, not confidential) for the device flow, GitLab 17.2 or later; without it, connections are made by token",
                    },
                },
            },
            "http_allow": [host],
            "git_provider": { "hosts": [host], "oauth": client_id().is_some() },
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

/// The exported symbols `wit-bindgen` generates for [`GitGitlab`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::GitGitlab;

    crate::bindings::export!(GitGitlab with_types_in crate::bindings);
}
