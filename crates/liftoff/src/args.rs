//! The command line, as clap derives (the commands).
//!
//! Every argument a command needs is optional to clap: what is missing is
//! asked for on a terminal and listed (exit 64) in batch mode, so clap's
//! own "required" check would take interactive mode away.

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

/// `liftoff` (`lof`): deploy to a liftoff server.
#[derive(Debug, Parser)]
#[command(name = "liftoff", version, about, long_about = None, propagate_version = true)]
pub(crate) struct Args {
    /// Print API responses as received instead of tables.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    /// Never ask: a missing argument ends the run with exit code 64.
    #[arg(long, global = true)]
    pub(crate) no_input: bool,
    /// The server of the CLI config to use.
    #[arg(long, global = true, value_name = "name")]
    pub(crate) server: Option<String>,
    /// The project, instead of the one `git remote` names.
    #[arg(long, global = true, value_name = "name")]
    pub(crate) project: Option<String>,
    /// The environment, instead of `.liftoff` or `prod`.
    #[arg(long, global = true, value_name = "name")]
    pub(crate) env: Option<String>,
    /// What to do.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Every command.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Log in and save the server: through the identity provider when the
    /// server has the `oidc` module, else with a single-use token.
    Login {
        /// Base URL of the server.
        #[arg(value_name = "url")]
        url: Option<String>,
        /// The token from `liftoffd setup` or `lof admin token` (skips the
        /// identity provider).
        #[arg(long, value_name = "t")]
        token: Option<String>,
    },
    /// End this session.
    Logout,
    /// Show who you are logged in as.
    Whoami,
    /// The servers this CLI knows.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Forge connections.
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
    /// Create a project: from a template, or of this repository.
    Create(CreateArgs),
    /// The template catalogue `create` offers.
    Templates {
        /// Only this group: `web`, `api`, `function` or `service`.
        #[arg(long, value_name = "G")]
        group: Option<String>,
    },
    /// Expose a local port on the network through the server:
    /// `https://<port>.localhost.<base>` on the server itself, else
    /// `https://<login>-<id>.localhost.<base>`; private until `--share`.
    Expose(ExposeArgs),
    /// Tenants: `use` is the CLI's own, the `tenants` module adds the
    /// rest (`create`, `ls`, `rm`, `members`).
    Tenant {
        #[command(subcommand)]
        command: TenantCommand,
    },
    /// Deploy an image tag to the environment.
    Deploy(DeployArgs),
    /// Deploy the current (or an older) deploy again.
    Redeploy {
        /// Number of the deploy to copy.
        #[arg(long, value_name = "N")]
        to: Option<u32>,
        /// Wait until the deploy succeeded or failed.
        #[arg(long)]
        wait: bool,
    },
    /// Show the environment.
    Status,
    /// The deploys of the environment.
    Deploys {
        #[command(subcommand)]
        command: Option<DeploysCommand>,
    },
    /// Logs of the environment, or of one deploy.
    Logs(LogsArgs),
    /// Environments of the project.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Domains.
    Domain {
        #[command(subcommand)]
        command: DomainCommand,
    },
    /// Route a domain to a service of the environment, or to its function.
    Bind {
        /// `<service>:<port>` inside the compose project; for a function
        /// environment, the domain alone (`lof bind <host>`).
        #[arg(value_name = "service:port")]
        service_port: Option<String>,
        /// A domain of the project.
        #[arg(value_name = "host")]
        host: Option<String>,
    },
    /// Remove the route of a domain.
    Unbind {
        /// The domain.
        #[arg(value_name = "host")]
        host: Option<String>,
    },
    /// Secrets of the environment, the project or the tenant.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Artifacts: HTML pages with their files, private with a TTL until
    /// shared.
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
    /// The deploy token of the project.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Administration (admins only).
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Modules of the server.
    Module {
        #[command(subcommand)]
        command: ModuleCommand,
    },
    /// Check the config, the server and the session.
    Doctor,
    /// Serve the API as MCP tools over stdio with the session of the
    /// config (`claude mcp add --transport stdio lof -- lof mcp`).
    Mcp,
    /// A command of an installed module: `<module> <command> [--<arg>
    /// V]...`, built from `GET /modules` (`crate::dynamic`).
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// `server …`
#[derive(Debug, Subcommand)]
pub(crate) enum ServerCommand {
    /// Move local-mode state to a server.
    Connect {
        /// Base URL of the server.
        #[arg(value_name = "url")]
        url: Option<String>,
    },
    /// List the servers of the config.
    Ls,
    /// Make a server the default.
    Use {
        /// Its name in the config.
        #[arg(value_name = "name")]
        name: Option<String>,
    },
    /// Upgrade the server.
    Upgrade,
    /// Whether the server is up and ready.
    Status,
}

/// A forge `git connect` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub(crate) enum Provider {
    /// GitHub (module `git.github`).
    Github,
    /// GitLab (module `git.gitlab`).
    Gitlab,
    /// Forgejo.
    #[default]
    Forgejo,
}

