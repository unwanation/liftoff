//! TC: CLIX-05. Several servers in the config: `default_server`, `--server`
//! and `.liftoff` pick one; `server ls|use|status` manage them.

use std::os::unix::fs::PermissionsExt as _;

use crate::fixtures::ME;
use crate::support::{Cli, HostRun, Stub, json};

/// Two installations: `west` (the default) and `staging`.
async fn two() -> (Stub, Stub, Cli) {
    let west = Stub::start()
        .await
        .route("GET", "/me", vec![json(200, ME)])
        .route("GET", "/healthz", vec![json(200, r#"{"status":"ok"}"#)]);
    let staging = Stub::start()
        .await
        .route("GET", "/me", vec![json(200, ME)])
        .route("GET", "/healthz", vec![json(200, r#"{"status":"ok"}"#)]);
    let cli = Cli::new();
    cli.write_config(&format!(
        "default_server = \"west\"\n\n[servers.west]\nurl = \"{}\"\ntoken = \"lfs_primary\"\n\n[servers.staging]\nurl = \"{}\"\ntoken = \"lfs_staging\"\n",
        west.url, staging.url
    ));
    (west, staging, cli)
}

fn bearer(stub: &Stub) -> Vec<Option<String>> {
    stub.seen_at("GET", "/me")
        .into_iter()
        .map(|seen| seen.authorization)
        .collect()
}

mod choice {
    use super::*;

    // TC: CLIX-05
    #[tokio::test]
    async fn uses_the_default_server_and_its_token_when_no_flag() {
        let (west, staging, cli) = two().await;
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(bearer(&west), vec![Some("Bearer lfs_primary".to_owned())]);
        assert!(staging.seen().is_empty());
    }

    // TC: CLIX-05
    #[tokio::test]
    async fn uses_the_named_server_and_its_token_when_server_flag() {
        let (west, staging, cli) = two().await;
        let outcome = cli.run(&["--server", "staging", "whoami"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            bearer(&staging),
            vec![Some("Bearer lfs_staging".to_owned())]
        );
        assert!(west.seen().is_empty());
    }

    // TC: CLIX-05 (`.liftoff` overrides the server)
    #[tokio::test]
    async fn uses_the_dotfile_server_when_dotfile_names_one() {
        let (west, staging, cli) = two().await;
        cli.dotfile("server = \"staging\"\n");
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            bearer(&staging),
            vec![Some("Bearer lfs_staging".to_owned())]
        );
        assert!(west.seen().is_empty());
    }

    // TC: CLIX-05 (a flag beats the file)
    #[tokio::test]
    async fn uses_the_server_flag_when_both_flag_and_dotfile() {
        let (west, staging, cli) = two().await;
        cli.dotfile("server = \"staging\"\n");
        let outcome = cli.run(&["--server", "west", "whoami"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(bearer(&west), vec![Some("Bearer lfs_primary".to_owned())]);
        assert!(staging.seen().is_empty());
    }

    // TC: CLIX-05 (an unknown name is a config error)
    #[tokio::test]
    async fn exits_78_naming_the_server_when_it_is_not_in_the_config() {
        let (west, staging, cli) = two().await;
        let outcome = cli.run(&["--server", "nope", "whoami"]).await;
        assert_eq!(outcome.code, 78, "{outcome:?}");
        assert!(outcome.err.starts_with("error: "), "{outcome:?}");
        assert!(outcome.err.contains("nope"), "{outcome:?}");
        assert!(west.seen().is_empty() && staging.seen().is_empty());
    }

    // TC: CLIX-05 (a broken config is exit 78)
    #[tokio::test]
    async fn exits_78_when_the_config_is_not_toml() {
        let cli = Cli::new();
        cli.write_config("default_server = \n");
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 78, "{outcome:?}");
        assert!(outcome.err.starts_with("error: "), "{outcome:?}");
    }

    // TC: CLIX-05 (the v1 single-server layout is not read silently)
    #[tokio::test]
    async fn exits_78_when_the_config_has_the_v1_layout() {
        let cli = Cli::new();
        cli.write_config("server = \"https://liftoff.example.com\"\ntoken = \"lf_old\"\n");
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 78, "{outcome:?}");
    }
}

mod server_commands {
    use super::*;

    // TC: CLIX-05
    #[tokio::test]
    async fn ls_lists_every_server_with_its_url() {
        let (west, staging, cli) = two().await;
        let outcome = cli.run(&["server", "ls"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let line = |name: &str| {
            outcome
                .out
                .lines()
                .find(|line| line.split_whitespace().any(|word| word == name))
                .map(str::to_owned)
        };
        let primary_line = line("west").expect("west is listed");
        let staging_line = line("staging").expect("staging is listed");
        assert!(primary_line.contains(&west.url), "{primary_line}");
        assert!(staging_line.contains(&staging.url), "{staging_line}");
        assert!(
            west.seen().is_empty() && staging.seen().is_empty(),
            "ls is local"
        );
    }

    // TC: CLIX-05 (ls never prints a token)
    #[tokio::test]
    async fn ls_prints_no_token() {
        let (_primary, _staging, cli) = two().await;
        let outcome = cli.run(&["server", "ls"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert!(outcome.out.contains("staging"), "{outcome:?}");
        assert!(!outcome.out.contains("lfs_"), "{outcome:?}");
    }

    // TC: CLIX-05
    #[tokio::test]
    async fn use_makes_the_named_server_the_default() {
        let (west, staging, cli) = two().await;
        let outcome = cli.run(&["server", "use", "staging"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let config = cli.config_toml();
        assert_eq!(
            config.get("default_server").and_then(toml::Value::as_str),
            Some("staging")
        );
        let servers = config
            .get("servers")
            .and_then(toml::Value::as_table)
            .unwrap();
        assert_eq!(
            (
                servers["west"]["url"].as_str(),
                servers["west"]["token"].as_str(),
                servers["staging"]["url"].as_str(),
                servers["staging"]["token"].as_str()
            ),
            (
                Some(west.url.as_str()),
                Some("lfs_primary"),
                Some(staging.url.as_str()),
                Some("lfs_staging")
            )
        );
        let after = cli.run(&["whoami"]).await;
        assert_eq!(after.code, 0, "{after:?}");
        assert_eq!(
            bearer(&staging),
            vec![Some("Bearer lfs_staging".to_owned())]
        );
    }

    // TC: CLIX-05 (the rewritten config stays private)
    #[tokio::test]
    async fn use_keeps_the_config_at_0600() {
        let (_primary, _staging, cli) = two().await;
        std::fs::set_permissions(&cli.config, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outcome = cli.run(&["server", "use", "staging"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let mode = std::fs::metadata(&cli.config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // TC: CLIX-05
    #[tokio::test]
    async fn use_exits_78_and_keeps_the_default_when_the_name_is_unknown() {
        let (_primary, _staging, cli) = two().await;
        let before = cli.config_text();
        let outcome = cli.run(&["server", "use", "nope"]).await;
        assert_eq!(outcome.code, 78, "{outcome:?}");
        assert_eq!(cli.config_text(), before);
    }

    // TC: CLIX-05
    #[tokio::test]
    async fn status_asks_the_chosen_server() {
        let (west, staging, cli) = two().await;
        let outcome = cli.run(&["--server", "staging", "server", "status"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert!(!staging.seen().is_empty(), "staging is asked");
        assert!(west.seen().is_empty(), "{:?}", west.trail());
    }
}

/// TC: SETUP-07. `lof server upgrade` on the server's host.
mod upgrade {
    use super::*;

    /// The installer of the release, as the repository has it.
    fn install_script() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh"),
        )
        .unwrap()
    }

    // TC: SETUP-07 (`install.sh` checks the signed SHA256SUMS and replaces
    // the binaries: SETUP-08)
    #[tokio::test]
    async fn runs_the_installer_then_restarts_the_unit() {
        let (west, staging, cli) = two().await;
        let outcome = cli.run(&["server", "upgrade"]).await;
        assert_eq!(
            (outcome.code, cli.host_runs()),
            (
                0,
                vec![
                    HostRun {
                        program: "sh".to_owned(),
                        args: vec!["-s".to_owned()],
                        stdin: Some(install_script()),
                    },
                    HostRun {
                        program: "systemctl".to_owned(),
                        args: vec!["restart".to_owned(), "liftoffd.service".to_owned()],
                        stdin: None,
                    },
                ]
            ),
            "{}",
            outcome.err
        );
        assert!(west.seen().is_empty() && staging.seen().is_empty());
    }

    // TC: SETUP-07 (a failed install leaves the running daemon alone)
    #[tokio::test]
    async fn restarts_nothing_when_the_installer_fails() {
        let cli = Cli::new().failing_host("sh");
        let outcome = cli.run(&["server", "upgrade"]).await;
        let programs: Vec<String> = cli.host_runs().into_iter().map(|run| run.program).collect();
        assert_eq!(
            (outcome.code, programs, outcome.err.as_str()),
            (
                1,
                vec!["sh".to_owned()],
                "error: `sh` failed (exit status: 1)\n"
            )
        );
    }

    // TC: SETUP-07 (it runs on the server's host, where there may be no
    // session)
    #[tokio::test]
    async fn needs_no_config_when_run_on_the_host() {
        let cli = Cli::new();
        let outcome = cli.run(&["server", "upgrade"]).await;
        assert_eq!(
            (outcome.code, cli.host_runs().len()),
            (0, 2),
            "{}",
            outcome.err
        );
    }
}
