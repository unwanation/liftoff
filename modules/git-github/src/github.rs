//! GitHub as one connection: the REST API (`api.github.com`), the
//! container registry (`ghcr.io`) and the OAuth device flow
//! (`github.com`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::bindings::liftoff::module::host::Error;
use crate::bindings::wasi::http::types::Method;
use crate::http::{self, Answer, Call, encode, form, path};
use crate::seal::seal;

/// The forge host the module serves.
pub(crate) const HOST: &str = "github.com";
/// GitHub's REST API.
pub(crate) const API: &str = "api.github.com";
/// GitHub's container registry.
pub(crate) const GHCR: &str = "ghcr.io";
/// The REST API version the module speaks (`X-GitHub-Api-Version`).
pub(crate) const API_VERSION: &str = "2022-11-28";
/// What the module asks of GitHub's REST API.
const API_ACCEPT: &str = "application/vnd.github+json";

/// The scopes a connection needs: `repo` (roles, Actions secrets,
/// generating from templates, `liftoff.toml`) and `read:packages` (the
/// images of `ghcr.io`, which `write:packages` includes).
pub(crate) const SCOPES: &str = "repo read:packages";

/// How many times an idempotent request answered with `5xx` is sent again
/// (GIT-08).
const RETRIES: u32 = 1;

/// A module error with `code`.
pub(crate) fn error(code: &str, message: impl Into<String>) -> Error {
    Error {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// `owner/name` as the core writes a repository, split in two.
fn split_repo(repo: &str) -> Result<(&str, &str), Error> {
    repo.split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| error("validation", format!("{repo:?} is not owner/name")))
}

/// The role `permissions` of a repository give.
fn role(permissions: &Value) -> &'static str {
    let granted = |name: &str| {
        permissions
            .get(name)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    if granted("admin") {
        "admin"
    } else if granted("push") {
        "write"
    } else if granted("pull") {
        "read"
    } else {
        "none"
    }
}

/// The scopes of `X-OAuth-Scopes` the module needs but the token lacks.
/// A header that is absent (a fine-grained token, which has permissions
/// per repository instead of scopes) lacks none: its calls answer for
/// themselves.
fn missing_scopes(header: Option<&str>) -> Vec<&'static str> {
    let Some(header) = header else {
        return Vec::new();
    };
    let granted: Vec<&str> = header.split(',').map(str::trim).collect();
    let mut missing = Vec::new();
    if !granted.contains(&"repo") {
        missing.push("repo");
    }
    if !granted.contains(&"read:packages") && !granted.contains(&"write:packages") {
        missing.push("read:packages");
    }
    missing
}

/// How a request is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Sent again once on `5xx` (`GET`, `HEAD`, `PUT`, and the device
    /// flow's `POST`s, which create nothing a second call would trip on).
    Once,
    /// Sent once: a `5xx` may come after GitHub already created what it
    /// asked for (`POST /repos/{t}/generate`).
    Never,
}

/// Sends `call`, again up to [`RETRIES`] times while it answers `5xx` if
/// `retry` allows. A request that never got an answer is not retried.
fn send(call: &Call<'_>, retry: Retry) -> Result<Answer, Error> {
    let mut retries_left = if retry == Retry::Once { RETRIES } else { 0 };
    loop {
        let answer = http::send(call).map_err(|why| upstream(&why))?;
        if answer.status >= 500 && retries_left > 0 {
            retries_left = retries_left.saturating_sub(1);
            continue;
        }
        return Ok(answer);
    }
}

/// GitHub failed: the core's `Upstream` (GIT-08).
fn upstream(why: &str) -> Error {
    error("upstream", format!("{HOST}: {why}"))
}

/// The JSON body of `answer`.
fn json_of(answer: &Answer) -> Result<Value, Error> {
    serde_json::from_slice(&answer.body)
        .map_err(|_malformed| upstream(&format!("malformed answer (HTTP {})", answer.status)))
}

/// GitHub's `{"message"}` of an error answer.
fn message_of(answer: &Answer) -> Option<String> {
    serde_json::from_slice::<Value>(&answer.body)
        .ok()?
        .get("message")?
        .as_str()
        .map(str::to_owned)
}

