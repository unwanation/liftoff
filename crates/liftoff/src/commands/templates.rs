//! `templates [--group G]`:
//! the template catalogue `create` offers, by group, with each
//! template's name and description. With a server it is `GET
//! /git/templates?group=&repos=` (`repos`: the CLI's `[local]
//! templates`, TPL-06); without one the core's `TemplateCatalog` over the
//! local connections, answered in the same wire type.

use std::sync::Arc;

use liftoff_api::TemplateCatalogView;
use liftoff_core::domain::{TemplateGroup, TemplateSource};
use liftoff_core::service::TemplateCatalog;

use super::{Ctx, parse, raw};
use crate::client::Client;
use crate::error::CliError;
use crate::local::{LocalMode, view};
use crate::{Sleeper, render};

/// Runs `templates`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    group: Option<String>,
) -> Result<(), CliError> {
    ctx.finish()?;
    let repos = local_templates(ctx);
    if ctx.is_local() {
        let local = ctx.local_mode().await?;
        let catalogue = local_catalogue(&local, group.as_deref(), &repos).await?;
        return ctx.say_json_or(&catalogue, render::catalogue);
    }
    let client = ctx.client()?;
    let (catalogue, body) = server_catalogue(&client, group.as_deref(), &repos).await?;
    let text = if ctx.json() {
        raw(&body)
    } else {
        render::catalogue(&catalogue)
    };
    ctx.say(&text)
}

/// `[local] templates` of the CLI config.
pub(super) fn local_templates<S: Sleeper>(ctx: &Ctx<'_, S>) -> Vec<String> {
    ctx.config
        .as_ref()
        .map(crate::config::Config::local_templates)
        .unwrap_or_default()
}

/// The server's catalogue (`GET /git/templates`), with `repos` added;
/// also the body as received, for `--json`.
pub(super) async fn server_catalogue(
    client: &Client,
    group: Option<&str>,
    repos: &[String],
) -> Result<(TemplateCatalogView, String), CliError> {
    let mut query = Vec::new();
    if let Some(group) = group {
        query.push(("group", group.to_owned()));
    }
    if !repos.is_empty() {
        query.push(("repos", repos.join(",")));
    }
    let reply = client.get("/git/templates", &query).await?;
    Ok((parse(&reply.body)?, reply.body))
}

/// The catalogue of local mode: `repos` read through the local
/// connections by the core's `TemplateCatalog`.
pub(super) async fn local_catalogue(
    local: &LocalMode,
    group: Option<&str>,
    repos: &[String],
) -> Result<TemplateCatalogView, CliError> {
    let group = group
        .map(TemplateGroup::parse)
        .transpose()
        .map_err(|error| CliError::Usage(format!("--group: {error}")))?;
    let repos = repos
        .iter()
        .map(|repo| TemplateSource::new(repo))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| CliError::Config(format!("[local] templates: {error}")))?;
    let catalog = TemplateCatalog::new(Arc::clone(&local.ports), Arc::clone(&local.settings))
        .list(&local.actor, &repos, group)
        .await?;
    Ok(view::catalog(&catalog))
}
