//! TC: v1 CLI-02 (the `logout` half; the v2 login arrives with).

use std::os::unix::fs::PermissionsExt as _;

use crate::support::{Cli, Stub, json, problem};

mod logout {
    use super::*;

    // TC: v1 CLI-02
    #[tokio::test]
    async fn clears_the_token_and_says_so_when_the_server_agrees() {
        let stub = Stub::start()
            .await
            .route("DELETE", "/auth/session", vec![json(204, "")]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["logout"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert!(outcome.out.contains("logged out"), "{outcome:?}");
        assert!(
            !cli.config_text().contains("token"),
            "{}",
            cli.config_text()
        );
    }

    // TC: v1 CLI-02 (an expired or revoked session is already logged out)
    #[tokio::test]
    async fn succeeds_and_clears_the_token_when_the_session_is_already_gone() {
        let stub = Stub::start().await.route(
            "DELETE",
            "/auth/session",
            vec![problem(401, "session expired")],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["logout"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert!(outcome.out.contains("logged out"), "{outcome:?}");
        assert!(
            !cli.config_text().contains("token"),
            "{}",
            cli.config_text()
        );
    }

    // TC: v1 CLI-02
    #[tokio::test]
    async fn exits_1_but_clears_the_token_when_the_server_fails() {
        let stub = Stub::start()
            .await
            .route("DELETE", "/auth/session", vec![problem(500, "boom")]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["logout"]).await;
        assert_eq!(outcome.code, 1, "{outcome:?}");
        assert!(
            !cli.config_text().contains("token"),
            "{}",
            cli.config_text()
        );
    }
}

mod config_file {
    use super::*;

    // TC: v1 CLI-02 (the config stays private when rewritten)
    #[tokio::test]
    async fn narrows_mode_to_0600_when_rewritten_after_logout() {
        let stub = Stub::start()
            .await
            .route("DELETE", "/auth/session", vec![json(204, "")]);
        let cli = Cli::logged_in(&stub.url);
        std::fs::set_permissions(&cli.config, std::fs::Permissions::from_mode(0o644)).unwrap();
        cli.run(&["logout"]).await;
        let mode = std::fs::metadata(&cli.config).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // TC: v1 CLI-02
    #[tokio::test]
    async fn leaves_no_temporary_file_when_rewritten() {
        let stub = Stub::start()
            .await
            .route("DELETE", "/auth/session", vec![json(204, "")]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["logout"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        let mut temporary = cli.config.clone().into_os_string();
        temporary.push(".tmp");
        assert!(!std::path::Path::new(&temporary).exists());
    }
}
