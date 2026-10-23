//! Each domain record as a row of the schema in `migrations/0001_init.sql`,
//! and back.

use liftoff_core::Error;
use liftoff_core::domain::{
    Artifact, ArtifactFile, ArtifactId, ArtifactPath, ArtifactVersion, Audit, AuditId, Binding,
    BindingId, BlobHash, Branch, CachedTemplate, ComponentHash, ContainerPort, Cpus,
    CredentialKind, Deploy, DeployId, DeployNumber, DeploySource, DeployStatus, DeployToken,
    DeployTokenId, Domain, DomainId, DomainKind, EnvKind, EnvName, EnvState, Environment,
    EnvironmentId, GateCode, GateSession, GitConnection, GitConnectionId, GitProvider, Host,
    HostPort, ImageTag, Limits, LinkTarget, Login, MemoryMb, Pin, Project, ProjectId, ProjectName,
    RepoRef, ReturnPath, ScopeKind, SealedEnv, Secret, SecretId, SecretKey, SecretScope,
    SecretSyncRecord, SecretVersion, ServiceName, Session, SessionId, SessionKind, SyncTarget,
    TemplateRef, TenantId, Trigger, UpdatedBy, User, UserId, UserRole, UserSource, VersionNumber,
    Visibility,
};
use liftoff_core::domain::{
    ConfigValue, InstalledModule, ModuleConfigRecord, ModuleName, ModuleVersion, OneTimeLink,
};
use liftoff_core::error::{Problem, ValidationError};
use rusqlite::Row;
use rusqlite::types::Value;

use crate::sql::{
    FromRow, Record, ToRecord, blob, flag, int, number, number_opt, opt_int, opt_text, opt_time,
    parsed, parsed_opt, text, time, timestamp, timestamp_opt, token_hash,
};

// ---- encodings of values that are not one domain type ------------------------

/// `triggered_by`: `user:<id>` or `token:<id>`.
pub(crate) fn trigger_text(trigger: Trigger) -> String {
    match trigger {
        Trigger::User(id) => format!("user:{id}"),
        Trigger::Token(id) => format!("token:{id}"),
    }
}

fn parse_trigger(raw: &str) -> Result<Trigger, ValidationError> {
    if let Some(id) = raw.strip_prefix("user:") {
        return UserId::parse(id).map(Trigger::User);
    }
    if let Some(id) = raw.strip_prefix("token:") {
        return DeployTokenId::parse(id).map(Trigger::Token);
    }
    Err(ValidationError::new(
        "triggered_by",
        Problem::Pattern {
            expected: "user:<id> or token:<id>",
        },
    ))
}

/// `updated_by`: a user id, `ci` or `import`.
pub(crate) fn updated_by_text(by: UpdatedBy) -> String {
    match by {
        UpdatedBy::User(id) => id.to_string(),
        UpdatedBy::Ci => "ci".to_owned(),
        UpdatedBy::Import => "import".to_owned(),
    }
}

fn parse_updated_by(raw: &str) -> Result<UpdatedBy, ValidationError> {
    match raw {
        "ci" => Ok(UpdatedBy::Ci),
        "import" => Ok(UpdatedBy::Import),
        id => UserId::parse(id).map(UpdatedBy::User),
    }
}

/// `secrets.scope` and `secrets.scope_id` of `scope`.
pub(crate) fn scope_args(scope: &SecretScope) -> [Value; 2] {
    [text(scope.kind()), text(scope.id())]
}

fn secret_scope(row: &Row<'_>) -> rusqlite::Result<SecretScope> {
    let kind = parsed(row, "scope", ScopeKind::parse)?;
    let unknown = || {
        ValidationError::new(
            "scope",
            Problem::Pattern {
                expected: "tenant, project or environment",
            },
        )
    };
    parsed(row, "scope_id", |raw| match kind {
        ScopeKind::Tenant => TenantId::parse(raw).map(SecretScope::Tenant),
        ScopeKind::Project => ProjectId::parse(raw).map(SecretScope::Project),
        ScopeKind::Environment => EnvironmentId::parse(raw).map(SecretScope::Environment),
        _ => Err(unknown()),
    })
}

fn sealed(row: &Row<'_>, column: &str) -> rusqlite::Result<SealedEnv> {
    row.get(column).map(SealedEnv::new)
}

// ---- users and sessions --------------------------------------------------------