/// An answer the call did not expect: `5xx` after the retry and the like.
fn unexpected(answer: &Answer) -> Error {
    let status = answer.status;
    upstream(&message_of(answer).map_or_else(
        || format!("HTTP {status}"),
        |message| format!("HTTP {status}: {message}"),
    ))
}

/// `401` of the REST API: the stored credential itself is refused
/// (revoked or expired).
fn rejected() -> Error {
    error(
        "forbidden",
        format!(
            "{HOST} rejected the stored token (revoked or expired): run `lof git connect github` again"
        ),
    )
}

/// One connection to GitHub: an account's token.
#[derive(Debug)]
pub(crate) struct Github<'a> {
    /// The plaintext credential of the connection.
    pub(crate) token: &'a str,
}

impl Github<'_> {
    /// A request to the REST API as the connection.
    fn api(&self, method: Method, segments: &[&str], body: Option<&Value>) -> Call<'static> {
        Call {
            method,
            authority: API,
            path: path(segments),
            accept: API_ACCEPT,
            authorization: Some(format!("Bearer {}", self.token)),
            body: body.map(|body| ("application/json", body.to_string().into_bytes())),
        }
    }

    /// `GET /user`: the login of the token's account, once its
    /// `X-OAuth-Scopes` name what the module needs (GIT-02).
    pub(crate) fn login(&self) -> Result<String, Error> {
        let answer = send(&self.api(Method::Get, &["user"], None), Retry::Once)?;
        match answer.status {
            200 => {
                let missing = missing_scopes(answer.scopes.as_deref());
                if !missing.is_empty() {
                    return Err(error(
                        "forbidden",
                        format!(
                            "the token lacks the scope {}; liftoff needs a token with the scopes repo and read:packages",
                            missing.join(" and ")
                        ),
                    ));
                }
                json_of(&answer)?
                    .get("login")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| upstream("the user answer has no login"))
            }
            401 | 403 => Err(error("forbidden", format!("{HOST} rejected the token"))),
            _ => Err(unexpected(&answer)),
        }
    }

    /// `GET /repos/{owner}/{name}`: what the account may do on it (GIT-03).
    pub(crate) fn repo_role(&self, repo: &str) -> Result<&'static str, Error> {
        let (owner, name) = split_repo(repo)?;
        let answer = send(
            &self.api(Method::Get, &["repos", owner, name], None),
            Retry::Once,
        )?;
        match answer.status {
            200 => Ok(role(
                json_of(&answer)?.get("permissions").unwrap_or(&Value::Null),
            )),
            // GitHub hides a repository the account may not see.
            403 | 404 => Ok("none"),
            401 => Err(rejected()),
            _ => Err(unexpected(&answer)),
        }
    }

    /// Whether `login` can pull `ghcr.io/<owner>/<name>:<tag>` (GIT-04):
    /// a pull token of the image's repository exchanged at
    /// `ghcr.io/token` for the connection (Basic `login:token`, as `docker
    /// login ghcr.io` does), then `HEAD /v2/<owner>/<name>/manifests/<tag>`
    /// with it.
    pub(crate) fn image_exists(
        &self,
        login: &str,
        owner: &str,
        name: &str,
        tag: &str,
    ) -> Result<bool, Error> {
        let scope = format!("repository:{owner}/{name}:pull");
        let basic = STANDARD.encode(format!("{login}:{}", self.token));
        let exchange = Call {
            method: Method::Get,
            authority: GHCR,
            path: format!("/token?{}", form(&[("scope", &scope), ("service", GHCR)])),
            accept: "application/json",
            authorization: Some(format!("Basic {basic}")),
            body: None,
        };
        let answer = send(&exchange, Retry::Once)?;
        let pull = match answer.status {
            200 => json_of(&answer)?
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| upstream("the registry's token answer has no token"))?,
            401 => return Err(rejected()),
            // The registry will not hand the account a token for it.
            403 | 404 => return Ok(false),
            _ => return Err(unexpected(&answer)),
        };
        let manifest = Call {
            method: Method::Head,
            authority: GHCR,
            path: format!(
                "/v2/{}/{}/manifests/{}",
                encode(owner),
                encode(name),
                encode(tag)
            ),
            accept: "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json",
            authorization: Some(format!("Bearer {pull}")),
            body: None,
        };
        let answer = send(&manifest, Retry::Once)?;
        match answer.status {
            200..=299 => Ok(true),
            // Missing, or not the account's to read.
            401 | 403 | 404 => Ok(false),
            _ => Err(unexpected(&answer)),
        }
    }

    /// `GET /repos/{r}/actions/secrets/public-key`, then `PUT
    /// /repos/{r}/actions/secrets/{key}` `{"encrypted_value","key_id"}`:
    /// the value sealed for the repository's key (GIT-05).
    pub(crate) fn put_ci_secret(
        &self,
        login: &str,
        repo: &str,
        key: &str,
        value: &str,
    ) -> Result<(), Error> {
        let (owner, name) = split_repo(repo)?;
        let refused = || {
            error(
                "forbidden",
                format!("{login} may not manage the CI secrets of {repo}"),
            )
        };
        let answer = send(
            &self.api(
                Method::Get,
                &["repos", owner, name, "actions", "secrets", "public-key"],
                None,
            ),
            Retry::Once,
        )?;
        let public = match answer.status {
            200 => json_of(&answer)?,
            403 | 404 => return Err(refused()),
            401 => return Err(rejected()),
            _ => return Err(unexpected(&answer)),
        };
        let (Some(key_id), Some(public_key)) = (
            public.get("key_id").and_then(Value::as_str),
            public.get("key").and_then(Value::as_str),
        ) else {
            return Err(upstream("the public key answer has no key_id or key"));
        };
        let body = json!({ "encrypted_value": seal(public_key, value)?, "key_id": key_id });
        let answer = send(
            &self.api(
                Method::Put,
                &["repos", owner, name, "actions", "secrets", key],
                Some(&body),
            ),
            Retry::Once,
        )?;
        match answer.status {
            200..=299 => Ok(()),
            403 | 404 => Err(refused()),
            401 => Err(rejected()),
            422 => Err(error(
                "validation",
                format!(
                    "{HOST} refused the CI secret {key}: {}",
                    message_of(&answer).unwrap_or_default()
                ),
            )),
            _ => Err(unexpected(&answer)),
        }
    }

    /// `GET /repos/{r}/contents/liftoff.toml`: the file on the default
    /// branch, `None` when the repository has none (or the account does
    /// not see it) (GIT-07).
    pub(crate) fn manifest(&self, repo: &str) -> Result<Option<String>, Error> {
        let (owner, name) = split_repo(repo)?;
        let answer = send(
            &self.api(
                Method::Get,
                &["repos", owner, name, "contents", "liftoff.toml"],
                None,
            ),
            Retry::Once,
        )?;
        match answer.status {
            200 => {
                let file = json_of(&answer)?;
                let content = file
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| upstream("the contents answer has no content"))?;
                // GitHub wraps the base64 content in lines.
                let packed: String = content.split_whitespace().collect();
                let bytes = STANDARD
                    .decode(packed)
                    .map_err(|_bad| upstream("the content of liftoff.toml is not base64"))?;
                String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_bad| upstream("liftoff.toml is not UTF-8"))
            }
            403 | 404 => Ok(None),
            401 => Err(rejected()),
            _ => Err(unexpected(&answer)),
        }
    }

    /// `POST /repos/{template}/generate` `{"owner","name","private",
    /// "include_all":true}`: the `owner/name` of the repository created
    /// (GIT-06). `include_all` copies every branch of the template, not
    /// only its default one. Not retried (see [`Retry::Never`]).
    pub(crate) fn generate(
        &self,
        template: &str,
        owner: &str,
        name: &str,
        private: bool,
    ) -> Result<String, Error> {
        let (template_owner, template_name) = split_repo(template)?;
        let body = json!({
            "owner": owner,
            "name": name,
            "private": private,
            "include_all": true,
        });
        let answer = send(
            &self.api(
                Method::Post,
                &["repos", template_owner, template_name, "generate"],
                Some(&body),
            ),
            Retry::Never,
        )?;
        match answer.status {
            200..=299 => json_of(&answer)?
                .get("full_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| upstream("the generated repository has no full_name")),
            401 => Err(rejected()),
            403 => Err(error(
                "forbidden",
                format!("the connection may not create {owner}/{name} from {template}"),
            )),
            404 => Err(error("not_found", format!("template {template} not found"))),
            422 => {
                let message = message_of(&answer).unwrap_or_default();
                let exists = String::from_utf8_lossy(&answer.body).contains("already exists");
                Err(if exists {
                    error(
                        "conflict",
                        format!("{owner}/{name} already exists on {HOST}"),
                    )
                } else {
                    error(
                        "validation",
                        format!("{HOST} refused {owner}/{name}: {message}"),
                    )
                })
            }
            _ => Err(unexpected(&answer)),
        }
    }
}

