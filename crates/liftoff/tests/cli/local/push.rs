//! TC: LCL-05. `lof secret push` copies the local store's secrets of the
//! project to the server it is now connected to (`PUT /secrets` per secret, its scope as locally); `--move` deletes them locally once
//! the server has them, and never before.

use super::keys;
use crate::support::{Cli, Stub, json, problem};

const SECRET: &str = r#"{"key":"API_KEY","scope":"environment","version":1,"ci":false,"updated_by":"u1","updated_at":"2026-10-20T10:00:00Z"}"#;

/// A CLI that set `API_KEY=local-value` of project `demo` without a
/// server, then logged in to `server`.
async fn local_secret_then_login(server: &Stub) -> Cli {
    let cli = Cli::new();
    let set = cli
        .run(&["--project", "demo", "secret", "set", "API_KEY=local-value"])
        .await;
    assert_eq!(set.code, 0, "secret set: {set:?}");
    cli.log_in(&server.url);
    cli
}

/// The local value of `API_KEY` once the config is gone again (local
/// mode), if there is one.
async fn local_value(cli: &Cli) -> Option<String> {
    cli.forget_config();
    let outcome = cli
        .run(&["--project", "demo", "secret", "get", "API_KEY"])
        .await;
    (outcome.code == 0).then(|| outcome.out.trim_end().to_owned())
}

mod secret_push {
    use super::*;

    // TC: LCL-05
    #[tokio::test]
    async fn puts_local_secret_to_server_when_pushed() {
        let server = Stub::start()
            .await
            .route("PUT", "/secrets", vec![json(200, SECRET)]);
        let cli = local_secret_then_login(&server).await;
        let outcome = cli.run(&["--project", "demo", "secret", "push"]).await;
        let sent: Vec<serde_json::Value> = server
            .seen_at("PUT", "/secrets")
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(
            (outcome.code, sent),
            (
                0,
                vec![serde_json::json!({
                    "scope": "environment",
                    "id": "demo/prod",
                    "key": "API_KEY",
                    "value": "local-value",
                    "ci": false,
                })]
            ),
            "{outcome:?}"
        );
    }

    // TC: LCL-05 (without `--move` the local copy stays)
    #[tokio::test]
    async fn keeps_local_secret_when_pushed_without_move() {
        let server = Stub::start()
            .await
            .route("PUT", "/secrets", vec![json(200, SECRET)]);
        let cli = local_secret_then_login(&server).await;
        let _outcome = cli.run(&["--project", "demo", "secret", "push"]).await;
        assert_eq!(local_value(&cli).await.as_deref(), Some("local-value"));
    }

    // TC: LCL-05
    #[tokio::test]
    async fn deletes_local_secret_when_pushed_with_move() {
        let server = Stub::start()
            .await
            .route("PUT", "/secrets", vec![json(200, SECRET)]);
        let cli = local_secret_then_login(&server).await;
        let pushed = cli
            .run(&["--project", "demo", "secret", "push", "--move"])
            .await;
        cli.forget_config();
        let listed = cli
            .run(&["--json", "--project", "demo", "secret", "ls"])
            .await;
        assert_eq!(
            (pushed.code, listed.code, keys(&listed.out)),
            (0, 0, Vec::<String>::new()),
            "{pushed:?} {listed:?}"
        );
    }

    // TC: LCL-05 (`--move` deletes only what the server took)
    #[tokio::test]
    async fn keeps_local_secret_when_server_refuses_push_with_move() {
        let server = Stub::start()
            .await
            .route("PUT", "/secrets", vec![problem(403, "not yours")]);
        let cli = local_secret_then_login(&server).await;
        let pushed = cli
            .run(&["--project", "demo", "secret", "push", "--move"])
            .await;
        assert_eq!(
            (pushed.code, local_value(&cli).await.as_deref()),
            (1, Some("local-value")),
            "{pushed:?}"
        );
    }
}

mod secret_push_without_local_store {
    use super::*;

    // TC: LCL-05 (nothing local: nothing is sent, and the server-mode
    // command makes no local key or store as a side effect)
    #[tokio::test]
    async fn makes_no_local_key_when_nothing_was_set_locally() {
        let server = Stub::start()
            .await
            .route("PUT", "/secrets", vec![json(200, SECRET)]);
        let cli = Cli::logged_in(&server.url);
        let outcome = cli.run(&["--project", "demo", "secret", "push"]).await;
        assert_eq!(
            (
                outcome.code,
                server.seen_at("PUT", "/secrets").len(),
                cli.config_dir().join("key").exists(),
                cli.config_dir().join("secrets.db").exists(),
            ),
            (0, 0, false, false),
            "{outcome:?}"
        );
    }
}
