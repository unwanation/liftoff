//! `module ls|install|remove|enable|config`.
//!
//! `install` asks `GET /modules/available` for the release (the one of
//! `--version`, else the newest the server can run) and `GET /modules`
//! for what is installed, so a missing dependency is refused before
//! anything is sent; it takes `--set k=v` and `--set
//! k=@file` (the file's content without its final newline), each value
//! typed by the release's `config_schema`, asks on a terminal for every
//! required field still missing (its `description` as the question; a
//! `"format": "secret"` field without echo) and lists them as `--set
//! <field>` in batch mode (exit 64); then `POST /modules`. `remove` is
//! `DELETE /modules/{name}` (`?purge=true` with `--purge`), `enable` and
//! `config` are `PATCH /modules/{name}`; `config` merges the fields it
//! sets into the stored configuration (`k=` with nothing after `=` is a
//! empty string; the server removes a field set to `null`).

use liftoff_api::{AvailableModuleView, ModuleInstall, ModulePatch, ModuleView};
use liftoff_core::domain::{ModuleName, ModuleVersion};
use liftoff_core::service::{InstallRequest, ModuleChange};
use reqwest::Method;
use serde_json::{Map, Value};

use super::{Ctx, parse, segment};
use crate::Sleeper;
use crate::args::ModuleCommand;
use crate::client::Client;
use crate::error::CliError;
use crate::local::view;
use crate::mode::Mode;
use crate::render;

/// Runs `module …`.
pub(super) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: ModuleCommand,
) -> Result<(), CliError> {
    if ctx.is_local() {
        return local(ctx, command).await;
    }
    match command {
        ModuleCommand::Ls { available } => ls(ctx, available).await,
        ModuleCommand::Install { name, version, set } => install(ctx, name, version, &set).await,
        ModuleCommand::Remove { name, purge } => {
            let name = ctx.text(name, "<name>", "Module")?;
            ctx.finish()?;
            let query = if purge {
                vec![("purge", "true".to_owned())]
            } else {
                Vec::new()
            };
            let reply = ctx
                .client()?
                .delete(&format!("/modules/{}", segment(&name)), &query)
                .await?;
            let kept = if purge { "" } else { "; its tables stay" };
            ctx.ack(&reply, &format!("removed {name}{kept}"))
        }
        ModuleCommand::Enable { name } => {
            let name = ctx.text(name, "<name>", "Module")?;
            ctx.finish()?;
            let body = ModulePatch {
                enabled: Some(true),
                config: None,
            };
            patch(ctx, &name, &body, &format!("enabled {name}")).await
        }
        ModuleCommand::Config { name, set } => config(ctx, name, &set).await,
    }
}

/// `module ls [--available]`.
async fn ls<S: Sleeper>(ctx: &mut Ctx<'_, S>, available: bool) -> Result<(), CliError> {
    ctx.finish()?;
    let client = ctx.client()?;
    if available {
        let reply = client.get("/modules/available", &[]).await?;
        return ctx.show(&reply, |body| {
            Ok(render::available_modules(
                &parse::<Vec<AvailableModuleView>>(body)?,
            ))
        });
    }
    let reply = client.get("/modules", &[]).await?;
    ctx.show(&reply, |body| {
        Ok(render::modules(&parse::<Vec<ModuleView>>(body)?))
    })
}

