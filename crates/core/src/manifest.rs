//! [`ProjectManifest`]: `liftoff.toml`, the committed description of a project and of a template.
//!
//! [`ProjectManifest::parse`] turns the text into the typed manifest and
//! is the only validation: unknown keys are refused (`deny_unknown_fields`),
//! exactly one of `[service]` and `[function]` is present, `expose` is
//! `service:port`, and every path stays inside the repository. Every
//! failure is an [`Error::Validation`] on field [`ProjectManifest::FIELD`]
//! whose path names the key (`service.expose`).

use std::collections::BTreeMap;

use crate::domain::{ContainerPort, ScopeKind, SecretKey, ServiceName, TemplateGroup};
use crate::error::{Error, Problem, ValidationError};

/// A parsed `liftoff.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectManifest {
    /// `[template]`: present in template repositories, which the catalogue
    /// lists by it.
    pub template: Option<TemplateInfo>,
    /// `[service]` or `[function]`.
    pub deploy: Deployable,
    /// `[dev]`.
    pub dev: Option<DevCommand>,
    /// `[secrets]`, by key.
    pub secrets: BTreeMap<SecretKey, SecretSpec>,
    /// `[substitute] files`: where `{{project}}` and `{{domain}}` are
    /// replaced.
    pub substitute: Vec<RepoPath>,
}

impl ProjectManifest {
    /// The field of every [`Error::Validation`] of
    /// [`ProjectManifest::parse`]; its path names the key.
    pub const FIELD: &'static str = "manifest";

