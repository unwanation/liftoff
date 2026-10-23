//! GitLab as one connection: the REST API below `/api/v4` and the OAuth
//! device flow below `/oauth`, both on the host of the module's config.

use serde_json::{Value, json};

use crate::bindings::liftoff::module::host::{self, Error, Level};
use crate::bindings::wasi::clocks::monotonic_clock;
use crate::bindings::wasi::http::types::Method;
use crate::http::{self, Answer, Call, form, path};

/// The GitLab the module serves when its config names no `host`.
pub(crate) const DEFAULT_HOST: &str = "gitlab.com";

/// The scope a connection needs: `api` (roles, CI variables, forks,
/// `liftoff.toml`, the container registry's API).
pub(crate) const SCOPE: &str = "api";

/// The access level from which an account administers a project:
/// Maintainer (40) and Owner (50).
const MAINTAINER: u64 = 40;
/// The access level from which an account pushes: Developer.
const DEVELOPER: u64 = 30;
/// The access level from which an account reads the code: Reporter.
const REPORTER: u64 = 20;

/// How many times an idempotent request answered with `5xx` is sent again
/// (GIT-14).
const RETRIES: u32 = 1;

/// How long to wait between two looks at a fork GitLab is still copying,
/// in nanoseconds (`wasi:clocks`): one second.
const IMPORT_POLL_NANOS: u64 = 1_000_000_000;
/// How many times a fork GitLab is still copying is looked at before the
/// module gives up: the wait (about six seconds) stays within the
/// daemon's default call timeout of ten.
const IMPORT_POLLS: u32 = 6;

