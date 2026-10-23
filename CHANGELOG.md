# Changelog

Notable changes per release. Versions follow [SemVer](https://semver.org/);
the format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [2.0.3] - 2026-10-23

### Changed

- Development, releases and the module registry moved to GitHub:
  `github.com/unwanation/liftoff` and `unwanation/liftoff-modules`.
  `install.sh`, `lof server upgrade`, the daemon's `[modules] registry`
  and local mode default to them; `LIFTOFF_FORGE` points `install.sh` at
  a Forgejo or Gitea mirror.
- The default template catalogue is `github.com/unwanation/liftoff-fn-ts`.

## [2.0.2] - 2026-10-23

### Fixed

- The protected-resource metadata of `/mcp`
  (`/.well-known/oauth-protected-resource`) now carries
  `scopes_supported`: the scopes the module `oidc` is configured with
  (its defaults `openid profile email urn:zitadel:iam:org:project:roles`
  when unset) plus `offline_access`. An MCP client asks the provider for
  exactly these (RFC 9728); without the field Claude Code sent no
  `scope` at all, which Zitadel refuses with "The scope of your request
  is missing".

## [2.0.1] - 2026-10-23

### Fixed

- The module host's outgoing HTTPS (`wasi:http` of every module: the forge
  checks of a deploy, Cloudflare, Zitadel) panicked in the daemon with
  "Could not automatically determine the process-level CryptoProvider":
  the binary links two rustls providers (ring through the tunnel and the
  module host, aws-lc-rs through reqwest) and the client configuration
  relied on the process default. It now names ring, as the tunnel client
  does.

## [2.0.0] - 2026-10-23

liftoff v2: a rewrite of the v1 daemon around a core without I/O, a
module host and a CLI that also works without a server. Upgrading a v1
installation: `liftoffd setup --import <v1 database>`.

### Added

- **Core and daemon.** Users and sessions (`cli`, `browser`, `mcp`),
  projects (one per repository) with environments of kind `compose`,
  `function` and `tunnel`, managed domains under the base domain and
  external domains verified over public DNS, bindings, scoped and
  versioned secrets (environment, project, tenant; `ci` ones go to the
  forge), compose deploys from CI (`POST /api/v1/deploys`) with history,
  `redeploy` and rollback, a deploy queue and a reconcile of the host's
  state on start. Caddy is driven as a whole through its admin API
  (compose project `lf_system`; `tls = "acme"` or `"edge"` behind another
  proxy). Every owner check asks the `authorize` hook first.
- **`liftoffd setup`** on a fresh host: user, directories, master key,
  config, systemd unit, sudoers rule, the Caddy of `lf_system`, a
  single-use admin token; `--import` brings a v1 installation over.
  `install.sh` installs the signed release binaries with the `lof`
  symlink; `lof server upgrade` runs it.
- **Modules.** Signed WebAssembly components (`wasm32-wasip2`, the world
  `liftoff:module@0.1.0`) the daemon loads without a restart: routes,
  CLI commands, MCP tools, hooks and `core-call`s, tables of their own.
  A registry (the releases of `<owner>/liftoff-modules`, an `index.json`
  plus minisign signatures checked against a key pinned in the
  binaries), `lof module ls|install|config|enable|remove`. The modules
  `oidc` (login through an OpenID Connect provider, browser and device
  flow, `verify_token` for access tokens), `tenants` (tenants, members,
  roles, quotas, `lof protect`), `dns.cloudflare`, `git.forgejo`,
  `git.github`, `git.gitlab` (forge connections, image checks, CI
  secrets, repository generation), `mcp` and `web`.
- **Templates and `lof create`.** A template is a repository with a
  `liftoff.toml`; `lof create <name> --template <repo>` generates the
  repository on the forge, clones it, substitutes `{{project}}` and
  `{{domain}}`, asks the template's secrets, commits the CI workflow for
  the forge and pushes; in an existing repository it reads or proposes
  `liftoff.toml` (stack detection). `lof templates` lists the catalogue
  (`[templates] repos` of the server plus `[local] templates`). The
  function templates `fn-ts`, `fn-rust` and `fn-python` ship in
  `templates/`.
- **Local mode.** Without a server the CLI runs the core's services on
  the machine: `lof git connect`, `lof templates`, `lof create`, `lof
  dev` and `lof secret` over local stores and its own module host; `lof
  server connect` moves a project to a server.
- **Functions.** An environment of kind `function` is a `wasi:http`
  component the daemon runs (wasmtime, precompiled, a warm pool,
  `wasi:config` for secrets, `wasi:keyvalue` for state); `lof deploy`
  builds, checks, uploads and waits; the CI workflow does the same.
- **Tunnels and `lof dev`.** `lof dev` runs `[dev] command` with `PORT`
  and the environment's secrets and, with a server, opens a tunnel
  (control frames and `yamux` over a WebSocket, `liftoff-tunnel`) to a
  dev address `https://<project>-dev.<base>`, private with a one-time
  login link; `--share` and `lof env share|unshare` open and close it.
- **Artifacts.** HTML pages with files and versions, private with a TTL
  until shared, a viewer with a version switcher around a sandboxed
  iframe, `lof artifact *`, REST, and `liftoffd import-artifacts <dir>`
  for the data of the earlier standalone artifacts server.
- **MCP.** `lof mcp` serves the API as tools over stdio; with the modules
  `oidc` and `mcp` the daemon serves Streamable HTTP at `/mcp` with
  OAuth (RFC 9728 metadata, access tokens verified by `oidc`). The
  artifact tools also answer to the short names of the earlier artifacts
  server.
- **Docs.** `docs/ARCHITECTURE.md`, `MODULES.md`, `TEMPLATES.md`,
  `FUNCTIONS.md`, `DEV.md`, `ARTIFACTS.md`, `MCP.md`,
  `ADDING_AN_INTERFACE.md`.

### Changed

- Tenant and target of v1 become project and environment; tenants,
  OIDC login, Cloudflare and the forge left the core for modules.
- Caddy is configured through its admin API instead of files on disk.
- The forge is reached with each user's own connection (`lof git
  connect`), not a server-wide token.

### Modules

| Module | Version | What |
|---|---|---|
| `oidc` | 0.2.0 | Login through an OpenID Connect provider; `verify_token` for `/mcp` |
| `tenants` | 0.2.0 | Tenants, members, roles, quotas, the SSO gate; everyone may publish artifacts |
| `dns.cloudflare` | 0.1.0 | A records of managed domains in Cloudflare and their proxy flag |
| `git.forgejo` | 0.1.0 | Forgejo as a git provider |
| `git.github` | 0.1.0 | GitHub as a git provider (device flow, `ghcr.io` images, sealed Actions secrets) |
| `git.gitlab` | 0.1.0 | GitLab as a git provider (device flow, templates by fork) |
| `mcp` | 0.1.0 | The protected resource `/mcp`: `resource`, `allowed_audiences`, `client_id` |
| `web` | 0.1.0 | The panel's origin: CORS, cookie policy, page redirects |

## [0.2.0] and earlier

liftoff v1: a single-tenant deploy daemon for compose projects with
Forgejo, Zitadel and Cloudflare built in. v2 imports its database.
