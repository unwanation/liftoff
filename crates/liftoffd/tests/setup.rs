#![cfg(test)]
//! SETUP-01…04: `liftoffd setup` in a temporary root, over a fake process
//! runner, a fake terminal and a fake Caddy admin API.
//!
//! Generated files are compared with `insta` snapshots and, where the
//! spec names a value, with that value.

use core::net::{Ipv4Addr, SocketAddr};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use liftoff_core::Error;
use liftoff_core::crypto::MasterKey;
use liftoff_core::domain::{OidcIssuer, SetupSecret, UserRole, UserSource};
use liftoff_core::ports::{Proxy, Store as _};
use liftoff_core::proxy_config::{ProxyConfig, Tls};
use liftoff_core::service::TlsMode;
use liftoff_store::SqliteStore;
use liftoffd::config::{AppConfig, TlsKind};
use liftoffd::error::AppError;
use liftoffd::setup::{
    Prompter, Question, RunOutput, Runner, SetupArgs, SetupEnv, SetupError, setup,
};
use tempfile::TempDir;

// ---- fakes -------------------------------------------------------------------

/// What the host saw, in order: commands and Caddy loads.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Run(Vec<String>),
    Load(ProxyConfig),
}

type Events = Arc<Mutex<Vec<Event>>>;

/// A host without user `liftoff`: `id`/`getent passwd` of it fail, the
/// default route goes out from `203.0.113.10`, the program in `failing`
/// (if any) exits 1, everything else succeeds.
#[derive(Debug, Clone)]
struct FakeRunner {
    events: Events,
    failing: Arc<Mutex<Option<&'static str>>>,
}

fn reply(argv: &[String]) -> RunOutput {
    let program = argv.first().map(String::as_str).unwrap_or_default();
    let ok = |stdout: &str| RunOutput {
        status: 0,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    };
    let asks_for_liftoff_user = argv.iter().any(|arg| arg == "liftoff")
        && (program == "id" || (program == "getent" && argv.iter().any(|arg| arg == "passwd")));
    match program {
        _ if asks_for_liftoff_user => RunOutput {
            status: 2,
            stdout: String::new(),
            stderr: "no such user\n".to_owned(),
        },
        "ip" => ok("1.1.1.1 via 203.0.113.1 dev eth0 src 203.0.113.10 uid 0 \n    cache \n"),
        "hostname" => ok("203.0.113.10 \n"),
        _ => ok(""),
    }
}

impl Runner for FakeRunner {
    fn run(&self, argv: &[String]) -> impl Future<Output = Result<RunOutput, Error>> + Send {
        self.events.lock().unwrap().push(Event::Run(argv.to_vec()));
        let failing = *self.failing.lock().unwrap();
        if failing.is_some() && argv.first().map(String::as_str) == failing {
            return core::future::ready(Ok(RunOutput {
                status: 1,
                stdout: String::new(),
                stderr: "failed\n".to_owned(),
            }));
        }
        core::future::ready(Ok(reply(argv)))
    }
}

/// Caddy's admin API: every load is recorded and accepted.
#[derive(Debug, Clone)]
struct FakeCaddy {
    events: Events,
}

impl Proxy for FakeCaddy {
    fn apply(&self, cfg: ProxyConfig) -> impl Future<Output = Result<(), Error>> + Send {
        self.events.lock().unwrap().push(Event::Load(cfg));
        core::future::ready(Ok(()))
    }

    fn ready(&self) -> impl Future<Output = Result<(), Error>> + Send {
        core::future::ready(Ok(()))
    }
}

/// A terminal that answers by flag and notes every question.
struct FakeTerminal {
    answers: HashMap<&'static str, String>,
    asked: Arc<Mutex<Vec<&'static str>>>,
}

impl Prompter for FakeTerminal {
    fn ask(&mut self, question: &Question) -> Result<String, SetupError> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(question.flag);
        self.answers
            .get(question.flag)
            .cloned()
            .ok_or_else(|| SetupError::Prompt(format!("unexpected question {}", question.flag)))
    }
}

/// A writer into a shared buffer.
#[derive(Debug, Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---- harness -----------------------------------------------------------------

/// One machine: a temporary root and what was done to it.
struct Machine {
    dir: TempDir,
    events: Events,
    asked: Arc<Mutex<Vec<&'static str>>>,
    out: Buffer,
    failing: Arc<Mutex<Option<&'static str>>>,
}