impl Provider {
    /// The wire name.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Forgejo => "forgejo",
        }
    }
}

/// `git …`
#[derive(Debug, Subcommand)]
pub(crate) enum GitCommand {
    /// Connect a forge account with an access token.
    Connect {
        /// The forge.
        #[arg(value_name = "github|gitlab|forgejo")]
        provider: Option<Provider>,
        /// The forge host, e.g. `git.example.com`.
        #[arg(long, value_name = "H")]
        host: Option<String>,
        /// Your access token on that forge.
        #[arg(long, value_name = "T")]
        token: Option<String>,
    },
    /// List the connections.
    Ls,
    /// Forget a connection.
    Disconnect {
        /// The forge host.
        #[arg(value_name = "host")]
        host: Option<String>,
    },
}

/// `create`
#[derive(Debug, ClapArgs)]
pub(crate) struct CreateArgs {
    /// Project name; the repository name by default.
    #[arg(value_name = "name")]
    pub(crate) name: Option<String>,
    /// The compose service that serves HTTP.
    #[arg(long, value_name = "S")]
    pub(crate) service: Option<String>,
    /// The port it listens on inside its container.
    #[arg(long, value_name = "P")]
    pub(crate) port: Option<u16>,
    /// The tenant the project goes to (with the `tenants` module); your
    /// only tenant by default.
    #[arg(long, value_name = "SLUG")]
    pub(crate) tenant: Option<String>,
    /// The template repository, `[host/]owner/name`.
    #[arg(long, value_name = "owner/repo")]
    pub(crate) template: Option<String>,
    /// Offer only the templates of this group: `web`, `api`, `function`
    /// or `service`.
    #[arg(long, value_name = "G")]
    pub(crate) group: Option<String>,
    /// The domain instead of `<name>.<base>`.
    #[arg(long, value_name = "host")]
    pub(crate) domain: Option<String>,
    /// A value of the template's `[secrets]`, `K=V` (repeatable).
    #[arg(long = "secret", value_name = "K=V")]
    pub(crate) secrets: Vec<String>,
    /// Generate a private repository (the default).
    #[arg(long, conflicts_with = "public")]
    pub(crate) private: bool,
    /// Generate a public repository.
    #[arg(long)]
    pub(crate) public: bool,
    /// The user or organisation the repository is generated under; your
    /// forge account by default.
    #[arg(long, value_name = "org")]
    pub(crate) owner: Option<String>,
}

/// `expose`.
#[derive(Debug, ClapArgs)]
pub(crate) struct ExposeArgs {
    /// The port of the app on this machine, e.g. 3000.
    #[arg(value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub(crate) port: u16,
    /// Open the address to everyone, without login.
    #[arg(long)]
    pub(crate) share: bool,
}

