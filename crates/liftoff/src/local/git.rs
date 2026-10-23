//! The `git` children of local mode: clone, commit and push in the working tree, and the default
//! branch of a clone.
//!
//! With setup's `Runner` and upgrade's `HostCommand`, the only host
//! processes outside `crates/host` (`docs/ARCHITECTURE.md`,
//! «Исключения»). Every child gets an explicit argument vector, never a
//! shell; `-c protocol.ext.allow=never` (no `ext::` remote runs a
//! command), `GIT_TERMINAL_PROMPT=0` (no question on the terminal), `--`
//! before every path and URL operand, stdin closed, its stdout captured
//! (the CLI owns stdout) and its stderr passed through. The forge
//! connection's token reaches it through the environment only
//! (`GIT_CONFIG_*`: an `Authorization` header for `https://<host>/`
//! alone), never through its arguments.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use liftoff_core::domain::{Host, RepoRef};
use liftoff_core::ports::GitAccess;

use crate::error::CliError;

/// How the `git` children run: the environment they get on top of the
/// CLI's own ([`Local::with_git_env`](super::Local::with_git_env)).
#[derive(Debug, Clone, Default)]
pub(crate) struct GitRunner {
    env: Vec<(String, String)>,
}

/// The credential of a forge connection, for the children that reach it.
#[derive(Clone)]
pub(crate) struct ForgeAuth {
    host: Host,
    header: String,
}

impl core::fmt::Debug for ForgeAuth {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ForgeAuth")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl ForgeAuth {
    /// `Authorization: Basic <login:token>` for `https://<host>/`, as the
    /// forges take a token over HTTPS.
    pub(crate) fn of(access: &GitAccess) -> Self {
        let pair = format!("{}:{}", access.login, access.token.expose());
        Self {
            host: access.host.clone(),
            header: format!("Authorization: Basic {}", STANDARD.encode(pair)),
        }
    }
}

/// `https://<host>/<owner>/<name>.git`.
pub(crate) fn clone_url(host: &Host, repo: &RepoRef) -> String {
    format!("https://{host}/{repo}.git")
}

impl GitRunner {
    /// Children with `env` on top of the CLI's environment.
    pub(crate) const fn new(env: Vec<(String, String)>) -> Self {
        Self { env }
    }

    /// The child `git <args>` in `dir` (see the [module
    /// documentation](self)): never a shell, `-c protocol.ext.allow=never`
    /// first, no prompt, stdin closed, stdout captured, stderr passed
    /// through, and `auth`'s header in its environment only.
    fn command(&self, dir: &Path, args: &[OsString], auth: Option<&ForgeAuth>) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-c")
            .arg("protocol.ext.allow=never")
            .args(args)
            .current_dir(dir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(auth) = auth {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env(
                    "GIT_CONFIG_KEY_0",
                    format!("http.https://{}/.extraHeader", auth.host),
                )
                .env("GIT_CONFIG_VALUE_0", &auth.header);
        }
        command
    }

