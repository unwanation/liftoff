#![cfg(test)]
//! The runtime of functions over built components (FN-01…FN-10): `liftoff_wasm::functions::FunctionRuntime` loads
//! the contract function `fn_rust` (`modules/testing/functions/rust`, the
//! FNC behaviour of) and the misbehaving `fn_misbehave`
//! (`modules/testing/functions/misbehave`), keeps their `wasi:keyvalue`
//! in the world's `InMemoryStore`, gives them the resolved secrets of an
//! environment as `wasi:config`, and sends their outgoing `wasi:http` to
//! fake servers on loopback.
//!
//! The components are built by `just functions` (part of `just test` and
//! `just ci`); `liftoff_wasm::testing::component` reads them from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;
use std::sync::Arc;

use bytes::Bytes;
use liftoff_core::domain::{ComponentHash, Env, EnvironmentId, SecretKey, SecretValue};
use liftoff_core::fakes::{InMemoryStore, World};
use liftoff_core::ports::Store as _;
use liftoff_wasm::CompileStats;
use liftoff_wasm::functions::{
    FunctionError, FunctionLimits, FunctionRuntime, MAX_COMPONENT_BYTES, MAX_RESPONSE_BYTES,
};
use liftoff_wasm::testing::{Listener, component};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{Semaphore, watch};

type Runtime = FunctionRuntime<InMemoryStore>;

/// How long a test waits for something that must happen.
const PATIENCE: Duration = Duration::from_secs(20);

/// The limits of the tests: 32 MiB of memory, a 20 s request timeout, two
/// instances per environment, 64 KiB of keys and values, 1 s for an
/// outgoing request.
const fn limits() -> FunctionLimits {
    FunctionLimits {
        max_memory_bytes: 32 * 1024 * 1024,
        timeout: Duration::from_secs(20),
        instances_per_env: 2,
        kv_max_bytes: 64 * 1024,
        outgoing_timeout: Duration::from_secs(1),
    }
}

/// The limits of the queue tests: one instance per environment, a 2 s
/// request timeout, and outgoing requests that wait longer than that.
const fn queued() -> FunctionLimits {
    FunctionLimits {
        timeout: Duration::from_secs(2),
        instances_per_env: 1,
        outgoing_timeout: Duration::from_secs(60),
        ..limits()
    }
}

/// A runtime over the world's store that may reach loopback.
fn runtime(world: &World, limits: FunctionLimits) -> Arc<Runtime> {
    Arc::new(
        FunctionRuntime::new(world.ports.store.clone(), limits)
            .unwrap()
            .allowing_private_networks(),
    )
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

/// Precompiles the built component `name` and activates it for
/// `environment` with `config`.
async fn activate(
    runtime: &Runtime,
    environment: EnvironmentId,
    name: &str,
    config: Env,
) -> Result<(), FunctionError> {
    let compiled = runtime.precompile(&component(name).unwrap()).await?;
    runtime.activate(environment, &compiled, config).await
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

fn post(target: &str, body: impl Into<Bytes>) -> http::Request<Bytes> {
    request("POST", target, body)
}

/// Sends `request` to `environment`'s function, at most [`PATIENCE`].
async fn invoke(
    runtime: &Runtime,
    environment: EnvironmentId,
    request: http::Request<Bytes>,
) -> Result<http::Response<Bytes>, FunctionError> {
    tokio::time::timeout(PATIENCE, runtime.invoke(environment, request))
        .await
        .expect("the request returns in time")
}

/// The status and body (as text) of the function's answer to `request`.
async fn answer(
    runtime: &Runtime,
    environment: EnvironmentId,
    request: http::Request<Bytes>,
) -> (u16, String) {
    let response = invoke(runtime, environment, request).await.unwrap();
    (
        response.status().as_u16(),
        String::from_utf8_lossy(response.body()).into_owned(),
    )
}

/// The status the listener answers for `request`: the function's own, or
/// the runtime's for its error.
async fn status(
    runtime: &Runtime,
    environment: EnvironmentId,
    request: http::Request<Bytes>,
) -> u16 {
    match invoke(runtime, environment, request).await {
        Ok(response) => response.status().as_u16(),
        Err(error) => error.status(),
    }
}

/// How many requests this instance of `fn_misbehave` served.
async fn served(runtime: &Runtime, environment: EnvironmentId) -> u64 {
    let (code, body) = answer(runtime, environment, get("/instance")).await;
    assert_eq!(code, 200, "GET /instance answers");
    body.parse().unwrap()
}

/// A function written as WAT, for what a Rust guest cannot do: it imports
/// the two resources of `wasi:http/types` plus `imports`, instantiates
/// `modules` (core modules and instances) beside its handler, and exports
/// `wasi:http/incoming-handler`, whose `handle` returns without setting a
/// response.
fn wat_function(imports: &str, modules: &str) -> Vec<u8> {
    let text = format!(
        r#"(component
  (import "wasi:http/types@0.2.12" (instance $types
    (export "incoming-request" (type (sub resource)))
    (export "response-outparam" (type (sub resource)))))
  (alias export $types "incoming-request" (type $request))
  (alias export $types "response-outparam" (type $outparam))
  {imports}
  (core module $handler (func (export "handle") (param i32 i32)))
  (core instance $handler (instantiate $handler))
  {modules}
  (func $handle (param "request" (own $request)) (param "response-out" (own $outparam))
    (canon lift (core func $handler "handle")))
  (instance $incoming (export "handle" (func $handle)))
  (export "wasi:http/incoming-handler@0.2.12" (instance $incoming)))"#
    );
    wat::parse_str(text).unwrap()
}

/// `count` core instances of a module with one memory of `pages` 64 KiB
/// pages, as WAT for [`wat_function`].
fn memories(count: usize, pages: u32) -> String {
    let mut text = format!("(core module $memory (memory {pages}))");
    for _ in 0..count {
        text.push_str(" (core instance (instantiate $memory))");
    }
    text
}

/// The reason of a refused activation.
fn warmup_reason(refused: Result<(), FunctionError>) -> String {
    match refused {
        Err(FunctionError::Warmup { reason }) => reason,
        other => panic!("expected a failed warm-up, got {other:?}"),
    }
}

/// How many files are below `dir`.
fn files_below(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() { files_below(&path) } else { 1 }
            })
            .sum()
    })
}