    /// Parses and validates the text of a `liftoff.toml`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on field [`ProjectManifest::FIELD`] with the
    /// path of the offending key: malformed TOML, an unknown key, both or
    /// neither of `[service]` and `[function]`, an `expose` that is not
    /// `service:port` (port 1-65535), a path that is absolute or leaves
    /// the repository, a `generate` other than `random:<n>`, a `scope` or
    /// `group` outside its spellings.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let raw: raw::Manifest = serde_path_to_error::deserialize(toml::Deserializer::new(text))
            .map_err(|error| {
                let path = dotted(error.path());
                let problem = Problem::Invalid(error.inner().message().trim().to_owned());
                match path {
                    Some(path) => ValidationError::at(Self::FIELD, path, problem),
                    None => ValidationError::new(Self::FIELD, problem),
                }
            })?;
        Ok(raw.into_manifest()?)
    }

    /// Reads the `liftoff.toml` a project stored (`Project::manifest`):
    /// `None` when the text is not a v2 manifest, that is not TOML or a
    /// table with neither `[service]` nor `[function]` (what a v1 client
    /// may have stored); otherwise [`ProjectManifest::parse`], so a v2
    /// manifest with a mistake is reported, not taken for no manifest.
    ///
    /// # Errors
    ///
    /// Those of [`ProjectManifest::parse`].
    pub fn parse_stored(text: &str) -> Result<Option<Self>, Error> {
        let claims_v2 = toml::from_str::<toml::Table>(text)
            .is_ok_and(|table| table.contains_key("service") || table.contains_key("function"));
        if claims_v2 {
            Self::parse(text).map(Some)
        } else {
            Ok(None)
        }
    }

    /// The manifest as `liftoff.toml` text, tables in the order of spec
    /// 8.5 (`[template]`, `[service]`/`[function]`, `[dev]`, `[secrets]`,
    /// `[substitute]`), absent ones left out, one blank line between
    /// tables; [`ProjectManifest::parse`] reads it back unchanged.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut tables: Vec<Vec<String>> = Vec::new();
        if let Some(template) = &self.template {
            tables.push(vec![
                "[template]".to_owned(),
                format!("group = {}", quoted(template.group.as_str())),
                format!("name = {}", quoted(&template.name)),
                format!("description = {}", quoted(&template.description)),
            ]);
        }
        tables.push(match &self.deploy {
            Deployable::Service { compose, expose } => {
                let mut table = vec![
                    "[service]".to_owned(),
                    format!("compose = {}", quoted(compose.as_str())),
                ];
                if let Some(expose) = expose {
                    let value = format!("{}:{}", expose.service, expose.port);
                    table.push(format!("expose = {}", quoted(&value)));
                }
                table
            }
            Deployable::Function { build, component } => vec![
                "[function]".to_owned(),
                format!("build = {}", quoted(build)),
                format!("component = {}", quoted(component.as_str())),
            ],
        });
        if let Some(dev) = &self.dev {
            tables.push(vec![
                "[dev]".to_owned(),
                format!("command = {}", quoted(&dev.command)),
            ]);
        }
        if !self.secrets.is_empty() {
            let mut table = vec!["[secrets]".to_owned()];
            table.extend(self.secrets.iter().map(|(key, spec)| {
                let mut fields = Vec::new();
                if let Some(hint) = &spec.hint {
                    fields.push(format!("hint = {}", quoted(hint)));
                }
                if let Some(Generate::Random(length)) = spec.generate {
                    fields.push(format!(
                        "generate = {}",
                        quoted(&format!("random:{length}"))
                    ));
                }
                if let Some(scope) = spec.scope {
                    fields.push(format!("scope = {}", quoted(scope.as_str())));
                }
                if fields.is_empty() {
                    format!("{key} = {{}}")
                } else {
                    format!("{key} = {{ {} }}", fields.join(", "))
                }
            }));
            tables.push(table);
        }
        if !self.substitute.is_empty() {
            let files: Vec<String> = self
                .substitute
                .iter()
                .map(|file| quoted(file.as_str()))
                .collect();
            tables.push(vec![
                "[substitute]".to_owned(),
                format!("files = [{}]", files.join(", ")),
            ]);
        }
        tables
            .iter()
            .map(|table| {
                let mut text = table.join("\n");
                text.push('\n');
                text
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `path` as dotted keys (`secrets.KEY.hint`, `substitute.files[1]`);
/// `None` at the root (malformed TOML).
fn dotted(path: &serde_path_to_error::Path) -> Option<String> {
    use serde_path_to_error::Segment;
    let mut out = String::new();
    for segment in path {
        match segment {
            Segment::Seq { index } => {
                out.push('[');
                out.push_str(&index.to_string());
                out.push(']');
            }
            Segment::Map { key } | Segment::Enum { variant: key } => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Segment::Unknown => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push('?');
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// `text` as a TOML basic string.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.extend(format!("\\u{:04X}", u32::from(c)).chars()),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A validation error of the manifest at `path`.
fn at(path: impl Into<String>, problem: Problem) -> ValidationError {
    ValidationError::at(ProjectManifest::FIELD, path, problem)
}

/// The shape of `expose`.
const EXPOSE_PATTERN: &str = "<service>:<port>, the port 1-65535";
/// The shape of `generate`.
const GENERATE_PATTERN: &str = "random:<n>, n bytes (1-65535)";
/// The shape of a path in the manifest.
const PATH_PATTERN: &str =
    "a relative path inside the repository, without `..` or control characters";

/// `[template]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateInfo {
    /// `group`.
    pub group: TemplateGroup,
    /// `name`, shown in the catalogue.
    pub name: String,
    /// `description`, shown in the catalogue.
    pub description: String,
}

/// What the project deploys: `[service]` or `[function]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deployable {
    /// `[service]`: a compose project.
    Service {
        /// `compose`, the compose file.
        compose: RepoPath,
        /// `expose`, the service and port the domain goes to.
        expose: Option<Expose>,
    },
    /// `[function]`: a WASM component.
    Function {
        /// `build`, the command that builds the component.
        build: String,
        /// `component`, the built component.
        component: RepoPath,
    },
}

/// `expose = "service:port"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expose {
    /// The compose service.
    pub service: ServiceName,
    /// Its port inside the container.
    pub port: ContainerPort,
}

/// `[dev]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevCommand {
    /// `command`, what `lof dev` runs.
    pub command: String,
}

/// One entry of `[secrets]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSpec {
    /// `hint`, shown when `lof create` asks for the value.
    pub hint: Option<String>,
    /// `generate`: the CLI makes the value itself.
    pub generate: Option<Generate>,
    /// `scope`; `None` leaves the default to `lof create`.
    pub scope: Option<ScopeKind>,
}