/// `tenant …`: `use` is built in (it writes the CLI config, which a
/// module's command cannot); every other word is a command of the
/// `tenants` module (`crate::dynamic`).
#[derive(Debug, Subcommand)]
pub(crate) enum TenantCommand {
    /// Make a tenant the default of `create` on the current server.
    Use {
        /// The tenant's slug.
        #[arg(value_name = "slug")]
        slug: Option<String>,
    },
    /// A command of the `tenants` module below `tenant` (`tenant
    /// create`, `tenant members add`).
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// `deploy`
#[derive(Debug, ClapArgs)]
pub(crate) struct DeployArgs {
    /// The image tag.
    #[arg(long, value_name = "T")]
    pub(crate) tag: Option<String>,
    /// A compose file to deploy with.
    #[arg(long, value_name = "F")]
    pub(crate) compose: Option<String>,
    /// Wait until the deploy succeeded or failed.
    #[arg(long)]
    pub(crate) wait: bool,
}

/// `deploys …`
#[derive(Debug, Subcommand)]
pub(crate) enum DeploysCommand {
    /// The deploys of the environment (the default).
    Ls,
    /// One deploy.
    Show {
        /// ULID of the deploy.
        #[arg(value_name = "id")]
        id: Option<String>,
    },
    /// Wait until a deploy succeeded or failed.
    Wait {
        /// ULID of the deploy.
        #[arg(value_name = "id")]
        id: Option<String>,
    },
}

/// `logs`
#[derive(Debug, ClapArgs)]
pub(crate) struct LogsArgs {
    /// One compose service.
    #[arg(value_name = "service")]
    pub(crate) service: Option<String>,
    /// Keep streaming.
    #[arg(short, long)]
    pub(crate) follow: bool,
    /// The last lines only.
    #[arg(long, value_name = "N")]
    pub(crate) tail: Option<u32>,
    /// The stored log of deploy number N instead.
    #[arg(long, value_name = "N")]
    pub(crate) deploy: Option<u32>,
}

/// `env …`
#[derive(Debug, Subcommand)]
pub(crate) enum EnvCommand {
    /// Add an environment.
    Add {
        /// Its name.
        #[arg(value_name = "name")]
        name: Option<String>,
        /// The branch CI deploys from.
        #[arg(long, value_name = "b")]
        branch: Option<String>,
        /// `compose` (the default).
        #[arg(long, value_name = "kind")]
        kind: Option<String>,
    },
    /// List the environments.
    Ls,
    /// Remove an environment and everything it runs.
    Rm {
        /// Its name.
        #[arg(value_name = "name")]
        name: Option<String>,
        /// Confirm the removal.
        #[arg(long)]
        yes: bool,
    },
    /// Stop the environment.
    Stop,
    /// Start the environment.
    Start,
    /// Open the environment to everyone (no login in front of it).
    Share,
    /// Put the login in front of the environment again.
    Unshare,
}

/// `domain …`
#[derive(Debug, Subcommand)]
pub(crate) enum DomainCommand {
    /// Add a label under the base domain, or an external host.
    Add {
        /// The label, or the host with `--external`.
        #[arg(value_name = "label|host")]
        name: Option<String>,
        /// A host outside the base domain, verified by CNAME or A.
        #[arg(long)]
        external: bool,
        /// The DNS module proxies the record from the start (needs
        /// `dns.cloudflare`; managed domains only).
        #[arg(long)]
        proxied: bool,
    },
    /// List the domains.
    Ls,
    /// Change a managed domain: whether the DNS module proxies it.
    Set {
        /// The domain.
        #[arg(value_name = "host")]
        host: Option<String>,
        /// `--proxied` or `--proxied=false` (needs `dns.cloudflare`).
        #[arg(
            long,
            value_name = "bool",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true"
        )]
        proxied: Option<bool>,
    },
    /// Remove a domain.
    Rm {
        /// The domain.
        #[arg(value_name = "host")]
        host: Option<String>,
    },
}

/// Where a secret lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub(crate) enum Scope {
    /// The environment (the default).
    #[default]
    Environment,
    /// The project: every environment.
    Project,
    /// The tenant of the project: every environment of its projects
    /// (module `tenants`).
    Tenant,
}

