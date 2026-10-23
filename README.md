# liftoff

Deploy-as-a-service for a single server. A project lives in a
repository on Forgejo, GitHub or GitLab and has environments of three
kinds:

- **compose**: CI pushes an image and calls `POST /api/v1/deploys`, and
  the `liftoffd` daemon runs `docker compose pull && up`, routes the
  environment's domains through Caddy and keeps a deploy history for
  redeploy and rollback;
- **function**: a WebAssembly `wasi:http` component the daemon itself
  runs, deployed from CI or by `lof deploy`, no container and no port;
- **tunnel**: the dev address of `lof dev`, a tunnel from the daemon to
  the developer's local port.

Around them: scoped and versioned secrets, managed and external domains,
private addresses with one-time login links, `lof create` that makes a
project from a template in one command, `lof dev` that runs it on the
machine, signed WebAssembly modules that add login (`oidc`), teams and
quotas (`tenants`), DNS (`dns.cloudflare`), forges (`git.*`), MCP over
HTTP (`mcp`) and the panel's origin (`web`), artifacts (HTML pages with
versions that Claude or a person publishes, private or shared) and MCP
for agents over stdio and HTTP. The CLI talks to the daemon's API and,
without a server, runs the same core services on the machine (local
mode).

Changes: [CHANGELOG.md](CHANGELOG.md). Design and the rules behind it:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

```text
 liftoffd ──► liftoff-rest, liftoff-store, liftoff-host, liftoff-proxy, liftoff-wasm ──► liftoff-core
 liftoffd ──► liftoff-mcp (mounts its Streamable HTTP transport at /mcp beside liftoff-rest)
 liftoffd, liftoff ──► liftoff-tunnel (no workspace crate of its own)
 liftoff-rest ──► liftoff-api ◄── liftoff-mcp ◄── liftoff (CLI; depends on liftoff-core for local mode, never on the server adapters)
 modules/* (wasm32-wasip2 components) ──► wit/liftoff-module.wit ◄── liftoff-wasm
```

## Installing a server

On a Linux host with systemd and Docker:

```sh
curl -fsSL https://github.com/unwanation/liftoff/releases/latest/download/install.sh | sudo sh
sudo liftoffd setup
```

`install.sh` downloads `liftoffd` and `liftoff` for the machine, verifies
the release's `SHA256SUMS-<target>.txt` against its ed25519 signature
(the public key pinned as `RELEASE_KEY` in the script, or
`LIFTOFF_RELEASE_KEY`) and the binaries against the sums, installs them in
`/usr/local/bin` with the `lof` symlink, and refuses anything unsigned or
mismatched. `LIFTOFF_VERSION` picks a tag instead of the latest release;
`LIFTOFF_FORGE` and `LIFTOFF_REPO` point it at a Forgejo or GitHub
repository of your own (the defaults are the releases of this one).

`liftoffd setup` asks for the base domain, the public API URL and the TLS
mode (`acme` with an e-mail, or `edge` behind another proxy on a loopback
port); without a terminal every answer is a flag (`--base-domain`,
`--public-url`, `--public-ip`, `--tls`, `--acme-email`, `--edge-port`,
`--yes`). It creates the `liftoff` user, `/etc/liftoff` (config and
`master.key`) and `/var/lib/liftoff`, the systemd unit and sudoers rule,
starts Caddy as the compose project `lf_system` with its admin API on
loopback, and prints a single-use admin token and the DNS records to add
(`A *.<base> → IP`). `--import <v1 database>` (with `--import-issuer` and
`--import-git-host`) brings a liftoff v1 installation over: users,
projects, environments, domains, bindings, deploy tokens, recent deploys
and their env as secrets.

`lof server upgrade`, run on the server, installs the latest release with
the same `install.sh` and restarts the unit.

## Using the CLI

The CLI is one binary, `liftoff`; the installer adds the `lof` symlink
and the docs use `lof`.

```sh
lof login https://liftoff.example.com --token <token>   # single-user mode
lof git connect forgejo --host git.example.com --token <forge token>
cd my-repo && lof create        # the project of this repository + liftoff.toml
lof token snippet               # the CI workflow that deploys
lof secret set DATABASE_URL=…   # environment scope by default
lof deploy --tag <tag> --wait
lof domain add shop && lof bind web:8080 shop.apps.example.com
lof status && lof logs --follow
```

- **Single-user mode.** Without the `oidc` module there is no identity
  provider: `lof login <url> --token <t>` trades the token printed by
  `liftoffd setup` for a `cli` session (30 days, extended while used); the
  token works once. Another machine gets its own token from `lof admin
  token`. With `oidc` installed, `lof login <url>` without `--token` runs
  the provider's device flow.
