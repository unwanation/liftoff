//! Request extractors whose rejections are problem documents.

use axum::Json;
use axum::extract::multipart::{Field, MultipartError};
use axum::extract::{FromRequest, FromRequestParts, Multipart, Query, Request};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::request::Parts;
use http::{StatusCode, header};
use liftoff_api::{ArtifactBody, DeployBody};
use liftoff_core::domain::{ArtifactPath, MAX_COMPONENT_BYTES};
use liftoff_core::error::{Error, Problem, ValidationError};
use liftoff_core::service::{ArtifactContent, ArtifactUpload};
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// `Json<T>` that rejects with a problem document, `400` for a body that
/// does not fit `T`.
#[derive(Debug)]
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        Ok(Self(Json::<T>::from_request(req, state).await?.0))
    }
}

/// Like [`ApiJson`], but a request without a body is `T::default()`.
#[derive(Debug)]
pub(crate) struct ApiJsonOrDefault<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ApiJsonOrDefault<T>
where
    T: DeserializeOwned + Default,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|rejection| ApiError::new(rejection.status(), Some(rejection.body_text())))?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self(T::default()));
        }
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|error| ApiError::bad_request(error.to_string()))
    }
}

/// `Query<T>` that rejects with a problem document.
#[derive(Debug)]
pub(crate) struct ApiQuery<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Query(value) = Query::<T>::from_request_parts(parts, state).await?;
        Ok(Self(value))
    }
}

/// How much of `POST /deploys` the extractor reads: the
/// component of a function at most [`MAX_COMPONENT_BYTES`], everything
/// else together at most `max_body_bytes`, like any other body. The deploy
/// route carries it as an extension.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeployLimits {
    /// The JSON body, or every multipart field but `component` together.
    pub(crate) text: usize,
}

impl DeployLimits {
    /// The most bytes the deploy route's body may have on the wire: the
    /// text, the component and the multipart framing around them (inside
    /// `text`'s share).
    pub(crate) const fn body(self) -> usize {
        self.text.saturating_add(MAX_COMPONENT_BYTES)
    }
}

/// The body of `POST /deploys`: JSON, or the `multipart/form-data` that
/// `curl -F` sends from CI (the snippet). Both fill the same fields and
/// reject unknown ones; a multipart body may carry the binary field
/// `component` of a function deploy, at most
/// [`MAX_COMPONENT_BYTES`] (larger: `413`, FDEP-01).
#[derive(Debug)]
pub(crate) struct DeployForm {
    pub(crate) body: DeployBody,
    pub(crate) component: Option<Vec<u8>>,
}

/// `413` as a problem document.
fn too_large(what: &str) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        Some(format!("{what} is too large")),
    )
}

impl<S: Send + Sync> FromRequest<S> for DeployForm {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let limits = req
            .extensions()
            .get::<DeployLimits>()
            .copied()
            .ok_or_else(|| {
                tracing::error!("the deploy route has no DeployLimits");
                ApiError::internal()
            })?;
        let is_form = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("multipart/form-data"));
        if !is_form {
            // The route takes a component's worth of bytes; JSON gets the
            // limit of every other body.
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, limits.text)
                .await
                .map_err(|_too_long| too_large("the request body"))?;
            let req = Request::from_parts(parts, axum::body::Body::from(bytes));
            return Ok(Self {
                body: ApiJson::<DeployBody>::from_request(req, state).await?.0,
                component: None,
            });
        }
        let mut multipart = Multipart::from_request(req, state)
            .await
            .map_err(|rejection| ApiError::new(rejection.status(), Some(rejection.body_text())))?;
        let mut body = DeployBody::default();
        let mut component = None;
        let mut text_left = limits.text;
        while let Some(mut field) = multipart
            .next_field()
            .await
            .map_err(|error| form_error(&error))?
        {
            let name = field.name().unwrap_or_default().to_owned();
            if name == "component" {
                component =
                    Some(read_field(&mut field, MAX_COMPONENT_BYTES, "the component").await?);
                continue;
            }
            let slot = match name.as_str() {
                "repo" => &mut body.repo,
                "ref" => &mut body.git_ref,
                "tag" => &mut body.tag,
                "compose" => &mut body.compose,
                "env" => &mut body.env,
                "project" => &mut body.project,
                "environment" => &mut body.environment,
                other => {
                    return Err(ApiError::bad_request(format!("unknown field `{other}`")));
                }
            };
            let bytes = read_field(&mut field, text_left, "the request body").await?;
            text_left = text_left.saturating_sub(bytes.len());
            let text = String::from_utf8(bytes).map_err(|_not_utf8| {
                ApiError::bad_request(format!("field `{name}` is not UTF-8"))
            })?;
            *slot = Some(text);
        }
        Ok(Self { body, component })
    }
}

