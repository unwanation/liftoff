//! [`CaddyAdmin`]: the `Proxy` port over Caddy's admin API.

use core::error::Error as StdError;
use core::net::SocketAddr;
use core::time::Duration;

use liftoff_core::ports::Proxy;
use liftoff_core::proxy_config::ProxyConfig;
use liftoff_core::{Error, System};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, RequestBuilder};

use crate::render;

/// Deadline of one admin API request, connection included.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Caddy's admin API at `http://<admin>` (loopback only).
#[derive(Debug, Clone)]
pub struct CaddyAdmin {
    admin: SocketAddr,
    client: Client,
}

impl CaddyAdmin {
    /// The admin API listening on `admin`, e.g. `127.0.0.1:2019`.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the HTTP client cannot be built.
    pub fn new(admin: SocketAddr) -> Result<Self, Error> {
        let client = Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(Error::internal)?;
        Ok(Self { admin, client })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.admin)
    }
}

impl Proxy for CaddyAdmin {
    /// `POST /load` with the [`render`]ing of `cfg` as `application/json`.
    /// Caddy keeps its previous configuration when it refuses the new one.
    ///
    /// # Errors
    ///
    /// [`Error::Upstream`] (system `caddy`) when the admin API cannot be
    /// reached or answers non-2xx; the detail is the `error` field of
    /// Caddy's JSON answer, or its whole body when that is not JSON.
    async fn apply(&self, cfg: ProxyConfig) -> Result<(), Error> {
        let body = serde_json::to_vec(&render(&cfg)).map_err(Error::internal)?;
        send(
            self.client
                .post(self.url("/load"))
                .header(CONTENT_TYPE, "application/json")
                .body(body),
        )
        .await
    }

    /// The readiness probe (`readyz`): `GET /config/` answers 2xx.
    ///
    /// # Errors
    ///
    /// [`Error::Upstream`] (system `caddy`) when the admin API cannot be
    /// reached or answers anything else.
    async fn ready(&self) -> Result<(), Error> {
        send(self.client.get(self.url("/config/"))).await
    }
}

/// Sends `request`; anything but a 2xx answer is [`Error::Upstream`].
async fn send(request: RequestBuilder) -> Result<(), Error> {
    let response = request.send().await.map_err(|e| unreachable(&e))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let text = response.text().await.map_err(|e| unreachable(&e))?;
    Err(Error::upstream(
        System::Caddy,
        refusal(status.as_u16(), &text),
    ))
}

/// A transport failure: the admin API did not answer. The cause (and the
/// loopback admin address in it) goes to the log; the caller gets a fixed
/// detail.
fn unreachable(error: &reqwest::Error) -> Error {
    tracing::error!(
        error = %error,
        source = ?StdError::source(error),
        "caddy admin api request failed"
    );
    let detail = if error.is_timeout() {
        "admin api timed out"
    } else {
        "admin api unreachable"
    };
    Error::upstream(System::Caddy, detail)
}

/// What Caddy said when it answered `status` with `body`: the `error` field
/// of its JSON answer, else the body, else the bare status.
fn refusal(status: u16, body: &str) -> String {
    let error = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned));
    match error {
        Some(text) => text,
        None if body.trim().is_empty() => format!("admin api answered {status}"),
        None => body.trim().to_owned(),
    }
}
