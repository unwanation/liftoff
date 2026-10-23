//! `liftoffd setup`: installs liftoff on a host.
//!
//! Every answer comes from a flag ([`SetupArgs`]) or, on a terminal, from
//! a question ([`Prompter`]); without a terminal a missing answer ends the
//! run with exit 64 and the list of the flags that are missing. With
//! `--yes` an answer that has a default takes it without asking
//! (`--public-url` defaults to `https://<base-domain>`, `--public-ip` to
//! the address of the host's default route).
//!
//! What it does, relative to [`SetupEnv::root`] (`/` in production):
//!
//! - user `liftoff` in group `docker`;
//! - `/etc/liftoff/liftoffd.toml` and `/etc/liftoff/master.key`
//!   (64 hex digits, `0600`; kept on `--force`, it seals every secret);
//! - `/var/lib/liftoff` (`0750`) with the database (`0600`), the owner
//!   (`local` admin) and its single-use setup token; with `--import
//!   <file>` the v1 database is imported first
//!   ([`liftoff_store::import_v1`]), its env files opened with
//!   the master key;
//! - `/etc/systemd/system/liftoffd.service` (equal to
//!   `scripts/liftoffd.service`) and `/etc/sudoers.d/liftoff` (equal to
//!   `scripts/sudoers`, `0440`);
//! - `/var/lib/liftoff/lf_system/compose.yaml`
//!   ([`crate::system_compose::render`]), brought up as compose project
//!   `lf_system`; then the first `POST /load` with the configuration of no
//!   domains; then the unit is enabled and started;
//! - prints the setup token and the reminder `A *.<base> → <ip>`.
//!
//! A machine that already has `/etc/liftoff/liftoffd.toml` is refused
//! without `--force`. That file is written last, just before the unit
//! starts (and removed when it does not), so a run that fails half-way
//! can be run again as it was. The exception is `--import`: it refuses a
//! database that already holds a user (IMP-06), so a run that failed after
//! the import is run again without `--import`, or after removing the
//! database.
//!
//! Switching a v1 machine over: v1 keeps the same unit, config
//! and database paths, so stop v1 (its compose projects keep running),
//! move its database aside (`/var/lib/liftoff/liftoff.db` →
//! `/var/lib/liftoff/liftoff-v1.db`) and run `setup --force --import
//! /var/lib/liftoff/liftoff-v1.db` with the v1 master key in place. A v1
//! database left at the database path is refused, not migrated: the
//! store does not open a v1 file. The import keeps the v1 compose
//! project names where it can and logs the environments it renames
//! ([`liftoff_store::ImportReport::renamed`]: the other repositories of a
//! tenant with several, and a target whose name clashes in a repository
//! two tenants pin). Such an environment's v1 containers keep running
//! under the v1 name and keep its host ports, so before its first v2
//! deploy run `docker compose -p <v1 name> down` for it; until then a v2
//! deploy of it cannot bind its ports.
//!
//! Host actions go through [`Runner`] and the proxy port, files under
//! [`SetupEnv::root`], so the tests run it in a temporary directory.

use core::fmt::Write as _;
use core::net::{Ipv4Addr, SocketAddr};
use core::time::Duration;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use clap::ValueEnum as _;
use liftoff_core::Error;
use liftoff_core::crypto::MasterKey;
use liftoff_core::domain::{Host, OidcIssuer, SetupSecret};
use liftoff_core::ports::Proxy;
use liftoff_core::proxy_config::ProxyConfig;
use liftoff_core::service::{Settings, bootstrap_admin};
use liftoff_store::{SqliteStore, V1Import, import_v1};

use crate::config::{AppConfig, DEFAULT_CONFIG_FILE, TlsKind};
use crate::system::{OsRng, SystemClock};
use crate::system_compose;

/// The answers given on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct SetupArgs {
    /// The wildcard base of managed domains, e.g. `apps.example.com`.
    #[arg(long, value_name = "DOMAIN")]
    pub base_domain: Option<String>,
    /// Where the API is reached; defaults to `https://<base-domain>`.
    #[arg(long, value_name = "URL")]
    pub public_url: Option<String>,
    /// The server's public IPv4 address; defaults to the address of the
    /// default route.
    #[arg(long, value_name = "IP")]
    pub public_ip: Option<Ipv4Addr>,
    /// How Caddy terminates TLS.
    #[arg(long, value_enum)]
    pub tls: Option<TlsKind>,
    /// The loopback port Caddy listens on with `--tls edge`.
    #[arg(long, value_name = "PORT")]
    pub edge_port: Option<u16>,
    /// The ACME account with `--tls acme`.
    #[arg(long, value_name = "EMAIL")]
    pub acme_email: Option<String>,
    /// The loopback address of the admin API of the `lf_system` Caddy;
    /// defaults to `127.0.0.1:2019`, Caddy's own default. That Caddy runs
    /// on the host network, so on a host whose Caddy already holds that
    /// address (the case `--tls edge` exists for) pass another one, or
    /// setup's first load replaces the host Caddy's configuration.
    #[arg(long, value_name = "ADDR")]
    pub proxy_admin: Option<SocketAddr>,
    /// Take the default of every answer that has one, without asking.
    #[arg(long)]
    pub yes: bool,
    /// Set up again on a machine that is already set up.
    #[arg(long)]
    pub force: bool,
    /// A liftoff v1 database to import into the new one before the admin
    /// is created; its env files must open with the master
    /// key this setup keeps or writes.
    #[arg(
        long = "import",
        value_name = "FILE",
        requires_all = ["import_issuer", "import_git_host"]
    )]
    pub import: Option<PathBuf>,
    /// With `--import`: the OIDC issuer the v1 users logged in with (v1
    /// `zitadel.issuer`).
    #[arg(long, value_name = "URL", requires = "import")]
    pub import_issuer: Option<String>,
    /// With `--import`: the forge of the v1 repositories (the host of v1
    /// `forgejo.url`).
    #[arg(long, value_name = "HOST", requires = "import")]
    pub import_git_host: Option<String>,
}

