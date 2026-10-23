//! The Forgejo API of one host as one connection (the earlier `ForgejoGit`
//! of `crates/remote`, over `wasi:http`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::bindings::liftoff::module::host::Error;
use crate::bindings::wasi::http::types::Method;
use crate::http::{self, Answer, encode};

/// How many times a request answered with `5xx` is sent again (GITF-04).
const RETRIES: u32 = 1;

/// A module error with `code`.
pub(crate) fn error(code: &str, message: impl Into<String>) -> Error {
    Error {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// `owner/name` as the core writes a repository, split in two.
pub(crate) fn split_repo(repo: &str) -> Result<(&str, &str), Error> {
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

/// One connection to the Forgejo API of `host` (`https://<host>/api/v1`).
#[derive(Debug)]
pub(crate) struct Forgejo<'a> {
    /// The forge host.
    pub(crate) host: &'a str,
    /// The plaintext credential of the connection.
    pub(crate) token: &'a str,
}

impl Forgejo<'_> {
    /// `GET /user`: the login of the token's account.
    pub(crate) fn login(&self) -> Result<String, Error> {
        let answer = self.call(&Method::Get, &["user"], None)?;
        match answer.status {
            200 => self
                .json(&answer)?
                .get("login")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| self.upstream("the user answer has no login")),
            401 | 403 => Err(error(
                "forbidden",
                format!("{} rejected the token", self.host),
            )),
            _ => Err(self.unexpected(&answer)),
        }
    }

    /// `GET /repos/{owner}/{name}`: what the account may do on it.
    pub(crate) fn repo_role(&self, repo: &str) -> Result<&'static str, Error> {
        let (owner, name) = split_repo(repo)?;
        let answer = self.call(&Method::Get, &["repos", owner, name], None)?;
        match answer.status {
            200 => Ok(role(
                self.json(&answer)?
                    .get("permissions")
                    .unwrap_or(&Value::Null),
            )),
            // Forgejo hides a repository the account may not see.
            403 | 404 => Ok("none"),
            401 => Err(self.rejected()),
            _ => Err(self.unexpected(&answer)),
        }
    }

    /// `GET /packages/{owner}/container/{name}/{tag}`: whether the account
    /// reads the image.
    pub(crate) fn image_exists(&self, owner: &str, name: &str, tag: &str) -> Result<bool, Error> {
        let answer = self.call(
            &Method::Get,
            &["packages", owner, "container", name, tag],
            None,
        )?;
        match answer.status {
            200..=299 => Ok(true),
            403 | 404 => Ok(false),
            401 => Err(self.rejected()),
            _ => Err(self.unexpected(&answer)),
        }
    }

    /// `PUT /repos/{owner}/{name}/actions/secrets/{key}` `{"data"}`.
    pub(crate) fn put_ci_secret(
        &self,
        login: &str,
        repo: &str,
        key: &str,
        value: &str,
    ) -> Result<(), Error> {
        let (owner, name) = split_repo(repo)?;
        let body = json!({ "data": value });
        let answer = self.call(
            &Method::Put,
            &["repos", owner, name, "actions", "secrets", key],
            Some(&body),
        )?;
        match answer.status {
            200..=299 => Ok(()),
            403 | 404 => Err(error(
                "forbidden",
                format!("{login} may not manage the CI secrets of {repo}"),
            )),
            401 => Err(self.rejected()),
            _ => Err(self.unexpected(&answer)),
        }
    }

    /// `GET /repos/{owner}/{name}/contents/liftoff.toml`: the file on the
    /// default branch, `None` when the repository has none (or the
    /// account does not see it).
    pub(crate) fn manifest(&self, repo: &str) -> Result<Option<String>, Error> {
        let (owner, name) = split_repo(repo)?;
        let answer = self.call(
            &Method::Get,
            &["repos", owner, name, "contents", "liftoff.toml"],
            None,
        )?;
        match answer.status {
            200 => {
                let file = self.json(&answer)?;
                let content = file
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| self.upstream("the contents answer has no content"))?;
                // Forgejo wraps long base64 content in lines.
                let packed: String = content.split_whitespace().collect();
                let bytes = STANDARD
                    .decode(packed)
                    .map_err(|_bad| self.upstream("the content of liftoff.toml is not base64"))?;
                String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_bad| self.upstream("liftoff.toml is not UTF-8"))
            }
            403 | 404 => Ok(None),
            401 => Err(self.rejected()),
            _ => Err(self.unexpected(&answer)),
        }
    }

    /// `POST /repos/{template}/generate` `{"owner","name","private",
    /// "git_content":true}`: the `owner/name` of the repository created.
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
            "git_content": true,
        });
        let answer = self.call(
            &Method::Post,
            &["repos", template_owner, template_name, "generate"],
            Some(&body),
        )?;
        match answer.status {
            200..=299 => self
                .json(&answer)?
                .get("full_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| self.upstream("the generated repository has no full_name")),
            401 => Err(self.rejected()),
            403 => Err(error(
                "forbidden",
                format!("the connection may not create {owner}/{name} from {template}"),
            )),
            404 => Err(error("not_found", format!("template {template} not found"))),
            409 => Err(error(
                "conflict",
                format!("{owner}/{name} already exists on {}", self.host),
            )),
            422 => Err(error(
                "validation",
                format!(
                    "{} refused {owner}/{name}: {}",
                    self.host,
                    message_of(&answer).unwrap_or_default()
                ),
            )),
            _ => Err(self.unexpected(&answer)),
        }
    }

    /// Sends one request below `/api/v1`, `segments` percent-encoded, and
    /// again up to [`RETRIES`] times while it answers `5xx` if `method`
    /// is idempotent (`GET`, `PUT`). A `POST` is sent once: a `5xx` may
    /// come after Forgejo already created the repository, and a second
    /// `POST` would report it as a conflict. A request that never got an
    /// answer is not retried.
    fn call(
        &self,
        method: &Method,
        segments: &[&str],
        body: Option<&Value>,
    ) -> Result<Answer, Error> {
        let path = segments
            .iter()
            .fold(String::from("/api/v1"), |mut path, segment| {
                path.push('/');
                path.push_str(&encode(segment));
                path
            });
        let body = body.map(|body| body.to_string().into_bytes());
        let idempotent = matches!(method, Method::Get | Method::Put);
        let mut retries_left = if idempotent { RETRIES } else { 0 };
        loop {
            let answer = http::send(method, self.host, &path, self.token, body.as_deref())
                .map_err(|why| self.upstream(&why))?;
            if answer.status >= 500 && retries_left > 0 {
                retries_left = retries_left.saturating_sub(1);
                continue;
            }
            return Ok(answer);
        }
    }

    /// `401`: the stored credential itself is refused (revoked or
    /// expired), GITF-05.
    fn rejected(&self) -> Error {
        let host = self.host;
        error(
            "forbidden",
            format!(
                "{host} rejected the stored token (revoked or expired): run `lof git connect forgejo --host {host}` again"
            ),
        )
    }

    /// The JSON body of `answer`.
    fn json(&self, answer: &Answer) -> Result<Value, Error> {
        serde_json::from_slice(&answer.body).map_err(|_malformed| {
            self.upstream(&format!("malformed answer (HTTP {})", answer.status))
        })
    }

    /// The forge failed: the core's `Upstream` (GITF-04).
    fn upstream(&self, why: &str) -> Error {
        error("upstream", format!("{}: {why}", self.host))
    }

    /// An answer the call did not expect: `5xx` after the retry and the
    /// like.
    fn unexpected(&self, answer: &Answer) -> Error {
        let status = answer.status;
        self.upstream(&message_of(answer).map_or_else(
            || format!("HTTP {status}"),
            |message| format!("HTTP {status}: {message}"),
        ))
    }
}

/// Forgejo's `{"message"}` of an error answer.
fn message_of(answer: &Answer) -> Option<String> {
    serde_json::from_slice::<Value>(&answer.body)
        .ok()?
        .get("message")?
        .as_str()
        .map(str::to_owned)
}
