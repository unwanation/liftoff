//! TC: SNIP-01..SNIP-03 through the CLI. `lof token snippet` prints the
//! workflow the server rendered for the project's forge under a YAML
//! comment naming the file it belongs at, so `lof token snippet >
//! <path>` writes a valid workflow.

use crate::support::{Cli, Outcome, Stub, json};

const SNIPPET: &str = "/projects/liftoff/snippet";

async fn token_snippet(body: &str) -> Outcome {
    let stub = Stub::start()
        .await
        .route("GET", SNIPPET, vec![json(200, body)]);
    let cli = Cli::logged_in(&stub.url);
    cli.run(&["--project", "liftoff", "token", "snippet"]).await
}

mod token_snippet {
    use super::*;

    // TC: SNIP-02
    #[tokio::test]
    async fn prints_github_workflow_under_its_path_when_project_forge_is_github() {
        let outcome = token_snippet(
            r#"{"path":".github/workflows/deploy.yml","snippet":"name: deploy\non: [push]\n"}"#,
        )
        .await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (
                0,
                "# .github/workflows/deploy.yml\nname: deploy\non: [push]\n"
            ),
            "{:?}",
            outcome.err
        );
    }

    // TC: SNIP-03
    #[tokio::test]
    async fn prints_gitlab_ci_file_under_its_path_when_project_forge_is_gitlab() {
        let outcome = token_snippet(
            r#"{"path":".gitlab-ci.yml","snippet":"deploy:\n  script:\n    - make\n"}"#,
        )
        .await;
        assert_eq!(
            (outcome.code, outcome.out.as_str()),
            (0, "# .gitlab-ci.yml\ndeploy:\n  script:\n    - make\n"),
            "{:?}",
            outcome.err
        );
    }
}
