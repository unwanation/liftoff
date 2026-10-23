//! Artifacts.
//!
//! `POST /artifacts` and `PUT /artifacts/{id}` take a version in one of
//! two forms: `multipart/form-data` (the field `html`, a `files` field per
//! file whose `filename` is its relative path, an optional `title`; what
//! `lof artifact publish` sends) or the JSON [`ArtifactBody`] with the
//! files' bytes in base64 (what the MCP tools send). An artifact is named
//! by its 12-character base62 id.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// The JSON form of a version (`POST /artifacts`, `PUT /artifacts/{id}`):
/// the HTML and its files as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactBody {
    /// The page, served as `index.html` of the version.
    pub html: String,
    /// The title; without one, the HTML's `<title>`, else the previous
    /// version's, else `Untitled`.
    #[serde(default)]
    pub title: Option<String>,
    /// The files next to the page: relative path (`img/logo.png`) to the
    /// base64 of its bytes.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

/// An artifact: the answer of publish, update, rollback, share, unshare
/// and get, and a row of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ArtifactView {
    /// 12 characters of `[0-9A-Za-z]`.
    pub id: String,
    /// `https://<api host>/a/<id>`, or `https://<artifacts.host>/<id>`.
    pub url: String,
    /// The title of the current version.
    pub title: String,
    /// Whether anybody may open it (`false`: private, with a TTL).
    pub shared: bool,
    /// RFC 3339: when a private artifact is deleted; `null` while shared.
    pub expires_at: Option<String>,
    /// The number of the current version.
    pub version: u32,
    /// RFC 3339: when version 1 was published.
    pub created_at: String,
    /// RFC 3339: when it last changed.
    pub updated_at: String,
    /// For the publish of a private artifact only: a one-time link
    /// (`https://<api host>/api/v1/auth/links/<token>`) that opens a
    /// browser session of its owner within ten minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_url: Option<String>,
}

/// One version of an artifact (`GET /artifacts/{id}/versions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ArtifactVersionView {
    /// Its number, from 1.
    pub n: u32,
    /// Its title.
    pub title: String,
    /// Its bytes, the HTML included.
    pub size: u64,
    /// RFC 3339.
    pub created_at: String,
}

/// Body of `POST /artifacts/{id}/rollback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRollback {
    /// The version copied as the new current one.
    pub to: u32,
}

/// Query of `GET /artifacts`.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ArtifactsQuery {
    /// Every user's artifacts instead of the caller's (admins only).
    #[serde(default)]
    pub all: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod artifact_body {
        use super::*;

        // TC: ART-15 (the JSON form: files by name, their bytes in
        // base64; `title` and `files` may be left out)
        #[test]
        fn reads_html_alone_when_title_and_files_left_out() {
            let body: Result<ArtifactBody, _> = serde_json::from_str(r#"{"html":"<p>hi</p>"}"#);
            assert_eq!(
                body.ok(),
                Some(ArtifactBody {
                    html: "<p>hi</p>".to_owned(),
                    title: None,
                    files: BTreeMap::new(),
                })
            );
        }

        // TC: ART-15 (`deny_unknown_fields`)
        #[test]
        fn rejects_body_when_field_unknown() {
            let body: Result<ArtifactBody, _> =
                serde_json::from_str(r#"{"html":"<p>hi</p>","file":{}}"#);
            assert!(body.is_err(), "{body:?}");
        }
    }

    mod artifact_view {
        use super::*;

        // TC: ART-15 (`login_url` only where there is one; `expires_at`
        // is `null` for a shared artifact, as the standalone artifacts server answers)
        #[test]
        fn leaves_out_login_url_and_keeps_null_expiry_when_shared() {
            let view = ArtifactView {
                id: "aB3dE6gH9jK2".to_owned(),
                url: "https://liftoff.test/a/aB3dE6gH9jK2".to_owned(),
                title: "Report".to_owned(),
                shared: true,
                expires_at: None,
                version: 2,
                created_at: "2026-10-13T00:00:00Z".to_owned(),
                updated_at: "2026-10-13T01:00:00Z".to_owned(),
                login_url: None,
            };
            let json = serde_json::to_value(&view).unwrap();
            assert_eq!(
                (
                    json.get("login_url").is_none(),
                    json.get("expires_at").cloned()
                ),
                (true, Some(serde_json::Value::Null))
            );
        }
    }
}
