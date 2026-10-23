//! The caller's own identity (`GET /me`) and small acknowledgements.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A person known to liftoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct UserView {
    /// ULID.
    pub id: String,
    /// Login.
    pub login: String,
    /// E-mail.
    pub email: String,
    /// `admin` or `user`.
    pub role: String,
    /// `local` or `oidc:<issuer>` (LOGN-06).
    pub source: String,
}

/// Answer of every successful `DELETE` that has something to say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct Deleted {
    /// What was deleted: a name, a login or a host.
    pub deleted: String,
}
