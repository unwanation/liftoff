//! TC: LCL-02. Without a server `lof secret *` works on the local store: `secrets.db` in the config home, every value sealed
//! with the core's `crypto` under the key in `key`, which is created
//! `0600`; `lof doctor` warns when the key is readable by others.
//!
//! A local project is named by `--project` (else by the name of the
//! `origin` repository); the scopes are the server's: `environment` (the
//! default, `prod` unless `--env`), `project`.

use std::os::unix::fs::PermissionsExt as _;

use super::{any_file_holds, keys};
use crate::support::Cli;

/// A value no other file of the test holds.
const VALUE: &str = "s3cr3t-local-value";

/// A CLI without a server that set `API_KEY` of project `demo` locally.
async fn with_secret() -> Cli {
    let cli = Cli::new();
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

/// `(check, status)` of each line of `doctor`: its first two words.
fn statuses(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((words.next()?.to_owned(), words.next()?.to_owned()))
        })
        .collect()
}

/// The status of the `secrets` check of `doctor`.
async fn secrets_check(cli: &Cli) -> Option<String> {
    let outcome = cli.run(&["doctor"]).await;
    statuses(&outcome.out)
        .into_iter()
        .find(|(check, _)| check == "secrets")
        .map(|(_, status)| status)
}

mod set_and_get {
    use super::*;

    // TC: LCL-02
    #[tokio::test]
    async fn prints_value_when_secret_was_set_without_server() {
        let cli = with_secret().await;
        let outcome = cli
            .run(&["--project", "demo", "secret", "get", "API_KEY"])
            .await;
        assert_eq!(
            (outcome.code, outcome.out),
            (0, format!("{VALUE}\n")),
            "{:?}",
            outcome.err
        );
    }

    // TC: LCL-02 (`ls` shows keys, scopes and `ci`, no values)
    #[tokio::test]
    async fn lists_key_without_value_when_secret_was_set_without_server() {
        let cli = with_secret().await;
        let outcome = cli
            .run(&["--json", "--project", "demo", "secret", "ls"])
            .await;
        assert_eq!(
            (
                outcome.code,
                keys(&outcome.out),
                outcome.out.contains(VALUE)
            ),
            (0, vec!["API_KEY".to_owned()], false),
            "{outcome:?}"
        );
    }

    // TC: LCL-02 (scopes stay apart locally too)
    #[tokio::test]
    async fn misses_environment_secret_when_project_scope_is_read() {
        let cli = with_secret().await;
        let outcome = cli
            .run(&[
                "--json",
                "--project",
                "demo",
                "secret",
                "ls",
                "--scope",
                "project",
            ])
            .await;
        assert_eq!(
            (outcome.code, keys(&outcome.out)),
            (0, Vec::<String>::new()),
            "{outcome:?}"
        );
    }
}

mod at_rest {
    use super::*;

    // TC: LCL-02 (the store is `secrets.db` of the config home)
    #[tokio::test]
    async fn keeps_secrets_in_config_home_when_set_without_server() {
        let cli = with_secret().await;
        assert!(cli.config_dir().join("secrets.db").is_file());
    }

    // TC: LCL-02 (values are sealed: no file of the config home holds one)
    #[tokio::test]
    async fn keeps_value_out_of_plaintext_when_set_without_server() {
        let cli = with_secret().await;
        assert!(
            !any_file_holds(&cli.config_dir(), VALUE),
            "the value is in a file of the config home in plaintext"
        );
    }

    // TC: LCL-02 (the local key `~/.config/liftoff/key` is `0600`)
    #[tokio::test]
    async fn creates_key_readable_by_owner_only_when_first_secret_is_set() {
        let cli = with_secret().await;
        let mode = std::fs::metadata(cli.config_dir().join("key"))
            .map(|meta| meta.permissions().mode() & 0o777)
            .ok();
        assert_eq!(mode, Some(0o600));
    }
}

mod doctor {
    use super::*;

