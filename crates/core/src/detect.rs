//! [`StackDetector`]: the `liftoff.toml` proposed for a repository that
//! has none.
//!
//! The detector is pure: it reads the repository through [`SourceTree`]
//! and never touches the network. [`DirTree`] is the `std::fs` tree the
//! CLI hands it for the working directory.
//!
//! Rules: `package.json` gives the package manager by its
//! lockfile (`bun.lock`/`bun.lockb` → bun, `pnpm-lock.yaml` → pnpm,
//! `yarn.lock` → yarn, `package-lock.json` or none → npm), the `dev`
//! (else `start`) script as `[dev] command`, and the framework by its
//! dependencies with its default port (Vite 5173, Next 3000, Astro 4321,
//! `SvelteKit` 5173, Hono/Bun 3000); `Cargo.toml` gives Axum/Actix 8080;
//! `pyproject.toml` or `requirements.txt` give uv, poetry or python and
//! FastAPI/uvicorn 8000; `go.mod` gives 8080; `deploy/compose.yaml` or
//! `compose.yaml` with one service carrying `expose` gives `[service]`
//! with that `service:port`; `wit/` or a `componentize-js`/
//! `componentize-py` dependency gives `[function]`. A stack whose
//! framework is recognised wins over one whose framework is not; several
//! recognised stacks become [`Question::Stack`]. What the files do not
//! settle becomes a [`Question`] the CLI asks.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::compose::ComposeFile;
use crate::domain::{ContainerPort, ServiceName, TemplateGroup};
use crate::manifest::{Deployable, DevCommand, Expose, ProjectManifest, RepoPath};

/// A read-only view of a repository's files, by path relative to its root
/// (`/`-separated, `deploy/compose.yaml`).
pub trait SourceTree {
    /// The text of the file at `path`; `None` when there is no such file
    /// or it is not UTF-8.
    fn read(&self, path: &str) -> Option<String>;

    /// Whether `path` is a file, whatever its content (`bun.lockb` is
    /// binary).
    fn exists(&self, path: &str) -> bool;

    /// Whether `path` is a directory.
    fn is_dir(&self, path: &str) -> bool;
}

/// A [`SourceTree`] over a directory on disk (`std::fs`).
#[derive(Debug, Clone)]
pub struct DirTree {
    /// The repository root.
    root: PathBuf,
}

impl DirTree {
    /// The tree rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SourceTree for DirTree {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }

    fn exists(&self, path: &str) -> bool {
        self.root.join(path).is_file()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.root.join(path).is_dir()
    }
}

/// What [`StackDetector::detect`] made of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// The proposed `[service]` (its `expose` carries the framework's
    /// port) or `[function]` (its `build` command); `None` when the files
    /// do not tell.
    pub deploy: Option<Deployable>,
    /// The proposed `[dev] command`.
    pub dev: Option<DevCommand>,
    /// The template group by default.
    pub group: Option<TemplateGroup>,
    /// What the CLI must ask before the manifest is complete, in the order
    /// it asks; empty when the files settle everything.
    pub questions: Vec<Question>,
}

impl Detected {
    /// Applies the answer to [`Question::Stack`]: the proposed `[service]`
    /// takes `alternative`'s `expose`, and `[dev]` its command.
    pub fn choose_stack(&mut self, alternative: &Alternative) {
        if let Some(Deployable::Service { expose, .. }) = &mut self.deploy {
            expose.clone_from(&alternative.expose);
        }
        self.dev.clone_from(&alternative.dev);
    }

    /// Applies the answer to [`Question::Expose`]: the service and port
    /// that get the domain. Without a proposed `[service]` the project is
    /// a compose project (the answer to [`Question::Kind`]: a function is
    /// recognised by its files) of the compose file `tree` holds, else of
    /// the one the detection proposes.
    pub fn answer_expose(&mut self, tree: &impl SourceTree, expose: Option<Expose>) {
        let compose = match &self.deploy {
            Some(Deployable::Service { compose, .. }) => compose.clone(),
            _ => RepoPath::constant(
                COMPOSE_FILES
                    .iter()
                    .chain(&COMPOSE_DEFAULT_NAMES)
                    .find(|path| tree.exists(path))
                    .copied()
                    .unwrap_or(DEFAULT_COMPOSE),
            ),
        };
        self.deploy = Some(Deployable::Service { compose, expose });
    }

    /// Applies the answer to [`Question::Build`]: the command that builds
    /// the proposed component, else the documented one.
    pub fn answer_build(&mut self, build: String) {
        let component = match &self.deploy {
            Some(Deployable::Function { component, .. }) => component.clone(),
            _ => RepoPath::constant(FUNCTION_COMPONENT),
        };
        self.deploy = Some(Deployable::Function { build, component });
    }

    /// Whether `question` is still open after the answers applied so far:
    /// a compose project builds no component, and a function exposes no
    /// service.
    #[must_use]
    pub const fn is_open(&self, question: &Question) -> bool {
        match question {
            Question::Build => !matches!(self.deploy, Some(Deployable::Service { .. })),
            Question::Expose { .. } => !matches!(self.deploy, Some(Deployable::Function { .. })),
            Question::Stack { .. } | Question::Kind | Question::DevCommand | Question::Group => {
                true
            }
        }
    }

    /// The proposed manifest: `deploy` and `dev`, without `[template]`,
    /// `[secrets]` or `[substitute]`; `None` while `deploy` is unknown.
    #[must_use]
    pub fn manifest(&self) -> Option<ProjectManifest> {
        Some(ProjectManifest {
            template: None,
            deploy: self.deploy.clone()?,
            dev: self.dev.clone(),
            secrets: BTreeMap::new(),
            substitute: Vec::new(),
        })
    }
}