/// `generate = "random:<n>"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generate {
    /// `n` random bytes.
    Random(u16),
}

/// A relative path inside the repository: not absolute, no `..`, no
/// control characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

impl RepoPath {
    /// Validates `raw` as a relative path inside the repository: not
    /// empty, not absolute, no `..` segment, no control character, naming
    /// something below the root.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `path`.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        if raw.is_empty() {
            return Err(ValidationError::new("path", Problem::Empty));
        }
        let segments: Vec<&str> = raw.split(['/', '\\']).collect();
        let absolute = raw.starts_with(['/', '\\'])
            || segments.first().is_some_and(|first| first.contains(':'));
        let leaves = segments.contains(&"..");
        let names_something = segments
            .iter()
            .any(|segment| !segment.is_empty() && *segment != ".");
        // A control character (NUL among them) names no file a template
        // ships, and its escape sequences would reach the terminal that
        // shows the plan.
        if absolute || leaves || !names_something || raw.chars().any(char::is_control) {
            return Err(ValidationError::new(
                "path",
                Problem::Pattern {
                    expected: PATH_PATTERN,
                },
            ));
        }
        Ok(Self(raw.to_owned()))
    }

    /// A path the core itself proposes (`deploy/compose.yaml`), checked by
    /// a unit test instead of at run time.
    pub(crate) fn constant(path: &'static str) -> Self {
        debug_assert!(Self::new(path).is_ok(), "{path} is a repository path");
        Self(path.to_owned())
    }

    /// The path as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Expose {
    /// Parses `service:port`.
    fn parse(raw: &str) -> Option<Self> {
        let (service, port) = raw.split_once(':')?;
        Some(Self {
            service: ServiceName::new(service).ok()?,
            port: ContainerPort::new(port.parse().ok()?).ok()?,
        })
    }
}

impl Generate {
    /// Parses `random:<n>` (`n` > 0): the manifest's spelling, and the
    /// wire's (`CreateStep::AskSecrets` of a plan).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let length: u16 = raw.strip_prefix("random:")?.parse().ok()?;
        (length > 0).then_some(Self::Random(length))
    }
}

/// The TOML shape of `liftoff.toml`: strict at every depth
/// (`deny_unknown_fields`), validated into [`ProjectManifest`] at the
/// boundary.
mod raw {
    use std::collections::BTreeMap;

    use serde::Deserialize;

