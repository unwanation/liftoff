//! `tenant use <slug>`: the tenant
//! `create` names without `--tenant`, written into the CLI config for the
//! current server, as `server use` writes the default server. It is the
//! CLI's own command: a module's command does not write the CLI config.
//! The other `tenant …` words are commands of the `tenants` module.

use liftoff_core::domain::TenantSlug;

use super::Ctx;
use crate::Sleeper;
use crate::error::CliError;

/// The words of a module's command below `tenant` (`create` → `tenant
/// create`).
pub(super) fn below_tenant(words: Vec<String>) -> Vec<String> {
    core::iter::once("tenant".to_owned()).chain(words).collect()
}

/// Runs `tenant use`: `[servers.<current>] tenant = "<slug>"`. Nothing is
/// sent: whether the tenant exists, and whether the caller belongs to
/// it, is the server's answer to the next `create`.
pub(super) fn use_tenant<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    slug: Option<String>,
) -> Result<(), CliError> {
    let slug = ctx.text(slug, "<slug>", "Tenant")?;
    ctx.finish()?;
    let slug =
        TenantSlug::new(&slug).map_err(|error| CliError::Usage(format!("<slug>: {error}")))?;
    let chosen = ctx.chosen.clone().ok_or(CliError::NotLoggedIn)?;
    let mut config = ctx.config.clone().unwrap_or_default();
    let server = config.servers.get_mut(&chosen.name).ok_or_else(|| {
        CliError::Config(format!(
            "no server named {:?} in the config; see `liftoff server ls`",
            chosen.name
        ))
    })?;
    server.tenant = Some(slug.to_string());
    ctx.save_config(&config)?;
    ctx.say(&format!(
        "`lof create` on {} names tenant {slug} now\n",
        chosen.name
    ))
}
