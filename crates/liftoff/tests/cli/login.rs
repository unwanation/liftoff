//! TC: CLIX-08. `login <url> --token <t>` trades the single-use token for a
//! session and writes the config with mode `0600`.
//!
//! TC: OIDC-01, OIDC-04, OIDC-05. With the `oidc`
//! module installed, `login <url>` without `--token` runs the module's
//! device flow (its manifest `replaces: ["login"]`): `POST
//! /m/oidc/device`, then `POST /m/oidc/device/poll` until it is no longer
//! `202`. A server without the module answers the device route `404`, and
//! the CLI asks for the token as before.

use std::os::unix::fs::PermissionsExt as _;

use crate::fixtures::SESSION_GRANTED;
use crate::support::{Cli, Stub, json, problem};

const GRANTED_TOKEN: &str = "lfs_granted0123456789abcdefghijABCDEFGHIJ01";

async fn stub() -> Stub {
    Stub::start()
        .await
        .route("POST", "/auth/token", vec![json(200, SESSION_GRANTED)])
}

/// The `[servers.*]` tables of the config.
fn servers(cli: &Cli) -> toml::Table {
    cli.config_toml()
        .get("servers")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default()
}

mod token_exchange {
    use super::*;

    // TC: CLIX-08
    #[tokio::test]
    async fn posts_the_token_and_the_machine_name() {
        let stub = stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let sent = stub.seen_at("POST", "/auth/token");
        assert_eq!(
            sent.first().map(crate::support::Seen::json),
            Some(serde_json::json!({"token": "lft_setup", "name": "test-box"}))
        );
    }

    // TC: CLIX-08
    #[tokio::test]
    async fn says_who_logged_in() {
        let stub = stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert!(outcome.out.contains("owner"), "{outcome:?}");
        assert!(
            !outcome.out.contains(GRANTED_TOKEN),
            "the session token is not shown"
        );
    }

    // TC: CLIX-08 (a used or wrong token writes nothing)
    #[tokio::test]
    async fn exits_1_and_writes_nothing_when_the_token_is_refused() {
        let stub = Stub::start().await.route(
            "POST",
            "/auth/token",
            vec![problem(401, "unknown or used token")],
        );
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_used"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: unknown or used token\n")
        );
        assert!(!cli.config.exists());
    }
}

mod json_flag {
    use super::*;

    // TC: CLIX-03, CLIX-08 (the one exception to "the body as received":
    // `login --json` prints the user, never the session token)
    #[tokio::test]
    async fn prints_only_the_user_when_json_flag() {
        let stub = stub().await;
        let cli = Cli::new();
        let outcome = cli
            .run(&["--json", "login", &stub.url, "--token", "lft_setup"])
            .await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert!(!outcome.out.contains("lfs_"), "{outcome:?}");
        assert!(outcome.out.ends_with('\n'), "{outcome:?}");
        let printed: serde_json::Value = serde_json::from_str(&outcome.out).unwrap();
        let granted: serde_json::Value = serde_json::from_str(SESSION_GRANTED).unwrap();
        assert_eq!(printed, granted["user"]);
    }
}

mod config_file {
    use super::*;

    // TC: CLIX-08
    #[tokio::test]
    async fn saves_url_and_session_token_as_the_default_server() {
        let stub = stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let config = cli.config_toml();
        let servers = servers(&cli);
        assert_eq!(servers.len(), 1, "{config:?}");
        let (name, server) = servers.iter().next().unwrap();
        assert_eq!(
            (
                server.get("url").and_then(toml::Value::as_str),
                server.get("token").and_then(toml::Value::as_str),
                config.get("default_server").and_then(toml::Value::as_str)
            ),
            (
                Some(stub.url.as_str()),
                Some(GRANTED_TOKEN),
                Some(name.as_str())
            )
        );
    }