/// One question of the interactive mode, named by the flag that answers
/// it (e.g. `--base-domain`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The flag that answers it without asking.
    pub flag: &'static str,
    /// What is asked.
    pub prompt: &'static str,
    /// The answer an empty reply takes, if any.
    pub default: Option<String>,
}

/// Asks the questions of the interactive mode; never called without a
/// terminal.
pub trait Prompter: Send {
    /// The answer to `question`, as typed.
    ///
    /// # Errors
    ///
    /// [`SetupError::Prompt`] when the terminal is gone or the user gave up.
    fn ask(&mut self, question: &Question) -> Result<String, SetupError>;
}

/// What a finished process left behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOutput {
    /// The exit status; `0` is success.
    pub status: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

/// Runs host commands as root (`useradd`, `chown`, `docker compose`,
/// `systemctl`, ...).
pub trait Runner: Send + Sync {
    /// Runs `argv` (program first) to completion. A non-zero exit is an
    /// `Ok` with its status.
    ///
    /// # Errors
    ///
    /// When the process cannot be started or waited for.
    fn run(&self, argv: &[String]) -> impl Future<Output = Result<RunOutput, Error>> + Send;
}

/// Everything a setup run touches.
pub struct SetupEnv<R, P> {
    /// Where `/` is: every file is written under it, every path a command
    /// gets is under it.
    pub root: PathBuf,
    /// Host commands.
    pub runner: R,
    /// The Caddy admin API of `lf_system`.
    pub proxy: P,
    /// Whether standard input is a terminal.
    pub tty: bool,
    /// Asks what the flags left open, on a terminal.
    pub prompter: Box<dyn Prompter>,
    /// Standard output: the setup token and the DNS reminder.
    pub out: Box<dyn Write + Send>,
}

impl<R, P> core::fmt::Debug for SetupEnv<R, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SetupEnv")
            .field("root", &self.root)
            .field("tty", &self.tty)
            .finish_non_exhaustive()
    }
}

/// Why `setup` stopped.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// Without a terminal, these flags were needed (exit 64).
    #[error("missing answers: {}", .0.join(", "))]
    Missing(Vec<&'static str>),
    /// An answer is malformed (exit 64).
    #[error("invalid {flag}: {reason}")]
    Invalid {
        /// The flag (or question) of the answer.
        flag: &'static str,
        /// Why.
        reason: String,
    },
    /// The machine is set up already and `--force` was not given.
    #[error("liftoff is already set up here ({}); pass --force to set it up again", path.display())]
    AlreadyConfigured {
        /// The configuration that exists.
        path: PathBuf,
    },
    /// The terminal could not ask.
    #[error("cannot ask: {0}")]
    Prompt(String),
    /// A step on the host failed.
    #[error("setup failed: {step}")]
    Step {
        /// What was being done.
        step: String,
        /// Why not.
        #[source]
        source: Error,
    },
}

impl SetupError {
    /// `sysexits.h`-style exit code: 64 (`EX_USAGE`) for missing or
    /// malformed answers, 73 (`EX_CANTCREAT`) for a machine that is set up
    /// already, 74 (`EX_IOERR`) when the terminal cannot ask, 70 when a
    /// host step fails.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Missing(_) | Self::Invalid { .. } => 64,
            Self::AlreadyConfigured { .. } => 73,
            Self::Prompt(_) => 74,
            Self::Step { .. } => 70,
        }
    }
}

// ---- the files and the user ----------------------------------------------------

/// The system user the daemon runs as.
const USER: &str = "liftoff";
/// The group that may use the docker socket.
const DOCKER_GROUP: &str = "docker";
/// The configuration directory.
const ETC: &str = "/etc/liftoff";
/// The systemd unit.
const UNIT: &str = "/etc/systemd/system/liftoffd.service";
/// The unit's name.
const UNIT_NAME: &str = "liftoffd.service";
/// The sudoers drop-in.
const SUDOERS: &str = "/etc/sudoers.d/liftoff";
/// The unit of the release (`scripts/liftoffd.service`).
const UNIT_FILE: &str = include_str!("../../../scripts/liftoffd.service");
/// The sudoers of the release (`scripts/sudoers`).
const SUDOERS_FILE: &str = include_str!("../../../scripts/sudoers");
/// How long setup waits for the admin API of a Caddy that just started.
const CADDY_START: Duration = Duration::from_secs(30);
/// Between two probes of that admin API.
const CADDY_PROBE: Duration = Duration::from_millis(250);

