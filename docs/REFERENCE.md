# liftoff reference

Everything the [README](../README.md) leaves out: what each command does,
what the installer and `liftoffd setup` write, local mode and the modules.
The design and the rules behind it are in [ARCHITECTURE.md](ARCHITECTURE.md)
(in Russian), changes in [CHANGELOG.md](../CHANGELOG.md).

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
commits it with the same plan. To run it on the machine, `lof secret
pull --write` writes the environment's secrets into `.env.<env>`
(`0600`), and `lof expose <PORT>` puts the running app on the network
(below).

Making a template (the manifest, substitutions, secrets, the CI workflow):
[docs/TEMPLATES.md](TEMPLATES.md). The templates liftoff ships are in
`templates/`: the functions `fn-ts`, `fn-rust` and `fn-python` (below),
published as `github.com/unwanation/liftoff-fn-<lang>`.

## Functions and `lof expose`

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
and cold starts: [docs/FUNCTIONS.md](FUNCTIONS.md).

`lof expose <PORT>` puts a port of the machine on the network through
the server, through a tunnel to the local port; no project is needed.
On the server's own machine the address is
`https://<port>.localhost.<base>`, anywhere else
`https://<login>-<id>.localhost.<base>` (stable for a user and port).
The app's own WebSocket (Vite's or webpack's hot reload) crosses the
tunnel, so a change shows in the browser without a reload:

```sh
npm run dev               # the app on localhost:3000, as always
lof expose 3000           # prints the address and a one-time login link
lof expose 3000 --share   # the address without a login
```

The address is private: its owner and the admin get in, through the
single sign-on of the `oidc` module or else by the printed link (one
use, 10 minutes, a 24-hour session). A dropped connection reconnects;
30 seconds without it the address answers `503` «dev offline», and
Ctrl-C takes it down at once. Without the `dns.*` module the server
needs the DNS record `*.localhost.<base>`. Details:
[docs/EXPOSE.md](EXPOSE.md).

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
[docs/ARTIFACTS.md](ARTIFACTS.md).

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
[docs/MCP.md](MCP.md).

## Local mode

Without a server in the config (and no `.liftoff` or `--server` naming
one) the CLI runs the core's services on the machine, over its own
adapters in `~/.config/liftoff/` (`0700`; `LIFTOFF_CONFIG` or
`XDG_CONFIG_HOME` move it):

```sh
lof git connect github --token <PAT>          # installs the git.github module locally
lof templates                                 # the catalogue of [local] templates
lof create blog --template github.com/unwanation/liftoff-fn-ts   # generate, clone, {{project}}, secrets, commit, push
cd blog && lof secret pull --write            # its secrets from the local store into .env.prod
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

Writing a module: [docs/MODULES.md](MODULES.md).

## Configuration

Layers, later wins: compiled defaults → `--config file.toml` → `LIFTOFF_*` env.
Env keys use `__` between sections: `LIFTOFF_API__BIND=127.0.0.1:7100`,
`LIFTOFF_LOG__LEVEL=debug`. Unknown keys are errors. `RUST_LOG` overrides the log
filter. See `config/default.toml` for every key.
