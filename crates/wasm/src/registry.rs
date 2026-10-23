//! [`HttpModuleSource`]: the `ModuleSource` port over the module
//! registry's HTTPS.

use core::time::Duration;
use std::path::Path;

use liftoff_core::ports::ModuleSource;
use liftoff_core::{Error, System};
use serde::Deserialize;

/// The largest `index.json` or component the registry may answer.
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;

/// The module registry of `[modules] registry` over HTTPS.
///
/// `registry` is either the URL of `index.json` itself (it ends in
/// `.json`), or the releases API of the Forgejo repository that publishes
/// the modules (`…/api/v1/repos/<owner>/liftoff-modules/releases`): then `index.json` is the asset of that name of its latest release,
/// which `modules/registry/` attaches to every release it makes. Nothing
/// it answers is trusted: the core checks every component against the
/// pinned key.
#[derive(Debug, Clone)]
pub struct HttpModuleSource {
    url: String,
    http: reqwest::Client,
}

impl HttpModuleSource {
    /// The registry at `url`.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the HTTP client cannot be built.
    pub fn new(url: impl Into<String>) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("liftoffd/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(Error::internal)?;
        Ok(Self {
            url: url.into(),
            http,
        })
    }

    /// The body at `url`, at most [`MAX_DOWNLOAD_BYTES`].
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, Error> {
        let failed = |why: &dyn core::fmt::Display| {
            Error::upstream(System::Registry, format!("{url}: {why}"))
        };
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| failed(&error))?;
        if !response.status().is_success() {
            return Err(failed(&response.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| failed(&error))? {
            if body.len().saturating_add(chunk.len()) > MAX_DOWNLOAD_BYTES {
                return Err(failed(&format_args!(
                    "larger than {MAX_DOWNLOAD_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Where `index.json` is: the URL itself, or the asset of the latest
    /// release.
    async fn index_url(&self) -> Result<String, Error> {
        let is_json = Path::new(&self.url)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        if is_json {
            return Ok(self.url.clone());
        }
        let latest = format!("{}/latest", self.url.trim_end_matches('/'));
        let release: Release = serde_json::from_slice(&self.fetch(&latest).await?)
            .map_err(|error| Error::upstream(System::Registry, format!("{latest}: {error}")))?;
        release
            .assets
            .into_iter()
            .find(|asset| asset.name == "index.json")
            .map(|asset| asset.browser_download_url)
            .ok_or_else(|| {
                Error::upstream(
                    System::Registry,
                    format!("{latest}: the latest release has no index.json"),
                )
            })
    }
}

/// What the releases API answers that the source reads.
#[derive(Debug, Deserialize)]
struct Release {
    assets: Vec<Asset>,
}

/// An asset of a [`Release`].
#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl ModuleSource for HttpModuleSource {
    async fn index(&self) -> Result<String, Error> {
        let url = self.index_url().await?;
        String::from_utf8(self.fetch(&url).await?)
            .map_err(|_not_utf8| Error::upstream(System::Registry, format!("{url}: not UTF-8")))
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, Error> {
        self.fetch(url).await
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::extract::State;
    use axum::routing::get;

    use super::*;

    /// `/releases/latest` of a Forgejo-like registry: its assets, the
    /// `index.json` one served at `/files/index.json`.
    async fn latest(State(base): State<String>) -> String {
        format!(
            r#"{{"tag_name":"modules/probe/v0.1.0","assets":[{{"name":"probe-0.1.0.wasm","browser_download_url":"{base}/files/probe.wasm"}},{{"name":"index.json","browser_download_url":"{base}/files/index.json"}}]}}"#
        )
    }

    /// The registry's `index.json`.
    async fn index_json() -> &'static str {
        r#"{"modules":{}}"#
    }

    /// Serves the registry on loopback; its base URL.
    async fn registry() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/releases/latest", get(latest))
            .route("/files/index.json", get(index_json))
            .with_state(base.clone());
        drop(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        base
    }

    mod index {
        use super::*;

        // TC: REG-01 (the configured `registry` is the releases
        // API; the index is the asset of the latest release)
        #[tokio::test]
        async fn reads_asset_of_latest_release_when_registry_is_releases_api() {
            let base = registry().await;
            let source = HttpModuleSource::new(format!("{base}/releases")).unwrap();
            assert_eq!(
                source.index().await.ok(),
                Some(r#"{"modules":{}}"#.to_owned())
            );
        }

        // TC: REG-01 (a registry may name its index.json)
        #[tokio::test]
        async fn reads_url_itself_when_registry_is_json() {
            let base = registry().await;
            let source = HttpModuleSource::new(format!("{base}/files/index.json")).unwrap();
            assert_eq!(
                source.index().await.ok(),
                Some(r#"{"modules":{}}"#.to_owned())
            );
        }

        // TC: REG-01 (design: an error answer is the registry's failure)
        #[tokio::test]
        async fn upstream_of_registry_when_answer_is_not_found() {
            let base = registry().await;
            let source = HttpModuleSource::new(format!("{base}/files/missing.json")).unwrap();
            let refused = source.index().await;
            assert!(
                matches!(
                    &refused,
                    Err(Error::Upstream {
                        system: System::Registry,
                        ..
                    })
                ),
                "{refused:?}"
            );
        }
    }
}