impl<R, P> SetupEnv<R, P> {
    /// `path`, absolute on the real machine, under [`SetupEnv::root`].
    fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        self.root.join(path.strip_prefix("/").unwrap_or(path))
    }
}

/// A [`SetupError::Step`] for `step`.
fn step(step: impl Into<String>) -> impl FnOnce(Error) -> SetupError {
    let step = step.into();
    move |source| SetupError::Step { step, source }
}

/// A [`SetupError::Step`] from an I/O failure of `step`.
fn io(step: impl Into<String>) -> impl FnOnce(std::io::Error) -> SetupError {
    let step = step.into();
    move |source| SetupError::Step {
        step,
        source: Error::internal(source),
    }
}

/// Writes `content` to `path` with `mode`, replacing what is there: a
/// temporary file next to it, renamed over it.
fn write_file(path: &Path, content: &str, mode: u32) -> Result<(), SetupError> {
    let what = || format!("write {}", path.display());
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _stale: Result<(), std::io::Error> = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(io(what()))?;
    file.write_all(content.as_bytes()).map_err(io(what()))?;
    file.sync_all().map_err(io(what()))?;
    drop(file);
    // The mode of `open` is filtered by the umask.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(io(what()))?;
    std::fs::rename(&tmp, path).map_err(io(what()))
}

/// `argv` as owned strings.
fn argv<I: IntoIterator<Item = S>, S: Into<String>>(args: I) -> Vec<String> {
    args.into_iter().map(Into::into).collect()
}

/// A path as a command argument.
fn arg(path: &Path) -> String {
    path.display().to_string()
}

/// Runs `argv` and requires exit 0.
async fn run<R: Runner>(runner: &R, argv: Vec<String>) -> Result<RunOutput, SetupError> {
    let what = argv.join(" ");
    let output = runner.run(&argv).await.map_err(step(what.clone()))?;
    if output.status == 0 {
        Ok(output)
    } else {
        Err(SetupError::Step {
            step: what,
            source: Error::internal_msg(format!(
                "exited with {}: {}",
                output.status,
                output.stderr.trim()
            )),
        })
    }
}

// ---- the answers -------------------------------------------------------------

/// Every answer, resolved and validated.
#[derive(Debug, Clone)]
struct Answers {
    base: String,
    public_url: String,
    public_ip: Ipv4Addr,
    tls: TlsKind,
    edge_port: Option<u16>,
    acme_email: Option<String>,
    proxy_admin: SocketAddr,
}

/// The questions of one run, and the flags that are missing so far.
struct Asker<'a, R, P> {
    env: &'a mut SetupEnv<R, P>,
    yes: bool,
    missing: Vec<&'static str>,
}

impl<R: Runner, P> Asker<'_, R, P> {
    /// Asks (or not) one question: the flag's value wins; then, with
    /// `--yes`, the default; then, on a terminal, the question; otherwise
    /// the flag is missing.
    fn ask(
        &mut self,
        flag_value: Option<String>,
        question: Question,
    ) -> Result<Option<String>, SetupError> {
        if let Some(value) = flag_value {
            return Ok(Some(value));
        }
        if self.yes
            && let Some(default) = question.default.clone()
        {
            return Ok(Some(default));
        }
        if !self.env.tty {
            self.missing.push(question.flag);
            return Ok(None);
        }
        let typed = self.env.prompter.ask(&question)?;
        let typed = typed.trim();
        if typed.is_empty() {
            return question.default.map(Some).ok_or(SetupError::Invalid {
                flag: question.flag,
                reason: "is required".to_owned(),
            });
        }
        Ok(Some(typed.to_owned()))
    }

    /// `--public-ip`: `Some(None)` when it is to be detected after the
    /// other answers are known valid (`--yes`); a terminal offers the
    /// detected address as the default.
    async fn public_ip(
        &mut self,
        flag_value: Option<Ipv4Addr>,
    ) -> Result<Option<Option<Ipv4Addr>>, SetupError> {
        if let Some(ip) = flag_value {
            return Ok(Some(Some(ip)));
        }
        if self.yes {
            return Ok(Some(None));
        }
        if !self.env.tty {
            self.missing.push("--public-ip");
            return Ok(None);
        }
        let detected = detect_ip(&self.env.runner).await;
        let typed = self.ask(
            None,
            Question {
                flag: "--public-ip",
                prompt: "Public IPv4 address of this server",
                default: detected.map(|ip| ip.to_string()),
            },
        )?;
        typed
            .map(|raw| parse("--public-ip", &raw).map(Some))
            .transpose()
    }

    /// `--tls`.
    fn tls(&mut self, flag_value: Option<TlsKind>) -> Result<Option<TlsKind>, SetupError> {
        if flag_value.is_some() {
            return Ok(flag_value);
        }
        self.ask(
            None,
            Question {
                flag: "--tls",
                prompt: "TLS: acme (Caddy gets certificates on 80/443) or edge (behind the host's proxy)",
                default: None,
            },
        )?
        .map(|raw| {
            TlsKind::from_str(&raw, true).map_err(|reason| SetupError::Invalid {
                flag: "--tls",
                reason,
            })
        })
        .transpose()
    }

    /// `--edge-port` with `edge`, `--acme-email` with `acme`.
    fn tls_details(
        &mut self,
        args: &SetupArgs,
        tls: Option<TlsKind>,
    ) -> Result<(Option<u16>, Option<String>), SetupError> {
        match tls {
            Some(TlsKind::Edge) if args.edge_port.is_some() => Ok((args.edge_port, None)),
            Some(TlsKind::Edge) => {
                let port = self
                    .ask(
                        None,
                        Question {
                            flag: "--edge-port",
                            prompt: "Loopback port Caddy listens on behind the host's proxy",
                            default: None,
                        },
                    )?
                    .map(|raw| parse("--edge-port", &raw))
                    .transpose()?;
                Ok((port, None))
            }
            Some(TlsKind::Acme) => {
                let email = self.ask(
                    args.acme_email.clone(),
                    Question {
                        flag: "--acme-email",
                        prompt: "E-mail of the ACME account",
                        default: None,
                    },
                )?;
                Ok((None, email))
            }
            None => Ok((None, None)),
        }
    }
}

