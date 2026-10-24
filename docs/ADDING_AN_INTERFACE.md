# Adding an interface adapter

Example: a WebSocket adapter named `ws`. Every step has a check.

1. **Create the crate.** `crates/ws/Cargo.toml` with `name = "liftoff-ws"`, the
   `*.workspace = true` package fields and `[lints] workspace = true`.
   Depend on `liftoff-core` plus the protocol stack, nothing else from the
   workspace. Add `liftoff-ws = { path = "crates/ws" }` to
   `[workspace.dependencies]` and `"crates/ws"` to `[workspace] members`.
   *Check:* `cargo check -p liftoff-ws`.

2. **Expose three things** from `lib.rs`, mirroring `liftoff-rest`:
   - `WsConfig`: `Deserialize + Serialize + Default`, `#[serde(deny_unknown_fields, default)]`,
     durations via `humantime_serde`, loopback bind by default.
   - A constructor taking the services it exposes: `fn router(state: AppState, config: &WsConfig)`.
   - `async fn serve(config, service, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<(), ServeError>`
     plus `serve_on(listener, …)` for tests.
   *Check:* `cargo doc -p liftoff-ws` has no missing-docs errors.

3. **Map errors once.** A `From<liftoff_core::Error>` into the protocol's error type with
   a trailing `_ => internal` arm. Internal causes are logged, never sent.
   *Check:* a unit test per variant, including one asserting the internal
   message does not contain the source text.

4. **Bound every input.** Payload size, message count, idle timeout, per-connection
   concurrency. Parse raw input into domain types (`ProjectName::new`) before calling a service.
   *Check:* one test per limit sending an over-limit input.

5. **Wire the binary.** In `crates/liftoffd/Cargo.toml` depend on `liftoff-ws`.
   In `config.rs` add a `pub ws: WsConfig` section to `AppConfig` (or map an
   existing section onto `liftoff_ws::WsConfig`, as `ApiConfig::rest` does). In
   `error.rs` add a `Ws(#[from] liftoff_ws::ServeError)` variant. In
   `daemon.rs::serve` spawn it with a child shutdown token
   (`stop.child_token()`) next to the REST listener, over the same `Services`.
   *Check:* `cargo check -p liftoffd`.

6. **Document.** A `[ws]` section in `config/default.toml`, a row in the
   the layout table of CONTRIBUTING.md, an env example in `.env.example`.
   *Check:* `just ci` is green; `just serve` logs `WS listening`.