/// A question the files left open; the CLI asks it.
///
/// `Stack` comes first: its answer replaces the proposed `expose`, `dev`
/// and group. `Expose` applies when the answer to `Kind` is a service, `Build` when it
/// is a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// Several language stacks are recognised: which one is the project?
    /// The first alternative is what [`Detected`] proposes.
    Stack {
        /// One per recognised stack, in the order `package.json`,
        /// `Cargo.toml`, Python, `go.mod`.
        alternatives: Vec<Alternative>,
    },
    /// `[service]` or `[function]`?
    Kind,
    /// Which `service:port` gets the domain, out of `candidates` (empty:
    /// the CLI asks for one).
    Expose {
        /// What the compose file offers.
        candidates: Vec<Expose>,
    },
    /// Which command builds the component (`[function] build`)?
    Build,
    /// Which command runs the project locally (`[dev] command`)?
    DevCommand,
    /// Which template group?
    Group,
}

/// What one recognised language stack proposes, for [`Question::Stack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternative {
    /// The file that gave the stack (`package.json`, `Cargo.toml`,
    /// `pyproject.toml`, `requirements.txt`, `go.mod`).
    pub source: &'static str,
    /// The `service:port` for the domain (a compose file's wins); `None`
    /// when the compose file leaves it to [`Question::Expose`].
    pub expose: Option<Expose>,
    /// The `[dev] command`; `None` when it is asked.
    pub dev: Option<DevCommand>,
    /// The template group by default.
    pub group: TemplateGroup,
}

/// Proposes a manifest from a repository's files.
#[derive(Debug, Clone, Copy, Default)]
pub struct StackDetector;

impl StackDetector {
    /// Detects the stack of the repository in `tree`.
    ///
    /// A function (`wit/`, `componentize-js`, `componentize-py`) wins
    /// over a service; a compose file's `expose` wins over the
    /// framework's port; the language stack gives `[dev] command` and the
    /// group (a compose file alone gives `service`). A stack with a
    /// recognised framework wins over one without; several recognised
    /// stacks are offered as [`Question::Stack`], the first proposed.
    #[must_use]
    pub fn detect(tree: &impl SourceTree) -> Detected {
        let node = tree
            .read("package.json")
            .and_then(|text| Node::read(tree, &text));
        let python = Python::read(tree);
        let rust = tree.read("Cargo.toml").map(|text| Rust::read(&text));
        let go = tree.exists("go.mod");

        if let Some(detected) = Self::function(tree, node.as_ref(), python.as_ref()) {
            return detected;
        }

        let stacks: Vec<Stack> = [
            node.as_ref().map(Node::stack),
            rust.as_ref().map(Rust::stack),
            python.as_ref().map(Python::stack),
            go.then(go_stack),
        ]
        .into_iter()
        .flatten()
        .collect();
        let stack = stacks
            .iter()
            .find(|stack| stack.group.is_some())
            .or_else(|| stacks.first())
            .cloned();
        let compose = COMPOSE_FILES
            .iter()
            .find_map(|path| Some((*path, tree.read(path)?)));

        if stack.is_none() && compose.is_none() {
            return Self::nothing();
        }

        let mut questions = Vec::new();
        let compose_expose = compose.as_ref().map(|(_, text)| exposed(text));
        let alternatives: Vec<Alternative> = stacks
            .iter()
            .filter_map(|stack| {
                let expose = compose_expose
                    .as_ref()
                    .map_or_else(|| stack.expose(), |(only, _)| only.clone());
                stack.alternative(expose)
            })
            .collect();
        if alternatives.len() > 1 {
            questions.push(Question::Stack { alternatives });
        }
        let (compose_path, expose) =
            if let (Some((path, _)), Some((only, candidates))) = (&compose, compose_expose) {
                if only.is_none() {
                    questions.push(Question::Expose { candidates });
                }
                (*path, only)
            } else {
                let expose = stack.as_ref().and_then(Stack::expose);
                if expose.is_none() {
                    questions.push(Question::Expose {
                        candidates: Vec::new(),
                    });
                }
                (DEFAULT_COMPOSE, expose)
            };
        let dev = stack.as_ref().map_or(Dev::None, |stack| stack.dev.clone());
        let group = stack
            .as_ref()
            .map_or(Some(TemplateGroup::Service), |stack| stack.group);
        Self::finish(
            Some(Deployable::Service {
                compose: RepoPath::constant(compose_path),
                expose,
            }),
            dev,
            group,
            questions,
        )
    }

    /// The detection of a repository whose files settle nothing.
    fn nothing() -> Detected {
        Detected {
            deploy: None,
            dev: None,
            group: None,
            questions: vec![
                Question::Kind,
                Question::Expose {
                    candidates: Vec::new(),
                },
                Question::Build,
                Question::DevCommand,
                Question::Group,
            ],
        }
    }

    /// The `[function]` proposal, when the repository builds a component.
    fn function(
        tree: &impl SourceTree,
        node: Option<&Node>,
        python: Option<&Python>,
    ) -> Option<Detected> {
        let js = node.filter(|node| node.depends_on(COMPONENTIZE_JS));
        let py = python.filter(|python| python.depends_on(COMPONENTIZE_PY));
        let build = if let Some(node) = js {
            Some(node.componentize(tree))
        } else if let Some(python) = py {
            Some(python.componentize(tree))
        } else if tree.is_dir("wit") {
            None
        } else {
            return None;
        };
        let dev = js.map_or(Dev::None, |node| node.dev.clone());
        let mut questions = Vec::new();
        let deploy = build.flatten().map(|build| Deployable::Function {
            build,
            component: RepoPath::constant(FUNCTION_COMPONENT),
        });
        if deploy.is_none() {
            questions.push(Question::Build);
        }
        Some(Self::finish(
            deploy,
            dev,
            Some(TemplateGroup::Function),
            questions,
        ))
    }

    /// The detection, with the dev and group questions appended.
    fn finish(
        deploy: Option<Deployable>,
        dev: Dev,
        group: Option<TemplateGroup>,
        mut questions: Vec<Question>,
    ) -> Detected {
        let dev = match dev {
            Dev::Known(command) => Some(DevCommand { command }),
            Dev::Ask => {
                questions.push(Question::DevCommand);
                None
            }
            Dev::None => None,
        };
        if group.is_none() {
            questions.push(Question::Group);
        }
        Detected {
            deploy,
            dev,
            group,
            questions,
        }
    }
}

