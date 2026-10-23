//! TC: LCL-03. Without a server `lof create <name> --template` runs the
//! plan of the core's `CreateService` up to `Push`:
//! the forge generates the repository from the template through the
//! `git.github` module, the CLI clones it, applies `[substitute]`,
//! generates the `[secrets]` with `generate` into the local store,
//! commits and pushes; then it stops and says that the project is ready
//! for `lof server connect`.

use super::forge::{TEMPLATE, TEMPLATE_MANIFEST};
use super::{Machine, connected, keys};
use crate::support::Outcome;

/// The repository the forge generates for `demo` under `owner`.
const REPO: &str = "owner/demo";

/// The compose file of the template.
const COMPOSE: &str = "services:\n  web:\n    image: ghcr.io/owner/demo:latest\n";

/// A connected machine whose forge holds what it generates for `demo`,
/// after `lof create demo --template acme/app`.
async fn created() -> (Machine, Outcome) {
    let machine = connected().await;
    machine.git.seed_remote(
        REPO,
        &[
            ("liftoff.toml", TEMPLATE_MANIFEST),
            ("README.md", "# {{project}}\n"),
            ("deploy/compose.yaml", COMPOSE),
        ],
    );
    let outcome = machine
        .cli
        .run(&["create", "demo", "--template", TEMPLATE])
        .await;
    (machine, outcome)
}

mod create_without_server {
    use super::*;

    // TC: LCL-03
    #[tokio::test]
    async fn exits_0_when_plan_stops_after_push() {
        let (_machine, outcome) = created().await;
        assert_eq!(outcome.code, 0, "{outcome:?}");
    }

    // TC: LCL-03 (the forge generates the repository, private under the connection's account by default)
    #[tokio::test]
    async fn generates_repo_from_template_when_created_without_server() {
        let (machine, _outcome) = created().await;
        let bodies: Vec<serde_json::Value> = machine
            .forge
            .seen_at("POST", "/repos/acme/app/generate")
            .iter()
            .map(super::super::forge::Seen::json)
            .collect();
        assert_eq!(
            bodies,
            vec![serde_json::json!({
                "owner": "owner",
                "name": "demo",
                "private": true,
                "include_all": true,
            })]
        );
    }

    // TC: LCL-03 (substituted, committed and pushed)
    #[tokio::test]
    async fn pushes_substituted_files_when_created_without_server() {
        let (machine, _outcome) = created().await;
        assert_eq!(
            machine.git.remote_file(REPO, "main", "README.md"),
            "# demo\n"
        );
    }

    // TC: LCL-03 (the clone is `./<name>`)
    #[tokio::test]
    async fn leaves_clone_in_working_directory_when_created_without_server() {
        let (machine, _outcome) = created().await;
        let readme = std::fs::read_to_string(machine.cli.cwd.join("demo").join("README.md"));
        assert_eq!(readme.ok().as_deref(), Some("# demo\n"));
    }

    // TC: LCL-03 (without a server it says the project is ready for `lof server connect`)
    #[tokio::test]
    async fn prints_server_connect_as_next_step_when_created_without_server() {
        let (_machine, outcome) = created().await;
        assert!(outcome.out.contains("lof server connect"), "{outcome:?}");
    }

    // TC: LCL-03 (the plan stops after `Push`: `WriteLiftoffFile`, which
    // names the server, is one of the steps after it)
    #[tokio::test]
    async fn writes_no_dotfile_when_created_without_server() {
        let (machine, _outcome) = created().await;
        assert!(!machine.cli.cwd.join("demo").join(".liftoff").exists());
    }

    // TC: LCL-03 (`secret *` work on the local store; a secret without `scope` goes to the project)
    #[tokio::test]
    async fn keeps_generated_secret_locally_when_created_without_server() {
        let (machine, _outcome) = created().await;
        let listed = machine
            .cli
            .run(&[
                "--json",
                "--project",
                "demo",
                "secret",
                "ls",
                "--scope",
                "project",
            ])
            .await;
        assert_eq!(
            keys(&listed.out),
            vec!["SESSION_SECRET".to_owned()],
            "{listed:?}"
        );
    }

    // TC: LCL-03, CRE-03 (`generate = "random:32"`: 32 bytes in URL-safe
    // base64 without padding, 43 characters)
    #[tokio::test]
    async fn generates_43_character_secret_when_created_without_server() {
        let (machine, _outcome) = created().await;
        let value = machine
            .cli
            .run(&[
                "--project",
                "demo",
                "secret",
                "get",
                "SESSION_SECRET",
                "--scope",
                "project",
            ])
            .await;
        assert_eq!(value.out.trim_end().len(), 43, "{value:?}");
    }

    // TC: LCL-03 (the server's plan needs the person's forge connection
    // there: the next steps say to make it)
    #[tokio::test]
    async fn prints_git_connect_on_server_as_next_step_when_created_without_server() {
        let (_machine, outcome) = created().await;
        assert!(
            outcome.out.contains("lof git connect github"),
            "{outcome:?}"
        );
    }
}

/// TC: LCL-03. `[substitute]` rewrites files of the clone only: a
/// template's symlink is not followed out of `./<name>`.
mod substitute_confined {
    use super::*;

    // TC: LCL-03
    #[tokio::test]
    async fn leaves_symlink_target_alone_when_template_links_substituted_file() {
        let machine = connected().await;
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim.md");
        std::fs::write(&target, "# {{project}}\n").unwrap();
        machine.git.seed_remote_with_links(
            REPO,
            &[
                ("liftoff.toml", TEMPLATE_MANIFEST),
                ("deploy/compose.yaml", COMPOSE),
            ],
            &[("README.md", target.as_path())],
        );
        let outcome = machine
            .cli
            .run(&["create", "demo", "--template", TEMPLATE])
            .await;
        assert_eq!(
            std::fs::read_to_string(&target).ok().as_deref(),
            Some("# {{project}}\n"),
            "{outcome:?}"
        );
    }
}