/// A loopback HTTP server that counts the requests it read and answers
/// each `200 held` only when the test lets one go ([`Holding::release`]).
struct Holding {
    port: u16,
    arrived: watch::Receiver<usize>,
    gate: Arc<Semaphore>,
}

impl Holding {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (count, arrived) = watch::channel(0_usize);
        let gate = Arc::new(Semaphore::new(0));
        let held = Arc::clone(&gate);
        tokio::spawn(async move {
            let count = Arc::new(count);
            while let Ok((mut stream, _)) = listener.accept().await {
                let count = Arc::clone(&count);
                let gate = Arc::clone(&held);
                tokio::spawn(async move {
                    let mut seen: Vec<u8> = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    while !seen.windows(4).any(|end: &[u8]| end == b"\r\n\r\n") {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => seen.extend(buffer.iter().take(read)),
                        }
                    }
                    count.send_modify(|arrived| *arrived = arrived.saturating_add(1));
                    let Ok(permit) = gate.acquire().await else {
                        return;
                    };
                    permit.forget();
                    let _sent = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\nheld",
                        )
                        .await;
                });
            }
        });
        Self {
            port,
            arrived,
            gate,
        }
    }

    fn url(&self) -> String {
        format!("/fetch?url=http://127.0.0.1:{}/hold", self.port)
    }

    /// Waits until `count` requests arrived; `false` when they did not
    /// within `limit`.
    async fn arrived(&self, count: usize, limit: Duration) -> bool {
        let mut arrived = self.arrived.clone();
        tokio::time::timeout(limit, arrived.wait_for(|seen| *seen >= count))
            .await
            .is_ok_and(|seen| seen.is_ok())
    }

    /// Lets `count` held requests answer.
    fn release(&self, count: usize) {
        self.gate.add_permits(count);
    }
}

mod load {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_component_when_it_lacks_incoming_handler() {
        // TC: FN-01
        let world = World::new().await;
        let runtime = runtime(&world, limits());

        // `trap` is a component of the module world: no incoming-handler.
        let refused = runtime.precompile(&component("trap").unwrap()).await;

