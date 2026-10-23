#![cfg(test)]
//! The contract of the function runtime in three languages (FNC-01…06): one test set runs the same
//! contract function written in Rust (`fn_rust`,
//! `modules/testing/functions/rust`), TypeScript (`fn_ts`,
//! `modules/testing/functions/ts`, componentize-js) and Python
//! (`fn_python`, `modules/testing/functions/python`, componentize-py)
//! through `liftoff_wasm::functions::FunctionRuntime`, under
//! the daemon's default `[functions]` limits.
//!
//! The contract (`modules/testing/functions/rust/src/lib.rs`):
//!
//! | Request | Answer |
//! |---|---|
//! | `GET /` | `200`, `application/json`: `{"method", "path", "greeting", "counter", "fetch"}` |
//! | `POST /echo` | `200` with the request body and its `content-type` |
//! | anything else | `404` |
//!
//! `greeting` is the `wasi:config` value `GREETING`, `counter` the
//! `wasi:keyvalue` counter `counter` after one more visitor, `fetch` the
//! body of a `GET` of the `wasi:config` value `FETCH_URL` (a fake host of
//! the test on loopback); the daemon's warm-up (`GET /` with
//! `X-Liftoff-Warmup`) neither counts nor fetches («»).
//!
//! The components are built by `just functions` (part of `just test` and
//! `just ci`); `liftoff_wasm::testing::component` reads them from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use liftoff_core::domain::{Env, EnvironmentId, SecretKey, SecretValue};
use liftoff_core::fakes::{InMemoryStore, World};
use liftoff_core::ports::Store as _;
use liftoff_wasm::functions::{FunctionLimits, FunctionRuntime};
use liftoff_wasm::testing::component;
use rstest::rstest;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

type Runtime = FunctionRuntime<InMemoryStore>;

/// How long a test waits for something that must happen.
const PATIENCE: Duration = Duration::from_secs(60);

/// What the fake host of the tests answers every `GET`.
const UPSTREAM_BODY: &str = "pong from the test host";

/// The daemon's default `[functions]` (`liftoffd`'s `FunctionsConfig`):
/// 128 MiB of memory, a 30 s timeout (for outgoing requests too), four
/// instances per environment, 64 MiB of keys and values.
const fn defaults() -> FunctionLimits {
    FunctionLimits {
        max_memory_bytes: 128 * 1024 * 1024,
        timeout: Duration::from_secs(30),
        instances_per_env: 4,
        kv_max_bytes: 64 * 1024 * 1024,
        outgoing_timeout: Duration::from_secs(30),
    }
}

/// A runtime over the world's store that may reach loopback, with
/// wasmtime's compile cache in the target directory, so the large TS and
/// Python components compile once per build rather than once per test.
fn runtime(world: &World) -> Runtime {
    FunctionRuntime::new(world.ports.store.clone(), defaults())
        .unwrap()
        .with_compile_cache(
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("contract-functions-cache"),
        )
        .unwrap()
        .allowing_private_networks()
}

/// `pairs` as an environment's resolved secrets.
fn env(pairs: &[(&str, &str)]) -> Env {
    Env::new(
        pairs
            .iter()
            .map(|(key, value)| {
                (
                    SecretKey::new(key).unwrap(),
                    SecretValue::new((*value).to_owned()),
                )
            })
            .collect(),
    )
}

/// Precompiles the built contract function `name` and activates it (its
/// warm-up included) for `environment` with `config`.
async fn activate(runtime: &Runtime, environment: EnvironmentId, name: &str, config: Env) {
    let compiled = tokio::time::timeout(PATIENCE, runtime.precompile(&component(name).unwrap()))
        .await
        .expect("the component compiles in time")
        .unwrap();
    tokio::time::timeout(PATIENCE, runtime.activate(environment, &compiled, config))
        .await
        .expect("the warm-up returns in time")
        .unwrap();
}

/// A runtime with `name` active in the world's `prod` with `config`.
async fn serving(world: &World, name: &str, config: &[(&str, &str)]) -> Runtime {
    let runtime = runtime(world);
    activate(&runtime, world.prod.id, name, env(config)).await;
    runtime
}

fn request(method: &str, target: &str, body: impl Into<Bytes>) -> http::Request<Bytes> {
    http::Request::builder()
        .method(method)
        .uri(target)
        .header("host", "shop.apps.test")
        .body(body.into())
        .unwrap()
}

fn get(target: &str) -> http::Request<Bytes> {
    request("GET", target, Bytes::new())
}

/// `POST target` with `body` of `content_type`.
fn post(target: &str, content_type: &str, body: impl Into<Bytes>) -> http::Request<Bytes> {
    let mut posted = request("POST", target, body);
    posted
        .headers_mut()
        .insert("content-type", content_type.parse().unwrap());
    posted
}

