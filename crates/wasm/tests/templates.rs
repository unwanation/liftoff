#![cfg(test)]
//! The function templates are deployable: each of `templates/fn-ts`, `templates/fn-rust` and
//! `templates/fn-python`, built by its own `[function] build` in a copy
//! (`scripts/build-templates.sh`, part of `just functions`, as
//! `tpl_fn_ts`, `tpl_fn_rust` and `tpl_fn_python`), passes the deploy's
//! [`liftoff_wasm::functions::check`], survives the warm-up of an
//! activation under the daemon's default `[functions]` limits and answers
//! as its README says:
//!
//! | Request | Answer |
//! |---|---|
//! | `GET /` | `200`, JSON `{"greeting", "visits"}`: the secret `GREETING` and a `wasi:keyvalue` counter |
//! | `POST /echo` | the request's body and `content-type` |
//! | anything else | `404` |
//!
//! `liftoff_wasm::testing::component` reads them from `WASM_MODULES_DIR`,
//! which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;

use bytes::Bytes;
use liftoff_core::domain::{Env, SecretKey, SecretValue};
use liftoff_core::fakes::{InMemoryStore, World};
use liftoff_wasm::functions::{self, FunctionLimits, FunctionRuntime};
use liftoff_wasm::testing::component;
use rstest::rstest;
use serde_json::{Value, json};

type Runtime = FunctionRuntime<InMemoryStore>;

/// How long a test waits for something that must happen.
const PATIENCE: Duration = Duration::from_secs(60);

/// The secret `GREETING` of the tests.
const GREETING: &str = "hello from the template test";

/// The daemon's default `[functions]` (`liftoffd`'s `FunctionsConfig`).
const fn defaults() -> FunctionLimits {
    FunctionLimits {
        max_memory_bytes: 128 * 1024 * 1024,
        timeout: Duration::from_secs(30),
        instances_per_env: 4,
        kv_max_bytes: 64 * 1024 * 1024,
        outgoing_timeout: Duration::from_secs(30),
    }
}

/// A runtime over the world's store with the built template `name`
/// active in `prod` (its warm-up included) and the secret `GREETING`.
async fn serving(world: &World, name: &str) -> Runtime {
    let runtime = FunctionRuntime::new(world.ports.store.clone(), defaults())
        .unwrap()
        .with_compile_cache(
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("template-functions-cache"),
        )
        .unwrap();
    let compiled = tokio::time::timeout(PATIENCE, runtime.precompile(&component(name).unwrap()))
        .await
        .expect("the component compiles in time")
        .unwrap();
    let config = Env::new(
        core::iter::once((
            SecretKey::new("GREETING").unwrap(),
            SecretValue::new(GREETING.to_owned()),
        ))
        .collect(),
    );
    tokio::time::timeout(PATIENCE, runtime.activate(world.prod.id, &compiled, config))
        .await
        .expect("the warm-up returns in time")
        .unwrap();
    runtime
}

fn request(method: &str, target: &str, body: impl Into<Bytes>) -> http::Request<Bytes> {
    http::Request::builder()
        .method(method)
        .uri(target)
        .header("host", "tpl-check.apps.test")
        .header("content-type", "text/plain")
        .body(body.into())
        .unwrap()
}

/// The function's answer to `request`, at most [`PATIENCE`].
async fn invoke(
    runtime: &Runtime,
    world: &World,
    request: http::Request<Bytes>,
) -> http::Response<Bytes> {
    tokio::time::timeout(PATIENCE, runtime.invoke(world.prod.id, request))
        .await
        .expect("the request returns in time")
        .unwrap()
}

/// The JSON of the answer to `GET /`, which must be `200`.
async fn root_json(runtime: &Runtime, world: &World) -> Value {
    let response = invoke(runtime, world, request("GET", "/", Bytes::new())).await;
    assert_eq!(response.status().as_u16(), 200);
    serde_json::from_slice(response.body()).unwrap()
}

mod check {
    use super::*;

    // TC: FDEP-02 (the deploy's check accepts each built template)
    #[rstest]
    fn accepts_component_when_template_built(
        #[values("tpl_fn_ts", "tpl_fn_rust", "tpl_fn_python")] template: &str,
    ) {
        assert!(functions::check(&component(template).unwrap()).is_ok());
    }
}

mod get_root {
    use super::*;

    // TC: FNC-01, FNC-03, FNC-04 (the templates' `GET /`)
    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_greeting_secret_and_first_visit_when_get_root(
        #[values("tpl_fn_ts", "tpl_fn_rust", "tpl_fn_python")] template: &str,
    ) {
        let world = World::new().await;
        let runtime = serving(&world, template).await;

        let json = root_json(&runtime, &world).await;

        assert_eq!(json, json!({"greeting": GREETING, "visits": 1}));
    }

    // TC: FNC-04 (the counter lives in the environment's key-value store)
    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn counts_second_visit_when_get_root_twice(
        #[values("tpl_fn_ts", "tpl_fn_rust", "tpl_fn_python")] template: &str,
    ) {
        let world = World::new().await;
        let runtime = serving(&world, template).await;
        root_json(&runtime, &world).await;

        let json = root_json(&runtime, &world).await;

        assert_eq!(json["visits"], json!(2));
    }
}

mod echo {
    use super::*;

    // TC: FNC-02 (the templates' `POST /echo`)
    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_body_and_content_type_when_post_echo(
        #[values("tpl_fn_ts", "tpl_fn_rust", "tpl_fn_python")] template: &str,
    ) {
        let world = World::new().await;
        let runtime = serving(&world, template).await;

        let response = invoke(&runtime, &world, request("POST", "/echo", "ping")).await;

        assert_eq!(
            (
                response.status().as_u16(),
                response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok()),
                response.body().as_ref(),
            ),
            (200, Some("text/plain"), b"ping".as_slice())
        );
    }
}

mod unknown_path {
    use super::*;

    // TC: FNC-06 (the templates' 404)
    #[rstest]
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_404_when_path_unknown(
        #[values("tpl_fn_ts", "tpl_fn_rust", "tpl_fn_python")] template: &str,
    ) {
        let world = World::new().await;
        let runtime = serving(&world, template).await;

        let response = invoke(&runtime, &world, request("GET", "/nope", Bytes::new())).await;

        assert_eq!(response.status().as_u16(), 404);
    }
}
