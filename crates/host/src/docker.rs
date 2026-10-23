//! `docker` and `docker compose` through [`Cmd`].

use core::time::Duration;
use std::ffi::OsString;
use std::path::PathBuf;

use futures_core::Stream;
use liftoff_core::domain::{ComposeProject, Host, Login, ServiceName};
use liftoff_core::ports::{CommandOutput, Docker, ServiceStatus};
use liftoff_core::{Error, System};
use serde::Deserialize;

use crate::cmd::{Cmd, failure_detail};

const DOCKER: &str = "docker";

/// Runs `docker compose` in `<projects_dir>/<project>/` with `DOCKER_CONFIG`
/// pointing at liftoff's own docker config (the registry login lives there).
#[derive(Debug, Clone)]
pub struct ProcessDocker {
    projects_dir: PathBuf,
    config_dir: PathBuf,
    program: OsString,
}

impl ProcessDocker {
    /// `projects_dir` holds one directory per project; `config_dir` is
    /// exported as `DOCKER_CONFIG`.
    #[must_use]
    pub fn new(projects_dir: impl Into<PathBuf>, config_dir: impl Into<PathBuf>) -> Self {
        Self {
            projects_dir: projects_dir.into(),
            config_dir: config_dir.into(),
            program: DOCKER.into(),
        }
    }

    /// Replaces the `docker` binary, so tests can run a stub.
    #[cfg(test)]
    fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    fn base(&self) -> Cmd {
        // Compose resolves `${VAR}` from the process environment before `.env`,
        // so a user's compose file could read the daemon's variables. Pass on only
        // what docker needs.
        let mut cmd = Cmd::new(self.program.clone())
            .env_clear()
            .env("DOCKER_CONFIG", self.config_dir.as_os_str())
            .env("HOME", self.config_dir.as_os_str());
        if let Some(path) = std::env::var_os("PATH") {
            cmd = cmd.env("PATH", path);
        }
        cmd
    }

    /// `docker compose -p <project> -f compose.yaml -f liftoff.override.yaml
    /// <rest>` in the project directory.
    fn compose<I, S>(&self, project: &ComposeProject, rest: I) -> Cmd
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.base()
            .cwd(self.projects_dir.join(project.as_str()))
            .args([
                "compose",
                "-p",
                project.as_str(),
                "-f",
                "compose.yaml",
                "-f",
                "liftoff.override.yaml",
            ])
            .args(rest)
    }

    /// `docker version --format {{.Server.Version}}`: fails when the daemon
    /// is unreachable, which is what the readiness probe wants to know.
    fn version_cmd(&self) -> Cmd {
        self.base()
            .args(["version", "--format", "{{.Server.Version}}"])
    }

    /// Whether the docker daemon answers (`docker version`).
    ///
    /// # Errors
    ///
    /// [`Error::Upstream`] (system `docker`) when the command cannot run or
    /// exits non-zero.
    pub async fn version(&self) -> Result<(), Error> {
        self.version_cmd().run_checked(System::Docker).await?;
        Ok(())
    }

    fn login_cmd(&self, registry: &Host, user: &Login) -> Cmd {
        self.base().args([
            "login",
            registry.as_str(),
            "--username",
            user.as_str(),
            "--password-stdin",
        ])
    }

    fn pull_cmd(&self, project: &ComposeProject) -> Cmd {
        self.compose(project, ["pull"])
    }

    fn up_cmd(&self, project: &ComposeProject, wait_timeout: Duration) -> Cmd {
        self.compose(
            project,
            [
                "up",
                "-d",
                "--remove-orphans",
                "--wait",
                "--wait-timeout",
                &wait_timeout.as_secs().to_string(),
            ],
        )
    }

    fn down_cmd(&self, project: &ComposeProject, volumes: bool) -> Cmd {
        let mut rest = vec!["down"];
        if volumes {
            rest.push("-v");
        }
        self.compose(project, rest)
    }

    /// Status needs only the project label, so no files and no directory.
    fn ps_cmd(&self, project: &ComposeProject) -> Cmd {
        self.base()
            .args(["compose", "-p", project.as_str(), "ps", "--format", "json"])
    }

    fn logs_cmd(
        &self,
        project: &ComposeProject,
        service: Option<&ServiceName>,
        tail: Option<u32>,
        follow: bool,
    ) -> Cmd {
        let mut rest = vec!["logs".to_owned(), "--no-color".to_owned()];
        if let Some(tail) = tail {
            rest.push("--tail".to_owned());
            rest.push(tail.to_string());
        }
        if follow {
            rest.push("-f".to_owned());
        }
        if let Some(service) = service {
            rest.push(service.as_str().to_owned());
        }
        self.compose(project, rest)
    }
}

