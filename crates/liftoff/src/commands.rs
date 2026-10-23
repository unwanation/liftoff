//! Command dispatch and what every command shares ([`Ctx`]): the chosen
//! server, the questions of interactive mode and the list of what is
//! missing in batch mode, the project and environment, and the output
//! mode.
//!
//! A command first gathers everything it needs (flags, answers, the
//! project from `git remote`), then calls [`Ctx::finish`], which ends a
//! batch run with exit 64 and the list of what is missing, and only then
//! talks to the server. So both modes send the same requests, and a
//! question the user gives up on sends nothing.

mod admin;
mod artifact;
mod auth;
mod create;
mod deploys;
mod dev;
mod doctor;
mod domains;
mod envs;
mod functions;
mod git;
mod mcp;
pub(crate) mod module;
mod plan;
mod secrets;
mod servers;
mod templates;
mod tenant;

use std::io::Write as _;
use std::str::FromStr;

use liftoff_api::{ProjectResolve, ProjectView};
use reqwest::Method;
use serde::de::DeserializeOwned;

use crate::args::{AdminCommand, Args, Command, ServerCommand, TenantCommand};
use crate::client::{Client, Reply};
use crate::config::{Chosen, Config};
use crate::error::CliError;
use crate::local::{Home, LocalMode};
use crate::mode::Mode;
use crate::project::{DEFAULT_ENV, Dotfile, Repository};
use crate::{Env, Sleeper};

/// The flags every command takes.
#[derive(Debug, Clone, Default)]
pub(crate) struct Flags {
    /// `--server`.
    pub(crate) server: Option<String>,
    /// `--project`.
    pub(crate) project: Option<String>,
    /// `--env`.
    pub(crate) env: Option<String>,
}

/// The project a command is about, before the server is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectRef {
    /// `--project` or an answer.
    Named(String),
    /// `git remote get-url origin`, resolved by the server.
    Repo(Repository),
}

/// One run of a command.
pub(crate) struct Ctx<'a, S> {
    pub(crate) env: &'a mut Env<S>,
    pub(crate) mode: Mode,
    json: bool,
    flags: Flags,
    dotfile: Dotfile,
    /// The config file; `None` when there is none.
    pub(crate) config: Option<Config>,
    /// The server of this run.
    pub(crate) chosen: Option<Chosen>,
    missing: Vec<String>,
}

/// Runs the command of `args`.
pub(crate) async fn dispatch<S: Sleeper>(
    args: Args,
    env: &mut Env<S>,
    mode: Mode,
) -> Result<(), CliError> {
    let flags = Flags {
        server: args.server,
        project: args.project,
        env: args.env,
    };
    let mut ctx = Ctx {
        env,
        mode,
        json: args.json,
        flags,
        dotfile: Dotfile::default(),
        config: None,
        chosen: None,
        missing: Vec::new(),
    };
    match args.command {
        Command::Login { url, token } => return auth::login(&mut ctx, url, token).await,
        Command::Doctor => return doctor::run(&mut ctx).await,
        // Runs on the server's host and needs no session.
        Command::Server {
            command: ServerCommand::Upgrade,
        } => return servers::upgrade(&mut ctx),
        _ => {}
    }
    // `server ls|use` repair a config whose default names no server.
    let lenient = matches!(
        args.command,
        Command::Server {
            command: ServerCommand::Ls | ServerCommand::Use { .. }
        }
    );
    ctx.load(lenient)?;
    match args.command {
        Command::Login { .. }
        | Command::Doctor
        | Command::Server {
            command: ServerCommand::Upgrade,
        } => Ok(()),
        Command::Logout => auth::logout(&mut ctx).await,
        Command::Whoami => auth::whoami(&mut ctx).await,
        Command::Server { command } => servers::run(&mut ctx, command).await,
        Command::Git { command } => git::run(&mut ctx, command).await,
        Command::Create(create) => create::run(&mut ctx, create).await,
        Command::Templates { group } => templates::run(&mut ctx, group).await,
        Command::Dev(dev) => dev::run(&mut ctx, dev).await,
        Command::Tenant {
            command: TenantCommand::External(words),
        } => crate::dynamic::run(&mut ctx, tenant::below_tenant(words)).await,
        Command::Tenant {
            command: TenantCommand::Use { slug },
        } => tenant::use_tenant(&mut ctx, slug),
        Command::Deploy(deploy) => deploys::deploy(&mut ctx, deploy).await,
        Command::Redeploy { to, wait } => deploys::redeploy(&mut ctx, to, wait).await,
        Command::Status => envs::status(&mut ctx).await,
        Command::Deploys { command } => deploys::history(&mut ctx, command).await,
        Command::Logs(logs) => deploys::logs(&mut ctx, logs).await,
        Command::Env { command } => envs::run(&mut ctx, command).await,
        Command::Domain { command } => domains::run(&mut ctx, command).await,
        Command::Bind { service_port, host } => envs::bind(&mut ctx, service_port, host).await,
        Command::Unbind { host } => envs::unbind(&mut ctx, host).await,
        Command::Secret { command } => secrets::run(&mut ctx, command).await,
        Command::Artifact { command } => artifact::run(&mut ctx, command).await,
        Command::Token { command } => deploys::token(&mut ctx, command).await,
        Command::Admin {
            command: AdminCommand::External(words),
        } => crate::dynamic::run(&mut ctx, admin::below_admin(words)).await,
        Command::Admin { command } => admin::run(&mut ctx, command).await,
        Command::Module { command } => module::run(&mut ctx, command).await,
        Command::Mcp => mcp::run(ctx.chosen.as_ref(), ctx.env.cancel.clone()).await,
        Command::External(words) => crate::dynamic::run(&mut ctx, words).await,
    }
}

