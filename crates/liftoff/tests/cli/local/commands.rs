//! TC: LCL-01, LCL-02. The other commands of local mode, each over the
//! core service it calls: `git disconnect`,
//! `module ls|remove|enable|config`, `secret rm|pull`,
//! `secret set --ci` outside the project scope, and the config home's
//! mode in the `secrets` check of `doctor`.

use std::os::unix::fs::PermissionsExt as _;

use super::{TOKEN, connected, keys};
use crate::support::Cli;

/// Runs `args` and asserts it exits 0; its stdout.
async fn ok(cli: &Cli, args: &[&str]) -> String {
    let outcome = cli.run(args).await;
    assert_eq!(outcome.code, 0, "{args:?}: {outcome:?}");
    outcome.out
}

/// The `name` of every module `--json module ls` lists.
async fn module_names(cli: &Cli) -> Vec<String> {
    let listed = ok(cli, &["--json", "module", "ls"]).await;
    serde_json::from_str::<Vec<serde_json::Value>>(&listed)
        .unwrap_or_default()
        .iter()
        .filter_map(|row| row.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect()
}

mod git_disconnect {
    use super::*;

    // TC: LCL-01 (the connection is gone from the local store)
    #[tokio::test]
    async fn lists_no_connection_when_disconnected_without_server() {
        let machine = connected().await;
        let _out = ok(&machine.cli, &["git", "disconnect", "github.com"]).await;
        let listed = ok(&machine.cli, &["--json", "git", "ls"]).await;
        assert_eq!(listed.trim(), "[]");
    }

    // TC: LCL-01 (`git connect` again works after it)
    #[tokio::test]
    async fn connects_again_when_disconnected_without_server() {
        let machine = connected().await;
        let _out = ok(&machine.cli, &["git", "disconnect", "github.com"]).await;
        let outcome = machine
            .cli
            .run(&["git", "connect", "github", "--token", TOKEN])
            .await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
    }
}

mod module_commands {
    use super::*;

    // TC: LCL-01 (`git connect` installed the forge's module)
    #[tokio::test]
    async fn lists_installed_module_when_listed_without_server() {
        let machine = connected().await;
        assert_eq!(module_names(&machine.cli).await, vec!["git.github"]);
    }

    // TC: LCL-01 (the registry's modules)
    #[tokio::test]
    async fn lists_registry_module_when_listed_available_without_server() {
        let machine = connected().await;
        let listed = ok(&machine.cli, &["--json", "module", "ls", "--available"]).await;
        assert!(listed.contains("\"git.github\""), "{listed}");
    }

    // TC: LCL-01
    #[tokio::test]
    async fn lists_nothing_when_module_removed_without_server() {
        let machine = connected().await;
        let _out = ok(&machine.cli, &["module", "remove", "git.github", "--purge"]).await;
        assert_eq!(module_names(&machine.cli).await, Vec::<String>::new());
    }

    // TC: LCL-01
    #[tokio::test]
    async fn prints_enabled_when_module_enabled_without_server() {
        let machine = connected().await;
        let out = ok(&machine.cli, &["module", "enable", "git.github"]).await;
        assert_eq!(out, "enabled git.github\n");
    }

    // TC: LCL-01 (a field of the module's schema)
    #[tokio::test]
    async fn prints_configured_when_module_configured_without_server() {
        let machine = connected().await;
        let out = ok(
            &machine.cli,
            &[
                "module",
                "config",
                "git.github",
                "--set",
                "client_id=Iv1.local",
            ],
        )
        .await;
        assert_eq!(out, "configured git.github\n");
    }
}

/// A CLI without a server that set `pairs` of project `demo` in `scope`.
async fn with(cli: &Cli, scope: &str, pairs: &[&str]) {
    let mut args = vec!["--project", "demo", "secret", "set", "--scope", scope];
    args.extend_from_slice(pairs);
    let _out = ok(cli, &args).await;
}

mod secret_rm {
    use super::*;

    // TC: LCL-02
    #[tokio::test]
    async fn lists_no_key_when_removed_without_server() {
        let cli = Cli::new();
        with(&cli, "environment", &["API_KEY=v"]).await;
        let _out = ok(&cli, &["--project", "demo", "secret", "rm", "API_KEY"]).await;
        let listed = ok(&cli, &["--json", "--project", "demo", "secret", "ls"]).await;
        assert_eq!(keys(&listed), Vec::<String>::new());
    }

    // TC: LCL-02
    #[tokio::test]
    async fn fails_when_removed_key_is_missing_without_server() {
        let cli = Cli::new();
        with(&cli, "environment", &["API_KEY=v"]).await;
        let outcome = cli
            .run(&["--project", "demo", "secret", "rm", "OTHER"])
            .await;
        assert_eq!(outcome.code, 1, "{outcome:?}");
    }
}

mod secret_pull {
    use super::*;

    // TC: LCL-02 (tenant, project, environment; the lower wins)
    #[tokio::test]
    async fn prints_merged_env_when_scopes_share_a_key_without_server() {
        let cli = Cli::new();
        with(&cli, "tenant", &["REGION=eu", "LOG_LEVEL=warn"]).await;
        with(&cli, "project", &["LOG_LEVEL=info", "API_KEY=project"]).await;
        with(&cli, "environment", &["API_KEY=prod"]).await;
        let out = ok(&cli, &["--project", "demo", "secret", "pull"]).await;
        assert_eq!(out, "API_KEY=prod\nLOG_LEVEL=info\nREGION=eu\n");
    }

    // TC: LCL-02 (keys for CI only never reach an env)
    #[tokio::test]
    async fn leaves_out_ci_only_keys_when_pulled_without_server() {
        let cli = Cli::new();
        let _out = ok(
            &cli,
            &[
                "--project",
                "demo",
                "secret",
                "set",
                "--scope",
                "project",
                "--ci",
                "LIFTOFF_TOKEN=lft_local",
            ],
        )
        .await;
        with(&cli, "project", &["LOG_LEVEL=info"]).await;
        let out = ok(&cli, &["--project", "demo", "secret", "pull"]).await;
        assert_eq!(out, "LOG_LEVEL=info\n");
    }
}

mod secret_set_ci {
    use super::*;

    // TC: LCL-02 (SYNC-04 locally: only project secrets go to CI)
    #[tokio::test]
    async fn refuses_ci_when_scope_is_environment_without_server() {
        let cli = Cli::new();
        let outcome = cli
            .run(&["--project", "demo", "secret", "set", "--ci", "API_KEY=v"])
            .await;
        let listed = ok(&cli, &["--json", "--project", "demo", "secret", "ls"]).await;
        assert_eq!(
            (outcome.code == 0, keys(&listed)),
            (false, Vec::<String>::new()),
            "{outcome:?}"
        );
    }
}

mod doctor_home {
    use super::*;

    /// The `secrets` line of `doctor`.
    async fn secrets_line(cli: &Cli) -> Option<String> {
        let outcome = cli.run(&["doctor"]).await;
        outcome
            .out
            .lines()
            .find(|line| line.starts_with("secrets"))
            .map(str::to_owned)
    }

    // TC: LCL-02 (the config home open to others is a warning)
    #[tokio::test]
    async fn warns_with_chmod_700_when_config_home_is_open_to_others() {
        let cli = Cli::new();
        with(&cli, "environment", &["API_KEY=v"]).await;
        std::fs::set_permissions(cli.config_dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let line = secrets_line(&cli).await.unwrap_or_default();
        assert!(
            line.contains("warn") && line.contains("chmod 700"),
            "{line:?}"
        );
    }

    // TC: LCL-02 (no local store yet: no `secrets` check)
    #[tokio::test]
    async fn has_no_secrets_check_when_no_local_key_exists() {
        let cli = Cli::new();
        assert_eq!(secrets_line(&cli).await, None);
    }
}