/// A module error with `code`.
pub(crate) fn error(code: &str, message: impl Into<String>) -> Error {
    Error {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// `owner/name` as the core writes a repository: GitLab's project path.
fn project(repo: &str) -> Result<&str, Error> {
    repo.split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .map(|_| repo)
        .ok_or_else(|| error("validation", format!("{repo:?} is not owner/name")))
}

/// The role of an account on a project from its `permissions` (the higher
/// of the project's and the group's access level) and its `visibility`:
/// a public or internal project is readable by every signed-in account.
fn role(project: &Value) -> &'static str {
    let permissions = project.get("permissions").unwrap_or(&Value::Null);
    let level = |of: &str| {
        permissions
            .get(of)
            .and_then(|access| access.get("access_level"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let level = level("project_access").max(level("group_access"));
    let visible = matches!(
        project.get("visibility").and_then(Value::as_str),
        Some("public" | "internal")
    );
    if level >= MAINTAINER {
        "admin"
    } else if level >= DEVELOPER {
        "write"
    } else if level >= REPORTER || visible {
        "read"
    } else {
        "none"
    }
}

/// How a request is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Sent again once on `5xx` (`GET`, `PUT`, `DELETE`, the variable's
    /// `POST`, which a second call answers "already taken", and the
    /// device flow's `POST`s, which create nothing a second call would
    /// trip on).
    Once,
    /// Sent once: a `5xx` may come after GitLab already created what it
    /// asked for (`POST /projects/{t}/fork`).
    Never,
}

/// The answer of `call`, sent again up to [`RETRIES`] times while it
/// answers `5xx` if `retry` allows; a request that never got an answer is
/// not retried.
fn send(on: &str, call: &Call<'_>, retry: Retry) -> Result<Answer, Error> {
    let mut retries_left = if retry == Retry::Once { RETRIES } else { 0 };
    loop {
        let answer = http::send(call).map_err(|why| upstream(on, &why))?;
        if answer.status >= 500 && retries_left > 0 {
            retries_left = retries_left.saturating_sub(1);
            continue;
        }
        return Ok(answer);
    }
}

/// GitLab failed: the core's `Upstream` (GIT-14).
fn upstream(on: &str, why: &str) -> Error {
    error("upstream", format!("{on}: {why}"))
}

/// The JSON body of `answer`.
fn json_of(on: &str, answer: &Answer) -> Result<Value, Error> {
    serde_json::from_slice(&answer.body)
        .map_err(|_malformed| upstream(on, &format!("malformed answer (HTTP {})", answer.status)))
}

/// GitLab's `{"message"}` (or `{"error"}`) of an error answer, as text:
/// a validation message is an object of field → messages.
fn message_of(answer: &Answer) -> Option<String> {
    let body = serde_json::from_slice::<Value>(&answer.body).ok()?;
    match body.get("message").or_else(|| body.get("error"))? {
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// An answer the call did not expect: `5xx` after the retry and the like.
fn unexpected(on: &str, answer: &Answer) -> Error {
    let status = answer.status;
    upstream(
        on,
        &message_of(answer).map_or_else(
            || format!("HTTP {status}"),
            |message| format!("HTTP {status}: {message}"),
        ),
    )
}

/// The `forbidden` of a `403 insufficient_scope`: the token GitLab knows
/// lacks [`SCOPE`] (one that only reads the user passes `GET /user`).
/// `None` for any other answer, which the caller maps.
fn insufficient_scope(host: &str, answer: &Answer) -> Option<Error> {
    let lacks = answer.status == 403
        && serde_json::from_slice::<Value>(&answer.body)
            .ok()?
            .get("error")
            .and_then(Value::as_str)
            == Some("insufficient_scope");
    lacks.then(|| {
        error(
            "forbidden",
            format!(
                "the stored token lacks the scope {SCOPE}; run `lof git connect gitlab --host {host}` again with a token that has it"
            ),
        )
    })
}

/// Whether GitLab refused a create because the name is taken (`400` with
/// "has already been taken" in a field's messages).
fn taken(answer: &Answer) -> bool {
    String::from_utf8_lossy(&answer.body).contains("has already been taken")
}

/// One connection to a GitLab: an account's token on `host`.
#[derive(Debug)]
pub(crate) struct Gitlab<'a> {
    /// The GitLab host of the module's config.
    pub(crate) host: &'a str,
    /// The plaintext credential of the connection.
    pub(crate) token: &'a str,
}

impl Gitlab<'_> {
    /// A request below `/api/v4` as the connection.
    fn api(&self, method: Method, segments: &[&str], query: &[(&str, &str)]) -> Call<'_> {
        let mut path = path(&[&["api", "v4"], segments].concat());
        if !query.is_empty() {
            path.push('?');
            path.push_str(&form(query));
        }
        Call {
            method,
            authority: self.host,
            path,
            accept: "application/json",
            authorization: Some(format!("Bearer {}", self.token)),
            body: None,
        }
    }

    /// [`Gitlab::api`] with a JSON `body`.
    fn api_json(&self, method: Method, segments: &[&str], body: &Value) -> Call<'_> {
        Call {
            body: Some(("application/json", body.to_string().into_bytes())),
            ..self.api(method, segments, &[])
        }
    }

    /// `401` of the API for a stored connection: its credential itself is
    /// refused (revoked or expired).
    fn rejected(&self) -> Error {
        let host = self.host;
        error(
            "forbidden",
            format!(
                "{host} rejected the stored token (revoked or expired): run `lof git connect gitlab --host {host}` again"
            ),
        )
    }

    /// `GET /user`: the username of the token's account (GIT-14). GitLab
    /// answers `401` for a token it does not know and `403
    /// insufficient_scope` for one without a scope that reads the user;
    /// both name the scope liftoff needs.
    pub(crate) fn login(&self) -> Result<String, Error> {
        let host = self.host;
        let answer = send(host, &self.api(Method::Get, &["user"], &[]), Retry::Once)?;
        match answer.status {
            200 => json_of(host, &answer)?
                .get("username")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| upstream(host, "the user answer has no username")),
            401 => Err(error(
                "forbidden",
                format!(
                    "{host} rejected the token; liftoff needs a personal access token with the scope {SCOPE}"
                ),
            )),
            403 => Err(error(
                "forbidden",
                format!(
                    "the token lacks the scope {SCOPE}; liftoff needs a token with the scope {SCOPE}"
                ),
            )),
            _ => Err(unexpected(host, &answer)),
        }
    }

    /// `GET /projects/{id}`: what the account may do on it (GIT-09).
    pub(crate) fn repo_role(&self, repo: &str) -> Result<&'static str, Error> {
        let answer = send(
            self.host,
            &self.api(Method::Get, &["projects", project(repo)?], &[]),
            Retry::Once,
        )?;
        if let Some(lacks) = insufficient_scope(self.host, &answer) {
            return Err(lacks);
        }
        match answer.status {
            200 => Ok(role(&json_of(self.host, &answer)?)),
            // GitLab hides a project the account may not see.
            403 | 404 => Ok("none"),
            401 => Err(self.rejected()),
            _ => Err(unexpected(self.host, &answer)),
        }
    }

    /// Whether the image `owner/name:tag` is in the container registry of
    /// the project `owner/name` (GIT-10): `GET /projects/{id}/registry/
    /// repositories?tags=true`, the repository whose `path` is the
    /// project's own (not one of its named sub-images) having the tag.
    /// The first 100 image repositories of the project are looked at.
    pub(crate) fn image_exists(&self, owner: &str, name: &str, tag: &str) -> Result<bool, Error> {
        let repo = format!("{owner}/{name}");
        let answer = send(
            self.host,
            &self.api(
                Method::Get,
                &["projects", project(&repo)?, "registry", "repositories"],
                &[("tags", "true"), ("per_page", "100")],
            ),
            Retry::Once,
        )?;
        if let Some(lacks) = insufficient_scope(self.host, &answer) {
            return Err(lacks);
        }
        match answer.status {
            200 => {
                let repositories = json_of(self.host, &answer)?;
                let has_tag = |repository: &Value| {
                    repository
                        .get("tags")
                        .and_then(Value::as_array)
                        .is_some_and(|tags| {
                            tags.iter()
                                .any(|found| found.get("name").and_then(Value::as_str) == Some(tag))
                        })
                };
                Ok(repositories.as_array().is_some_and(|repositories| {
                    repositories.iter().any(|repository| {
                        repository
                            .get("path")
                            .and_then(Value::as_str)
                            .is_some_and(|path| path.eq_ignore_ascii_case(&repo))
                            && has_tag(repository)
                    })
                }))
            }
            // A project the account may not see has no image it can pull.
            403 | 404 => Ok(false),
            401 => Err(self.rejected()),
            _ => Err(unexpected(self.host, &answer)),
        }
    }

    /// `POST /projects/{id}/variables` `{key, value, masked: true,
    /// protected: true}`; a key GitLab already has (`400` "has already
    /// been taken") is updated with `PUT /projects/{id}/variables/{key}`
    /// (GIT-11). Masked, so a job log never shows it: GitLab masks only a
    /// value of at least 8 characters from its maskable set and refuses
    /// any other (`validation`, never stored unmasked). Protected, so only
    /// the pipelines of protected branches and tags receive it.
    pub(crate) fn put_ci_secret(
        &self,
        login: &str,
        repo: &str,
        key: &str,
        value: &str,
    ) -> Result<(), Error> {
        let id = project(repo)?;
        let refused = || {
            error(
                "forbidden",
                format!("{login} may not manage the CI variables of {repo} (it takes Maintainer)"),
            )
        };
        let created = send(
            self.host,
            &self.api_json(
                Method::Post,
                &["projects", id, "variables"],
                &json!({ "key": key, "value": value, "masked": true, "protected": true }),
            ),
            Retry::Once,
        )?;
        let answer = match created.status {
            200..=299 => return Ok(()),
            400 if taken(&created) => send(
                self.host,
                &self.api_json(
                    Method::Put,
                    &["projects", id, "variables", key],
                    &json!({ "value": value, "masked": true, "protected": true }),
                ),
                Retry::Once,
            )?,
            _ => created,
        };
        if let Some(lacks) = insufficient_scope(self.host, &answer) {
            return Err(lacks);
        }
        match answer.status {
            200..=299 => Ok(()),
            403 | 404 => Err(refused()),
            401 => Err(self.rejected()),
            400 => Err(error(
                "validation",
                format!(
                    "{} refused the CI variable {key}: {} (a masked value needs at least 8 characters, all from GitLab's maskable set)",
                    self.host,
                    message_of(&answer).unwrap_or_default()
                ),
            )),
            _ => Err(unexpected(self.host, &answer)),
        }
    }

    /// `GET /projects/{id}/repository/files/liftoff.toml/raw?ref=HEAD`:
    /// the file on the default branch, `None` when the project has none
    /// (or the account does not see it) (GIT-13).
    pub(crate) fn manifest(&self, repo: &str) -> Result<Option<String>, Error> {
        let answer = send(
            self.host,
            &self.api(
                Method::Get,
                &[
                    "projects",
                    project(repo)?,
                    "repository",
                    "files",
                    "liftoff.toml",
                    "raw",
                ],
                &[("ref", "HEAD")],
            ),
            Retry::Once,
        )?;
        if let Some(lacks) = insufficient_scope(self.host, &answer) {
            return Err(lacks);
        }
        match answer.status {
            200 => String::from_utf8(answer.body)
                .map(Some)
                .map_err(|_bad| upstream(self.host, "liftoff.toml is not UTF-8")),
            403 | 404 => Ok(None),
            401 => Err(self.rejected()),
            _ => Err(unexpected(self.host, &answer)),
        }
    }

    /// The project `owner/name` made from `template` (GIT-12): `POST /projects/{template}/fork` `{namespace_path, name,
    /// path, visibility}`, since GitLab has no template repositories, then
    /// `DELETE /projects/{new}/fork`, so the project is not left a fork of
    /// the template, once GitLab finished copying it (see
    /// [`Gitlab::imported`]). The fork is not retried (see
    /// [`Retry::Never`]); a failed unlink is logged and the project, which
    /// exists and works, is still answered.
    pub(crate) fn fork(
        &self,
        template: &str,
        owner: &str,
        name: &str,
        private: bool,
    ) -> Result<String, Error> {
        let host = self.host;
        let body = json!({
            "namespace_path": owner,
            "name": name,
            "path": name,
            "visibility": if private { "private" } else { "public" },
        });
        let answer = send(
            host,
            &self.api_json(
                Method::Post,
                &["projects", project(template)?, "fork"],
                &body,
            ),
            Retry::Never,
        )?;
        if let Some(lacks) = insufficient_scope(host, &answer) {
            return Err(lacks);
        }
        let (created, status) = match answer.status {
            200..=299 => {
                let fork = json_of(host, &answer)?;
                let created = fork
                    .get("path_with_namespace")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| upstream(host, "the fork has no path_with_namespace"))?;
                (created, import_status(&fork))
            }
            401 => return Err(self.rejected()),
            403 => {
                return Err(error(
                    "forbidden",
                    format!("the connection may not create {owner}/{name} from {template}"),
                ));
            }
            404 => {
                return Err(error(
                    "not_found",
                    format!("template {template} (or the namespace {owner}) not found on {host}"),
                ));
            }
            409 => {
                return Err(error(
                    "conflict",
                    format!("{owner}/{name} already exists on {host}"),
                ));
            }
            400 | 422 if taken(&answer) => {
                return Err(error(
                    "conflict",
                    format!("{owner}/{name} already exists on {host}"),
                ));
            }
            400 | 422 => {
                return Err(error(
                    "validation",
                    format!(
                        "{host} refused {owner}/{name}: {}",
                        message_of(&answer).unwrap_or_default()
                    ),
                ));
            }
            _ => return Err(unexpected(host, &answer)),
        };
        self.imported(&created, template, status)?;
        self.unlink(&created, template);
        Ok(created)
    }

    /// `DELETE /projects/{created}/fork`: `created` is no longer a fork of
    /// `template`. A failure is logged only: the project exists and works.
    fn unlink(&self, created: &str, template: &str) {
        let host = self.host;
        let unlinked = send(
            host,
            &self.api(Method::Delete, &["projects", created, "fork"], &[]),
            Retry::Once,
        );
        match unlinked {
            // `304`: GitLab saw no fork relation.
            Ok(answer) if (200..=299).contains(&answer.status) || answer.status == 304 => {}
            Ok(answer) => host::log(
                Level::Warn,
                &format!(
                    "{created} stays a fork of {template}: {host} answered HTTP {} to removing the relation",
                    answer.status
                ),
            ),
            Err(why) => host::log(
                Level::Warn,
                &format!("{created} stays a fork of {template}: {}", why.message),
            ),
        }
    }

    /// Waits until GitLab finished copying the fork `created` of
    /// `template`, whose last known `import_status` is `status`: `lof
    /// create` clones it right away, and its fork relation is removed only
    /// afterwards. `failed` is `upstream`, and so is a copy that outlasts
    /// [`IMPORT_POLLS`] looks [`IMPORT_POLL_NANOS`] apart (the project exists,
    /// still a fork, and the message names it).
    fn imported(&self, created: &str, template: &str, mut status: String) -> Result<(), Error> {
        let host = self.host;
        let mut looks_left = IMPORT_POLLS;
        loop {
            match status.as_str() {
                "finished" | "none" => return Ok(()),
                "failed" => {
                    return Err(upstream(
                        host,
                        &format!(
                            "{created} was created, but copying {template} into it failed; delete it on {host} and try again"
                        ),
                    ));
                }
                _ if looks_left == 0 => {
                    return Err(upstream(
                        host,
                        &format!(
                            "{created} was created, but {host} is still copying {template} into it (import {status}); wait until it finished"
                        ),
                    ));
                }
                _ => {}
            }
            looks_left = looks_left.saturating_sub(1);
            monotonic_clock::subscribe_duration(IMPORT_POLL_NANOS).block();
            let answer = send(
                host,
                &self.api(Method::Get, &["projects", created], &[]),
                Retry::Once,
            )?;
            status = match answer.status {
                200 => import_status(&json_of(host, &answer)?),
                401 => return Err(self.rejected()),
                _ => return Err(unexpected(host, &answer)),
            };
        }
    }
}

