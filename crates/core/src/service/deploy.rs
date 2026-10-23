//! `DeployService` (test cases DEP-01..DEP-10, DEP-12).
//!
//! Deploys of compose environments:
//!
//! - [`DeployService::request_ci`]: `POST /deploys` with a deploy token.
//!   The token's project must be the project of `repo` (DEP-01); every
//!   compose environment of it pinned to `ref` gets a `queued` deploy
//!   handed to the [`DeployQueue`](crate::ports::DeployQueue), none is an
//!   empty result (DEP-02). A body `env` is deprecated: its
//!   lines become environment secrets with `updated_by = ci` and the
//!   outcome says so, for the `Deprecation` header (DEP-04).
//! - [`DeployService::redeploy`]: a copy of the compose and tag of a
//!   succeeded deploy, `source = redeploy` (DEP-12).
//! - [`DeployService::run`]: the execution the queue calls. Hook
//!   `before_deploy` (a `Deny` fails the deploy before docker, DEP-08);
//!   the env from the secrets, never from the deploy (DEP-03); compose
//!   validation with every error listed (DEP-05); bindings (DEP-06); the
//!   images through [`Git`](crate::ports::Git) as the connection of
//!   `pinned_by` (DEP-07); files, slice, `pull`, `up --wait`; on success
//!   the environment runs this deploy, the whole proxy configuration is
//!   applied, hook `after_deploy`, history trimmed (DEP-09); a failed `up`
//!   rolls back to the previous succeeded deploy (DEP-10).
//!
//! Deploys of function environments
//! go through the same queue and history:
//!
//! - [`DeployService::request_function`] (`lof deploy`) and
//!   [`DeployService::request_ci_function`] (CI): the component is
//!   checked by [`FunctionHost::check`](crate::ports::FunctionHost::check)
//!   before anything is written (FDEP-02), the deploys are stored with its
//!   `component_hash`, its file is kept by
//!   [`ComponentFiles`](crate::ports::ComponentFiles) and the deploys are
//!   queued;
//! - `run` of a function deploy: hook `before_deploy`, precompile, then
//!   warm-up and route switch (FDEP-03); a failed warm-up fails the
//!   deploy and leaves the previous component serving, with no rollback
//!   deploy (FDEP-04); after it finishes, succeeded or failed, the history
//!   is trimmed and every file no remaining deploy refers to is deleted
//!   (FDEP-06), under the lock its requests keep their files under;
//! - `redeploy --to N` copies the `component_hash` of #N (FDEP-05).
//!
//! `run` knows nothing about the queue.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::secrets::{DEPLOY_TOKEN_KEY, SYSTEM_VARS, SecretsService};
use super::settings::Settings;
use super::support::{
    actor_access, apply_proxy, authorized, authorized_environment, authorized_project,
    compose_only, open_connection, project_of,
};
use crate::compose::{ComposeFile, ImageSource, PublishedPort, parse_env, render_override};
use crate::detect::DEFAULT_COMPOSE;
use crate::domain::{
    Binding, Branch, ComponentHash, ComposeProject, Deploy, DeployId, DeployNumber, DeploySource,
    DeployStatus, DeployToken, DeployTokenId, DeployTokenSecret, EnvKind, EnvName, EnvState,
    Environment, Host, ImageTag, Project, ProjectName, RepoRef, ScopeRef, SecretKey, SecretScope,
    SecretSyncRecord, SecretValue, ServiceName, SliceName, SyncTarget, Trigger, User,
};
use crate::error::Error;
use crate::manifest::{Deployable, ProjectManifest, RepoPath};
use crate::ports::{
    Action, Clock as _, CommandOutput, ComponentFiles as _, DeployQueue as _, Docker as _,
    FunctionHost as _, Git as _, GitAccess, HookResult, Hooks as _, PackageRef, Ports, ProjectFile,
    ProjectFiles as _, Resource, Rng as _, Store as _, Systemd as _,
};
use crate::snippet::{CiSnippet, SnippetTarget};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod function_tests;

/// What `POST /deploys` carries with a deploy token, validated by the
/// adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiDeployRequest {
    /// The repository the image was built from; must be the token's
    /// project's.
    pub repo: RepoRef,
    /// The branch that was pushed.
    pub git_ref: Branch,
    /// The image tag to deploy.
    pub tag: ImageTag,
    /// The compose file.
    pub compose: String,
    /// Deprecated: `.env` lines, stored as environment secrets
    /// with `updated_by = ci`.
    pub env: Option<String>,
}

/// What `POST /deploys` carries with a deploy token for a function: the multipart fields `repo`, `ref` and `component`, as the
/// CI snippet sends them.
#[derive(Clone, PartialEq, Eq)]
pub struct CiFunctionRequest {
    /// The repository the component was built from; must be the token's
    /// project's.
    pub repo: RepoRef,
    /// The branch that was pushed.
    pub git_ref: Branch,
    /// The `.wasm` component, at most 50 MiB (the adapter's limit).
    pub component: Vec<u8>,
}

impl core::fmt::Debug for CiFunctionRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CiFunctionRequest")
            .field("repo", &self.repo)
            .field("git_ref", &self.git_ref)
            .field("component_bytes", &self.component.len())
            .finish()
    }
}

/// A deploy that was created and handed to the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedDeploy {
    /// The project.
    pub project: ProjectName,
    /// The environment.
    pub environment: EnvName,
    /// The new deploy.
    pub deploy_id: DeployId,
    /// Its number within the environment.
    pub number: DeployNumber,
}

/// A deploy with the names of its project and environment, as `GET
/// /deploys/{id}` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployRecord {
    /// The project.
    pub project: ProjectName,
    /// The environment.
    pub environment: EnvName,
    /// The deploy.
    pub deploy: Deploy,
}