impl Machine {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            events: Events::default(),
            asked: Arc::default(),
            out: Buffer::default(),
            failing: Arc::default(),
        }
    }

    /// From now on, `program` exits 1 on this host (`None`: nothing fails).
    fn fail(&self, program: Option<&'static str>) {
        *self.failing.lock().unwrap() = program;
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// `path` (absolute on the real machine) under the root.
    fn file(&self, path: &str) -> PathBuf {
        self.root().join(path.trim_start_matches('/'))
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.file(path))
            .unwrap_or_else(|error| panic!("{path} was not written: {error}"))
    }

    fn mode(&self, path: &str) -> u32 {
        std::fs::metadata(self.file(path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    fn write(&self, path: &str, content: &str) {
        let file = self.file(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    /// Runs setup without a terminal.
    async fn batch(&self, args: SetupArgs) -> Result<(), SetupError> {
        self.setup(args, false, HashMap::new()).await
    }

    /// Runs setup on a terminal that answers `answers`.
    async fn terminal(
        &self,
        args: SetupArgs,
        answers: &[(&'static str, &str)],
    ) -> Result<(), SetupError> {
        let answers = answers
            .iter()
            .map(|(flag, answer)| (*flag, (*answer).to_owned()))
            .collect();
        self.setup(args, true, answers).await
    }

    async fn setup(
        &self,
        args: SetupArgs,
        tty: bool,
        answers: HashMap<&'static str, String>,
    ) -> Result<(), SetupError> {
        let mut env = SetupEnv {
            root: self.root().to_path_buf(),
            runner: FakeRunner {
                events: Arc::clone(&self.events),
                failing: Arc::clone(&self.failing),
            },
            proxy: FakeCaddy {
                events: Arc::clone(&self.events),
            },
            tty,
            prompter: Box::new(FakeTerminal {
                answers,
                asked: Arc::clone(&self.asked),
            }),
            out: Box::new(self.out.clone()),
        };
        setup(args, &mut env).await
    }

    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    fn commands(&self) -> Vec<Vec<String>> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Run(argv) => Some(argv),
                Event::Load(_) => None,
            })
            .collect()
    }

    fn loads(&self) -> Vec<ProxyConfig> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Load(cfg) => Some(cfg),
                Event::Run(_) => None,
            })
            .collect()
    }

    fn asked(&self) -> Vec<&'static str> {
        self.asked.lock().unwrap().clone()
    }

    fn output(&self) -> String {
        String::from_utf8(self.out.0.lock().unwrap().clone()).unwrap()
    }

    /// The generated `/etc/liftoff/liftoffd.toml`, loaded as the daemon
    /// loads it.
    fn config(&self) -> AppConfig {
        AppConfig::load(Some(&self.file("/etc/liftoff/liftoffd.toml")))
            .unwrap_or_else(|error| panic!("the generated config does not load: {error}"))
    }

    fn store(&self) -> SqliteStore {
        let path = self.file("/var/lib/liftoff/liftoff.db");
        assert!(path.is_file(), "no database at {}", path.display());
        SqliteStore::open(path).unwrap()
    }

    /// Every file under the root, relative to it.
    fn files(&self) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, found);
                } else {
                    found.push(path.strip_prefix(root).unwrap().display().to_string());
                }
            }
        }
        let mut found = Vec::new();
        walk(self.root(), self.root(), &mut found);
        found.sort();
        found
    }
}

/// The command of the acceptance criterion:
/// `liftoffd setup --yes --base-domain apps.test --tls edge --edge-port 7002`.
fn criterion() -> SetupArgs {
    SetupArgs {
        yes: true,
        base_domain: Some("apps.test".to_owned()),
        tls: Some(TlsKind::Edge),
        edge_port: Some(7002),
        ..SetupArgs::default()
    }
}

/// Every answer for `acme` as a flag, without `--yes`.
fn acme() -> SetupArgs {
    SetupArgs {
        base_domain: Some("apps.test".to_owned()),
        public_url: Some("https://liftoff.apps.test".to_owned()),
        public_ip: Some(Ipv4Addr::new(203, 0, 113, 10)),
        tls: Some(TlsKind::Acme),
        acme_email: Some("ops@example.com".to_owned()),
        ..SetupArgs::default()
    }
}

/// The flags a refusal names; `None` when it was not about answers.
fn missing(result: Result<(), SetupError>) -> Option<Vec<&'static str>> {
    match result {
        Err(SetupError::Missing(flags)) => Some(flags),
        _ => None,
    }
}

fn exit_code(result: Result<(), SetupError>) -> Option<u8> {
    result.err().map(|error| AppError::from(error).exit_code())
}

fn has(argv: &[String], word: &str) -> bool {
    argv.iter().any(|arg| arg == word)
}

fn snapshot(name: &str, text: &str) {
    insta::with_settings!({ prepend_module_to_snapshot => false, omit_expression => true }, {
        insta::assert_snapshot!(name, text);
    });
}

fn repo_file(path: &str) -> String {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    std::fs::read_to_string(&file)
        .unwrap_or_else(|error| panic!("{} is missing: {error}", file.display()))
}

// ---- SETUP-01: answers from flags or questions -------------------------------

mod answers {
    use super::*;

