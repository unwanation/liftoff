# Contributing to liftoff

How the repository is built, laid out and checked. What liftoff does for
its users: [README.md](README.md) and [docs/REFERENCE.md](docs/REFERENCE.md);
the design and the rules behind it: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
(in Russian); the conventions agents and people follow here: [AGENTS.md](AGENTS.md).

## Crates

```text
 liftoffd ──► liftoff-rest, liftoff-store, liftoff-host, liftoff-proxy, liftoff-wasm ──► liftoff-core
 liftoffd ──► liftoff-mcp (mounts its Streamable HTTP transport at /mcp beside liftoff-rest)
 liftoffd, liftoff ──► liftoff-tunnel (no workspace crate of its own)
 liftoff-rest ──► liftoff-api ◄── liftoff-mcp ◄── liftoff (CLI; depends on liftoff-core for local mode, never on the server adapters)
 modules/* (wasm32-wasip2 components) ──► wit/liftoff-module.wit ◄── liftoff-wasm
```

## Development

```sh
just setup                            # mise install + lefthook install
just ci                               # exactly what CI runs
just modules                          # build every module for wasm32-wasip2
just functions                        # build the contract functions (Rust, TS with bun, Python with uv)
just serve                            # liftoffd with pretty logs
```

## Layout

| Path               | Crate            | Role                                                                 |
|--------------------|------------------|----------------------------------------------------------------------|
| `crates/core`      | `liftoff-core`   | Domain, use-case services, port traits, hooks, the master-key cipher; fakes and `World` under `test-support`. No I/O. |
| `crates/api`       | `liftoff-api`    | API request/response types shared by the daemon and the CLI.        |
| `crates/rest`      | `liftoff-rest`   | axum router, auth middleware, problem+json, `/openapi.json`; the login page, the gate, the artifact viewer `/a/`, `/.well-known/oauth-protected-resource`, the panel's CORS. |
| `crates/store`     | `liftoff-store`  | SQLite implementation of `Store`; `import_v1`; `FileBlobs`, the artifact files on disk. |
| `crates/host`      | `liftoff-host`   | `Docker`, `Systemd` and compose project files on the host; `Resolver` over public DNS. |
| `crates/proxy`     | `liftoff-proxy`  | `Proxy` through the Caddy admin API.                                 |
| `crates/wasm`      | `liftoff-wasm`   | wasmtime host of the modules; `ModuleSource` over the module registry; the host contract (`test-support`); the function runtime (`functions`). |
| `crates/mcp`       | `liftoff-mcp`    | MCP server: the API's operations, the modules' tools and the artifact aliases as MCP tools; stdio behind `lof mcp`, Streamable HTTP as a tower service the daemon mounts at `/mcp`. |
| `crates/tunnel`    | `liftoff-tunnel` | The tunnel of `lof expose`: control frames and `yamux` over a WebSocket, the daemon's server and the CLI's client. |
| `wit/`             |                  | The module world `liftoff:module@0.1.0`, the function world `liftoff:function@0.1.0` (`wit/function/`) and the WASI WIT they import. |
| `modules/`         |                  | The modules (`oidc`, `tenants`, `dns.cloudflare`, `git.forgejo`, `git.github`, `git.gitlab`, `mcp`, `web`) and the host's contract components (`testing/`, the contract functions in `testing/functions/`), a workspace of its own for `wasm32-wasip2`; `registry/` signs releases. |
| `templates/`       |                  | The sources of the templates liftoff ships (`fn-ts`, `fn-rust`, `fn-python`), published as repositories of their own. |
| `crates/liftoffd`  | `liftoffd`       | Daemon binary: config, `setup`, `lf_system`, deploy queue, reconcile, the function and tunnel listeners, `/mcp`, the artifact sweeper, `import-artifacts`, tracing, shutdown. |
| `crates/liftoff`   | `liftoff`        | CLI binary (`lof`): clap, HTTP client, two modes, `.liftoff`, `~/.config/liftoff/config.toml`; local mode (`src/local/`): the core's services over local adapters, its own module host, `git`. |
| `config/`          |                  | `default.toml` (documented defaults), `local.toml` (dev).            |
| `scripts/`         |                  | `install.sh`, release packaging and publishing, systemd unit, sudoers. |
| `docs/`            |                  | Architecture and the guides (modules, templates, functions, dev, artifacts, MCP). |

