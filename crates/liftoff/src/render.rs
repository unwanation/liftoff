//! Human-readable output: tables (`comfy-table`) and key/value lists.

use std::fmt::Write as _;

use comfy_table::{CellAlignment, ColumnConstraint, ContentArrangement, Table, presets};
use liftoff_api::{
    AvailableModuleView, BindingView, ConnectionView, DeployView, DomainView, EnvironmentView,
    ModuleView, SecretRevisionView, SecretView, SecretsSynced, TemplateCatalogView, UserView,
};

/// Gap between columns.
const GAP: &str = "  ";

/// [`GAP`] as a padding width.
const GAP_WIDTH: u16 = 2;

/// Aligns `rows` under `headers` without borders. Every line ends in a
/// newline and carries no trailing blanks.
pub(crate) fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut table = Table::new();
    table
        .load_style(presets::NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(headers.iter().copied());
    for row in rows {
        table.add_row(row.iter().map(String::as_str));
    }
    let last = headers.len().saturating_sub(1);
    for (index, column) in table.column_iter_mut().enumerate() {
        column.set_padding((0, if index < last { GAP_WIDTH } else { 0 }));
        column.set_cell_alignment(CellAlignment::Left);
        column.set_constraint(ColumnConstraint::ContentWidth);
    }
    let mut text = String::new();
    for line in table.lines() {
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

/// Aligned `key  value` lines.
pub(crate) fn pairs(rows: &[(&str, String)]) -> String {
    let width = rows.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(key, value)| {
            let pad = " ".repeat(width.saturating_sub(key.len()));
            [*key, pad.as_str(), GAP, value.as_str(), "\n"].concat()
        })
        .collect()
}

fn or_dash(value: Option<&String>) -> String {
    value.map_or_else(|| "-".to_owned(), Clone::clone)
}

fn yes_no(value: bool) -> String {
    (if value { "yes" } else { "no" }).to_owned()
}

/// `whoami`.
pub(crate) fn user(user: &UserView) -> String {
    pairs(&[
        ("login", user.login.clone()),
        ("email", user.email.clone()),
        ("role", user.role.clone()),
    ])
}

/// `admin users`.
pub(crate) fn users(users: &[UserView]) -> String {
    let rows: Vec<Vec<String>> = users
        .iter()
        .map(|user| {
            vec![
                user.login.clone(),
                user.email.clone(),
                user.role.clone(),
                user.source.clone(),
            ]
        })
        .collect();
    table(&["LOGIN", "EMAIL", "ROLE", "SOURCE"], &rows)
}

/// `status`, `env stop|start`.
pub(crate) fn environment(env: &EnvironmentView) -> String {
    pairs(&environment_rows(env))
}

/// `status`: the environment, then a `tunnel <env> <state>` line per
/// tunnel environment of `listed`.
pub(crate) fn status(env: &EnvironmentView, listed: &[EnvironmentView]) -> String {
    let mut rows = environment_rows(env).to_vec();
    rows.extend(
        listed
            .iter()
            .filter(|listed| listed.kind == "tunnel")
            .map(|tunnel| ("tunnel", format!("{} {}", tunnel.name, tunnel.state))),
    );
    pairs(&rows)
}

fn environment_rows(env: &EnvironmentView) -> [(&'static str, String); 9] {
    [
        ("project", env.project.clone()),
        ("env", env.name.clone()),
        ("state", env.state.clone()),
        ("kind", env.kind.clone()),
        ("branch", or_dash(env.branch.as_ref())),
        ("cpus", env.cpus.to_string()),
        ("memory", format!("{} MB", env.memory_mb)),
        ("deploy", or_dash(env.current_deploy_id.as_ref())),
        ("visibility", env.visibility.clone()),
    ]
}

/// `env ls`.
pub(crate) fn environments(envs: &[EnvironmentView]) -> String {
    let rows: Vec<Vec<String>> = envs
        .iter()
        .map(|env| {
            vec![
                env.name.clone(),
                env.state.clone(),
                env.kind.clone(),
                or_dash(env.branch.as_ref()),
                env.visibility.clone(),
            ]
        })
        .collect();
    table(&["NAME", "STATE", "KIND", "BRANCH", "VISIBILITY"], &rows)
}

/// `domain ls`.
pub(crate) fn domains(domains: &[DomainView]) -> String {
    let rows: Vec<Vec<String>> = domains
        .iter()
        .map(|domain| {
            vec![
                domain.host.clone(),
                domain.project.clone(),
                domain.kind.clone(),
                or_dash(domain.verified_at.as_ref()),
                yes_no(domain.proxied),
            ]
        })
        .collect();
    table(&["HOST", "PROJECT", "KIND", "VERIFIED", "PROXIED"], &rows)
}

/// `domain add`.
pub(crate) fn domain(domain: &DomainView) -> String {
    let mut text = format!("added {} ({})\n", domain.host, domain.kind);
    if domain.kind == "external" && domain.verified_at.is_none() {
        text.push_str("point a CNAME or A record at this server to verify it\n");
    }
    text
}

/// `bind`.
pub(crate) fn binding(binding: &BindingView) -> String {
    // The port-less binding of a function environment.
    if binding.service == "function" && binding.container_port == 80 {
        return format!(
            "bound {} to the function of {}/{}\n",
            binding.host, binding.project, binding.environment
        );
    }
    format!(
        "bound {} to {}:{} of {}/{}\n",
        binding.host, binding.service, binding.container_port, binding.project, binding.environment
    )
}

/// `deploys`.
pub(crate) fn deploys(deploys: &[DeployView]) -> String {
    let rows: Vec<Vec<String>> = deploys
        .iter()
        .map(|deploy| {
            vec![
                deploy.number.to_string(),
                deploy.status.clone(),
                deploy.tag.clone(),
                or_dash(deploy.git_ref.as_ref()),
                deploy.source.clone(),
                or_dash(deploy.started_at.as_ref()),
            ]
        })
        .collect();
    table(&["NUM", "STATUS", "TAG", "REF", "SOURCE", "STARTED"], &rows)
}

/// `deploys show`.
pub(crate) fn deploy(deploy: &DeployView) -> String {
    let mut rows = vec![
        ("deploy", format!("{} (#{})", deploy.id, deploy.number)),
        ("env", format!("{}/{}", deploy.project, deploy.environment)),
        ("status", deploy.status.clone()),
        ("tag", deploy.tag.clone()),
        ("ref", or_dash(deploy.git_ref.as_ref())),
        ("source", deploy.source.clone()),
        ("started", or_dash(deploy.started_at.as_ref())),
        ("finished", or_dash(deploy.finished_at.as_ref())),
    ];
    if let Some(error) = &deploy.error {
        rows.push(("error", error.clone()));
    }
    if let Some(number) = deploy.rolled_back_to {
        rows.push(("rolled back to", format!("#{number}")));
    }
    pairs(&rows)
}

/// `secret ls`.
pub(crate) fn secrets(secrets: &[SecretView]) -> String {
    let rows: Vec<Vec<String>> = secrets
        .iter()
        .map(|secret| {
            vec![
                secret.key.clone(),
                secret.scope.clone(),
                secret.version.to_string(),
                yes_no(secret.ci),
                secret.updated_at.clone(),
            ]
        })
        .collect();
    table(&["KEY", "SCOPE", "VERSION", "CI", "UPDATED"], &rows)
}

/// `secret history`.
pub(crate) fn revisions(revisions: &[SecretRevisionView]) -> String {
    let rows: Vec<Vec<String>> = revisions
        .iter()
        .map(|revision| {
            vec![
                revision.version.to_string(),
                revision.updated_by.clone(),
                revision.updated_at.clone(),
            ]
        })
        .collect();
    table(&["VERSION", "BY", "AT"], &rows)
}

/// `git ls`.
pub(crate) fn connections(connections: &[ConnectionView]) -> String {
    let rows: Vec<Vec<String>> = connections
        .iter()
        .map(|connection| {
            vec![
                connection.host.clone(),
                connection.provider.clone(),
                connection.account_login.clone(),
                connection.kind.clone(),
                or_dash(connection.expires_at.as_ref()),
            ]
        })
        .collect();
    table(&["HOST", "PROVIDER", "ACCOUNT", "KIND", "EXPIRES"], &rows)
}

/// What `secret sync` did: one line per key, sent, unchanged, then
/// failed with the reason (each as printable text).
pub(crate) fn synced(answer: &SecretsSynced, project: &str) -> String {
    if answer.synced.is_empty() && answer.unchanged.is_empty() && answer.failed.is_empty() {
        return format!(
            "no CI secrets in {} (`lof secret set K=V --scope project --ci`)\n",
            printable(project)
        );
    }
    let synced = answer
        .synced
        .iter()
        .map(|key| format!("synced {}\n", printable(key)));
    let unchanged = answer
        .unchanged
        .iter()
        .map(|key| format!("unchanged {}\n", printable(key)));
    let failed = answer.failed.iter().map(|failure| {
        format!(
            "failed {}: {}\n",
            printable(&failure.key),
            printable(&failure.error)
        )
    });
    synced.chain(unchanged).chain(failed).collect()
}

/// `secret pull`: one `.env` line. A value a `.env` reader would split or
/// misread is double-quoted with `\n`, `"` and `\` escaped.
pub(crate) fn dotenv_line(key: &str, value: &str) -> String {
    let plain = !value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '#'));
    if plain {
        return format!("{key}={value}\n");
    }
    let mut quoted = String::with_capacity(value.len().saturating_add(2));
    for c in value.chars() {
        match c {
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            other => quoted.push(other),
        }
    }
    format!("{key}=\"{quoted}\"\n")
}