    /// Runs `git <args>` in `dir`; its stdout.
    fn run(
        &self,
        dir: &Path,
        args: &[OsString],
        auth: Option<&ForgeAuth>,
    ) -> Result<String, CliError> {
        let mut command = self.command(dir, args, auth);
        let what = args
            .first()
            .map_or_else(String::new, |word| word.to_string_lossy().into_owned());
        let output = command
            .output()
            .map_err(|error| CliError::Local(format!("cannot run git: {error}")))?;
        if !output.status.success() {
            return Err(CliError::Local(format!(
                "git {what} failed ({}); see its message above",
                output.status
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// `git clone <url> <dest>` in `dir`.
    pub(crate) fn clone_into(
        &self,
        dir: &Path,
        url: &str,
        dest: &Path,
        auth: Option<&ForgeAuth>,
    ) -> Result<(), CliError> {
        self.run(
            dir,
            &[
                "clone".into(),
                "--quiet".into(),
                "--".into(),
                url.into(),
                dest.as_os_str().to_owned(),
            ],
            auth,
        )
        .map(drop)
    }

    /// Stages everything in the work tree `dir` and commits it with
    /// `message`; whether there was anything to commit.
    pub(crate) fn commit_all(&self, dir: &Path, message: &str) -> Result<bool, CliError> {
        let _staged = self.run(
            dir,
            &["add".into(), "--all".into(), "--".into(), ".".into()],
            None,
        )?;
        let status = self.run(
            dir,
            &[
                "status".into(),
                "--porcelain".into(),
                "--untracked-files=no".into(),
            ],
            None,
        )?;
        if status.trim().is_empty() {
            return Ok(false);
        }
        self.run(
            dir,
            &[
                "commit".into(),
                "--quiet".into(),
                "--message".into(),
                message.into(),
            ],
            None,
        )
        .map(|_| true)
    }

    /// Whether a tracked file of the work tree `dir` has changes not
    /// committed, staged or not (untracked files do not count).
    pub(crate) fn has_uncommitted_changes(&self, dir: &Path) -> Result<bool, CliError> {
        let status = self.run(
            dir,
            &[
                "status".into(),
                "--porcelain".into(),
                "--untracked-files=no".into(),
            ],
            None,
        )?;
        Ok(!status.trim().is_empty())
    }

    /// Stages `paths` of the work tree `dir` (relative to it) and commits
    /// them alone with `message`: nothing else of the work tree, staged or
    /// not, goes into the commit. Whether there was anything to commit.
    pub(crate) fn commit_paths(
        &self,
        dir: &Path,
        paths: &[String],
        message: &str,
    ) -> Result<bool, CliError> {
        if paths.is_empty() {
            return Ok(false);
        }
        let operands = || paths.iter().map(OsString::from);
        let mut add: Vec<OsString> = vec!["add".into(), "--".into()];
        add.extend(operands());
        let _staged = self.run(dir, &add, None)?;
        let mut staged: Vec<OsString> = vec![
            "diff".into(),
            "--cached".into(),
            "--name-only".into(),
            "--".into(),
        ];
        staged.extend(operands());
        if self.run(dir, &staged, None)?.trim().is_empty() {
            return Ok(false);
        }
        let mut commit: Vec<OsString> = vec![
            "commit".into(),
            "--quiet".into(),
            "--only".into(),
            "--message".into(),
            message.into(),
            "--".into(),
        ];
        commit.extend(operands());
        self.run(dir, &commit, None).map(|_| true)
    }

    /// Pushes the current commit of `dir` to `branch` of `origin`.
    pub(crate) fn push_to(
        &self,
        dir: &Path,
        branch: &str,
        auth: Option<&ForgeAuth>,
    ) -> Result<(), CliError> {
        self.run(
            dir,
            &[
                "push".into(),
                "--quiet".into(),
                "origin".into(),
                format!("HEAD:refs/heads/{branch}").into(),
            ],
            auth,
        )
        .map(drop)
    }

    /// Pushes the current branch of `dir` to `origin`.
    pub(crate) fn push(&self, dir: &Path, auth: Option<&ForgeAuth>) -> Result<(), CliError> {
        self.run(
            dir,
            &[
                "push".into(),
                "--quiet".into(),
                "origin".into(),
                "HEAD".into(),
            ],
            auth,
        )
        .map(drop)
    }

    /// The default branch of `origin` of the work tree `dir`, as the forge
    /// names it (`git ls-remote --symref origin HEAD`); the branch `dir`
    /// is on when `origin` does not say (`prod` is pinned to the default branch).
    pub(crate) fn default_branch(
        &self,
        dir: &Path,
        auth: Option<&ForgeAuth>,
    ) -> Result<String, CliError> {
        let listed = self.run(
            dir,
            &[
                "ls-remote".into(),
                "--symref".into(),
                "origin".into(),
                "HEAD".into(),
            ],
            auth,
        );
        listed
            .ok()
            .as_deref()
            .and_then(symref_branch)
            .map_or_else(|| self.current_branch(dir), |branch| Ok(branch.to_owned()))
    }

    /// The branch `dir` is on: the default branch of a fresh clone.
    pub(crate) fn current_branch(&self, dir: &Path) -> Result<String, CliError> {
        let branch = self.run(
            dir,
            &["symbolic-ref".into(), "--short".into(), "HEAD".into()],
            None,
        )?;
        let branch = branch.trim();
        if branch.is_empty() {
            return Err(CliError::Local(
                "the working tree is on no branch".to_owned(),
            ));
        }
        Ok(branch.to_owned())
    }
}

/// The branch of `ref: refs/heads/<branch>\tHEAD` in what `git ls-remote
/// --symref` printed.
fn symref_branch(listed: &str) -> Option<&str> {
    listed.lines().find_map(|line| {
        let (target, name) = line.strip_prefix("ref: ")?.split_once('\t')?;
        let branch = target.strip_prefix("refs/heads/")?;
        (name == "HEAD" && !branch.is_empty()).then_some(branch)
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use liftoff_core::domain::{GitProvider, Login, SecretValue};

    use super::*;

    const TOKEN: &str = "ghp_secret_token";

    fn auth() -> ForgeAuth {
        ForgeAuth::of(&GitAccess {
            provider: GitProvider::Github,
            host: Host::new("github.com").unwrap(),
            login: Login::new("owner").unwrap(),
            token: SecretValue::new(TOKEN.to_owned()),
        })
    }

    fn clone_command() -> Command {
        GitRunner::default().command(
            Path::new("/work"),
            &[
                "clone".into(),
                "--".into(),
                "https://github.com/owner/demo.git".into(),
                "/work/demo".into(),
            ],
            Some(&auth()),
        )
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env(command: &Command, key: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new(key))
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    }

    mod command {
        use super::*;

        // TC: LCL-03 (no `ext::` remote runs a command)
        #[test]
        fn starts_with_ext_protocol_off_when_built() {
            assert_eq!(
                args(&clone_command()).get(..2),
                Some(&["-c".to_owned(), "protocol.ext.allow=never".to_owned()][..])
            );
        }

        // TC: LCL-03 (the operands are never options)
        #[test]
        fn puts_double_dash_before_operands_when_cloning() {
            let args = args(&clone_command());
            let dash = args.iter().position(|arg| arg == "--");
            let url = args.iter().position(|arg| arg.starts_with("https://"));
            assert!(dash.is_some() && dash < url, "{args:?}");
        }

        // TC: LCL-03 (a token never reaches an argument vector)
        #[test]
        fn keeps_token_out_of_arguments_when_authenticated() {
            let args = args(&clone_command());
            let encoded = STANDARD.encode(format!("owner:{TOKEN}"));
            assert!(
                !args
                    .iter()
                    .any(|arg| arg.contains(TOKEN) || arg.contains(&encoded)),
                "{args:?}"
            );
        }

        // TC: LCL-03 (no question on the terminal)
        #[test]
        fn turns_terminal_prompt_off_when_built() {
            assert_eq!(
                env(&clone_command(), "GIT_TERMINAL_PROMPT").as_deref(),
                Some("0")
            );
        }

        // TC: LCL-03 (the header goes to the connection's host only)
        #[test]
        fn scopes_header_to_forge_host_when_authenticated() {
            let command = clone_command();
            let encoded = STANDARD.encode(format!("owner:{TOKEN}"));
            assert_eq!(
                (
                    env(&command, "GIT_CONFIG_COUNT"),
                    env(&command, "GIT_CONFIG_KEY_0"),
                    env(&command, "GIT_CONFIG_VALUE_0"),
                ),
                (
                    Some("1".to_owned()),
                    Some("http.https://github.com/.extraHeader".to_owned()),
                    Some(format!("Authorization: Basic {encoded}")),
                )
            );
        }

        // TC: LCL-03 (a child without a connection gets no header)
        #[test]
        fn sets_no_header_when_unauthenticated() {
            let command =
                GitRunner::default().command(Path::new("/work"), &["status".into()], None);
            assert_eq!(env(&command, "GIT_CONFIG_COUNT"), None);
        }
    }

    mod symref_branch {
        use super::*;

        // TC: LCL-04 (the forge's default branch)
        #[test]
        fn names_branch_of_head_when_remote_lists_it() {
            assert_eq!(
                symref_branch("ref: refs/heads/trunk\tHEAD\n0123abcd\tHEAD\n"),
                Some("trunk")
            );
        }

        // TC: LCL-04
        #[test]
        fn none_when_remote_lists_no_symref() {
            assert_eq!(symref_branch("0123abcd\tHEAD\n"), None);
        }
    }
}