    use super::{
        Deployable, DevCommand, EXPOSE_PATTERN, Expose, GENERATE_PATTERN, Generate,
        ProjectManifest, RepoPath, SecretSpec, TemplateInfo, at,
    };
    use crate::domain::{ScopeKind, SecretKey, TemplateGroup};
    use crate::error::{Problem, ValidationError};

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Manifest {
        #[serde(default)]
        template: Option<Template>,
        #[serde(default)]
        service: Option<Service>,
        #[serde(default)]
        function: Option<Function>,
        #[serde(default)]
        dev: Option<Dev>,
        #[serde(default)]
        secrets: BTreeMap<String, Secret>,
        #[serde(default)]
        substitute: Option<Substitute>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Template {
        group: String,
        name: String,
        description: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Service {
        compose: String,
        #[serde(default)]
        expose: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Function {
        build: String,
        component: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Dev {
        command: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Secret {
        #[serde(default)]
        hint: Option<String>,
        #[serde(default)]
        generate: Option<String>,
        #[serde(default)]
        scope: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Substitute {
        #[serde(default)]
        files: Vec<String>,
    }

    /// `raw` as a [`RepoPath`], or an error at `path`.
    fn repo_path(raw: &str, path: &str) -> Result<RepoPath, ValidationError> {
        RepoPath::new(raw).map_err(|error| at(path, error.problem))
    }

    /// `raw` when it is not blank, or [`Problem::Empty`] at `path`.
    fn filled(raw: String, path: &str) -> Result<String, ValidationError> {
        if raw.trim().is_empty() {
            Err(at(path, Problem::Empty))
        } else {
            Ok(raw)
        }
    }

    /// `raw`, which the catalogue or `lof create` shows on a terminal
    /// (TPL-03; a secret's hint): a template's text without control
    /// characters, whose escape sequences would reach the terminal.
    fn printable(raw: String, path: &str) -> Result<String, ValidationError> {
        if raw.chars().any(char::is_control) {
            Err(at(
                path,
                Problem::Invalid("must not contain control characters".to_owned()),
            ))
        } else {
            Ok(raw)
        }
    }

    impl Manifest {
        pub(super) fn into_manifest(self) -> Result<ProjectManifest, ValidationError> {
            let template = self.template.map(Template::into_info).transpose()?;
            let deploy = match (self.service, self.function) {
                (Some(service), None) => service.into_deployable()?,
                (None, Some(function)) => function.into_deployable()?,
                (Some(_), Some(_)) => {
                    return Err(ValidationError::new(
                        ProjectManifest::FIELD,
                        Problem::Invalid(
                            "[service] and [function] exclude each other: keep one".to_owned(),
                        ),
                    ));
                }
                (None, None) => {
                    return Err(ValidationError::new(
                        ProjectManifest::FIELD,
                        Problem::Invalid("needs a [service] or a [function] table".to_owned()),
                    ));
                }
            };
            let dev = self
                .dev
                .map(|dev| filled(dev.command, "dev.command").map(|command| DevCommand { command }))
                .transpose()?;
            let secrets = self
                .secrets
                .into_iter()
                .map(|(key, secret)| secret.into_spec(&key))
                .collect::<Result<_, _>>()?;
            let substitute = self
                .substitute
                .map(|substitute| substitute.files)
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(index, file)| repo_path(file, &format!("substitute.files[{index}]")))
                .collect::<Result<_, _>>()?;
            Ok(ProjectManifest {
                template,
                deploy,
                dev,
                secrets,
                substitute,
            })
        }
    }

    impl Template {
        fn into_info(self) -> Result<TemplateInfo, ValidationError> {
            Ok(TemplateInfo {
                group: TemplateGroup::parse(&self.group)
                    .map_err(|error| at("template.group", error.problem))?,
                name: printable(filled(self.name, "template.name")?, "template.name")?,
                description: printable(self.description, "template.description")?,
            })
        }
    }

    impl Service {
        fn into_deployable(self) -> Result<Deployable, ValidationError> {
            let expose = self
                .expose
                .map(|raw| {
                    Expose::parse(&raw).ok_or_else(|| {
                        at(
                            "service.expose",
                            Problem::Pattern {
                                expected: EXPOSE_PATTERN,
                            },
                        )
                    })
                })
                .transpose()?;
            Ok(Deployable::Service {
                compose: repo_path(&self.compose, "service.compose")?,
                expose,
            })
        }
    }

    impl Function {
        fn into_deployable(self) -> Result<Deployable, ValidationError> {
            Ok(Deployable::Function {
                build: filled(self.build, "function.build")?,
                component: repo_path(&self.component, "function.component")?,
            })
        }
    }

    impl Secret {
        fn into_spec(self, key: &str) -> Result<(SecretKey, SecretSpec), ValidationError> {
            let secret_key =
                SecretKey::new(key).map_err(|error| at(format!("secrets.{key}"), error.problem))?;
            let generate = self
                .generate
                .map(|raw| {
                    Generate::parse(&raw).ok_or_else(|| {
                        at(
                            format!("secrets.{key}.generate"),
                            Problem::Pattern {
                                expected: GENERATE_PATTERN,
                            },
                        )
                    })
                })
                .transpose()?;
            let scope = self
                .scope
                .map(|raw| {
                    ScopeKind::parse(&raw)
                        .map_err(|error| at(format!("secrets.{key}.scope"), error.problem))
                })
                .transpose()?;
            Ok((
                secret_key,
                SecretSpec {
                    hint: self
                        .hint
                        .map(|hint| printable(hint, &format!("secrets.{key}.hint")))
                        .transpose()?,
                    generate,
                    scope,
                },
            ))
        }
    }
}

#[cfg(test)]
mod tests;
