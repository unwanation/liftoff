//! [`CiSnippet`]: the CI workflow that deploys a project to liftoff, one
//! per forge provider and per kind of project.
//!
//! | Provider | File | CI |
//! |---|---|---|
//! | Forgejo | `.forgejo/workflows/deploy.yml` | Forgejo Actions |
//! | GitHub | `.github/workflows/deploy.yml` | GitHub Actions |
//! | GitLab | `.gitlab-ci.yml` | GitLab CI/CD |
//!
//! A compose project (`[service]`) builds its image with `docker buildx`
//! (the Forgejo runner needs a Docker daemon: `crane`, the plan's other
//! choice, pushes images but cannot build one from a Dockerfile; a
//! template for a daemonless runner ships its own workflow),
//! pushes it to the forge's registry as `sha-<commit>` and calls `POST
//! /deploys` with the deploy token from the CI secret
//! [`DEPLOY_TOKEN_KEY`] and the compose file of the manifest;
//! on Actions the image name is the repository in lowercase, which
//! registries require. A function (`[function]`) runs the
//! manifest's `build` and uploads its `component` to the same endpoint
//! (the daemon accepts components). Every value
//! the CI run supplies (repository, ref, commit, secrets) reaches the
//! shell through the environment, never through an interpolated
//! expression; so does a repository file whose path is not a plain word
//! (`[A-Za-z0-9._/+-]`).
//!
//! The renderer is pure: the operation that serves it
//! ([`DeployService::snippet`](crate::service::DeployService::snippet))
//! picks the provider from the module serving the project's forge host
//! and the kind from its manifest; `lof create` commits the file when the
//! template has none.

use crate::domain::{GitProvider, Host};
use crate::manifest::{Deployable, RepoPath};
use crate::service::DEPLOY_TOKEN_KEY;

/// The CI secret holding the forge account a Forgejo workflow logs in to
/// the registry as (Forgejo's automatic token cannot push packages).
pub const REGISTRY_USER_KEY: &str = "REGISTRY_USER";
/// The CI secret holding that account's token with package write access.
pub const REGISTRY_TOKEN_KEY: &str = "REGISTRY_TOKEN";

/// Where a snippet deploys from and to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetTarget {
    /// The forge of the repository: chooses the CI dialect and the file.
    pub provider: GitProvider,
    /// The forge host of the repository (`git.example.com`): the registry of
    /// a Forgejo image.
    pub forge_host: Host,
    /// The API site of the installation (`liftoff.example.com`): the snippet
    /// calls `https://<api_host>/api/v1/deploys`.
    pub api_host: Host,
}

/// A rendered CI workflow and the file of the repository it belongs at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiSnippet {
    /// The file, relative to the repository root
    /// (`.forgejo/workflows/deploy.yml`).
    pub path: RepoPath,
    /// The workflow text, ending in a newline.
    pub content: String,
}

impl CiSnippet {
    /// The workflow that deploys `deploy` from a repository on `target`'s
    /// forge (`CiSnippet::for(provider, kind)`).
    #[must_use]
    pub fn render(target: &SnippetTarget, deploy: &Deployable) -> Self {
        let (path, content) = match target.provider {
            GitProvider::Forgejo => (FORGEJO_PATH, actions(target, deploy)),
            GitProvider::Github => (GITHUB_PATH, actions(target, deploy)),
            GitProvider::Gitlab => (GITLAB_PATH, gitlab(target, deploy)),
        };
        Self {
            path: RepoPath::constant(path),
            content,
        }
    }
}

/// The workflow file of Forgejo Actions.
const FORGEJO_PATH: &str = ".forgejo/workflows/deploy.yml";
/// The workflow file of GitHub Actions.
const GITHUB_PATH: &str = ".github/workflows/deploy.yml";
/// The pipeline file of GitLab CI/CD.
const GITLAB_PATH: &str = ".gitlab-ci.yml";
/// The registry of a GitHub image.
const GITHUB_REGISTRY: &str = "ghcr.io";
/// The environment variable a repository file the shell cannot take as a
/// plain word reaches `curl` through.
const FILE_VAR: &str = "LIFTOFF_FILE";