- **Two modes.** With a terminal, a command asks for what is missing,
  offers lists and prints tables. Without one, with `--no-input` or with
  every flag given, it never asks: a missing required argument exits with
  code 64 and names what is missing. `--json` prints API responses as
  received.
- **Which project.** The project comes from the `git remote` of the
  current directory, resolved by the server; `--project` and `--env`
  override it. An optional, uncommitted `.liftoff` picks the server
  (when the config knows several, `lof server ls|use`) and the default
  environment (`prod` otherwise). The CLI config is
  `~/.config/liftoff/config.toml`.
- **Local mode.** Without a server the CLI works on the machine (below).

## From template to URL

With a server (`lof login` done) and the forge connected on it:

```sh
lof git connect github --token <PAT>          # or gitlab, or forgejo --host git.example.com
lof templates                                 # the catalogue, by group
lof create blog --template github.com/unwanation/liftoff-fn-ts   # on a terminal plain `lof create` asks
```

`lof create` has the forge generate `blog` from the template
(private, under your account unless `--public`/`--owner`), clones it into
`./blog`, replaces `{{project}}` and `{{domain}}` in the template's
`[substitute]` files, asks the template's `[secrets]` (`--secret K=V`;
`generate` ones are made silently), commits the CI workflow for the forge
(`.forgejo/workflows/deploy.yml`, `.github/workflows/deploy.yml` or
`.gitlab-ci.yml`) and pushes; the clone and the push use this machine's
git access (a git credential, or a forge connection of local mode). The
server then has the project, the
environment `prod` on the default branch, the domain `blog.<base>` (or
`--domain`), the route to `[service] expose`, the deploy token and the
secrets, with the CI secrets sent to the repository; `.liftoff` names the
server and is listed in `.gitignore`. The CLI prints the URL: it answers
once the CI run of that push has deployed. `--json` prints the plan and
the result; `--tenant` picks the tenant with the `tenants` module (`lof
tenant use <slug>` sets the default).

In a repository that already exists, `lof create` without `--template`
reads its `liftoff.toml`, or proposes one from the files (stack
detection: `package.json` and its lockfile, `Cargo.toml`,
`pyproject.toml`, `go.mod`, a compose file, `wit/`), confirms it and
commits it with the same plan. `lof dev` runs the project on the machine:
`[dev] command` with `PORT` (a free port of 4000–4999) and the
environment's secrets in its environment, `http://localhost:<PORT>`
printed; Vite, Astro, SvelteKit and Expo get `--port N`.

Making a template (the manifest, substitutions, secrets, the CI workflow):
[docs/TEMPLATES.md](docs/TEMPLATES.md). The templates liftoff ships are in
`templates/`: the functions `fn-ts`, `fn-rust` and `fn-python` (below),
published as `github.com/unwanation/liftoff-fn-<lang>`.

## Functions and dev addresses

A function is an HTTP handler compiled to a WebAssembly component of
`wasi:http/proxy@0.2` (plus `wasi:config` for its secrets and
`wasi:keyvalue` for a key-value store); the daemon runs it, no container
and no port per project:

```sh
lof create hello --template github.com/unwanation/liftoff-fn-ts   # or liftoff-fn-rust, liftoff-fn-python
cd hello && lof secret set GREETING=hi        # reaches the function through wasi:config
lof deploy                                    # [function] build, check, upload, wait
curl https://hello.<base>/                    # {"greeting":"hi","visits":1}
lof logs -f                                   # its stdout and stderr
```

`lof deploy` runs `[function] build` of `liftoff.toml`, checks that
`[function] component` exports `wasi:http/incoming-handler@0.2` and
imports only `wasi:` (at most 50 MiB), uploads it, and waits while the
daemon precompiles it, warms one instance up and switches the route;
the CI workflow `lof create` committed does the same on every push. A
function is open to everyone until `lof env unshare`. Languages, limits
and cold starts: [docs/FUNCTIONS.md](docs/FUNCTIONS.md).

`lof dev` runs the project on the machine and, with a server, gives it a
dev address `https://<project>-dev.<base>` (`<project>-<login>` with the
`tenants` module) through a tunnel to the local port:

```sh
lof dev                   # prints http://localhost:<PORT>, the dev address and a one-time login link
lof dev --share           # the dev address without a login
lof env share --env hello-dev   # the same outside dev; `unshare` puts the login back
```

The dev address is private: its owner and the admin get in, a browser
by the printed link (one use, 10 minutes, a 24-hour session). A dropped
connection reconnects; 30 seconds without it the address answers `503`
«dev offline», and Ctrl-C takes it down at once. Details:
[docs/DEV.md](docs/DEV.md).

## Artifacts

An artifact is an HTML page with its files, kept with versions and shown
at a stable address:

