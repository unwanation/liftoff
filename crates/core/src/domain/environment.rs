//! Environments of a project.

use super::ids::{DeployId, EnvironmentId, ProjectId, UserId};
use super::limits::Limits;
use super::names::EnvName;
use super::refs::Branch;
use super::spelled::spelled_enum;
use super::timestamp::Timestamp;

/// What an environment runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EnvKind {
    /// A compose project from CI images, limited by a systemd slice.
    Compose,
    /// A `wasi:http` component run by the daemon: no compose
    /// project, no slice, no ports; its pin is optional.
    Function,
    /// A dev address that leads to a developer's machine through the
    /// tunnel: it has an `owner_user_id`, and its state may
    /// be `offline`.
    Tunnel,
}

spelled_enum!(EnvKind {
    Compose => "compose",
    Function => "function",
    Tunnel => "tunnel",
});

impl EnvKind {
    /// The visibility a new environment of this kind starts with: a
    /// tunnel is private (VIS-01), a compose or function
    /// environment is shared.
    #[must_use]
    pub const fn default_visibility(self) -> Visibility {
        match self {
            Self::Tunnel => Visibility::Private,
            Self::Compose | Self::Function => Visibility::Shared,
        }
    }
}

/// Lifecycle state of an environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EnvState {
    /// Never deployed.
    Empty,
    /// Up from the current deploy.
    Running,
    /// The last deploy failed and nothing could be rolled back to.
    Failed,
    /// Stopped by a user; volumes kept.
    Stopped,
    /// A tunnel without its CLI.
    Offline,
}

spelled_enum!(EnvState {
    Empty => "empty",
    Running => "running",
    Failed => "failed",
    Stopped => "stopped",
    Offline => "offline",
});

/// Who may open an environment's domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Only after login (tunnels, gated environments).
    Private,
    /// Anybody.
    Shared,
}

spelled_enum!(Visibility { Private => "private", Shared => "shared" });

/// The branch whose CI deploys into an environment and who pinned it;
/// set or cleared together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// Branch of the project's repository.
    pub branch: Branch,
    /// Whose forge connection checks CI deploys.
    pub pinned_by: UserId,
}

/// A deployment slot of a project; unique by `(project_id, name)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    /// Identifier.
    pub id: EnvironmentId,
    /// The project.
    pub project_id: ProjectId,
    /// Name, unique within the project.
    pub name: EnvName,
    /// What it runs.
    pub kind: EnvKind,
    /// The branch it follows, if pinned.
    pub pin: Option<Pin>,
    /// The user whose machine a tunnel leads to.
    pub owner_user_id: Option<UserId>,
    /// Slice limits.
    pub limits: Limits,
    /// Lifecycle state.
    pub state: EnvState,
    /// The deploy currently running, if any.
    pub current_deploy_id: Option<DeployId>,
    /// Who may open its domains.
    pub visibility: Visibility,
    /// Whether its domains sit behind the gate (`lof protect`): Caddy asks the gate listener (`forward_auth`) before
    /// every request. Only the `tenants` module writes it (through
    /// `core-call environments.set_protected`); without it the flag stays
    /// `false`.
    pub protected: bool,
    /// Creation time.
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod env_kind {
        use super::*;

        // TC: VIS-01
        #[test]
        fn starts_private_when_kind_is_tunnel() {
            assert_eq!(EnvKind::Tunnel.default_visibility(), Visibility::Private);
        }

        // TC: VIS-01 (the documented kinds and their spelling in the store)
        #[test]
        fn spells_every_kind_when_parsed_back() {
            let spelled: Vec<Result<EnvKind, _>> = ["compose", "function", "tunnel"]
                .into_iter()
                .map(EnvKind::parse)
                .collect();
            assert_eq!(
                spelled,
                vec![
                    Ok(EnvKind::Compose),
                    Ok(EnvKind::Function),
                    Ok(EnvKind::Tunnel)
                ]
            );
        }
    }
}