/// The bytes of `field`, refused with `413` (naming `what`) past `limit`
/// before more of it is read.
async fn read_field(field: &mut Field<'_>, limit: usize, what: &str) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|error| form_error(&error))? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(too_large(what));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The room a body of `POST /artifacts` and `PUT /artifacts/{id}` gets
/// for its framing on top of a version: the multipart headers of every
/// file, the JSON keys around them.
const ARTIFACT_HEADROOM: usize = 256 * 1024;

/// How much of a version's body the artifact routes read (amendment of): a version is at most `[artifacts]
/// max_size_mb`, its multipart body that plus [`ARTIFACT_HEADROOM`], its
/// JSON body four thirds of it (base64) plus the same. The routes carry
/// it as an extension; a version above the limit within its body is the
/// service's `400` on `files` (ART-03), a body above its limit `413`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArtifactLimits {
    /// `[artifacts] max_size_mb` in bytes.
    pub(crate) version: usize,
}

impl ArtifactLimits {
    /// The most bytes of a multipart body.
    pub(crate) const fn multipart(self) -> usize {
        self.version.saturating_add(ARTIFACT_HEADROOM)
    }

    /// The most bytes of a JSON body: base64 takes four bytes for three.
    pub(crate) const fn json(self) -> usize {
        self.version
            .div_ceil(3)
            .saturating_mul(4)
            .saturating_add(ARTIFACT_HEADROOM)
    }

    /// The most bytes of either body on the wire.
    pub(crate) const fn body(self) -> usize {
        let (multipart, json) = (self.multipart(), self.json());
        if multipart > json { multipart } else { json }
    }
}

/// A version of an artifact, `POST /artifacts` and `PUT /artifacts/{id}`: `multipart/form-data` (`html`, a `files` field per
/// file named by its `filename`, `title`; another field is `400`) or the
/// JSON [`ArtifactBody`] with the files in base64. Every name is an
/// [`ArtifactPath`] here, at the boundary (ART-02).
#[derive(Debug)]
pub(crate) struct ArtifactForm(pub(crate) ArtifactContent);

impl<S: Send + Sync> FromRequest<S> for ArtifactForm {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let limits = req
            .extensions()
            .get::<ArtifactLimits>()
            .copied()
            .ok_or_else(|| {
                tracing::error!("the artifact routes have no ArtifactLimits");
                ApiError::internal()
            })?;
        let is_form = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("multipart/form-data"));
        let limit = if is_form {
            limits.multipart()
        } else {
            limits.json()
        };
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, limit)
            .await
            .map_err(|_too_long| too_large("the request body"))?;
        let req = Request::from_parts(parts, axum::body::Body::from(bytes));
        if !is_form {
            let body = ApiJson::<ArtifactBody>::from_request(req, state).await?.0;
            return Ok(Self(json_content(body)?));
        }
        let mut multipart = Multipart::from_request(req, state)
            .await
            .map_err(|rejection| ApiError::new(rejection.status(), Some(rejection.body_text())))?;
        let mut html = None;
        let mut content = ArtifactContent::default();
        while let Some(mut field) = multipart
            .next_field()
            .await
            .map_err(|error| form_error(&error))?
        {
            let name = field.name().unwrap_or_default().to_owned();
            let filename = field.file_name().map(str::to_owned);
            let bytes = read_field(&mut field, limit, "the request body").await?;
            match name.as_str() {
                "html" => html = Some(bytes),
                "title" => {
                    let title = String::from_utf8(bytes).map_err(|_not_utf8| {
                        ApiError::from(Error::invalid("title", "is not UTF-8"))
                    })?;
                    content.title = Some(title);
                }
                "files" => {
                    let filename = filename.ok_or_else(|| {
                        ApiError::from(Error::invalid(
                            "files",
                            "a `files` field names its file by its `filename`",
                        ))
                    })?;
                    content.files.push(ArtifactUpload {
                        name: ArtifactPath::new(&filename)?,
                        bytes,
                    });
                }
                other => {
                    return Err(ApiError::bad_request(format!("unknown field `{other}`")));
                }
            }
        }
        content.html = html.ok_or_else(|| {
            ApiError::from(Error::invalid(
                "html",
                "is required: the page, as the field `html`",
            ))
        })?;
        Ok(Self(content))
    }
}

/// The version of the JSON form: each name checked, each content decoded
/// from base64.
fn json_content(body: ArtifactBody) -> Result<ArtifactContent, ApiError> {
    let mut files = Vec::with_capacity(body.files.len());
    for (name, encoded) in body.files {
        let path = ArtifactPath::new(&name)?;
        let bytes = STANDARD.decode(encoded.as_bytes()).map_err(|_not_base64| {
            ValidationError::at(
                "files",
                name.as_str(),
                Problem::Invalid("is not base64".to_owned()),
            )
        })?;
        files.push(ArtifactUpload { name: path, bytes });
    }
    Ok(ArtifactContent {
        html: body.html.into_bytes(),
        files,
        title: body.title,
    })
}

fn form_error(error: &MultipartError) -> ApiError {
    let status = error.status();
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        return too_large("the request body");
    }
    ApiError::bad_request(error.body_text())
}