/// One entry of `docker compose ps --format json`; other fields are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsEntry {
    service: String,
    state: String,
    #[serde(default)]
    health: String,
}

impl From<PsEntry> for ServiceStatus {
    fn from(entry: PsEntry) -> Self {
        Self {
            service: entry.service,
            state: entry.state,
            health: Some(entry.health).filter(|health| !health.is_empty()),
        }
    }
}

/// Parses the output of `compose ps --format json`: Compose v2 prints either
/// one JSON array or, since 2.21, one JSON object per line.
fn parse_ps(output: &str) -> Result<Vec<ServiceStatus>, Error> {
    let bad = |source: serde_json::Error| {
        Error::upstream(
            System::Docker,
            format!("unreadable `compose ps` output: {source}"),
        )
    };
    let trimmed = output.trim();
    let entries: Vec<PsEntry> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).map_err(bad)?
    } else {
        trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(bad))
            .collect::<Result<_, _>>()?
    };
    Ok(entries.into_iter().map(ServiceStatus::from).collect())
}

impl Docker for ProcessDocker {
    async fn login(
        &self,
        registry: &Host,
        user: &Login,
        token: &str,
    ) -> Result<CommandOutput, Error> {
        self.login_cmd(registry, user)
            .stdin(token)
            .run(System::Docker)
            .await
    }

    async fn compose_pull(&self, project: &ComposeProject) -> Result<CommandOutput, Error> {
        self.pull_cmd(project).run(System::Docker).await
    }

    async fn compose_up(
        &self,
        project: &ComposeProject,
        wait_timeout: Duration,
    ) -> Result<CommandOutput, Error> {
        self.up_cmd(project, wait_timeout).run(System::Docker).await
    }

    async fn compose_down(
        &self,
        project: &ComposeProject,
        volumes: bool,
    ) -> Result<CommandOutput, Error> {
        self.down_cmd(project, volumes).run(System::Docker).await
    }

    async fn compose_ps(&self, project: &ComposeProject) -> Result<Vec<ServiceStatus>, Error> {
        let output = self.ps_cmd(project).run(System::Docker).await?;
        if !output.success() {
            return Err(Error::upstream(System::Docker, failure_detail(&output)));
        }
        let stdout: Vec<&str> = output
            .lines
            .iter()
            .filter(|line| line.stream == liftoff_core::ports::OutputStream::Stdout)
            .map(|line| line.text.as_str())
            .collect();
        parse_ps(&stdout.join("\n"))
    }

    fn logs(
        &self,
        project: &ComposeProject,
        service: Option<&ServiceName>,
        tail: Option<u32>,
        follow: bool,
    ) -> impl Future<
        Output = Result<impl Stream<Item = Result<String, Error>> + Send + 'static, Error>,
    > + Send {
        std::future::ready(
            self.logs_cmd(project, service, tail, follow)
                .stream(System::Docker),
        )
    }
}

#[cfg(test)]
mod tests {
    use liftoff_core::domain::{EnvName, ProjectName};

    use super::*;

    fn docker() -> ProcessDocker {
        ProcessDocker::new("/srv/liftoff/compose", "/var/lib/liftoff/docker")
    }

    fn project() -> ComposeProject {
        let project = ProjectName::new("acme").expect("valid project");
        let env = EnvName::new("qa").expect("valid env");
        ComposeProject::new(&project, &env)
    }

