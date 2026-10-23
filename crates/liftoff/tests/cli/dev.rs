//! `lof dev` run locally (`DEV-01…05`).
//!
//! The binary itself runs as a child of the test, with the config home
//! of a [`Cli`] (`LIFTOFF_CONFIG`, `HOME` in its tempdir, nothing else of
//! the test's environment but `PATH`), so the dev command it starts is a
//! real process and Ctrl-C is a real `SIGINT`. The `[dev] command` of the
//! project is `sh <tempdir>/dev.sh`, a script that records its arguments,
//! its environment and the command line of its parent next to it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::{Child, Command};

use crate::support::{Cli, Stub, json};

/// A value no other file of the test holds.
const VALUE: &str = "s3cr3t-dev-value";

/// How long a run of `lof dev` may take before the test gives up.
const PATIENCE: Duration = Duration::from_secs(60);

/// `[service]` of every manifest here.
const SERVICE: &str = "[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n";

/// A `package.json` whose framework is Vite.
const VITE: &str = r#"{"scripts": {"dev": "vite"}, "devDependencies": {"vite": "^5.4.0"}}"#;

/// A `package.json` whose framework is Hono.
const HONO: &str =
    r#"{"scripts": {"dev": "bun run src/index.ts"}, "dependencies": {"hono": "^4.6.0"}}"#;

/// The script that records how it was started.
fn recorder(root: &Path) -> String {
    let root = root.display();
    format!(
        "for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done > {root}/rec.args\n\
         tr '\\000' '\\n' < /proc/$PPID/cmdline > {root}/rec.parent\n\
         env > {root}/rec.env\n"
    )
}

/// A project in the working tree of a fresh [`Cli`] whose `[dev] command`
/// runs the script `script(root)` (`dev.sh`) gives for the root of the
/// working tree, with `package_json` beside it.
fn project_running(script: impl FnOnce(&Path) -> String, package_json: Option<&str>) -> Cli {
    let cli = Cli::new();
    let path = cli.root.join("dev.sh");
    std::fs::write(&path, script(&cli.root)).unwrap();
    cli.write_root(
        "liftoff.toml",
        &format!("{SERVICE}\n[dev]\ncommand = \"sh {}\"\n", path.display()),
    );
    if let Some(text) = package_json {
        cli.write_root("package.json", text);
    }
    cli
}

/// A project whose dev command is [`recorder`].
fn recording(package_json: Option<&str>) -> Cli {
    project_running(recorder, package_json)
}

/// The home directory of `cli`'s config (`<home>/.config/liftoff/config.toml`).
fn home_of(cli: &Cli) -> PathBuf {
    cli.config.ancestors().nth(3).unwrap().to_path_buf()
}

/// `liftoff <args>` started as a process with the config home of `cli`.
pub(crate) fn spawn(cli: &Cli, args: &[&str]) -> Child {
    spawn_on_path(cli, args, None)
}