/// Where a compose file is looked for, in order.
const COMPOSE_FILES: [&str; 2] = ["deploy/compose.yaml", "compose.yaml"];
/// The other names `docker compose` reads by default, and the `.yml`
/// documented spelling: the compose file of an answered
/// [`Question::Expose`] when detection read none.
const COMPOSE_DEFAULT_NAMES: [&str; 4] = [
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
    "deploy/compose.yml",
];
/// The compose file proposed when the repository has none.
pub(crate) const DEFAULT_COMPOSE: &str = "deploy/compose.yaml";
/// The built component of a proposed `[function]`.
const FUNCTION_COMPONENT: &str = "dist/fn.wasm";
/// The manifest of a JavaScript package.
const PACKAGE_JSON: &str = "package.json";
/// The npm package that builds a JavaScript component.
const COMPONENTIZE_JS: &str = "@bytecodealliance/componentize-js";
/// The npm package whose `componentize` runs [`COMPONENTIZE_JS`] over a
/// WIT directory (`templates/fn-ts`).
const JCO: &str = "@bytecodealliance/jco";
/// The Python package that builds a Python component.
const COMPONENTIZE_PY: &str = "componentize-py";

/// The one `service:port` a compose file exposes, else no choice and the
/// candidates to ask from.
fn exposed(text: &str) -> (Option<Expose>, Vec<Expose>) {
    let mut candidates = candidates(text);
    if candidates.len() == 1 {
        (candidates.pop(), Vec::new())
    } else {
        (None, candidates)
    }
}