/// The OAuth application `client_id` of the module's config, on
/// `github.com` (RFC 8628).
#[derive(Debug)]
pub(crate) struct DeviceFlow<'a> {
    /// The OAuth application.
    pub(crate) client_id: &'a str,
}

impl DeviceFlow<'_> {
    /// A form `POST` to `github.com`, the answer asked for as JSON.
    fn post(path: &str, params: &[(&str, &str)]) -> Result<Value, Error> {
        let call = Call {
            method: Method::Post,
            authority: HOST,
            path: path.to_owned(),
            accept: "application/json",
            authorization: None,
            body: Some((
                "application/x-www-form-urlencoded",
                form(params).into_bytes(),
            )),
        };
        let answer = send(&call, Retry::Once)?;
        match answer.status {
            200 => json_of(&answer),
            // GitHub answers an unknown `client_id` with `404`.
            404 => Err(error(
                "validation",
                format!("{HOST} does not know the OAuth application of the module's client_id"),
            )),
            _ => Err(unexpected(&answer)),
        }
    }

    /// `POST /login/device/code`: the codes of a new flow asking for
    /// [`SCOPES`].
    pub(crate) fn start(&self) -> Result<Value, Error> {
        let answer = Self::post(
            "/login/device/code",
            &[("client_id", self.client_id), ("scope", SCOPES)],
        )?;
        if let Some(code) = answer.get("error").and_then(Value::as_str) {
            return Err(oauth_error(code, &answer));
        }
        let text = |name: &str| {
            answer
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| upstream(&format!("the device code answer has no {name}")))
        };
        let number = |name: &str| {
            answer
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| upstream(&format!("the device code answer has no {name}")))
        };
        Ok(json!({
            "device_code": text("device_code")?,
            "user_code": text("user_code")?,
            "verification_uri": text("verification_uri")?,
            "interval": number("interval")?,
            "expires_in": number("expires_in")?,
        }))
    }

    /// `POST /login/oauth/access_token` once: `{"status":"pending"}`
    /// while the person has not confirmed, `{"status":"slow_down",
    /// "interval"}` when GitHub asks to poll less often (RFC 8628 3.5;
    /// `interval` is GitHub's new one, absent when it sent none), else
    /// `{"status":"granted","token"}`.
    pub(crate) fn poll(&self, device_code: &str) -> Result<Value, Error> {
        let answer = Self::post(
            "/login/oauth/access_token",
            &[
                ("client_id", self.client_id),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )?;
        if let Some(token) = answer.get("access_token").and_then(Value::as_str) {
            return Ok(json!({ "status": "granted", "token": token }));
        }
        match answer.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => Ok(json!({ "status": "pending" })),
            Some("slow_down") => Ok(answer.get("interval").and_then(Value::as_u64).map_or_else(
                || json!({ "status": "slow_down" }),
                |interval| json!({ "status": "slow_down", "interval": interval }),
            )),
            Some(code) => Err(oauth_error(code, &answer)),
            None => Err(upstream(
                "the token answer has neither access_token nor error",
            )),
        }
    }
}

/// The core's error for GitHub's OAuth error `code` (RFC 8628 3.5).
fn oauth_error(code: &str, answer: &Value) -> Error {
    let description = answer
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or(code);
    match code {
        "expired_token" => error(
            "gone",
            "the device code expired: run `lof git connect github` again",
        ),
        "access_denied" => error("forbidden", "the person denied the access at github.com"),
        "incorrect_device_code" => error("validation", "github.com does not know the device code"),
        "device_flow_disabled" | "incorrect_client_credentials" | "unsupported_grant_type" => {
            error(
                "validation",
                format!("the module's OAuth application cannot run the device flow: {description}"),
            )
        }
        _ => upstream(&format!("OAuth error {code}: {description}")),
    }
}