    // TC: CLIX-08
    #[tokio::test]
    async fn writes_the_config_with_mode_0600() {
        let stub = stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let mode = std::fs::metadata(&cli.config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // TC: CLIX-08 (an existing wider file is narrowed)
    #[tokio::test]
    async fn narrows_an_existing_config_to_0600() {
        let stub = stub().await;
        let cli = Cli::new();
        cli.write_config("");
        std::fs::set_permissions(&cli.config, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let mode = std::fs::metadata(&cli.config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // TC: CLIX-08 (the write is atomic)
    #[tokio::test]
    async fn leaves_no_temporary_file() {
        let stub = stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let mut temporary = cli.config.clone().into_os_string();
        temporary.push(".tmp");
        assert!(!std::path::Path::new(&temporary).exists());
    }

    // TC: CLIX-08 (several servers; a new one does not take over)
    #[tokio::test]
    async fn adds_a_second_server_and_keeps_the_default() {
        let stub = stub().await;
        let cli = Cli::new();
        cli.write_config(
            "default_server = \"west\"\n\n[servers.west]\nurl = \"https://liftoff.example.com\"\ntoken = \"lfs_primary\"\n",
        );
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let servers = servers(&cli);
        let added = servers
            .values()
            .filter(|server| {
                server.get("url").and_then(toml::Value::as_str) == Some(stub.url.as_str())
            })
            .count();
        assert_eq!(
            (
                servers.len(),
                added,
                servers["west"]["token"].as_str(),
                cli.config_toml()
                    .get("default_server")
                    .and_then(toml::Value::as_str)
            ),
            (2, 1, Some("lfs_primary"), Some("west"))
        );
    }

    // TC: CLIX-08 (logging in again to the same URL replaces its token)
    #[tokio::test]
    async fn replaces_the_token_when_the_url_is_already_saved() {
        let stub = stub().await;
        let cli = Cli::new();
        cli.write_config(&format!(
            "default_server = \"main\"\n\n[servers.main]\nurl = \"{}\"\ntoken = \"lfs_old\"\n",
            stub.url
        ));
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let servers = servers(&cli);
        assert_eq!(
            (servers.len(), servers["main"]["token"].as_str()),
            (1, Some(GRANTED_TOKEN))
        );
    }
}

mod device_flow {
    use super::*;

    const DEVICE_START: &str = r#"{"device_code":"dc-1","user_code":"ABCD-EFGH","verification_uri_complete":"https://id.example/device?user_code=ABCD-EFGH","interval":7,"expires_in":300}"#;
    const PENDING: &str = r#"{"status":"pending"}"#;

    /// A server with the `oidc` module: one pending poll, then the session.
    async fn oidc_stub() -> Stub {
        Stub::start()
            .await
            .route("POST", "/m/oidc/device", vec![json(200, DEVICE_START)])
            .route(
                "POST",
                "/m/oidc/device/poll",
                vec![json(202, PENDING), json(200, SESSION_GRANTED)],
            )
    }

    /// A server with the `oidc` module whose poll answers `answer`.
    async fn refusing_stub(answer: crate::support::Answer) -> Stub {
        Stub::start()
            .await
            .route("POST", "/m/oidc/device", vec![json(200, DEVICE_START)])
            .route("POST", "/m/oidc/device/poll", vec![answer])
    }

    // TC: OIDC-01 (`lof login <url>` through the module ends in a saved
    // session)
    #[tokio::test]
    async fn saves_session_token_when_device_login_is_confirmed() {
        let stub = oidc_stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let servers = servers(&cli);
        assert_eq!(
            servers
                .values()
                .map(|server| server.get("token").and_then(toml::Value::as_str))
                .collect::<Vec<_>>(),
            vec![Some(GRANTED_TOKEN)]
        );
    }

    // TC: OIDC-01 (the link and the code go to stderr: stdout is the
    // result's)
    #[tokio::test]
    async fn shows_link_and_code_on_stderr_when_device_login_starts() {
        let stub = oidc_stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert!(
            outcome
                .err
                .contains("https://id.example/device?user_code=ABCD-EFGH")
                && outcome.err.contains("ABCD-EFGH"),
            "{outcome:?}"
        );
    }

    // TC: OIDC-01 (says who logged in, never the session token)
    #[tokio::test]
    async fn says_who_logged_in_when_device_login_is_confirmed() {
        let stub = oidc_stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert!(
            outcome.code == 0
                && outcome.out.contains("owner")
                && !outcome.out.contains(GRANTED_TOKEN)
                && !outcome.err.contains(GRANTED_TOKEN),
            "{outcome:?}"
        );
    }

    // TC: OIDC-01, CLIX-03 (`--json` prints the user only)
    #[tokio::test]
    async fn prints_only_the_user_when_json_flag() {
        let stub = oidc_stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["--json", "login", &stub.url]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let printed: serde_json::Value = serde_json::from_str(&outcome.out).unwrap();
        let granted: serde_json::Value = serde_json::from_str(SESSION_GRANTED).unwrap();
        assert_eq!(printed, granted["user"]);
    }

    // TC: OIDC-04 (`202` means poll again, with the device code and the
    // machine's name)
    #[tokio::test]
    async fn polls_again_with_device_code_and_machine_when_pending() {
        let stub = oidc_stub().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let polls: Vec<serde_json::Value> = stub
            .seen_at("POST", "/m/oidc/device/poll")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        let poll = serde_json::json!({"device_code": "dc-1", "name": "test-box"});
        assert_eq!(polls, vec![poll.clone(), poll]);
    }

    // TC: OIDC-05 (denied: exit 1 with the server's detail, nothing
    // written)
    #[tokio::test]
    async fn exits_1_and_writes_nothing_when_login_is_denied() {
        let stub = refusing_stub(problem(403, "login was denied")).await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.ends_with("error: login was denied\n"),
                cli.config.exists()
            ),
            (1, true, false),
            "{outcome:?}"
        );
    }

    // TC: OIDC-05 (expired: exit 1 with the server's detail, nothing
    // written)
    #[tokio::test]
    async fn exits_1_and_writes_nothing_when_device_code_expired() {
        let stub = refusing_stub(problem(410, "device code expired")).await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.ends_with("error: device code expired\n"),
                cli.config.exists()
            ),
            (1, true, false),
            "{outcome:?}"
        );
    }

    // TC: OIDC-04, OIDC-05 (the server keeps answering `202`: the CLI
    // gives up once `expires_in` has passed, after expires_in / interval
    // polls, and writes nothing)
    #[tokio::test]
    async fn exits_1_when_server_stays_pending_past_expiry() {
        let start = r#"{"device_code":"dc-1","user_code":"ABCD-EFGH","verification_uri_complete":"https://id.example/device?user_code=ABCD-EFGH","interval":7,"expires_in":21}"#;
        let stub = Stub::start()
            .await
            .route("POST", "/m/oidc/device", vec![json(200, start)])
            .route("POST", "/m/oidc/device/poll", vec![json(202, PENDING)]);
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.ends_with("error: device code expired\n"),
                stub.seen_at("POST", "/m/oidc/device/poll").len(),
                cli.config.exists()
            ),
            (1, true, 3, false),
            "{outcome:?}"
        );
    }

    // TC: OIDC-05 (the module cannot start: exit 1 with the server's
    // detail, no poll, nothing written)
    #[tokio::test]
    async fn exits_1_with_detail_when_device_start_fails() {
        let stub = Stub::start().await.route(
            "POST",
            "/m/oidc/device",
            vec![problem(502, "the identity provider failed")],
        );
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert_eq!(
            (
                outcome.code,
                outcome
                    .err
                    .ends_with("error: the identity provider failed\n"),
                stub.seen_at("POST", "/m/oidc/device/poll").len(),
                cli.config.exists()
            ),
            (1, true, 0, false),
            "{outcome:?}"
        );
    }

    // TC: OIDC-01 (no `oidc` module: the device route is `404` and the
    // CLI wants the token, a usage error without a terminal)
    #[tokio::test]
    async fn asks_for_token_when_server_has_no_oidc_module() {
        let stub = Stub::start().await;
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str(), stub.trail()),
            (
                64,
                "error: missing required arguments: --token\n",
                vec![("POST".to_owned(), "/m/oidc/device".to_owned())]
            ),
            "{outcome:?}"
        );
        assert!(!cli.config.exists(), "nothing is written");
    }

    // TC: OIDC-01 (`--token` keeps the single-user exchange and never
    // starts the device flow)
    #[tokio::test]
    async fn exchanges_token_without_device_flow_when_token_given() {
        let stub = oidc_stub()
            .await
            .route("POST", "/auth/token", vec![json(200, SESSION_GRANTED)]);
        let cli = Cli::new();
        let outcome = cli.run(&["login", &stub.url, "--token", "lft_setup"]).await;
        assert_eq!(
            (outcome.code, stub.trail()),
            (0, vec![("POST".to_owned(), "/auth/token".to_owned())]),
            "{outcome:?}"
        );
    }
}