/// The function's answer to `request` in `environment`, at most
/// [`PATIENCE`].
async fn invoke(
    runtime: &Runtime,
    environment: EnvironmentId,
    request: http::Request<Bytes>,
) -> http::Response<Bytes> {
    tokio::time::timeout(PATIENCE, runtime.invoke(environment, request))
        .await
        .expect("the request returns in time")
        .unwrap()
}

/// The JSON of the function's answer to `GET target` in `environment`.
async fn root_json(runtime: &Runtime, environment: EnvironmentId, target: &str) -> Value {
    let response = invoke(runtime, environment, get(target)).await;
    assert_eq!(response.status().as_u16(), 200, "GET {target} answers 200");
    serde_json::from_slice(response.body()).unwrap()
}

/// The `content-type` of `response`.
fn content_type(response: &http::Response<Bytes>) -> Option<String> {
    response
        .headers()
        .get("content-type")
        .map(|value| value.to_str().unwrap().to_owned())
}

/// A loopback HTTP server, the "test's host" of the contract's `fetch`:
/// it records the request line of every request and answers each
/// `200` [`UPSTREAM_BODY`].
struct Upstream {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut head: Vec<u8> = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    while !head.windows(4).any(|end: &[u8]| end == b"\r\n\r\n") {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => head.extend(buffer.iter().take(read)),
                        }
                    }
                    let text = String::from_utf8_lossy(&head).into_owned();
                    let line = text.lines().next().unwrap_or_default().to_owned();
                    recorded
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(line);
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{UPSTREAM_BODY}",
                        UPSTREAM_BODY.len()
                    );
                    let _sent = stream.write_all(answer.as_bytes()).await;
                });
            }
        });
        Self { port, seen }
    }

    /// `http://127.0.0.1:<port><path>`.
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// The request lines it read so far.
    fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

