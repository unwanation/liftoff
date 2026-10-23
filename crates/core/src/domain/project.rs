//! Projects: one per repository.

use super::ids::{ProjectId, TenantId, UserId};
use super::names::{ProjectName, TenantSlug};
use super::refs::{Host, RepoRef};
use super::template::TemplateGroup;
use super::timestamp::Timestamp;
use crate::error::{Problem, ValidationError};

/// The template a project was created from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TemplateRef {
    /// Template group, one of the catalogue's ([`TemplateGroup`]), e.g.
    /// `web`.
    pub group: String,
    /// Template name, e.g. `acme-app`.
    pub name: String,
}

impl TemplateRef {
    /// The template `name` of `group`, as `POST /projects {template}` and
    /// the CLI's local mode name it.
    ///
    /// # Errors
    ///
    /// [`ValidationError`] on `template.name` when `name` is blank.
    pub fn new(group: TemplateGroup, name: &str) -> Result<Self, ValidationError> {
        if name.trim().is_empty() {
            return Err(ValidationError::new("template.name", Problem::Empty));
        }
        Ok(Self {
            group: group.as_str().to_owned(),
            name: name.to_owned(),
        })
    }
}

/// A repository liftoff deploys; unique by `(git_host, repo)` and by
/// `(tenant_id, name)` (also when `tenant_id` is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Identifier.
    pub id: ProjectId,
    /// Owning tenant; `None` without the `tenants` module.
    pub tenant_id: Option<TenantId>,
    /// Name.
    pub name: ProjectName,
    /// Forge host of the repository, e.g. `git.example.com`.
    pub git_host: Host,
    /// `owner/name` on that host.
    pub repo: RepoRef,
    /// The text of the last `liftoff.toml` the CLI read, as sent: opaque
    /// to the store; [`ProjectManifest::parse_stored`](crate::manifest::ProjectManifest::parse_stored)
    /// reads it, and a v1 client's text may not be a v2 manifest.
    pub manifest: Option<String>,
    /// The template the project was created from.
    pub template: Option<TemplateRef>,
    /// Who created it.
    pub created_by: UserId,
    /// Creation time.
    pub created_at: Timestamp,
}

/// What `POST /projects` asks for, each field validated at the boundary:
/// the input of
/// [`ProjectService::create`](crate::service::ProjectService::create).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    /// The tenant the actor names by slug (`lof create --tenant`); `None`
    /// leaves the choice to the `project_tenant` hook.
    pub tenant: Option<TenantSlug>,
    /// Name.
    pub name: ProjectName,
    /// Forge host of the repository.
    pub git_host: Host,
    /// `owner/name` on that host.
    pub repo: RepoRef,
    /// JSON of the `liftoff.toml` read, if any.
    pub manifest: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod template_ref {
        use super::*;

        // TC: CRE-01 (`POST /projects {template}` and the CLI's local mode
        // name a template through the same rule)
        #[test]
        fn keeps_group_and_name_when_name_is_given() {
            let template = TemplateRef::new(TemplateGroup::Web, "acme-app").unwrap();
            assert_eq!(
                (template.group.as_str(), template.name.as_str()),
                ("web", "acme-app")
            );
        }

        // TC: CRE-01
        #[test]
        fn validation_on_template_name_when_blank() {
            let error = TemplateRef::new(TemplateGroup::Api, "  ").unwrap_err();
            assert_eq!(
                (error.field, error.problem),
                ("template.name", Problem::Empty)
            );
        }
    }
}