/// What [`DeployService::request_ci`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiDeployOutcome {
    /// One deploy per environment pinned to the pushed branch; empty when
    /// none is (DEP-02).
    pub deploys: Vec<QueuedDeploy>,
    /// Whether the request carried the deprecated `env` field: the adapter
    /// answers with a `Deprecation` header (DEP-04).
    pub deprecated_env: bool,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct DeployService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for DeployService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> DeployService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// Whether `token` is a live deploy token, asked before the body of
    /// its request is read: an upload of up to 50 MiB is accepted only
    /// from a caller the installation knows.
    ///
    /// # Errors
    ///
    /// - [`Error::Unauthenticated`] for a token that is not live;
    /// - whatever the store reports.
    pub async fn check_deploy_token(&self, token: &DeployTokenSecret) -> Result<(), Error> {
        self.ports
            .store()
            .live_deploy_token_by_hash(&token.hash())
            .await?
            .map(drop)
            .ok_or_else(|| Error::unauthenticated("the deploy token is not valid"))
    }

    /// Creates and enqueues the deploys of a CI call.
    ///
    /// Nothing is created for a branch no compose environment is pinned
    /// to (DEP-02). The deprecated `env` lines are checked before anything
    /// is stored and then written as secrets of every environment the call
    /// deploys to (DEP-04).
    ///
    /// # Errors
    ///
    /// - [`Error::Unauthenticated`] for a token that is not live;
    /// - [`Error::Forbidden`] when `repo` is not the repository of the
    ///   token's project (DEP-01);
    /// - [`Error::Validation`] on field `env` for a line that is not
    ///   `KEY=value` with a valid secret key;
    /// - whatever the store, the secrets or the queue report.
    pub async fn request_ci(
        &self,
        token: &DeployTokenSecret,
        request: CiDeployRequest,
    ) -> Result<CiDeployOutcome, Error> {
        let store = self.ports.store();
        let live = store
            .live_deploy_token_by_hash(&token.hash())
            .await?
            .ok_or_else(|| Error::unauthenticated("the deploy token is not valid"))?;
        let project = store.project(live.project_id).await?.ok_or_else(|| {
            Error::internal_msg(format!("deploy token {} has no project", live.id))
        })?;
        if request.repo != project.repo {
            return Err(Error::forbidden(format!(
                "the deploy token belongs to project {}, not to {}",
                project.name, request.repo
            )));
        }
        let deprecated_env = request.env.is_some();
        let body_env = match &request.env {
            Some(text) => {
                tracing::warn!(
                    project = %project.name,
                    "POST /deploys with the deprecated `env` field; its lines become environment secrets"
                );
                body_secrets(text)?
            }
            None => Vec::new(),
        };
        let environments = self
            .compose_environments_on(&project, &request.git_ref)
            .await?;
        let secrets = SecretsService::new(Arc::clone(&self.ports));
        for environment in &environments {
            for (key, value) in &body_env {
                secrets
                    .set_by_ci(&project, environment, key.clone(), value)
                    .await?;
            }
        }
        let mut stored = Vec::with_capacity(environments.len());
        for environment in &environments {
            let mut deploy = Deploy {
                id: DeployId::new(self.ports.rng().ulid()),
                environment_id: environment.id,
                // Numbered when stored, see `insert_numbered`.
                number: DeployNumber::FIRST,
                source: DeploySource::Ci,
                git_ref: Some(request.git_ref.clone()),
                tag: request.tag.clone(),
                compose: request.compose.clone(),
                component_hash: None,
                status: DeployStatus::Queued,
                log: String::new(),
                error: None,
                rolled_back_to: None,
                triggered_by: Trigger::Token(live.id),
                started_at: None,
                finished_at: None,
            };
            if let Err(error) = self.insert_numbered(&mut deploy).await {
                self.abandon(&stored).await;
                return Err(error);
            }
            stored.push(deploy);
        }
        let mut deploys = Vec::with_capacity(stored.len());
        for (index, (environment, deploy)) in environments.iter().zip(&stored).enumerate() {
            if let Err(error) = self.ports.queue().enqueue(deploy.id).await {
                self.abandon(stored.get(index..).unwrap_or_default()).await;
                return Err(error);
            }
            deploys.push(QueuedDeploy {
                project: project.name.clone(),
                environment: environment.name.clone(),
                deploy_id: deploy.id,
                number: deploy.number,
            });
        }
        Ok(CiDeployOutcome {
            deploys,
            deprecated_env,
        })
    }

    /// The CI call of a function: `component` is checked as a
    /// `wasi:http` component before anything is kept (FDEP-02), kept by
    /// its hash, and every function environment of the token's project
    /// pinned to `ref` gets a `queued` deploy of it handed to the queue;
    /// none is an empty result. Compose environments of the branch get
    /// nothing: their deploys are images.
    ///
    /// # Errors
    ///
    /// - [`Error::Unauthenticated`] for a token that is not live;
    /// - [`Error::Forbidden`] when `repo` is not the repository of the
    ///   token's project;
    /// - [`Error::Validation`] on field `component` when it is not a
    ///   `wasi:http` component (FDEP-02);
    /// - whatever the store, the component files or the queue report.
    pub async fn request_ci_function(
        &self,
        token: &DeployTokenSecret,
        request: CiFunctionRequest,
    ) -> Result<CiDeployOutcome, Error> {
        let store = self.ports.store();
        let live = store
            .live_deploy_token_by_hash(&token.hash())
            .await?
            .ok_or_else(|| Error::unauthenticated("the deploy token is not valid"))?;
        let project = store.project(live.project_id).await?.ok_or_else(|| {
            Error::internal_msg(format!("deploy token {} has no project", live.id))
        })?;
        if request.repo != project.repo {
            return Err(Error::forbidden(format!(
                "the deploy token belongs to project {}, not to {}",
                project.name, request.repo
            )));
        }
        self.ports.functions().check(&request.component).await?;
        let environments: Vec<Environment> = store
            .environments_on_branch(project.id, &request.git_ref)
            .await?
            .into_iter()
            .filter(|environment| environment.kind == EnvKind::Function)
            .collect();
        if environments.is_empty() {
            return Ok(CiDeployOutcome {
                deploys: Vec::new(),
                deprecated_env: false,
            });
        }
        let hash = ComponentHash::of(&request.component);
        let tag = function_tag(&hash)?;
        let drafts = environments
            .iter()
            .map(|environment| Deploy {
                id: DeployId::new(self.ports.rng().ulid()),
                environment_id: environment.id,
                number: DeployNumber::FIRST,
                source: DeploySource::Ci,
                git_ref: Some(request.git_ref.clone()),
                tag: tag.clone(),
                compose: String::new(),
                component_hash: Some(hash.clone()),
                status: DeployStatus::Queued,
                log: String::new(),
                error: None,
                rolled_back_to: None,
                triggered_by: Trigger::Token(live.id),
                started_at: None,
                finished_at: None,
            })
            .collect();
        let stored = self
            .keep_and_queue(drafts, &hash, &request.component)
            .await?;
        let deploys = environments
            .iter()
            .zip(&stored)
            .map(|(environment, deploy)| QueuedDeploy {
                project: project.name.clone(),
                environment: environment.name.clone(),
                deploy_id: deploy.id,
                number: deploy.number,
            })
            .collect();
        Ok(CiDeployOutcome {
            deploys,
            deprecated_env: false,
        })
    }

    /// `lof deploy` in a function project: queues a deploy of
    /// the function environment `env` by `actor` (`source = cli`) with
    /// `component`. The component is checked as a `wasi:http` component
    /// before anything is kept (FDEP-02), then kept by its hash
    /// (`deploys.component_hash`, FDEP-03).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the environment (SEC-10);
    /// - [`Error::NotFound`] for an unknown project or environment;
    /// - [`Error::Validation`] on field `kind` when `env` is not a function
    ///   environment, on field `component` when it is not a `wasi:http`
    ///   component (FDEP-02);
    /// - whatever the store, the component files or the queue report.
    pub async fn request_function(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        component: Vec<u8>,
    ) -> Result<QueuedDeploy, Error> {
        let (project, environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::DeployRequest,
            project,
            env,
        )
        .await?;
        if environment.kind != EnvKind::Function {
            return Err(Error::invalid(
                "kind",
                format!(
                    "{} is a {} environment: a component is deployed to a function environment",
                    environment.name, environment.kind
                ),
            ));
        }
        self.ports.functions().check(&component).await?;
        let hash = ComponentHash::of(&component);
        let draft = Deploy {
            id: DeployId::new(self.ports.rng().ulid()),
            environment_id: environment.id,
            number: DeployNumber::FIRST,
            source: DeploySource::Cli,
            git_ref: None,
            tag: function_tag(&hash)?,
            compose: String::new(),
            component_hash: Some(hash.clone()),
            status: DeployStatus::Queued,
            log: String::new(),
            error: None,
            rolled_back_to: None,
            triggered_by: Trigger::User(actor.id),
            started_at: None,
            finished_at: None,
        };
        let stored = self.keep_and_queue(vec![draft], &hash, &component).await?;
        let deploy = stored
            .first()
            .ok_or_else(|| Error::internal_msg("a function deploy was not stored"))?;
        Ok(QueuedDeploy {
            project: project.name,
            environment: environment.name,
            deploy_id: deploy.id,
            number: deploy.number,
        })
    }

    /// Stores `drafts` (deploys of the checked component `hash`), keeps
    /// the component's file and hands the deploys to the queue. The rows
    /// and the file are written under the [`ComponentsLock`](crate::ports::ComponentsLock), so a file
    /// cleanup never runs between them and never removes the file of a
    /// deploy about to run, even one kept before (FDEP-06). A failure
    /// fails the deploys stored so far, as a compose request does.
    async fn keep_and_queue(
        &self,
        drafts: Vec<Deploy>,
        hash: &ComponentHash,
        component: &[u8],
    ) -> Result<Vec<Deploy>, Error> {
        let files = self.ports.components();
        let held = files.lock().hold().await;
        let mut stored = Vec::with_capacity(drafts.len());
        for mut deploy in drafts {
            if let Err(error) = self.insert_numbered(&mut deploy).await {
                drop(held);
                self.abandon(&stored).await;
                return Err(error);
            }
            stored.push(deploy);
        }
        let saved = files.save(hash, component).await;
        drop(held);
        if let Err(error) = saved {
            self.abandon(&stored).await;
            return Err(error);
        }
        for (index, deploy) in stored.iter().enumerate() {
            if let Err(error) = self.ports.queue().enqueue(deploy.id).await {
                self.abandon(stored.get(index..).unwrap_or_default()).await;
                return Err(error);
            }
        }
        Ok(stored)
    }

    /// Queues a copy of the compose and tag of deploy `to` of `env` (the
    /// current deploy when `None`), `source = redeploy` (DEP-12); of a
    /// function environment, a copy of its `component_hash` (FDEP-05).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the environment (SEC-10);
    /// - [`Error::NotFound`] for an unknown project or environment;
    /// - [`Error::Validation`] on field `to` when it is not a succeeded
    ///   deploy, or nothing was deployed;
    /// - whatever the store or the queue report.
    pub async fn redeploy(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        to: Option<DeployNumber>,
    ) -> Result<QueuedDeploy, Error> {
        let (project, environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::DeployRedeploy,
            project,
            env,
        )
        .await?;
        if environment.kind != EnvKind::Function {
            compose_only(&environment, "a redeploy")?;
        }
        // A function's copy refers to a kept file: it is picked and
        // stored between cleanups (FDEP-06).
        let held = if environment.kind == EnvKind::Function {
            Some(self.ports.components().lock().hold().await)
        } else {
            None
        };
        let store = self.ports.store();
        let source = match to {
            Some(wanted) => store
                .deploy_by_number(environment.id, wanted)
                .await?
                .filter(|deploy| deploy.status == DeployStatus::Succeeded)
                .ok_or_else(|| {
                    Error::invalid(
                        "to",
                        format!(
                            "#{wanted} is not a succeeded deploy of {}",
                            environment.name
                        ),
                    )
                })?,
            None => match environment.current_deploy_id {
                Some(current) => store.deploy(current).await?,
                None => None,
            }
            .ok_or_else(|| {
                Error::invalid(
                    "to",
                    format!("nothing has been deployed to {}", environment.name),
                )
            })?,
        };
        let mut copy = self.copy_of(&source, Trigger::User(actor.id), None);
        self.insert_numbered(&mut copy).await?;
        drop(held);
        self.ports.queue().enqueue(copy.id).await?;
        Ok(QueuedDeploy {
            project: project.name,
            environment: environment.name,
            deploy_id: copy.id,
            number: copy.number,
        })
    }

    /// `lof deploy --tag T [--compose F]`: queues a deploy of `env` by
    /// `actor` (`source = cli`) with `compose`, or the compose of the
    /// current deploy when `None`.
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the environment (SEC-10);
    /// - [`Error::NotFound`] for an unknown project or environment;
    /// - [`Error::Validation`] on field `compose` without one and without
    ///   a current deploy;
    /// - whatever the store or the queue report.
    pub async fn request_cli(
        &self,
        actor: &User,
        project: &ProjectName,
        env: &EnvName,
        tag: ImageTag,
        compose: Option<String>,
    ) -> Result<QueuedDeploy, Error> {
        let (project, environment) = authorized_environment(
            self.ports.as_ref(),
            actor,
            Action::DeployRequest,
            project,
            env,
        )
        .await?;
        compose_only(&environment, "a compose deploy")?;
        let store = self.ports.store();
        let compose = match compose {
            Some(compose) => compose,
            None => match environment.current_deploy_id {
                Some(current) => store.deploy(current).await?.map(|deploy| deploy.compose),
                None => None,
            }
            .ok_or_else(|| {
                Error::invalid(
                    "compose",
                    format!(
                        "{} has no deploy yet: send the compose file",
                        environment.name
                    ),
                )
            })?,
        };
        let mut deploy = Deploy {
            id: DeployId::new(self.ports.rng().ulid()),
            environment_id: environment.id,
            number: DeployNumber::FIRST,
            source: DeploySource::Cli,
            git_ref: None,
            tag,
            compose,
            component_hash: None,
            status: DeployStatus::Queued,
            log: String::new(),
            error: None,
            rolled_back_to: None,
            triggered_by: Trigger::User(actor.id),
            started_at: None,
            finished_at: None,
        };
        self.insert_numbered(&mut deploy).await?;
        if let Err(error) = self.ports.queue().enqueue(deploy.id).await {
            self.abandon(core::slice::from_ref(&deploy)).await;
            return Err(error);
        }
        Ok(QueuedDeploy {
            project: project.name,
            environment: environment.name,
            deploy_id: deploy.id,
            number: deploy.number,
        })
    }

    /// The deploy `id` with the names of its project and environment.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` owns the deploy (SEC-10);
    /// [`Error::NotFound`] naming the id for an unknown deploy.
    pub async fn get(&self, actor: &User, id: DeployId) -> Result<DeployRecord, Error> {
        let store = self.ports.store();
        let found = match store.deploy(id).await {
            Ok(Some(deploy)) => store
                .environment(deploy.environment_id)
                .await?
                .ok_or_else(|| Error::internal_msg(format!("deploy {id} has no environment")))
                .map(|environment| (deploy, environment)),
            Ok(None) => Err(Error::not_found(format!("deploy {id}"))),
            Err(error) => Err(error),
        };
        let (deploy, environment) = authorized(
            self.ports.as_ref(),
            actor,
            Action::DeployGet,
            found,
            |(_, environment)| Resource::Environment(environment.id),
        )
        .await?;
        let project = project_of(self.ports.as_ref(), &environment).await?;
        Ok(DeployRecord {
            project: project.name,
            environment: environment.name,
            deploy,
        })
    }

    /// `lof token rotate`: a new deploy token for `project`. Its value is
    /// sent to the CI secrets of the repository as `actor`'s own forge
    /// connection, then stored as the project secret `LIFTOFF_TOKEN` with
    /// `ci = true`; only then is the old token revoked, so a refused step
    /// leaves the project deployable with the old one and `LIFTOFF_TOKEN`
    /// unchanged. The value is never returned.
    ///
    /// The actor's connection only carries the value: its own token is
    /// never stored as a secret. The registry login of a Forgejo
    /// compose workflow is a credential of its own, which `lof create`
    /// asks for ([`CreateService`](super::CreateService)).
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] for an unknown project;
    /// - [`Error::Forbidden`] when `actor` does not own the project's
    ///   secrets, has no connection on its forge, or the forge refuses
    ///   (not an admin of the repository);
    /// - whatever the forge, the secrets or the store report.
    pub async fn rotate_token(
        &self,
        actor: &User,
        project: &ProjectName,
    ) -> Result<DeployToken, Error> {
        let project = authorized_project(
            self.ports.as_ref(),
            actor,
            Action::DeployRotateToken,
            project,
        )
        .await?;
        let access = actor_access(self.ports.as_ref(), actor, &project).await?;
        let secret = DeployTokenSecret::from_body(&self.ports.rng().token_secret());
        let now = self.ports.clock().now();
        let token = DeployToken {
            id: DeployTokenId::new(self.ports.rng().ulid()),
            project_id: project.id,
            token_hash: secret.hash(),
            created_by: actor.id,
            created_at: now,
            revoked_at: None,
        };
        let key = SecretKey::new(DEPLOY_TOKEN_KEY)
            .map_err(|error| Error::internal_msg(format!("{DEPLOY_TOKEN_KEY}: {error}")))?;
        let value = SecretValue::new(secret.expose().to_owned());
        // check, act outside, then write. Nothing is stored before
        // the forge took the value, so a refusal leaves `LIFTOFF_TOKEN` and
        // the live token as they were. A store failure after it leaves CI
        // with a token that is not live yet; rotating again repairs it.
        self.ports
            .git()
            .put_ci_secret(&access, &project.repo, &key, &value)
            .await?;
        // `deploy.rotate_token` was the one question: writing the secret
        // is part of it, not a second `secret.set`.
        let scope = SecretScope::Project(project.id);
        let entry = SecretsService::new(Arc::clone(&self.ports))
            .set_authorized(
                actor,
                (&ScopeRef::Project(project.name.clone()), scope),
                key.clone(),
                value,
                true,
            )
            .await?;
        let store = self.ports.store();
        // The forge holds this version already: `lof secret sync` (and the
        // `SyncCi` step of `lof create`) need not send it again (SYNC-03).
        if let Some(stored) = store.secret(&scope, &key).await? {
            store
                .put_secret_sync(&SecretSyncRecord {
                    secret_id: stored.id,
                    target: SyncTarget::Ci,
                    version: Some(entry.version),
                    synced_at: Some(now),
                    error: None,
                })
                .await?;
        }
        match store.live_deploy_token(project.id).await? {
            Some(old) => store.replace_deploy_token(old.id, &token, now).await?,
            None => store.insert_deploy_token(&token).await?,
        }
        tracing::info!(project = %project.name, token = %token.id, "rotated the deploy token");
        Ok(token)
    }

    /// The CI workflow that deploys `project` (`lof token snippet`, `GET /projects/{p}/snippet`): [`CiSnippet::render`]
    /// for the provider of the project's forge host (the `git.*` module
    /// serving it, whoever asks) and the kind of its manifest (`[service]`
    /// or `[function]`; a project without a v2 manifest is a compose
    /// project with `deploy/compose.yaml`), calling the API site of the
    /// installation.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] for an unknown project;
    /// - [`Error::Validation`] on `host` when no git module serves the
    ///   project's forge host, on `manifest` when its stored `liftoff.toml`
    ///   is a v2 manifest with a mistake;
    /// - [`Error::ModuleRequired`] when the module serving the host is
    ///   disabled.
    pub async fn snippet(&self, actor: &User, project: &ProjectName) -> Result<CiSnippet, Error> {
        let project =
            authorized_project(self.ports.as_ref(), actor, Action::DeploySnippet, project).await?;
        let deployable = snippet_deployable(&project)?;
        let target = SnippetTarget {
            provider: self.ports.git().provider_of(&project.git_host).await?,
            forge_host: project.git_host.clone(),
            api_host: self.settings.proxy.api_host.clone(),
        };
        Ok(CiSnippet::render(&target, &deployable))
    }

    /// Executes a queued deploy. A deploy that fails is
    /// not an `Err`: its record is `failed` with the reason in `error`, and
    /// `run` returns it. After a failed `up` the previous succeeded deploy
    /// is redeployed inside this call, as a new deploy with
    /// `rolled_back_to` (DEP-10). Without a previous success, or when the
    /// rollback fails too, the environment becomes `failed` and its
    /// containers stay as they are for debugging. A deploy that fails
    /// earlier (hook, compose, bindings, images) touches neither docker nor
    /// the environment.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown deploy, [`Error::Conflict`] when
    /// it is not `queued`, whatever the store reports.
    pub async fn run(&self, id: DeployId) -> Result<Deploy, Error> {
        let outcome = self.execute(id).await?;
        if !outcome.apply_failed {
            return Ok(outcome.deploy);
        }
        let mut failed = outcome.deploy;
        let previous = self
            .ports
            .store()
            .last_succeeded_deploy(failed.environment_id)
            .await?;
        let recovered = match previous {
            Some(previous) => match self.roll_back(&mut failed, &previous).await {
                Ok(recovered) => recovered,
                Err(error) => {
                    self.give_up(&mut failed, &error).await;
                    return Err(error);
                }
            },
            None => false,
        };
        if !recovered {
            self.mark_environment_failed(&failed).await?;
        }
        Ok(failed)
    }

    /// Stores `deploy` under the next free number of its environment.
    /// Another request may take the number between the read and the
    /// insert; the store refuses the duplicate and the next number is
    /// tried.
    /// The compose environments of `project` pinned to `branch`: a
    /// compose deploy from CI goes to them only, a function's deploys are
    /// components.
    async fn compose_environments_on(
        &self,
        project: &Project,
        branch: &Branch,
    ) -> Result<Vec<Environment>, Error> {
        Ok(self
            .ports
            .store()
            .environments_on_branch(project.id, branch)
            .await?
            .into_iter()
            .filter(|environment| environment.kind == EnvKind::Compose)
            .collect())
    }

    async fn insert_numbered(&self, deploy: &mut Deploy) -> Result<(), Error> {
        let store = self.ports.store();
        for _ in 1..NUMBER_ATTEMPTS {
            deploy.number = store.next_deploy_number(deploy.environment_id).await?;
            match store.insert_deploy(deploy).await {
                Err(Error::Conflict(_)) => {}
                result => return result,
            }
        }
        deploy.number = store.next_deploy_number(deploy.environment_id).await?;
        store.insert_deploy(deploy).await
    }

    /// A request failed half way: deploys already stored but never queued
    /// must not stay `queued` forever.
    async fn abandon(&self, stored: &[Deploy]) {
        let store = self.ports.store();
        for deploy in stored {
            let mut deploy = deploy.clone();
            deploy.status = DeployStatus::Failed;
            deploy.error = Some("the deploy could not be queued".to_owned());
            deploy.finished_at = Some(self.ports.clock().now());
            if let Err(error) = store.update_deploy(&deploy).await {
                tracing::error!(?error, deploy = %deploy.id, "could not abandon a deploy");
            }
        }
    }

    /// The rollback itself broke: the environment cannot be trusted, and
    /// the failed deploy says so. Best effort, the original error is
    /// returned.
    async fn give_up(&self, failed: &mut Deploy, error: &Error) {
        tracing::error!(?error, deploy = %failed.id, "rollback broke");
        failed
            .log
            .push_str("rolling back failed: liftoff could not start the rollback\n");
        if let Err(error) = self.ports.store().update_deploy(failed).await {
            tracing::error!(?error, "could not note the failed rollback");
        }
        if let Err(error) = self.mark_environment_failed(failed).await {
            tracing::error!(?error, "could not mark the environment failed");
        }
    }

    /// Redeploys `previous` right now after `failed` broke the
    /// environment. Returns whether the rollback succeeded; `failed` gets
    /// a log line either way.
    async fn roll_back(&self, failed: &mut Deploy, previous: &Deploy) -> Result<bool, Error> {
        let mut rollback = self.copy_of(previous, failed.triggered_by, Some(previous.number));
        self.insert_numbered(&mut rollback).await?;
        let done = self.execute(rollback.id).await?.deploy;
        let recovered = done.status == DeployStatus::Succeeded;
        let note = if recovered {
            format!(
                "deploy #{} failed, rolled back to #{} (deploy #{})",
                failed.number, previous.number, done.number
            )
        } else {
            format!(
                "deploy #{} failed, rolling back to #{} (deploy #{}) failed too: {}",
                failed.number,
                previous.number,
                done.number,
                done.error.as_deref().unwrap_or("unknown error")
            )
        };
        failed.log.push_str(&note);
        failed.log.push('\n');
        self.ports.store().update_deploy(failed).await?;
        Ok(recovered)
    }

    /// The environment of `failed` could not be brought up and nothing
    /// could be restored: it is `failed`, and its domains say so.
    async fn mark_environment_failed(&self, failed: &Deploy) -> Result<(), Error> {
        let store = self.ports.store();
        let mut environment = store
            .environment(failed.environment_id)
            .await?
            .ok_or_else(|| {
                Error::internal_msg(format!(
                    "environment {} vanished during a deploy",
                    failed.environment_id
                ))
            })?;
        environment.state = EnvState::Failed;
        store.update_environment(&environment).await?;
        // The state is stored; a proxy that refuses catches up on the
        // next apply.
        if let Err(error) = apply_proxy(self.ports.as_ref(), &self.settings).await {
            tracing::error!(?error, environment = %environment.id, "proxy apply after a failed deploy failed");
        }
        Ok(())
    }

    /// Makes the current deploy of the function `environment` its active
    /// component again: the kept file of its component, the
    /// environment's secrets as `wasi:config`, precompile (the compile
    /// cache has it), warm-up. The daemon's start does it: active components live in its memory.
    /// Nothing is written: a component that does not come back leaves the
    /// environment as it is, its listener answering `503`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the current deploy or its component's
    /// file is gone; whatever the store, the files or the function host
    /// report.
    pub(super) async fn reactivate(&self, environment: &Environment) -> Result<(), Error> {
        let store = self.ports.store();
        let Some(current) = environment.current_deploy_id else {
            return Ok(());
        };
        let deploy = store
            .deploy(current)
            .await?
            .ok_or_else(|| Error::not_found(format!("deploy {current}")))?;
        let hash = deploy
            .component_hash
            .ok_or_else(|| Error::not_found(format!("the component of deploy {current}")))?;
        let component = self
            .ports
            .components()
            .read(&hash)
            .await?
            .ok_or_else(|| Error::not_found(format!("the kept component {}", hash.short())))?;
        let project = project_of(self.ports.as_ref(), environment).await?;
        let config = SecretsService::new(Arc::clone(&self.ports))
            .assemble(&project, environment, None)
            .await?;
        let functions = self.ports.functions();
        functions.precompile(&component).await?;
        functions
            .activate(environment.id, &component, config)
            .await?;
        tracing::info!(environment = %environment.id, component = %hash.short(), "a function is active again");
        Ok(())
    }

    /// Queues a copy of `current` that rolls back to it, triggered as
    /// `interrupted` was (start-up recovery). Returns the new
    /// deploy's number.
    pub(super) async fn restore_current(
        &self,
        current: &Deploy,
        interrupted: &Deploy,
    ) -> Result<DeployNumber, Error> {
        let mut copy = self.copy_of(current, interrupted.triggered_by, Some(current.number));
        self.insert_numbered(&mut copy).await?;
        self.ports.queue().enqueue(copy.id).await?;
        Ok(copy.number)
    }

    /// A new `queued` deploy with the content (ref, tag, compose) of
    /// `source`, as `redeploy`; numbered when stored.
    fn copy_of(
        &self,
        source: &Deploy,
        triggered_by: Trigger,
        rolled_back_to: Option<DeployNumber>,
    ) -> Deploy {
        Deploy {
            id: DeployId::new(self.ports.rng().ulid()),
            source: DeploySource::Redeploy,
            status: DeployStatus::Queued,
            log: String::new(),
            error: None,
            rolled_back_to,
            triggered_by,
            started_at: None,
            finished_at: None,
            ..source.clone()
        }
    }

    /// One execution of a queued deploy, without any rollback.
    async fn execute(&self, id: DeployId) -> Result<Outcome, Error> {
        let store = self.ports.store();
        let mut deploy = store
            .deploy(id)
            .await?
            .ok_or_else(|| Error::not_found(format!("deploy {id}")))?;
        if deploy.status != DeployStatus::Queued {
            return Err(Error::conflict(format!(
                "deploy #{} is {}, not queued",
                deploy.number, deploy.status
            )));
        }
        let environment = store
            .environment(deploy.environment_id)
            .await?
            .ok_or_else(|| Error::internal_msg(format!("deploy {id} has no environment")))?;
        let project = project_of(self.ports.as_ref(), &environment).await?;
        deploy.status = DeployStatus::Running;
        deploy.started_at = Some(self.ports.clock().now());
        store.update_deploy(&deploy).await?;
        let mut journal = Journal::default();
        let target = Target {
            project: &project,
            environment: &environment,
        };
        let done = if environment.kind == EnvKind::Function {
            journal.line(format!(
                "deploy #{} of {}/{}, component {}",
                deploy.number,
                project.name,
                environment.name,
                deploy
                    .component_hash
                    .as_ref()
                    .map_or("(none)", ComponentHash::short)
            ));
            self.function_steps(&deploy, target, &mut journal).await
        } else {
            journal.line(format!(
                "deploy #{} of {}/{}, tag {}",
                deploy.number, project.name, environment.name, deploy.tag
            ));
            self.steps(&deploy, target, &mut journal).await
        };
        match done {
            Ok(()) => self.succeed(deploy, journal).await,
            Err(failure) => {
                let outcome = self.fail(deploy, journal, failure).await?;
                // A failed upload keeps a file too: the history and the
                // files are trimmed as after a success, or failed
                // deploys would pile up on disk (FDEP-06).
                if environment.kind == EnvKind::Function {
                    self.trim_history(&environment).await;
                }
                Ok(outcome)
            }
        }
    }

    /// The steps of a function deploy: hook `before_deploy`,
    /// the kept file of its component, the environment's secrets as
    /// `wasi:config`, precompile, then warm-up and route switch in one
    /// step. No step touches docker, systemd or compose files, and none
    /// fails while applying: a failed warm-up activates nothing, so the
    /// environment keeps its previous component and nothing is rolled
    /// back (FDEP-04).
    async fn function_steps(
        &self,
        deploy: &Deploy,
        target: Target<'_>,
        journal: &mut Journal,
    ) -> Result<(), Failure> {
        journal.line("hook before_deploy");
        if let HookResult::Deny(reason) = self
            .ports
            .hooks()
            .before_deploy(target.environment, deploy)
            .await
        {
            return Err(Failure::config(format!(
                "refused by before_deploy: {reason}"
            )));
        }
        let hash = deploy
            .component_hash
            .as_ref()
            .ok_or_else(|| Failure::config("the deploy carries no component"))?;
        let component = self
            .ports
            .components()
            .read(hash)
            .await
            .map_err(|error| Failure::port(&error))?
            .ok_or_else(|| {
                Failure::config(format!(
                    "the component {} is no longer kept: deploy it again",
                    hash.short()
                ))
            })?;
        journal.line("assembling wasi:config from secrets");
        let config = SecretsService::new(Arc::clone(&self.ports))
            .assemble(target.project, target.environment, None)
            .await
            .map_err(|error| Failure::port(&error))?;
        journal.line(format!("precompiling component {}", hash.short()));
        let functions = self.ports.functions();
        functions
            .precompile(&component)
            .await
            .map_err(|error| Failure::port(&error))?;
        journal.line("warming up and switching the route");
        functions
            .activate(target.environment.id, &component, config)
            .await
            .map_err(|error| Failure::port(&error))
    }

    /// Deletes every kept component file no deploy of any function
    /// environment refers to any more (FDEP-06): what the trimmed history
    /// let go, and files nobody referred to. It runs under the
    /// [`ComponentsLock`](crate::ports::ComponentsLock), which a request holds from its rows to its
    /// file, so the file of a queued deploy is never taken for a stray
    /// one, even when it was kept before the request came.
    async fn remove_unused_components(&self) -> Result<(), Error> {
        let files = self.ports.components();
        let _held = files.lock().hold().await;
        let kept = files.list().await?;
        if kept.is_empty() {
            return Ok(());
        }
        let store = self.ports.store();
        let mut used = std::collections::BTreeSet::new();
        for environment in store.environments().await? {
            if environment.kind != EnvKind::Function {
                continue;
            }
            for deploy in store.deploys_of_environment(environment.id).await? {
                if let Some(hash) = deploy.component_hash {
                    let _new = used.insert(hash);
                }
            }
        }
        for hash in kept.into_iter().filter(|hash| !used.contains(hash)) {
            files.remove(&hash).await?;
            tracing::info!(component = %hash, "removed a component no deploy refers to");
        }
        Ok(())
    }

    /// A step failed: the record says why.
    async fn fail(
        &self,
        mut deploy: Deploy,
        mut journal: Journal,
        failure: Failure,
    ) -> Result<Outcome, Error> {
        journal.line(format!("deploy failed: {}", failure.error));
        deploy.status = DeployStatus::Failed;
        deploy.error = Some(failure.error);
        deploy.finished_at = Some(self.ports.clock().now());
        deploy.log = journal.into_text();
        self.ports.store().update_deploy(&deploy).await?;
        Ok(Outcome {
            deploy,
            apply_failed: failure.apply,
        })
    }

    /// Everything up to `up --wait`.
    async fn steps(
        &self,
        deploy: &Deploy,
        target: Target<'_>,
        journal: &mut Journal,
    ) -> Result<(), Failure> {
        journal.line("hook before_deploy");
        if let HookResult::Deny(reason) = self
            .ports
            .hooks()
            .before_deploy(target.environment, deploy)
            .await
        {
            return Err(Failure::config(format!(
                "refused by before_deploy: {reason}"
            )));
        }
        let plan = self.check(deploy, target, journal).await?;
        self.apply(deploy, &plan, target, journal).await
    }

    /// The env, the compose file, the bindings and the images: nothing is
    /// touched unless the deploy is sound.
    async fn check(
        &self,
        deploy: &Deploy,
        target: Target<'_>,
        journal: &mut Journal,
    ) -> Result<Plan, Failure> {
        journal.line("assembling env from secrets");
        let env = SecretsService::new(Arc::clone(&self.ports))
            .assemble(target.project, target.environment, Some(&deploy.tag))
            .await
            .map_err(|error| Failure::port(&error))?;
        let mut values = BTreeMap::new();
        let mut dotenv = String::new();
        for (key, value) in env.vars() {
            dotenv.push_str(&dotenv_line(key, value).map_err(Failure::config)?);
            dotenv.push('\n');
            let _previous = values.insert(key.to_string(), value.expose().to_owned());
        }
        journal.line("validating compose");
        let compose = ComposeFile::parse(&deploy.compose)
            .map_err(|error| Failure::config(error.to_string()))?;
        compose.validate().map_err(|errors| {
            Failure::config(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })?;

        journal.line("checking bindings");
        let bindings = self.check_bindings(&compose, target.environment).await?;

        journal.line("checking images");
        self.check_images(&compose, &values, deploy, target).await?;
        Ok(Plan {
            compose,
            env: dotenv,
            bindings,
        })
    }

    /// Every bound service exists in the compose file (DEP-06).
    async fn check_bindings(
        &self,
        compose: &ComposeFile,
        environment: &Environment,
    ) -> Result<Vec<Binding>, Failure> {
        let store = self.ports.store();
        let bindings = store
            .bindings_of_environment(environment.id)
            .await
            .map_err(|error| Failure::port(&error))?;
        let services = compose.services();
        let mut dangling = Vec::new();
        for binding in &bindings {
            if services.contains(&binding.service) {
                continue;
            }
            let domain = store
                .domain(binding.domain_id)
                .await
                .map_err(|error| Failure::port(&error))?
                .map_or_else(
                    || binding.domain_id.to_string(),
                    |domain| domain.host.to_string(),
                );
            dangling.push(format!(
                "binding {domain} → service {} not in compose",
                binding.service
            ));
        }
        if dangling.is_empty() {
            Ok(bindings)
        } else {
            Err(Failure::config(dangling.join("; ")))
        }
    }

    /// Whoever vouches for the deploy can read every image of the
    /// project's forge registry (DEP-07).
    async fn check_images(
        &self,
        compose: &ComposeFile,
        env: &BTreeMap<String, String>,
        deploy: &Deploy,
        target: Target<'_>,
    ) -> Result<(), Failure> {
        let registry = target.project.git_host.to_string();
        let shown = shown_references(compose, env, &registry);
        let mut rejected = Vec::new();
        let mut access = None;
        for image in compose.images_resolved(env, &registry) {
            match image.source {
                ImageSource::External => {}
                ImageSource::Invalid(reason) => {
                    rejected.push(format!("image of service {}: {reason}", image.service));
                }
                ImageSource::Internal {
                    owner,
                    package,
                    tag,
                } => {
                    let connection = match &access {
                        Some(connection) => connection,
                        None => access.insert(self.voucher(deploy, target).await?),
                    };
                    let readable = self
                        .ports
                        .git()
                        .image_exists(
                            connection,
                            &PackageRef {
                                owner,
                                name: package,
                                tag,
                            },
                        )
                        .await
                        .map_err(|error| Failure::port(&error))?;
                    if !readable {
                        let reference = shown.get(&image.service).unwrap_or(&image.written);
                        rejected.push(format!(
                            "image {reference} is not readable by {}",
                            connection.login
                        ));
                    }
                }
            }
        }
        if rejected.is_empty() {
            Ok(())
        } else {
            Err(Failure::config(rejected.join("; ")))
        }
    }

    /// The forge connection that vouches for the images of `deploy`: the
    /// user who started it, or for CI the one who pinned the environment.
    async fn voucher(&self, deploy: &Deploy, target: Target<'_>) -> Result<GitAccess, Failure> {
        let id = match deploy.triggered_by {
            Trigger::User(id) => id,
            Trigger::Token(_) => target
                .environment
                .pin
                .as_ref()
                .map(|pin| pin.pinned_by)
                .ok_or_else(|| {
                    Failure::config(format!(
                        "environment {} is not pinned any more, nobody can vouch for its images",
                        target.environment.name
                    ))
                })?,
        };
        let store = self.ports.store();
        let user = store
            .user(id)
            .await
            .map_err(|error| Failure::port(&error))?
            .ok_or_else(|| {
                Failure::config("the user who vouches for the deploy no longer exists")
            })?;
        let host: &Host = &target.project.git_host;
        let connection = store
            .git_connection(user.id, host)
            .await
            .map_err(|error| Failure::port(&error))?
            .ok_or_else(|| {
                Failure::config(format!(
                    "{} has no connection to {host} to check the images: connect Forgejo with `lof git connect {host}`",
                    user.login
                ))
            })?;
        open_connection(self.ports.as_ref(), &connection).map_err(|error| Failure::port(&error))
    }

    /// Files, slice, `pull`, `up --wait`.
    async fn apply(
        &self,
        deploy: &Deploy,
        plan: &Plan,
        target: Target<'_>,
        journal: &mut Journal,
    ) -> Result<(), Failure> {
        let (project, environment) = (target.project, target.environment);
        let compose_project = ComposeProject::new(&project.name, &environment.name);
        let slice = SliceName::new(&project.name, &environment.name);

        journal.line("writing files");
        let published: Vec<PublishedPort> = plan
            .bindings
            .iter()
            .map(|binding| PublishedPort {
                service: binding.service.clone(),
                container_port: binding.container_port,
                host_port: binding.host_port,
            })
            .collect();
        let overlay = render_override(&plan.compose, &slice, &published);
        let files = self.ports.files();
        for (file, content) in [
            (ProjectFile::Compose, deploy.compose.as_str()),
            (ProjectFile::Env, plan.env.as_str()),
            (ProjectFile::Override, overlay.as_str()),
        ] {
            files
                .write(&compose_project, file, content)
                .await
                .map_err(|error| Failure::port_applying(&error))?;
        }

        journal.line(format!("setting limits of {slice}"));
        self.ports
            .systemd()
            .set_slice_limits(&slice, environment.limits)
            .await
            .map_err(|error| Failure::port_applying(&error))?;

        journal.line("docker compose pull");
        let pulled = self.ports.docker().compose_pull(&compose_project).await;
        journal.command("docker compose pull", pulled)?;
        journal.line("docker compose up");
        let started = self
            .ports
            .docker()
            .compose_up(&compose_project, self.settings.wait_timeout)
            .await;
        journal.command("docker compose up", started)
    }

    /// The containers are up: the environment runs this deploy, the proxy
    /// routes to it, hook `after_deploy`, the history is trimmed (DEP-09).
    async fn succeed(&self, mut deploy: Deploy, mut journal: Journal) -> Result<Outcome, Error> {
        let store = self.ports.store();
        // The fresh row: only the fields a deploy owns change.
        let mut environment = store
            .environment(deploy.environment_id)
            .await?
            .ok_or_else(|| {
                Error::internal_msg(format!("deploy {} has no environment", deploy.id))
            })?;
        environment.state = EnvState::Running;
        environment.current_deploy_id = Some(deploy.id);
        store.update_environment(&environment).await?;
        journal.line("updating the proxy");
        // The containers are up whatever the proxy says, and the next apply
        // renders everything again, so a proxy failure is reported, not
        // fatal.
        if let Err(error) = apply_proxy(self.ports.as_ref(), &self.settings).await {
            tracing::error!(?error, environment = %environment.id, "proxy apply after a deploy failed");
            journal.line(format!("warning: the proxy was not updated: {error}"));
        }
        journal.line("deploy succeeded");
        deploy.status = DeployStatus::Succeeded;
        deploy.finished_at = Some(self.ports.clock().now());
        deploy.log = journal.into_text();
        store.update_deploy(&deploy).await?;
        if let HookResult::Deny(reason) =
            self.ports.hooks().after_deploy(&environment, &deploy).await
        {
            // The deploy is done; a module cannot undo it.
            tracing::warn!(deploy = %deploy.id, %reason, "after_deploy denied a finished deploy");
        }
        self.trim_history(&environment).await;
        Ok(Outcome {
            deploy,
            apply_failed: false,
        })
    }

    /// Trims the history of `environment` to the last `deploy_history`
    /// deploys (DEP-09) and, of a function environment, removes the
    /// files no deploy refers to any more (FDEP-06).
    ///
    /// Bookkeeping after the fact: a deploy that is up stays succeeded
    /// (and a rollback that came up is not undone) when the trim fails;
    /// the next deploy trims again.
    async fn trim_history(&self, environment: &Environment) {
        if let Err(error) = self
            .ports
            .store()
            .prune_deploys(environment.id, self.settings.deploy_history)
            .await
        {
            tracing::error!(?error, environment = %environment.id, "could not trim the deploy history");
        } else if environment.kind == EnvKind::Function
            && let Err(error) = self.remove_unused_components().await
        {
            tracing::error!(?error, environment = %environment.id, "could not remove unused components");
        }
    }
}