/// The `curl` call of a CI deploy: the API site, the deploy token from
/// the environment, then the form fields; `indent` is that of the line.
fn deploy_call(target: &SnippetTarget, fields: &[String], indent: &str) -> String {
    let mut call = format!(
        "{indent}curl -fsS https://{}/api/v1/deploys \\\n{indent}  -H \"Authorization: Bearer ${DEPLOY_TOKEN_KEY}\" \\\n",
        target.api_host.as_str()
    );
    let lines: Vec<String> = fields
        .chunks(2)
        .map(|pair| format!("{indent}  {}", pair.join(" ")))
        .collect();
    call.push_str(&lines.join(" \\\n"));
    call.push('\n');
    call
}

/// The form fields after the token: the repository and ref of the run,
/// then the image tag and compose file or the component.
fn fields(
    repo_var: &str,
    ref_var: &str,
    sha_var: &str,
    file: &FileArg,
    deploy: &Deployable,
) -> Vec<String> {
    let mut fields = vec![
        format!("-F \"repo=${repo_var}\""),
        format!("-F \"ref=${ref_var}\""),
    ];
    match deploy {
        Deployable::Service { .. } => {
            fields.push(format!("-F \"tag=sha-${sha_var}\""));
            fields.push(format!("-F \"compose=@{}\"", file.word));
        }
        Deployable::Function { .. } => {
            fields.push(format!("-F \"component=@{}\"", file.word));
        }
    }
    fields
}

/// The repository file a deploy uploads, as the shell sees it.
struct FileArg {
    /// What goes inside the double quotes of the `-F` argument.
    word: String,
    /// The YAML double-quoted value of [`FILE_VAR`] when the path is not a
    /// plain word.
    env: Option<String>,
}

impl FileArg {
    /// The compose file or component of `deploy`.
    fn of(deploy: &Deployable) -> Self {
        let path = match deploy {
            Deployable::Service { compose, .. } => compose,
            Deployable::Function { component, .. } => component,
        };
        let raw = path.as_str();
        let plain = raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-' | '+'));
        if plain {
            Self {
                word: raw.to_owned(),
                env: None,
            }
        } else {
            Self {
                word: format!("${FILE_VAR}"),
                env: Some(yaml_quoted(raw)),
            }
        }
    }
}

/// `text` as a YAML double-quoted scalar.
fn yaml_quoted(text: &str) -> String {
    let mut quoted = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            // Every control character is below U+0100.
            c if c.is_control() => {
                let code = u32::from(c);
                quoted.push_str("\\x");
                quoted.extend(
                    [code >> 4, code & 0xf].map(|digit| char::from_digit(digit, 16).unwrap_or('0')),
                );
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// The build command of a function as the lines of a YAML block scalar
/// indented by `indent` (leading blanks of the command dropped so the
/// block's indentation is that of its first line).
fn block(command: &str, indent: &str) -> String {
    command
        .trim_start()
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                "\n".to_owned()
            } else {
                format!("{indent}{line}\n")
            }
        })
        .collect()
}