    fn argv(cmd: &Cmd) -> Vec<String> {
        cmd.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn strs(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    mod commands {
        use super::*;

        // TC: v1 ADP-01
        #[test]
        fn pull_uses_both_files_when_project_is_lf_acme_qa() {
            let cmd = docker().pull_cmd(&project());
            assert_eq!(cmd.program, "docker");
            assert_eq!(
                argv(&cmd),
                strs(&[
                    "compose",
                    "-p",
                    "lf_acme_qa",
                    "-f",
                    "compose.yaml",
                    "-f",
                    "liftoff.override.yaml",
                    "pull"
                ])
            );
        }

        // TC: v1 ADP-01
        #[test]
        fn up_waits_and_removes_orphans_when_timeout_is_120s() {
            let cmd = docker().up_cmd(&project(), Duration::from_secs(120));
            let argv = argv(&cmd);
            assert_eq!(
                argv.get(7..),
                Some(
                    strs(&[
                        "up",
                        "-d",
                        "--remove-orphans",
                        "--wait",
                        "--wait-timeout",
                        "120"
                    ])
                    .as_slice()
                )
            );
            assert_eq!(argv.get(..7), Some(argv_prefix().as_slice()));
        }

        fn argv_prefix() -> Vec<String> {
            strs(&[
                "compose",
                "-p",
                "lf_acme_qa",
                "-f",
                "compose.yaml",
                "-f",
                "liftoff.override.yaml",
            ])
        }

        // TC: v1 ADP-01
        #[test]
        fn down_has_no_volumes_flag_when_volumes_not_requested() {
            let cmd = docker().down_cmd(&project(), false);
            assert_eq!(argv(&cmd).get(7..), Some(strs(&["down"]).as_slice()));
        }

        // TC: v1 ADP-01
        #[test]
        fn down_removes_volumes_when_requested() {
            let cmd = docker().down_cmd(&project(), true);
            assert_eq!(argv(&cmd).get(7..), Some(strs(&["down", "-v"]).as_slice()));
        }

        // TC: v1 ADP-01
        #[test]
        fn logs_tail_follow_and_service_when_all_given() {
            let service = ServiceName::new("web").expect("valid service");
            let cmd = docker().logs_cmd(&project(), Some(&service), Some(50), true);
            assert_eq!(
                argv(&cmd).get(7..),
                Some(strs(&["logs", "--no-color", "--tail", "50", "-f", "web"]).as_slice())
            );
        }

        // TC: v1 ADP-01
        #[test]
        fn logs_are_bare_when_no_options_given() {
            let cmd = docker().logs_cmd(&project(), None, None, false);
            assert_eq!(
                argv(&cmd).get(7..),
                Some(strs(&["logs", "--no-color"]).as_slice())
            );
        }

        // TC: v1 ADP-01
        #[test]
        fn only_the_needed_variables_reach_docker_when_any_command_is_built() {
            let mut keys: Vec<String> = docker()
                .pull_cmd(&project())
                .env
                .iter()
                .map(|(key, _)| key.to_string_lossy().into_owned())
                .collect();
            keys.sort();
            let wanted: &[&str] = if std::env::var_os("PATH").is_some() {
                &["DOCKER_CONFIG", "HOME", "PATH"]
            } else {
                &["DOCKER_CONFIG", "HOME"]
            };
            assert!(docker().pull_cmd(&project()).clear_env);
            assert_eq!(keys, wanted);
        }

        // TC: v1 ADP-01
        #[test]
        fn docker_config_is_set_when_any_command_is_built() {
            let expected = (
                OsString::from("DOCKER_CONFIG"),
                OsString::from("/var/lib/liftoff/docker"),
            );
            for cmd in [
                docker().pull_cmd(&project()),
                docker().ps_cmd(&project()),
                docker().down_cmd(&project(), false),
                docker().up_cmd(&project(), Duration::from_secs(1)),
                docker().logs_cmd(&project(), None, None, false),
                docker().login_cmd(
                    &Host::new("git.example.com").expect("valid host"),
                    &Login::new("bob").expect("valid login"),
                ),
            ] {
                assert!(cmd.env.contains(&expected), "env of {cmd:?}");
            }
        }

        // TC: v1 ADP-01
        #[test]
        fn runs_in_the_project_directory_when_compose_files_are_relative() {
            let cmd = docker().pull_cmd(&project());
            assert_eq!(
                cmd.cwd,
                Some(PathBuf::from("/srv/liftoff/compose/lf_acme_qa"))
            );
        }

        // TC: v1 ADP-01
        #[test]
        fn ps_asks_for_json_by_project_when_status_is_read() {
            let cmd = docker().ps_cmd(&project());
            assert_eq!(
                argv(&cmd),
                strs(&["compose", "-p", "lf_acme_qa", "ps", "--format", "json"])
            );
        }

        // TC: v1 ADP-01 (the token goes through stdin, never argv)
        #[test]
        fn login_reads_the_token_from_stdin_when_registry_given() {
            let registry = Host::new("git.example.com").expect("valid host");
            let user = Login::new("bob").expect("valid login");
            let cmd = docker().login_cmd(&registry, &user);
            assert_eq!(
                argv(&cmd),
                strs(&[
                    "login",
                    "git.example.com",
                    "--username",
                    "bob",
                    "--password-stdin"
                ])
            );
        }
    }

    mod trait_methods {
        use std::os::unix::fs::PermissionsExt as _;

        use liftoff_core::ports::OutputStream;

        use super::*;

        /// A stub `docker` that prints argv, cwd, `DOCKER_CONFIG` and stdin,
        /// plus whatever `extra` adds. Returns the guard and the docker.
        fn stub(extra: &str) -> (tempfile::TempDir, ProcessDocker) {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(dir.path().join("compose/lf_acme_qa")).expect("project dir");
            let script = dir.path().join("docker");
            let body = format!(
                "#!/bin/sh\nfor a in \"$@\"; do echo \"arg:$a\"; done\n\
                 echo \"cwd:$(pwd)\"\necho \"config:$DOCKER_CONFIG\"\n\
                 {extra}\n"
            );
            std::fs::write(&script, body).expect("write stub");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            let docker =
                ProcessDocker::new(dir.path().join("compose"), "/cfg").with_program(script);
            (dir, docker)
        }

        fn stdout(output: &CommandOutput) -> Vec<&str> {
            output
                .lines
                .iter()
                .filter(|l| l.stream == OutputStream::Stdout)
                .map(|l| l.text.as_str())
                .collect()
        }

        // TC: v1 ADP-01
        #[tokio::test]
        async fn down_passes_volumes_flag_when_requested() {
            let (_dir, docker) = stub("");
            let output = docker.compose_down(&project(), true).await.expect("runs");
            let lines = stdout(&output);
            assert!(lines.contains(&"arg:-v"));
            assert!(lines.contains(&"config:/cfg"));
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("cwd:") && l.ends_with("/compose/lf_acme_qa"))
            );
            let output = docker.compose_down(&project(), false).await.expect("runs");
            assert!(!stdout(&output).contains(&"arg:-v"));
        }

        #[tokio::test]
        async fn version_asks_the_server_when_called() {
            let (_dir, docker) = stub("");
            docker.version().await.expect("stub exits zero");
            let output = docker
                .version_cmd()
                .run(System::Docker)
                .await
                .expect("runs");
            let lines = stdout(&output);
            assert_eq!(
                (
                    lines.contains(&"arg:version"),
                    lines.contains(&"arg:{{.Server.Version}}")
                ),
                (true, true)
            );
        }

        #[tokio::test]
        async fn version_fails_when_docker_exits_non_zero() {
            let (_dir, docker) = stub("echo 'Cannot connect to the Docker daemon' >&2; exit 1");
            let error = docker.version().await.expect_err("daemon is down");
            assert!(
                matches!(error, Error::Upstream { .. }),
                "upstream error, got {error:?}"
            );
        }

        // TC: v1 ADP-01
        #[tokio::test]
        async fn up_passes_the_given_timeout_when_called() {
            let (_dir, docker) = stub("");
            let output = docker
                .compose_up(&project(), Duration::from_secs(77))
                .await
                .expect("runs");
            let lines = stdout(&output);
            assert!(lines.contains(&"arg:--wait-timeout"));
            assert!(lines.contains(&"arg:77"));
        }

        // TC: v1 ADP-01
        #[tokio::test]
        async fn login_sends_token_on_stdin_and_not_in_argv() {
            let (_dir, docker) = stub("cat");
            let registry = Host::new("git.example.com").expect("valid host");
            let user = Login::new("bob").expect("valid login");
            let output = docker
                .login(&registry, &user, "s3cret")
                .await
                .expect("runs");
            let lines = stdout(&output);
            assert!(lines.contains(&"s3cret"), "stdin: {lines:?}");
            assert!(lines.contains(&"arg:--password-stdin"));
            assert!(!lines.contains(&"arg:s3cret"));
            assert!(lines.contains(&"config:/cfg"));
        }

        // TC: v1 ADP-02
        #[tokio::test]
        async fn ps_ignores_stderr_warnings_when_output_is_json() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("docker");
            std::fs::write(
                &path,
                "#!/bin/sh\necho 'WARN[0000] something' >&2\n\
                 echo '{\"Service\":\"web\",\"State\":\"running\",\"Health\":\"healthy\"}'\n",
            )
            .expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            let docker = ProcessDocker::new(dir.path(), "/cfg").with_program(path);
            let services = docker.compose_ps(&project()).await.expect("parses");
            assert_eq!(
                services,
                vec![ServiceStatus {
                    service: "web".to_owned(),
                    state: "running".to_owned(),
                    health: Some("healthy".to_owned()),
                }]
            );
        }

