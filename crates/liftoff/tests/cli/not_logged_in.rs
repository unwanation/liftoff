//! TC: v1 CLI-01.

use rstest::rstest;

use crate::support::{Cli, Stub};

// TC: v1 CLI-01
#[rstest]
#[case::whoami(&["whoami"])]
#[case::logout(&["logout"])]
// TC: MCP-03 (`lof mcp` needs the session of the config: without one it
// never starts a server)
#[case::mcp(&["mcp"])]
#[tokio::test]
async fn exits_1_when_no_config(#[case] args: &[&str]) {
    let stub = Stub::start().await;
    let cli = Cli::new();
    let outcome = cli.run(args).await;
    assert_eq!(outcome.code, 1, "{outcome:?}");
    assert!(outcome.err.contains("not logged in"), "{outcome:?}");
    assert!(outcome.out.is_empty(), "{outcome:?}");
    assert!(stub.seen().is_empty(), "no request without a session");
}

// TC: v1 CLI-01
#[tokio::test]
async fn exits_1_when_config_has_no_token() {
    let cli = Cli::new();
    cli.write_config("default_server = \"main\"\n\n[servers.main]\nurl = \"http://127.0.0.1:9\"\n");
    let outcome = cli.run(&["whoami"]).await;
    assert_eq!(outcome.code, 1, "{outcome:?}");
    assert!(outcome.err.contains("not logged in"), "{outcome:?}");
}