/// A Forgejo Actions or GitHub Actions workflow; both read the GitHub
/// dialect and set the `GITHUB_*` variables.
fn actions(target: &SnippetTarget, deploy: &Deployable) -> String {
    let github = target.provider == GitProvider::Github;
    let file = FileArg::of(deploy);
    let mut out: Vec<String> = Vec::new();
    let secrets = match (deploy, github) {
        // A Forgejo runner may run jobs without a Docker socket; the
        // build needs one (crane pushes images but builds none).
        (Deployable::Service { .. }, false) => format!(
            "{DEPLOY_TOKEN_KEY}, {REGISTRY_USER_KEY}, {REGISTRY_TOKEN_KEY}.\n# The runner needs a Docker daemon for docker buildx; for one without,\n# commit your own workflow (`lof create` keeps a template's)."
        ),
        (Deployable::Service { .. }, true) => format!("{DEPLOY_TOKEN_KEY}."),
        (Deployable::Function { .. }, _) => {
            format!("{DEPLOY_TOKEN_KEY}. The runner needs the toolchain of the build command.")
        }
    };
    out.push(format!(
        "# Deploys to liftoff on every push. CI secrets from `lof secret sync`:\n# {secrets}\n"
    ));
    out.push("name: deploy\non: [push]\n".to_owned());
    if github {
        out.push("permissions:\n  contents: read\n".to_owned());
        if matches!(deploy, Deployable::Service { .. }) {
            out.push("  packages: write\n".to_owned());
        }
    }
    out.push("jobs:\n  deploy:\n".to_owned());
    out.push(
        if github {
            "    runs-on: ubuntu-latest\n"
        } else {
            "    runs-on: docker\n"
        }
        .to_owned(),
    );
    out.push("    steps:\n      - uses: actions/checkout@v4\n".to_owned());
    match deploy {
        Deployable::Service { .. } => {
            let (registry, username, password) = if github {
                (
                    GITHUB_REGISTRY.to_owned(),
                    "${{ github.actor }}".to_owned(),
                    "${{ secrets.GITHUB_TOKEN }}".to_owned(),
                )
            } else {
                (
                    target.forge_host.as_str().to_owned(),
                    format!("${{{{ secrets.{REGISTRY_USER_KEY} }}}}"),
                    format!("${{{{ secrets.{REGISTRY_TOKEN_KEY} }}}}"),
                )
            };
            // Registries take lowercase names only; the repository of the
            // run may have capitals.
            out.push(format!(
                "      - name: image name\n        run: echo \"IMAGE={registry}/$(echo \"$GITHUB_REPOSITORY\" | tr '[:upper:]' '[:lower:]')\" >> \"$GITHUB_ENV\"\n"
            ));
            out.push("      - uses: docker/setup-buildx-action@v3\n".to_owned());
            out.push(format!(
                "      - uses: docker/login-action@v3\n        with:\n          registry: {registry}\n          username: {username}\n          password: {password}\n"
            ));
            out.push("      - uses: docker/build-push-action@v6\n        with:\n          context: .\n          push: true\n          tags: ${{ env.IMAGE }}:sha-${{ github.sha }}\n".to_owned());
        }
        Deployable::Function { build, .. } => {
            out.push("      - name: build the component\n        run: |\n".to_owned());
            out.push(block(build, "          "));
        }
    }
    out.push(format!(
        "      - name: deploy\n        env:\n          {DEPLOY_TOKEN_KEY}: ${{{{ secrets.{DEPLOY_TOKEN_KEY} }}}}\n"
    ));
    if let Some(value) = &file.env {
        out.push(format!("          {FILE_VAR}: {value}\n"));
    }
    out.push("        run: |\n".to_owned());
    out.push(deploy_call(
        target,
        &fields(
            "GITHUB_REPOSITORY",
            "GITHUB_REF_NAME",
            "GITHUB_SHA",
            &file,
            deploy,
        ),
        "          ",
    ));
    out.concat()
}