/// The commands of `module` as they are typed: `lof <module> <command>`.
fn module_commands(module: &ModuleView) -> Vec<String> {
    module
        .commands
        .iter()
        .map(|command| format!("lof {} {}", module.name, command.name))
        .collect()
}

/// `module ls`.
pub(crate) fn modules(modules: &[ModuleView]) -> String {
    if modules.is_empty() {
        return "no modules installed\n".to_owned();
    }
    let rows: Vec<Vec<String>> = modules
        .iter()
        .map(|module| {
            let commands: Vec<String> = module
                .commands
                .iter()
                .map(|command| command.name.clone())
                .collect();
            vec![
                module.name.clone(),
                module.version.clone(),
                yes_no(module.enabled),
                if commands.is_empty() {
                    "-".to_owned()
                } else {
                    commands.join(", ")
                },
                or_dash(module.error.as_ref()),
            ]
        })
        .collect();
    table(&["NAME", "VERSION", "ENABLED", "COMMANDS", "ERROR"], &rows)
}

/// `module ls --available`.
pub(crate) fn available_modules(modules: &[AvailableModuleView]) -> String {
    if modules.is_empty() {
        return "the registry has no module this server can run\n".to_owned();
    }
    let rows: Vec<Vec<String>> = modules
        .iter()
        .filter_map(|module| {
            let newest = module.releases.first()?;
            Some(vec![
                module.name.clone(),
                newest.version.clone(),
                if newest.requires.is_empty() {
                    "-".to_owned()
                } else {
                    newest.requires.join(", ")
                },
            ])
        })
        .collect();
    table(&["NAME", "VERSION", "REQUIRES"], &rows)
}