/// The image reference of each service as a message may show it (secrets never reach the journal): interpolated when only the system
/// variables went into it, as written when a secret did.
fn shown_references(
    compose: &ComposeFile,
    env: &BTreeMap<String, String>,
    registry: &str,
) -> BTreeMap<ServiceName, String> {
    let public: BTreeMap<String, String> = env
        .iter()
        .filter(|(key, _)| SYSTEM_VARS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let full: BTreeMap<ServiceName, String> = compose
        .images_resolved(env, registry)
        .into_iter()
        .map(|image| (image.service, image.reference))
        .collect();
    compose
        .images_resolved(&public, registry)
        .into_iter()
        .map(|image| {
            let shown = if full.get(&image.service) == Some(&image.reference) {
                image.reference
            } else {
                image.written
            };
            (image.service, shown)
        })
        .collect()
}

/// The `tag` of a function deploy, which has no image: `sha256-` and the
/// short hash of its component (`component_hash` is what a function deploy deploys; the column stays filled for the history).
fn function_tag(hash: &ComponentHash) -> Result<ImageTag, Error> {
    ImageTag::new(&format!("sha256-{}", hash.short()))
        .map_err(|error| Error::internal_msg(format!("the tag of component {hash}: {error}")))
}

/// How often a deploy number is claimed again after another request took it.
const NUMBER_ATTEMPTS: u8 = 5;

/// What the CI snippet of `project` deploys: the `[service]` or
/// `[function]` of its stored `liftoff.toml`. A project without one, or
/// with a stored text that is not a v2 manifest (a v1 project's), is the
/// compose project with `deploy/compose.yaml`; a v2 manifest
/// with a mistake is an error ([`ProjectManifest::parse_stored`]).
fn snippet_deployable(project: &Project) -> Result<Deployable, Error> {
    let stored = match project.manifest.as_deref() {
        Some(text) => ProjectManifest::parse_stored(text)?,
        None => None,
    };
    Ok(stored.map_or_else(
        || Deployable::Service {
            compose: RepoPath::constant(DEFAULT_COMPOSE),
            expose: None,
        },
        |manifest| manifest.deploy,
    ))
}

/// The deprecated `env` of a CI call as secrets (DEP-04).
fn body_secrets(text: &str) -> Result<Vec<(SecretKey, SecretValue)>, Error> {
    parse_env(text)
        .map_err(|why| Error::invalid("env", why))?
        .into_iter()
        .map(|(key, value)| {
            SecretKey::new(&key)
                .map(|key| (key, SecretValue::new(value)))
                .map_err(|error| Error::invalid("env", format!("{key}: {error}")))
        })
        .collect()
}

/// `KEY=value` as compose reads it back literally: bare when the value is
/// plain, single-quoted (no interpolation) otherwise, double-quoted when it
/// holds a single quote and nothing compose would expand. A value that fits
/// none (a control character, or a single quote next to `"`, `\\` or `$`)
/// cannot be written.
fn dotenv_line(key: &SecretKey, value: &SecretValue) -> Result<String, String> {
    let raw = value.expose();
    let unwritable = || format!("env: secret {key} cannot be written to .env");
    if raw.chars().any(char::is_control) {
        return Err(unwritable());
    }
    let plain =
        !raw.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '#' | '\\' | '$'));
    if plain {
        Ok(format!("{key}={raw}"))
    } else if !raw.contains('\'') {
        Ok(format!("{key}='{raw}'"))
    } else if !raw.contains(['"', '\\', '$']) {
        Ok(format!("{key}=\"{raw}\""))
    } else {
        Err(unwritable())
    }
}