impl ToRecord for User {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "user",
            table: "users",
            columns: vec![
                ("id", text(self.id)),
                ("login", text(&self.login)),
                ("email", text(&self.email)),
                ("role", text(self.role)),
                ("source", text(&self.source)),
                ("subject", opt_text(self.subject.as_ref())),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for User {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", UserId::parse)?,
            login: parsed(row, "login", Login::new)?,
            email: row.get("email")?,
            role: parsed(row, "role", UserRole::parse)?,
            source: parsed(row, "source", UserSource::parse)?,
            subject: row.get("subject")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

impl ToRecord for Session {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "session",
            table: "sessions",
            columns: vec![
                ("id", text(self.id)),
                ("user_id", text(self.user_id)),
                ("token_hash", blob(&self.token_hash)),
                ("name", text(&self.name)),
                ("kind", text(self.kind)),
                ("expires_at", time(self.expires_at)?),
                ("last_used_at", time(self.last_used_at)?),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for Session {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", SessionId::parse)?,
            user_id: parsed(row, "user_id", UserId::parse)?,
            token_hash: token_hash(row, "token_hash")?,
            name: row.get("name")?,
            kind: parsed(row, "kind", SessionKind::parse)?,
            expires_at: timestamp(row, "expires_at")?,
            last_used_at: timestamp(row, "last_used_at")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

impl ToRecord for GateCode {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "gate code",
            table: "gate_codes",
            columns: vec![
                ("code_hash", blob(&self.code_hash)),
                ("session_id", text(self.session_id)),
                ("user_id", text(self.user_id)),
                ("host", text(&self.host)),
                ("rd", text(&self.rd)),
                ("expires_at", time(self.expires_at)?),
            ],
        })
    }
}

impl FromRow for GateCode {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            code_hash: token_hash(row, "code_hash")?,
            session_id: parsed(row, "session_id", SessionId::parse)?,
            user_id: parsed(row, "user_id", UserId::parse)?,
            host: parsed(row, "host", Host::new)?,
            rd: parsed(row, "rd", ReturnPath::new)?,
            expires_at: timestamp(row, "expires_at")?,
        })
    }
}

