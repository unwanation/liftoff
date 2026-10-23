//! The commands of installed modules: `lof <module> <command> [--<arg> V]...`, or the words of
//! the command alone, as a native one (`lof tenant members add`, `lof
//! protect`, and below the built-in `admin`: `lof admin tenant set`).
//!
//! A word clap does not know as a command reaches [`run`]. It reads `GET
//! /modules`, finds the enabled module of that name and its command (or,
//! when the word is no module's name, the enabled module's command whose
//! words begin the given ones, the longest) and builds its clap command
//! on the fly from what the manifest declares: each
//! argument is a `--<name>` flag (`integer` parsed as one, `boolean` a
//! switch, `project` filled when not given from the global `--project` or
//! the working tree's repository, as the server resolves it), plus
//! `--json` and `--no-input`, which mean what they mean before the
//! module's name. The same mode rules as every built-in command
//! apply: a required argument that is not given is asked on a terminal
//! with the manifest's `hint` (else its name) and listed (exit 64) in
//! batch mode, before anything is sent (REG-08). The command runs as `POST
//! /api/v1/m/<module>/<command words joined by />` with the arguments as
//! a JSON object and prints the answer as received.
//!
//! Without a session, or when the server cannot be reached, the word is
//! still an unrecognized subcommand (exit 64), with why the commands of
//! modules could not be asked for.
//!
//! Because the tree comes from the server at each run, a module installed
//! a moment ago has its commands at once (REG-06).

use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, value_parser};
use liftoff_api::{ArgView, CommandView, ModuleView};
use reqwest::Method;
use serde_json::{Map, Value};

use crate::Sleeper;
use crate::commands::{Ctx, ProjectRef, parse, raw, resolve, segment};
use crate::error::CliError;
use crate::mode::Mode;
use crate::project::Repository;

/// Runs `words`: the module's name, its command and their arguments, or
/// the words of a module's command alone (`tenant members add`, `protect`: commands of modules look like native ones), also
/// below a built-in word (`admin tenant set`).
pub(crate) async fn run<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    words: Vec<String>,
) -> Result<(), CliError> {
    let Some(word) = words.first().cloned() else {
        return Err(CliError::Usage("no command given".to_owned()));
    };
    // Without a session or a server the word is still a usage error
    // (exit 64), not a failed login (1) or an unreachable server (69).
    let unknown = |error: CliError| match error {
        CliError::NotLoggedIn | CliError::Unauthorized { .. } | CliError::Network(_) => {
            CliError::Usage(format!(
                "unrecognized subcommand '{word}'; it is no command of liftoff, and the commands of modules cannot be asked for: {error}"
            ))
        }
        other => other,
    };
    let client = ctx.client().map_err(unknown)?;
    let reply = client.get("/modules", &[]).await.map_err(unknown)?;
    let modules: Vec<ModuleView> = parse(&reply.body)?;
    let modules: Vec<ModuleView> = modules
        .into_iter()
        .filter(|module| module.enabled)
        .collect();
    let (module, command, rest) = if let Some(module) =
        modules.iter().find(|module| module.name == word)
    {
        let rest = words.get(1..).unwrap_or_default();
        match matching(module, rest) {
            Some((command, used)) => (module, command, rest.get(used..).unwrap_or_default()),
            None => return module_help(ctx, module, rest),
        }
    } else {
        let found = modules
            .iter()
            .filter_map(|module| {
                matching(module, &words).map(|(command, used)| (module, command, used))
            })
            .max_by_key(|(_, _, used)| *used);
        let Some((module, command, used)) = found else {
            return Err(CliError::Usage(format!(
                "unrecognized subcommand '{word}'; it is no command of liftoff and no enabled module of this server"
            )));
        };
        (module, command, words.get(used..).unwrap_or_default())
    };
    let argv = core::iter::once(command.name.clone()).chain(rest.iter().cloned());
    let matches = match subcommand(command)
        .bin_name(format!("liftoff {}", command.name))
        .try_get_matches_from(argv)
    {
        Ok(matches) => matches,
        Err(error) => return clap_error(ctx, &error),
    };
    // `--json` before the command is the run's own: the run prints a
    // refused request's problem then. Given only after the command, it
    // is printed here.
    let json_here = matches.get_flag("json") && !ctx.json();
    if matches.get_flag("json") {
        ctx.set_json();
    }
    if matches.get_flag("no-input") {
        ctx.mode = Mode::Batch;
    }
    let (mut body, from_repo) = arguments(ctx, command, &matches)?;
    ctx.finish()?;
    for (name, repo) in from_repo {
        let project = resolve(&client, ProjectRef::Repo(repo)).await?;
        let _none = body.insert(name, Value::String(project));
    }
    let path = format!("/m/{}/{}", segment(&module.name), route_of(&command.name));
    match client
        .send_json(Method::POST, &path, &[], &Value::Object(body))
        .await
    {
        Ok(reply) => ctx.say(&raw(&reply.body)),
        Err(error) => {
            if json_here && let Some(problem) = error.body() {
                ctx.say(&raw(problem))?;
            }
            Err(error)
        }
    }
}

/// The command of `module` whose words begin `words`, the longest one,
/// and how many of `words` it took.
fn matching<'m>(module: &'m ModuleView, words: &[String]) -> Option<(&'m CommandView, usize)> {
    module
        .commands
        .iter()
        .filter_map(|command| {
            let name: Vec<&str> = command.name.split(' ').collect();
            let given = words.get(..name.len())?;
            given
                .iter()
                .zip(&name)
                .all(|(given, word)| given == word)
                .then_some((command, name.len()))
        })
        .max_by_key(|(_, used)| *used)
}

