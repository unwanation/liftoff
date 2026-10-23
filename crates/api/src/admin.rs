//! Administration.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query of `DELETE /admin/sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionsRevoke {
    /// Whose sessions end.
    pub login: String,
}