impl ToRecord for GateSession {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "gate session",
            table: "gate_sessions",
            columns: vec![
                ("token_hash", blob(&self.token_hash)),
                ("session_id", text(self.session_id)),
                ("user_id", text(self.user_id)),
                ("host", text(&self.host)),
                ("expires_at", time(self.expires_at)?),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for GateSession {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            token_hash: token_hash(row, "token_hash")?,
            session_id: parsed(row, "session_id", SessionId::parse)?,
            user_id: parsed(row, "user_id", UserId::parse)?,
            host: parsed(row, "host", Host::new)?,
            expires_at: timestamp(row, "expires_at")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

impl ToRecord for OneTimeLink {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "one-time link",
            table: "one_time_links",
            columns: vec![
                ("token_hash", blob(&self.token_hash)),
                ("user_id", text(self.user_id)),
                ("target", Value::Text(self.target.as_stored())),
                ("expires_at", time(self.expires_at)?),
                ("used_at", opt_time(self.used_at)?),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for OneTimeLink {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            token_hash: token_hash(row, "token_hash")?,
            user_id: parsed(row, "user_id", UserId::parse)?,
            target: parsed(row, "target", LinkTarget::parse)?,
            expires_at: timestamp(row, "expires_at")?,
            used_at: timestamp_opt(row, "used_at")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

impl ToRecord for GitConnection {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "git connection",
            table: "git_connections",
            columns: vec![
                ("id", text(self.id)),
                ("user_id", text(self.user_id)),
                ("provider", text(self.provider)),
                ("host", text(&self.host)),
                ("account_login", text(&self.account_login)),
                ("credential", text(self.credential.as_str())),
                ("kind", text(self.kind)),
                ("expires_at", opt_time(self.expires_at)?),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for GitConnection {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", GitConnectionId::parse)?,
            user_id: parsed(row, "user_id", UserId::parse)?,
            provider: parsed(row, "provider", GitProvider::parse)?,
            host: parsed(row, "host", Host::new)?,
            account_login: parsed(row, "account_login", Login::new)?,
            credential: sealed(row, "credential")?,
            kind: parsed(row, "kind", CredentialKind::parse)?,
            expires_at: timestamp_opt(row, "expires_at")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

// ---- template cache ------------------------------------------------------------

impl ToRecord for CachedTemplate {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "cached template",
            table: "cached_templates",
            columns: vec![
                ("user_id", text(self.user_id)),
                ("host", text(&self.host)),
                ("repo", text(&self.repo)),
                ("manifest", opt_text(self.manifest.as_ref())),
                ("fetched_at", time(self.fetched_at)?),
            ],
        })
    }
}

impl FromRow for CachedTemplate {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            user_id: parsed(row, "user_id", UserId::parse)?,
            host: parsed(row, "host", Host::new)?,
            repo: parsed(row, "repo", RepoRef::new)?,
            manifest: row.get("manifest")?,
            fetched_at: timestamp(row, "fetched_at")?,
        })
    }
}

// ---- projects and environments -----------------------------------------------

impl ToRecord for Project {
    fn record(&self) -> Result<Record, Error> {
        let template = self.template.as_ref();
        Ok(Record {
            what: "project",
            table: "projects",
            columns: vec![
                ("id", text(self.id)),
                ("tenant_id", opt_text(self.tenant_id)),
                ("name", text(&self.name)),
                ("git_host", text(&self.git_host)),
                ("repo", text(&self.repo)),
                ("manifest", opt_text(self.manifest.as_ref())),
                ("template_group", opt_text(template.map(|t| &t.group))),
                ("template_name", opt_text(template.map(|t| &t.name))),
                ("created_by", text(self.created_by)),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for Project {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let group: Option<String> = row.get("template_group")?;
        let name: Option<String> = row.get("template_name")?;
        Ok(Self {
            id: parsed(row, "id", ProjectId::parse)?,
            tenant_id: parsed_opt(row, "tenant_id", TenantId::parse)?,
            name: parsed(row, "name", ProjectName::new)?,
            git_host: parsed(row, "git_host", Host::new)?,
            repo: parsed(row, "repo", RepoRef::new)?,
            manifest: row.get("manifest")?,
            template: group
                .zip(name)
                .map(|(group, name)| TemplateRef { group, name }),
            created_by: parsed(row, "created_by", UserId::parse)?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

impl ToRecord for Environment {
    fn record(&self) -> Result<Record, Error> {
        let pin = self.pin.as_ref();
        Ok(Record {
            what: "environment",
            table: "environments",
            columns: vec![
                ("id", text(self.id)),
                ("project_id", text(self.project_id)),
                ("name", text(&self.name)),
                ("kind", text(self.kind)),
                ("branch", opt_text(pin.map(|p| &p.branch))),
                ("pinned_by", opt_text(pin.map(|p| p.pinned_by))),
                ("owner_user_id", opt_text(self.owner_user_id)),
                ("cpus_hundredths", int(self.limits.cpus.hundredths())),
                ("memory_mb", int(self.limits.memory_mb.get())),
                ("state", text(self.state)),
                ("current_deploy_id", opt_text(self.current_deploy_id)),
                ("visibility", text(self.visibility)),
                ("protected", flag(self.protected)),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for Environment {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let branch = parsed_opt(row, "branch", Branch::new)?;
        let pinned_by = parsed_opt(row, "pinned_by", UserId::parse)?;
        Ok(Self {
            id: parsed(row, "id", EnvironmentId::parse)?,
            project_id: parsed(row, "project_id", ProjectId::parse)?,
            name: parsed(row, "name", EnvName::new)?,
            kind: parsed(row, "kind", EnvKind::parse)?,
            pin: branch
                .zip(pinned_by)
                .map(|(branch, pinned_by)| Pin { branch, pinned_by }),
            owner_user_id: parsed_opt(row, "owner_user_id", UserId::parse)?,
            limits: Limits {
                cpus: number(row, "cpus_hundredths", Cpus::from_hundredths)?,
                memory_mb: number(row, "memory_mb", MemoryMb::new)?,
            },
            state: parsed(row, "state", EnvState::parse)?,
            current_deploy_id: parsed_opt(row, "current_deploy_id", DeployId::parse)?,
            visibility: parsed(row, "visibility", Visibility::parse)?,
            protected: row.get("protected")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

// ---- domains and bindings ------------------------------------------------------

impl ToRecord for Domain {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "domain",
            table: "domains",
            columns: vec![
                ("id", text(self.id)),
                ("tenant_id", opt_text(self.tenant_id)),
                ("project_id", text(self.project_id)),
                ("host", text(&self.host)),
                ("kind", text(self.kind)),
                ("verified_at", opt_time(self.verified_at)?),
                ("record_lost_at", opt_time(self.record_lost_at)?),
                ("proxied", flag(self.proxied)),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for Domain {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", DomainId::parse)?,
            tenant_id: parsed_opt(row, "tenant_id", TenantId::parse)?,
            project_id: parsed(row, "project_id", ProjectId::parse)?,
            host: parsed(row, "host", Host::new)?,
            kind: parsed(row, "kind", DomainKind::parse)?,
            verified_at: timestamp_opt(row, "verified_at")?,
            record_lost_at: timestamp_opt(row, "record_lost_at")?,
            proxied: row.get("proxied")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

impl ToRecord for Binding {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "binding",
            table: "bindings",
            columns: vec![
                ("id", text(self.id)),
                ("environment_id", text(self.environment_id)),
                ("domain_id", text(self.domain_id)),
                ("service", text(&self.service)),
                ("container_port", int(self.container_port.get())),
                ("host_port", int(self.host_port.get())),
            ],
        })
    }
}

impl FromRow for Binding {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", BindingId::parse)?,
            environment_id: parsed(row, "environment_id", EnvironmentId::parse)?,
            domain_id: parsed(row, "domain_id", DomainId::parse)?,
            service: parsed(row, "service", ServiceName::new)?,
            container_port: number(row, "container_port", ContainerPort::new)?,
            host_port: number(row, "host_port", HostPort::new)?,
        })
    }
}

// ---- deploys and tokens --------------------------------------------------------

impl ToRecord for Deploy {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "deploy",
            table: "deploys",
            columns: vec![
                ("id", text(self.id)),
                ("environment_id", text(self.environment_id)),
                ("number", int(self.number.get())),
                ("source", text(self.source)),
                ("ref", opt_text(self.git_ref.as_ref())),
                ("tag", text(&self.tag)),
                ("compose", text(&self.compose)),
                ("component_hash", opt_text(self.component_hash.as_ref())),
                ("status", text(self.status)),
                ("log", text(&self.log)),
                ("error", opt_text(self.error.as_ref())),
                (
                    "rolled_back_to",
                    opt_int(self.rolled_back_to.map(DeployNumber::get)),
                ),
                ("triggered_by", Value::Text(trigger_text(self.triggered_by))),
                ("started_at", opt_time(self.started_at)?),
                ("finished_at", opt_time(self.finished_at)?),
            ],
        })
    }
}

impl FromRow for Deploy {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", DeployId::parse)?,
            environment_id: parsed(row, "environment_id", EnvironmentId::parse)?,
            number: number(row, "number", DeployNumber::new)?,
            source: parsed(row, "source", DeploySource::parse)?,
            git_ref: parsed_opt(row, "ref", Branch::new)?,
            tag: parsed(row, "tag", ImageTag::new)?,
            compose: row.get("compose")?,
            component_hash: parsed_opt(row, "component_hash", ComponentHash::parse)?,
            status: parsed(row, "status", DeployStatus::parse)?,
            log: row.get("log")?,
            error: row.get("error")?,
            rolled_back_to: number_opt(row, "rolled_back_to", DeployNumber::new)?,
            triggered_by: parsed(row, "triggered_by", parse_trigger)?,
            started_at: timestamp_opt(row, "started_at")?,
            finished_at: timestamp_opt(row, "finished_at")?,
        })
    }
}

impl ToRecord for DeployToken {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "deploy token",
            table: "deploy_tokens",
            columns: vec![
                ("id", text(self.id)),
                ("project_id", text(self.project_id)),
                ("token_hash", blob(&self.token_hash)),
                ("created_by", text(self.created_by)),
                ("created_at", time(self.created_at)?),
                ("revoked_at", opt_time(self.revoked_at)?),
            ],
        })
    }
}

impl FromRow for DeployToken {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", DeployTokenId::parse)?,
            project_id: parsed(row, "project_id", ProjectId::parse)?,
            token_hash: token_hash(row, "token_hash")?,
            created_by: parsed(row, "created_by", UserId::parse)?,
            created_at: timestamp(row, "created_at")?,
            revoked_at: timestamp_opt(row, "revoked_at")?,
        })
    }
}

// ---- secrets and audit -----------------------------------------------------------

impl ToRecord for Secret {
    fn record(&self) -> Result<Record, Error> {
        let [scope, scope_id] = scope_args(&self.scope);
        Ok(Record {
            what: "secret",
            table: "secrets",
            columns: vec![
                ("id", text(self.id)),
                ("scope", scope),
                ("scope_id", scope_id),
                ("key", text(&self.key)),
                ("value", text(self.value.as_str())),
                ("version", int(self.version.get())),
                ("ci", flag(self.ci)),
                ("updated_by", Value::Text(updated_by_text(self.updated_by))),
                ("updated_at", time(self.updated_at)?),
                ("created_at", time(self.created_at)?),
            ],
        })
    }
}

impl FromRow for Secret {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", SecretId::parse)?,
            scope: secret_scope(row)?,
            key: parsed(row, "key", SecretKey::new)?,
            value: sealed(row, "value")?,
            version: number(row, "version", VersionNumber::new)?,
            ci: row.get("ci")?,
            updated_by: parsed(row, "updated_by", parse_updated_by)?,
            updated_at: timestamp(row, "updated_at")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}

/// The history row of `secret`'s current version.
pub(crate) fn version_record(secret: &Secret) -> Result<Record, Error> {
    Ok(Record {
        what: "secret version",
        table: "secret_versions",
        columns: vec![
            ("secret_id", text(secret.id)),
            ("version", int(secret.version.get())),
            ("value", text(secret.value.as_str())),
            (
                "updated_by",
                Value::Text(updated_by_text(secret.updated_by)),
            ),
            ("updated_at", time(secret.updated_at)?),
        ],
    })
}

impl FromRow for SecretVersion {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            secret_id: parsed(row, "secret_id", SecretId::parse)?,
            version: number(row, "version", VersionNumber::new)?,
            value: sealed(row, "value")?,
            updated_by: parsed(row, "updated_by", parse_updated_by)?,
            updated_at: timestamp(row, "updated_at")?,
        })
    }
}

// ---- secret syncs ---------------------------------------------------------------

impl ToRecord for SecretSyncRecord {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "secret sync",
            table: "secret_syncs",
            columns: vec![
                ("secret_id", text(self.secret_id)),
                ("target", Value::Text(self.target.as_str().to_owned())),
                ("version", opt_int(self.version.map(VersionNumber::get))),
                ("synced_at", opt_time(self.synced_at)?),
                ("error", opt_text(self.error.as_ref())),
            ],
        })
    }
}

