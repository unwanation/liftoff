//! The module's configuration: `issuer`, `client_id` (a
//! secret), `admin_role`, `scopes`; for the browser login (the gate's) the web client `web_client_id` and
//! `web_client_secret` (both secrets) and its `redirect_uri`.

use serde_json::{Value, json};

use crate::bindings::liftoff::module::host;
use crate::jwt::ROLES_CLAIM;

/// The admin role when `admin_role` is not set.
const DEFAULT_ADMIN_ROLE: &str = "admin";

/// The scopes when `scopes` is not set: the person, the address and
/// Zitadel's project roles (v1).
const DEFAULT_SCOPES: [&str; 4] = ["openid", "profile", "email", ROLES_CLAIM];

/// The configuration as the module uses it.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// The provider's issuer URL, exactly as its ID tokens name it.
    pub(crate) issuer: String,
    /// The OAuth client of the flow: the CLI's (Zitadel's native app),
    /// or, in [`Config::for_browser`], the web client's.
    pub(crate) client_id: String,
    /// The secret of the client, sent with the code exchange
    /// (`client_secret_post`): the web client's, when one is set.
    pub(crate) client_secret: Option<String>,
    /// The web client of the browser login (`liftoff gate` at the
    /// provider), when one is set; else the browser login uses
    /// `client_id` with PKCE alone.
    web_client: Option<(String, Option<String>)>,
    /// The role of the roles claim that makes a liftoff admin.
    pub(crate) admin_role: String,
    /// The scopes asked for.
    pub(crate) scopes: Vec<String>,
    /// Where the provider sends the browser back after `GET /login`
    /// (`https://<api>/api/v1/m/oidc/callback`, registered at the
    /// provider): the admin's `redirect_uri`, else the one of the API
    /// host the core names (`core-call system.api_url`, from `[api]
    /// public_url`); never taken from a request's `Host` or
    /// `X-Forwarded-*`, which the client controls.
    pub(crate) redirect_uri: Option<String>,
}

impl Config {
    /// The configuration of this installation; `None` until `issuer` and
    /// `client_id` are set.
    pub(crate) fn read() -> Option<Self> {
        let issuer = host::config_get("issuer").filter(|value| !value.is_empty())?;
        let client_id = host::config_get("client_id").filter(|value| !value.is_empty())?;
        let admin_role = host::config_get("admin_role")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_ADMIN_ROLE.to_owned());
        let scopes = host::config_get("scopes")
            .map(|raw| scopes(&raw))
            .filter(|scopes| !scopes.is_empty())
            .unwrap_or_else(|| DEFAULT_SCOPES.map(str::to_owned).to_vec());
        let redirect_uri = host::config_get("redirect_uri")
            .filter(|value| crate::http::Url::parse(value).is_ok())
            .or_else(api_callback);
        let web_client = host::config_get("web_client_id")
            .filter(|value| !value.is_empty())
            .map(|id| {
                let secret =
                    host::config_get("web_client_secret").filter(|value| !value.is_empty());
                (id, secret)
            });
        Some(Self {
            issuer,
            client_id,
            client_secret: None,
            web_client,
            admin_role,
            scopes,
            redirect_uri,
        })
    }

    /// The configuration of the browser login: the web client and its
    /// secret when one is set, else the CLI's client.
    pub(crate) fn for_browser(&self) -> Self {
        let mut browser = self.clone();
        if let Some((id, secret)) = &self.web_client {
            browser.client_id.clone_from(id);
            browser.client_secret.clone_from(secret);
        }
        browser
    }

    /// The `scope` parameter: the scopes separated by spaces.
    pub(crate) fn scope(&self) -> String {
        self.scopes.join(" ")
    }

    /// Whether `roles` (the keys of the roles claim) make an admin.
    pub(crate) fn is_admin(&self, roles: &[String]) -> bool {
        roles.contains(&self.admin_role)
    }
}

/// `scopes` as `config-get` answers it: a JSON array of strings, or
/// (set by hand) words separated by spaces or commas.
fn scopes(raw: &str) -> Vec<String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => raw
            .split([' ', ','])
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect(),
    }
}

/// `<api url>/api/v1/m/oidc/callback`, the API host the core names; `None`
/// when it names none.
fn api_callback() -> Option<String> {
    let answer = host::core_call("system", "api_url", "{}").ok()?;
    let url = serde_json::from_str::<Value>(&answer)
        .ok()?
        .get("url")?
        .as_str()?
        .trim_end_matches('/')
        .to_owned();
    Some(format!("{url}{}/callback", crate::PREFIX))
}

/// `host[:port]` of the issuer: the one host the module talks to.
pub(crate) fn issuer_authority(issuer: &str) -> Option<String> {
    crate::http::Url::parse(issuer)
        .ok()
        .map(|url| url.authority)
}

/// The JSON Schema of the configuration (REG-05: `client_id` sealed).
pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "required": ["issuer", "client_id"],
        "additionalProperties": false,
        "properties": {
            "issuer": {
                "type": "string",
                "description": "Issuer URL of the OpenID Connect provider, as its ID tokens name it",
            },
            "client_id": {
                "type": "string",
                "format": "secret",
                "description": "OAuth client ID of the liftoff CLI at the provider",
            },
            "admin_role": {
                "type": "string",
                "default": DEFAULT_ADMIN_ROLE,
                "description": "Role in the provider's roles claim that makes a liftoff admin",
            },
            "scopes": {
                "type": "array",
                "items": {"type": "string"},
                "default": DEFAULT_SCOPES,
                "description": "Scopes asked for at login",
            },
            "redirect_uri": {
                "type": "string",
                "pattern": "^https?://",
                "description": "Callback of the browser login registered at the provider; https://<api host>/api/v1/m/oidc/callback of [api] public_url by default",
            },
            "web_client_id": {
                "type": "string",
                "format": "secret",
                "description": "OAuth client ID of the browser login (the web application `liftoff gate` at the provider); client_id by default",
            },
            "web_client_secret": {
                "type": "string",
                "format": "secret",
                "description": "Secret of the web client, sent with the code exchange",
            },
        },
    })
}
