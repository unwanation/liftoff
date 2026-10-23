//! TC: LCL-08. The server of a run is `.liftoff`, else the config, else
//! none (local mode). A `.liftoff` naming a server the
//! config does not have is a config error (exit 78) with the name, never
//! a silent fall back to local mode.

use crate::support::{Cli, Stub};

/// `.liftoff` naming `west`, which the config does not have.
const DOTFILE: &str = "server = \"west\"\n";

mod dotfile_server {
    use super::*;

    // TC: LCL-08
    #[tokio::test]
    async fn exits_78_when_dotfile_names_server_and_there_is_no_config() {
        let cli = Cli::new();
        cli.dotfile(DOTFILE);
        let outcome = cli.run(&["--project", "demo", "secret", "ls"]).await;
        assert_eq!(outcome.code, 78, "{outcome:?}");
    }

    // TC: LCL-08 (the error names the server and where to look)
    #[tokio::test]
    async fn names_missing_server_when_dotfile_names_server_and_there_is_no_config() {
        let cli = Cli::new();
        cli.dotfile(DOTFILE);
        let outcome = cli.run(&["--project", "demo", "secret", "ls"]).await;
        assert!(
            outcome.err.starts_with("error: ") && outcome.err.contains("\"west\""),
            "{outcome:?}"
        );
    }

    // TC: LCL-08 (nothing is kept locally instead)
    #[tokio::test]
    async fn keeps_nothing_locally_when_dotfile_names_server_and_there_is_no_config() {
        let cli = Cli::new();
        cli.dotfile(DOTFILE);
        let outcome = cli
            .run(&["--project", "demo", "secret", "set", "API_KEY=v"])
            .await;
        assert_eq!(
            (outcome.code, cli.config_dir().join("secrets.db").exists()),
            (78, false),
            "{outcome:?}"
        );
    }

    // TC: LCL-08 (a config with other servers: the same error, nothing
    // sent to them)
    #[tokio::test]
    async fn exits_78_when_dotfile_names_server_missing_from_config() {
        let other = Stub::start().await;
        let cli = Cli::logged_in(&other.url);
        cli.dotfile(DOTFILE);
        let outcome = cli.run(&["--project", "demo", "secret", "ls"]).await;
        assert_eq!(
            (
                outcome.code,
                outcome.err.contains("\"west\""),
                other.seen().len()
            ),
            (78, true, 0),
            "{outcome:?}"
        );
    }
}