impl FromRow for SecretSyncRecord {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            secret_id: parsed(row, "secret_id", SecretId::parse)?,
            target: parsed(row, "target", SyncTarget::parse)?,
            version: number_opt(row, "version", VersionNumber::new)?,
            synced_at: timestamp_opt(row, "synced_at")?,
            error: row.get("error")?,
        })
    }
}

impl ToRecord for Audit {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "audit entry",
            table: "audit",
            columns: vec![
                ("id", text(self.id)),
                ("actor_user_id", opt_text(self.actor_user_id)),
                ("action", text(&self.action)),
                ("resource", text(&self.resource)),
                ("at", time(self.at)?),
            ],
        })
    }
}

impl FromRow for Audit {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", AuditId::parse)?,
            actor_user_id: parsed_opt(row, "actor_user_id", UserId::parse)?,
            action: row.get("action")?,
            resource: row.get("resource")?,
            at: timestamp(row, "at")?,
        })
    }
}

// ---- modules ---------------------------------------------------------------

/// `modules.config`: by field, `{"plain": <value>}` or `{"sealed":
/// "<ciphertext>"}` (REG-05).
fn config_text(config: &ModuleConfigRecord) -> String {
    let fields: serde_json::Map<String, serde_json::Value> = config
        .iter()
        .map(|(field, value)| {
            let encoded = match value {
                ConfigValue::Plain(json) => serde_json::json!({ "plain": json }),
                ConfigValue::Sealed(sealed) => serde_json::json!({ "sealed": sealed.as_str() }),
            };
            (field.clone(), encoded)
        })
        .collect();
    serde_json::Value::Object(fields).to_string()
}