/// A GitLab CI/CD pipeline of one job.
fn gitlab(target: &SnippetTarget, deploy: &Deployable) -> String {
    let file = FileArg::of(deploy);
    let mut out: Vec<String> = Vec::new();
    out.push(format!(
        "# Deploys to liftoff on every push. CI/CD variables from `lof secret sync`:\n# {DEPLOY_TOKEN_KEY}."
    ));
    match deploy {
        Deployable::Service { .. } => out.push("\n".to_owned()),
        Deployable::Function { .. } => out.push(
            " The job's image needs curl and the toolchain of the build\n# command.\n".to_owned(),
        ),
    }
    out.push("deploy:\n".to_owned());
    let mut variables = Vec::new();
    if matches!(deploy, Deployable::Service { .. }) {
        out.push("  image: docker:27\n  services: [docker:27-dind]\n".to_owned());
        variables.push("    DOCKER_TLS_CERTDIR: \"/certs\"\n".to_owned());
    }
    if let Some(value) = &file.env {
        // `expand: false`: GitLab would expand a `$` of the path.
        variables.push(format!(
            "    {FILE_VAR}:\n      value: {value}\n      expand: false\n"
        ));
    }
    if !variables.is_empty() {
        out.push("  variables:\n".to_owned());
        out.extend(variables);
    }
    out.push("  script:\n".to_owned());
    match deploy {
        Deployable::Service { .. } => {
            out.push(concat!(
                "    - echo \"$CI_REGISTRY_PASSWORD\" | docker login \"$CI_REGISTRY\" -u \"$CI_REGISTRY_USER\" --password-stdin\n",
                "    - docker buildx build --push -t \"$CI_REGISTRY_IMAGE:sha-$CI_COMMIT_SHA\" .\n",
                "    - apk add --no-cache curl\n",
            )
            .to_owned());
        }
        Deployable::Function { build, .. } => {
            out.push("    - |\n".to_owned());
            out.push(block(build, "      "));
        }
    }
    out.push("    - |\n".to_owned());
    out.push(deploy_call(
        target,
        &fields(
            "CI_PROJECT_PATH",
            "CI_COMMIT_REF_NAME",
            "CI_COMMIT_SHA",
            &file,
            deploy,
        ),
        "      ",
    ));
    out.concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forgejo() -> SnippetTarget {
        SnippetTarget {
            provider: GitProvider::Forgejo,
            forge_host: Host::new("git.example.com").unwrap(),
            api_host: Host::new("liftoff.example.com").unwrap(),
        }
    }

    fn gitlab_target() -> SnippetTarget {
        SnippetTarget {
            provider: GitProvider::Gitlab,
            ..forgejo()
        }
    }

    fn compose(path: &str) -> Deployable {
        Deployable::Service {
            compose: RepoPath::new(path).unwrap(),
            expose: None,
        }
    }

    mod file_arg {
        use super::*;

        // TC: SNIP-01 (a path the shell would read is passed through the
        // environment, quoted for YAML)
        #[test]
        fn passes_path_through_environment_when_path_is_not_plain() {
            let content =
                CiSnippet::render(&forgejo(), &compose("deploy/$(id) \"x\".yaml")).content;
            assert_eq!(
                (
                    content.contains("          LIFTOFF_FILE: \"deploy/$(id) \\\"x\\\".yaml\"\n"),
                    content.contains("-F \"compose=@$LIFTOFF_FILE\""),
                    content.contains("$(id)\"x"),
                ),
                (true, true, false),
                "{content}"
            );
        }

        // TC: SNIP-03 (GitLab expands `$` in a variable unless told not to)
        #[test]
        fn keeps_gitlab_variable_unexpanded_when_path_is_not_plain() {
            let content =
                CiSnippet::render(&gitlab_target(), &compose("deploy/$HOME.yaml")).content;
            assert!(
                content.contains(
                    "    LIFTOFF_FILE:\n      value: \"deploy/$HOME.yaml\"\n      expand: false\n"
                ),
                "{content}"
            );
        }

        // TC: SNIP-01
        #[test]
        fn escapes_control_characters_when_quoting_for_yaml() {
            assert_eq!(yaml_quoted("a\nb\u{1b}\\"), "\"a\\nb\\x1b\\\\\"");
        }
    }

    mod block {
        use super::*;

        // TC: SNIP-01 (every line of a multi-line build stays inside the
        // block scalar)
        #[test]
        fn indents_every_line_when_build_has_several() {
            assert_eq!(
                block("  make\n\ncp a b\r\n", "    "),
                "    make\n\n    cp a b\n"
            );
        }
    }
}