/// Parses an answer of `flag`.
fn parse<T: core::str::FromStr>(flag: &'static str, raw: &str) -> Result<T, SetupError>
where
    T::Err: core::fmt::Display,
{
    raw.parse().map_err(|error: T::Err| SetupError::Invalid {
        flag,
        reason: error.to_string(),
    })
}

/// The address the default route leaves from (`ip -4 route get`), or the
/// first address of the host (`hostname -I`).
async fn detect_ip<R: Runner>(runner: &R) -> Option<Ipv4Addr> {
    let route = runner
        .run(&argv(["ip", "-4", "route", "get", "1.1.1.1"]))
        .await
        .ok()
        .filter(|output| output.status == 0)
        .and_then(|output| {
            let mut words = output.stdout.split_whitespace();
            words.find(|word| *word == "src")?;
            words.next()?.parse().ok()
        });
    if route.is_some() {
        return route;
    }
    runner
        .run(&argv(["hostname", "-I"]))
        .await
        .ok()
        .filter(|output| output.status == 0)
        .and_then(|output| {
            output
                .stdout
                .split_whitespace()
                .find_map(|word| word.parse().ok())
        })
}

/// The flag that answers the configuration key `field`, if one does.
fn flag_of(field: &str) -> Option<&'static str> {
    match field {
        "domains.base" => Some("--base-domain"),
        "api.public_url" => Some("--public-url"),
        "domains.public_ip" => Some("--public-ip"),
        "proxy.acme_email" => Some("--acme-email"),
        "proxy.edge_port" => Some("--edge-port"),
        "proxy.admin" => Some("--proxy-admin"),
        _ => None,
    }
}

/// Why the configuration of the answers does not validate: the flag of
/// the key when one answers it, the key otherwise (a default of
/// `config/default.toml`); anything but a validation error is a failed
/// step, not a wrong answer.
fn answer_error(error: Error) -> SetupError {
    match error {
        Error::Validation(error) => SetupError::Invalid {
            flag: flag_of(error.field).unwrap_or("configuration"),
            reason: error.to_string(),
        },
        other => SetupError::Step {
            step: "check the configuration of the answers".to_owned(),
            source: other,
        },
    }
}

/// Validates `answers` as the daemon will read them.
fn validate(answers: &Answers) -> Result<(AppConfig, Settings), SetupError> {
    let config = answers.config();
    let settings = config.settings().map_err(answer_error)?;
    Ok((config, settings))
}

/// Every answer, from the flags, the defaults and the terminal; nothing on
/// the host is touched before they are complete and valid, except reading
/// the default route for the default of `--public-ip`.
async fn answers<R: Runner, P>(
    args: &SetupArgs,
    env: &mut SetupEnv<R, P>,
) -> Result<Answers, SetupError> {
    let mut asker = Asker {
        env,
        yes: args.yes,
        missing: Vec::new(),
    };
    let base = asker.ask(
        args.base_domain.clone(),
        Question {
            flag: "--base-domain",
            prompt: "Base domain of the apps (a wildcard *.<base> points here)",
            default: None,
        },
    )?;
    // With `--yes` and no base domain, the URL would be derived from it:
    // only the base domain is missing.
    let public_url = if args.yes && args.public_url.is_none() && base.is_none() {
        None
    } else {
        asker.ask(
            args.public_url.clone(),
            Question {
                flag: "--public-url",
                prompt: "Public URL of the API",
                default: base.as_ref().map(|base| format!("https://{base}")),
            },
        )?
    };
    let public_ip = asker.public_ip(args.public_ip).await?;
    let tls = asker.tls(args.tls)?;
    let (edge_port, acme_email) = asker.tls_details(args, tls)?;
    let (Some(base), Some(public_url), Some(public_ip), Some(tls), true) =
        (base, public_url, public_ip, tls, asker.missing.is_empty())
    else {
        return Err(SetupError::Missing(asker.missing));
    };
    let mut answers = Answers {
        base,
        public_url,
        public_ip: public_ip.unwrap_or(Ipv4Addr::UNSPECIFIED),
        tls,
        edge_port,
        acme_email,
        proxy_admin: args
            .proxy_admin
            .unwrap_or_else(|| AppConfig::default().proxy.admin),
    };
    validate(&answers)?;
    if public_ip.is_none() {
        answers.public_ip =
            detect_ip(&asker.env.runner)
                .await
                .ok_or_else(|| SetupError::Invalid {
                    flag: "--public-ip",
                    reason: "cannot detect the address of the default route; pass it".to_owned(),
                })?;
    }
    Ok(answers)
}