/// The route of the command `name`: its words joined by `/`, each one
/// percent-encoded.
fn route_of(name: &str) -> String {
    name.split(' ').map(segment).collect::<Vec<_>>().join("/")
}

/// `lof <module>` without one of its commands: clap's answer over the
/// module's tree (its help, or a usage error).
fn module_help<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    module: &ModuleView,
    rest: &[String],
) -> Result<(), CliError> {
    let argv = core::iter::once(module.name.clone()).chain(rest.iter().cloned());
    match tree(module).try_get_matches_from(argv) {
        Ok(_) => Err(CliError::Usage(format!(
            "{}: no command given",
            module.name
        ))),
        Err(error) => clap_error(ctx, &error),
    }
}

/// The clap tree of `module`: one subcommand per command it declares.
fn tree(module: &ModuleView) -> clap::Command {
    let mut root = clap::Command::new(module.name.clone())
        .bin_name(format!("liftoff {}", module.name))
        .about(format!(
            "Commands of the module {} {}",
            module.name, module.version
        ))
        .subcommand_required(true)
        .arg_required_else_help(true);
    for command in &module.commands {
        root = root.subcommand(subcommand(command).name(command.name.replace(' ', "-")));
    }
    root
}

/// The clap command of one module command. Every argument is optional to
/// clap: what is missing is asked or listed, as for built-in commands.
fn subcommand(command: &CommandView) -> clap::Command {
    let mut built = clap::Command::new(command.name.clone())
        .about(command.description.clone())
        .arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .help("Print the answer as received"),
        )
        .arg(
            Arg::new("no-input")
                .long("no-input")
                .action(ArgAction::SetTrue)
                .help("Never ask: a missing argument ends the run with exit code 64"),
        );
    for arg in &command.args {
        let flag = Arg::new(arg.name.clone()).long(arg.name.clone());
        let flag = match arg.kind.as_str() {
            "boolean" => flag.action(ArgAction::SetTrue),
            "integer" => flag
                .value_name("n")
                .value_parser(value_parser!(i64))
                .action(ArgAction::Set),
            _ => flag.value_name("v").action(ArgAction::Set),
        };
        built = built.arg(match &arg.hint {
            Some(hint) => flag.help(hint.clone()),
            None => flag,
        });
    }
    built
}

/// Arguments of type `project` left to the server to resolve: the
/// argument's name and the working tree's repository.
type FromRepo = Vec<(String, Repository)>;

/// The JSON arguments of `command`: given, else asked or listed when
/// required; and the arguments of type `project` that were not given and
/// are the working tree's repository, which the server resolves.
///
/// An argument of type `project` not given is the global `--project`,
/// else the repository of the working tree (in a project's directory the project is its `git remote`), else asked or listed.
fn arguments<S: Sleeper>(
    ctx: &mut Ctx<'_, S>,
    command: &CommandView,
    matches: &ArgMatches,
) -> Result<(Map<String, Value>, FromRepo), CliError> {
    let mut body = Map::new();
    let mut from_repo = Vec::new();
    for arg in &command.args {
        let value = match arg.kind.as_str() {
            "boolean" => Some(Value::Bool(matches.get_flag(&arg.name))),
            "integer" => match matches.get_one::<i64>(&arg.name) {
                Some(number) => Some(Value::from(*number)),
                None => asked(ctx, arg)?,
            },
            "project" => match matches.get_one::<String>(&arg.name) {
                Some(text) => Some(Value::String(text.clone())),
                None => match (ctx.project_flag(), ctx.repository()) {
                    (Some(name), _) => Some(Value::String(name)),
                    (None, Some(repo)) => {
                        from_repo.push((arg.name.clone(), repo));
                        None
                    }
                    (None, None) => asked(ctx, arg)?,
                },
            },
            _ => match matches.get_one::<String>(&arg.name) {
                Some(text) => Some(Value::String(text.clone())),
                None => asked(ctx, arg)?,
            },
        };
        if let Some(value) = value {
            let _none = body.insert(arg.name.clone(), value);
        }
    }
    Ok((body, from_repo))
}

/// A required argument that was not given: asked on a terminal with its
/// hint, or listed in batch mode. Nothing for an optional one.
fn asked<S: Sleeper>(ctx: &mut Ctx<'_, S>, arg: &ArgView) -> Result<Option<Value>, CliError> {
    if !arg.required {
        return Ok(None);
    }
    let label = format!("--{}", arg.name);
    let question = arg.hint.clone().unwrap_or_else(|| arg.name.clone());
    let text = ctx.text(None, &label, &question)?;
    if ctx.mode == Mode::Batch {
        return Ok(None);
    }
    if arg.kind == "integer" {
        return text
            .parse::<i64>()
            .map(|number| Some(Value::from(number)))
            .map_err(|_unparsed| CliError::Usage(format!("{label}: not an integer: {text:?}")));
    }
    Ok(Some(Value::String(text)))
}

/// Help goes to stdout and ends the run well; a real mistake is a usage
/// error (exit 64) with clap's text.
fn clap_error<S: Sleeper>(ctx: &mut Ctx<'_, S>, error: &clap::Error) -> Result<(), CliError> {
    let text = error.render().to_string();
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        return ctx.say(&text);
    }
    Err(CliError::Usage(
        text.trim_start_matches("error: ").trim_end().to_owned(),
    ))
}
