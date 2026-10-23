//! A fake module registry on loopback: `index.json` and the
//! built components of `modules/`, signed with the key of the core's
//! `FakeModuleSource` (a fresh minisign key per registry), which the CLI
//! of the test pins through `Local::new`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use liftoff_core::fakes::{CallLog, FakeModuleSource, FakeRelease};

/// Where `FakeRelease::url` says a component is.
const FAKE_BASE: &str = "https://registry.test/";

/// The registry.
#[derive(Debug)]
pub(crate) struct FakeRegistry {
    /// The URL of its `index.json`.
    pub(crate) url: String,
    /// The base64 line of the key its releases are signed with.
    pub(crate) key: String,
}

#[derive(Debug)]
struct Served {
    index: String,
    files: BTreeMap<String, Vec<u8>>,
}

impl FakeRegistry {
    /// A registry serving, for each `(module, component)`, the built
    /// component `component` of `modules/` as `module` 0.1.0.
    pub(crate) async fn serving(modules: &[(&str, &str)]) -> Self {
        let source = FakeModuleSource::with_log(CallLog::default());
        let mut files = BTreeMap::new();
        for (module, component) in modules {
            let bytes = liftoff_wasm::testing::component(component).unwrap();
            let manifest = serde_json::json!({"name": module, "version": "0.1.0"}).to_string();
            let release = FakeRelease::new(&manifest, bytes.clone()).unwrap();
            let file = release.url().strip_prefix(FAKE_BASE).unwrap().to_owned();
            let _none = files.insert(file, bytes);
            source.publish(release).unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = Arc::new(Served {
            index: source
                .index_json()
                .replace(FAKE_BASE, &format!("{base}/files/")),
            files,
        });
        #[expect(
            clippy::literal_string_with_formatting_args,
            reason = "an axum path parameter"
        )]
        let app = Router::new()
            .route("/index.json", get(index))
            .route("/files/{file}", get(file))
            .with_state(served);
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        Self {
            url: format!("{base}/index.json"),
            key: source.public_key(),
        }
    }
}

async fn index(State(served): State<Arc<Served>>) -> String {
    served.index.clone()
}

async fn file(State(served): State<Arc<Served>>, Path(file): Path<String>) -> Response {
    served.files.get(&file).map_or_else(
        || {
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap()
        },
        |bytes| Response::new(Body::from(bytes.clone())),
    )
}