impl Answers {
    /// The configuration these answers make, every other key at its
    /// default.
    fn config(&self) -> AppConfig {
        let mut config = AppConfig::default();
        config.api.public_url.clone_from(&self.public_url);
        config.domains.base.clone_from(&self.base);
        config.domains.public_ip = Some(self.public_ip);
        config.proxy.tls = self.tls;
        config.proxy.edge_port = self.edge_port;
        config.proxy.acme_email.clone_from(&self.acme_email);
        config.proxy.admin = self.proxy_admin;
        config
    }
}

/// A TOML basic string.
fn toml_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len().saturating_add(2));
    quoted.push('"');
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => {
                let _infallible = write!(quoted, "\\u{:04X}", u32::from(c));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// `/etc/liftoff/liftoffd.toml` of `config`: the answers and the
/// paths; every other key keeps its default (`config/default.toml`).
fn render_config(config: &AppConfig) -> String {
    let mut text = String::from(
        "# /etc/liftoff/liftoffd.toml, written by `liftoffd setup`.\n\
         # Every key not here keeps its default (config/default.toml); env\n\
         # LIFTOFF_<SECTION>__<KEY> overrides a key. Secrets live in files only.\n",
    );
    let _infallible = write!(
        text,
        "\n[api]\npublic_url = {}\nbind = {}\n",
        toml_string(&config.api.public_url),
        toml_string(&config.api.bind.to_string()),
    );
    let _infallible = write!(
        text,
        "\n[domains]\nbase = {}\npublic_ip = {}\n",
        toml_string(&config.domains.base),
        toml_string(
            &config
                .domains
                .public_ip
                .map(|ip| ip.to_string())
                .unwrap_or_default()
        ),
    );
    let tls = match config.proxy.tls {
        TlsKind::Acme => "acme",
        TlsKind::Edge => "edge",
    };
    let _infallible = write!(text, "\n[proxy]\ntls = {}\n", toml_string(tls));
    if let Some(email) = &config.proxy.acme_email {
        let _infallible = writeln!(text, "acme_email = {}", toml_string(email));
    }
    if let Some(port) = config.proxy.edge_port {
        let _infallible = writeln!(text, "edge_port = {port}");
    }
    let _infallible = writeln!(
        text,
        "admin = {}",
        toml_string(&config.proxy.admin.to_string())
    );
    let _infallible = write!(
        text,
        "\n[store]\npath = {}\n\n[paths]\ndata = {}\nmaster_key = {}\n",
        toml_string(&config.store.path.display().to_string()),
        toml_string(&config.paths.data.display().to_string()),
        toml_string(&config.paths.master_key.display().to_string()),
    );
    text
}

/// 32 random bytes as 64 hex digits.
fn new_master_key() -> Result<String, SetupError> {
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key).map_err(|error| SetupError::Step {
        step: "draw the master key".to_owned(),
        source: Error::internal_msg(error.to_string()),
    })?;
    let mut hex = String::with_capacity(64);
    for byte in key {
        let _infallible = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// The DNS records to create: `A *.<base>`, plus `A <api host>` when the
/// wildcard does not cover the API's host.
fn dns_records(settings: &Settings) -> Vec<String> {
    let base = settings.base.as_str();
    let ip = settings.public_ip;
    let mut records = vec![format!("A *.{base} → {ip}")];
    let api = settings.proxy.api_host.as_str();
    let covered = api
        .strip_suffix(base)
        .and_then(|label| label.strip_suffix('.'))
        .is_some_and(|label| !label.is_empty() && !label.contains('.'));
    if !covered {
        records.push(format!("A {api} → {ip}"));
    }
    records
}

// ---- the run -------------------------------------------------------------------

/// Runs `liftoffd setup` with `args` in `env`.
///
/// # Errors
///
/// [`SetupError::Missing`] / [`SetupError::Invalid`] before anything is
/// touched; [`SetupError::AlreadyConfigured`] without `--force` on a set-up
/// machine, also before anything is touched; [`SetupError::Step`] when a
/// host step fails.
pub async fn setup<R: Runner, P: Proxy>(
    args: SetupArgs,
    env: &mut SetupEnv<R, P>,
) -> Result<(), SetupError> {
    let config_file = env.path(DEFAULT_CONFIG_FILE);
    if !args.force && config_file.exists() {
        return Err(SetupError::AlreadyConfigured { path: config_file });
    }
    let answers = answers(&args, env).await?;
    let (config, settings) = validate(&answers)?;
    let database = env.path(&config.store.path);
    let import = import_source(&args, env, &database)?;

    ensure_user(&env.runner).await?;
    let data = env.path(&config.paths.data);
    let lf_system = data.join(system_compose::PROJECT);
    // The data directory holds the database: closed to other local users
    // whatever root's umask is. `/etc/liftoff` stays traversable for the
    // daemon; its only secret, `master.key`, is `0600` itself.
    for (dir, mode) in [
        (env.path(ETC), 0o755),
        (data.clone(), 0o750),
        (lf_system.clone(), 0o750),
    ] {
        make_dir(&dir, mode)?;
    }
    let master_key = write_master_key(&env.runner, &env.path(&config.paths.master_key)).await?;
    install_unit_and_sudoers(&env.runner, &env.path(UNIT), &env.path(SUDOERS)).await?;
    let compose = lf_system.join("compose.yaml");
    write_file(&compose, &system_compose::render(config.proxy.admin), 0o644)?;

    let token = fill_database(&database, import.map(|source| (source, master_key))).await?;
    close_database(&database)?;
    run(
        &env.runner,
        argv([
            "chown".to_owned(),
            "-R".to_owned(),
            format!("{USER}:{USER}"),
            arg(&data),
        ]),
    )
    .await?;

    run(
        &env.runner,
        argv([
            "docker".to_owned(),
            "compose".to_owned(),
            "--project-name".to_owned(),
            system_compose::PROJECT.to_owned(),
            "--file".to_owned(),
            arg(&compose),
            "up".to_owned(),
            "--detach".to_owned(),
            "--wait".to_owned(),
        ]),
    )
    .await?;
    wait_for_caddy(&env.proxy).await?;
    env.proxy
        .apply(ProxyConfig::from(&[], &[], &[], &settings))
        .await
        .map_err(step("load the first Caddy configuration"))?;

    // The configuration marks the machine as set up, so it comes last:
    // a run that fails before it can simply be run again. The unit reads
    // it, so it is taken back when the unit does not start.
    write_file(&config_file, &render_config(&config), 0o644)?;
    if let Err(error) = start_unit(&env.runner).await {
        let _removed: Result<(), std::io::Error> = std::fs::remove_file(&config_file);
        return Err(error);
    }

    report(env, &config, &settings, &token)
}

/// Enables and (re)starts the unit.
async fn start_unit<R: Runner>(runner: &R) -> Result<(), SetupError> {
    run(runner, argv(["systemctl", "daemon-reload"])).await?;
    run(runner, argv(["systemctl", "enable", UNIT_NAME])).await?;
    run(runner, argv(["systemctl", "restart", UNIT_NAME])).await?;
    Ok(())
}

/// `dir` and its parents, `dir` itself with `mode` (also when it existed).
fn make_dir(dir: &Path, mode: u32) -> Result<(), SetupError> {
    let what = || format!("create {}", dir.display());
    std::fs::create_dir_all(dir).map_err(io(what()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).map_err(io(what()))
}

/// The database at `path` and its SQLite side files, readable by their
/// owner only.
fn close_database(path: &Path) -> Result<(), SetupError> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        let file = PathBuf::from(file);
        if file.exists() {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                .map_err(io(format!("restrict {}", file.display())))?;
        }
    }
    Ok(())
}

/// User `liftoff` in group `docker`: added when the host has none, put in
/// the group when it has one.
async fn ensure_user<R: Runner>(runner: &R) -> Result<(), SetupError> {
    let exists = runner
        .run(&argv(["id", "-u", USER]))
        .await
        .map_err(step(format!("look up user {USER}")))?
        .status
        == 0;
    if exists {
        run(
            runner,
            argv(["usermod", "--append", "--groups", DOCKER_GROUP, USER]),
        )
        .await?;
    } else {
        run(
            runner,
            argv([
                "useradd",
                "--system",
                "--user-group",
                "--groups",
                DOCKER_GROUP,
                "--home-dir",
                "/var/lib/liftoff",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                USER,
            ]),
        )
        .await?;
    }
    Ok(())
}

/// A new master key at `path` (`0600`, owned by `liftoff`), unless one is
/// there: it seals every secret, so `--force` keeps it, closed to `0600`
/// again however it was left.
async fn write_master_key<R: Runner>(runner: &R, path: &Path) -> Result<MasterKey, SetupError> {
    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(io(format!("close {}", path.display())))?;
    } else {
        write_file(path, &new_master_key()?, 0o600)?;
    }
    let bytes = std::fs::read(path).map_err(io(format!("read {}", path.display())))?;
    let key =
        MasterKey::from_key_bytes(&bytes).map_err(step(format!("check {}", path.display())))?;
    run(
        runner,
        argv(["chown".to_owned(), format!("{USER}:{USER}"), arg(path)]),
    )
    .await?;
    Ok(key)
}

