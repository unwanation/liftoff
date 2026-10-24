//! The HTTP client: bearer token, problem+json errors, network failures.

use std::future::Future;
use std::time::Duration;

use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::error::CliError;

/// How long a connection attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an ordinary request may take from start to the last byte. Longer
/// than the daemon's slowest route (150 s: start, stop, remove, reconcile),
/// so the daemon answers before the CLI gives up on work that still succeeds.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(3);

/// A successful answer: the body exactly as received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) body: String,
}

/// A query string as pairs, in order.
pub(crate) type Query<'a> = &'a [(&'a str, String)];

/// Talks to `<server>/api/v1`.
#[derive(Debug)]
pub(crate) struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    cancel: CancellationToken,
}

impl Client {
    /// A client for `server`, authenticated with `token` when there is one.
    /// Every request ends with [`CliError::Interrupted`] once `cancel` fires.
    pub(crate) fn new(
        server: &str,
        token: Option<String>,
        cancel: CancellationToken,
    ) -> Result<Self, CliError> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("liftoff-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| CliError::Local(format!("cannot start the HTTP client: {error}")))?;
        Ok(Self {
            http,
            base: format!("{}/api/v1", server.trim_end_matches('/')),
            token,
            cancel,
        })
    }

    fn request(&self, method: Method, path: &str, query: Query<'_>) -> RequestBuilder {
        let mut builder = self
            .http
            .request(method, format!("{}{path}", self.base))
            .timeout(REQUEST_TIMEOUT);
        if !query.is_empty() {
            builder = builder.query(query);
        }
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// `GET path?query`.
    pub(crate) async fn get(&self, path: &str, query: Query<'_>) -> Result<Reply, CliError> {
        self.exchange(self.request(Method::GET, path, query)).await
    }

    /// `POST path` with no body.
    pub(crate) async fn post(&self, path: &str) -> Result<Reply, CliError> {
        self.exchange(self.request(Method::POST, path, &[])).await
    }

    /// `method path?query` with a JSON body.
    pub(crate) async fn send_json(
        &self,
        method: Method,
        path: &str,
        query: Query<'_>,
        body: &(impl Serialize + Sync),
    ) -> Result<Reply, CliError> {
        self.exchange(self.request(method, path, query).json(body))
            .await
    }

    /// `method path` with a JSON body, whatever status it answers: the
    /// status and the body, for a caller that tells `2xx` answers apart
    /// or falls back on one (the `oidc` device flow). Only a network
    /// failure or Ctrl-C is an error; [`refusal`] turns an answer into
    /// one.
    pub(crate) async fn send_json_any(
        &self,
        method: Method,
        path: &str,
        body: &(impl Serialize + Sync),
    ) -> Result<(StatusCode, String), CliError> {
        self.any(self.request(method, path, &[]).json(body)).await
    }

    /// Sends the request and reads the whole answer, whatever its status.
    async fn any(&self, builder: RequestBuilder) -> Result<(StatusCode, String), CliError> {
        self.interruptible(async {
            let response = builder
                .send()
                .await
                .map_err(|error| CliError::Network(describe(&error)))?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|error| CliError::Network(describe(&error)))?;
            Ok((status, body))
        })
        .await
    }

    /// `method path` with `body` of `content_type` (the multipart upload
    /// of a function or of an artifact's version).
    pub(crate) async fn send_body(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        body: reqwest::Body,
    ) -> Result<Reply, CliError> {
        let builder = self
            .request(method, path, &[])
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body);
        self.exchange(builder).await
    }

    /// `DELETE path?query`.
    pub(crate) async fn delete(&self, path: &str, query: Query<'_>) -> Result<Reply, CliError> {
        self.exchange(self.request(Method::DELETE, path, query))
            .await
    }

    /// `GET path?query` whose body the caller reads as it arrives.
    pub(crate) async fn stream(&self, path: &str, query: Query<'_>) -> Result<Response, CliError> {
        // No overall timeout: the body may stay open for as long as it likes.
        let builder = self
            .request(Method::GET, path, query)
            .timeout(Duration::MAX);
        self.interruptible(send(builder)).await
    }

    /// Sends the request and reads the whole answer.
    async fn exchange(&self, builder: RequestBuilder) -> Result<Reply, CliError> {
        self.interruptible(async { read(send(builder).await?).await })
            .await
    }

    /// Runs `work` unless Ctrl-C comes first.
    async fn interruptible<T>(
        &self,
        work: impl Future<Output = Result<T, CliError>>,
    ) -> Result<T, CliError> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(CliError::Interrupted),
            result = work => result,
        }
    }
}

/// Sends the request; an error status becomes [`CliError::Api`] (401:
/// [`CliError::Unauthorized`]).
async fn send(builder: RequestBuilder) -> Result<Response, CliError> {
    let response = builder
        .send()
        .await
        .map_err(|error| CliError::Network(describe(&error)))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(refusal(status, body))
}

/// The error of an answer with the error status `status`: its `detail`
/// (401: [`CliError::Unauthorized`]).
pub(crate) fn refusal(status: StatusCode, body: String) -> CliError {
    let message = problem_text(status, &body);
    if status == StatusCode::UNAUTHORIZED {
        return CliError::Unauthorized { message, body };
    }
    CliError::Api { message, body }
}

async fn read(response: Response) -> Result<Reply, CliError> {
    let body = response
        .text()
        .await
        .map_err(|error| CliError::Network(describe(&error)))?;
    Ok(Reply { body })
}

/// The text of a failure without the URL (it may carry nothing secret, but it
/// is noise next to "connection refused").
pub(crate) fn describe(error: &reqwest::Error) -> String {
    let mut causes = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    if causes.is_empty() {
        error.to_string()
    } else {
        causes.join(": ")
    }
}

/// `detail` of a problem document, else its `title`, else the status line.
fn problem_text(status: StatusCode, body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(name))
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    field("detail")
        .or_else(|| field("title"))
        .unwrap_or_else(|| status.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod request_timeout {
        use super::*;

        // TC: v1 CLI-05 (the daemon's slow routes answer within 150 s)
        #[test]
        fn outlasts_the_daemons_slow_routes_when_configured() {
            assert!(REQUEST_TIMEOUT > Duration::from_secs(150));
        }
    }
}