/// `module install <name> [--version v] [--set k=v]...`.
async fn install<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    name: Option<String>,
    version: Option<String>,
    set: &[String],
) -> Result<(), CliError> {
    let name = ctx.text(name, "<name>", "Module")?;
    if name.is_empty() {
        return ctx.finish();
    }
    let client = ctx.client()?;
    let available: Vec<AvailableModuleView> =
        parse(&client.get("/modules/available", &[]).await?.body)?;
    let installed: Vec<ModuleView> = parse(&client.get("/modules", &[]).await?.body)?;
    let release = available
        .iter()
        .find(|module| module.name == name)
        .and_then(|module| {
            module.releases.iter().find(|release| {
                version
                    .as_ref()
                    .is_none_or(|wanted| &release.version == wanted)
            })
        });
    let schema = release.and_then(|release| release.config_schema.clone());
    if let Some(release) = release
        && let Some(missing) = release
            .requires
            .iter()
            .find(|dependency| !installed.iter().any(|module| &module.name == *dependency))
    {
        return Err(CliError::ModuleRequired(missing.clone()));
    }
    let mut config = given_config(ctx, set, schema.as_ref())?;
    ask_required(ctx, schema.as_ref(), &mut config)?;
    ctx.finish()?;
    let body = ModuleInstall {
        name: name.clone(),
        version,
        config,
    };
    let reply = client
        .send_json(Method::POST, "/modules", &[], &body)
        .await?;
    ctx.show(&reply, |body| {
        let module: ModuleView = parse(body)?;
        Ok(render::installed(&module))
    })
}

/// `module config <name> [--set k=v]...`: on a terminal without `--set`,
/// every field of the schema is asked (an empty answer keeps it).
async fn config<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    name: Option<String>,
    set: &[String],
) -> Result<(), CliError> {
    let name = ctx.text(name, "<name>", "Module")?;
    if name.is_empty() {
        return ctx.finish();
    }
    let client = ctx.client()?;
    let installed: Vec<ModuleView> = parse(&client.get("/modules", &[]).await?.body)?;
    let schema = installed
        .into_iter()
        .find(|module| module.name == name)
        .ok_or_else(|| CliError::Usage(format!("module {name} is not installed")))?
        .config_schema;
    let mut config = given_config(ctx, set, schema.as_ref())?;
    if config.is_empty() {
        if ctx.mode == Mode::Batch {
            let _empty = ctx.text(None, "--set <field>=<value>", "")?;
        } else {
            for (field, property) in properties(schema.as_ref()) {
                let question = format!("{} (empty keeps it)", question(&field, property));
                let answer = if is_secret(property) {
                    ctx.env.prompter.password(&question)?
                } else {
                    ctx.env.prompter.text(&question)?.trim().to_owned()
                };
                if !answer.is_empty() {
                    let _previous = config.insert(field.clone(), typed(&field, &answer, property)?);
                }
            }
        }
    }
    ctx.finish()?;
    let body = ModulePatch {
        enabled: None,
        config: Some(config),
    };
    patch(ctx, &name, &body, &format!("configured {name}")).await
}

/// `PATCH /modules/{name}` with `body`, acknowledged with `message`.
async fn patch<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    name: &str,
    body: &ModulePatch,
    message: &str,
) -> Result<(), CliError> {
    let client: Client = ctx.client()?;
    let reply = client
        .send_json(
            Method::PATCH,
            &format!("/modules/{}", segment(name)),
            &[],
            body,
        )
        .await?;
    ctx.ack(&reply, message)
}

/// The fields of `--set k=v` and `--set k=@file` (a path relative to the
/// working directory; its content without one final newline), each typed
/// by `schema`.
fn given_config<S: Sleeper>(
    ctx: &Ctx<'_, S>,
    set: &[String],
    schema: Option<&Value>,
) -> Result<Map<String, Value>, CliError> {
    let mut config = Map::new();
    for pair in set {
        let (field, raw) = pair
            .split_once('=')
            .filter(|(field, _)| !field.is_empty())
            .ok_or_else(|| {
                CliError::Usage(format!("--set expects k=v or k=@file, got {pair:?}"))
            })?;
        let text = match raw.strip_prefix('@') {
            Some(file) => {
                let path = ctx.env.cwd.join(file);
                let content = std::fs::read_to_string(&path).map_err(|error| {
                    CliError::Local(format!("cannot read {}: {error}", path.display()))
                })?;
                let content = content.strip_suffix('\n').unwrap_or(&content);
                content.strip_suffix('\r').unwrap_or(content).to_owned()
            }
            None => raw.to_owned(),
        };
        let property = property(schema, field);
        let _previous = config.insert(field.to_owned(), typed(field, &text, property)?);
    }
    Ok(config)
}

