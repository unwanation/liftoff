//! [`ModuleManifest`]: what a module declares in `manifest()`.
//!
//! The JSON is strict: an unknown field anywhere is a refusal (MOD-06),
//! so a module built against a newer world fails to install instead of
//! silently losing what it declared. Every field but `name` and `version`
//! may be left out and is then empty (`migrations` is `0`).
//!
//! Values are validated here, at the boundary, so the domain never checks
//! them again: `version` is semver, a route has a known method and a path
//! below the module's prefix, an `http_allow` entry is an [`HttpAllow`]
//! (a host, or a host and a port), a forge host is a host, the names of
//! commands, arguments, tools and replaced commands are lowercase words,
//! and a schema is a JSON Schema object or boolean (compiling it is left to installation, which validates the configuration against it).
//!
//! ```json
//! {
//!   "name": "allimports",
//!   "version": "0.1.0",
//!   "requires": ["oidc"],
//!   "config_schema": {"type": "object"},
//!   "http_allow": ["127.0.0.1"],
//!   "commands": [{"name": "greet", "description": "Says hello",
//!                 "args": [{"name": "who", "type": "string", "required": true, "hint": "Who to greet"}]}],
//!   "routes": [{"method": "GET", "path": "/hello", "public": true}],
//!   "tools": [{"name": "core-call", "description": "Calls the core", "schema": {"type": "object"}}],
//!   "migrations": 1,
//!   "hooks": ["authorize", "before_deploy"],
//!   "replaces": ["login"],
//!   "git_provider": {"hosts": ["git.example.org"], "oauth": false}
//! }
//! ```

use crate::domain::ModuleName;
use crate::error::{Problem, ValidationError};

/// A module's declaration of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleManifest {
    /// `name`: the module's name.
    pub name: ModuleName,
    /// `version`: its version, semver `X.Y.Z` with an optional
    /// pre-release and build.
    pub version: String,
    /// `requires`: modules that must be installed first (`tenants` →
    /// `oidc`).
    pub requires: Vec<ModuleName>,
    /// `config_schema`: the JSON Schema of its configuration, if it has
    /// one.
    pub config_schema: Option<serde_json::Value>,
    /// `http_allow`: the hosts `wasi:http` may reach (MOD-07).
    pub http_allow: Vec<HttpAllow>,
    /// `commands`: the CLI commands it adds.
    pub commands: Vec<CommandSpec>,
    /// `routes`: its routes under `/api/v1/m/<name>/`.
    pub routes: Vec<RouteSpec>,
    /// `tools`: its MCP tools.
    pub tools: Vec<ToolSpec>,
    /// `migrations`: how many migrations its storage has.
    pub migrations: u32,
    /// `hooks`: the core hooks it subscribes to.
    pub hooks: Vec<HookName>,
    /// `replaces`: core CLI commands its own commands take over
    /// (`oidc`: `login`).
    pub replaces: Vec<String>,
    /// `git_provider`: the forge hosts it serves.
    pub git_provider: Option<GitProviderSpec>,
}

/// A CLI command of a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// `name`: up to four lowercase words (`[a-z][a-z0-9_-]*`) separated
    /// by single spaces (`tenant members add`): the words of `lof` that
    /// run it.
    pub name: String,
    /// `description`.
    pub description: String,
    /// `args`, in order.
    pub args: Vec<ArgSpec>,
}

/// An argument of a [`CommandSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    /// `name`.
    pub name: String,
    /// `type`.
    pub kind: ArgKind,
    /// `required` (default `false`).
    pub required: bool,
    /// `hint`: what the CLI asks with on a TTY (REG-08).
    pub hint: Option<String>,
}

/// The `type` of an [`ArgSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// `string`.
    String,
    /// `integer`.
    Integer,
    /// `boolean`.
    Boolean,
    /// `project`: a project name, which the CLI fills, when it is not
    /// given, from `--project` or the repository of the working tree, as
    /// for its own commands (`lof protect --env prod` in a project's directory).
    Project,
}

impl ArgKind {
    /// Its name in a manifest (`string`, `integer`, `boolean`,
    /// `project`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Project => "project",
        }
    }
}

/// A route of a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    /// `method`: one of [`RouteSpec::METHODS`].
    pub method: String,
    /// `path` below the module's prefix: `/`, or `/`-separated segments of
    /// ASCII letters, digits and `-._~{}`, none of them empty, `.` or
    /// `..`, and not below [`RouteSpec::RESERVED`]. A whole segment in
    /// braces (`{id}`) matches any one segment of a request.
    pub path: String,
    /// `public` (default `false`): reachable without a session (OIDC
    /// callbacks, the login page).
    pub public: bool,
}

impl RouteSpec {
    /// The first segment of the paths the host calls a module's
    /// `handle-route` on for the hooks that answer data (`authenticate`, `render_page`): no route may be declared below it,
    /// and [`RouteSpec::matches`] matches no request below it (in any
    /// case, `.` percent-encoded or not), so no request from outside
    /// reaches those calls, not even through a parameter route.
    pub const RESERVED: &'static str = ".hook";