## Quality gates

| Gate                        | Tool                                  | Where                           |
|-----------------------------|---------------------------------------|---------------------------------|
| Formatting                  | rustfmt, taplo, typos                 | pre-commit, CI                  |
| Lints (deny warnings)       | clippy: all + pedantic + nursery + cargo + ~60 restriction lints; rustc `unsafe_code = forbid`, `missing_docs`, `unreachable_pub` | pre-commit, CI |
| Unused deps                 | cargo-machete                         | pre-commit, CI                  |
| Feature matrix              | cargo-hack                            | pre-push, CI                    |
| Tests + doctests            | cargo-nextest, `cargo test --doc`     | pre-push, CI                    |
| Coverage                    | cargo-llvm-cov                        | CI artifact                     |
| Advisories, licenses, bans  | cargo-deny, rustsec/audit-check       | pre-push, CI, daily             |
| Secrets                     | gitleaks                              | pre-commit, CI                  |
| Commit messages             | committed (Conventional Commits)      | commit-msg                      |
| Workflows / scripts / image | actionlint, zizmor, shellcheck, hadolint | pre-commit, CI               |
| Release                     | cargo-auditable binaries, `lof` archive, `install.sh`, signed `SHA256SUMS`, CycloneDX SBOM, distroless image with provenance | tag `v*` |

Overrides use `#[expect(lint, reason = "...")]`, never `allow`; an `expect`
that stops firing is itself an error, so suppressions cannot rot.

## Performance defaults

- Release: `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`, symbols stripped,
  `overflow-checks = true` (correctness over the last few percent).
- `profile.profiling` keeps line tables for `perf`/`samply`.
- Ports return `impl Future`, adapters are generic: static dispatch, no boxing.
- Static musl binary in a distroless image; nothing else in the container.
- Linux link flags: full RELRO, `-z now`, non-executable stack.

## Security defaults

- Listeners bind loopback unless configured; TLS/auth/rate-limits belong to the
  ingress, documented in [SECURITY.md](SECURITY.md).
- REST: body limit, timeout, panic guard, request ids, redacted auth headers,
  `nosniff` / CSP / `no-store`; errors never carry internal detail.
- Inputs are parsed into validated types at the boundary (`ProjectName`, `Host`, ...).
- `unsafe` is forbidden; OpenSSL and a list of unmaintained crates are banned.

## Documentation

- [docs/REFERENCE.md](docs/REFERENCE.md) — every command, the installer, local mode, modules, configuration.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — design, decisions, assumptions.
- [docs/MODULES.md](docs/MODULES.md) — how to write, test and release a module.
- [docs/TEMPLATES.md](docs/TEMPLATES.md) — how to make a template for `lof create`.
- [docs/FUNCTIONS.md](docs/FUNCTIONS.md) — functions: the world, languages, build, deploy, limits.
- [docs/EXPOSE.md](docs/EXPOSE.md) — `lof expose`, the address, private and share, the tunnel.
- [docs/ARTIFACTS.md](docs/ARTIFACTS.md) — artifacts: versions, share and TTL, the sandbox, the viewer, CLI, MCP, the import.
- [docs/MCP.md](docs/MCP.md) — MCP over stdio and HTTP, OAuth, the tools and the aliases.
- [docs/ADDING_AN_INTERFACE.md](docs/ADDING_AN_INTERFACE.md) — step-by-step recipe.
- [SECURITY.md](SECURITY.md) — threat model boundaries and reporting.
- `cargo doc --open` — API docs; every public item is documented (enforced).
