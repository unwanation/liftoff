# Working in this repository

Cargo workspace: domain, services and ports in `crates/core`; API wire types
in `crates/api`; driven adapters in `crates/{store,host,proxy,wasm}` (`wasm`
is the module host and the function runtime); the tunnel of `lof dev`
(protocol, multiplexer, both sides; no workspace dependency) in
`crates/tunnel`; the HTTP adapter in `crates/rest` (with the artifact
viewer and the panel's CORS); the MCP server in `crates/mcp` (stdio for
`lof mcp`, Streamable HTTP that the daemon mounts at `/mcp`); the daemon
binary `liftoffd` in `crates/liftoffd` and the CLI binary `liftoff` in
`crates/liftoff`. The modules are a second workspace, `modules/` (see
«Modules» below). The sources of the templates liftoff ships are in
`templates/<name>/` (published as repositories of their own;
`crates/core/tests/templates.rs` parses each `liftoff.toml`). Design and the rules behind it: `docs/ARCHITECTURE.md`.
What users do, command by command: `docs/REFERENCE.md` (the README
stays a plain description of the features). Layout and checks:
`CONTRIBUTING.md`.
Adding an interface: `docs/ADDING_AN_INTERFACE.md`. Writing a module:
`docs/MODULES.md`. Making a template: `docs/TEMPLATES.md`. Functions:
`docs/FUNCTIONS.md`. `lof dev` and the tunnel: `docs/DEV.md`. Artifacts:
`docs/ARTIFACTS.md`. MCP (both transports, OAuth): `docs/MCP.md`.

## Done means

`just ci` passes. It runs rustfmt, taplo, typos, clippy with every workspace
lint denied, cargo-machete, the feature powerset, nextest + doctests, rustdoc
and cargo-deny, over both workspaces, plus `just modules` (every component
built for `wasm32-wasip2`) and `just functions` (the contract functions)
before the tests, a dry run of the module releases and
`scripts/wit-deps.sh --check`. The pre-commit/pre-push hooks run the same commands.

## Conventions the linters do not fully enforce

- Validate at the boundary: raw input becomes a domain type (`ProjectName::new`) in
  the adapter; the domain never re-validates.
- Error mapping lives in one `From`/`fn` per adapter with a trailing
  `_ => internal` arm; internal sources go to `tracing::error!`, never to callers.
- Ports return `impl Future`; services are generic over `P: Ports` (one
  associated type per port) and adapters over the port traits they use
  (`S: Store`). Reach for `Arc<dyn …>` only when a heterogeneous
  collection needs it.
- Every rule lives in a `liftoff-core` service; adapters translate or do
  I/O. Extension points go through `ports::Hooks`, whose defaults mean "no
  opinion" (`NoHooks` is the core without modules).
- The CLI (`crates/liftoff`) depends on `liftoff-api` and `liftoff-mcp`,
  and for local mode on `liftoff-core`,
  `liftoff-wasm` and `liftoff-module-sql`: without a server it runs the
  same core services over its own adapters (`crates/liftoff/src/local/`).
  Never on `store`, `host`, `proxy` or `rest` (`cargo tree -p liftoff`);
  `crates/mcp` never depends on `liftoff-core`.