/// A project's `import_status` (`none` when GitLab names none).
fn import_status(project: &Value) -> String {
    project
        .get("import_status")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_owned()
}

/// The OAuth application `client_id` of the module's config on `host`
/// (RFC 8628; GitLab 17.2 or later).
#[derive(Debug)]
pub(crate) struct DeviceFlow<'a> {
    /// The GitLab host of the module's config.
    pub(crate) host: &'a str,
    /// The OAuth application.
    pub(crate) client_id: &'a str,
}

impl DeviceFlow<'_> {
    /// A form `POST` below `/oauth`: the JSON answer of `200` and of
    /// GitLab's OAuth errors (`400`, `401`), which name an `error`.
    fn post(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, Error> {
        let host = self.host;
        let call = Call {
            method: Method::Post,
            authority: host,
            path: path.to_owned(),
            accept: "application/json",
            authorization: None,
            body: Some((
                "application/x-www-form-urlencoded",
                form(params).into_bytes(),
            )),
        };
        let answer = send(host, &call, Retry::Once)?;
        match answer.status {
            200 => json_of(host, &answer),
            400 | 401 => {
                let body = json_of(host, &answer)?;
                if body.get("error").and_then(Value::as_str).is_some() {
                    Ok(body)
                } else {
                    Err(unexpected(host, &answer))
                }
            }
            // A GitLab before 17.2 has no device flow.
            404 => Err(error(
                "validation",
                format!("{host} has no OAuth device flow (it takes GitLab 17.2 or later)"),
            )),
            _ => Err(unexpected(host, &answer)),
        }
    }

    /// `POST /oauth/authorize_device`: the codes of a new flow asking for
    /// [`SCOPE`].
    pub(crate) fn start(&self) -> Result<Value, Error> {
        let host = self.host;
        let answer = self.post(
            "/oauth/authorize_device",
            &[("client_id", self.client_id), ("scope", SCOPE)],
        )?;
        if let Some(code) = answer.get("error").and_then(Value::as_str) {
            return Err(oauth_error(host, code, &answer));
        }
        let text = |name: &str| {
            answer
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| upstream(host, &format!("the device code answer has no {name}")))
        };
        let number = |name: &str| {
            answer
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| upstream(host, &format!("the device code answer has no {name}")))
        };
        Ok(json!({
            "device_code": text("device_code")?,
            "user_code": text("user_code")?,
            "verification_uri": text("verification_uri")?,
            "interval": number("interval")?,
            "expires_in": number("expires_in")?,
        }))
    }

    /// `POST /oauth/token` once: `{"status":"pending"}` while the person
    /// has not confirmed, `{"status":"slow_down"}` when GitLab asks to
    /// poll less often (RFC 8628 3.5; GitLab names no new interval, so
    /// one is passed only when it sends one), else
    /// `{"status":"granted","token","expires_in"}` (the token's lifetime
    /// in seconds, when GitLab names it; its refresh token is dropped).
    pub(crate) fn poll(&self, device_code: &str) -> Result<Value, Error> {
        let host = self.host;
        let answer = self.post(
            "/oauth/token",
            &[
                ("client_id", self.client_id),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )?;
        if let Some(token) = answer.get("access_token").and_then(Value::as_str) {
            return Ok(answer.get("expires_in").and_then(Value::as_u64).map_or_else(
                || json!({ "status": "granted", "token": token }),
                |expires_in| json!({ "status": "granted", "token": token, "expires_in": expires_in }),
            ));
        }
        match answer.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => Ok(json!({ "status": "pending" })),
            Some("slow_down") => Ok(answer.get("interval").and_then(Value::as_u64).map_or_else(
                || json!({ "status": "slow_down" }),
                |interval| json!({ "status": "slow_down", "interval": interval }),
            )),
            Some(code) => Err(oauth_error(host, code, &answer)),
            None => Err(upstream(
                host,
                "the token answer has neither access_token nor error",
            )),
        }
    }
}

/// The core's error for GitLab's OAuth error `code` (RFC 6749 5.2, RFC
/// 8628 3.5).
fn oauth_error(host: &str, code: &str, answer: &Value) -> Error {
    let description = answer
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or(code);
    match code {
        "expired_token" => error(
            "gone",
            format!("the device code expired: run `lof git connect gitlab --host {host}` again"),
        ),
        "access_denied" => error(
            "forbidden",
            format!("the person denied the access at {host}"),
        ),
        "invalid_grant" => error(
            "validation",
            format!("{host} does not know the device code"),
        ),
        "invalid_client" | "unauthorized_client" | "unsupported_grant_type" | "invalid_scope" => {
            error(
                "validation",
                format!(
                    "the module's OAuth application cannot run the device flow on {host}: {description}"
                ),
            )
        }
        _ => upstream(host, &format!("OAuth error {code}: {description}")),
    }
}
