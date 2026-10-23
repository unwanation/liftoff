//! `lof templates [--group G]` with a server: the catalogue of `GET /git/templates` by
//! group, each template with its name and description; the CLI's
//! `[local] templates` go with it as `repos` (TPL-06).

use crate::create::CATALOGUE;
use crate::support::{Cli, SERVER, SESSION, Seen, Stub, json};

/// The catalogue of [`CATALOGUE`] plus a skipped repository.
const WARNED: &str = r#"{"groups":[{"group":"web","templates":[{"host":"github.com","repo":"acme/app","name":"acme-app","description":"SolidJS + acme-ui, Vite, Bun","kind":"service"}]}],"warnings":[{"repo":"acme/broken","reason":"no liftoff.toml"}]}"#;

async fn stub(catalogue: &str) -> Stub {
    Stub::start()
        .await
        .route("GET", "/git/templates", vec![json(200, catalogue)])
}

/// The query pairs of every `GET /git/templates` `stub` saw.
fn queries(stub: &Stub) -> Vec<Vec<(String, String)>> {
    stub.seen_at("GET", "/git/templates")
        .iter()
        .map(Seen::query_pairs)
        .collect()
}

mod list_templates {
    use super::*;

    // TC: TPL-03 (the catalogue by group, in its order, with names and
    // descriptions)
    #[tokio::test]
    async fn lists_groups_with_names_and_descriptions_when_templates_run() {
        let stub = stub(CATALOGUE).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["templates"]).await;
        let positions: Option<Vec<usize>> = [
            "web",
            "astro-site",
            "acme-app",
            "SolidJS + acme-ui, Vite, Bun",
            "api",
            "hono-api",
            "Hono on Bun",
        ]
        .iter()
        .map(|needle| outcome.out.find(needle))
        .collect();
        assert!(
            outcome.code == 0 && positions.is_some_and(|at| at.is_sorted()),
            "{outcome:?}"
        );
    }

    // TC: TPL-03 (`--group` asks for that group only)
    #[tokio::test]
    async fn asks_for_group_when_group_given() {
        let stub = stub(CATALOGUE).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["templates", "--group", "api"]).await;
        assert_eq!(
            (outcome.code, queries(&stub)),
            (0, vec![vec![("group".to_owned(), "api".to_owned())]]),
            "{outcome:?}"
        );
    }

    // TC: TPL-06 (`[local] templates` of the CLI config are added as
    // `repos`)
    #[tokio::test]
    async fn sends_local_templates_as_repos_when_config_lists_them() {
        let stub = stub(CATALOGUE).await;
        let cli = Cli::new();
        cli.write_config(&format!(
            "default_server = \"{SERVER}\"\n\n[servers.{SERVER}]\nurl = \"{}\"\ntoken = \"{SESSION}\"\n\n[local]\ntemplates = [\"acme/extra\", \"git.example.com/team/site\"]\n",
            stub.url
        ));
        let outcome = cli.run(&["templates"]).await;
        assert_eq!(
            (outcome.code, queries(&stub)),
            (
                0,
                vec![vec![(
                    "repos".to_owned(),
                    "acme/extra,git.example.com/team/site".to_owned()
                )]]
            ),
            "{outcome:?}"
        );
    }

    // TC: TPL-04 (a skipped repository is named with why)
    #[tokio::test]
    async fn names_skipped_repository_when_catalogue_warns() {
        let stub = stub(WARNED).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["templates"]).await;
        let printed = format!("{}{}", outcome.out, outcome.err);
        assert!(
            outcome.code == 0
                && printed.contains("acme/broken")
                && printed.contains("no liftoff.toml"),
            "{outcome:?}"
        );
    }

    // TC: TPL-03 (`--json` prints the answer as received)
    #[tokio::test]
    async fn prints_catalogue_as_received_when_json() {
        let stub = stub(CATALOGUE).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["--json", "templates"]).await;
        assert_eq!((outcome.code, outcome.out), (0, format!("{CATALOGUE}\n")));
    }
}

/// TC: TPL-03. The catalogue's text comes from template repositories: an
/// escape sequence in it reaches the terminal escaped.
mod hostile_text {
    use super::*;

    /// A catalogue whose description and warning carry escape sequences.
    const HOSTILE: &str = r#"{"groups":[{"group":"web","templates":[{"host":"github.com","repo":"acme/app","name":"acme-app","description":"x\u001b]0;owned\u0007","kind":"service"}]}],"warnings":[{"repo":"acme/broken","reason":"bad\u001b[2J"}]}"#;

    // TC: TPL-03
    #[tokio::test]
    async fn prints_no_control_character_when_catalogue_holds_escapes() {
        let stub = stub(HOSTILE).await;
        let cli = Cli::logged_in(&stub.url);
        let outcome = cli.run(&["templates"]).await;
        assert!(
            outcome.code == 0
                && !outcome.out.contains('\u{1b}')
                && !outcome.out.contains('\u{7}')
                && outcome.out.contains("x\u{fffd}]0;owned\u{fffd}"),
            "{outcome:?}"
        );
    }

    // TC: TPL-03, CRE-11 (nor in the wizard's list of templates)
    #[tokio::test]
    async fn offers_no_control_character_when_catalogue_holds_escapes() {
        let stub = stub(HOSTILE).await;
        let cli = Cli::logged_in(&stub.url).terminal(vec![]);
        let outcome = cli.run(&["create"]).await;
        // The script records a list's options as `{options:?}`, which
        // would show a raw control character as `\u{1b}`.
        let asked = cli.asked().join("\n");
        assert!(
            asked.contains("x\u{fffd}]0;owned\u{fffd}")
                && !asked.contains("\\u{1b}")
                && !outcome.err.contains('\u{1b}'),
            "{outcome:?} {:?}",
            cli.asked()
        );
    }
}