fn parse_config(raw: &str) -> Result<ModuleConfigRecord, ValidationError> {
    let corrupt = || {
        ValidationError::new(
            "config",
            Problem::Invalid("not a module configuration".to_owned()),
        )
    };
    let fields: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(raw).map_err(|_unparsed| corrupt())?;
    fields
        .into_iter()
        .map(|(field, encoded)| {
            let value = match (encoded.get("plain"), encoded.get("sealed")) {
                (Some(json), None) => ConfigValue::Plain(json.clone()),
                (None, Some(serde_json::Value::String(sealed))) => {
                    ConfigValue::Sealed(SealedEnv::new(sealed.clone()))
                }
                _ => return Err(corrupt()),
            };
            Ok((field, value))
        })
        .collect()
}

impl ToRecord for InstalledModule {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "module",
            table: "modules",
            columns: vec![
                ("name", text(&self.name)),
                ("version", text(&self.version)),
                ("sha256", text(&self.sha256)),
                ("enabled", flag(self.enabled)),
                ("config", text(config_text(&self.config))),
                ("error", opt_text(self.error.as_ref())),
                ("installed_at", time(self.installed_at)?),
            ],
        })
    }
}

impl FromRow for InstalledModule {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            name: parsed(row, "name", ModuleName::new)?,
            version: parsed(row, "version", ModuleVersion::new)?,
            sha256: row.get("sha256")?,
            enabled: row.get("enabled")?,
            config: parsed(row, "config", parse_config)?,
            error: row.get("error")?,
            installed_at: timestamp(row, "installed_at")?,
        })
    }
}