/// The unit and the sudoers drop-in of the release; sudoers is checked by
/// `visudo` and removed when it does not pass.
async fn install_unit_and_sudoers<R: Runner>(
    runner: &R,
    unit: &Path,
    sudoers: &Path,
) -> Result<(), SetupError> {
    for file in [unit, sudoers] {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(io(format!("create {}", dir.display())))?;
        }
    }
    write_file(unit, UNIT_FILE, 0o644)?;
    write_file(sudoers, SUDOERS_FILE, 0o440)?;
    if let Err(error) = run(
        runner,
        argv([
            "visudo".to_owned(),
            "-c".to_owned(),
            "-f".to_owned(),
            arg(sudoers),
        ]),
    )
    .await
    {
        let _removed: Result<(), std::io::Error> = std::fs::remove_file(sudoers);
        return Err(error);
    }
    Ok(())
}

/// What `--import` brings in: the v1 file (under the root) and how to
/// read it.
#[derive(Debug, Clone)]
struct ImportSource {
    path: PathBuf,
    issuer: OidcIssuer,
    git_host: Host,
}

/// The checked `--import` answers, before anything is touched: the file
/// exists and is not the database setup writes (the v1 database would be
/// migrated in place), the issuer and the forge host are given and
/// well-formed.
fn import_source<R, P>(
    args: &SetupArgs,
    env: &SetupEnv<R, P>,
    database: &Path,
) -> Result<Option<ImportSource>, SetupError> {
    let Some(raw) = &args.import else {
        return Ok(None);
    };
    let missing: Vec<&'static str> = [
        ("--import-issuer", args.import_issuer.is_none()),
        ("--import-git-host", args.import_git_host.is_none()),
    ]
    .into_iter()
    .filter_map(|(flag, absent)| absent.then_some(flag))
    .collect();
    let (Some(issuer), Some(git_host)) = (&args.import_issuer, &args.import_git_host) else {
        return Err(SetupError::Missing(missing));
    };
    let path = env.path(raw);
    if !path.is_file() {
        return Err(SetupError::Invalid {
            flag: "--import",
            reason: format!("{} is not a file", raw.display()),
        });
    }
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(&path, database) {
        return Err(SetupError::Invalid {
            flag: "--import",
            reason: format!(
                "{} is the database setup writes; move the v1 database aside and import it from there",
                raw.display()
            ),
        });
    }
    let issuer = OidcIssuer::new(issuer).map_err(|error| SetupError::Invalid {
        flag: "--import-issuer",
        reason: error.to_string(),
    })?;
    let git_host = Host::new(git_host).map_err(|error| SetupError::Invalid {
        flag: "--import-git-host",
        reason: error.to_string(),
    })?;
    Ok(Some(ImportSource {
        path,
        issuer,
        git_host,
    }))
}