- Adapters never depend on each other, not even as dev-dependencies:
  `liftoffd → rest, store, host, proxy, wasm, tunnel, mcp → core`
  (`liftoffd::mcp_http` mounts `liftoff-mcp`'s Streamable HTTP
  transport beside the REST router); `rest → api`;
  `liftoff → core, wasm, module-sql, mcp → api`; `liftoffd` (the
  daemon's side) and `liftoff` (`lof dev`'s client) use
  `liftoff-tunnel`, which depends on no workspace crate (not even
  `core`). The function runtime lives in `crates/wasm` behind the core's
  `FunctionHost`, the component files behind `ComponentFiles` in
  `crates/host`, and the listeners (`functions.bind`, `tunnel.bind`,
  hyper) in `crates/liftoffd`; the files of artifact versions live
  behind the core's `Blobs` port in `crates/store` (`FileBlobs`), the
  MCP HTTP transport in `crates/mcp` as a tower service
  (`liftoff_mcp::http`), the panel's CORS layer in `crates/rest`
  (`cors.rs`, asking the core's `WebPanelService`); `cargo tree -p
  liftoff-core` has no hyper, tokio or wasmtime. `liftoff-module-sql` is
  no adapter but the SQL of module tables (key spaces, the `store-query`
  authorizer) that the daemon's store and the CLI's `modules.db` share.
  A test that needs two adapters (a module over SQLite) lives in
  `crates/liftoffd/tests`.
- Override a lint with `#[expect(lint, reason = "…")]` on the smallest item.
  Widening a workspace allow-list in `Cargo.toml` is a design decision; say so
  in the commit body.
- Tests: `mod <unit_of_work> { fn <expected>_when_<state>() }`, one behaviour per
  test, hand-written fakes (no mocking crates). Integration tests start
  with `#![cfg(test)]`. Cite the catalogue ID in a comment (`// TC: DEP-09`).
- Service and REST tests start from `liftoff_core::fakes::World` (feature
  `test-support`): the catalogue's user, project, environments and
  secrets over `FakePorts`, whose fakes
  share one ordered `CallLog` and take `fail_next(op, error)`. The `Store`
  contract lives in `fakes::store_contract`; every `Store` implementation
  of an installation must pass it. The CLI's `LocalSecretStore` keeps only
  local mode's records (connections, modules, template cache) and
  refuses an installation's with `Unavailable`: it runs the contract's
  rules that seed no users (the module rules), and its own tests cover
  the local-user versions of the rest; `LocalModuleStore` runs the
  contract's `ModuleStore` rules. Local mode keeps rows; the secret rules
  it applies are the core's (`service::check_ci_scope`, `env_scopes`,
  `merge_layers`).