    // TC: LCL-02 (a key readable by others is a warning of `doctor`)
    #[tokio::test]
    async fn warns_when_key_is_readable_by_others() {
        let cli = with_secret().await;
        std::fs::set_permissions(
            cli.config_dir().join("key"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(secrets_check(&cli).await.as_deref(), Some("warn"));
    }

    // TC: LCL-02 (the warning names the file and the fix)
    #[tokio::test]
    async fn names_key_and_chmod_when_key_is_readable_by_others() {
        let cli = with_secret().await;
        let key = cli.config_dir().join("key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outcome = cli.run(&["doctor"]).await;
        let line = outcome
            .out
            .lines()
            .find(|line| line.starts_with("secrets"))
            .unwrap_or_default()
            .to_owned();
        assert!(
            line.contains(&key.display().to_string()) && line.contains("chmod 600"),
            "{line:?}"
        );
    }

    // TC: LCL-02
    #[tokio::test]
    async fn reports_ok_when_key_is_owner_only() {
        let cli = with_secret().await;
        assert_eq!(secrets_check(&cli).await.as_deref(), Some("ok"));
    }
}

/// `secret set API_KEY=<value>` of project `demo` on `cli`.
async fn set_value(cli: &Cli, value: &str) {
    let outcome = cli
        .run(&[
            "--project",
            "demo",
            "secret",
            "set",
            &format!("API_KEY={value}"),
        ])
        .await;
    assert_eq!(outcome.code, 0, "secret set: {outcome:?}");
}

/// The versions `secret history API_KEY --json` lists, in its order.
async fn history(cli: &Cli) -> Vec<u64> {
    let outcome = cli
        .run(&[
            "--json",
            "--project",
            "demo",
            "secret",
            "history",
            "API_KEY",
        ])
        .await;
    assert_eq!(outcome.code, 0, "secret history: {outcome:?}");
    serde_json::from_str::<Vec<serde_json::Value>>(&outcome.out)
        .unwrap_or_default()
        .iter()
        .filter_map(|revision| revision.get("version").and_then(serde_json::Value::as_u64))
        .collect()
}

/// TC: LCL-02: `set` writes a new version locally too,
/// `history` lists the kept ones and `rollback --to N` makes version N
/// the new current one, without a server.
mod versions {
    use super::*;

    // TC: LCL-02 (`history K` shows the versions, newest first)
    #[tokio::test]
    async fn lists_versions_newest_first_when_secret_was_set_twice() {
        let cli = with_secret().await;
        set_value(&cli, "second").await;
        assert_eq!(history(&cli).await, vec![2, 1]);
    }

    // TC: LCL-02 (`history` carries no values)
    #[tokio::test]
    async fn shows_no_value_when_history_is_listed() {
        let cli = with_secret().await;
        let outcome = cli
            .run(&[
                "--json",
                "--project",
                "demo",
                "secret",
                "history",
                "API_KEY",
            ])
            .await;
        assert_eq!(
            (outcome.code, outcome.out.contains(VALUE)),
            (0, false),
            "{outcome:?}"
        );
    }

    // TC: LCL-02 (`rollback K --to N` makes version N current)
    #[tokio::test]
    async fn prints_old_value_when_secret_was_rolled_back() {
        let cli = with_secret().await;
        set_value(&cli, "second").await;
        let rolled = cli
            .run(&[
                "--project",
                "demo",
                "secret",
                "rollback",
                "API_KEY",
                "--to",
                "1",
            ])
            .await;
        assert_eq!(rolled.code, 0, "{rolled:?}");
        let outcome = cli
            .run(&["--project", "demo", "secret", "get", "API_KEY"])
            .await;
        assert_eq!(outcome.out, format!("{VALUE}\n"), "{outcome:?}");
    }

    // TC: LCL-02 (a rollback is a new version, as on a server: SEC-05)
    #[tokio::test]
    async fn adds_version_when_secret_was_rolled_back() {
        let cli = with_secret().await;
        set_value(&cli, "second").await;
        let _rolled = cli
            .run(&[
                "--project",
                "demo",
                "secret",
                "rollback",
                "API_KEY",
                "--to",
                "1",
            ])
            .await;
        assert_eq!(history(&cli).await, vec![3, 2, 1]);
    }

    // TC: LCL-02 (a version that is not kept)
    #[tokio::test]
    async fn exits_1_when_rollback_names_unknown_version() {
        let cli = with_secret().await;
        let outcome = cli
            .run(&[
                "--project",
                "demo",
                "secret",
                "rollback",
                "API_KEY",
                "--to",
                "7",
            ])
            .await;
        assert_eq!(outcome.code, 1, "{outcome:?}");
    }

    // TC: LCL-02 (SEC-03: the newest 20 versions are kept)
    #[tokio::test]
    async fn keeps_newest_twenty_versions_when_set_more_often() {
        let cli = with_secret().await;
        for round in 2..=21 {
            set_value(&cli, &format!("value-{round}")).await;
        }
        assert_eq!(history(&cli).await, (2..=21).rev().collect::<Vec<u64>>());
    }

    // TC: LCL-02 (`rm` takes the history with it)
    #[tokio::test]
    async fn forgets_versions_when_secret_was_removed() {
        let cli = with_secret().await;
        let _removed = cli
            .run(&["--project", "demo", "secret", "rm", "API_KEY"])
            .await;
        set_value(&cli, "again").await;
        assert_eq!(history(&cli).await, vec![1]);
    }
}

/// TC: LCL-02. The core's rule of `get` holds locally: the keys for CI
/// only are write-only (`SecretsService::get`).
mod ci_only {
    use super::*;

    // TC: LCL-02
    #[tokio::test]
    async fn refuses_get_when_key_is_for_ci_only() {
        let cli = Cli::new();
        let set = cli
            .run(&[
                "--project",
                "demo",
                "secret",
                "set",
                "--scope",
                "project",
                "--ci",
                &format!("REGISTRY_TOKEN={VALUE}"),
            ])
            .await;
        assert_eq!(set.code, 0, "{set:?}");
        let outcome = cli
            .run(&[
                "--project",
                "demo",
                "secret",
                "get",
                "--scope",
                "project",
                "REGISTRY_TOKEN",
            ])
            .await;
        assert_eq!(
            (outcome.code, outcome.out.contains(VALUE)),
            (1, false),
            "{outcome:?}"
        );
    }
}

/// TC: LCL-02. `secret push` and `secret sync` need a server: push copies
/// to one, and the CI secrets a sync sends (`LIFTOFF_TOKEN` first) are
/// the server's.
mod needs_server {
    use super::*;

    // TC: LCL-02
    #[tokio::test]
    async fn refuses_sync_naming_server_connect_when_no_server() {
        let cli = with_secret().await;
        let outcome = cli.run(&["--project", "demo", "secret", "sync"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.out.as_str(),
                outcome.err.contains("lof server connect")
            ),
            (1, "", true),
            "{outcome:?}"
        );
    }

    // TC: LCL-02
    #[tokio::test]
    async fn refuses_push_naming_server_connect_when_no_server() {
        let cli = with_secret().await;
        let outcome = cli.run(&["--project", "demo", "secret", "push"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.out.as_str(),
                outcome.err.contains("lof server connect")
            ),
            (1, "", true),
            "{outcome:?}"
        );
    }
}
