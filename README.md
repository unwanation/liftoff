# liftoff

Your own deploy platform on one server. Push to your repository and the
app is live at its own HTTPS address, with no Kubernetes, no cloud account
and no YAML to write by hand.

liftoff runs on any Linux machine with Docker: a VPS, a home server, a
spare box. It works with your repositories on GitHub, GitLab or Forgejo,
and you drive it from the terminal with the `lof` command (or let an AI
agent drive it for you).

## What you can do with it

- **Start a project in one command.** `lof create blog` makes a
  repository from a template, sets up CI, a domain and secrets, and pushes.
  A minute later `https://blog.your-domain.com` answers.
- **Deploy on every push.** Each push builds the app in your forge's CI
  and liftoff rolls it out. Every deploy is kept, so going back to the
  previous version is one command.
- **Run any app in containers.** Anything with a Dockerfile or a compose
  file: a website, an API, a bot, a database next to it.
- **Run small functions without containers.** Write an HTTP handler in
  TypeScript, Rust or Python. liftoff runs it as WebAssembly: it starts in
  milliseconds and gets its own address and a small key-value store.
- **Keep secrets in one place.** API keys and passwords are stored
  encrypted, with a history of every change. Each environment (production,
  staging, your own) has its own values. Your app gets them as environment
  variables, and so does your laptop when you run it locally.
- **Show your work-in-progress.** `lof dev` runs the app on your machine
  and gives it a private public address through a tunnel. Send the link to
  a colleague or open it on your phone.
- **Use your own domains.** Every project gets a subdomain with HTTPS
  automatically. Add your own domains too, and with Cloudflare liftoff
  creates the DNS records itself.
- **Keep things private.** Put any environment behind a login so only
  your team gets in.
- **Publish pages and reports.** Share an HTML page (a report, a chart, a
  prototype) at a stable link. Pages keep their versions and are private
  until you share them. They can also disappear on their own after a few
  hours.
- **Let AI agents deploy.** liftoff speaks MCP, so Claude and other
  agents can create projects, deploy, read logs and publish pages for you.
- **Work as a team.** With single sign-on (any OpenID Connect provider),
  people log in with their usual account. Teams, roles and quotas decide
  who can do what.
- **Start without a server.** The CLI works on its own, too: create
  projects from templates, run them locally and keep their secrets. When
  you get a server, move everything over with one command.

## Get started

**1. Install the server** on a Linux machine with Docker and systemd:

```sh
curl -fsSL https://github.com/unwanation/liftoff/releases/latest/download/install.sh | sudo sh
sudo liftoffd setup
```

Setup asks for your domain and prints the DNS records to add (a wildcard
`*.your-domain.com` pointing at the server) and a one-time login token.

**2. Log in** from your machine with `lof` (the installer puts it next to
the server; on another Linux machine, install it with
`LIFTOFF_BIN_DIR=~/.local/bin` in front of `sh`):

```sh
lof login https://liftoff.your-domain.com --token <token from setup>
lof git connect github --token <personal access token>
```

**3. Create and ship a project:**

```sh
lof create hello --template github.com/unwanation/liftoff-fn-ts
cd hello
# edit src/index.ts, then
git commit -am "say hello" && git push     # CI deploys it
lof logs -f                                # watch it run
```

Or bring a repository you already have: run `lof create` inside it. It
looks at your files, proposes a configuration and wires up the deploys.

## Everyday commands

```sh
lof status                     # what runs where, and at which version
lof deploys                    # the deploy history
lof redeploy --to 41           # back to an earlier deploy
lof deploy --wait              # deploy a function now instead of waiting for CI
lof secret set API_KEY=…       # add or change a secret
lof domain add shop            # a new address for the project
lof dev                        # run locally with a shareable address
lof artifact publish page.html # publish a page, get a link
```

Every command explains itself with `--help`. In a terminal, `lof` asks
for whatever is missing. In scripts and CI, it takes flags and never asks.

## Extend it

Features beyond the core (single sign-on, teams, Cloudflare DNS, GitHub,
GitLab and Forgejo integration, MCP over HTTP, a web panel) come as
signed modules you install and remove without restarting anything:

```sh
lof module ls --available
lof module install dns.cloudflare
```

Templates are ordinary repositories with a `liftoff.toml`, so your team
can have its own. See [docs/TEMPLATES.md](docs/TEMPLATES.md).

## Learn more

- [docs/REFERENCE.md](docs/REFERENCE.md): every command, the installer,
  local mode, modules and configuration in detail.
- Guides: [functions](docs/FUNCTIONS.md), [`lof dev` and tunnels](docs/DEV.md),
  [pages (artifacts)](docs/ARTIFACTS.md), [MCP for agents](docs/MCP.md),
  [templates](docs/TEMPLATES.md), [writing a module](docs/MODULES.md).
- [CHANGELOG.md](CHANGELOG.md): what changed in each release.
- [SECURITY.md](SECURITY.md): the security model and how to report a
  vulnerability.
- [CONTRIBUTING.md](CONTRIBUTING.md): building liftoff, the layout of the
  code and its checks.

## License

liftoff is free and open source under your choice of the
[MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE) license.