/// Percent-encodes one path segment.
pub(crate) fn segment(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Decodes a response body into `T`.
pub(crate) fn parse<T: DeserializeOwned>(body: &str) -> Result<T, CliError> {
    serde_json::from_str(body).map_err(|error| CliError::Unexpected(error.to_string()))
}

/// A body as printed by `--json`: as received, with a final newline when
/// it has none; nothing for an empty body.
pub(crate) fn raw(body: &str) -> String {
    if body.is_empty() || body.ends_with('\n') {
        body.to_owned()
    } else {
        format!("{body}\n")
    }
}

/// `/projects/<p>/environments/<e>`.
pub(crate) fn env_path(project: &str, env: &str) -> String {
    format!(
        "/projects/{}/environments/{}",
        segment(project),
        segment(env)
    )
}

/// The name of the project: as named, or as the server resolves the
/// repository.
pub(crate) async fn resolve(client: &Client, project: ProjectRef) -> Result<String, CliError> {
    match project {
        ProjectRef::Named(name) => Ok(name),
        ProjectRef::Repo(repo) => {
            let body = ProjectResolve {
                git_host: repo.host,
                repo: repo.repo,
            };
            let reply = client
                .send_json(Method::POST, "/projects/-/resolve", &[], &body)
                .await?;
            Ok(parse::<ProjectView>(&reply.body)?.name)
        }
    }
}

impl<S: Sleeper> Ctx<'_, S> {
    /// Reads `.liftoff` and the config, and picks the server: `--server`,
    /// else `.liftoff`, else `default_server`. `lenient` leaves the server
    /// unchosen instead of failing on a name the config does not have.
    fn load(&mut self, lenient: bool) -> Result<(), CliError> {
        self.dotfile = Dotfile::find(&self.env.cwd)?;
        self.config = Config::read(&self.env.config_path)?;
        let wanted = self
            .flags
            .server
            .clone()
            .or_else(|| self.dotfile.server.clone());
        self.chosen = match (&self.config, lenient) {
            (Some(config), false) => config.choose(wanted.as_deref())?,
            (Some(config), true) => config.choose(wanted.as_deref()).ok().flatten(),
            // A server named without any config is a config error, never
            // local mode (LCL-08).
            (None, false) => match wanted {
                Some(name) => {
                    return Err(CliError::Config(format!(
                        "no server named {name:?}: there is no config at {}; log in to it with `liftoff login <url> --token <t>`",
                        self.env.config_path.display()
                    )));
                }
                None => None,
            },
            (None, true) => None,
        };
        Ok(())
    }

    /// Whether this run is in local mode: no server is chosen.
    pub(crate) const fn is_local(&self) -> bool {
        self.chosen.is_none()
    }

    /// The config home, where local mode keeps its files.
    pub(crate) fn home(&self) -> Home {
        Home::of(&self.env.config_path)
    }

    /// The core services of local mode, with every installed module
    /// loaded.
    pub(crate) fn local_mode(
        &self,
    ) -> impl Future<Output = Result<LocalMode, CliError>> + Send + 'static {
        let home = self.home();
        let local = self.env.local.clone();
        async move { LocalMode::open(&home, &local).await }
    }

    /// A client for the chosen server with its session.
    pub(crate) fn client(&self) -> Result<Client, CliError> {
        let chosen = self.chosen.as_ref().ok_or(CliError::NotLoggedIn)?;
        let token = chosen.server.token.clone().ok_or(CliError::NotLoggedIn)?;
        Client::new(&chosen.server.url, Some(token), self.env.cancel.clone())
    }

    /// Persists `config`.
    pub(crate) fn save_config(&self, config: &Config) -> Result<(), CliError> {
        config.save(&self.env.config_path)
    }

    // --- Gathering ---------------------------------------------------------

    /// `given`, else the answer to `question` on a terminal. In batch mode a
    /// missing value is recorded as `label` and stands in as empty until
    /// [`Self::finish`] reports it.
    pub(crate) fn text(
        &mut self,
        given: Option<String>,
        label: &str,
        question: &str,
    ) -> Result<String, CliError> {
        self.answer(given, label, question, false)
    }

    /// Like [`Self::text`], without echoing the answer.
    pub(crate) fn secret(
        &mut self,
        given: Option<String>,
        label: &str,
        question: &str,
    ) -> Result<String, CliError> {
        self.answer(given, label, question, true)
    }

    fn answer(
        &mut self,
        given: Option<String>,
        label: &str,
        question: &str,
        hidden: bool,
    ) -> Result<String, CliError> {
        if let Some(value) = given {
            return Ok(value);
        }
        if self.mode == Mode::Batch {
            self.missing.push(label.to_owned());
            return Ok(String::new());
        }
        let answer = if hidden {
            self.env.prompter.password(question)?
        } else {
            self.env.prompter.text(question)?
        };
        let answer = if hidden {
            answer
        } else {
            answer.trim().to_owned()
        };
        if answer.is_empty() {
            return Err(CliError::Usage(format!("{label} is required")));
        }
        Ok(answer)
    }

    /// A question the person may leave empty on a terminal: the trimmed
    /// answer, `None` when it is empty. In batch mode nothing is asked and
    /// nothing is missing: the caller's default stands.
    pub(crate) fn optional_text(&mut self, question: &str) -> Result<Option<String>, CliError> {
        if self.mode == Mode::Batch {
            return Ok(None);
        }
        let answer = self.env.prompter.text(question)?;
        let answer = answer.trim();
        Ok((!answer.is_empty()).then(|| answer.to_owned()))
    }

    /// A value that parses from text, given or asked like [`Self::text`].
    pub(crate) fn parsed<T: FromStr + Default>(
        &mut self,
        given: Option<T>,
        label: &str,
        question: &str,
    ) -> Result<T, CliError> {
        if let Some(value) = given {
            return Ok(value);
        }
        let text = self.text(None, label, question)?;
        if self.mode == Mode::Batch {
            return Ok(T::default());
        }
        text.parse()
            .map_err(|_unparsed| CliError::Usage(format!("{label}: not a valid value: {text:?}")))
    }

    /// One of `options`, given or picked from a list.
    pub(crate) fn pick<T: Copy + Default>(
        &mut self,
        given: Option<T>,
        label: &str,
        question: &str,
        options: &[(&str, T)],
    ) -> Result<T, CliError> {
        if let Some(value) = given {
            return Ok(value);
        }
        if self.mode == Mode::Batch {
            self.missing.push(label.to_owned());
            return Ok(T::default());
        }
        let names: Vec<String> = options.iter().map(|(name, _)| (*name).to_owned()).collect();
        let index = self.env.prompter.select(question, &names)?;
        options
            .get(index)
            .map(|(_, value)| *value)
            .ok_or_else(|| CliError::Local(format!("no option {index}")))
    }

    /// `--yes`, else a yes on a terminal; a no ends the run.
    pub(crate) fn confirm(
        &mut self,
        given: bool,
        label: &str,
        question: &str,
    ) -> Result<(), CliError> {
        if given {
            return Ok(());
        }
        if self.mode == Mode::Batch {
            self.missing.push(label.to_owned());
            return Ok(());
        }
        if self.env.prompter.confirm(question)? {
            Ok(())
        } else {
            Err(CliError::Aborted(
                "not confirmed; nothing was changed".to_owned(),
            ))
        }
    }

    /// The global `--project`, if given.
    pub(crate) fn project_flag(&self) -> Option<String> {
        self.flags.project.clone()
    }

    /// The project: `--project`, else the `origin` of the working tree,
    /// else asked (or missing as `--project`).
    pub(crate) fn want_project(&mut self) -> Result<ProjectRef, CliError> {
        if let Some(name) = self.flags.project.clone() {
            return Ok(ProjectRef::Named(name));
        }
        if let Some(repo) = self.repository() {
            return Ok(ProjectRef::Repo(repo));
        }
        let name = self.text(None, "--project", "Project")?;
        Ok(ProjectRef::Named(name))
    }

    /// The forge repository of the working tree, if its `origin` names one.
    pub(crate) fn repository(&self) -> Option<Repository> {
        let url = self.env.git.origin(&self.env.cwd)?;
        Repository::parse(&url)
    }

    /// Ends a batch run that lacks something with the list of it.
    pub(crate) fn finish(&mut self) -> Result<(), CliError> {
        if self.missing.is_empty() {
            Ok(())
        } else {
            Err(CliError::Missing(std::mem::take(&mut self.missing)))
        }
    }

    // --- Talking to the server --------------------------------------------

    /// The environment: `--env`, else `.liftoff`, else `prod`.
    pub(crate) fn env_name(&self) -> String {
        self.flags
            .env
            .clone()
            .or_else(|| self.dotfile.env.clone())
            .unwrap_or_else(|| DEFAULT_ENV.to_owned())
    }

    // --- Output ------------------------------------------------------------

    /// Turns `--json` on (a module command takes it after its name).
    pub(crate) const fn set_json(&mut self) {
        self.json = true;
    }

    /// Whether `--json` is on.
    pub(crate) const fn json(&self) -> bool {
        self.json
    }

    /// Writes `text` to stdout.
    pub(crate) fn say(&mut self, text: &str) -> Result<(), CliError> {
        self.env.out.write_all(text.as_bytes())?;
        self.env.out.flush()?;
        Ok(())
    }

    /// Writes `text` to stderr: what the person must do while a command
    /// runs (stdout is the result's).
    pub(crate) fn tell(&mut self, text: &str) -> Result<(), CliError> {
        self.env.err.write_all(text.as_bytes())?;
        self.env.err.flush()?;
        Ok(())
    }

    /// `--json`: the body as received. Otherwise whatever `human` makes of it.
    pub(crate) fn show(
        &mut self,
        reply: &Reply,
        human: impl FnOnce(&str) -> Result<String, CliError>,
    ) -> Result<(), CliError> {
        let text = if self.json {
            raw(&reply.body)
        } else {
            human(&reply.body)?
        };
        self.say(&text)
    }

    /// An answer of local mode: with `--json` `value` as the API would
    /// have sent it, otherwise what `human` makes of it.
    pub(crate) fn say_json_or<T: serde::Serialize>(
        &mut self,
        value: &T,
        human: impl FnOnce(&T) -> String,
    ) -> Result<(), CliError> {
        let text = if self.json {
            let text = serde_json::to_string(value)
                .map_err(|error| CliError::Local(format!("cannot encode the answer: {error}")))?;
            format!("{text}\n")
        } else {
            human(value)
        };
        self.say(&text)
    }

    /// `--json`: the body as received. Otherwise `message`.
    pub(crate) fn ack(&mut self, reply: &Reply, message: &str) -> Result<(), CliError> {
        self.show(reply, |_| Ok(format!("{message}\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod raw {
        use super::*;

        // TC: CLIX-03
        #[test]
        fn keeps_the_bytes_when_the_body_ends_in_newlines() {
            assert_eq!(raw("{}\n\n"), "{}\n\n");
        }

        // TC: CLIX-03
        #[test]
        fn prints_nothing_when_the_body_is_empty() {
            assert_eq!(raw(""), "");
        }
    }

    mod segment {
        use super::*;

        #[test]
        fn escapes_a_slash_when_encoded() {
            assert_eq!(segment("a/b c"), "a%2Fb%20c");
        }
    }
}
