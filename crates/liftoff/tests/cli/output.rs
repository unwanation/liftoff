//! TC: v1 CLI-04.

use crate::support::{Cli, SESSION, Stub, json};

const ME: &str =
    r#"{"id":"u1","login":"owner","email":"owner@example.com","role":"admin","source":"local"}"#;

mod json_flag {
    use super::*;

    // TC: v1 CLI-04
    #[tokio::test]
    async fn prints_response_as_is_when_json_flag() {
        // Pretty-printed: typed re-serialisation would change whitespace.
        let value: serde_json::Value = serde_json::from_str(ME).unwrap();
        let body = serde_json::to_string_pretty(&value).unwrap();
        let stub = Stub::start()
            .await
            .route("GET", "/me", vec![json(200, &body)]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "whoami"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(outcome.out, format!("{}\n", body.trim_end()));
    }
}

mod human {
    use super::*;

    // TC: v1 CLI-04
    #[tokio::test]
    async fn whoami_prints_aligned_pairs() {
        let stub = Stub::start().await.route("GET", "/me", vec![json(200, ME)]);
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["whoami"]).await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
        assert_eq!(
            outcome.out,
            "login  owner\nemail  owner@example.com\nrole   admin\n"
        );
    }

    #[tokio::test]
    async fn sends_the_saved_token_as_bearer() {
        let stub = Stub::start().await.route("GET", "/me", vec![json(200, ME)]);
        let cli = Cli::logged_in(&stub.url);
        cli.run(&["whoami"]).await;
        let seen = stub.seen_at("GET", "/me");
        assert_eq!(
            seen.first().and_then(|s| s.authorization.as_deref()),
            Some(format!("Bearer {SESSION}").as_str())
        );
    }
}