/// [`spawn`] with `first` ahead of the test's `PATH`.
fn spawn_on_path(cli: &Cli, args: &[&str], first: Option<&Path>) -> Child {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = first.map_or_else(
        || inherited.clone(),
        |first| {
            std::env::join_paths(
                std::iter::once(first.to_path_buf()).chain(std::env::split_paths(&inherited)),
            )
            .unwrap()
        },
    );
    Command::new(env!("CARGO_BIN_EXE_liftoff"))
        .args(args)
        .env_clear()
        .env("PATH", path)
        .env("HOME", home_of(cli))
        .env("LIFTOFF_CONFIG", &cli.config)
        .current_dir(&cli.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("liftoff starts")
}

/// What a finished run of the binary printed and returned.
#[derive(Debug)]
pub(crate) struct Ran {
    pub(crate) code: Option<i32>,
    pub(crate) out: String,
    pub(crate) err: String,
}

impl From<Output> for Ran {
    fn from(output: Output) -> Self {
        Self {
            code: output.status.code(),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// Runs `liftoff <args>` to its end.
pub(crate) async fn lof(cli: &Cli, args: &[&str]) -> Ran {
    wait(spawn(cli, args)).await
}

/// The run of `child` to its end.
async fn wait(child: Child) -> Ran {
    tokio::time::timeout(PATIENCE, child.wait_with_output())
        .await
        .expect("liftoff ends")
        .unwrap()
        .into()
}

/// `lof --project demo dev --local`, to its end.
async fn dev_local(cli: &Cli) -> Ran {
    lof(cli, &["--project", "demo", "dev", "--local"]).await
}

/// A file the recorder wrote.
fn recorded(cli: &Cli, name: &str, ran: &Ran) -> String {
    std::fs::read_to_string(cli.root.join(name))
        .unwrap_or_else(|error| panic!("the dev command did not record {name} ({error}): {ran:?}"))
}

/// The arguments the dev command got.
fn args_of(cli: &Cli, ran: &Ran) -> Vec<String> {
    recorded(cli, "rec.args", ran)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The environment the dev command got.
fn env_of(cli: &Cli, ran: &Ran) -> BTreeMap<String, String> {
    recorded(cli, "rec.env", ran)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// `PORT` of the dev command.
fn port_of(cli: &Cli, ran: &Ran) -> u16 {
    env_of(cli, ran)
        .get("PORT")
        .unwrap_or_else(|| panic!("no PORT: {ran:?}"))
        .parse()
        .unwrap()
}

mod port {
    use super::*;

    // TC: DEV-01
    #[tokio::test]
    async fn sets_free_port_of_dev_range_other_than_busy_one_when_port_is_taken() {
        let busy = (4000..=4999)
            .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
            .expect("a port of 4000–4999 is free");
        let held = busy.local_addr().unwrap().port();
        let cli = recording(None);
        let ran = dev_local(&cli).await;
        let port = port_of(&cli, &ran);
        assert!(
            port != held && (4000..=4999).contains(&port),
            "PORT={port}, busy {held}: {ran:?}"
        );
        drop(busy);
    }

    // TC: DEV-01 (a dev server on `localhost` may listen on `[::1]` only)
    #[tokio::test]
    async fn sets_port_other_than_one_busy_on_ipv6_loopback_when_port_is_taken() {
        let busy = (4000..=4999)
            .find_map(|port| {
                let v4 = std::net::TcpListener::bind(("0.0.0.0", port)).ok()?;
                drop(v4);
                std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).ok()
            })
            .expect("a port of 4000–4999 is free on [::1]");
        let held = busy.local_addr().unwrap().port();
        let cli = recording(None);
        let ran = dev_local(&cli).await;
        let port = port_of(&cli, &ran);
        assert!(port != held, "PORT={port}, busy on [::1]: {ran:?}");
        drop(busy);
    }

    // TC: DEV-01 (`http://localhost:<PORT>`)
    #[tokio::test]
    async fn prints_localhost_url_of_port_when_run() {
        let cli = recording(None);
        let ran = dev_local(&cli).await;
        let port = port_of(&cli, &ran);
        assert!(
            ran.out.contains(&format!("http://localhost:{port}")),
            "{ran:?}"
        );
    }
}

mod port_flag {
    use super::*;

    /// A project whose dev command is [`recorder`] run with `--port 4567`.
    fn naming_port(package_json: Option<&str>) -> Cli {
        let cli = recording(package_json);
        cli.write_root(
            "liftoff.toml",
            &format!(
                "{SERVICE}\n[dev]\ncommand = \"sh {} --port 4567\"\n",
                cli.root.join("dev.sh").display()
            ),
        );
        cli
    }

    // TC: DEV-02 (the port a command names is the one its server listens on)
    #[tokio::test]
    async fn sets_port_named_by_command_when_command_names_port() {
        let cli = naming_port(Some(VITE));
        let ran = dev_local(&cli).await;
        assert_eq!(port_of(&cli, &ran), 4567, "{ran:?}");
    }

    // TC: DEV-02
    #[tokio::test]
    async fn prints_url_of_port_named_by_command_when_command_names_port() {
        let cli = naming_port(Some(VITE));
        let ran = dev_local(&cli).await;
        assert!(ran.out.contains("http://localhost:4567"), "{ran:?}");
    }

    // TC: DEV-02
    #[tokio::test]
    async fn passes_command_port_flag_once_when_command_names_port() {
        let cli = naming_port(Some(VITE));
        let ran = dev_local(&cli).await;
        assert_eq!(
            args_of(&cli, &ran),
            vec!["--port".to_owned(), "4567".to_owned()],
            "{ran:?}"
        );
    }

    // TC: DEV-02
    #[tokio::test]
    async fn exits_64_when_port_flag_differs_from_port_named_by_command() {
        let cli = naming_port(Some(VITE));
        let ran = lof(
            &cli,
            &["--project", "demo", "dev", "--local", "--port", "4600"],
        )
        .await;
        assert_eq!(ran.code, Some(64), "{ran:?}");
    }

    // TC: DEV-02
    #[tokio::test]
    async fn passes_port_flag_of_port_when_framework_is_vite() {
        let cli = recording(Some(VITE));
        let ran = dev_local(&cli).await;
        let port = port_of(&cli, &ran);
        assert_eq!(
            args_of(&cli, &ran),
            vec!["--port".to_owned(), port.to_string()],
            "{ran:?}"
        );
    }

    // TC: DEV-02
    #[tokio::test]
    async fn passes_no_argument_when_framework_is_hono() {
        let cli = recording(Some(HONO));
        let ran = dev_local(&cli).await;
        assert_eq!(args_of(&cli, &ran), Vec::<String>::new(), "{ran:?}");
    }

    // TC: DEV-02
    #[tokio::test]
    async fn sets_port_of_dev_range_when_framework_is_hono() {
        let cli = recording(Some(HONO));
        let ran = dev_local(&cli).await;
        let port = port_of(&cli, &ran);
        assert!((4000..=4999).contains(&port), "PORT={port}: {ran:?}");
    }
}

mod secrets {
    use super::*;

    /// A project whose `API_KEY` is set in the local store (environment
    /// `prod` of `demo`, the default scope).
    async fn with_local_secret() -> Cli {
        let cli = recording(None);
        let outcome = cli
            .run(&[
                "--project",
                "demo",
                "secret",
                "set",
                &format!("API_KEY={VALUE}"),
            ])
            .await;
        assert_eq!(outcome.code, 0, "secret set: {outcome:?}");
        cli
    }

    /// A server whose `GET /secrets/resolve` answers `API_KEY`, and
    /// whose `GET /server` answers the base domain the tunnel
    /// asks for (it has no `GET /tunnel`: the command runs locally
    /// while the tunnel retries).
    async fn resolving() -> Stub {
        Stub::start()
            .await
            .route(
                "GET",
                "/secrets/resolve",
                vec![json(
                    200,
                    &format!(r#"{{"vars": [{{"key": "API_KEY", "value": "{VALUE}"}}]}}"#),
                )],
            )
            .route(
                "GET",
                "/server",
                vec![json(
                    200,
                    r#"{"base_domain":"west.test","public_ip":"203.0.113.7","checks":[]}"#,
                )],
            )
    }

    // TC: DEV-03
    #[tokio::test]
    async fn puts_local_secret_in_environment_when_no_server() {
        let cli = with_local_secret().await;
        let ran = dev_local(&cli).await;
        assert_eq!(
            env_of(&cli, &ran).get("API_KEY").map(String::as_str),
            Some(VALUE),
            "{ran:?}"
        );
    }

    // TC: DEV-03 (the system variables on top, as the server adds them)
    #[tokio::test]
    async fn puts_system_variables_in_environment_when_no_server() {
        let cli = with_local_secret().await;
        let ran = dev_local(&cli).await;
        let env = env_of(&cli, &ran);
        let system: Vec<(&str, Option<&str>)> =
            ["LIFTOFF_PROJECT", "LIFTOFF_ENV", "LIFTOFF_DOMAIN"]
                .into_iter()
                .map(|key| (key, env.get(key).map(String::as_str)))
                .collect();
        assert_eq!(
            system,
            vec![
                ("LIFTOFF_PROJECT", Some("demo")),
                ("LIFTOFF_ENV", Some("prod")),
                ("LIFTOFF_DOMAIN", Some("")),
            ],
            "{ran:?}"
        );
    }

    // TC: DEV-03
    #[tokio::test]
    async fn keeps_local_secret_out_of_arguments_when_no_server() {
        let cli = with_local_secret().await;
        let ran = dev_local(&cli).await;
        let parent = recorded(&cli, "rec.parent", &ran);
        let args = recorded(&cli, "rec.args", &ran);
        assert!(
            !args.contains(VALUE) && !parent.contains(VALUE),
            "args {args:?}, parent {parent:?}"
        );
    }

    // TC: DEV-03
    #[tokio::test(flavor = "multi_thread")]
    async fn puts_resolved_secret_in_environment_when_server_is_configured() {
        let stub = resolving().await;
        let cli = recording(None);
        cli.log_in(&stub.url);
        let ran = lof(&cli, &["--project", "demo", "dev"]).await;
        assert_eq!(
            env_of(&cli, &ran).get("API_KEY").map(String::as_str),
            Some(VALUE),
            "{ran:?}"
        );
    }

    // TC: DEV-03
    #[tokio::test(flavor = "multi_thread")]
    async fn asks_resolve_of_project_and_prod_when_server_is_configured() {
        let stub = resolving().await;
        let cli = recording(None);
        cli.log_in(&stub.url);
        let ran = lof(&cli, &["--project", "demo", "dev"]).await;
        let asked: Vec<Vec<(String, String)>> = stub
            .seen_at("GET", "/secrets/resolve")
            .iter()
            .map(crate::support::Seen::query_pairs)
            .collect();
        assert_eq!(
            asked,
            vec![vec![
                ("project".to_owned(), "demo".to_owned()),
                ("env".to_owned(), "prod".to_owned()),
            ]],
            "{ran:?}"
        );
    }

    // TC: DEV-03
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_resolved_secret_out_of_arguments_when_server_is_configured() {
        let stub = resolving().await;
        let cli = recording(None);
        cli.log_in(&stub.url);
        let ran = lof(&cli, &["--project", "demo", "dev"]).await;
        let parent = recorded(&cli, "rec.parent", &ran);
        let args = recorded(&cli, "rec.args", &ran);
        assert!(
            !args.contains(VALUE) && !parent.contains(VALUE),
            "args {args:?}, parent {parent:?}"
        );
    }
}

mod ctrl_c {
    use super::*;

    /// Kills the grandchild whatever the test's outcome.
    struct Reaper(Option<u32>);

    impl Drop for Reaper {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                drop(
                    std::process::Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .stderr(Stdio::null())
                        .status(),
                );
            }
        }
    }

    /// A project whose dev command starts `sleep 100` in the background,
    /// records its pid and waits for it.
    fn sleeping() -> Cli {
        project_running(
            |root| {
                format!(
                    "sleep 100 &\necho $! > {}\nwait\n",
                    root.join("grandchild.pid").display()
                )
            },
            None,
        )
    }

    /// The pid of the backgrounded `sleep`, once recorded.
    async fn grandchild(root: &Path, child: &mut Child) -> u32 {
        let path = root.join("grandchild.pid");
        let wait = async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                {
                    return pid;
                }
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("liftoff ended with {status} before the dev command started");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .expect("the dev command records its grandchild")
    }

    /// Sends `SIGINT` to `pid` only.
    fn interrupt(pid: u32) {
        send("INT", pid);
    }

    /// Sends the signal `name` to `pid` only.
    fn send(name: &str, pid: u32) {
        let status = std::process::Command::new("kill")
            .args([&format!("-{name}"), &pid.to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "kill -{name} {pid}");
    }

    /// Whether `pid` ends within [`PATIENCE`].
    async fn ends(pid: u32) -> bool {
        for _ in 0..200 {
            if gone(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        gone(pid)
    }

    /// The grandchild of a run that gets the signal `name` outlives it.
    async fn grandchild_ends_when_cli_gets(name: &str) -> bool {
        let cli = sleeping();
        let mut child = spawn(&cli, &["--project", "demo", "dev", "--local"]);
        let pid = grandchild(&cli.root, &mut child).await;
        let _reaper = Reaper(Some(pid));
        send(name, child.id().unwrap());
        let _ended = tokio::time::timeout(PATIENCE, child.wait()).await;
        ends(pid).await
    }

    // TC: DEV-04 (`kill <pid>` of the CLI)
    #[tokio::test]
    async fn ends_grandchild_when_cli_gets_sigterm() {
        assert!(
            grandchild_ends_when_cli_gets("TERM").await,
            "the grandchild outlived SIGTERM"
        );
    }

    // TC: DEV-04 (the terminal closed)
    #[tokio::test]
    async fn ends_grandchild_when_cli_gets_sighup() {
        assert!(
            grandchild_ends_when_cli_gets("HUP").await,
            "the grandchild outlived SIGHUP"
        );
    }

    // TC: DEV-04 (the grace period is the group's, not the shell's)
    #[tokio::test]
    async fn lets_grandchild_clean_up_after_sigterm_when_cli_gets_sigint() {
        let cli = project_running(
            |root| {
                let root = root.display();
                format!(
                    "sh -c 'trap \"sleep 1; echo done > {root}/cleaned; exit 0\" TERM; \
                     echo $$ > {root}/grandchild.pid; while :; do sleep 0.1; done' &\nwait\n"
                )
            },
            None,
        );
        let mut child = spawn(&cli, &["--project", "demo", "dev", "--local"]);
        let pid = grandchild(&cli.root, &mut child).await;
        let _reaper = Reaper(Some(pid));
        interrupt(child.id().unwrap());
        let _ended = tokio::time::timeout(PATIENCE, child.wait()).await;
        assert!(ends(pid).await, "the grandchild {pid} outlived Ctrl-C");
        assert!(
            cli.root.join("cleaned").exists(),
            "the grandchild got SIGKILL before it cleaned up"
        );
    }

    /// Whether `pid` is gone (no process, or a zombie nobody reaped yet).
    fn gone(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
    }

    // TC: DEV-04
    #[tokio::test]
    async fn ends_grandchild_when_cli_gets_sigint() {
        let cli = sleeping();
        let mut child = spawn(&cli, &["--project", "demo", "dev", "--local"]);
        let pid = grandchild(&cli.root, &mut child).await;
        let _reaper = Reaper(Some(pid));
        interrupt(child.id().unwrap());
        let _ended = tokio::time::timeout(PATIENCE, child.wait()).await;
        let mut ended = gone(pid);
        for _ in 0..200 {
            if ended {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            ended = gone(pid);
        }
        assert!(ended, "the grandchild {pid} outlived Ctrl-C");
    }

    // TC: DEV-04
    #[tokio::test]
    async fn exits_when_cli_gets_sigint() {
        let cli = sleeping();
        let mut child = spawn(&cli, &["--project", "demo", "dev", "--local"]);
        let pid = grandchild(&cli.root, &mut child).await;
        let _reaper = Reaper(Some(pid));
        interrupt(child.id().unwrap());
        let ended = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
        assert!(ended.is_ok(), "liftoff still runs 20 s after Ctrl-C");
    }
}

mod tunnel_note {
    use super::*;

    /// How often `text` is in `haystack`.
    fn count(haystack: &str, text: &str) -> usize {
        haystack.matches(text).count()
    }

    // TC: DEV-03 (without a server there is no tunnel)
    #[tokio::test]
    async fn says_once_there_is_no_tunnel_when_no_server_and_not_local() {
        let cli = recording(None);
        let ran = lof(&cli, &["--project", "demo", "dev"]).await;
        assert_eq!(count(&ran.err, "without a tunnel"), 1, "{ran:?}");
    }

    // TC: DEV-03 (`--local` silences the note)
    #[tokio::test]
    async fn says_nothing_of_tunnel_when_local() {
        let cli = recording(None);
        let ran = dev_local(&cli).await;
        assert!(!ran.err.contains("tunnel"), "{ran:?}");
    }
}

mod detected {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// A project with no `[dev]` whose stack proposes `npm run dev`, and a
    /// directory holding an `npm` that records how it was started.
    fn detectable() -> (Cli, PathBuf) {
        let cli = Cli::new();
        cli.write_root("liftoff.toml", SERVICE);
        cli.write_root("package.json", HONO);
        let bin = cli.root.parent().unwrap().join("stub-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let npm = bin.join("npm");
        std::fs::write(&npm, format!("#!/bin/sh\n{}", recorder(&cli.root))).unwrap();
        std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
        (cli, bin)
    }

    // TC: DEV-05 (`[dev] command` or detection)
    #[tokio::test]
    async fn runs_detected_command_when_manifest_has_no_dev() {
        let (cli, bin) = detectable();
        let ran = wait(spawn_on_path(
            &cli,
            &["--project", "demo", "dev", "--local"],
            Some(&bin),
        ))
        .await;
        assert_eq!(
            args_of(&cli, &ran),
            vec!["run".to_owned(), "dev".to_owned()],
            "{ran:?}"
        );
    }

    // TC: DEV-01 (the detected command gets `PORT` too)
    #[tokio::test]
    async fn sets_port_of_dev_range_when_command_is_detected() {
        let (cli, bin) = detectable();
        let ran = wait(spawn_on_path(
            &cli,
            &["--project", "demo", "dev", "--local"],
            Some(&bin),
        ))
        .await;
        let port = port_of(&cli, &ran);
        assert!((4000..=4999).contains(&port), "PORT={port}: {ran:?}");
    }
}

mod no_command {
    use super::*;

    // TC: DEV-05
    #[tokio::test]
    async fn exits_64_when_no_manifest_and_nothing_detected() {
        let cli = Cli::new();
        let ran = dev_local(&cli).await;
        assert_eq!(ran.code, Some(64), "{ran:?}");
    }

    // TC: DEV-05
    #[tokio::test]
    async fn exits_64_when_manifest_has_no_dev_and_nothing_detected() {
        let cli = Cli::new();
        cli.write_root("liftoff.toml", SERVICE);
        let ran = dev_local(&cli).await;
        assert_eq!(ran.code, Some(64), "{ran:?}");
    }

    // TC: DEV-05
    #[tokio::test]
    async fn hints_to_write_dev_command_in_manifest_when_nothing_detected() {
        let cli = Cli::new();
        cli.write_root("liftoff.toml", SERVICE);
        let ran = dev_local(&cli).await;
        assert!(
            ran.err.contains("[dev]") && ran.err.contains("liftoff.toml"),
            "{ran:?}"
        );
    }
}