/// The project and environment a deploy runs in.
#[derive(Clone, Copy)]
struct Target<'a> {
    project: &'a Project,
    environment: &'a Environment,
}

/// What the checks settled, for the files and docker.
struct Plan {
    compose: ComposeFile,
    /// The `.env` to write.
    env: String,
    bindings: Vec<Binding>,
}

/// What one execution of a deploy came to.
struct Outcome {
    deploy: Deploy,
    /// Whether it failed while applying (files, slice, `pull`, `up`), the
    /// failures that roll back.
    apply_failed: bool,
}

/// Why a deploy stopped before it succeeded.
struct Failure {
    /// Shown to users as the deploy's `error`.
    error: String,
    /// Whether it failed while applying.
    apply: bool,
}

impl Failure {
    /// The deploy is wrong (hook, env, compose, bindings, images): nothing
    /// ran.
    fn config(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            apply: false,
        }
    }

    /// A port failed before anything was applied.
    fn port(error: &Error) -> Self {
        Self {
            error: Self::describe(error),
            apply: false,
        }
    }

    /// A port failed while the files were being replaced: the directory may
    /// hold a mix of two deploys, so the previous success is restored like
    /// after a failed `up`.
    fn port_applying(error: &Error) -> Self {
        Self {
            error: Self::describe(error),
            apply: true,
        }
    }

    /// `pull` or `up` failed.
    fn apply(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            apply: true,
        }
    }

    /// What a user may read of `error`; the source of an internal error goes
    /// to the logs only.
    fn describe(error: &Error) -> String {
        if matches!(error, Error::Internal(_)) {
            tracing::error!(?error, "deploy step failed");
        }
        error.to_string()
    }
}

/// The log of a deploy, one line at a time.
#[derive(Debug, Default)]
struct Journal(String);

impl Journal {
    fn line(&mut self, text: impl AsRef<str>) {
        self.0.push_str(text.as_ref());
        self.0.push('\n');
    }

    fn into_text(self) -> String {
        self.0
    }

    /// Logs what a docker command printed and judges its exit: the last line
    /// of stderr is the reason of a failure (DEP-10).
    fn command(
        &mut self,
        label: &str,
        result: Result<CommandOutput, Error>,
    ) -> Result<(), Failure> {
        match result {
            Ok(output) => {
                for line in &output.lines {
                    self.line(&line.text);
                }
                if output.success() {
                    return Ok(());
                }
                let exit = output.exit_code.map_or_else(
                    || format!("{label} was killed"),
                    |code| format!("{label} exited with {code}"),
                );
                Err(Failure::apply(
                    output.last_stderr_line().map_or(exit, str::to_owned),
                ))
            }
            Err(error) => {
                self.line(format!("{label}: {error}"));
                Err(Failure::apply(Failure::describe(&error)))
            }
        }
    }
}