/// Opens the database at `path`, imports the v1 database of `import`
/// (whose env files open with its master key) when there is one, and mints
/// the admin's setup token; the store is closed before the data directory
/// changes hands. The import comes first: it refuses a database that holds
/// a user.
async fn fill_database(
    path: &Path,
    import: Option<(ImportSource, MasterKey)>,
) -> Result<SetupSecret, SetupError> {
    let store = SqliteStore::open(path).map_err(step(format!("open {}", path.display())))?;
    if let Some((source, master_key)) = import {
        let what = format!("import {}", source.path.display());
        let report = import_v1(
            &source.path,
            &store,
            V1Import {
                issuer: source.issuer,
                git_host: source.git_host,
                secrets: master_key,
                rng: OsRng,
                clock: SystemClock,
            },
        )
        .await
        .map_err(step(what))?;
        tracing::info!(?report, "imported the v1 database");
    }
    let (_admin, token) = bootstrap_admin(&store, &SystemClock, &OsRng)
        .await
        .map_err(step("create the admin"))?;
    drop(store);
    Ok(token)
}

/// Waits until the admin API of a Caddy that was just started answers.
async fn wait_for_caddy<P: Proxy>(proxy: &P) -> Result<(), SetupError> {
    let probe = async {
        loop {
            match proxy.ready().await {
                Ok(()) => return Ok(()),
                Err(_not_yet) => tokio::time::sleep(CADDY_PROBE).await,
            }
        }
    };
    match tokio::time::timeout(CADDY_START, probe).await {
        Ok(ready) => ready,
        Err(_elapsed) => Err(SetupError::Step {
            step: "wait for the Caddy admin API".to_owned(),
            source: proxy
                .ready()
                .await
                .err()
                .unwrap_or_else(|| Error::unavailable("the Caddy admin API did not answer")),
        }),
    }
}