    /// Whether this route is the one a request `method path` (without
    /// the query) asks for. A request below [`RouteSpec::RESERVED`]
    /// matches no route.
    #[must_use]
    pub fn matches(&self, method: &str, path: &str) -> bool {
        if self.method != method || is_reserved(path) {
            return false;
        }
        let mut declared = self.path.split('/');
        let mut asked = path.split('/');
        loop {
            match (declared.next(), asked.next()) {
                (None, None) => return true,
                (Some(want), Some(got)) => {
                    let parameter = want.len() > 2 && want.starts_with('{') && want.ends_with('}');
                    if !(want == got || parameter && !got.is_empty()) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
    }

    /// The methods a route may have.
    pub const METHODS: &'static [&'static str] =
        &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
}

/// An entry of `http_allow`: a host a module's `wasi:http` may reach,
/// on the default port of the request's scheme or on the port the entry
/// names (MOD-07).
///
/// The host is a DNS name (lowercased), an IPv4 literal or an IPv6
/// literal (bracketed when a port follows); `*`, schemes, paths and
/// empty entries are refused. Whether its addresses may be private is
/// the host's policy, not the manifest's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpAllow {
    host: String,
    port: Option<u16>,
}

impl HttpAllow {
    /// Parses `host` or `host:port` (`[v6]:port`).
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] of the field `manifest` naming the entry.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let refused = || {
            invalid(&format_args!(
                "http_allow: {raw:?} is not a host or host:port"
            ))
        };
        let (host, port) = split_host_port(raw).ok_or_else(refused)?;
        if !is_host(host) {
            return Err(refused());
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// The host, without brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port it names, if any.
    #[must_use]
    pub const fn port(&self) -> Option<u16> {
        self.port
    }

    /// Whether a request to `host` (as a URI has it: an IPv6 literal in
    /// brackets, a trailing dot allowed) on `port` is allowed, where
    /// `default_port` is the port of the request's scheme: the hosts match
    /// ignoring ASCII case, and the port is the entry's, or the default one
    /// when the entry names none.
    #[must_use]
    pub fn allows(&self, host: &str, port: u16, default_port: u16) -> bool {
        let host = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(host);
        let host = host.strip_suffix('.').unwrap_or(host);
        !host.is_empty()
            && self.host.eq_ignore_ascii_case(host)
            && port == self.port.unwrap_or(default_port)
    }
}

impl core::fmt::Display for HttpAllow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]", self.host)?;
        } else {
            f.write_str(&self.host)?;
        }
        self.port.map_or(Ok(()), |port| write!(f, ":{port}"))
    }
}

/// `raw` as a host and an optional port: `[v6]`, `[v6]:port`, a bare
/// IPv6 literal, `host` or `host:port`; `None` when it has none of
/// these shapes or the port is not in `1..=65535`.
fn split_host_port(raw: &str) -> Option<(&str, Option<u16>)> {
    let port = |digits: &str| -> Option<u16> {
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok().filter(|port| *port != 0)
    };
    if let Some(bracketed) = raw.strip_prefix('[') {
        let (inner, rest) = bracketed.split_once(']')?;
        inner.parse::<core::net::Ipv6Addr>().ok()?;
        return match rest.strip_prefix(':') {
            None if rest.is_empty() => Some((inner, None)),
            None => None,
            Some(digits) => Some((inner, Some(port(digits)?))),
        };
    }
    if raw.parse::<core::net::Ipv6Addr>().is_ok() {
        return Some((raw, None));
    }
    match raw.split_once(':') {
        None => Some((raw, None)),
        Some((host, digits)) => Some((host, Some(port(digits)?))),
    }
}

/// Whether `host` is an IP literal or a DNS name: dot-separated labels
/// of 1 to 63 ASCII letters, digits and inner hyphens, 253 bytes in all.
fn is_host(host: &str) -> bool {
    if host.parse::<core::net::IpAddr>().is_ok() {
        return true;
    }
    let label = |label: &str| {
        (1..=63).contains(&label.len())
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    (1..=253).contains(&host.len()) && host.split('.').all(label)
}

/// Whether `version` is semver: `X.Y.Z` of decimal numbers, optionally
/// followed by `-pre.release` and `+build` identifiers.
fn is_semver(version: &str) -> bool {
    let (rest, build) = version
        .split_once('+')
        .map_or((version, None), |(rest, build)| (rest, Some(build)));
    let (numbers, pre) = rest
        .split_once('-')
        .map_or((rest, None), |(numbers, pre)| (numbers, Some(pre)));
    let identifiers = |part: &str| {
        part.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    };
    let number = |number: &str| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit());
    numbers.split('.').count() == 3
        && numbers.split('.').all(number)
        && pre.is_none_or(identifiers)
        && build.is_none_or(identifiers)
}

/// Whether the first segment of the request `path` is
/// [`RouteSpec::RESERVED`], ignoring ASCII case and with `%2E` read as
/// `.`, so that a module that decodes or folds its paths cannot be
/// reached on its hook paths either.
fn is_reserved(path: &str) -> bool {
    let first = path
        .strip_prefix('/')
        .unwrap_or(path)
        .split('/')
        .next()
        .unwrap_or_default();
    let decoded = first.to_ascii_lowercase().replace("%2e", ".");
    decoded == RouteSpec::RESERVED
}

/// Whether `path` is a route path below a module's prefix (see
/// [`RouteSpec::path`]).
fn is_route_path(path: &str) -> bool {
    let Some(below) = path.strip_prefix('/') else {
        return false;
    };
    if below.split('/').next() == Some(RouteSpec::RESERVED) {
        return false;
    }
    below.is_empty()
        || below.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-._~{}".contains(c))
        })
}