```sh
lof artifact publish report.html --files chart.js,img/logo.png   # prints the URL and a one-time login link
lof artifact update <id> report.html                             # a new version, same URL
lof artifact share <id>                                          # anyone with the link, no TTL
lof artifact versions <id> && lof artifact rollback <id> --to 1
lof artifact ls && lof artifact rm <id>
```

A new artifact is private: its owner and the admin see it after signing
in (the printed link, or `lof artifact open <id>` for a fresh one), and
it is deleted `[artifacts] ttl` (5 hours) after its latest version unless
it is shared. `https://<api>/a/<id>` (or `https://<[artifacts] host>/<id>`)
shows a top bar with the title, the version switcher and the owner's
share button around the version in a sandboxed iframe; the HTML runs in
an opaque origin (`Content-Security-Policy: sandbox …`), where scripts
and relative files work but `fetch()` of its own files and
`localStorage` do not. Without modules only the admin publishes; with
`tenants` every user publishes their own. `liftoffd import-artifacts
<dir>` brings over the data directory of the earlier standalone artifacts
server (`artifacts.db` and `artifacts/<id>/`). Details:
[docs/ARTIFACTS.md](docs/ARTIFACTS.md).

## MCP

The API is an MCP server for agents: every REST operation is a tool
`liftoff_<operation>`, the installed modules' tools join as
`liftoff_<module>_<tool>`, the artifact tools also answer to the short
names of the earlier artifacts server (`publish_artifact`,
`update_artifact`, `share_artifact`, `unshare_artifact`,
`list_artifacts`, `list_artifact_versions`, `delete_artifact`) with its
arguments, and logs are resources.

```sh
claude mcp add --transport stdio lof -- lof mcp                         # stdio, the CLI's session
claude mcp add --transport http liftoff https://liftoff.example.com/mcp  # HTTP, OAuth through oidc
```

Over HTTP the daemon serves Streamable HTTP at `/mcp` once the modules
`oidc` and `mcp` are installed (`lof module install mcp --set
resource=https://<api>/mcp --set allowed_audiences=<aud>`): a request
without a token gets `401` pointing at
`/.well-known/oauth-protected-resource`, the client signs in at the
`oidc` provider, and the tools act as the token's user. Details:
[docs/MCP.md](docs/MCP.md).

## Local mode

Without a server in the config (and no `.liftoff` or `--server` naming
one) the CLI runs the core's services on the machine, over its own
adapters in `~/.config/liftoff/` (`0700`; `LIFTOFF_CONFIG` or
`XDG_CONFIG_HOME` move it):

```sh
lof git connect github --token <PAT>          # installs the git.github module locally
lof templates                                 # the catalogue of [local] templates
lof create blog --template github.com/unwanation/liftoff-fn-ts   # generate, clone, {{project}}, secrets, commit, push
cd blog && lof dev                            # run it, secrets from the local store
lof secret set API_KEY=… --scope project      # set|get|ls|rm|pull|history|rollback
lof login https://liftoff.example.com --token <t> && lof git connect github --token <PAT>
lof server connect https://liftoff.example.com   # the project and its secrets move to the server
lof secret push --move                        # the rest of the local secrets
lof secret sync                               # the `--ci` secrets to the forge's CI
```

- The catalogue is the CLI config's `[local] templates` (`templates =
  ["github.com/unwanation/liftoff-fn-ts"]`, each `[host/]owner/name`);
  with a server it is added to the server's `[templates] repos`.
- `secrets.db` keeps the local secrets, the forge connections and the
  installed modules, every secret sealed with `key` (`0600`; `lof doctor`
  warns when it is wider). The `git.*` modules come from the module
  registry, checked as by the daemon, into `modules/` and run in the
  CLI's own module host (their tables in `modules.db`).
- Local `create` stops after the push: `{{domain}}`, the CI workflow, the
  environment, the domain and `.liftoff` come with `lof server connect`,
  which needs the work tree on the default branch without uncommitted
  changes to tracked files and commits only the files its plan wrote.
  Deploys, domains, artifacts, tunnels and MCP need a server.

## Modules

Modules are signed WebAssembly components the daemon loads without a
restart: routes under `/api/v1/m/<module>/`, CLI commands, MCP tools and
subscriptions to the core's hooks. The admin installs them from the
registry, the releases of
[unwanation/liftoff-modules](https://github.com/unwanation/liftoff-modules)
(`[modules] registry` in the daemon config; a Forgejo or GitHub
repository of your own works the same):

```sh
lof module ls --available                     # what the registry offers this server
lof module install oidc --set issuer=https://id.example.com \
  --set client_id=@client_id.txt              # k=@file reads the value from a file
