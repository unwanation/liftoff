//! Login: a single-use token traded for a session, or the
//! device flow of the `oidc` module.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::users::UserView;

/// Body of `POST /auth/token`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenExchange {
    /// The single-use token from `liftoffd setup` or `lof admin token`
    /// (`lft_` + 40 base62 characters).
    pub token: String,
    /// Name of the machine, shown with the session. Defaults to `cli`.
    #[serde(default)]
    pub name: Option<String>,
}

/// `200` body of `POST /auth/token`: the session token, shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SessionGranted {
    /// Bearer token for later requests (`lfs_` + 40 base62 characters).
    pub token: String,
    /// Who logged in.
    pub user: UserView,
}

/// `201` body of `POST /admin/token`: a new single-use token, shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SetupTokenGranted {
    /// The token for `lof login <url> --token` on another machine.
    pub token: String,
}

/// `200` body of `POST /m/oidc/device` (module `oidc`): a device login started at the provider (RFC 8628 3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct DeviceLogin {
    /// What the CLI polls with.
    pub device_code: String,
    /// The code the person confirms at the provider.
    pub user_code: String,
    /// The page to open, with the code filled in.
    pub verification_uri_complete: String,
    /// Seconds to wait between two polls.
    pub interval: u64,
    /// Seconds the device code stays valid.
    pub expires_in: u64,
}

/// Body of `POST /m/oidc/device/poll`: `202` `{"status":"pending"}`
/// until the person confirmed, then `200` [`SessionGranted`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DevicePoll {
    /// [`DeviceLogin::device_code`].
    pub device_code: String,
    /// Name of the machine, shown with the session.
    #[serde(default)]
    pub name: Option<String>,
}

/// Body of `POST /auth/links`: what the one-time
/// link opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkCreate {
    /// `environment:<id>` (an artifact's `artifact:<id>`).
    pub target: String,
}

/// `201` body of `POST /auth/links`: the link, shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct LinkIssued {
    /// `https://<api host>/api/v1/auth/links/<token>`: opens a `browser`
    /// session once, within ten minutes.
    pub url: String,
    /// RFC 3339: until when it can be used.
    pub expires_at: String,
}