/// The services of a compose file that expose exactly one port, as
/// `service:port`; a service exposing several ports offers each.
fn candidates(text: &str) -> Vec<Expose> {
    ComposeFile::parse(text)
        .map(|file| {
            file.exposed()
                .into_iter()
                .flat_map(|(service, ports)| {
                    ports.into_iter().map(move |port| Expose {
                        service: service.clone(),
                        port,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What the language stack says.
#[derive(Debug, Clone)]
struct Stack {
    /// The file that gave the stack.
    source: &'static str,
    /// The service name and the framework's default port.
    port: Option<(&'static str, u16)>,
    /// The `[dev] command`.
    dev: Dev,
    /// The template group.
    group: Option<TemplateGroup>,
}

impl Stack {
    /// The stack for a framework of `group` listening on `port`.
    const fn framework(source: &'static str, group: TemplateGroup, port: u16, dev: Dev) -> Self {
        let service = match group {
            TemplateGroup::Web => "web",
            TemplateGroup::Api | TemplateGroup::Function | TemplateGroup::Service => "api",
        };
        Self {
            source,
            port: Some((service, port)),
            dev,
            group: Some(group),
        }
    }

    /// A stack whose framework is not known: port and group are asked.
    const fn unknown(source: &'static str, dev: Dev) -> Self {
        Self {
            source,
            port: None,
            dev,
            group: None,
        }
    }

    /// The alternative this stack offers when its framework is
    /// recognised, with `expose` for the domain.
    fn alternative(&self, expose: Option<Expose>) -> Option<Alternative> {
        Some(Alternative {
            source: self.source,
            expose,
            dev: match &self.dev {
                Dev::Known(command) => Some(DevCommand {
                    command: command.clone(),
                }),
                Dev::Ask | Dev::None => None,
            },
            group: self.group?,
        })
    }

    /// `service:port` for the domain.
    fn expose(&self) -> Option<Expose> {
        let (service, port) = self.port?;
        Some(Expose {
            service: ServiceName::new(service).ok()?,
            port: ContainerPort::new(port).ok()?,
        })
    }
}

/// What the files say about `[dev] command`.
#[derive(Debug, Clone)]
enum Dev {
    /// This command.
    Known(String),
    /// The stack runs locally, but the files do not say how: ask.
    Ask,
    /// The stack has no local command to propose (a compose file alone,
    /// a Python component): leave `[dev]` out.
    None,
}

/// The stack of a `go.mod`: an API on 8080.
fn go_stack() -> Stack {
    Stack::framework(
        "go.mod",
        TemplateGroup::Api,
        8080,
        Dev::Known("go run .".to_owned()),
    )
}

/// A JavaScript package manager, by lockfile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageManager {
    Bun,
    Pnpm,
    Yarn,
    Npm,
}

impl PackageManager {
    /// The manager whose lockfile is in `tree`; npm without one.
    fn detect(tree: &impl SourceTree) -> Self {
        let has = |path: &str| tree.exists(path);
        if has("bun.lock") || has("bun.lockb") {
            Self::Bun
        } else if has("pnpm-lock.yaml") {
            Self::Pnpm
        } else if has("yarn.lock") {
            Self::Yarn
        } else {
            Self::Npm
        }
    }

    /// The command that runs `script` of `package.json`.
    fn run(self, script: &str) -> String {
        match self {
            Self::Bun => format!("bun {script}"),
            Self::Pnpm => format!("pnpm {script}"),
            Self::Yarn => format!("yarn {script}"),
            Self::Npm if script == "start" => "npm start".to_owned(),
            Self::Npm => format!("npm run {script}"),
        }
    }

    /// The manager's command (`bun install`, `pnpm run build`).
    const fn name(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Npm => "npm",
        }
    }
}

/// A `package.json`.
#[derive(Debug, Clone)]
struct Node {
    manager: PackageManager,
    /// `dependencies` and `devDependencies`.
    dependencies: BTreeSet<String>,
    /// The `dev` (else `start`) script, as a command.
    dev: Dev,
    /// Whether `scripts` has `build`.
    has_build_script: bool,
}

impl Node {
    /// Reads `package.json` (`text`); `None` when it is not a JSON object.
    fn read(tree: &impl SourceTree, text: &str) -> Option<Self> {
        let json: serde_json::Value = serde_json::from_str(text).ok()?;
        let json = json.as_object()?;
        let manager = PackageManager::detect(tree);
        let dependencies = ["dependencies", "devDependencies"]
            .iter()
            .filter_map(|table| json.get(*table)?.as_object())
            .flat_map(|table| table.keys().cloned())
            .collect();
        let scripts = json.get("scripts").and_then(serde_json::Value::as_object);
        let dev = ["dev", "start"]
            .iter()
            .find(|script| scripts.is_some_and(|scripts| scripts.contains_key(**script)))
            .map_or(Dev::Ask, |script| Dev::Known(manager.run(script)));
        let has_build_script = scripts.is_some_and(|scripts| scripts.contains_key("build"));
        Some(Self {
            manager,
            dependencies,
            dev,
            has_build_script,
        })
    }

    fn depends_on(&self, package: &str) -> bool {
        self.dependencies.contains(package)
    }

    /// The framework by dependencies, with its default port. `SvelteKit`
    /// and Astro come before Vite, which they depend on; a Bun project
    /// without a framework is `Bun.serve` on 3000.
    fn stack(&self) -> Stack {
        let dev = self.dev.clone();
        let frameworks: [(&str, TemplateGroup, u16); 5] = [
            ("next", TemplateGroup::Web, 3000),
            ("astro", TemplateGroup::Web, 4321),
            ("@sveltejs/kit", TemplateGroup::Web, 5173),
            ("vite", TemplateGroup::Web, 5173),
            ("hono", TemplateGroup::Api, 3000),
        ];
        match frameworks
            .iter()
            .find(|(package, _, _)| self.depends_on(package))
        {
            Some((_, group, port)) => Stack::framework(PACKAGE_JSON, *group, *port, dev),
            None if self.manager == PackageManager::Bun => {
                Stack::framework(PACKAGE_JSON, TemplateGroup::Api, 3000, dev)
            }
            None => Stack::unknown(PACKAGE_JSON, dev),
        }
    }

    /// The `[function] build` of a JavaScript component: the package's own `build` script after an
    /// install; else, under bun, the pipeline of `templates/fn-ts` over
    /// `src/index.ts` (or `src/index.js`) and the world of
    /// `wit/world.wit`: `bun build` into one ESM file with `wasi:*`
    /// external, then `jco componentize` (the componentize-js CLI of
    /// 0.23.0 does not read the WIT directory). `None` when neither
    /// applies: without bun there is no bundler to propose.
    fn componentize(&self, tree: &impl SourceTree) -> Option<String> {
        if self.has_build_script {
            let manager = self.manager.name();
            return Some(format!("{manager} install && {manager} run build"));
        }
        if self.manager != PackageManager::Bun {
            return None;
        }
        let entry = ["src/index.ts", "src/index.js"]
            .into_iter()
            .find(|path| tree.exists(path))?;
        let world = tree
            .read("wit/world.wit")
            .and_then(|text| world_name(&text))
            .unwrap_or_else(|| "function".to_owned());
        Some(format!(
            "bun build {entry} --outfile build/index.js --target browser --format esm \
             --external 'wasi:*' && mkdir -p dist && bunx {JCO} componentize build/index.js \
             --wit wit --world-name {world} --out {FUNCTION_COMPONENT}"
        ))
    }
}

/// A Python project: `pyproject.toml` or `requirements.txt`.
#[derive(Debug, Clone)]
struct Python {
    /// `pyproject.toml`, else `requirements.txt`.
    source: &'static str,
    /// `uv`, `poetry` or `python`.
    runner: PythonRunner,
    /// Normalised distribution names.
    dependencies: BTreeSet<String>,
    /// The module holding the application (`main.py` → `main`).
    module: Option<&'static str>,
}

/// How Python commands run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PythonRunner {
    Uv,
    Poetry,
    Plain,
}

impl PythonRunner {
    /// `command` run in the project's environment.
    fn run(self, command: &str) -> String {
        match self {
            Self::Uv => format!("uv run {command}"),
            Self::Poetry => format!("poetry run {command}"),
            Self::Plain => command.to_owned(),
        }
    }
}

impl Python {
    /// Reads the project; `None` when it has neither file.
    fn read(tree: &impl SourceTree) -> Option<Self> {
        let pyproject = tree
            .read("pyproject.toml")
            .and_then(|text| text.parse::<toml::Table>().ok());
        let requirements = tree.read("requirements.txt");
        if pyproject.is_none() && requirements.is_none() {
            return None;
        }
        let poetry = pyproject
            .as_ref()
            .and_then(|table| table.get("tool")?.get("poetry"));
        let source = if pyproject.is_some() {
            "pyproject.toml"
        } else {
            "requirements.txt"
        };
        let runner = if tree.exists("uv.lock") {
            PythonRunner::Uv
        } else if tree.exists("poetry.lock") || poetry.is_some() {
            PythonRunner::Poetry
        } else {
            PythonRunner::Plain
        };
        let mut dependencies = BTreeSet::new();
        if let Some(listed) = pyproject
            .as_ref()
            .and_then(|table| table.get("project")?.get("dependencies")?.as_array())
        {
            dependencies.extend(
                listed
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(requirement_name),
            );
        }
        if let Some(table) = poetry
            .and_then(|poetry| poetry.get("dependencies"))
            .and_then(toml::Value::as_table)
        {
            dependencies.extend(table.keys().map(|name| requirement_name(name)));
        }
        if let Some(text) = &requirements {
            dependencies.extend(
                text.lines()
                    .map(|line| line.split('#').next().unwrap_or_default().trim())
                    .filter(|line| !line.is_empty() && !line.starts_with('-'))
                    .map(requirement_name),
            );
        }
        let module = ["main", "app"]
            .into_iter()
            .find(|module| tree.exists(&format!("{module}.py")));
        Some(Self {
            source,
            runner,
            dependencies,
            module,
        })
    }

    fn depends_on(&self, package: &str) -> bool {
        self.dependencies.contains(package)
    }

    /// `FastAPI` or uvicorn: an API on 8000 run by uvicorn.
    fn stack(&self) -> Stack {
        if self.depends_on("fastapi") || self.depends_on("uvicorn") {
            let dev = self.module.map_or(Dev::Ask, |module| {
                Dev::Known(self.runner.run(&format!("uvicorn {module}:app --reload")))
            });
            Stack::framework(self.source, TemplateGroup::Api, 8000, dev)
        } else {
            Stack::unknown(self.source, Dev::Ask)
        }
    }

    /// `componentize-py` over `wit/` and the application module; `None`
    /// without a module.
    fn componentize(&self, tree: &impl SourceTree) -> Option<String> {
        let module = self.module?;
        let world = tree
            .read("wit/world.wit")
            .and_then(|text| world_name(&text))
            .unwrap_or_else(|| "proxy".to_owned());
        Some(self.runner.run(&format!(
            "componentize-py -d wit -w {world} componentize {module} -o {FUNCTION_COMPONENT}"
        )))
    }
}

/// The distribution name of a requirement (`fastapi>=0.115` → `fastapi`),
/// lower-cased with `_` and `.` as `-` (PEP 503).
fn requirement_name(requirement: &str) -> String {
    requirement
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace(['_', '.'], "-")
}

/// The first `world <name>` of a WIT file.
fn world_name(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let name = line.trim().strip_prefix("world ")?;
        let name = name.trim_end_matches(['{', ' ', '\t']).trim();
        (!name.is_empty()).then(|| name.to_owned())
    })
}

/// A `Cargo.toml`.
#[derive(Debug, Clone)]
struct Rust {
    /// `[dependencies]`.
    dependencies: BTreeSet<String>,
}

impl Rust {
    fn read(text: &str) -> Self {
        let dependencies = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|table| table.get("dependencies")?.as_table().cloned())
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default();
        Self { dependencies }
    }

    /// Axum or Actix: an API on 8080.
    fn stack(&self) -> Stack {
        let dev = Dev::Known("cargo run".to_owned());
        if self.dependencies.contains("axum") || self.dependencies.contains("actix-web") {
            Stack::framework("Cargo.toml", TemplateGroup::Api, 8080, dev)
        } else {
            Stack::unknown("Cargo.toml", dev)
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// A tree of in-memory files; a directory exists when a file is below it.
    struct Files(BTreeMap<&'static str, &'static str>);

    impl Files {
        fn of(files: &[(&'static str, &'static str)]) -> Self {
            Self(files.iter().copied().collect())
        }
    }

    impl SourceTree for Files {
        fn read(&self, path: &str) -> Option<String> {
            self.0.get(path).map(|text| (*text).to_owned())
        }

        fn exists(&self, path: &str) -> bool {
            self.0.contains_key(path)
        }

        fn is_dir(&self, path: &str) -> bool {
            let prefix = format!("{path}/");
            self.0.keys().any(|file| file.starts_with(&prefix))
        }
    }

    fn expose(service: &str, port: u16) -> Expose {
        Expose {
            service: ServiceName::new(service).unwrap(),
            port: ContainerPort::new(port).unwrap(),
        }
    }

    /// The proposed `[dev] command` of `files`.
    fn dev(files: &[(&'static str, &'static str)]) -> Option<String> {
        StackDetector::detect(&Files::of(files))
            .dev
            .map(|dev| dev.command)
    }

    /// The proposed `[service] expose` of `files`.
    fn exposed_by(files: &[(&'static str, &'static str)]) -> Option<Expose> {
        match StackDetector::detect(&Files::of(files)).deploy {
            Some(Deployable::Service { expose, .. }) => expose,
            _ => None,
        }
    }

    /// The proposed `[function] build` of `files`.
    fn build(files: &[(&'static str, &'static str)]) -> Option<String> {
        match StackDetector::detect(&Files::of(files)).deploy {
            Some(Deployable::Function { build, .. }) => Some(build),
            _ => None,
        }
    }

    mod dir_tree {
        use super::*;

        // TC: DET-01 (a lockfile is present even when it is not UTF-8)
        #[test]
        fn exists_when_file_is_not_utf8() {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/detect/vite-bun-lockb");
            assert!(DirTree::new(root).exists("bun.lockb"));
        }

        // TC: DET-10 (a directory is not a file)
        #[test]
        fn does_not_exist_when_path_is_directory() {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
            assert!(!DirTree::new(root).exists("detect"));
        }
    }

    mod compose {
        use super::*;

        /// Two services with `expose`, at the repository root.
        const TWO_EXPOSED: [(&str, &str); 1] = [(
            "compose.yaml",
            "services:\n  web:\n    expose: [3000]\n  admin:\n    expose: [\"8080/tcp\"]\n",
        )];

        /// A Vite project whose compose file exposes `app:80`.
        const VITE_WITH_COMPOSE: [(&str, &str); 2] = [
            (
                "package.json",
                r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"6"}}"#,
            ),
            (
                "deploy/compose.yaml",
                "services:\n  app:\n    expose: [80]\n",
            ),
        ];

        // TC: DET-07 (ambiguity gives the variants to ask from)
        #[test]
        fn asks_expose_with_candidates_when_two_services_expose() {
            assert_eq!(
                StackDetector::detect(&Files::of(&TWO_EXPOSED)).questions,
                [Question::Expose {
                    candidates: vec![expose("web", 3000), expose("admin", 8080)],
                }]
            );
        }

        // TC: DET-07 (the root compose file is proposed, its expose left open)
        #[test]
        fn proposes_root_compose_without_expose_when_two_services_expose() {
            assert_eq!(
                StackDetector::detect(&Files::of(&TWO_EXPOSED)).deploy,
                Some(Deployable::Service {
                    compose: RepoPath::new("compose.yaml").unwrap(),
                    expose: None,
                })
            );
        }

        // TC: DET-07 (`compose.yaml` at the root, one exposed service)
        #[test]
        fn proposes_root_compose_service_when_one_service_exposes() {
            let tree = Files::of(&[(
                "compose.yaml",
                "services:\n  db:\n    image: postgres\n  api:\n    expose: [9000]\n",
            )]);
            assert_eq!(
                StackDetector::detect(&tree).deploy,
                Some(Deployable::Service {
                    compose: RepoPath::new("compose.yaml").unwrap(),
                    expose: Some(expose("api", 9000)),
                })
            );
        }

        // TC: DET-07 (the compose file's port wins over the framework's)
        #[test]
        fn takes_compose_port_when_framework_also_present() {
            assert_eq!(exposed_by(&VITE_WITH_COMPOSE), Some(expose("app", 80)));
        }

        // TC: DET-07 (the framework still gives the group)
        #[test]
        fn takes_framework_group_when_compose_present() {
            assert_eq!(
                StackDetector::detect(&Files::of(&VITE_WITH_COMPOSE)).group,
                Some(TemplateGroup::Web)
            );
        }

        // TC: DET-07 (the framework still gives `[dev] command`)
        #[test]
        fn takes_framework_dev_command_when_compose_present() {
            assert_eq!(dev(&VITE_WITH_COMPOSE).as_deref(), Some("npm run dev"));
        }
    }

    mod package_json {
        use super::*;

        // TC: DET-01 (no lockfile is npm; `start` when no `dev`)
        #[test]
        fn proposes_npm_start_when_no_lockfile_and_only_start_script() {
            let files = [(
                "package.json",
                r#"{"scripts":{"start":"node server.js"},"dependencies":{"next":"15"}}"#,
            )];
            assert_eq!(dev(&files).as_deref(), Some("npm start"));
        }

        // TC: DET-01 (the package manager by lockfile)
        #[rstest]
        #[case::bun_lock("bun.lock", "bun dev")]
        #[case::bun_lockb("bun.lockb", "bun dev")]
        #[case::pnpm("pnpm-lock.yaml", "pnpm dev")]
        #[case::yarn("yarn.lock", "yarn dev")]
        #[case::npm("package-lock.json", "npm run dev")]
        fn runs_dev_script_with_manager_when_lockfile(
            #[case] lockfile: &'static str,
            #[case] expected: &str,
        ) {
            let files = [
                ("package.json", r#"{"scripts":{"dev":"vite"}}"#),
                (lockfile, ""),
            ];
            assert_eq!(dev(&files).as_deref(), Some(expected));
        }

        // TC: DET-01 (the framework's default port, the documented table)
        #[rstest]
        #[case::vite(r#"{"devDependencies":{"vite":"6"}}"#, "web", 5173)]
        #[case::next(r#"{"dependencies":{"next":"15"}}"#, "web", 3000)]
        #[case::astro(r#"{"dependencies":{"astro":"5"}}"#, "web", 4321)]
        #[case::sveltekit(r#"{"devDependencies":{"@sveltejs/kit":"2"}}"#, "web", 5173)]
        #[case::hono(r#"{"dependencies":{"hono":"4"}}"#, "api", 3000)]
        fn exposes_framework_default_port_when_dependency(
            #[case] package: &'static str,
            #[case] service: &str,
            #[case] port: u16,
        ) {
            assert_eq!(
                exposed_by(&[("package.json", package)]),
                Some(expose(service, port))
            );
        }

        // TC: DET-01 (Astro depends on Vite: Astro's port wins)
        #[test]
        fn proposes_astro_on_4321_when_astro_and_vite_both_listed() {
            let files = [(
                "package.json",
                r#"{"dependencies":{"astro":"5"},"devDependencies":{"vite":"6"}}"#,
            )];
            assert_eq!(exposed_by(&files), Some(expose("web", 4321)));
        }

        // TC: DET-03 (Bun without a framework is `Bun.serve` on 3000)
        #[test]
        fn proposes_api_on_3000_when_bun_without_framework() {
            let files = [
                ("package.json", r#"{"scripts":{"dev":"bun run index.ts"}}"#),
                ("bun.lock", ""),
            ];
            assert_eq!(exposed_by(&files), Some(expose("api", 3000)));
        }

        // TC: DET-10 (an unknown framework: port, dev command and group asked)
        #[test]
        fn asks_expose_dev_and_group_when_framework_unknown_and_no_script() {
            let tree = Files::of(&[("package.json", r#"{"dependencies":{"express":"5"}}"#)]);
            assert_eq!(
                StackDetector::detect(&tree).questions,
                [
                    Question::Expose {
                        candidates: Vec::new(),
                    },
                    Question::DevCommand,
                    Question::Group,
                ]
            );
        }
    }

    mod several_stacks {
        use super::*;

        /// A tooling-only `package.json` beside an Axum `Cargo.toml`.
        const TOOLING_AND_AXUM: [(&str, &str); 2] = [
            (
                "package.json",
                r#"{"devDependencies":{"prettier":"3"},"scripts":{"dev":"tailwindcss -w"}}"#,
            ),
            ("Cargo.toml", "[dependencies]\naxum = \"0.8\"\n"),
        ];

        /// A Vite `package.json` beside a `go.mod`.
        const VITE_AND_GO: [(&str, &str); 2] = [
            (
                "package.json",
                r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"6"}}"#,
            ),
            ("go.mod", "module example.com/app\n"),
        ];

        // TC: DET-04 (a recognised framework wins over an unknown one)
        #[test]
        fn proposes_axum_when_package_json_framework_unknown() {
            assert_eq!(exposed_by(&TOOLING_AND_AXUM), Some(expose("api", 8080)));
        }

        // TC: DET-04 (nothing to ask when only one stack is recognised)
        #[test]
        fn asks_nothing_when_package_json_framework_unknown_beside_axum() {
            assert_eq!(
                StackDetector::detect(&Files::of(&TOOLING_AND_AXUM)).questions,
                []
            );
        }

        // TC: DET-06 (ambiguity gives the variants to ask from)
        #[test]
        fn asks_stack_with_both_alternatives_when_vite_and_go() {
            assert_eq!(
                StackDetector::detect(&Files::of(&VITE_AND_GO)).questions,
                [Question::Stack {
                    alternatives: vec![
                        Alternative {
                            source: "package.json",
                            expose: Some(expose("web", 5173)),
                            dev: Some(DevCommand {
                                command: "npm run dev".to_owned(),
                            }),
                            group: TemplateGroup::Web,
                        },
                        Alternative {
                            source: "go.mod",
                            expose: Some(expose("api", 8080)),
                            dev: Some(DevCommand {
                                command: "go run .".to_owned(),
                            }),
                            group: TemplateGroup::Api,
                        },
                    ],
                }]
            );
        }

        // TC: DET-06 (the first alternative is the proposal by default)
        #[test]
        fn proposes_first_alternative_when_vite_and_go() {
            assert_eq!(exposed_by(&VITE_AND_GO), Some(expose("web", 5173)));
        }
    }

    mod function {
        use super::*;

        /// A repository with only `wit/`.
        const ONLY_WIT: [(&str, &str); 1] = [("wit/world.wit", "world w {}\n")];

        // TC: DET-08 (`wit/` alone is a function, its build unknown)
        #[test]
        fn proposes_no_deploy_when_only_wit_directory() {
            assert_eq!(StackDetector::detect(&Files::of(&ONLY_WIT)).deploy, None);
        }

        // TC: DET-08 (`wit/` alone is a function; the build is asked)
        #[test]
        fn asks_build_when_only_wit_directory() {
            assert_eq!(
                StackDetector::detect(&Files::of(&ONLY_WIT)).questions,
                [Question::Build]
            );
        }

        // TC: DET-08 (`wit/` alone gives the function group)
        #[test]
        fn defaults_group_to_function_when_only_wit_directory() {
            assert_eq!(
                StackDetector::detect(&Files::of(&ONLY_WIT)).group,
                Some(TemplateGroup::Function)
            );
        }

        /// A TS function's `package.json` without a `build` script.
        const TS_PACKAGE: &str = r#"{"devDependencies":{"@bytecodealliance/componentize-js":"0.23.0","@bytecodealliance/jco":"1.39.0"}}"#;
        /// [`TS_PACKAGE`] with a `build` script.
        const TS_PACKAGE_WITH_BUILD: &str = r#"{"scripts":{"build":"anything"},"devDependencies":{"@bytecodealliance/componentize-js":"0.23.0"}}"#;

        // TC: DET-08 (the package's own `build` script, after an install,
        // through the manager's own `run`)
        #[rstest]
        #[case::bun("bun.lock", "bun install && bun run build")]
        #[case::pnpm("pnpm-lock.yaml", "pnpm install && pnpm run build")]
        #[case::yarn("yarn.lock", "yarn install && yarn run build")]
        #[case::npm("package-lock.json", "npm install && npm run build")]
        fn runs_build_script_after_install_when_package_has_one(
            #[case] lockfile: &'static str,
            #[case] expected: &str,
        ) {
            let files = [
                ("package.json", TS_PACKAGE_WITH_BUILD),
                (lockfile, ""),
                ("src/index.ts", ""),
            ];
            assert_eq!(build(&files).as_deref(), Some(expected));
        }

        // TC: DET-08 (no `build` script under bun: the pipeline of templates/fn-ts, `bun build` and `jco componentize`, never the componentize-js CLI that 0.23.0 runs without the WIT)
        #[test]
        fn bundles_with_bun_and_componentizes_with_jco_when_no_build_script() {
            let files = [
                ("package.json", TS_PACKAGE),
                ("bun.lock", ""),
                ("src/index.ts", ""),
                ("wit/world.wit", "package a:b;\n\nworld function {\n}\n"),
            ];
            assert_eq!(
                build(&files).as_deref(),
                Some(
                    "bun build src/index.ts --outfile build/index.js --target browser --format esm \
                     --external 'wasi:*' && mkdir -p dist && bunx @bytecodealliance/jco componentize \
                     build/index.js --wit wit --world-name function --out dist/fn.wasm"
                )
            );
        }

        // TC: DET-08 (`src/index.js` is the entry when there is no `src/index.ts`)
        #[test]
        fn bundles_src_index_js_when_no_typescript_entry() {
            let files = [
                ("package.json", TS_PACKAGE),
                ("bun.lock", ""),
                ("src/index.js", ""),
                ("wit/world.wit", "world w {}\n"),
            ];
            let proposed = build(&files).unwrap_or_default();
            assert!(
                proposed.starts_with("bun build src/index.js ")
                    && proposed.contains(" --world-name w "),
                "{proposed}"
            );
        }

        // TC: DET-08 (without bun and without a `build` script there is no
        // bundler to propose: the build is asked)
        #[test]
        fn asks_build_when_no_build_script_and_not_bun() {
            let tree = Files::of(&[
                ("package.json", TS_PACKAGE),
                ("package-lock.json", ""),
                ("src/index.ts", ""),
            ]);
            let detected = StackDetector::detect(&tree);
            assert_eq!(
                (
                    detected.deploy,
                    detected.questions.contains(&Question::Build)
                ),
                (None, true)
            );
        }

        // TC: DET-09 (the world of `wit/world.wit`; `requirements.txt`)
        #[test]
        fn builds_world_of_wit_when_requirements_list_componentize_py() {
            let tree = Files::of(&[
                ("requirements.txt", "componentize-py==0.16  # build\n"),
                ("main.py", ""),
                ("wit/world.wit", "package a:b;\n\nworld handler {\n}\n"),
            ]);
            assert_eq!(
                StackDetector::detect(&tree).deploy,
                Some(Deployable::Function {
                    build: "componentize-py -d wit -w handler componentize main -o dist/fn.wasm"
                        .to_owned(),
                    component: RepoPath::new("dist/fn.wasm").unwrap(),
                })
            );
        }
    }

    mod python {
        use super::*;

        /// A poetry project listing `FastAPI`, its app in `app.py`.
        const POETRY_FASTAPI: [(&str, &str); 2] = [
            (
                "pyproject.toml",
                "[tool.poetry.dependencies]\npython = \"^3.12\"\nFastAPI = \"*\"\n",
            ),
            ("app.py", ""),
        ];

        // TC: DET-05 (poetry by `[tool.poetry]`)
        #[test]
        fn proposes_poetry_uvicorn_when_poetry_lists_fastapi() {
            assert_eq!(
                dev(&POETRY_FASTAPI).as_deref(),
                Some("poetry run uvicorn app:app --reload")
            );
        }

        // TC: DET-05 (`FastAPI` is an API)
        #[test]
        fn defaults_group_to_api_when_poetry_lists_fastapi() {
            assert_eq!(
                StackDetector::detect(&Files::of(&POETRY_FASTAPI)).group,
                Some(TemplateGroup::Api)
            );
        }

        // TC: DET-05 (poetry by `poetry.lock` alone)
        #[test]
        fn proposes_poetry_when_poetry_lock_without_tool_poetry() {
            let files = [
                (
                    "pyproject.toml",
                    "[project]\ndependencies = [\"uvicorn\"]\n",
                ),
                ("poetry.lock", ""),
                ("main.py", ""),
            ];
            assert_eq!(
                dev(&files).as_deref(),
                Some("poetry run uvicorn main:app --reload")
            );
        }

        // TC: DET-05 (`requirements.txt` with no lockfile is plain python)
        #[test]
        fn proposes_plain_uvicorn_when_requirements_list_fastapi() {
            let files = [
                ("requirements.txt", "fastapi[standard]>=0.115\n"),
                ("main.py", ""),
            ];
            assert_eq!(dev(&files).as_deref(), Some("uvicorn main:app --reload"));
        }

        // TC: DET-05 (`FastAPI` from `requirements.txt` listens on 8000)
        #[test]
        fn exposes_api_on_8000_when_requirements_list_fastapi() {
            let files = [("requirements.txt", "fastapi\n"), ("main.py", "")];
            assert_eq!(exposed_by(&files), Some(expose("api", 8000)));
        }
    }

    mod cargo_toml {
        use super::*;

        // TC: DET-04 (Actix like Axum, an API on 8080)
        #[test]
        fn proposes_api_on_8080_when_actix_web() {
            let files = [("Cargo.toml", "[dependencies]\nactix-web = \"4\"\n")];
            assert_eq!(exposed_by(&files), Some(expose("api", 8080)));
        }
    }

    /// The answers to what the files leave open (the CLI asks, the detector applies the answer).
    mod answers {
        use super::*;

        /// The manifest `files` give once `expose` answers `Expose`.
        fn answered(files: &[(&'static str, &'static str)], service: &str, port: u16) -> String {
            let tree = Files::of(files);
            let mut detected = StackDetector::detect(&tree);
            detected.answer_expose(&tree, Some(expose(service, port)));
            detected
                .manifest()
                .map(|manifest| manifest.to_toml())
                .unwrap_or_default()
        }

        // TC: CRE-09 (files that settle nothing: the answer to `Kind` is a
        // compose project of the compose file proposed by the detection)
        #[test]
        fn proposes_default_compose_when_expose_answered_for_nothing() {
            assert_eq!(
                answered(&[("README.md", "# site\n")], "web", 3000),
                "[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n"
            );
        }

        // TC: CRE-09 (a compose file `docker compose` reads by default, which
        // detection does not read services from, is the one named)
        #[test]
        fn names_docker_compose_file_when_expose_answered_for_it() {
            assert_eq!(
                answered(&[("docker-compose.yml", "services: {}\n")], "web", 3000),
                "[service]\ncompose = \"docker-compose.yml\"\nexpose = \"web:3000\"\n"
            );
        }

        // TC: CRE-09 (the proposed compose file stays)
        #[test]
        fn keeps_proposed_compose_when_expose_answered() {
            let files = [(
                "compose.yaml",
                "services:\n  web:\n    expose: [3000]\n  admin:\n    expose: [8080]\n",
            )];
            assert_eq!(
                answered(&files, "admin", 8080),
                "[service]\ncompose = \"compose.yaml\"\nexpose = \"admin:8080\"\n"
            );
        }

        // TC: CRE-09 (a compose project builds no component: `Build` closes)
        #[test]
        fn closes_build_when_expose_answered() {
            let tree = Files::of(&[("README.md", "")]);
            let mut detected = StackDetector::detect(&tree);
            let before = detected.is_open(&Question::Build);
            detected.answer_expose(&tree, Some(expose("web", 3000)));
            assert_eq!((before, detected.is_open(&Question::Build)), (true, false));
        }

        // TC: CRE-09 (`wit/` without a build: the typed command builds the
        // documented component)
        #[test]
        fn proposes_default_component_when_build_answered() {
            let tree = Files::of(&[("wit/world.wit", "")]);
            let mut detected = StackDetector::detect(&tree);
            detected.answer_build("cargo component build".to_owned());
            assert_eq!(
                detected.deploy,
                Some(Deployable::Function {
                    build: "cargo component build".to_owned(),
                    component: RepoPath::constant("dist/fn.wasm"),
                })
            );
        }

        // TC: CRE-09, DET-06 (the picked stack's `expose` and `dev`)
        #[test]
        fn proposes_picked_stack_when_stack_answered() {
            let files = [
                (
                    "package.json",
                    r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"5"}}"#,
                ),
                ("go.mod", "module x\n"),
            ];
            let mut detected = StackDetector::detect(&Files::of(&files));
            let Some(Question::Stack { alternatives }) = detected.questions.first().cloned() else {
                panic!("expected a stack question: {detected:?}");
            };
            let go = alternatives.last().cloned().unwrap();
            detected.choose_stack(&go);
            assert_eq!(
                (exposed_of(&detected), detected.dev.clone()),
                (go.expose.clone(), go.dev.clone())
            );
        }

        fn exposed_of(detected: &Detected) -> Option<Expose> {
            match &detected.deploy {
                Some(Deployable::Service { expose, .. }) => expose.clone(),
                _ => None,
            }
        }
    }
}