/// Asks for every field `schema` requires that `config` lacks, or lists
/// it as `--set <field>` in batch mode.
fn ask_required<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    schema: Option<&Value>,
    config: &mut Map<String, Value>,
) -> Result<(), CliError> {
    let required: Vec<String> = schema
        .and_then(|schema| schema.get("required"))
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    for field in required {
        if config.contains_key(&field) {
            continue;
        }
        let property = property(schema, &field);
        let label = format!("--set {field}");
        let question = question(&field, property);
        let answer = if is_secret(property) {
            ctx.secret(None, &label, &question)?
        } else {
            ctx.text(None, &label, &question)?
        };
        if ctx.mode == Mode::Interactive {
            let _none = config.insert(field.clone(), typed(&field, &answer, property)?);
        }
    }
    Ok(())
}

/// The top-level properties of `schema`, in order.
fn properties(schema: Option<&Value>) -> Vec<(String, Option<&Value>)> {
    schema
        .and_then(|schema| schema.get("properties"))
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(field, property)| (field.clone(), Some(property)))
                .collect()
        })
        .unwrap_or_default()
}

/// The schema of the top-level `field`.
fn property<'a>(schema: Option<&'a Value>, field: &str) -> Option<&'a Value> {
    let properties = schema?.get("properties")?;
    properties.get(field)
}

/// What a field is asked with: its `description`, else its name.
fn question(field: &str, property: Option<&Value>) -> String {
    property
        .and_then(|property| property.get("description"))
        .and_then(Value::as_str)
        .map_or_else(|| field.to_owned(), str::to_owned)
}

/// Whether the field is `"format": "secret"`.
fn is_secret(property: Option<&Value>) -> bool {
    property
        .and_then(|property| property.get("format"))
        .and_then(Value::as_str)
        == Some("secret")
}

/// `text` as the JSON its schema `type` asks for: an integer, a number or
/// a boolean parsed; an array or an object parsed as JSON (an array of
/// strings also as `a,b`, each part trimmed); anything else a string.
fn typed(field: &str, text: &str, property: Option<&Value>) -> Result<Value, CliError> {
    let kind = property
        .and_then(|property| property.get("type"))
        .and_then(Value::as_str);
    let refused = || {
        CliError::Usage(format!(
            "--set {field}: not a valid {}: {text:?}",
            kind.unwrap_or("value")
        ))
    };
    match kind {
        Some("integer") => text
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_unparsed| refused()),
        Some("number") => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(refused),
        Some("boolean") => text
            .parse::<bool>()
            .map(Value::from)
            .map_err(|_unparsed| refused()),
        Some("object") => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(refused),
        Some("array") => match serde_json::from_str::<Value>(text) {
            Ok(array @ Value::Array(_)) => Ok(array),
            _ if strings_only(property) => Ok(Value::Array(
                text.split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(|part| Value::String(part.to_owned()))
                    .collect(),
            )),
            _ => Err(refused()),
        },
        _ => Ok(Value::String(text.to_owned())),
    }
}

/// Whether the array field's `items` are `"type": "string"`.
fn strings_only(property: Option<&Value>) -> bool {
    property
        .and_then(|property| property.get("items"))
        .and_then(|items| items.get("type"))
        .and_then(Value::as_str)
        == Some("string")
}

// ---- local mode -----------------------------------------------------------------

/// A module name of the command line.
fn module_name(raw: &str) -> Result<ModuleName, CliError> {
    ModuleName::new(raw).map_err(|error| CliError::Usage(format!("{raw}: {error}")))
}