/// Whether `name` is a lowercase word: `[a-z][a-z0-9_-]*`, at most 64
/// bytes.
fn is_word(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Whether `name` is a command name: at most four lowercase words
/// separated by single spaces (`tenant members add`; a module's commands look like native ones).
fn is_command_name(name: &str) -> bool {
    let words: Vec<&str> = name.split(' ').collect();
    name.len() <= 64 && words.len() <= 4 && words.iter().all(|word| is_word(word))
}

/// Whether `schema` can be a JSON Schema: an object or a boolean.
const fn is_schema(schema: &serde_json::Value) -> bool {
    matches!(
        schema,
        serde_json::Value::Object(_) | serde_json::Value::Bool(_)
    )
}

/// An MCP tool of a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// `name`: a lowercase word.
    pub name: String,
    /// `description`.
    pub description: String,
    /// `schema`: the JSON Schema of its arguments (an object or a
    /// boolean).
    pub schema: serde_json::Value,
}

/// A core hook a module subscribes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookName {
    /// `authenticate`.
    Authenticate,
    /// `authorize`.
    Authorize,
    /// `before_deploy`.
    BeforeDeploy,
    /// `after_deploy`.
    AfterDeploy,
    /// `domain_created`.
    DomainCreated,
    /// `domain_removed`.
    DomainRemoved,
    /// `gate`.
    Gate,
    /// `render_page`.
    RenderPage,
    /// `tenant_quotas`: a hook that answers
    /// data, like `render_page`.
    TenantQuotas,
    /// `project_tenant`: the tenant of a new
    /// project, a hook that answers data.
    ProjectTenant,
    /// `mcp_resource`: the protected resource of
    /// `/mcp`, a hook that answers data.
    McpResource,
    /// `verify_token`: who an OAuth access token
    /// stands for, a hook that answers data.
    VerifyToken,
    /// `web_panel`: the origin of the panel
    /// `liftoff-web`, a hook that answers data.
    WebPanel,
}

impl HookName {
    /// Every hook.
    pub const ALL: [Self; 13] = [
        Self::Authenticate,
        Self::Authorize,
        Self::BeforeDeploy,
        Self::AfterDeploy,
        Self::DomainCreated,
        Self::DomainRemoved,
        Self::Gate,
        Self::RenderPage,
        Self::TenantQuotas,
        Self::ProjectTenant,
        Self::McpResource,
        Self::VerifyToken,
        Self::WebPanel,
    ];

    /// Its name as a manifest lists it and `on-hook` receives it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authenticate => "authenticate",
            Self::Authorize => "authorize",
            Self::BeforeDeploy => "before_deploy",
            Self::AfterDeploy => "after_deploy",
            Self::DomainCreated => "domain_created",
            Self::DomainRemoved => "domain_removed",
            Self::Gate => "gate",
            Self::RenderPage => "render_page",
            Self::TenantQuotas => "tenant_quotas",
            Self::ProjectTenant => "project_tenant",
            Self::McpResource => "mcp_resource",
            Self::VerifyToken => "verify_token",
            Self::WebPanel => "web_panel",
        }
    }
}

/// What a forge module serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProviderSpec {
    /// `hosts`: the forge hosts it answers for (hosts without a port).
    pub hosts: Vec<String>,
    /// `oauth`: whether it connects users by OAuth.
    pub oauth: bool,
}

impl ModuleManifest {
    /// Parses the JSON a module's `manifest()` returned.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] of the field `manifest` naming what is
    /// wrong: JSON that does not parse, an unknown field at any depth, a
    /// missing `name` or `version`, a value of the wrong type, a `name` or
    /// a `requires` entry that is not a [`ModuleName`], or a value the
    /// module docs above refuse (version, route, `http_allow` entry, forge
    /// host, name, schema).
    pub fn parse(json: &str) -> Result<Self, ValidationError> {
        let raw: raw::Manifest = serde_json::from_str(json).map_err(|error| invalid(&error))?;
        raw.into_manifest()
    }
}

/// A refusal of the manifest for `why`.
fn invalid(why: &dyn core::fmt::Display) -> ValidationError {
    ValidationError::new("manifest", Problem::Invalid(why.to_string()))
}

/// The JSON shape of a manifest: strict at every depth
/// (`deny_unknown_fields`), validated into [`ModuleManifest`] at the
/// boundary.
mod raw {
    use serde::Deserialize;