lof module install tenants                    # requires oidc
lof module install dns.cloudflare --set zone_id=… --set api_token=@cf.token --set ip=203.0.113.10
lof module install mcp --set resource=https://liftoff.example.com/mcp --set allowed_audiences=<aud>   # requires oidc
lof module install web --set origin=https://app.example.com   # the panel liftoff-web: CORS, SameSite=None
lof module ls                                 # installed, enabled, error
lof module config dns.cloudflare --set ip=203.0.113.11
lof module enable <name>                      # after a module was disabled by failures
lof module remove <name> [--purge]            # --purge drops its tables and keys too
```

- The daemon picks the newest release whose WIT world and `liftoffd_min`
  fit it, refuses a missing dependency, checks the component's SHA-256
  and minisign signature against the key pinned in `liftoffd`, and
  validates the configuration against the module's schema; fields the
  schema marks secret are sealed with the master key. Without `--set`
  the CLI asks for the required fields on a terminal and lists them as
  `--set <field>` otherwise (exit code 64).
- The registry's minisign public key is pinned in the binaries
  (`REGISTRY_PUBLIC_KEY` in `crates/core/src/module/registry_remote.rs`,
  shared by the daemon and the CLI); a registry of your own means a
  build with your key.
- `--set` types a value by the schema: an integer, a number or a boolean
  is parsed, an array or an object field takes JSON
  (`--set 'hosts=["git.example.org"]'`), and an array of strings also
  `a,b` (`--set hosts=git.example.org,code.example.org`).
- A module's commands appear as `lof` commands (`lof tenant create`,
  `lof protect --env prod`) and its tools in `lof mcp` and `/mcp` as
  `liftoff_<module>_<tool>`. A module that fails three calls in a row is
  disabled (`lof module ls` shows the error) and the daemon runs without
  it until `lof module enable`.

Writing a module: [docs/MODULES.md](docs/MODULES.md).

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
| `crates/tunnel`    | `liftoff-tunnel` | The tunnel of `lof dev`: control frames and `yamux` over a WebSocket, the daemon's server and the CLI's client. |
| `wit/`             |                  | The module world `liftoff:module@0.1.0`, the function world `liftoff:function@0.1.0` (`wit/function/`) and the WASI WIT they import. |
| `modules/`         |                  | The modules (`oidc`, `tenants`, `dns.cloudflare`, `git.forgejo`, `git.github`, `git.gitlab`, `mcp`, `web`) and the host's contract components (`testing/`, the contract functions in `testing/functions/`), a workspace of its own for `wasm32-wasip2`; `registry/` signs releases. |
| `templates/`       |                  | The sources of the templates liftoff ships (`fn-ts`, `fn-rust`, `fn-python`), published as repositories of their own. |
| `crates/liftoffd`  | `liftoffd`       | Daemon binary: config, `setup`, `lf_system`, deploy queue, reconcile, the function and tunnel listeners, `/mcp`, the artifact sweeper, `import-artifacts`, tracing, shutdown. |
| `crates/liftoff`   | `liftoff`        | CLI binary (`lof`): clap, HTTP client, two modes, `.liftoff`, `~/.config/liftoff/config.toml`; local mode (`src/local/`): the core's services over local adapters, its own module host, `git`. |
| `config/`          |                  | `default.toml` (documented defaults), `local.toml` (dev).            |
| `scripts/`         |                  | `install.sh`, release packaging and publishing, systemd unit, sudoers. |
| `docs/`            |                  | Architecture and the guides (modules, templates, functions, dev, artifacts, MCP). |

## Configuration

Layers, later wins: compiled defaults → `--config file.toml` → `LIFTOFF_*` env.
Env keys use `__` between sections: `LIFTOFF_API__BIND=127.0.0.1:7100`,
`LIFTOFF_LOG__LEVEL=debug`. Unknown keys are errors. `RUST_LOG` overrides the log
filter. See `config/default.toml` for every key.

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

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — design, decisions, assumptions.
- [docs/MODULES.md](docs/MODULES.md) — how to write, test and release a module.
- [docs/TEMPLATES.md](docs/TEMPLATES.md) — how to make a template for `lof create`.
- [docs/FUNCTIONS.md](docs/FUNCTIONS.md) — functions: the world, languages, build, deploy, limits.
- [docs/DEV.md](docs/DEV.md) — `lof dev`, the dev address, private and share, the tunnel.
- [docs/ARTIFACTS.md](docs/ARTIFACTS.md) — artifacts: versions, share and TTL, the sandbox, the viewer, CLI, MCP, the import.
- [docs/MCP.md](docs/MCP.md) — MCP over stdio and HTTP, OAuth, the tools and the aliases.
- [docs/ADDING_AN_INTERFACE.md](docs/ADDING_AN_INTERFACE.md) — step-by-step recipe.
- [SECURITY.md](SECURITY.md) — threat model boundaries and reporting.
- `cargo doc --open` — API docs; every public item is documented (enforced).

## License

Dual-licensed under MIT or Apache-2.0 at your option
([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
