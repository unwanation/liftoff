//! TC: LCL-01. Without a server `lof git connect` checks the token through
//! the `git.github` module the CLI runs itself (one call to `/user`) and keeps the connection in the config home, its token sealed;
//! the module comes from the registry into `~/.config/liftoff/modules/`
//! (`lof module install` in the local context, or automatically
//! at `git connect`). With the module's `client_id` set, `git connect`
//! runs GitHub's device flow instead, as with a server.

use std::time::Duration;

use super::forge::USER_CODE;
use super::{TOKEN, any_file_holds, connected, machine};

mod git_connect {
    use super::*;

    // TC: LCL-01
    #[tokio::test]
    async fn prints_connection_when_token_is_accepted_without_server() {
        let machine = machine().await;
        let outcome = machine
            .cli
            .run(&["git", "connect", "github", "--token", TOKEN])
            .await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (0, "connected github.com as owner\n"),
            "{outcome:?}"
        );
    }

    // TC: LCL-01 (the token is checked with one call to `/user` of the forge, as the account it logs in as)
    #[tokio::test]
    async fn checks_token_with_forge_when_connecting_without_server() {
        let machine = connected().await;
        let asked: Vec<Option<String>> = machine
            .forge
            .seen_at("GET", "/user")
            .into_iter()
            .map(|seen| seen.authorization)
            .collect();
        assert_eq!(asked, vec![Some(format!("Bearer {TOKEN}"))]);
    }

    // TC: LCL-01
    #[tokio::test]
    async fn lists_connection_locally_when_connected_without_server() {
        let machine = connected().await;
        let outcome = machine.cli.run(&["--json", "git", "ls"]).await;
        let listed: serde_json::Value = serde_json::from_str(&outcome.out).unwrap_or_default();
        assert_eq!(
            (outcome.code, listed),
            (
                0,
                serde_json::json!([{
                    "provider": "github",
                    "host": "github.com",
                    "account_login": "owner",
                    "kind": "token",
                    "expires_at": null,
                }])
            ),
            "{outcome:?}"
        );
    }

    // TC: LCL-01 (a stored credential is sealed, never plaintext)
    #[tokio::test]
    async fn keeps_token_out_of_plaintext_when_connected_without_server() {
        let machine = connected().await;
        assert!(
            !any_file_holds(&machine.cli.config_dir(), TOKEN),
            "the token is in a file of the config home in plaintext"
        );
    }

    // TC: LCL-01 (`git connect` installs the `git.*` module it
    // needs into the config home)
    #[tokio::test]
    async fn downloads_module_into_config_home_when_connecting_without_server() {
        let machine = connected().await;
        let modules: Vec<String> = std::fs::read_dir(machine.cli.config_dir().join("modules"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            modules.iter().any(|name| name.starts_with("git.github")),
            "{modules:?}"
        );
    }

    // TC: LCL-01 (a token the forge rejects is not kept)
    #[tokio::test]
    async fn keeps_no_connection_when_forge_rejects_token() {
        let machine = machine().await;
        machine.forge.reject_tokens();
        let refused = machine
            .cli
            .run(&["git", "connect", "github", "--token", TOKEN])
            .await;
        let listed = machine.cli.run(&["--json", "git", "ls"]).await;
        assert_eq!(
            (refused.code, listed.code, listed.out.as_str()),
            (1, 0, "[]\n"),
            "{refused:?} {listed:?}"
        );
    }
}

mod device_flow {
    use super::*;

    /// A machine whose `git.github` has an OAuth application, then `git
    /// connect github` without a token.
    async fn connected_by_device() -> (super::super::Machine, crate::support::Outcome) {
        let machine = machine().await;
        let installed = machine
            .cli
            .run(&[
                "module",
                "install",
                "git.github",
                "--set",
                "client_id=Iv1.local",
            ])
            .await;
        assert_eq!(installed.code, 0, "module install: {installed:?}");
        let outcome = machine.cli.run(&["git", "connect", "github"]).await;
        (machine, outcome)
    }

    // TC: LCL-01, GIT-01 (with a client_id, the device flow without a server, as with one)
    #[tokio::test]
    async fn connects_after_pending_and_slow_down_when_module_has_client_id() {
        let (machine, outcome) = connected_by_device().await;
        assert_eq!(
            (
                outcome.code,
                outcome.out.as_str(),
                outcome.err.contains(USER_CODE),
                machine
                    .forge
                    .seen_at("POST", "/login/oauth/access_token")
                    .len()
            ),
            (0, "connected github.com as owner\n", true, 3),
            "{outcome:?}"
        );
    }

    // TC: LCL-01 (RFC 8628 3.5: `slow_down` raises the interval to the
    // one GitHub asks)
    #[tokio::test]
    async fn waits_the_raised_interval_after_slow_down_when_polling() {
        let (machine, outcome) = connected_by_device().await;
        assert_eq!(
            (outcome.code, machine.cli.slept()),
            (
                0,
                vec![
                    Duration::from_secs(5),
                    Duration::from_secs(5),
                    Duration::from_secs(12)
                ]
            ),
            "{outcome:?}"
        );
    }

    // TC: LCL-01 (the granted token is sealed in the config home, the connection is listed as OAuth)
    #[tokio::test]
    async fn keeps_the_granted_token_sealed_when_connected_by_device() {
        let (machine, _) = connected_by_device().await;
        let listed = machine.cli.run(&["--json", "git", "ls"]).await;
        let rows: serde_json::Value = serde_json::from_str(&listed.out).unwrap_or_default();
        assert_eq!(
            (
                rows[0]["host"].as_str(),
                rows[0]["kind"].as_str(),
                any_file_holds(&machine.cli.config_dir(), TOKEN)
            ),
            (Some("github.com"), Some("oauth"), false),
            "{listed:?}"
        );
    }
}

mod module_install {
    use super::*;

    // TC: LCL-01 (`lof module install` in the local context
    // downloads the module into the config home through the same registry
    // client: index, signature, hash, WIT version)
    #[tokio::test]
    async fn keeps_component_in_config_home_when_installed_without_server() {
        let machine = machine().await;
        let outcome = machine.cli.run(&["module", "install", "git.github"]).await;
        let kept =
            std::fs::read_dir(machine.cli.config_dir().join("modules")).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("git.github")
                })
            });
        assert_eq!((outcome.code, kept), (0, true), "{outcome:?}");
    }
}