    use super::{
        ArgKind, ArgSpec, CommandSpec, GitProviderSpec, HookName, HttpAllow, ModuleManifest,
        RouteSpec, ToolSpec, invalid, is_command_name, is_host, is_route_path, is_schema,
        is_semver, is_word,
    };
    use crate::domain::ModuleName;
    use crate::error::ValidationError;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Manifest {
        name: String,
        version: String,
        #[serde(default)]
        requires: Vec<String>,
        #[serde(default)]
        config_schema: Option<serde_json::Value>,
        #[serde(default)]
        http_allow: Vec<String>,
        #[serde(default)]
        commands: Vec<Command>,
        #[serde(default)]
        routes: Vec<Route>,
        #[serde(default)]
        tools: Vec<Tool>,
        #[serde(default)]
        migrations: u32,
        #[serde(default)]
        hooks: Vec<Hook>,
        #[serde(default)]
        replaces: Vec<String>,
        #[serde(default)]
        git_provider: Option<GitProvider>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Command {
        name: String,
        description: String,
        #[serde(default)]
        args: Vec<Arg>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Arg {
        name: String,
        #[serde(rename = "type")]
        kind: Kind,
        #[serde(default)]
        required: bool,
        #[serde(default)]
        hint: Option<String>,
    }

    #[derive(Deserialize, Clone, Copy)]
    #[serde(rename_all = "snake_case")]
    enum Kind {
        String,
        Integer,
        Boolean,
        Project,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Route {
        method: String,
        path: String,
        #[serde(default)]
        public: bool,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Tool {
        name: String,
        description: String,
        schema: serde_json::Value,
    }

    #[derive(Deserialize, Clone, Copy)]
    #[serde(rename_all = "snake_case")]
    enum Hook {
        Authenticate,
        Authorize,
        BeforeDeploy,
        AfterDeploy,
        DomainCreated,
        DomainRemoved,
        Gate,
        RenderPage,
        TenantQuotas,
        ProjectTenant,
        McpResource,
        VerifyToken,
        WebPanel,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitProvider {
        hosts: Vec<String>,
        #[serde(default)]
        oauth: bool,
    }

    /// `raw` as a [`ModuleName`], the refusal naming `field`.
    fn module_name(field: &str, raw: &str) -> Result<ModuleName, ValidationError> {
        ModuleName::new(raw)
            .map_err(|why| invalid(&format_args!("{field}: {raw:?}: {}", why.problem)))
    }

    /// `raw` when `valid`, else a refusal naming `field`.
    fn checked<T: core::fmt::Debug>(
        field: &str,
        raw: T,
        valid: bool,
    ) -> Result<T, ValidationError> {
        if valid {
            Ok(raw)
        } else {
            Err(invalid(&format_args!("{field}: {raw:?} is not valid")))
        }
    }

    /// `name` when it is a lowercase word.
    fn word(field: &str, name: String) -> Result<String, ValidationError> {
        let valid = is_word(&name);
        checked(field, name, valid)
    }

    /// `schema` when it can be a JSON Schema.
    fn schema(
        field: &str,
        schema: serde_json::Value,
    ) -> Result<serde_json::Value, ValidationError> {
        let valid = is_schema(&schema);
        checked(field, schema, valid)
    }

    /// `schema` when it can be a JSON Schema whose only secrets are
    /// top-level properties: those are the fields the core seals
    /// (REG-05), so a secret declared anywhere else would be stored in
    /// plain text.
    fn config_schema(schema: serde_json::Value) -> Result<serde_json::Value, ValidationError> {
        const FIELD: &str = "config_schema";
        let schema = self::schema(FIELD, schema)?;
        nested_secret(&schema, &mut Vec::new()).map_or(Ok(schema), |path| {
            Err(invalid(&format_args!(
                "{FIELD}: \"format\": \"secret\" at {path:?}; only a top-level property (/properties/<field>) can be a secret"
            )))
        })
    }

    /// The JSON pointer of the first subschema of `node` (at `path`) that
    /// is `"format": "secret"` but not a top-level property. The values
    /// of `const`, `enum`, `default` and `examples` are data, not
    /// subschemas, and are not walked.
    fn nested_secret(node: &serde_json::Value, path: &mut Vec<String>) -> Option<String> {
        match node {
            serde_json::Value::Object(object) => {
                let top_level =
                    matches!(path.as_slice(), [properties, _] if properties == "properties");
                if !top_level
                    && object.get("format").and_then(serde_json::Value::as_str) == Some("secret")
                {
                    return Some(pointer(path));
                }
                for (key, child) in object {
                    if matches!(key.as_str(), "const" | "enum" | "default" | "examples") {
                        continue;
                    }
                    path.push(key.clone());
                    let found = nested_secret(child, path);
                    let _key = path.pop();
                    if found.is_some() {
                        return found;
                    }
                }
                None
            }
            serde_json::Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    path.push(index.to_string());
                    let found = nested_secret(child, path);
                    let _index = path.pop();
                    if found.is_some() {
                        return found;
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// `path` as a JSON pointer (RFC 6901).
    fn pointer(path: &[String]) -> String {
        path.iter().fold(String::new(), |mut pointer, segment| {
            pointer.push('/');
            pointer.push_str(&segment.replace('~', "~0").replace('/', "~1"));
            pointer
        })
    }

    impl Manifest {
        pub(super) fn into_manifest(self) -> Result<ModuleManifest, ValidationError> {
            let version_ok = is_semver(&self.version);
            Ok(ModuleManifest {
                name: module_name("name", &self.name)?,
                version: checked("version", self.version, version_ok)?,
                requires: self
                    .requires
                    .iter()
                    .map(|required| module_name("requires", required))
                    .collect::<Result<_, _>>()?,
                config_schema: self.config_schema.map(config_schema).transpose()?,
                http_allow: self
                    .http_allow
                    .iter()
                    .map(|entry| HttpAllow::new(entry))
                    .collect::<Result<_, _>>()?,
                commands: self
                    .commands
                    .into_iter()
                    .map(Command::into_spec)
                    .collect::<Result<_, _>>()?,
                routes: self
                    .routes
                    .into_iter()
                    .map(Route::into_spec)
                    .collect::<Result<_, _>>()?,
                tools: self
                    .tools
                    .into_iter()
                    .map(Tool::into_spec)
                    .collect::<Result<_, _>>()?,
                migrations: self.migrations,
                hooks: self.hooks.into_iter().map(Hook::into_name).collect(),
                replaces: self
                    .replaces
                    .into_iter()
                    .map(|replaced| word("replaces", replaced))
                    .collect::<Result<_, _>>()?,
                git_provider: self.git_provider.map(GitProvider::into_spec).transpose()?,
            })
        }
    }

    impl Route {
        fn into_spec(self) -> Result<RouteSpec, ValidationError> {
            let method_ok = RouteSpec::METHODS.contains(&self.method.as_str());
            let path_ok = is_route_path(&self.path);
            Ok(RouteSpec {
                method: checked("routes.method", self.method, method_ok)?,
                path: checked("routes.path", self.path, path_ok)?,
                public: self.public,
            })
        }
    }

    impl Tool {
        fn into_spec(self) -> Result<ToolSpec, ValidationError> {
            Ok(ToolSpec {
                name: word("tools.name", self.name)?,
                description: self.description,
                schema: schema("tools.schema", self.schema)?,
            })
        }
    }

    impl GitProvider {
        fn into_spec(self) -> Result<GitProviderSpec, ValidationError> {
            Ok(GitProviderSpec {
                hosts: self
                    .hosts
                    .into_iter()
                    .map(|host| {
                        let valid = is_host(&host);
                        checked("git_provider.hosts", host, valid)
                            .map(|host| host.to_ascii_lowercase())
                    })
                    .collect::<Result<_, _>>()?,
                oauth: self.oauth,
            })
        }
    }

    impl Command {
        fn into_spec(self) -> Result<CommandSpec, ValidationError> {
            Ok(CommandSpec {
                name: {
                    let valid = is_command_name(&self.name);
                    checked("commands.name", self.name, valid)?
                },
                description: self.description,
                args: self
                    .args
                    .into_iter()
                    .map(|arg| {
                        Ok(ArgSpec {
                            name: word("commands.args.name", arg.name)?,
                            kind: match arg.kind {
                                Kind::String => ArgKind::String,
                                Kind::Integer => ArgKind::Integer,
                                Kind::Boolean => ArgKind::Boolean,
                                Kind::Project => ArgKind::Project,
                            },
                            required: arg.required,
                            hint: arg.hint,
                        })
                    })
                    .collect::<Result<_, ValidationError>>()?,
            })
        }
    }

    impl Hook {
        const fn into_name(self) -> HookName {
            match self {
                Self::Authenticate => HookName::Authenticate,
                Self::Authorize => HookName::Authorize,
                Self::BeforeDeploy => HookName::BeforeDeploy,
                Self::AfterDeploy => HookName::AfterDeploy,
                Self::DomainCreated => HookName::DomainCreated,
                Self::DomainRemoved => HookName::DomainRemoved,
                Self::Gate => HookName::Gate,
                Self::RenderPage => HookName::RenderPage,
                Self::TenantQuotas => HookName::TenantQuotas,
                Self::ProjectTenant => HookName::ProjectTenant,
                Self::McpResource => HookName::McpResource,
                Self::VerifyToken => HookName::VerifyToken,
                Self::WebPanel => HookName::WebPanel,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest with every field.
    const FULL: &str = r#"{
        "name": "allimports",
        "version": "0.1.0",
        "requires": ["oidc"],
        "config_schema": {"type": "object", "properties": {"greeting": {"type": "string"}}},
        "http_allow": ["127.0.0.1"],
        "commands": [{"name": "greet", "description": "Says hello",
                      "args": [{"name": "who", "type": "string", "required": true, "hint": "Who to greet"}]}],
        "routes": [{"method": "GET", "path": "/hello", "public": true}],
        "tools": [{"name": "core-call", "description": "Calls the core", "schema": {"type": "object"}}],
        "migrations": 1,
        "hooks": ["authorize", "before_deploy"],
        "replaces": ["login"],
        "git_provider": {"hosts": ["git.example.org"], "oauth": false}
    }"#;

    fn name(raw: &str) -> ModuleName {
        ModuleName::new(raw).unwrap()
    }

    fn refusal(json: &str) -> String {
        ModuleManifest::parse(json).unwrap_err().to_string()
    }

    mod parse {
        use super::*;

        // TC: MOD-06 (the documented manifest parses into its typed form)
        #[test]
        fn reads_every_field_when_manifest_is_full() {
            let manifest = ModuleManifest::parse(FULL).unwrap();
            assert_eq!(
                manifest,
                ModuleManifest {
                    name: name("allimports"),
                    version: "0.1.0".to_owned(),
                    requires: vec![name("oidc")],
                    config_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": {"greeting": {"type": "string"}}
                    })),
                    http_allow: vec![HttpAllow::new("127.0.0.1").unwrap()],
                    commands: vec![CommandSpec {
                        name: "greet".to_owned(),
                        description: "Says hello".to_owned(),
                        args: vec![ArgSpec {
                            name: "who".to_owned(),
                            kind: ArgKind::String,
                            required: true,
                            hint: Some("Who to greet".to_owned()),
                        }],
                    }],
                    routes: vec![RouteSpec {
                        method: "GET".to_owned(),
                        path: "/hello".to_owned(),
                        public: true,
                    }],
                    tools: vec![ToolSpec {
                        name: "core-call".to_owned(),
                        description: "Calls the core".to_owned(),
                        schema: serde_json::json!({"type": "object"}),
                    }],
                    migrations: 1,
                    hooks: vec![HookName::Authorize, HookName::BeforeDeploy],
                    replaces: vec!["login".to_owned()],
                    git_provider: Some(GitProviderSpec {
                        hosts: vec!["git.example.org".to_owned()],
                        oauth: false,
                    }),
                }
            );
        }

        // TC: MOD-06 (only `name` and `version` are required: the probe's
        // manifest of stays valid)
        #[test]
        fn leaves_lists_empty_when_only_name_version_and_migrations_given() {
            let manifest =
                ModuleManifest::parse(r#"{"name":"probe","version":"0.1.0","migrations":1}"#)
                    .unwrap();
            assert_eq!(
                manifest,
                ModuleManifest {
                    name: name("probe"),
                    version: "0.1.0".to_owned(),
                    requires: Vec::new(),
                    config_schema: None,
                    http_allow: Vec::new(),
                    commands: Vec::new(),
                    routes: Vec::new(),
                    tools: Vec::new(),
                    migrations: 1,
                    hooks: Vec::new(),
                    replaces: Vec::new(),
                    git_provider: None,
                }
            );
        }

        // TC: MOD-06 (route and argument flags default to `false`)
        #[test]
        fn defaults_flags_to_false_when_route_and_arg_omit_them() {
            let manifest = ModuleManifest::parse(
                r#"{"name":"probe","version":"0.1.0",
                    "routes":[{"method":"POST","path":"/items"}],
                    "commands":[{"name":"add","description":"Adds",
                                 "args":[{"name":"count","type":"integer"}]}]}"#,
            )
            .unwrap();
            assert_eq!(
                (
                    manifest.routes,
                    manifest.commands.first().map(|c| c.args.clone())
                ),
                (
                    vec![RouteSpec {
                        method: "POST".to_owned(),
                        path: "/items".to_owned(),
                        public: false,
                    }],
                    Some(vec![ArgSpec {
                        name: "count".to_owned(),
                        kind: ArgKind::Integer,
                        required: false,
                        hint: None,
                    }])
                )
            );
        }

        // TC: TEN-06 (an argument of type `project`,
        // which the CLI fills from the working tree)
        #[test]
        fn reads_project_arg_when_command_declares_one() {
            let manifest = ModuleManifest::parse(
                r#"{"name":"probe","version":"0.1.0",
                    "commands":[{"name":"guard","description":"Guards",
                                 "args":[{"name":"project","type":"project","required":true}]}]}"#,
            )
            .unwrap();
            assert_eq!(
                manifest
                    .commands
                    .first()
                    .and_then(|command| command.args.first())
                    .map(|arg| (arg.kind, arg.kind.as_str())),
                Some((ArgKind::Project, "project"))
            );
        }

        // TC: REG-08 (a module's command looks like a
        // native one, `lof tenant members add`, so its name may be several
        // lowercase words separated by single spaces)
        #[test]
        fn keeps_command_of_several_words_when_each_word_is_lowercase() {
            let manifest = ModuleManifest::parse(
                r#"{"name":"tenants","version":"0.1.0",
                    "commands":[{"name":"tenant members add","description":"Adds a member"},
                                {"name":"admin tenant set","description":"Sets quotas"}]}"#,
            )
            .unwrap();
            let names: Vec<String> = manifest
                .commands
                .iter()
                .map(|command| command.name.clone())
                .collect();
            assert_eq!(names, vec!["tenant members add", "admin tenant set"]);
        }

        // TC: MOD-06
        #[test]
        fn rejects_manifest_when_top_level_field_is_unknown() {
            let refused = refusal(r#"{"name":"probe","version":"0.1.0","colour":"blue"}"#);
            assert!(refused.contains("colour"), "{refused}");
        }

        // TC: MOD-06 (strict at every depth)
        #[test]
        fn rejects_manifest_when_route_field_is_unknown() {
            let refused = refusal(
                r#"{"name":"probe","version":"0.1.0",
                    "routes":[{"method":"GET","path":"/x","auth":"none"}]}"#,
            );
            assert!(refused.contains("auth"), "{refused}");
        }

        // TC: MOD-06 (strict at every depth)
        #[test]
        fn rejects_manifest_when_command_arg_field_is_unknown() {
            let refused = refusal(
                r#"{"name":"probe","version":"0.1.0",
                    "commands":[{"name":"add","description":"Adds",
                                 "args":[{"name":"n","type":"integer","default":3}]}]}"#,
            );
            assert!(refused.contains("default"), "{refused}");
        }

        // TC: MOD-06
        #[test]
        fn rejects_manifest_when_hook_is_unknown() {
            let refused = refusal(r#"{"name":"probe","version":"0.1.0","hooks":["on_boot"]}"#);
            assert!(refused.contains("on_boot"), "{refused}");
        }

        // TC: MOD-06 (the name is validated at the boundary)
        #[test]
        fn rejects_manifest_when_name_is_not_a_module_name() {
            assert!(ModuleManifest::parse(r#"{"name":"Probe!","version":"0.1.0"}"#).is_err());
        }

        // TC: MOD-06
        #[test]
        fn rejects_manifest_when_version_is_missing() {
            let refused = refusal(r#"{"name":"probe"}"#);
            assert!(refused.contains("version"), "{refused}");
        }

        /// A manifest of `name` and `version` with `extra` fields.
        fn with(extra: &str) -> String {
            format!(r#"{{"name":"probe","version":"0.1.0"{extra}}}"#)
        }

        // TC: MOD-06 (the version is `X.Y.Z`, validated at the boundary)
        #[test]
        fn rejects_manifest_when_version_is_not_semver() {
            for version in ["1.0", "v1.0.0", "1.0.0.0", "1.x.0", "", "1.0.0-"] {
                let json = format!(r#"{{"name":"probe","version":"{version}"}}"#);
                assert!(ModuleManifest::parse(&json).is_err(), "{version:?}");
            }
        }

        // TC: MOD-06 (a pre-release and build metadata are semver too)
        #[test]
        fn reads_version_when_it_has_pre_release_and_build() {
            let json = r#"{"name":"probe","version":"1.2.3-alpha.1+git.abc"}"#;
            assert_eq!(
                ModuleManifest::parse(json).map(|manifest| manifest.version),
                Ok("1.2.3-alpha.1+git.abc".to_owned())
            );
        }

        // TC: MOD-06
        #[test]
        fn rejects_manifest_when_route_method_is_unknown() {
            for method in ["get", "FETCH", "", "GET "] {
                let json = with(&format!(
                    r#","routes":[{{"method":"{method}","path":"/x"}}]"#
                ));
                assert!(ModuleManifest::parse(&json).is_err(), "{method:?}");
            }
        }

        // TC: MOD-06 (a path below the module's prefix: no escape, no query)
        #[test]
        fn rejects_manifest_when_route_path_is_not_below_prefix() {
            for path in [
                "x",
                "",
                "/../auth",
                "/a/./b",
                "/a//b",
                "/a?b=1",
                "/a#b",
                "/a b",
                "/.hook/authenticate",
                "/.hook",
            ] {
                let json = with(&format!(
                    r#","routes":[{{"method":"GET","path":"{path}"}}]"#
                ));
                assert!(ModuleManifest::parse(&json).is_err(), "{path:?}");
            }
        }

        // TC: MOD-06
        #[test]
        fn reads_route_path_when_it_has_segments_and_parameters() {
            let json = with(r#","routes":[{"method":"DELETE","path":"/items/{id}/tags"}]"#);
            assert_eq!(
                ModuleManifest::parse(&json).map(|manifest| manifest.routes),
                Ok(vec![RouteSpec {
                    method: "DELETE".to_owned(),
                    path: "/items/{id}/tags".to_owned(),
                    public: false,
                }])
            );
        }

        // TC: HOOK-03 (a parameter segment matches any
        // one segment)
        #[test]
        fn matches_request_when_parameter_segment_takes_any_segment() {
            let route = RouteSpec {
                method: "DELETE".to_owned(),
                path: "/items/{id}/tags".to_owned(),
                public: false,
            };
            assert_eq!(
                [
                    route.matches("DELETE", "/items/42/tags"),
                    route.matches("DELETE", "/items/{id}/tags"),
                    route.matches("GET", "/items/42/tags"),
                    route.matches("DELETE", "/items//tags"),
                    route.matches("DELETE", "/items/42"),
                    route.matches("DELETE", "/items/42/tags/x"),
                ],
                [true, true, false, false, false, false]
            );
        }

        // TC: HOOK-06 (a request below the reserved
        // `/.hook/` paths matches no route, so the host's own
        // `authenticate` and `render_page` calls cannot be made from
        // outside through a parameter route)
        #[test]
        fn matches_no_request_when_path_is_below_reserved_segment() {
            let route = RouteSpec {
                method: "POST".to_owned(),
                path: "/{a}/{b}".to_owned(),
                public: true,
            };
            assert_eq!(
                [
                    route.matches("POST", "/.hook/authenticate"),
                    route.matches("POST", "/.HOOK/authenticate"),
                    route.matches("POST", "/%2Ehook/authenticate"),
                    route.matches("POST", "/%2ehook/authenticate"),
                    route.matches("POST", "/device/authenticate"),
                    route.matches("POST", "/device/.hook"),
                ],
                [false, false, false, false, true, true]
            );
        }

        // TC: MOD-06, MOD-07 (an entry is a host, or a host and a port)
        #[test]
        fn rejects_manifest_when_http_allow_entry_is_not_a_host() {
            for entry in [
                "",
                "*",
                "*.example.org",
                "https://api.example.org",
                "api.example.org/v1",
                "api.example.org:",
                "api.example.org:0",
                "api.example.org:99999",
                "-api.example.org",
                "api..example.org",
                "api_example.org",
                "[::1",
            ] {
                let json = with(&format!(r#","http_allow":["{entry}"]"#));
                assert!(ModuleManifest::parse(&json).is_err(), "{entry:?}");
            }
        }

        // TC: MOD-07
        #[test]
        fn reads_http_allow_when_entries_are_hosts_and_ports() {
            let json = with(
                r#","http_allow":["API.Example.org","api.example.org:8443","10.0.0.1","[::1]:8080","::1"]"#,
            );
            let allowed = ModuleManifest::parse(&json).map(|manifest| {
                manifest
                    .http_allow
                    .iter()
                    .map(|entry| (entry.host().to_owned(), entry.port(), entry.to_string()))
                    .collect::<Vec<_>>()
            });
            assert_eq!(
                allowed,
                Ok(vec![
                    (
                        "api.example.org".to_owned(),
                        None,
                        "api.example.org".to_owned()
                    ),
                    (
                        "api.example.org".to_owned(),
                        Some(8443),
                        "api.example.org:8443".to_owned()
                    ),
                    ("10.0.0.1".to_owned(), None, "10.0.0.1".to_owned()),
                    ("::1".to_owned(), Some(8080), "[::1]:8080".to_owned()),
                    ("::1".to_owned(), None, "[::1]".to_owned()),
                ])
            );
        }

        // TC: MOD-06
        #[test]
        fn rejects_manifest_when_git_host_has_a_port() {
            let json = with(r#","git_provider":{"hosts":["git.example.org:22"]}"#);
            assert!(ModuleManifest::parse(&json).is_err());
        }

        // TC: MOD-06 (commands, arguments, tools and replaced commands are
        // lowercase words)
        #[test]
        fn rejects_manifest_when_a_name_is_not_a_word() {
            for extra in [
                r#","commands":[{"name":"Greet","description":"d"}]"#,
                r#","commands":[{"name":"greet","description":"d","args":[{"name":"w ho","type":"string"}]}]"#,
                r#","tools":[{"name":"","description":"d","schema":{}}]"#,
                r#","replaces":["log in"]"#,
            ] {
                assert!(ModuleManifest::parse(&with(extra)).is_err(), "{extra}");
            }
        }

        // TC: MOD-06 (a schema is a JSON Schema object or boolean; compiling it is left to installation)
        #[test]
        fn rejects_manifest_when_a_schema_is_not_a_schema() {
            for extra in [
                r#","config_schema":5"#,
                r#","config_schema":"object""#,
                r#","tools":[{"name":"t","description":"d","schema":[]}]"#,
            ] {
                assert!(ModuleManifest::parse(&with(extra)).is_err(), "{extra}");
            }
        }

        // TC: REG-05 (only a top-level property is sealed, so a secret
        // anywhere else is refused rather than stored in plain text)
        #[test]
        fn rejects_manifest_naming_path_when_secret_is_not_top_level_property() {
            for (extra, path) in [
                (
                    r#","config_schema":{"properties":{"cloudflare":{"type":"object","properties":{"token":{"type":"string","format":"secret"}}}}}"#,
                    "/properties/cloudflare/properties/token",
                ),
                (
                    r##","config_schema":{"properties":{"t":{"$ref":"#/$defs/s"}},"$defs":{"s":{"format":"secret"}}}"##,
                    "/$defs/s",
                ),
                (
                    r#","config_schema":{"additionalProperties":{"format":"secret"}}"#,
                    "/additionalProperties",
                ),
                (
                    r#","config_schema":{"properties":{"t":{"anyOf":[{"format":"secret"}]}}}"#,
                    "/properties/t/anyOf/0",
                ),
                (r#","config_schema":{"format":"secret"}"#, "\"\""),
            ] {
                let refused = ModuleManifest::parse(&with(extra));
                assert!(
                    refused
                        .as_ref()
                        .is_err_and(|why| why.to_string().contains(path)),
                    "{extra}: {refused:?}"
                );
            }
        }

        // TC: REG-05 (a top-level secret property, and `format` as a
        // property name or a value, are fine)
        #[test]
        fn accepts_manifest_when_secret_is_top_level_property() {
            let extra = r#","config_schema":{"properties":{"token":{"type":"string","format":"secret"},"format":{"type":"string","default":"secret","enum":[{"format":"secret"}]}}}"#;
            assert!(
                ModuleManifest::parse(&with(extra)).is_ok(),
                "{:?}",
                ModuleManifest::parse(&with(extra))
            );
        }

        // TC: MOD-06
        #[test]
        fn names_manifest_field_when_refused() {
            let refused = ModuleManifest::parse("not json").unwrap_err();
            assert_eq!(refused.field, "manifest", "{refused}");
        }
    }
    mod allows {
        use super::*;

        fn entry(raw: &str) -> HttpAllow {
            HttpAllow::new(raw).unwrap()
        }

        // TC: MOD-07 (an entry without a port allows the scheme's own)
        #[test]
        fn allows_default_port_only_when_entry_has_no_port() {
            let allowed = entry("api.example.org");
            assert_eq!(
                (
                    allowed.allows("api.example.org", 443, 443),
                    allowed.allows("api.example.org", 80, 80),
                    allowed.allows("api.example.org", 22, 443),
                    allowed.allows("api.example.org", 443, 80),
                ),
                (true, true, false, false)
            );
        }

        // TC: MOD-07
        #[test]
        fn allows_listed_port_only_when_entry_has_port() {
            let allowed = entry("api.example.org:8443");
            assert_eq!(
                (
                    allowed.allows("api.example.org", 8443, 443),
                    allowed.allows("api.example.org", 443, 443),
                ),
                (true, false)
            );
        }

        // TC: MOD-07
        #[test]
        fn matches_host_when_it_differs_in_case_brackets_or_trailing_dot() {
            assert_eq!(
                (
                    entry("api.example.org").allows("API.example.org.", 443, 443),
                    entry("[::1]:8080").allows("[::1]", 8080, 80),
                    entry("api.example.org").allows("example.org", 443, 443),
                    entry("api.example.org").allows("", 443, 443),
                ),
                (true, true, false, false)
            );
        }
    }
    mod hook_name {
        use super::*;

        // TC: MOD-06 (`on-hook` gets the name the manifest lists)
        #[test]
        fn names_hook_as_manifest_lists_it_when_written() {
            for hook in HookName::ALL {
                let json = format!(
                    r#"{{"name":"probe","version":"0.1.0","hooks":["{}"]}}"#,
                    hook.as_str()
                );
                assert_eq!(
                    ModuleManifest::parse(&json).map(|manifest| manifest.hooks),
                    Ok(vec![hook]),
                    "{hook:?}"
                );
            }
        }
    }
}