/// The setup token and the DNS reminder, on standard output.
fn report<R, P>(
    env: &mut SetupEnv<R, P>,
    config: &AppConfig,
    settings: &Settings,
    token: &SetupSecret,
) -> Result<(), SetupError> {
    let mut text = String::new();
    let _infallible = write!(
        text,
        "liftoff is set up; liftoffd runs as {UNIT_NAME}.\n\n\
         Log in once with this single-use token:\n\n  \
         lof login {} --token {}\n\n\
         Create these DNS records:\n",
        config.api.public_url,
        token.expose(),
    );
    for record in dns_records(settings) {
        let _infallible = writeln!(text, "  {record}");
    }
    env.out
        .write_all(text.as_bytes())
        .and_then(|()| env.out.flush())
        .map_err(io("print the setup token"))
}

// ---- production ----------------------------------------------------------------

/// Host commands as child processes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessRunner;

impl Runner for ProcessRunner {
    async fn run(&self, argv: &[String]) -> Result<RunOutput, Error> {
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| Error::internal_msg("an empty command"))?;
        let output = tokio::process::Command::new(program)
            .args(rest)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(Error::internal)?;
        Ok(RunOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Questions on standard error, answers from standard input.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdinPrompter;

impl Prompter for StdinPrompter {
    fn ask(&mut self, question: &Question) -> Result<String, SetupError> {
        let mut stderr = std::io::stderr().lock();
        let shown = question.default.as_ref().map_or_else(
            || format!("{}: ", question.prompt),
            |default| format!("{} [{default}]: ", question.prompt),
        );
        stderr
            .write_all(shown.as_bytes())
            .and_then(|()| stderr.flush())
            .map_err(|error| SetupError::Prompt(error.to_string()))?;
        let mut line = String::new();
        let read = std::io::stdin()
            .read_line(&mut line)
            .map_err(|error| SetupError::Prompt(error.to_string()))?;
        if read == 0 {
            return Err(SetupError::Prompt(format!(
                "no answer to {}: standard input is closed",
                question.flag
            )));
        }
        Ok(line.trim().to_owned())
    }
}

/// `liftoffd setup` on this host: root `/`, real processes, the Caddy
/// admin API on `--proxy-admin` (`127.0.0.1:2019` by default), the
/// terminal of the process.
///
/// # Errors
///
/// As [`setup`].
pub async fn setup_host(args: SetupArgs) -> Result<(), SetupError> {
    use std::io::IsTerminal as _;
    let admin = args
        .proxy_admin
        .unwrap_or_else(|| AppConfig::default().proxy.admin);
    let proxy =
        liftoff_proxy::CaddyAdmin::new(admin).map_err(step("build the Caddy admin client"))?;
    let mut env = SetupEnv {
        root: PathBuf::from("/"),
        runner: ProcessRunner,
        proxy,
        tty: std::io::stdin().is_terminal(),
        prompter: Box::new(StdinPrompter),
        out: Box::new(std::io::stdout()),
    };
    setup(args, &mut env).await
}

#[cfg(test)]
mod tests {
    use super::*;

    mod dns_records {
        use super::*;

        fn settings(base: &str, api: &str) -> Settings {
            let mut config = AppConfig::default();
            config.api.public_url = format!("https://{api}");
            config.domains.base = base.to_owned();
            config.domains.public_ip = Some(Ipv4Addr::new(203, 0, 113, 10));
            config.proxy.tls = TlsKind::Edge;
            config.proxy.edge_port = Some(7002);
            config.settings().unwrap()
        }

        // TC: SETUP-02 (`A <base>` when the API lives there)
        #[test]
        fn names_the_api_host_when_the_wildcard_does_not_cover_it() {
            assert_eq!(
                dns_records(&settings("apps.test", "liftoff.example.com")),
                vec![
                    "A *.apps.test → 203.0.113.10".to_owned(),
                    "A liftoff.example.com → 203.0.113.10".to_owned()
                ]
            );
        }

        // TC: SETUP-02
        #[test]
        fn names_the_wildcard_alone_when_it_covers_the_api_host() {
            assert_eq!(
                dns_records(&settings("apps.test", "liftoff.apps.test")),
                vec!["A *.apps.test → 203.0.113.10".to_owned()]
            );
        }
    }

    mod answer_error {
        use super::*;

        #[test]
        fn names_the_flag_when_the_key_has_one() {
            let error = answer_error(Error::invalid("proxy.edge_port", "is required"));
            assert!(
                matches!(
                    error,
                    SetupError::Invalid {
                        flag: "--edge-port",
                        ..
                    }
                ),
                "{error:?}"
            );
        }

        #[test]
        fn names_the_key_not_a_flag_when_no_flag_answers_it() {
            let error = answer_error(Error::invalid("domains.reserved", "is malformed"));
            assert_eq!(
                (error.to_string().contains("--"), error.exit_code()),
                (false, 64),
                "{error}"
            );
        }

        #[test]
        fn is_a_failed_step_not_an_answer_when_the_error_is_internal() {
            let error = answer_error(Error::internal_msg("boom"));
            assert_eq!(error.exit_code(), 70, "{error}");
        }
    }

    mod toml_string {
        use super::*;

        #[test]
        fn escapes_quotes_and_backslashes_when_quoted() {
            assert_eq!(toml_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        }
    }
}
