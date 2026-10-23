use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use liftoff_core::{Error, ValidationError};
use serde::Serialize;

/// Media type for RFC 9457 problem details.
pub const PROBLEM_JSON: HeaderValue = HeaderValue::from_static("application/problem+json");

/// An error rendered as an RFC 9457 problem document.
///
/// Only information that is safe to show to an anonymous caller goes into
/// `detail`; internal causes are logged by the handler, never serialised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiError {
    #[serde(with = "http_serde_status")]
    status: StatusCode,
    title: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl ApiError {
    /// Builds a problem with the canonical reason phrase as title.
    #[must_use]
    pub fn new(status: StatusCode, detail: Option<String>) -> Self {
        Self {
            status,
            title: status.canonical_reason().unwrap_or("Error"),
            detail,
        }
    }

    /// `400 Bad Request` with a caller-safe explanation.
    #[must_use]
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some(detail.into()))
    }

    /// `404 Not Found`.
    #[must_use]
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, None)
    }

    /// `500 Internal Server Error` with no detail.
    #[must_use]
    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, None)
    }

    /// `503 Service Unavailable`.
    #[must_use]
    pub fn unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, None)
    }

    /// The HTTP status this problem renders with.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// The caller-safe explanation, if there is one.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// The one place where domain errors become HTTP.
///
/// Everything a caller may read is the error's `Display`, which the domain
/// keeps free of credentials and internals; the source of an internal error
/// goes to the log and nowhere else.
impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match &error {
            Error::Validation(_) => StatusCode::BAD_REQUEST,
            Error::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Gone(_) => StatusCode::GONE,
            Error::ModuleRequired(_) => StatusCode::FAILED_DEPENDENCY,
            Error::Upstream { .. } => StatusCode::BAD_GATEWAY,
            Error::Unavailable(_) | Error::Offline(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => {
                tracing::error!(
                    error = %error,
                    source = ?core::error::Error::source(&error).map(ToString::to_string),
                    "internal error"
                );
                return Self::internal();
            }
        };
        Self::new(status, Some(error.to_string()))
    }
}

impl From<ValidationError> for ApiError {
    fn from(error: ValidationError) -> Self {
        Self::from(Error::from(error))
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::bad_request(rejection.body_text())
    }
}

impl From<JsonRejection> for ApiError {
    /// A body that does not fit the input type (unknown field, wrong type,
    /// bad syntax) is a `400`, as every other validation failure.
    fn from(rejection: JsonRejection) -> Self {
        let status = if rejection.status() == StatusCode::UNPROCESSABLE_ENTITY {
            StatusCode::BAD_REQUEST
        } else {
            rejection.status()
        };
        Self::new(status, Some(rejection.body_text()))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let mut response = (status, Json(self)).into_response();
        let headers = response.headers_mut();
        let _previous: Option<HeaderValue> = headers.insert(header::CONTENT_TYPE, PROBLEM_JSON);
        if status == StatusCode::UNAUTHORIZED {
            let _previous: Option<HeaderValue> =
                headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

mod http_serde_status {
    use http::StatusCode;
    use serde::Serializer;

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "signature dictated by serde `with`"
    )]
    pub(super) fn serialize<S: Serializer>(status: &StatusCode, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u16(status.as_u16())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod from_core_error {
        use liftoff_core::System;
        use rstest::rstest;

        use super::*;

        // TC: v1 AUTH-05, v1 AUTH-08 (the whole table)
        #[rstest]
        #[case::validation(Error::invalid("slug", "bad"), 400)]
        #[case::unauthenticated(Error::unauthenticated("expired"), 401)]
        #[case::forbidden(Error::forbidden("no"), 403)]
        #[case::not_found(Error::not_found("project shop"), 404)]
        #[case::conflict(Error::conflict("taken"), 409)]
        #[case::gone(Error::gone("device code expired"), 410)]
        #[case::upstream(Error::upstream(System::Caddy, "boom"), 502)]
        #[case::unavailable(Error::unavailable("no free port"), 503)]
        #[case::internal(Error::internal_msg("db password is hunter2"), 500)]
        // TC: REST-10 (the v2 additions to the dictionary)
        #[case::module_required(Error::module_required("dns.cloudflare"), 424)]
        #[case::offline(Error::offline("no tunnel client for shop/dev"), 503)]
        fn status_matches_spec_when_mapped(#[case] error: Error, #[case] expected: u16) {
            assert_eq!(ApiError::from(error).status().as_u16(), expected);
        }

        #[test]
        fn shows_upstream_text_when_upstream() {
            let api = ApiError::from(Error::upstream(System::Caddy, "unrecognized directive"));
            assert_eq!(api.detail(), Some("caddy: unrecognized directive"));
        }

        // TC: REST-10 («install the module …» names the module)
        #[test]
        fn names_the_module_when_module_required() {
            let api = ApiError::from(Error::module_required("dns.cloudflare"));
            assert_eq!(
                (api.status().as_u16(), api.detail()),
                (424, Some("install the module `dns.cloudflare`"))
            );
        }

        // TC: REST-10 (`Offline` is a 503 that says why)
        #[test]
        fn says_why_when_offline() {
            let api = ApiError::from(Error::offline("no tunnel client for shop/dev"));
            assert_eq!(
                (api.status().as_u16(), api.detail()),
                (503, Some("no tunnel client for shop/dev"))
            );
        }

        #[test]
        fn hides_source_when_internal() {
            let api = ApiError::from(Error::internal_msg("db password is hunter2"));
            assert_eq!(api.detail(), None);
        }

        #[test]
        fn asks_for_bearer_when_unauthenticated() {
            let response = ApiError::from(Error::unauthenticated("expired")).into_response();
            assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
        }
    }

    mod api_error {
        use super::*;

        #[test]
        fn canonical_reason_when_title() {
            assert_eq!(
                ApiError::unavailable(),
                ApiError {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    title: "Service Unavailable",
                    detail: None,
                }
            );
        }

        #[test]
        fn no_detail_when_internal() {
            let json = serde_json::to_value(ApiError::internal()).unwrap();
            assert_eq!(
                json,
                serde_json::json!({"status": 500, "title": "Internal Server Error"})
            );
        }

        #[test]
        fn problem_document_when_serialised() {
            let json = serde_json::to_value(ApiError::bad_request("nope")).unwrap();
            assert_eq!(
                json,
                serde_json::json!({"status": 400, "title": "Bad Request", "detail": "nope"})
            );
        }
    }
}