        // TC: v1 ADP-02
        #[tokio::test]
        async fn ps_is_upstream_error_when_docker_fails() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("docker");
            std::fs::write(&path, "#!/bin/sh\necho 'daemon down' >&2\nexit 1\n").expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            let docker = ProcessDocker::new(dir.path(), "/cfg").with_program(path);
            let err = docker.compose_ps(&project()).await.expect_err("fails");
            assert!(
                matches!(
                    &err,
                    Error::Upstream {
                        system: System::Docker,
                        ..
                    }
                ),
                "got {err:?}"
            );
            assert!(err.to_string().contains("daemon down"), "got {err}");
        }
    }

    mod ps_parsing {
        use super::*;

        const NDJSON: &str = r#"{"ID":"a1","Name":"lf_acme_qa-web-1","Service":"web","State":"running","Health":"healthy","ExitCode":0}
{"ID":"b2","Name":"lf_acme_qa-worker-1","Service":"worker","State":"exited","Health":"","ExitCode":1}
"#;

        const ARRAY: &str = r#"[{"ID":"a1","Name":"lf_acme_qa-web-1","Service":"web","State":"running","Health":"healthy"},{"ID":"b2","Name":"lf_acme_qa-worker-1","Service":"worker","State":"exited","Health":""}]"#;

        fn expected() -> Vec<ServiceStatus> {
            vec![
                ServiceStatus {
                    service: "web".to_owned(),
                    state: "running".to_owned(),
                    health: Some("healthy".to_owned()),
                },
                ServiceStatus {
                    service: "worker".to_owned(),
                    state: "exited".to_owned(),
                    health: None,
                },
            ]
        }

        // TC: v1 ADP-02
        #[test]
        fn services_when_output_is_ndjson() {
            assert_eq!(parse_ps(NDJSON).expect("parses"), expected());
        }

        // TC: v1 ADP-02
        #[test]
        fn services_when_output_is_a_json_array() {
            assert_eq!(parse_ps(ARRAY).expect("parses"), expected());
        }

        // TC: v1 ADP-02
        #[test]
        fn empty_when_nothing_runs() {
            assert_eq!(parse_ps("").expect("parses"), Vec::new());
            assert_eq!(parse_ps("[]\n").expect("parses"), Vec::new());
        }

        // TC: v1 ADP-02
        #[test]
        fn health_is_none_when_field_is_absent() {
            let parsed = parse_ps(r#"{"Service":"db","State":"running"}"#).expect("parses");
            assert_eq!(parsed.first().and_then(|s| s.health.clone()), None);
        }

        // TC: v1 ADP-02
        #[test]
        fn upstream_error_when_output_is_not_json() {
            let err = parse_ps("not json").expect_err("garbage");
            assert!(
                matches!(
                    err,
                    Error::Upstream {
                        system: System::Docker,
                        ..
                    }
                ),
                "got {err:?}"
            );
        }
    }
}