        assert!(
            matches!(refused, Err(FunctionError::Load { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn names_incoming_handler_when_component_lacks_it() {
        // TC: FN-01
        let world = World::new().await;
        let runtime = runtime(&world, limits());

        let error = runtime
            .precompile(&component("trap").unwrap())
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("wasi:http/incoming-handler"),
            "{error}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_bytes_when_they_are_not_a_component() {
        // TC: FN-01
        let world = World::new().await;
        let runtime = runtime(&world, limits());

        let refused = runtime.precompile(b"not wasm at all").await;

        assert!(
            matches!(refused, Err(FunctionError::Load { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn names_store_query_when_component_imports_it() {
        // TC: FN-01
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let bytes = wat_function(
            r#"(import "liftoff:module/host@0.1.0" (instance
                (export "store-query" (func (param "sql" string)))))"#,
            "",
        );

        let refused = runtime.precompile(&bytes).await;

        assert!(
            matches!(&refused, Err(FunctionError::Load { reason }) if reason.contains("liftoff:module/host")),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_nothing_to_cache_when_component_lacks_incoming_handler() {
        // TC: FN-01
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let runtime = FunctionRuntime::new(world.ports.store.clone(), limits())
            .unwrap()
            .with_compile_cache(&cache)
            .unwrap();

        let _refused = runtime.precompile(&component("trap").unwrap()).await;

        assert_eq!(files_below(&cache), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_nothing_to_cache_when_component_imports_store_query() {
        // TC: FN-01
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let runtime = FunctionRuntime::new(world.ports.store.clone(), limits())
            .unwrap()
            .with_compile_cache(&cache)
            .unwrap();
        let bytes = wat_function(
            r#"(import "liftoff:module/host@0.1.0" (instance
                (export "store-query" (func (param "sql" string)))))"#,
            "",
        );

        let _refused = runtime.precompile(&bytes).await;

        assert_eq!(files_below(&cache), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_component_when_it_passes_max_component_bytes() {
        // TC: FN-01
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let mut bytes = component("fn_rust").unwrap();
        bytes.resize(MAX_COMPONENT_BYTES.saturating_add(1), 0);

        let refused = runtime.precompile(&bytes).await;

        assert!(
            matches!(&refused, Err(FunctionError::Load { reason }) if reason.contains("larger than")),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_compiled_component_when_it_comes_from_another_runtime() {
        // TC: FN-01
        let world = World::new().await;
        let other = runtime(&world, limits());
        let compiled = other
            .precompile(&component("fn_misbehave").unwrap())
            .await
            .unwrap();
        let runtime = runtime(&world, limits());

        let refused = runtime.activate(world.prod.id, &compiled, env(&[])).await;

        assert!(
            matches!(refused, Err(FunctionError::Load { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hashes_component_when_it_exports_incoming_handler() {
        // TC: FN-01
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let bytes = component("fn_rust").unwrap();

        let compiled = runtime.precompile(&bytes).await.unwrap();

        assert_eq!(compiled.hash(), &ComponentHash::of(&bytes));
    }
}

mod sandbox {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_file_when_function_reads_one() {
        // TC: FN-01
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();

        let (code, _) = answer(&runtime, world.prod.id, get("/files?path=/etc/passwd")).await;

        assert_eq!(code, 403);
    }
}

/// What a function writes to stdout and stderr is its environment's log
/// (`lof logs` of a function).
mod logs {
    use futures_util::StreamExt as _;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn lines_of_stdout_and_stderr_when_function_prints() {
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();

        let (code, _) = answer(&runtime, world.prod.id, get("/print?text=hello")).await;
        let lines: Vec<String> = runtime
            .logs(world.prod.id, None, false)
            .map(Result::unwrap)
            .collect()
            .await;

        assert_eq!(
            (code, lines),
            (200, vec!["out hello".to_owned(), "err hello".to_owned()])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_of_other_environment_when_one_prints() {
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();

        let _answered = answer(&runtime, world.prod.id, get("/print?text=hello")).await;
        let lines: Vec<String> = runtime
            .logs(world.qa.id, None, false)
            .map(Result::unwrap)
            .collect()
            .await;

        assert_eq!(lines, Vec::<String>::new());
    }
}

mod config {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn serves_greeting_when_contract_function_reads_config() {
        // TC: FN-02
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(
            &runtime,
            world.prod.id,
            "fn_rust",
            env(&[("GREETING", "hello")]),
        )
        .await
        .unwrap();

        let (_, body) = answer(&runtime, world.prod.id, get("/")).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["greeting"], "hello");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serves_environment_value_when_project_sets_same_key() {
        // TC: FN-02
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        // Project `DATABASE_URL=postgres://db/shop`, `qa`'s own
        // `postgres://db/shop_qa`.
        let resolved = world
            .secrets()
            .resolve(&world.owner, &world.shop.name, &world.qa.name, None)
            .await
            .unwrap();
        activate(&runtime, world.qa.id, "fn_misbehave", resolved)
            .await
            .unwrap();

        let served = answer(&runtime, world.qa.id, get("/config?key=DATABASE_URL")).await;

        assert_eq!(served, (200, "postgres://db/shop_qa".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serves_project_value_when_environment_does_not_set_key() {
        // TC: FN-02
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let resolved = world
            .secrets()
            .resolve(&world.owner, &world.shop.name, &world.qa.name, None)
            .await
            .unwrap();
        activate(&runtime, world.qa.id, "fn_misbehave", resolved)
            .await
            .unwrap();

        let served = answer(&runtime, world.qa.id, get("/config?key=LOG_LEVEL")).await;

        assert_eq!(served, (200, "info".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_none_when_key_is_not_a_secret_of_environment() {
        // TC: FN-02
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(
            &runtime,
            world.prod.id,
            "fn_misbehave",
            env(&[("GREETING", "hello")]),
        )
        .await
        .unwrap();

        let (code, _) = answer(&runtime, world.prod.id, get("/config?key=MISSING")).await;

        assert_eq!(code, 404);
    }
}

mod keyvalue {
    use super::*;

    /// `fn_misbehave` active for both `prod` and `qa`.
    async fn both(world: &World) -> Arc<Runtime> {
        let runtime = runtime(world, limits());
        for environment in [world.prod.id, world.qa.id] {
            activate(&runtime, environment, "fn_misbehave", env(&[]))
                .await
                .unwrap();
        }
        runtime
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_back_value_when_function_set_it() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;
        let (set, _) = answer(&runtime, world.prod.id, post("/kv/set?key=color", "blue")).await;
        assert_eq!(set, 204, "the set succeeds");

        let read = answer(&runtime, world.prod.id, get("/kv/get?key=color")).await;

        assert_eq!(read, (200, "blue".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stores_value_in_kv_of_its_environment_when_set() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;

        answer(&runtime, world.prod.id, post("/kv/set?key=color", "blue")).await;

        assert_eq!(
            world
                .ports
                .store
                .kv_get(world.prod.id, "color")
                .await
                .unwrap(),
            Some(b"blue".to_vec())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hides_key_from_other_environment_when_both_run_same_function() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;
        answer(&runtime, world.prod.id, post("/kv/set?key=color", "blue")).await;

        let (code, _) = answer(&runtime, world.qa.id, get("/kv/get?key=color")).await;

        assert_eq!(code, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_own_value_when_other_environment_sets_same_key() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;
        answer(&runtime, world.prod.id, post("/kv/set?key=color", "blue")).await;
        answer(&runtime, world.qa.id, post("/kv/set?key=color", "green")).await;

        let read = answer(&runtime, world.prod.id, get("/kv/get?key=color")).await;

        assert_eq!(read, (200, "blue".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forgets_key_when_function_deletes_it() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;
        answer(&runtime, world.prod.id, post("/kv/set?key=color", "blue")).await;
        let (deleted, _) = answer(&runtime, world.prod.id, post("/kv/delete?key=color", "")).await;
        assert_eq!(deleted, 204, "the delete succeeds");

        let (code, _) = answer(&runtime, world.prod.id, get("/kv/get?key=color")).await;

        assert_eq!(code, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn counts_per_environment_when_function_increments() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;
        answer(&runtime, world.prod.id, get("/kv/incr?key=hits")).await;
        answer(&runtime, world.prod.id, get("/kv/incr?key=hits")).await;

        let counted = answer(&runtime, world.qa.id, get("/kv/incr?key=hits")).await;

        assert_eq!(counted, (200, "1".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn increments_counter_when_contract_function_answers_root() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_rust", env(&[]))
            .await
            .unwrap();
        answer(&runtime, world.prod.id, get("/")).await;

        let (_, body) = answer(&runtime, world.prod.id, get("/")).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["counter"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_bucket_when_name_is_not_default() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;

        let opened = answer(&runtime, world.prod.id, get("/kv/open?name=other")).await;

        assert_eq!(opened, (507, "no-such-store".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn opens_bucket_when_name_is_empty() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;

        let opened = answer(&runtime, world.prod.id, get("/kv/open?name=")).await;

        assert_eq!(opened, (200, "ok".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_key_when_it_passes_256_bytes() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;
        let key = "k".repeat(257);

        let (code, _) = answer(
            &runtime,
            world.prod.id,
            post(&format!("/kv/set?key={key}"), "v"),
        )
        .await;

        assert_eq!(code, 507);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn counts_every_increment_when_requests_run_in_parallel() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;

        let running: Vec<_> = (0..20)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                let prod = world.prod.id;
                tokio::spawn(async move { runtime.invoke(prod, get("/kv/incr?key=hits")).await })
            })
            .collect();
        for request in running {
            request.await.unwrap().unwrap();
        }

        assert_eq!(
            world
                .ports
                .store
                .kv_get(world.prod.id, "hits")
                .await
                .unwrap(),
            Some(b"20".to_vec())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reports_host_failure_when_store_fails_under_write() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;
        world.ports.store.fail_next(
            "kv_set",
            liftoff_core::Error::Unavailable("the disk is on fire".to_owned()),
        );

        let refused = invoke(&runtime, world.prod.id, post("/kv/set?key=color", "blue")).await;

        assert!(
            matches!(&refused, Err(FunctionError::HostFailed { reason }) if !reason.contains("disk")),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_many_values_when_function_sets_them_in_batch() {
        // TC: FN-03
        let world = World::new().await;
        let runtime = both(&world).await;
        let (set, _) = answer(&runtime, world.prod.id, post("/kv/set-many", "a=1&b=2")).await;
        assert_eq!(set, 204, "the batch set succeeds");

        let read = answer(
            &runtime,
            world.prod.id,
            get("/kv/get-many?keys=a,missing,b"),
        )
        .await;

        assert_eq!(read, (200, "1,-,2".to_owned()));
    }
}

mod kv_limit {
    use super::*;

    /// 1 KiB of keys and values per environment.
    const fn small_kv() -> FunctionLimits {
        FunctionLimits {
            kv_max_bytes: 1024,
            ..limits()
        }
    }

    /// `fn_misbehave` active for `prod` and `qa` with [`small_kv`].
    async fn small(world: &World) -> Arc<Runtime> {
        let runtime = runtime(world, small_kv());
        for environment in [world.prod.id, world.qa.id] {
            activate(&runtime, environment, "fn_misbehave", env(&[]))
                .await
                .unwrap();
        }
        runtime
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_function_error_when_value_passes_kv_max() {
        // TC: FN-04
        let world = World::new().await;
        let runtime = small(&world).await;

        // The function answers 507 for a `wasi:keyvalue` error: the
        // request itself succeeds.
        let answered = invoke(
            &runtime,
            world.prod.id,
            post("/kv/set?key=big", vec![b'x'; 2048]),
        )
        .await;

        assert_eq!(answered.map(|response| response.status().as_u16()), Ok(507));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn names_kv_max_mb_when_value_passes_limit() {
        // TC: FN-04
        let world = World::new().await;
        let runtime = small(&world).await;

        let (_, body) = answer(
            &runtime,
            world.prod.id,
            post("/kv/set?key=big", vec![b'x'; 2048]),
        )
        .await;

        assert!(body.contains("kv_max_mb"), "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stores_nothing_when_value_passes_kv_max() {
        // TC: FN-04
        let world = World::new().await;
        let runtime = small(&world).await;

        answer(
            &runtime,
            world.prod.id,
            post("/kv/set?key=big", vec![b'x'; 2048]),
        )
        .await;

        assert_eq!(
            world
                .ports
                .store
                .kv_get(world.prod.id, "big")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stores_nothing_when_batch_passes_kv_max() {
        // TC: FN-04
        let world = World::new().await;
        let runtime = small(&world).await;
        let batch = format!("a={}&b={}", "x".repeat(600), "x".repeat(600));
        let (code, _) = answer(&runtime, world.prod.id, post("/kv/set-many", batch)).await;
        assert_eq!(code, 507, "the batch is refused");

        let stored = world.ports.store.kv_get(world.prod.id, "a").await.unwrap();

        assert_eq!(stored, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_value_when_environment_total_passes_kv_max() {
        // TC: FN-04
        let world = World::new().await;
        let runtime = small(&world).await;
        let (first, _) = answer(
            &runtime,
            world.prod.id,
            post("/kv/set?key=a", vec![b'x'; 600]),
        )
        .await;
        assert_eq!(first, 204, "the first value fits");

        let (second, _) = answer(
            &runtime,
            world.prod.id,
            post("/kv/set?key=b", vec![b'x'; 600]),
        )
        .await;

        assert_eq!(second, 507);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn counts_limit_per_environment_when_other_environment_is_full() {
        // TC: FN-04
        let world = World::new().await;
        let runtime = small(&world).await;
        answer(
            &runtime,
            world.prod.id,
            post("/kv/set?key=a", vec![b'x'; 600]),
        )
        .await;

        let (other, _) = answer(
            &runtime,
            world.qa.id,
            post("/kv/set?key=a", vec![b'x'; 600]),
        )
        .await;

        assert_eq!(other, 204);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_serving_when_value_passed_kv_max() {
        // TC: FN-04
        let world = World::new().await;
        let runtime = small(&world).await;
        answer(
            &runtime,
            world.prod.id,
            post("/kv/set?key=big", vec![b'x'; 2048]),
        )
        .await;
        answer(&runtime, world.prod.id, post("/kv/set?key=small", "fits")).await;

        let read = answer(&runtime, world.prod.id, get("/kv/get?key=small")).await;

        assert_eq!(read, (200, "fits".to_owned()));
    }
}

mod timeout {
    use super::*;

    /// A 300 ms request timeout and a single instance.
    const fn short() -> FunctionLimits {
        FunctionLimits {
            timeout: Duration::from_millis(300),
            instances_per_env: 1,
            ..limits()
        }
    }

    async fn misbehaving(world: &World) -> Arc<Runtime> {
        let runtime = runtime(world, short());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        runtime
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_504_when_function_spins_past_timeout() {
        // TC: FN-05
        let world = World::new().await;
        let runtime = misbehaving(&world).await;

        let answered = status(&runtime, world.prod.id, get("/spin")).await;

        assert_eq!(answered, 504);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reports_timeout_when_function_spins_past_timeout() {
        // TC: FN-05
        let world = World::new().await;
        let runtime = misbehaving(&world).await;

        let refused = invoke(&runtime, world.prod.id, get("/spin")).await;

        assert_eq!(refused.map(|_| ()), Err(FunctionError::Timeout));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_504_when_function_waits_on_host_past_timeout() {
        // TC: FN-05
        let world = World::new().await;
        let runtime = misbehaving(&world).await;

        let answered = status(&runtime, world.prod.id, get("/sleep?ms=60000")).await;

        assert_eq!(answered, 504);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_from_fresh_instance_when_previous_request_timed_out() {
        // TC: FN-05
        let world = World::new().await;
        let runtime = misbehaving(&world).await;
        let first = served(&runtime, world.prod.id).await;
        assert_eq!(
            served(&runtime, world.prod.id).await,
            first.saturating_add(1),
            "the single instance serves both"
        );
        status(&runtime, world.prod.id, get("/spin")).await;

        let after = served(&runtime, world.prod.id).await;

        assert_eq!(after, 1);
    }
}

mod traps {
    use super::*;

    async fn misbehaving(world: &World) -> Arc<Runtime> {
        let runtime = runtime(world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        runtime
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_500_when_function_grows_past_memory_cap() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = misbehaving(&world).await;

        // 64 MiB against a cap of 32 MiB.
        let answered = status(&runtime, world.prod.id, get("/grow?bytes=67108864")).await;

        assert_eq!(answered, 500);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reports_trap_when_function_grows_past_memory_cap() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = misbehaving(&world).await;

        let refused = invoke(&runtime, world.prod.id, get("/grow?bytes=67108864")).await;

        assert!(
            matches!(refused, Err(FunctionError::Trap { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_within_cap_when_function_grows_below_it() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = misbehaving(&world).await;

        // 4 MiB against a cap of 32 MiB.
        let answered = status(&runtime, world.prod.id, get("/grow?bytes=4194304")).await;

        assert_eq!(answered, 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_pool_serving_when_request_trapped_on_memory() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = misbehaving(&world).await;
        status(&runtime, world.prod.id, get("/grow?bytes=67108864")).await;

        let answered = status(&runtime, world.prod.id, get("/instance")).await;

        assert_eq!(answered, 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_activation_when_memories_together_pass_memory_cap() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        // Two memories of 20 MiB each against a cap of 32 MiB.
        let compiled = runtime
            .precompile(&wat_function("", &memories(2, 320)))
            .await
            .unwrap();

        let refused = runtime.activate(world.prod.id, &compiled, env(&[])).await;

        let reason = warmup_reason(refused);
        assert!(reason.contains("max_memory_mb"), "{reason}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_activation_when_table_passes_table_cap() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let compiled = runtime
            .precompile(&wat_function(
                "",
                "(core module $table (table 1000000 funcref)) (core instance (instantiate $table))",
            ))
            .await
            .unwrap();

        let refused = runtime.activate(world.prod.id, &compiled, env(&[])).await;

        let reason = warmup_reason(refused);
        assert!(reason.contains("table"), "{reason}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hides_cause_when_core_instances_pass_their_cap() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        // 64 core instances of a module without memory.
        let modules = format!(
            "(core module $empty){}",
            " (core instance (instantiate $empty))".repeat(64)
        );
        let compiled = runtime
            .precompile(&wat_function("", &modules))
            .await
            .unwrap();

        let refused = runtime.activate(world.prod.id, &compiled, env(&[])).await;

        // wasmtime's own message stays in the daemon's log.
        let reason = warmup_reason(refused);
        assert!(reason.contains("the daemon's log"), "{reason}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_500_when_function_returns_without_response() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = misbehaving(&world).await;

        let refused = invoke(&runtime, world.prod.id, get("/silent")).await;

        assert!(
            matches!(refused, Err(FunctionError::Trap { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_500_when_response_passes_max_response_bytes() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = misbehaving(&world).await;
        let target = format!("/big?bytes={}", MAX_RESPONSE_BYTES.saturating_add(1));

        let refused = invoke(&runtime, world.prod.id, get(&target)).await;

        assert!(
            matches!(refused, Err(FunctionError::Trap { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_500_when_function_panics() {
        // TC: FN-06
        let world = World::new().await;
        let runtime = misbehaving(&world).await;

        let answered = status(&runtime, world.prod.id, get("/panic")).await;

        assert_eq!(answered, 500);
    }
}

mod pool {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn reuses_instance_when_request_finished() {
        // TC: FN-07
        let world = World::new().await;
        let runtime = runtime(
            &world,
            FunctionLimits {
                instances_per_env: 1,
                ..limits()
            },
        );
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let first = served(&runtime, world.prod.id).await;

        let second = served(&runtime, world.prod.id).await;

        assert_eq!(second, first.saturating_add(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runs_requests_in_parallel_up_to_instances_per_env() {
        // TC: FN-07
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let upstream = Holding::start().await;

        let running: Vec<_> = (0..2)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                let target = upstream.url();
                let prod = world.prod.id;
                tokio::spawn(async move { runtime.invoke(prod, get(&target)).await })
            })
            .collect();

        assert!(
            upstream.arrived(2, PATIENCE).await,
            "both requests run at once"
        );
        upstream.release(2);
        for request in running {
            request.await.unwrap().unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn holds_next_request_when_every_instance_is_busy() {
        // TC: FN-07
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let upstream = Holding::start().await;
        let running: Vec<_> = (0..3)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                let target = upstream.url();
                let prod = world.prod.id;
                tokio::spawn(async move { runtime.invoke(prod, get(&target)).await })
            })
            .collect();
        assert!(upstream.arrived(2, PATIENCE).await, "two requests run");

        let third_ran = upstream.arrived(3, Duration::from_millis(300)).await;

        upstream.release(3);
        for request in running {
            let _answered = request.await;
        }
        assert!(!third_ran, "the third request waits for an instance");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runs_waiting_request_when_instance_frees() {
        // TC: FN-07
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let upstream = Holding::start().await;
        let running: Vec<_> = (0..3)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                let target = upstream.url();
                let prod = world.prod.id;
                tokio::spawn(async move { runtime.invoke(prod, get(&target)).await })
            })
            .collect();
        assert!(upstream.arrived(2, PATIENCE).await, "two requests run");

        upstream.release(1);

        assert!(
            upstream.arrived(3, PATIENCE).await,
            "the third request runs once an instance frees"
        );
        upstream.release(2);
        for request in running {
            let response = request.await.unwrap().unwrap();
            assert_eq!(response.status().as_u16(), 200, "every request answers");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn times_out_waiting_request_within_timeout_of_its_arrival() {
        // TC: FN-07
        let world = World::new().await;
        let runtime = runtime(&world, queued());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let upstream = Holding::start().await;
        let holder = {
            let runtime = Arc::clone(&runtime);
            let target = upstream.url();
            let prod = world.prod.id;
            tokio::spawn(async move { runtime.invoke(prod, get(&target)).await })
        };
        assert!(upstream.arrived(1, PATIENCE).await, "the instance is busy");
        let started = std::time::Instant::now();

        let waited_out = invoke(&runtime, world.prod.id, get("/spin")).await;

        let waited = started.elapsed();
        upstream.release(1);
        let _held = holder.await;
        assert_eq!(waited_out.map(|_| ()), Err(FunctionError::Timeout));
        assert!(
            waited < queued().timeout.mul_f32(1.5),
            "the wait for an instance counts toward functions.timeout: {waited:?}"
        );
    }
}

mod outgoing {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn returns_upstream_body_when_function_fetches() {
        // TC: FN-08
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let upstream = Listener::start().await.unwrap();

        let fetched = answer(
            &runtime,
            world.prod.id,
            get(&format!("/fetch?url=http://127.0.0.1:{}/", upstream.port)),
        )
        .await;

        assert_eq!(fetched, (200, "ok".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetches_any_host_when_contract_function_has_fetch_url() {
        // TC: FN-08
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let upstream = Listener::start().await.unwrap();
        let fetch_url = format!("http://127.0.0.1:{}/hello", upstream.port);
        activate(
            &runtime,
            world.prod.id,
            "fn_rust",
            env(&[("FETCH_URL", &fetch_url)]),
        )
        .await
        .unwrap();

        let (_, body) = answer(&runtime, world.prod.id, get("/")).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["fetch"], "ok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fails_fetch_when_upstream_passes_outgoing_timeout() {
        // TC: FN-08
        let world = World::new().await;
        // A request may run 20 s, an outgoing request 300 ms; the test
        // waits 10 s at most, so only the outgoing timeout can end it.
        let runtime = runtime(
            &world,
            FunctionLimits {
                outgoing_timeout: Duration::from_millis(300),
                ..limits()
            },
        );
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let upstream = Listener::silent().await.unwrap();

        let answered = tokio::time::timeout(
            Duration::from_secs(10),
            runtime.invoke(
                world.prod.id,
                get(&format!("/fetch?url=http://127.0.0.1:{}/", upstream.port)),
            ),
        )
        .await
        .expect("the outgoing timeout ends the request")
        .map(|response| response.status().as_u16());

        // The function answers 502 for its failed fetch.
        assert_eq!(answered, Ok(502));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_loopback_when_private_networks_are_not_allowed() {
        // TC: FN-08
        let world = World::new().await;
        let runtime = FunctionRuntime::new(world.ports.store.clone(), limits()).unwrap();
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let upstream = Listener::start().await.unwrap();

        let (code, _) = answer(
            &runtime,
            world.prod.id,
            get(&format!("/fetch?url=http://127.0.0.1:{}/", upstream.port)),
        )
        .await;

        assert_eq!((code, upstream.accepted()), (502, 0));
    }
}

mod compile_cache {
    use super::*;

    fn cached(world: &World, dir: &std::path::Path) -> Runtime {
        FunctionRuntime::new(world.ports.store.clone(), limits())
            .unwrap()
            .allowing_private_networks()
            .with_compile_cache(dir)
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compiles_component_when_cache_is_empty() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = cached(&world, &dir.path().join("cache"));

        runtime
            .precompile(&component("fn_rust").unwrap())
            .await
            .unwrap();

        assert_eq!(
            runtime.compile_stats(),
            CompileStats {
                compiled: 1,
                cached: 0
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compiles_nothing_when_same_runtime_precompiles_same_hash_again() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let runtime = cached(&world, &dir.path().join("cache"));
        let bytes = component("fn_rust").unwrap();
        runtime.precompile(&bytes).await.unwrap();

        runtime.precompile(&bytes).await.unwrap();

        assert_eq!(runtime.compile_stats().compiled, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compiles_nothing_when_second_runtime_precompiles_same_hash() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let bytes = component("fn_rust").unwrap();
        cached(&world, &dir.path().join("cache"))
            .precompile(&bytes)
            .await
            .unwrap();
        let second = cached(&world, &dir.path().join("cache"));

        second.precompile(&bytes).await.unwrap();

        assert_eq!(
            second.compile_stats(),
            CompileStats {
                compiled: 0,
                cached: 1
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compiles_again_when_component_hash_differs() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        cached(&world, &dir.path().join("cache"))
            .precompile(&component("fn_rust").unwrap())
            .await
            .unwrap();
        let second = cached(&world, &dir.path().join("cache"));

        second
            .precompile(&component("fn_misbehave").unwrap())
            .await
            .unwrap();

        assert_eq!(
            second.compile_stats(),
            CompileStats {
                compiled: 1,
                cached: 0
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_compiled_code_in_cache_dir_when_precompiled() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let runtime = cached(&world, &cache);

        runtime
            .precompile(&component("fn_rust").unwrap())
            .await
            .unwrap();

        let kept = files_below(&cache);
        assert!(kept > 0, "files in the cache directory: {kept}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn makes_cache_dir_private_when_it_is_missing() {
        // TC: FN-09
        use std::os::unix::fs::PermissionsExt as _;
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let runtime = cached(&world, &cache);

        runtime
            .precompile(&component("fn_rust").unwrap())
            .await
            .unwrap();

        let mode = std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    /// The wasmtime version of `Cargo.lock`, the one the runtime is
    /// built with.
    fn wasmtime_version() -> String {
        let lock = include_str!("../../../Cargo.lock");
        let entry = lock
            .split("[[package]]")
            .find(|entry| entry.contains("\nname = \"wasmtime\"\n"))
            .expect("Cargo.lock has wasmtime");
        entry
            .lines()
            .find_map(|line| line.strip_prefix("version = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("wasmtime has a version")
            .to_owned()
    }

    /// The directories under the cache's `modules/`.
    fn version_dirs(cache: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(cache.join("modules"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_compiled_code_under_wasmtime_version_when_precompiled() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let runtime = cached(&world, &cache);

        runtime
            .precompile(&component("fn_rust").unwrap())
            .await
            .unwrap();

        assert_eq!(
            version_dirs(&cache),
            vec![format!("wasmtime-{}", wasmtime_version())]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compiles_again_when_cache_was_written_by_another_wasmtime_version() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let bytes = component("fn_rust").unwrap();
        cached(&world, &cache).precompile(&bytes).await.unwrap();
        // The same entries, as an older wasmtime would have left them.
        let current = cache
            .join("modules")
            .join(format!("wasmtime-{}", wasmtime_version()));
        std::fs::rename(&current, cache.join("modules").join("wasmtime-0.0.1")).unwrap();
        let second = cached(&world, &cache);

        second.precompile(&bytes).await.unwrap();

        assert_eq!(
            second.compile_stats(),
            CompileStats {
                compiled: 1,
                cached: 0
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_cache_dir_without_naming_it_when_it_is_a_file() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, b"").unwrap();

        let refused = FunctionRuntime::new(world.ports.store.clone(), limits())
            .unwrap()
            .with_compile_cache(&file)
            .map(|_| ());

        let Err(FunctionError::HostFailed { reason }) = refused else {
            panic!("an internal failure: {refused:?}");
        };
        assert!(
            !reason.contains("not-a-directory"),
            "the daemon's paths stay in its log: {reason}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_requests_when_component_comes_from_cache() {
        // TC: FN-09
        let world = World::new().await;
        let dir = tempfile::tempdir().unwrap();
        let bytes = component("fn_misbehave").unwrap();
        cached(&world, &dir.path().join("cache"))
            .precompile(&bytes)
            .await
            .unwrap();
        let second = cached(&world, &dir.path().join("cache"));
        let compiled = second.precompile(&bytes).await.unwrap();
        second
            .activate(world.prod.id, &compiled, env(&[]))
            .await
            .unwrap();

        let answered = status(&second, world.prod.id, get("/instance")).await;

        assert_eq!(answered, 200);
    }
}

mod warmup {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn sends_get_root_with_warmup_header_when_activated() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, limits());

        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();

        // `fn_misbehave` keeps what its warm-up request was.
        assert_eq!(
            world
                .ports
                .store
                .kv_get(world.prod.id, "warmup")
                .await
                .unwrap(),
            Some(b"GET /".to_vec())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activates_component_when_warmup_answers_500() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, limits());

        let activated = activate(
            &runtime,
            world.prod.id,
            "fn_misbehave",
            env(&[("WARMUP", "fail")]),
        )
        .await;

        assert_eq!(activated, Ok(()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_activation_when_warmup_traps() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, limits());

        let refused = activate(
            &runtime,
            world.prod.id,
            "fn_misbehave",
            env(&[("WARMUP", "trap")]),
        )
        .await;

        assert!(
            matches!(refused, Err(FunctionError::Warmup { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_activation_when_warmup_passes_timeout() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(
            &world,
            FunctionLimits {
                timeout: Duration::from_millis(300),
                ..limits()
            },
        );

        let refused = activate(
            &runtime,
            world.prod.id,
            "fn_misbehave",
            env(&[("WARMUP", "spin")]),
        )
        .await;

        assert!(
            matches!(refused, Err(FunctionError::Warmup { .. })),
            "{refused:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leaves_environment_inactive_when_warmup_passes_timeout() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(
            &world,
            FunctionLimits {
                timeout: Duration::from_millis(300),
                ..limits()
            },
        );
        let _refused = activate(
            &runtime,
            world.prod.id,
            "fn_misbehave",
            env(&[("WARMUP", "spin")]),
        )
        .await;

        let refused = invoke(&runtime, world.prod.id, get("/instance")).await;

        assert_eq!(refused.map(|_| ()), Err(FunctionError::NotActive));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leaves_environment_inactive_when_first_warmup_traps() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let _refused = activate(
            &runtime,
            world.prod.id,
            "fn_misbehave",
            env(&[("WARMUP", "trap")]),
        )
        .await;

        let refused = invoke(&runtime, world.prod.id, get("/instance")).await;

        assert_eq!(refused.map(|_| ()), Err(FunctionError::NotActive));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reports_inactive_when_first_warmup_traps() {
        // TC: FN-10 (what `start` of a function asks)
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let _refused = activate(
            &runtime,
            world.prod.id,
            "fn_misbehave",
            env(&[("WARMUP", "trap")]),
        )
        .await;

        assert!(!runtime.is_active(world.prod.id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reports_active_when_warmup_answers() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_rust", env(&[]))
            .await
            .unwrap();

        assert!(runtime.is_active(world.prod.id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_previous_component_when_warmup_traps() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_rust", env(&[]))
            .await
            .unwrap();
        let _refused = activate(
            &runtime,
            world.prod.id,
            "fn_misbehave",
            env(&[("WARMUP", "trap")]),
        )
        .await;

        // `fn_rust` echoes; `fn_misbehave` would answer 404.
        let echoed = answer(&runtime, world.prod.id, post("/echo", "still the old one")).await;

        assert_eq!(echoed, (200, "still the old one".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hides_warmup_header_when_client_sends_it() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let mut forged = get("/?from=client");
        forged
            .headers_mut()
            .insert("x-liftoff-warmup", http::HeaderValue::from_static("1"));

        let _answered = answer(&runtime, world.prod.id, forged).await;

        // `fn_misbehave` records a warm-up it sees; only the daemon's is.
        assert_eq!(
            world
                .ports
                .store
                .kv_get(world.prod.id, "warmup")
                .await
                .unwrap(),
            Some(b"GET /".to_vec())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_activation_within_timeout_when_every_instance_is_busy() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, queued());
        activate(&runtime, world.prod.id, "fn_misbehave", env(&[]))
            .await
            .unwrap();
        let next = runtime
            .precompile(&component("fn_misbehave").unwrap())
            .await
            .unwrap();
        let upstream = Holding::start().await;
        let holder = {
            let runtime = Arc::clone(&runtime);
            let target = upstream.url();
            let prod = world.prod.id;
            tokio::spawn(async move { runtime.invoke(prod, get(&target)).await })
        };
        assert!(upstream.arrived(1, PATIENCE).await, "the instance is busy");
        let started = std::time::Instant::now();

        let refused = runtime
            .activate(world.prod.id, &next, env(&[("WARMUP", "spin")]))
            .await;

        let waited = started.elapsed();
        upstream.release(1);
        let _held = holder.await;
        assert!(
            matches!(refused, Err(FunctionError::Warmup { .. })),
            "{refused:?}"
        );
        assert!(
            waited < queued().timeout.mul_f32(1.5),
            "the wait for an instance counts toward functions.timeout: {waited:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_503_when_environment_was_never_activated() {
        // TC: FN-10
        let world = World::new().await;
        let runtime = runtime(&world, limits());

        let answered = status(&runtime, world.prod.id, get("/")).await;

        assert_eq!(answered, 503);
    }
}

/// The runtime as the core's `FunctionHost`:
/// what a deploy is told.
mod deploy_host {
    use liftoff_core::Error;
    use liftoff_core::ports::FunctionHost as _;
    use liftoff_wasm::functions::FunctionDeployer;

    use super::*;

    // TC: FDEP-02 (the check of `POST /deploys`, before anything is kept)
    #[tokio::test]
    async fn validation_saying_not_a_wasi_http_component_when_checked_component_lacks_handler() {
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let checked = FunctionDeployer::new(Arc::clone(&runtime))
            .check(&component("probe").unwrap())
            .await;
        assert!(
            matches!(&checked, Err(Error::Validation(e))
                if e.field == "component" && e.to_string().contains("not a wasi:http component")),
            "{checked:?}"
        );
    }

    // TC: FDEP-02
    #[tokio::test]
    async fn compiles_nothing_when_checked() {
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        FunctionDeployer::new(Arc::clone(&runtime))
            .check(&component("fn_rust").unwrap())
            .await
            .unwrap();
        assert_eq!(runtime.compile_stats(), CompileStats::default());
    }

    // TC: FDEP-03 (activated through the port, the function answers)
    #[tokio::test]
    async fn function_answers_when_activated_through_port() {
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let bytes = component("fn_rust").unwrap();
        let deployer = FunctionDeployer::new(Arc::clone(&runtime));
        deployer.precompile(&bytes).await.unwrap();
        deployer
            .activate(world.prod.id, &bytes, env(&[]))
            .await
            .unwrap();
        assert_eq!(status(&runtime, world.prod.id, get("/")).await, 200);
    }

    // TC: FDEP-04 (a warm-up that traps is the component's fault: the
    // deploy fails with the reason)
    #[tokio::test]
    async fn validation_naming_warm_up_when_warm_up_traps() {
        let world = World::new().await;
        let runtime = runtime(&world, limits());
        let activated = FunctionDeployer::new(Arc::clone(&runtime))
            .activate(
                world.prod.id,
                &component("fn_misbehave").unwrap(),
                env(&[("WARMUP", "trap")]),
            )
            .await;
        assert!(
            matches!(&activated, Err(Error::Validation(e))
                if e.field == "component" && e.to_string().contains("warm-up")),
            "{activated:?}"
        );
    }
}
