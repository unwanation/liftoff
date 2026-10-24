//! TC: CLIX-07. `secret pull` prints the `.env` of the environment in the
//! order `GET /secrets/resolve` returns, and `--write` writes it into
//! `.env.<env>`. `secret sync`
//! sends the project's `ci = true` secrets to its forge through
//! `POST /secrets/sync` (SYNC).

use crate::support::{Cli, Stub, json, problem};

const RESOLVE: &str = "/secrets/resolve";

async fn pull(body: &str) -> (crate::support::Outcome, Stub) {
    let stub = Stub::start()
        .await
        .route("GET", RESOLVE, vec![json(200, body)]);
    let cli = Cli::logged_in(&stub.url);
    let outcome = cli.run(&["--project", "liftoff", "secret", "pull"]).await;
    (outcome, stub)
}

mod pull {
    use super::*;

    // TC: CLIX-07
    #[tokio::test]
    async fn prints_key_value_lines_in_resolve_order() {
        let (outcome, _stub) = pull(crate::fixtures::RESOLVED_ENV).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (
                0,
                "ZETA=last-alphabetically\nALPHA=1\nLIFTOFF_PROJECT=liftoff\nLIFTOFF_ENV=prod\n"
            ),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIX-07
    #[tokio::test]
    async fn asks_for_the_project_and_default_environment() {
        let (_outcome, stub) = pull(crate::fixtures::RESOLVED_ENV).await;
        let sent = stub.seen_at("GET", RESOLVE);
        assert_eq!(
            sent.first().map(crate::support::Seen::query_pairs),
            Some(vec![
                ("project".to_owned(), "liftoff".to_owned()),
                ("env".to_owned(), "prod".to_owned())
            ])
        );
    }

    // TC: CLIX-07 (a value a `.env` reader would split is double-quoted,
    // with `\n`, `\"` and `\\` escaped)
    #[tokio::test]
    async fn quotes_values_with_spaces_newlines_and_quotes() {
        let body = r#"{"vars":[{"key":"GREETING","value":"hello world"},{"key":"MULTI","value":"one\ntwo"},{"key":"QUOTE","value":"say \"hi\""},{"key":"PLAIN","value":"postgres://db:5432/app"}]}"#;
        let (outcome, _stub) = pull(body).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (
                0,
                "GREETING=\"hello world\"\nMULTI=\"one\\ntwo\"\nQUOTE=\"say \\\"hi\\\"\"\nPLAIN=postgres://db:5432/app\n"
            ),
            "{:?}",
            outcome.err
        );
    }