/// `module …` without a server (LCL-01): the core's
/// `ModuleService` over the local ports, so a local install checks the
/// registry's index, signature, hash and WIT version as the daemon's
/// does, and keeps the component in `~/.config/liftoff/modules/`.
async fn local<S: Sleeper>(ctx: &mut Ctx<'_, S>, command: ModuleCommand) -> Result<(), CliError> {
    match command {
        ModuleCommand::Ls { available } => {
            ctx.finish()?;
            let local = ctx.local_mode().await?;
            if available {
                let modules = local.modules().available(&local.actor).await?;
                let views: Vec<AvailableModuleView> =
                    modules.iter().map(view::available_module).collect();
                return ctx.say_json_or(&views, |views| render::available_modules(views));
            }
            let records = local.modules().list(&local.actor).await?;
            let views: Vec<ModuleView> = records.iter().map(view::module).collect();
            ctx.say_json_or(&views, |views| render::modules(views))
        }
        ModuleCommand::Install { name, version, set } => {
            local_install(ctx, name, version, &set).await
        }
        ModuleCommand::Remove { name, purge } => {
            let name = ctx.text(name, "<name>", "Module")?;
            ctx.finish()?;
            let module = module_name(&name)?;
            let local = ctx.local_mode().await?;
            local.modules().remove(&local.actor, &module, purge).await?;
            let kept = if purge { "" } else { "; its tables stay" };
            ctx.say_json_or(&serde_json::json!({"name": name}), |_| {
                format!("removed {name}{kept}\n")
            })
        }
        ModuleCommand::Enable { name } => {
            let name = ctx.text(name, "<name>", "Module")?;
            ctx.finish()?;
            let module = module_name(&name)?;
            let local = ctx.local_mode().await?;
            let change = ModuleChange {
                enabled: Some(true),
                config: None,
            };
            let record = local
                .modules()
                .update(&local.actor, &module, change)
                .await?;
            ctx.say_json_or(&view::module(&record), |_| format!("enabled {name}\n"))
        }
        ModuleCommand::Config { name, set } => local_config(ctx, name, &set).await,
    }
}

/// `module install <name>` without a server: the release's schema types
/// `--set` and names what is missing, as with a server.
async fn local_install<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    name: Option<String>,
    version: Option<String>,
    set: &[String],
) -> Result<(), CliError> {
    let name = ctx.text(name, "<name>", "Module")?;
    if name.is_empty() {
        return ctx.finish();
    }
    let module = module_name(&name)?;
    let version = version
        .as_deref()
        .map(ModuleVersion::new)
        .transpose()
        .map_err(|error| CliError::Usage(format!("--version: {error}")))?;
    let local = ctx.local_mode().await?;
    let modules = local.modules();
    let available = modules.available(&local.actor).await?;
    let schema = available
        .iter()
        .find(|candidate| candidate.name == module)
        .and_then(|candidate| {
            candidate.releases.iter().find(|release| {
                version
                    .as_ref()
                    .is_none_or(|wanted| &release.version == wanted)
            })
        })
        .and_then(|release| release.config_schema.clone());
    let mut config = given_config(ctx, set, schema.as_ref())?;
    ask_required(ctx, schema.as_ref(), &mut config)?;
    ctx.finish()?;
    let record = modules
        .install(
            &local.actor,
            InstallRequest {
                name: module,
                version,
                config,
            },
        )
        .await?;
    ctx.say_json_or(&view::module(&record), render::installed)
}

/// `module config <name> --set k=v…` without a server.
async fn local_config<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    name: Option<String>,
    set: &[String],
) -> Result<(), CliError> {
    let name = ctx.text(name, "<name>", "Module")?;
    if name.is_empty() {
        return ctx.finish();
    }
    let module = module_name(&name)?;
    let local = ctx.local_mode().await?;
    let schema = local
        .modules()
        .list(&local.actor)
        .await?
        .into_iter()
        .find(|record| record.installed.name == module)
        .ok_or_else(|| CliError::Usage(format!("module {name} is not installed")))?
        .manifest
        .and_then(|manifest| manifest.config_schema);
    let config = given_config(ctx, set, schema.as_ref())?;
    if config.is_empty() {
        let _empty = ctx.text(None, "--set <field>=<value>", "")?;
    }
    ctx.finish()?;
    let change = ModuleChange {
        enabled: None,
        config: Some(config),
    };
    let record = local
        .modules()
        .update(&local.actor, &module, change)
        .await?;
    ctx.say_json_or(&view::module(&record), |_| format!("configured {name}\n"))
}
