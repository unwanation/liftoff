//! Value types and records of the domain.
//!
//! Every constructor validates, so the rest of the system relies on
//! invariants instead of re-checking them. Record fields are public: they
//! are plain data, and every invariant lives in the field types.

mod artifact;
mod audit;
mod binding;
mod deploy;
mod domains;
mod environment;
mod gate;
mod ids;
mod limits;
mod link;
mod module;
mod names;
mod project;
mod refs;
mod sealed;
mod secret;
mod session;
mod spelled;
mod template;
mod timestamp;
mod token;
mod user;
mod web;

pub use artifact::{
    Artifact, ArtifactFile, ArtifactId, ArtifactPath, ArtifactVersion, BlobHash, clean_title,
    title_of,
};
pub use audit::Audit;
pub use binding::Binding;
pub use deploy::{
    ComponentHash, Deploy, DeployNumber, DeploySource, DeployStatus, DeployToken,
    MAX_COMPONENT_BYTES, Trigger,
};
pub use domains::{Domain, DomainKind};
pub use environment::{EnvKind, EnvState, Environment, Pin, Visibility};
pub use gate::{GateCode, GateSession, ReturnPath};
pub use ids::{
    AuditId, BindingId, DeployId, DeployTokenId, DomainId, EnvironmentId, GitConnectionId,
    ProjectId, SecretId, SessionId, TenantId, UserId,
};
pub use limits::{Cpus, Limits, MemoryMb, TenantQuotas};
pub use link::{LinkTarget, OneTimeLink};
pub use module::{ConfigValue, InstalledModule, ModuleConfigRecord, ModuleName, ModuleVersion};
pub use names::{
    ComposeProject, EnvName, LABEL_MAX_LEN, LABEL_PATTERN, Label, ProjectName, ServiceName,
    SliceName, TenantSlug,
};
pub use project::{NewProject, Project, TemplateRef};
pub use refs::{
    AcmeEmail, Branch, ContainerPort, Host, HostPort, ImageTag, Login, OidcIssuer, RepoRef,
};
pub use sealed::SealedEnv;
pub use secret::{
    Env, ScopeKind, ScopeRef, Secret, SecretKey, SecretScope, SecretSyncRecord, SecretValue,
    SecretVersion, SyncTarget, UpdatedBy, VersionNumber,
};
pub use session::{Session, SessionKind, SetupToken};
pub use template::{CachedTemplate, TemplateGroup, TemplateSource};
pub use timestamp::Timestamp;
pub use token::{
    Bearer, DeployTokenSecret, GateCodeSecret, GateSecret, LinkSecret, SecretBody, SessionSecret,
    SetupSecret, TokenHash,
};
pub use user::{
    CredentialKind, DeviceCode, GitConnection, GitProvider, User, UserRole, UserSource,
};
pub use web::{Origin, WebUrl};