mod get_root {
    use super::*;

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_200_with_json_when_get_root(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-01
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let response = invoke(&runtime, world.prod.id, get("/")).await;

        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(content_type(&response).as_deref(), Some("application/json"));
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_exactly_contract_fields_when_get_root(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-01
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        let mut fields: Vec<&str> = json
            .as_object()
            .expect("the answer is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(fields, ["counter", "fetch", "greeting", "method", "path"]);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn reports_get_and_root_path_when_get_root(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-01
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(
            (&json["method"], &json["path"]),
            (&json!("GET"), &json!("/"))
        );
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn reports_path_with_query_when_get_root_has_query(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-01
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let json = root_json(&runtime, world.prod.id, "/?lang=en&page=2").await;

        assert_eq!(json["path"], "/?lang=en&page=2");
    }
}

mod echo {
    use super::*;

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_body_when_post_echo(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-02
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let response = invoke(
            &runtime,
            world.prod.id,
            post("/echo", "text/plain", "hello, echo"),
        )
        .await;

        assert_eq!(
            (response.status().as_u16(), response.body().as_ref()),
            (200, b"hello, echo".as_slice())
        );
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_request_content_type_when_post_echo(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-02
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let response = invoke(
            &runtime,
            world.prod.id,
            post("/echo", "application/json", r#"{"ping":1}"#),
        )
        .await;

        assert_eq!(content_type(&response).as_deref(), Some("application/json"));
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_binary_body_unchanged_when_post_echo(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-02
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;
        let bytes: Vec<u8> = (0..=255_u8).cycle().take(200_000).collect();

        let response = invoke(
            &runtime,
            world.prod.id,
            post("/echo", "application/octet-stream", bytes.clone()),
        )
        .await;

        // Not `assert_eq!`: 200 000 bytes would flood the failure.
        assert!(
            response.body().as_ref() == bytes.as_slice(),
            "the echoed body differs: {} bytes, status {}",
            response.body().len(),
            response.status()
        );
    }
}

mod config {
    use super::*;

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_greeting_when_config_has_greeting(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-03
        let world = World::new().await;
        let runtime = serving(&world, function, &[("GREETING", "Hello from liftoff")]).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(json["greeting"], "Hello from liftoff");
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_null_greeting_when_config_lacks_greeting(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-03
        let world = World::new().await;
        let runtime = serving(&world, function, &[("OTHER", "value")]).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(json["greeting"], Value::Null);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_each_environments_greeting_when_two_run_function(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-03
        let world = World::new().await;
        let runtime = runtime(&world);
        activate(
            &runtime,
            world.prod.id,
            function,
            env(&[("GREETING", "Hello, prod")]),
        )
        .await;
        activate(
            &runtime,
            world.qa.id,
            function,
            env(&[("GREETING", "Hello, qa")]),
        )
        .await;

        let json = root_json(&runtime, world.qa.id, "/").await;

        assert_eq!(json["greeting"], "Hello, qa");
    }
}

mod keyvalue {
    use super::*;

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn counts_one_when_first_visitor_after_warmup(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-04
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(json["counter"], 1);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn increments_counter_when_get_root_again(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-04
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;
        root_json(&runtime, world.prod.id, "/").await;
        root_json(&runtime, world.prod.id, "/").await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(json["counter"], 3);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_counter_as_decimal_text_in_kv_when_get_root(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-04
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;
        root_json(&runtime, world.prod.id, "/").await;
        root_json(&runtime, world.prod.id, "/").await;

        let stored = world
            .ports
            .store
            .kv_get(world.prod.id, "counter")
            .await
            .unwrap();

        assert_eq!(stored, Some(b"2".to_vec()));
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn counts_per_environment_when_two_run_function(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-04
        let world = World::new().await;
        let runtime = runtime(&world);
        activate(&runtime, world.prod.id, function, env(&[])).await;
        activate(&runtime, world.qa.id, function, env(&[])).await;
        root_json(&runtime, world.prod.id, "/").await;
        root_json(&runtime, world.prod.id, "/").await;

        let json = root_json(&runtime, world.qa.id, "/").await;

        assert_eq!(json["counter"], 1);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn leaves_counter_unset_when_only_warmed_up(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-04
        let world = World::new().await;
        let _runtime = serving(&world, function, &[]).await;

        let stored = world
            .ports
            .store
            .kv_get(world.prod.id, "counter")
            .await
            .unwrap();

        assert_eq!(stored, None);
    }
}

mod fetch {
    use super::*;

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_upstream_body_when_config_has_fetch_url(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-05
        let world = World::new().await;
        let upstream = Upstream::start().await;
        let fetch_url = upstream.url("/hello");
        let runtime = serving(&world, function, &[("FETCH_URL", &fetch_url)]).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(json["fetch"], "pong from the test host");
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn requests_get_of_fetch_url_path_when_get_root(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-05
        let world = World::new().await;
        let upstream = Upstream::start().await;
        let fetch_url = upstream.url("/hello?from=function");
        let runtime = serving(&world, function, &[("FETCH_URL", &fetch_url)]).await;

        root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(upstream.seen(), ["GET /hello?from=function HTTP/1.1"]);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_null_fetch_when_config_lacks_fetch_url(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-05
        let world = World::new().await;
        let runtime = serving(&world, function, &[("GREETING", "hi")]).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(json["fetch"], Value::Null);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_error_text_when_fetch_url_refuses_connections(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-05
        let world = World::new().await;
        // A port nothing listens on: bound, then closed.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let fetch_url = format!("http://127.0.0.1:{port}/");
        let runtime = serving(&world, function, &[("FETCH_URL", &fetch_url)]).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        // The same text in every language: the WIT case of
        // `wasi:http/types.error-code` in UpperCamelCase, no prefix.
        assert_eq!(json["fetch"], "error: ConnectionRefused");
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn fetches_nothing_when_only_warmed_up(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-05
        let world = World::new().await;
        let upstream = Upstream::start().await;
        let fetch_url = upstream.url("/hello");
        let runtime = serving(&world, function, &[("FETCH_URL", &fetch_url)]).await;
        // A request after the warm-up proves the function is up; the
        // upstream sees exactly that one fetch.
        root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(upstream.seen().len(), 1);
    }
}

mod unknown_path {
    use super::*;

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_404_when_path_is_unknown(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-06
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let response = invoke(&runtime, world.prod.id, get("/no-such-page")).await;

        assert_eq!(response.status().as_u16(), 404);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_404_when_get_echo(#[values("fn_rust", "fn_ts", "fn_python")] function: &str) {
        // TC: FNC-06
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let response = invoke(&runtime, world.prod.id, get("/echo")).await;

        assert_eq!(response.status().as_u16(), 404);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_404_when_post_root(#[values("fn_rust", "fn_ts", "fn_python")] function: &str) {
        // TC: FNC-06
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;

        let response = invoke(
            &runtime,
            world.prod.id,
            post("/", "text/plain", "not a form"),
        )
        .await;

        assert_eq!(response.status().as_u16(), 404);
    }

    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn leaves_counter_unchanged_when_path_is_unknown(
        #[values("fn_rust", "fn_ts", "fn_python")] function: &str,
    ) {
        // TC: FNC-06
        let world = World::new().await;
        let runtime = serving(&world, function, &[]).await;
        invoke(&runtime, world.prod.id, get("/no-such-page")).await;

        let json = root_json(&runtime, world.prod.id, "/").await;

        assert_eq!(json["counter"], 1);
    }
}
