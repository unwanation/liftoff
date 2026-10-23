#![cfg(test)]
//! SNIP-01..SNIP-03: the CI workflow of each forge for a compose project
//! and for a function.
//!
//! Each snapshot (`tests/snapshots/snip*.snap`) holds both renders of one
//! provider, each under a `==> <path> (<kind>)` line naming the file the
//! workflow belongs at. The inputs: the compose
//! project `deploy/compose.yaml` exposing `web:3000`, and the function
//! built by `componentize-js` into `dist/fn.wasm`; the API site is
//! `liftoff.example.com`.

use liftoff_core::domain::{ContainerPort, GitProvider, Host, ServiceName};
use liftoff_core::manifest::{Deployable, Expose, RepoPath};
use liftoff_core::snippet::{CiSnippet, SnippetTarget};

/// The API site every snippet calls.
const API_HOST: &str = "liftoff.example.com";

fn target(provider: GitProvider, forge_host: &str) -> SnippetTarget {
    SnippetTarget {
        provider,
        forge_host: Host::new(forge_host).unwrap(),
        api_host: Host::new(API_HOST).unwrap(),
    }
}

/// `[service] compose = "deploy/compose.yaml"`, `expose = "web:3000"`.
fn compose() -> Deployable {
    Deployable::Service {
        compose: RepoPath::new("deploy/compose.yaml").unwrap(),
        expose: Some(Expose {
            service: ServiceName::new("web").unwrap(),
            port: ContainerPort::new(3000).unwrap(),
        }),
    }
}

/// The documented `[function]` manifest.
fn function() -> Deployable {
    Deployable::Function {
        build: "bun install && bun run build".to_owned(),
        component: RepoPath::new("dist/fn.wasm").unwrap(),
    }
}

/// Both renders for `target`, each under its `==> <path> (<kind>)` line.
fn rendered(target: &SnippetTarget) -> String {
    [("compose", compose()), ("function", function())]
        .iter()
        .map(|(kind, deploy)| {
            let snippet = CiSnippet::render(target, deploy);
            format!(
                "==> {} ({kind})\n{}",
                snippet.path.as_str(),
                snippet.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compares `value` with `tests/snapshots/<name>.snap`.
fn assert_matches_snapshot(name: &str, value: &str) {
    insta::with_settings!({ prepend_module_to_snapshot => false, omit_expression => true }, {
        insta::assert_snapshot!(name, value);
    });
}

mod render {
    use super::*;

    // TC: SNIP-01
    #[test]
    fn writes_forgejo_actions_workflows_when_provider_is_forgejo() {
        assert_matches_snapshot(
            "snip01_forgejo",
            &rendered(&target(GitProvider::Forgejo, "git.example.com")),
        );
    }

    // TC: SNIP-02
    #[test]
    fn writes_github_actions_workflows_when_provider_is_github() {
        assert_matches_snapshot(
            "snip02_github",
            &rendered(&target(GitProvider::Github, "github.com")),
        );
    }

    // TC: SNIP-03
    #[test]
    fn writes_gitlab_ci_files_when_provider_is_gitlab() {
        assert_matches_snapshot(
            "snip03_gitlab",
            &rendered(&target(GitProvider::Gitlab, "gitlab.com")),
        );
    }

    // TC: SNIP-01, SNIP-02, SNIP-03 (a snapshot ignores the final newline;
    // every file of a snippet ends with one)
    #[test]
    fn ends_every_workflow_with_newline_when_rendered() {
        let unterminated: Vec<(GitProvider, &str)> = [
            (GitProvider::Forgejo, "git.example.com"),
            (GitProvider::Github, "github.com"),
            (GitProvider::Gitlab, "gitlab.com"),
        ]
        .into_iter()
        .flat_map(|(provider, host)| {
            [("compose", compose()), ("function", function())]
                .into_iter()
                .filter(move |(_, deploy)| {
                    !CiSnippet::render(&target(provider, host), deploy)
                        .content
                        .ends_with('\n')
                })
                .map(move |(kind, _)| (provider, kind))
        })
        .collect();
        assert_eq!(unterminated, Vec::new());
    }
}