// ---- artifacts --------------------------------------

impl ToRecord for Artifact {
    fn record(&self) -> Result<Record, Error> {
        Ok(Record {
            what: "artifact",
            table: "artifacts",
            columns: vec![
                ("id", text(self.id)),
                ("owner_user_id", text(self.owner_user_id)),
                ("title", text(&self.title)),
                ("visibility", text(self.visibility)),
                ("current_version", int(self.current_version.get())),
                ("expires_at", opt_time(self.expires_at)?),
                ("created_at", time(self.created_at)?),
                ("updated_at", time(self.updated_at)?),
            ],
        })
    }
}

impl FromRow for Artifact {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: parsed(row, "id", ArtifactId::parse)?,
            owner_user_id: parsed(row, "owner_user_id", UserId::parse)?,
            title: row.get("title")?,
            visibility: parsed(row, "visibility", Visibility::parse)?,
            current_version: number(row, "current_version", VersionNumber::new)?,
            expires_at: timestamp_opt(row, "expires_at")?,
            created_at: timestamp(row, "created_at")?,
            updated_at: timestamp(row, "updated_at")?,
        })
    }
}

/// The row of a version (`artifact_versions`) and the rows of its files
/// (`artifact_files`, by position).
pub(crate) fn artifact_version_records(
    version: &ArtifactVersion,
) -> Result<(Record, Vec<Record>), Error> {
    let size = |bytes: u64| {
        i64::try_from(bytes).map_err(|_too_big| Error::internal_msg("artifact size beyond i64"))
    };
    let row = Record {
        what: "artifact version",
        table: "artifact_versions",
        columns: vec![
            ("artifact_id", text(version.artifact_id)),
            ("number", int(version.number.get())),
            ("title", text(&version.title)),
            ("size_bytes", Value::Integer(size(version.size_bytes)?)),
            ("created_by", text(version.created_by)),
            ("created_at", time(version.created_at)?),
        ],
    };
    let files = version
        .files
        .iter()
        .enumerate()
        .map(|(position, file)| {
            Ok(Record {
                what: "artifact file",
                table: "artifact_files",
                columns: vec![
                    ("artifact_id", text(version.artifact_id)),
                    ("number", int(version.number.get())),
                    (
                        "position",
                        Value::Integer(i64::try_from(position).unwrap_or(i64::MAX)),
                    ),
                    ("name", text(&file.name)),
                    ("hash", text(&file.hash)),
                    ("size", Value::Integer(size(file.size)?)),
                ],
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok((row, files))
}

/// A version as `artifact_versions` holds it, before its files are read.
pub(crate) struct ArtifactVersionRow(pub(crate) ArtifactVersion);

impl FromRow for ArtifactVersionRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self(ArtifactVersion {
            artifact_id: parsed(row, "artifact_id", ArtifactId::parse)?,
            number: number(row, "number", VersionNumber::new)?,
            title: row.get("title")?,
            size_bytes: number(row, "size_bytes", |raw: i64| u64::try_from(raw))?,
            created_by: parsed(row, "created_by", UserId::parse)?,
            created_at: timestamp(row, "created_at")?,
            files: Vec::new(),
        }))
    }
}

/// A file of a version with the number of the version it belongs to.
pub(crate) struct ArtifactFileRow(pub(crate) VersionNumber, pub(crate) ArtifactFile);

impl FromRow for ArtifactFileRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self(
            number(row, "number", VersionNumber::new)?,
            ArtifactFile {
                name: parsed(row, "name", ArtifactPath::stored)?,
                hash: parsed(row, "hash", BlobHash::parse)?,
                size: number(row, "size", |raw: i64| u64::try_from(raw))?,
            },
        ))
    }
}