/// `secret …`
#[derive(Debug, Subcommand)]
pub(crate) enum SecretCommand {
    /// Write new versions.
    Set {
        /// `KEY=value` pairs.
        #[arg(value_name = "K=V")]
        pairs: Vec<String>,
        /// The scope to write to.
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
        /// Send it to the forge's CI too (project scope only: with `--scope project`).
        #[arg(long)]
        ci: bool,
    },
    /// Keys, scopes and flags, without values.
    Ls {
        /// The scope to list.
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// The value of a key.
    Get {
        /// The key.
        #[arg(value_name = "K")]
        key: Option<String>,
        /// The scope to read.
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Remove a key with its history.
    Rm {
        /// The key.
        #[arg(value_name = "K")]
        key: Option<String>,
        /// The scope to remove from.
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Print the `.env` the environment runs with.
    Pull {
        /// Write it to `.env.<env>` at the top of the working tree
        /// (owner-only) instead of printing it.
        #[arg(long)]
        write: bool,
    },
    /// Copy the secrets of local mode to the server.
    Push {
        /// Delete them locally once the server has them.
        #[arg(long = "move")]
        move_: bool,
    },
    /// Send the project's `ci` secrets to its repository's CI.
    Sync,
    /// The versions of a key.
    History {
        /// The key.
        #[arg(value_name = "K")]
        key: Option<String>,
        /// The scope to read.
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
    /// Make an older version the current one.
    Rollback {
        /// The key.
        #[arg(value_name = "K")]
        key: Option<String>,
        /// The version to go back to.
        #[arg(long, value_name = "N")]
        to: Option<u32>,
        /// The scope of the key.
        #[arg(long, value_enum, default_value_t)]
        scope: Scope,
    },
}

/// `artifact …`.
///
/// A file of `--files` is `path` or `name=path`, read relative to the
/// current directory. A bare `path` inside the current directory is named
/// in the artifact by its path from there (`img/logo.png`); one outside
/// it by its base name (`../shared/logo.png` is `logo.png`); `name=path`
/// names it `name`.
#[derive(Debug, Subcommand)]
pub(crate) enum ArtifactCommand {
    /// Publish an HTML page with its files as a new private artifact:
    /// prints its address and a one-time link that logs a browser in.
    Publish {
        /// The HTML file.
        #[arg(value_name = "html")]
        html: Option<String>,
        /// The files next to the page, comma-separated or repeated:
        /// `path` (named by its path from the current directory, or by
        /// its base name when it lies outside it) or `name=path`.
        #[arg(long, value_name = "a,b", value_delimiter = ',')]
        files: Vec<String>,
        /// The title (default: the page's `<title>`).
        #[arg(long, value_name = "T")]
        title: Option<String>,
    },
    /// Publish a new version (the page and its files as a whole).
    Update {
        /// The artifact.
        #[arg(value_name = "id")]
        id: Option<String>,
        /// The HTML file.
        #[arg(value_name = "html")]
        html: Option<String>,
        /// The files next to the page, as for `publish`.
        #[arg(long, value_name = "a,b", value_delimiter = ',')]
        files: Vec<String>,
        /// The title (default: the page's `<title>`, else the previous
        /// version's).
        #[arg(long, value_name = "T")]
        title: Option<String>,
    },
    /// Open an artifact to anybody, for good.
    Share {
        /// The artifact.
        #[arg(value_name = "id")]
        id: Option<String>,
    },
    /// Make an artifact private again, with a TTL from now.
    Unshare {
        /// The artifact.
        #[arg(value_name = "id")]
        id: Option<String>,
    },
    /// Print a fresh one-time link that logs a browser in to an artifact.
    Open {
        /// The artifact.
        #[arg(value_name = "id")]
        id: Option<String>,
        /// Also open the link in the browser of this machine.
        #[arg(long)]
        browser: bool,
    },
    /// Your artifacts.
    Ls {
        /// Every user's (admins only).
        #[arg(long)]
        all: bool,
    },
    /// The versions of an artifact.
    Versions {
        /// The artifact.
        #[arg(value_name = "id")]
        id: Option<String>,
    },
    /// Copy an older version as the new current one.
    Rollback {
        /// The artifact.
        #[arg(value_name = "id")]
        id: Option<String>,
        /// The version to copy.
        #[arg(long, value_name = "N")]
        to: Option<u32>,
    },
    /// Delete an artifact, or one version of it.
    #[command(disable_version_flag = true)]
    Rm {
        /// The artifact.
        #[arg(value_name = "id")]
        id: Option<String>,
        /// Only this version (not the current one).
        #[arg(long, value_name = "N")]
        version: Option<u32>,
    },
}

/// `token …`
#[derive(Debug, Subcommand)]
pub(crate) enum TokenCommand {
    /// Replace the deploy token of the project.
    Rotate,
    /// The CI step that deploys with it.
    Snippet,
}

/// `module …`.
#[derive(Debug, Subcommand)]
pub(crate) enum ModuleCommand {
    /// The installed modules, or with `--available` the registry's.
    Ls {
        /// The modules of the registry this server can run.
        #[arg(long)]
        available: bool,
    },
    /// Install a module without restarting the server.
    #[command(disable_version_flag = true)]
    Install {
        /// The module.
        #[arg(value_name = "name")]
        name: Option<String>,
        /// The release (default: the newest one the server can run).
        #[arg(long, value_name = "v")]
        version: Option<String>,
        /// A configuration field: `k=v`, or `k=@file` for its content.
        #[arg(long = "set", value_name = "k=v")]
        set: Vec<String>,
    },
    /// Remove a module; its tables stay unless `--purge`.
    Remove {
        /// The module.
        #[arg(value_name = "name")]
        name: Option<String>,
        /// Drop its tables and keys too.
        #[arg(long)]
        purge: bool,
    },
    /// Enable a module again after a fix.
    Enable {
        /// The module.
        #[arg(value_name = "name")]
        name: Option<String>,
    },
    /// Change the configuration of a module.
    Config {
        /// The module.
        #[arg(value_name = "name")]
        name: Option<String>,
        /// A configuration field: `k=v`, or `k=@file` for its content.
        #[arg(long = "set", value_name = "k=v")]
        set: Vec<String>,
    },
}

/// `admin …`
#[derive(Debug, Subcommand)]
pub(crate) enum AdminCommand {
    /// A single-use login token for another machine.
    Token,
    /// Every user.
    Users,
    /// Sessions.
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// Bring every environment back to its recorded state.
    Reconcile,
    /// Environment limits.
    Env {
        #[command(subcommand)]
        command: AdminEnvCommand,
    },
    /// A command of an installed module below `admin` (`admin tenant
    /// set`; `crate::dynamic`).
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// `admin sessions …`
#[derive(Debug, Subcommand)]
pub(crate) enum SessionsCommand {
    /// End every session of a user.
    Revoke {
        /// The user.
        #[arg(value_name = "login")]
        login: Option<String>,
    },
}

/// `admin env …`
#[derive(Debug, Subcommand)]
pub(crate) enum AdminEnvCommand {
    /// Change the CPU and memory allowance.
    Set {
        /// `<project>/<environment>`.
        #[arg(value_name = "project/env")]
        target: Option<String>,
        /// CPU allowance.
        #[arg(long, value_name = "X")]
        cpus: Option<f64>,
        /// Memory allowance in MiB.
        #[arg(long, value_name = "M")]
        memory: Option<u32>,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use clap::error::ErrorKind;

    use super::Args;

    /// The sources whose messages point the user to a command.
    const SOURCES: &[(&str, &str)] = &[
        ("error.rs", include_str!("error.rs")),
        ("config.rs", include_str!("config.rs")),
        ("client.rs", include_str!("client.rs")),
        ("commands.rs", include_str!("commands.rs")),
        ("commands/auth.rs", include_str!("commands/auth.rs")),
        ("commands/servers.rs", include_str!("commands/servers.rs")),
        ("commands/doctor.rs", include_str!("commands/doctor.rs")),
        ("commands/create.rs", include_str!("commands/create.rs")),
        (
            "commands/templates.rs",
            include_str!("commands/templates.rs"),
        ),
        ("commands/tenant.rs", include_str!("commands/tenant.rs")),
        ("commands/admin.rs", include_str!("commands/admin.rs")),
        ("project.rs", include_str!("project.rs")),
    ];

    /// Every `` `liftoff …` `` or `` `lof …` `` quoted in `text`.
    fn hints(text: &str) -> Vec<String> {
        text.split('`')
            .skip(1)
            .step_by(2)
            .filter(|quoted| quoted.starts_with("liftoff ") || quoted.starts_with("lof "))
            .map(str::to_owned)
            .collect()
    }

    mod hints {
        use super::*;

        // TC: CLIX-06 (every hint of the CLI names a subcommand
        // that exists)
        #[test]
        fn name_existing_subcommands_when_parsed() {
            let mut checked = 0;
            for (file, text) in SOURCES {
                for hint in hints(text) {
                    // `<url>`-style placeholders stand for any value.
                    let words = hint.split_whitespace().map(|word| {
                        if word.starts_with('<') {
                            "x".to_owned()
                        } else {
                            word.to_owned()
                        }
                    });
                    if let Err(error) = Args::try_parse_from(words) {
                        assert!(
                            !matches!(
                                error.kind(),
                                ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument
                            ),
                            "{file}: `{hint}`: {error}"
                        );
                    }
                    checked += 1;
                }
            }
            assert!(checked > 0, "the CLI points to `liftoff login` somewhere");
        }
    }
}