    // TC: CLIX-07
    #[tokio::test]
    async fn prints_nothing_when_the_env_is_empty() {
        let (outcome, _stub) = pull(r#"{"vars":[]}"#).await;
        assert_eq!((outcome.code, outcome.out.as_str()), (0, ""));
    }

    // TC: CLIX-07 (the CLI owns stdout: nothing but the `.env` goes there)
    #[tokio::test]
    async fn writes_nothing_else_to_stdout() {
        let (outcome, _stub) = pull(r#"{"vars":[{"key":"A","value":"1"}]}"#).await;
        assert_eq!(outcome.out, "A=1\n");
    }
}

mod pull_write {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// `secret pull --write` against a server answering `body`.
    async fn pull_write(body: &str) -> (crate::support::Outcome, Cli) {
        let stub = Stub::start()
            .await
            .route("GET", RESOLVE, vec![json(200, body)]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli
            .run(&["--project", "liftoff", "secret", "pull", "--write"])
            .await;
        (outcome, cli)
    }

    // TC: CLIX-07 (`--write`: the `.env` of the environment in `.env.<env>`)
    #[tokio::test]
    async fn writes_env_file_of_environment_when_write() {
        let (outcome, cli) = pull_write(crate::fixtures::RESOLVED_ENV).await;
        let written = std::fs::read_to_string(cli.root.join(".env.prod")).unwrap_or_default();
        assert_eq!(
            (outcome.code, written.as_str()),
            (
                0,
                "ZETA=last-alphabetically\nALPHA=1\nLIFTOFF_PROJECT=liftoff\nLIFTOFF_ENV=prod\n"
            ),
            "{outcome:?}"
        );
    }

    // TC: CLIX-07 (secrets: readable by the owner only)
    #[tokio::test]
    async fn owner_only_file_when_write() {
        let (_outcome, cli) = pull_write(crate::fixtures::RESOLVED_ENV).await;
        let mode = std::fs::metadata(cli.root.join(".env.prod"))
            .map(|meta| meta.permissions().mode() & 0o777)
            .ok();
        assert_eq!(mode, Some(0o600));
    }

    // TC: CLIX-07 (stdout names the file, never a value)
    #[tokio::test]
    async fn prints_path_without_values_when_write() {
        let (outcome, cli) = pull_write(crate::fixtures::RESOLVED_ENV).await;
        assert_eq!(
            outcome.out,
            format!("{}\n", cli.root.join(".env.prod").display())
        );
    }

    // TC: CLIX-07 (a second pull replaces the file whole)
    #[tokio::test]
    async fn replaces_env_file_when_written_again() {
        let stub = Stub::start().await.route(
            "GET",
            RESOLVE,
            vec![
                json(
                    200,
                    r#"{"vars":[{"key":"A","value":"1"},{"key":"B","value":"2"}]}"#,
                ),
                json(200, r#"{"vars":[{"key":"A","value":"3"}]}"#),
            ],
        );
        let cli = Cli::logged_in(&stub.url);
        let words = ["--project", "liftoff", "secret", "pull", "--write"];
        cli.run(&words).await;
        cli.run(&words).await;
        let written = std::fs::read_to_string(cli.root.join(".env.prod")).unwrap_or_default();
        assert_eq!(written, "A=3\n");
    }
}

const SYNC: &str = "/secrets/sync";

async fn sync(args: &[&str], body: &str) -> (crate::support::Outcome, Stub) {
    let stub = Stub::start()
        .await
        .route("POST", SYNC, vec![json(200, body)]);
    let cli = Cli::logged_in(&stub.url);
    let words: Vec<&str> = ["--project", "liftoff"]
        .into_iter()
        .chain(args.iter().copied())
        .chain(["secret", "sync"])
        .collect();
    let outcome = cli.run(&words).await;
    (outcome, stub)
}

const SYNCED: &str = r#"{"synced":["LIFTOFF_TOKEN"],"unchanged":["DB_URL"],"failed":[]}"#;
const PARTLY: &str = r#"{"synced":["LIFTOFF_TOKEN"],"unchanged":[],"failed":[{"key":"API_KEY","error":"forge answered 403"}]}"#;

mod sync {
    use super::*;

    // TC: SYNC-01 (the CLI asks the server for the project's sync)
    #[tokio::test]
    async fn posts_the_project_to_secrets_sync() {
        let (_outcome, stub) = sync(&[], SYNCED).await;
        let sent: Vec<serde_json::Value> = stub
            .seen_at("POST", SYNC)
            .iter()
            .map(crate::support::Seen::json)
            .collect();
        assert_eq!(sent, vec![serde_json::json!({"project": "liftoff"})]);
    }

    // TC: SYNC-03 (what was sent and what the forge already held)
    #[tokio::test]
    async fn prints_each_key_with_its_outcome() {
        let (outcome, _stub) = sync(&[], SYNCED).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (0, "synced LIFTOFF_TOKEN\nunchanged DB_URL\n"),
            "{:?}",
            outcome.err
        );
    }

    // TC: SYNC-01
    #[tokio::test]
    async fn says_so_when_no_secret_is_marked_ci() {
        let (outcome, _stub) = sync(&[], r#"{"synced":[],"unchanged":[],"failed":[]}"#).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (
                0,
                "no CI secrets in liftoff (`lof secret set K=V --scope project --ci`)\n"
            )
        );
    }

    // TC: SYNC-02 (the other keys were sent; the run still fails)
    #[tokio::test]
    async fn exits_1_naming_the_failed_keys_when_the_forge_refused_one() {
        let (outcome, _stub) = sync(&[], PARTLY).await;
        assert_eq!(
            (outcome.code, outcome.out.as_str(), outcome.err.as_str()),
            (
                1,
                "synced LIFTOFF_TOKEN\nfailed API_KEY: forge answered 403\n",
                "error: 1 CI secret not synced: API_KEY\n"
            )
        );
    }

    // TC: SYNC-02 (`--json` prints the answer as received)
    #[tokio::test]
    async fn prints_the_answer_as_received_with_json() {
        let (outcome, _stub) = sync(&["--json"], PARTLY).await;
        let printed: serde_json::Value =
            serde_json::from_str(&outcome.out).unwrap_or(serde_json::Value::Null);
        assert_eq!(
            (outcome.code, printed),
            (
                1,
                serde_json::from_str::<serde_json::Value>(PARTLY)
                    .unwrap_or(serde_json::Value::Null)
            )
        );
    }

    // TC: SYNC-01
    #[tokio::test]
    async fn exits_1_with_the_detail_when_the_server_refuses() {
        let stub = Stub::start().await.route(
            "POST",
            SYNC,
            vec![problem(404, "project liftoff not found")],
        );
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--project", "liftoff", "secret", "sync"]).await;
        assert_eq!(
            (outcome.code, outcome.err.as_str()),
            (1, "error: project liftoff not found\n")
        );
    }
}