/// `module install`: what was installed and the commands it adds.
pub(crate) fn installed(module: &ModuleView) -> String {
    let mut text = format!("installed {} {}\n", module.name, module.version);
    for command in module_commands(module) {
        text.push_str("  ");
        text.push_str(&command);
        text.push('\n');
    }
    text
}

/// `text` as the terminal may show it: text that came from elsewhere (a
/// template repository's `liftoff.toml`, a server) with each control
/// character replaced by U+FFFD, so that no escape sequence reaches the
/// terminal.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// The template catalogue (`lof templates`): one row per template, by
/// group, then the repositories skipped and why.
pub(crate) fn catalogue(catalogue: &TemplateCatalogView) -> String {
    let rows: Vec<Vec<String>> = catalogue
        .groups
        .iter()
        .flat_map(|group| {
            group.templates.iter().map(|template| {
                vec![
                    printable(&group.group),
                    printable(&template.name),
                    printable(&template.description),
                    printable(&format!("{}/{}", template.host, template.repo)),
                ]
            })
        })
        .collect();
    let mut text = if rows.is_empty() {
        "no templates: list template repositories in `[local] templates` of the CLI config, or in `templates.repos` of the server\n".to_owned()
    } else {
        table(&["GROUP", "TEMPLATE", "DESCRIPTION", "REPOSITORY"], &rows)
    };
    for warning in &catalogue.warnings {
        let _ok = writeln!(
            text,
            "skipped {}: {}",
            printable(&warning.repo),
            printable(&warning.reason)
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    mod table {
        use super::*;

        #[test]
        fn aligns_columns_without_trailing_blanks_when_rendered() {
            let rows = vec![
                vec!["west".to_owned(), "https://a".to_owned()],
                vec!["staging-long".to_owned(), "https://b".to_owned()],
            ];
            assert_eq!(
                table(&["NAME", "URL"], &rows),
                "NAME          URL\nwest          https://a\nstaging-long  https://b\n"
            );
        }
    }

    mod dotenv_line {
        use super::*;

        // TC: CLIX-07
        #[test]
        fn leaves_a_plain_value_bare_when_nothing_needs_quoting() {
            assert_eq!(
                dotenv_line("A", "postgres://db:5432/x"),
                "A=postgres://db:5432/x\n"
            );
        }

        // TC: CLIX-07
        #[test]
        fn leaves_an_empty_value_bare_when_written() {
            assert_eq!(dotenv_line("A", ""), "A=\n");
        }

        // TC: CLIX-07 (`#` would start a comment)
        #[test]
        fn quotes_a_hash_when_the_value_has_one() {
            assert_eq!(dotenv_line("A", "a#b"), "A=\"a#b\"\n");
        }
    }
}