- CLI tests never touch the real home directory: `tests/cli/support.rs`
  gives every run a tempdir config home (`Cli::new`: the `liftoff::Env`
  it runs with names a config path there, as `LIFTOFF_CONFIG` does for
  the binary), and the local-mode tests add the
  module registry, the forge and `git`'s own environment through
  `liftoff::local::Local` (`with_git_env`, `routing_https_to`). A remote
  is a bare repository under a tempdir (`tests/cli/local/repo.rs`,
  `GitHome`: `https://github.com/` rewritten to `file://` URLs of them
  by the test's own `GIT_CONFIG_GLOBAL`), never the network; the forge's
  API is a fake on loopback. Tests that need a live forge are
  `#[ignore = "live: …"]`.
- Commits follow Conventional Commits (`committed.toml`).
- The CLI owns stdout; anything it runs must log to stderr. Its own log
  (`tracing`, `LIFTOFF_LOG`, `warn` by default) goes to stderr too, and
  internal causes go there, never into the answer.
- The only host processes outside `crates/host` are `liftoffd setup`'s
  `Runner`, `lof server upgrade`'s `HostCommand` (which `lof artifact
  open --browser` also takes to run `xdg-open <link>`, `open` on macOS;
  its failure is a warning), local mode's `git`
  children (`crates/liftoff/src/local/git.rs`), the `[dev] command` of
  `lof dev` (`crates/liftoff/src/commands/dev.rs`) and the `[function]
  build` of `lof deploy` (`crates/liftoff/src/commands/functions.rs`;
  `docs/ARCHITECTURE.md`, «Исключения»): an argument vector, never a
  shell; `-c protocol.ext.allow=never`, `GIT_TERMINAL_PROMPT=0`, `--`
  before operands, stderr passed through; secrets through the
  environment only. The only shells are `sh -c` over the `[dev] command`
  and the `[function] build` strings, which come from the person's own
  `liftoff.toml` (or `package.json`), never from a server; the dev
  command and the build each lead their own process group, which Ctrl-C
  (and the build's own exit) ends; the build's stdout and stderr both go
  to the CLI's stderr.
- Releases: a `v*.*.*` tag runs the release workflows, which ship
  `liftoffd`, `liftoff`, an archive with the `lof` symlink, `install.sh`
  and `SHA256SUMS-<target>.txt` signed with the `LIFTOFF_SIGNING_KEY`
  secret (`scripts/package-release.sh`). Record user-visible changes in
  `CHANGELOG.md`.

## Modules

- `modules/` is its own Cargo workspace (the root one excludes it): one
  crate per module, contract components of the host in `modules/testing/*`,
  release scripts in `modules/registry/`. Build with the native
  `wasm32-wasip2` target (in `rust-toolchain.toml`) and `wit-bindgen` 0.63
  over the repository's `wit/`; `cargo-component` stays out (unmaintained).
  Its lints are the root's except `unsafe_code = "deny"`, so the
  wit-bindgen glue takes `#[expect(unsafe_code)]`. Dependencies are pure
  Rust: there is no C toolchain for `wasm32-wasip2`.
- The modules: `oidc`, `tenants`, `dns.cloudflare`, `git.forgejo`,
  `git.github`, `git.gitlab`, `mcp` (MCP over HTTP; requires `oidc`,
  whose `verify_token` checks the access tokens) and `web` (the panel
  `liftoff-web`: CORS, cookie policy, page redirects), in
  `modules/<crate-dir>`. The core asks these data hooks of one
  module only: `mcp_resource` of `mcp`, `web_panel` of `web`,
  `verify_token` of the module that replaces `login`; a page redirect
  only from `web` to its panel's origin.
- wasmtime lives in `crates/wasm` only; `cargo tree -p liftoff-core` stays
  free of it and of every HTTP, runtime or I/O crate.
- The core knows no tenant rules. Tenants, memberships, roles and quotas
  live in `modules/tenants`' own tables, and the module decides: the core
  keeps only the opaque `tenant_id` of projects and domains and
  `environments.protected`, asks hooks (`authorize`, `gate`,
  `project_tenant` for a new project's tenant, `tenant_quotas` whose
  answer it enforces), and takes every other write of those columns from
  the module named `tenants` alone (`core-call tenancy.*`,
  `environments.set_protected`). A new tenant rule goes into the module,
  or into a hook the core asks.
- A module has no tests of its own crate: host tests load the built
  component, `crates/wasm/tests/<module>.rs` over `InMemoryStore`, or
  `crates/liftoffd/tests/<module>.rs` when it uses `store-query` (SQLite).
  They find the `.wasm` in `WASM_MODULES_DIR`, which the `justfile` sets,
  else in `modules/target/wasm32-wasip2/release/` (out of git); run
  `just modules` before a bare `cargo nextest`.
- The contract functions (`modules/testing/functions/{rust,ts,python}`,
  plus `misbehave`) are built by `just functions` into
  `WASM_MODULES_DIR`: Rust with `wit-bindgen` over `wit/function/`, TS
  with `bun` 1.4.2 (`bun build`, then `jco componentize` 1.39.0 of
  `componentize-js` 0.23.0), Python with `uvx componentize-py@0.25.1`
  (`uv` 0.12.17); `bun` and `uv` are pinned in `mise.toml` and both
  fetch their packages from the network. `crates/wasm/tests/contract_functions.rs`
  runs all three. The function templates `templates/fn-*` use the same
  tools and versions and carry a copy of `wit/function/deps/` that
  `scripts/wit-deps.sh --check` holds equal; their build outputs stay
  out of git.
- A module ships by the tag `modules/<crate-dir>/vX.Y.Z`
  (`.github/workflows/modules.yml`); a releasable crate carries
  `[package.metadata.liftoff] liftoffd_min`.

## Where the project is

v2 is released (`v2.0.x`): the core, the module host and registry, the
eight modules, templates and `lof create`, functions and tunnels,
artifacts and MCP over HTTP. Design and the rules behind it:
`docs/ARCHITECTURE.md`; user-visible changes: `CHANGELOG.md`. Tests cite
the ids of the original test-case catalogue (`// TC: DEP-09`) as stable
labels of the behaviour they pin.