    // TC: SETUP-01
    #[tokio::test]
    async fn succeeds_without_a_terminal_when_the_criterion_flags_are_given() {
        let machine = Machine::new();
        let result = machine.batch(criterion()).await;
        assert!(result.is_ok(), "{result:?}");
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn succeeds_without_a_terminal_when_every_answer_is_a_flag() {
        let machine = Machine::new();
        let result = machine.batch(acme()).await;
        assert!(result.is_ok(), "{result:?}");
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn exits_64_when_the_base_domain_flag_is_missing_without_a_terminal() {
        let machine = Machine::new();
        let args = SetupArgs {
            base_domain: None,
            ..criterion()
        };
        assert_eq!(exit_code(machine.batch(args).await), Some(64));
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn names_the_base_domain_flag_when_it_is_missing_without_a_terminal() {
        let machine = Machine::new();
        let args = SetupArgs {
            base_domain: None,
            ..criterion()
        };
        assert_eq!(
            missing(machine.batch(args).await),
            Some(vec!["--base-domain"])
        );
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn names_every_missing_flag_at_once_when_none_is_given_without_a_terminal() {
        let machine = Machine::new();
        let flags = missing(machine.batch(SetupArgs::default()).await).unwrap_or_default();
        assert_eq!(
            (
                flags.contains(&"--base-domain"),
                flags.contains(&"--tls"),
                flags.contains(&"--public-url"),
            ),
            (true, true, true),
            "{flags:?}"
        );
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn names_the_edge_port_when_edge_has_none_without_a_terminal() {
        let machine = Machine::new();
        let args = SetupArgs {
            edge_port: None,
            ..criterion()
        };
        assert_eq!(
            missing(machine.batch(args).await),
            Some(vec!["--edge-port"])
        );
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn names_the_acme_email_when_acme_has_none_without_a_terminal() {
        let machine = Machine::new();
        let args = SetupArgs {
            acme_email: None,
            ..acme()
        };
        assert_eq!(
            missing(machine.batch(args).await),
            Some(vec!["--acme-email"])
        );
    }

    // TC: SETUP-01 (a default is taken only with `--yes`)
    #[tokio::test]
    async fn names_the_public_url_when_it_is_missing_without_yes_or_a_terminal() {
        let machine = Machine::new();
        let args = SetupArgs {
            public_url: None,
            ..acme()
        };
        assert_eq!(
            missing(machine.batch(args).await),
            Some(vec!["--public-url"])
        );
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn touches_nothing_when_a_flag_is_missing_without_a_terminal() {
        let machine = Machine::new();
        let args = SetupArgs {
            base_domain: None,
            ..criterion()
        };
        let _refused = machine.batch(args).await;
        assert_eq!(
            (machine.events(), machine.files()),
            (Vec::new(), Vec::<String>::new())
        );
    }

    // TC: SETUP-01 (validated at the boundary)
    #[tokio::test]
    async fn exits_64_and_touches_nothing_when_the_base_domain_is_malformed() {
        let machine = Machine::new();
        let args = SetupArgs {
            base_domain: Some("Not A Domain".to_owned()),
            ..criterion()
        };
        let code = exit_code(machine.batch(args).await);
        assert_eq!(
            (code, machine.events(), machine.files()),
            (Some(64), Vec::new(), Vec::<String>::new())
        );
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn exits_64_when_the_public_url_is_not_a_url() {
        let machine = Machine::new();
        let args = SetupArgs {
            public_url: Some("liftoff.apps.test".to_owned()),
            ..acme()
        };
        assert_eq!(exit_code(machine.batch(args).await), Some(64));
    }

    // TC: SETUP-01 (validated at the boundary; the Caddy admin API only on loopback)
    #[tokio::test]
    async fn exits_64_naming_the_flag_when_the_proxy_admin_is_not_loopback() {
        let machine = Machine::new();
        let args = SetupArgs {
            proxy_admin: Some(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 2029))),
            ..criterion()
        };
        let result = machine.batch(args).await;
        let flag = match &result {
            Err(SetupError::Invalid { flag, .. }) => Some(*flag),
            _ => None,
        };
        assert_eq!(
            (exit_code(result), flag, machine.events()),
            (Some(64), Some("--proxy-admin"), Vec::new())
        );
    }

    // TC: SETUP-01 (with a terminal the same run asks)
    #[tokio::test]
    async fn asks_for_what_is_missing_when_on_a_terminal() {
        let machine = Machine::new();
        let result = machine
            .terminal(
                SetupArgs::default(),
                &[
                    ("--base-domain", "apps.test"),
                    ("--public-url", "https://liftoff.apps.test"),
                    ("--public-ip", "203.0.113.10"),
                    ("--tls", "edge"),
                    ("--edge-port", "7002"),
                ],
            )
            .await;
        assert!(result.is_ok(), "{result:?}");
        let asked = machine.asked();
        assert_eq!(
            (
                asked.contains(&"--base-domain"),
                asked.contains(&"--tls"),
                asked.contains(&"--edge-port"),
                asked.contains(&"--acme-email"),
            ),
            (true, true, true, false),
            "{asked:?}"
        );
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn writes_the_typed_answers_when_on_a_terminal() {
        let machine = Machine::new();
        machine
            .terminal(
                SetupArgs::default(),
                &[
                    ("--base-domain", "apps.test"),
                    ("--public-url", "https://liftoff.apps.test"),
                    ("--public-ip", "203.0.113.10"),
                    ("--tls", "edge"),
                    ("--edge-port", "7002"),
                ],
            )
            .await
            .unwrap();
        let settings = machine.config().settings().unwrap();
        assert_eq!(
            (
                settings.base.as_str(),
                settings.public_ip,
                settings.proxy.api_host.as_str(),
                settings.proxy.tls,
            ),
            (
                "apps.test",
                Ipv4Addr::new(203, 0, 113, 10),
                "liftoff.apps.test",
                TlsMode::Edge { port: 7002 },
            )
        );
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn asks_nothing_when_flags_answer_everything_on_a_terminal() {
        let machine = Machine::new();
        let result = machine.terminal(acme(), &[]).await;
        assert_eq!(
            (result.is_ok(), machine.asked()),
            (true, Vec::<&str>::new()),
            "{result:?}"
        );
    }

    // TC: SETUP-01 (`--yes` takes the defaults: `https://<base>`, the
    // address of the default route)
    #[tokio::test]
    async fn takes_the_defaults_when_yes_is_given() {
        let machine = Machine::new();
        machine.batch(criterion()).await.unwrap();
        let settings = machine.config().settings().unwrap();
        assert_eq!(
            (
                machine.config().api.public_url,
                settings.proxy.api_host.as_str().to_owned(),
                settings.public_ip,
            ),
            (
                "https://apps.test".to_owned(),
                "apps.test".to_owned(),
                Ipv4Addr::new(203, 0, 113, 10),
            )
        );
    }

    // TC: SETUP-01
    #[tokio::test]
    async fn asks_nothing_with_a_default_when_yes_is_given_on_a_terminal() {
        let machine = Machine::new();
        let result = machine.terminal(criterion(), &[]).await;
        assert_eq!(
            (result.is_ok(), machine.asked()),
            (true, Vec::<&str>::new()),
            "{result:?}"
        );
    }
}

// ---- SETUP-02: what setup leaves on the machine --------------------------------

mod generated {
    use super::*;

    const KEY: &str = "/etc/liftoff/master.key";
    const CONFIG: &str = "/etc/liftoff/liftoffd.toml";
    const UNIT: &str = "/etc/systemd/system/liftoffd.service";
    const SUDOERS: &str = "/etc/sudoers.d/liftoff";
    const COMPOSE: &str = "/var/lib/liftoff/lf_system/compose.yaml";

    async fn set_up() -> Machine {
        let machine = Machine::new();
        machine.batch(criterion()).await.unwrap();
        machine
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn writes_a_master_key_of_64_hex_digits_when_set_up() {
        let machine = set_up().await;
        let key = machine.read(KEY);
        let key = key.trim();
        assert_eq!(
            (key.len(), key.bytes().all(|b| b.is_ascii_hexdigit())),
            (64, true),
            "{key:?}"
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn writes_a_master_key_the_daemon_loads_when_set_up() {
        let machine = set_up().await;
        let bytes = std::fs::read(machine.file(KEY)).unwrap();
        assert!(MasterKey::from_key_bytes(&bytes).is_ok());
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn keeps_the_master_key_private_when_set_up() {
        let machine = set_up().await;
        assert_eq!(machine.mode(KEY), 0o600);
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn draws_a_new_master_key_when_another_machine_is_set_up() {
        let (one, two) = (set_up().await, set_up().await);
        assert_ne!(one.read(KEY), two.read(KEY));
    }

    // TC: SETUP-02 (the answers, loaded as the daemon loads them)
    #[tokio::test]
    async fn writes_a_config_with_the_answers_when_set_up() {
        let machine = set_up().await;
        let settings = machine.config().settings().unwrap();
        assert_eq!(
            (settings.base.as_str(), settings.proxy.tls),
            ("apps.test", TlsMode::Edge { port: 7002 })
        );
    }

    // TC: SETUP-02 (secrets only as files)
    #[tokio::test]
    async fn keeps_the_key_out_of_the_config_when_set_up() {
        let machine = set_up().await;
        let key = machine.read(KEY);
        let config = machine.read(CONFIG);
        assert_eq!(
            (
                config.contains(key.trim()),
                machine.config().paths.master_key.to_str()
            ),
            (false, Some("/etc/liftoff/master.key"))
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn matches_the_config_snapshot_when_set_up() {
        let machine = set_up().await;
        snapshot("setup02_liftoffd_toml", &machine.read(CONFIG));
    }

    // TC: SETUP-02 (the unit)
    #[tokio::test]
    async fn writes_a_unit_with_the_spec_settings_when_set_up() {
        let machine = set_up().await;
        let unit = machine.read(UNIT);
        let lines: Vec<&str> = unit.lines().map(str::trim).collect();
        assert_eq!(
            (
                lines.contains(&"KillMode=mixed"),
                lines.contains(&"TimeoutStopSec=150"),
                lines.contains(&"RestartPreventExitStatus=78"),
                lines.contains(&"User=liftoff"),
                lines
                    .iter()
                    .any(|line| line.starts_with("ExecStart=/usr/local/bin/liftoffd")),
            ),
            (true, true, true, true, true),
            "{unit}"
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn writes_the_unit_of_the_release_when_set_up() {
        let machine = set_up().await;
        assert_eq!(machine.read(UNIT), repo_file("scripts/liftoffd.service"));
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn matches_the_unit_snapshot_when_set_up() {
        let machine = set_up().await;
        snapshot("setup02_liftoffd_service", &machine.read(UNIT));
    }

    // TC: SETUP-02 (exactly the slice commands of `SudoSystemd`)
    #[tokio::test]
    async fn writes_sudoers_for_the_slice_commands_when_set_up() {
        let machine = set_up().await;
        let sudoers = machine.read(SUDOERS);
        // The rules themselves, not the comments: one anchored systemctl
        // pattern per command (that they admit exactly the argv of
        // `SudoSystemd` is checked in crates/host/src/systemd.rs).
        let rules: Vec<&str> = sudoers
            .lines()
            .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
            .map(|line| {
                line.strip_prefix("liftoff ALL=(root) NOPASSWD: /usr/bin/systemctl ^")
                    .and_then(|rest| rest.strip_suffix('$'))
                    .and_then(|args| args.split_whitespace().next())
                    .unwrap_or(line)
            })
            .collect();
        assert_eq!(rules, vec!["set-property", "stop"], "{sudoers}");
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn writes_sudoers_read_only_when_set_up() {
        let machine = set_up().await;
        assert_eq!(machine.mode(SUDOERS), 0o440);
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn writes_the_sudoers_of_the_release_when_set_up() {
        let machine = set_up().await;
        assert_eq!(machine.read(SUDOERS), repo_file("scripts/sudoers"));
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn checks_the_sudoers_file_when_set_up() {
        let machine = set_up().await;
        let path = machine.file(SUDOERS).display().to_string();
        assert!(
            machine
                .commands()
                .iter()
                .any(|argv| argv.first().map(String::as_str) == Some("visudo")
                    && has(argv, "-c")
                    && argv.iter().any(|arg| arg.ends_with(&path))),
            "{:?}",
            machine.commands()
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn matches_the_sudoers_snapshot_when_set_up() {
        let machine = set_up().await;
        snapshot("setup02_sudoers", &machine.read(SUDOERS));
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn writes_the_system_compose_when_set_up() {
        let machine = set_up().await;
        assert_eq!(
            machine.read(COMPOSE),
            liftoffd::system_compose::render(SocketAddr::from((Ipv4Addr::LOCALHOST, 2019)))
        );
    }

    // TC: SETUP-02 (`--proxy-admin`: a host whose own Caddy already
    // holds 127.0.0.1:2019, the case `--tls edge` exists for)
    #[tokio::test]
    async fn gives_lf_system_the_proxy_admin_when_one_is_passed() {
        let machine = Machine::new();
        let admin = SocketAddr::from((Ipv4Addr::LOCALHOST, 2029));
        let args = SetupArgs {
            proxy_admin: Some(admin),
            ..criterion()
        };
        machine.batch(args).await.unwrap();
        assert_eq!(
            (machine.config().proxy.admin, machine.read(COMPOSE)),
            (admin, liftoffd::system_compose::render(admin))
        );
    }

    // TC: SETUP-02 (the data directory holds the database: closed to
    // other local users whatever root's umask)
    #[tokio::test]
    async fn closes_the_data_directories_to_others_when_set_up() {
        let machine = set_up().await;
        assert_eq!(
            (
                machine.mode("/var/lib/liftoff"),
                machine.mode("/var/lib/liftoff/lf_system"),
                machine.mode("/var/lib/liftoff/liftoff.db"),
                machine.mode("/etc/liftoff"),
            ),
            (0o750, 0o750, 0o600, 0o755)
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn closes_the_data_directory_when_it_existed_open() {
        let machine = Machine::new();
        std::fs::create_dir_all(machine.file("/var/lib/liftoff")).unwrap();
        std::fs::set_permissions(
            machine.file("/var/lib/liftoff"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        machine.batch(criterion()).await.unwrap();
        assert_eq!(machine.mode("/var/lib/liftoff"), 0o750);
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn creates_the_liftoff_directories_when_set_up() {
        let machine = set_up().await;
        assert_eq!(
            (
                machine.file("/etc/liftoff").is_dir(),
                machine.file("/var/lib/liftoff").is_dir(),
            ),
            (true, true)
        );
    }

    // TC: SETUP-02 (user `liftoff` in group `docker`)
    #[tokio::test]
    async fn adds_user_liftoff_when_the_host_has_none() {
        let machine = set_up().await;
        assert!(
            machine
                .commands()
                .iter()
                .any(|argv| argv.first().map(String::as_str) == Some("useradd")
                    && argv.last().map(String::as_str) == Some("liftoff")),
            "{:?}",
            machine.commands()
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn puts_user_liftoff_in_group_docker_when_set_up() {
        let machine = set_up().await;
        let in_docker = |argv: &Vec<String>| {
            let program = argv.first().map(String::as_str).unwrap_or_default();
            matches!(program, "useradd" | "usermod" | "gpasswd" | "adduser")
                && has(argv, "liftoff")
                && argv
                    .iter()
                    .any(|arg| arg == "docker" || arg.ends_with("=docker"))
        };
        assert!(
            machine.commands().iter().any(in_docker),
            "{:?}",
            machine.commands()
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn hands_the_data_directory_to_liftoff_when_set_up() {
        let machine = set_up().await;
        let data = machine.file("/var/lib/liftoff").display().to_string();
        assert!(
            machine.commands().iter().any(|argv| {
                argv.first().map(String::as_str) == Some("chown")
                    && argv.iter().any(|arg| arg.starts_with("liftoff"))
                    && has(argv, &data)
            }),
            "{:?}",
            machine.commands()
        );
    }

    // TC: SETUP-02 (Caddy in the compose project `lf_system`)
    #[tokio::test]
    async fn brings_up_lf_system_when_set_up() {
        let machine = set_up().await;
        let compose = machine.file(COMPOSE).display().to_string();
        assert!(
            machine.commands().iter().any(|argv| {
                argv.first().map(String::as_str) == Some("docker")
                    && has(argv, "compose")
                    && has(argv, "up")
                    && has(argv, &compose)
            }),
            "{:?}",
            machine.commands()
        );
    }

    // TC: SETUP-02 (the first `POST /load` with an empty config)
    #[tokio::test]
    async fn loads_caddy_once_with_no_sites_when_set_up() {
        let machine = set_up().await;
        let loads = machine.loads();
        assert_eq!(
            (
                loads.len(),
                loads.first().map(|cfg| cfg.sites.len()),
                loads.first().map(|cfg| cfg.api.host.as_str().to_owned()),
                loads.first().map(|cfg| cfg.api.upstream),
            ),
            (
                1,
                Some(0),
                Some("apps.test".to_owned()),
                Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 7000))),
            )
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn loads_caddy_after_lf_system_is_up_when_set_up() {
        let machine = set_up().await;
        let events = machine.events();
        let up = events.iter().position(
            |event| matches!(event, Event::Run(argv) if has(argv, "compose") && has(argv, "up")),
        );
        let load = events
            .iter()
            .position(|event| matches!(event, Event::Load(_)));
        assert!(
            matches!((up, load), (Some(up), Some(load)) if up < load),
            "{events:?}"
        );
    }

    // TC: SETUP-02 (the daemon runs afterwards)
    #[tokio::test]
    async fn enables_and_starts_the_unit_when_set_up() {
        let machine = set_up().await;
        let commands = machine.commands();
        let systemctl = |argv: &&Vec<String>| argv.first().map(String::as_str) == Some("systemctl");
        let reloaded = commands
            .iter()
            .filter(systemctl)
            .any(|argv| has(argv, "daemon-reload"));
        let started = commands.iter().filter(systemctl).any(|argv| {
            (has(argv, "liftoffd") || has(argv, "liftoffd.service"))
                && (has(argv, "start")
                    || has(argv, "restart")
                    || (has(argv, "enable") && has(argv, "--now")))
        });
        assert_eq!((reloaded, started), (true, true), "{commands:?}");
    }

    // TC: SETUP-02 (one `local` admin in single-user mode)
    #[tokio::test]
    async fn creates_one_local_admin_when_set_up() {
        let machine = set_up().await;
        let users = machine.store().users().await.unwrap();
        assert_eq!(
            users
                .iter()
                .map(|user| (user.role, user.source.clone()))
                .collect::<Vec<_>>(),
            vec![(UserRole::Admin, UserSource::Local)]
        );
    }

    /// The `lft_` token in the output.
    fn printed_token(output: &str) -> Option<SetupSecret> {
        output
            .split(|c: char| c.is_whitespace() || c == '`' || c == '"' || c == '\'')
            .find(|word| word.starts_with(SetupSecret::PREFIX))
            .and_then(|word| SetupSecret::parse(word).ok())
    }

    // TC: SETUP-02 (the one-time admin token)
    #[tokio::test]
    async fn prints_the_live_setup_token_when_set_up() {
        let machine = set_up().await;
        let token = printed_token(&machine.output());
        assert!(token.is_some(), "{}", machine.output());
        let live = machine
            .store()
            .setup_token(&token.unwrap().hash())
            .await
            .unwrap();
        assert!(live.is_some(), "the printed token is not the live one");
    }

    // TC: SETUP-02 (`A *.<base> → IP`)
    #[tokio::test]
    async fn reminds_of_the_wildcard_record_when_set_up() {
        let machine = set_up().await;
        let output = machine.output();
        assert!(
            output
                .lines()
                .any(|line| line.contains("A *.apps.test") && line.contains("203.0.113.10")),
            "{output}"
        );
    }

    // TC: SETUP-02 (`A <base>` when the API lives there too)
    #[tokio::test]
    async fn reminds_of_the_base_record_when_the_api_lives_on_the_base() {
        let machine = set_up().await;
        let output = machine.output();
        assert!(
            output
                .lines()
                .any(|line| line.contains("A apps.test") && line.contains("203.0.113.10")),
            "{output}"
        );
    }

    // TC: SETUP-02
    #[tokio::test]
    async fn names_no_base_record_when_the_api_lives_elsewhere() {
        let machine = Machine::new();
        machine.batch(acme()).await.unwrap();
        let output = machine.output();
        assert!(
            output.contains("A *.apps.test") && !output.contains("A apps.test"),
            "{output}"
        );
    }
}

// ---- SETUP-03: what Caddy listens on ------------------------------------------

mod listeners {
    use super::*;

    async fn first_load(args: SetupArgs) -> ProxyConfig {
        let machine = Machine::new();
        machine.batch(args).await.unwrap();
        machine.loads().into_iter().next().expect("one load")
    }

    // TC: SETUP-03
    #[tokio::test]
    async fn loads_acme_with_the_account_when_tls_is_acme() {
        let cfg = first_load(acme()).await;
        assert!(
            matches!(&cfg.tls, Tls::Acme { email } if email.as_str() == "ops@example.com"),
            "{:?}",
            cfg.tls
        );
    }

    // TC: SETUP-03
    #[tokio::test]
    async fn loads_one_loopback_listener_when_tls_is_edge() {
        let cfg = first_load(criterion()).await;
        assert_eq!(
            cfg.tls,
            Tls::Edge {
                listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 7002))
            }
        );
    }

    /// Every `listen` address of the Caddy JSON of `cfg`.
    fn listeners(cfg: &ProxyConfig) -> Vec<String> {
        let json = liftoff_proxy::render(cfg);
        let servers = json
            .pointer("/apps/http/servers")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut found: Vec<String> = servers
            .values()
            .filter_map(|server| server.get("listen")?.as_array().cloned())
            .flatten()
            .filter_map(|listen| listen.as_str().map(str::to_owned))
            .collect();
        found.sort();
        found
    }

    // TC: SETUP-03 (acme publishes 80 and 443)
    #[tokio::test]
    async fn caddy_listens_on_80_and_443_when_tls_is_acme() {
        let cfg = first_load(acme()).await;
        assert_eq!(listeners(&cfg), vec![":443".to_owned(), ":80".to_owned()]);
    }

    // TC: SETUP-03 (edge publishes only `127.0.0.1:<port>`)
    #[tokio::test]
    async fn caddy_listens_only_on_the_edge_port_when_tls_is_edge() {
        let cfg = first_load(criterion()).await;
        assert_eq!(listeners(&cfg), vec!["127.0.0.1:7002".to_owned()]);
    }

    // TC: SETUP-03
    #[tokio::test]
    async fn writes_tls_acme_with_the_account_when_tls_is_acme() {
        let machine = Machine::new();
        machine.batch(acme()).await.unwrap();
        let config = machine.config();
        assert_eq!(
            (
                config.proxy.tls,
                config.proxy.acme_email.as_deref(),
                config.proxy.edge_port
            ),
            (TlsKind::Acme, Some("ops@example.com"), None)
        );
    }
}

// ---- SETUP-04: a machine that is already set up --------------------------------

mod again {
    use super::*;

    const CONFIG: &str = "/etc/liftoff/liftoffd.toml";
    const KEY: &str = "/etc/liftoff/master.key";

    /// A machine that has a configuration and a key of its own.
    fn configured() -> Machine {
        let machine = Machine::new();
        machine.write(CONFIG, "# configured by hand\n");
        machine.write(KEY, &"11".repeat(32));
        machine
    }

    // TC: SETUP-04
    #[tokio::test]
    async fn refuses_without_force_when_the_machine_is_set_up() {
        let machine = configured();
        let result = machine.batch(criterion()).await;
        assert!(
            matches!(&result, Err(SetupError::AlreadyConfigured { path }) if path.ends_with("etc/liftoff/liftoffd.toml")),
            "{result:?}"
        );
    }

    // TC: SETUP-04
    #[tokio::test]
    async fn touches_nothing_when_refused() {
        let machine = configured();
        let _refused = machine.batch(criterion()).await;
        assert_eq!(
            (
                machine.events(),
                machine.read(CONFIG),
                machine.read(KEY),
                machine.files(),
            ),
            (
                Vec::new(),
                "# configured by hand\n".to_owned(),
                "11".repeat(32),
                vec![
                    "etc/liftoff/liftoffd.toml".to_owned(),
                    "etc/liftoff/master.key".to_owned()
                ],
            )
        );
    }

    // TC: SETUP-04
    #[tokio::test]
    async fn exits_non_zero_without_usage_code_when_refused() {
        let machine = configured();
        let code = exit_code(machine.batch(criterion()).await);
        assert!(
            matches!(code, Some(code) if code != 0 && code != 64),
            "{code:?}"
        );
    }

    // TC: SETUP-04
    #[tokio::test]
    async fn rewrites_the_config_with_force_when_the_machine_is_set_up() {
        let machine = configured();
        machine
            .batch(SetupArgs {
                force: true,
                ..criterion()
            })
            .await
            .unwrap();
        assert_eq!(machine.config().domains.base, "apps.test");
    }

    // TC: SETUP-04 (the key seals every secret: `--force` keeps it)
    #[tokio::test]
    async fn keeps_the_master_key_with_force_when_the_machine_is_set_up() {
        let machine = configured();
        machine
            .batch(SetupArgs {
                force: true,
                ..criterion()
            })
            .await
            .unwrap();
        assert_eq!(machine.read(KEY), "11".repeat(32));
    }

    // TC: SETUP-04 (the kept key is closed again, as the directories are)
    #[tokio::test]
    async fn closes_the_kept_master_key_with_force_when_it_was_open() {
        let machine = configured();
        std::fs::set_permissions(machine.file(KEY), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        machine
            .batch(SetupArgs {
                force: true,
                ..criterion()
            })
            .await
            .unwrap();
        assert_eq!(machine.mode(KEY), 0o600);
    }

    // TC: SETUP-04
    #[tokio::test]
    async fn keeps_one_admin_with_force_when_set_up_twice() {
        let machine = Machine::new();
        machine.batch(criterion()).await.unwrap();
        machine
            .batch(SetupArgs {
                force: true,
                ..criterion()
            })
            .await
            .unwrap();
        assert_eq!(machine.store().users().await.unwrap().len(), 1);
    }

    // TC: SETUP-04 (the configuration marks a machine as set up: it is
    // written only once every other step is done)
    #[tokio::test]
    async fn writes_no_config_when_lf_system_does_not_come_up() {
        let machine = Machine::new();
        machine.fail(Some("docker"));
        let result = machine.batch(criterion()).await;
        assert_eq!(
            (
                result.is_err(),
                machine.file(CONFIG).exists(),
                machine.loads().len()
            ),
            (true, false, 0),
            "{result:?}"
        );
    }

    // TC: SETUP-04
    #[tokio::test]
    async fn leaves_no_config_when_the_unit_does_not_start() {
        let machine = Machine::new();
        machine.fail(Some("systemctl"));
        let result = machine.batch(criterion()).await;
        assert_eq!(
            (result.is_err(), machine.file(CONFIG).exists()),
            (true, false),
            "{result:?}"
        );
    }

    // TC: SETUP-04
    #[tokio::test]
    async fn sets_up_without_force_when_an_earlier_run_failed() {
        let machine = Machine::new();
        machine.fail(Some("docker"));
        let _failed = machine.batch(criterion()).await;
        let key = machine.read(KEY);
        machine.fail(None);
        let result = machine.batch(criterion()).await;
        assert_eq!(
            (
                result.is_ok(),
                machine.read(KEY),
                machine.config().domains.base
            ),
            (true, key, "apps.test".to_owned()),
            "{result:?}"
        );
    }

    // TC: SETUP-04
    #[tokio::test]
    async fn refuses_the_second_run_without_force_when_set_up_once() {
        let machine = Machine::new();
        machine.batch(criterion()).await.unwrap();
        let result = machine.batch(criterion()).await;
        assert!(
            matches!(result, Err(SetupError::AlreadyConfigured { .. })),
            "{result:?}"
        );
    }
}

/// `liftoffd setup --import` with the v1
/// fixture of the store's import tests, its env files sealed with the
/// master key the machine already has.
mod import {
    use liftoff_core::domain::{EnvironmentId, SealedEnv, SecretKey, SecretScope};
    use liftoff_core::fakes::FakeSecrets;
    use liftoff_core::ports::Secrets as _;

    use super::*;

    const V1_SQL: &str = include_str!("../../store/tests/fixtures/v1.sql");
    /// The master key v1 and v2 share.
    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    /// Where the v1 database is put aside.
    const V1_DB: &str = "/srv/liftoff-v1.db";
    const ACME_PROD: &str = "01HV000000000000000000C001";

    /// A fresh machine with the v1 master key in place and the v1 database
    /// at [`V1_DB`], sealed with that key.
    fn v1_machine() -> Machine {
        let machine = Machine::new();
        machine.write("/etc/liftoff/master.key", KEY);
        let path = machine.file(V1_DB);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(V1_SQL).unwrap();
        let key = MasterKey::from_key_bytes(KEY.as_bytes()).unwrap();
        let fake = FakeSecrets::new();
        let envs: Vec<(String, String)> = conn
            .prepare("SELECT id, env FROM deploys")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (id, env) in envs {
            let plain = fake.decrypt(&SealedEnv::new(env)).unwrap();
            let sealed = key.encrypt(&plain).unwrap();
            conn.execute(
                "UPDATE deploys SET env = ? WHERE id = ?",
                [sealed.as_str(), id.as_str()],
            )
            .unwrap();
        }
        machine
    }

    fn with_import(source: &str) -> SetupArgs {
        SetupArgs {
            import: Some(PathBuf::from(source)),
            import_issuer: Some("https://auth.example.com".to_owned()),
            import_git_host: Some("git.example.com".to_owned()),
            ..criterion()
        }
    }

    // TC: IMP-01 (the v1 users beside the local admin)
    #[tokio::test]
    async fn imports_v1_users_beside_the_local_admin_when_import_given() {
        let machine = v1_machine();
        machine.batch(with_import(V1_DB)).await.unwrap();
        let users: Vec<(String, UserSource)> = machine
            .store()
            .users()
            .await
            .unwrap()
            .into_iter()
            .map(|user| (user.login.as_str().to_owned(), user.source))
            .collect();
        let oidc = UserSource::Oidc(OidcIssuer::new("https://auth.example.com").unwrap());
        assert_eq!(
            users,
            [
                ("admin".to_owned(), UserSource::Local),
                ("alice".to_owned(), oidc.clone()),
                ("bob".to_owned(), oidc),
            ]
        );
    }

    // TC: IMP-02 (the env opens with the shared master key)
    #[tokio::test]
    async fn opens_v1_env_with_the_master_key_when_import_given() {
        let machine = v1_machine();
        machine.batch(with_import(V1_DB)).await.unwrap();
        let secret = machine
            .store()
            .secret(
                &SecretScope::Environment(EnvironmentId::parse(ACME_PROD).unwrap()),
                &SecretKey::new("DATABASE_URL").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        let key = MasterKey::from_key_bytes(KEY.as_bytes()).unwrap();
        assert_eq!(key.decrypt(&secret.value).unwrap(), "postgres://db/acme");
    }

    // TC: IMP-06
    #[tokio::test]
    async fn fails_when_the_database_already_has_users() {
        let machine = v1_machine();
        machine.batch(criterion()).await.unwrap();
        let again = SetupArgs {
            force: true,
            ..with_import(V1_DB)
        };
        let result = machine.batch(again).await;
        assert_eq!(exit_code(result), Some(70));
    }

    // TC: IMP-06
    #[tokio::test]
    async fn imports_nothing_when_the_database_already_has_users() {
        let machine = v1_machine();
        machine.batch(criterion()).await.unwrap();
        let again = SetupArgs {
            force: true,
            ..with_import(V1_DB)
        };
        let _refused = machine.batch(again).await;
        let store = machine.store();
        assert_eq!(
            (
                store.users().await.unwrap().len(),
                store.projects().await.unwrap().len()
            ),
            (1, 0)
        );
    }

    // TC: IMP-06 (the v1 file would be migrated in place)
    #[tokio::test]
    async fn exits_64_when_the_import_is_the_database_setup_writes() {
        let machine = v1_machine();
        machine.write("/var/lib/liftoff/liftoff.db", "");
        let result = machine
            .batch(with_import("/var/lib/liftoff/liftoff.db"))
            .await;
        assert_eq!(exit_code(result), Some(64));
    }

    // TC: IMP-01
    #[tokio::test]
    async fn exits_64_when_the_import_file_is_missing() {
        let machine = Machine::new();
        let result = machine.batch(with_import(V1_DB)).await;
        assert_eq!(exit_code(result), Some(64));
    }

    // TC: IMP-01
    #[tokio::test]
    async fn names_the_issuer_when_import_has_none() {
        let machine = v1_machine();
        let args = SetupArgs {
            import_issuer: None,
            ..with_import(V1_DB)
        };
        assert_eq!(
            missing(machine.batch(args).await),
            Some(vec!["--import-issuer"])
        );
    }

    // TC: IMP-01 (users of a malformed issuer could never log in)
    #[tokio::test]
    async fn exits_64_when_the_import_issuer_is_not_an_https_url() {
        let machine = v1_machine();
        let args = SetupArgs {
            import_issuer: Some("auth.example.com".to_owned()),
            ..with_import(V1_DB)
        };
        assert_eq!(exit_code(machine.batch(args).await), Some(64));
    }

    // TC: IMP-06 (the v1 database stays where v1 runs it; a copy imported from elsewhere never makes setup take the v1 file for its own database and migrate it in place)
    #[tokio::test]
    async fn leaves_v1_database_unchanged_when_it_is_at_the_database_path() {
        let machine = v1_machine();
        let in_place = machine.file("/var/lib/liftoff/liftoff.db");
        std::fs::create_dir_all(in_place.parent().unwrap()).unwrap();
        let _bytes = std::fs::copy(machine.file(V1_DB), &in_place).unwrap();
        let before = std::fs::read(&in_place).unwrap();
        let result = machine.batch(with_import(V1_DB)).await;
        assert_eq!(
            (
                exit_code(result),
                std::fs::read(&in_place).unwrap() == before
            ),
            (Some(70), true)
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn touches_nothing_when_the_import_is_refused() {
        let machine = Machine::new();
        let args = SetupArgs {
            public_ip: Some(Ipv4Addr::new(203, 0, 113, 10)),
            ..with_import(V1_DB)
        };
        let _refused = machine.batch(args).await;
        assert_eq!(
            (machine.events(), machine.files()),
            (Vec::new(), Vec::<String>::new())
        );
    }
}
