//! Local mode: the CLI with a temp config home and no server configured.
//!
//! The `git.*` modules are the built components of `modules/` (`just
//! modules`), which the CLI downloads from a fake registry ([`registry`])
//! signed with the core's `FakeModuleSource` key and runs in its own
//! module host; their `wasi:http` reaches a fake GitHub on loopback
//! ([`forge`]) through `Local::routing_https_to`. The `git` the CLI runs is
//! the real one, with a `GIT_CONFIG_GLOBAL` of the test's own that
//! rewrites `https://github.com/` to bare repositories under a tempdir
//! ([`repo`]). Nothing reads or writes the real home directory.
//!
//! One module per catalogue case: `connect` (LCL-01), `secrets` (LCL-02),
//! `create` (LCL-03), `server_connect` (LCL-04), `push` (LCL-05),
//! `dotfile` (LCL-08), `commands` (the other local commands, LCL-01 and
//! LCL-02), `bootstrap` (`lof create` and `lof templates` of
//! without a server, CRE-11…16). LCL-06 and LCL-07 run against the CLI's module
//! host itself in `tests/module_host.rs`.

mod bootstrap;
mod commands;
mod connect;
mod create;
mod dotfile;
mod forge;
mod push;
mod registry;
pub(crate) mod repo;
mod secrets;
mod server_connect;

use std::path::Path;

use liftoff::local::{Local, RegistryKey};

use crate::support::{Cli, Reply};
use forge::FakeGithub;
use registry::FakeRegistry;
use repo::GitHome;

/// The token of the account `owner` on the fake GitHub.
pub(crate) const TOKEN: &str = "ghp_owner";

/// Local mode over `registry`, `forge` and `git`.
pub(crate) fn local(registry: &FakeRegistry, forge: &FakeGithub, git: &GitHome) -> Local {
    git.env().into_iter().fold(
        Local::new(registry.url.clone(), RegistryKey::new(&registry.key))
            .routing_https_to(forge::API, forge.addr)
            .routing_https_to(forge::WEB, forge.addr),
        |local, (key, value)| local.with_git_env(key, value),
    )
}

/// Everything local mode of a test talks to.
pub(crate) struct Machine {
    pub(crate) cli: Cli,
    pub(crate) forge: FakeGithub,
    pub(crate) git: GitHome,
    _registry: FakeRegistry,
}

/// A machine without a server whose registry serves `git.github`.
pub(crate) async fn machine() -> Machine {
    let registry = FakeRegistry::serving(&[("git.github", "git_github")]).await;
    let forge = FakeGithub::start().await;
    let git = GitHome::new();
    let cli = Cli::new().local(local(&registry, &forge, &git));
    Machine {
        cli,
        forge,
        git,
        _registry: registry,
    }
}

impl Machine {
    /// Stdin is a terminal whose user answers `replies` in order.
    pub(crate) fn terminal(mut self, replies: Vec<Reply>) -> Self {
        self.cli = self.cli.terminal(replies);
        self
    }
}

/// [`machine`], connected to GitHub as `owner` with [`TOKEN`].
pub(crate) async fn connected() -> Machine {
    let machine = machine().await;
    let outcome = machine
        .cli
        .run(&["git", "connect", "github", "--token", TOKEN])
        .await;
    assert_eq!(outcome.code, 0, "git connect: {outcome:?}");
    machine
}

/// Whether some file below `dir` holds `needle`.
pub(crate) fn any_file_holds(dir: &Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            any_file_holds(&path, needle)
        } else {
            std::fs::read(&path).is_ok_and(|bytes| {
                bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            })
        }
    })
}

/// The `key` of every object of a JSON array.
pub(crate) fn keys(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .iter()
        .filter_map(|row| row.get("key").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect()
}
