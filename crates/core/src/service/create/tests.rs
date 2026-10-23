//! Tests of [`CreateService`] and the pure helpers of a plan over the
//! shared [`World`] (`CRE-01`…`CRE-10`).
//!
//! The world's base domain is `apps.test`, its API site `liftoff.test`;
//! `owner` has a Forgejo connection on `git.example.com`, which the fake
//! forge serves as Forgejo. Project `shop` (`owner/shop`) and the domain
//! `shop.apps.test` already exist.

use std::collections::BTreeMap;

use super::*;
use crate::detect::{SourceTree, StackDetector};
use crate::domain::{
    ContainerPort, DomainKind, EnvKind, GitProvider, Pin, Secret, SecretScope, ServiceName,
};
use crate::fakes::{GitCall, World};
use crate::ports::{Action, HookResult, RepoRole, Resource, Secrets as _};
use crate::service::DEPLOY_TOKEN_KEY;
use crate::snippet::{REGISTRY_TOKEN_KEY, REGISTRY_USER_KEY, SnippetTarget};

/// The `liftoff.toml` of the template `acme/app`, in the
/// canonical form of [`ProjectManifest::to_toml`].
const APP_TEMPLATE: &str = r#"[template]
group = "web"
name = "acme-app"
description = "SolidJS + acme-ui, Vite, Bun"

[service]
compose = "deploy/compose.yaml"
expose = "web:3000"

[dev]
command = "bun dev"

[secrets]
DATABASE_URL = { hint = "postgres://…", scope = "environment" }
SESSION_SECRET = { hint = "32+ random bytes", generate = "random:32" }

[substitute]
files = ["README.md", "package.json", "deploy/compose.yaml"]
"#;

/// The `liftoff.toml` of the function template `acme/fn-ts`.
const FN_TS: &str = r#"[template]
group = "function"
name = "fn-ts"
description = "TypeScript function"

[function]
build = "bun install && bun run build"
component = "dist/fn.wasm"
"#;

/// A compose repository's manifest without `[secrets]` (what stack
/// detection proposes for one service exposing port 3000).
const SITE: &str = r#"[service]
compose = "deploy/compose.yaml"
expose = "web:3000"
"#;

fn host(raw: &str) -> Host {
    Host::new(raw).unwrap()
}

fn repo(raw: &str) -> RepoRef {
    RepoRef::new(raw).unwrap()
}

fn project(raw: &str) -> ProjectName {
    ProjectName::new(raw).unwrap()
}

fn env(raw: &str) -> EnvName {
    EnvName::new(raw).unwrap()
}

fn key(raw: &str) -> SecretKey {
    SecretKey::new(raw).unwrap()
}

fn path(raw: &str) -> RepoPath {
    RepoPath::new(raw).unwrap()
}

fn value(raw: &str) -> SecretValue {
    SecretValue::new(raw.to_owned())
}

fn manifest(text: &str) -> ProjectManifest {
    ProjectManifest::parse(text).unwrap()
}

/// The `tenants`-like answer of the next `project_tenant`: the tenant
/// asked for exists and the actor may create projects in it.
fn admit_tenant(world: &World) {
    let tenant = TenantId::new(crate::ports::Rng::ulid(&world.ports.rng));
    world.ports.hooks.answer_project_tenant(Ok(Some(tenant)));
}

/// The tenant of every `CreateProject` step of `plan`.
fn tenant_of(plan: &CreatePlan) -> Vec<Option<TenantSlug>> {
    plan.steps
        .iter()
        .filter_map(|step| match step {
            Step::CreateProject { project, .. } => Some(project.tenant.clone()),
            _ => None,
        })
        .collect()
}

/// `owner/name` on `git.example.com`.
fn on_forge(raw: &str) -> TemplateSource {
    TemplateSource {
        host: Some(host(World::GIT_HOST)),
        repo: repo(raw),
    }
}

/// Gives the template `raw` the `liftoff.toml` `toml`, readable by
/// `owner`.
fn publish(world: &World, raw: &str, toml: &str) {
    world.ports.git.template(&repo(raw), toml);
    world
        .ports
        .git
        .grant(&world.owner.login, &repo(raw), RepoRole::Read);
}

/// The CI workflow of `text`'s deployable for a Forgejo repository on
/// `git.example.com` deploying to `liftoff.test` (pinned by SNIP-01).
fn forgejo_snippet(text: &str) -> CiSnippet {
    CiSnippet::render(
        &SnippetTarget {
            provider: GitProvider::Forgejo,
            forge_host: host(World::GIT_HOST),
            api_host: host(World::API_HOST),
        },
        &manifest(text).deploy,
    )
}

fn bind_web_3000(domain: &str) -> Step {
    Step::Bind {
        env: env("prod"),
        request: BindRequest {
            service: ServiceName::new("web").unwrap(),
            port: ContainerPort::new(3000).unwrap(),
            host: host(domain),
        },
    }
}

/// The `REGISTRY_USER` a Forgejo compose plan of `owner` asks for: the
/// registry login is a credential of its own, never the actor's forge
/// connection (CRE-08).
fn registry_user_ask() -> SecretAsk {
    SecretAsk {
        key: key(REGISTRY_USER_KEY),
        hint: Some(
            "the git.example.com account the workflow pushes the image as, e.g. owner".to_owned(),
        ),
        generate: None,
        scope: ScopeKind::Project,
        ci: true,
    }
}

/// The `REGISTRY_TOKEN` a Forgejo compose plan asks for.
fn registry_token_ask() -> SecretAsk {
    SecretAsk {
        key: key(REGISTRY_TOKEN_KEY),
        hint: Some(
            "a token of that account with only the package scope (write:package), not the `lof git connect` token"
                .to_owned(),
        ),
        generate: None,
        scope: ScopeKind::Project,
        ci: true,
    }
}

/// Where `StoreSecrets` stores `name`.
fn place(name: &str, scope: ScopeKind, ci: bool) -> SecretPlace {
    SecretPlace {
        key: key(name),
        scope,
        ci,
    }
}

/// The values the person gave the registry login of a Forgejo compose
/// plan.
const REGISTRY_USER_VALUE: &str = "owner";
const REGISTRY_TOKEN_VALUE: &str = "package-token-of-owner";

/// The plan of `lof create blog --template git.example.com/acme/app` by
/// `owner` with no other flag.
fn blog_plan() -> CreatePlan {
    CreatePlan {
        project: project("blog"),
        git_host: host(World::GIT_HOST),
        repo: repo("owner/blog"),
        domain: host("blog.apps.test"),
        steps: vec![
            Step::GenerateRepo {
                template: repo("acme/app"),
                target: NewRepo {
                    repo: repo("owner/blog"),
                    private: true,
                },
            },
            Step::Clone,
            Step::Substitute(Substitution {
                files: vec![
                    path("README.md"),
                    path("package.json"),
                    path("deploy/compose.yaml"),
                ],
                project: project("blog"),
                domain: host("blog.apps.test"),
            }),
            Step::AskSecrets(vec![
                SecretAsk {
                    key: key("DATABASE_URL"),
                    hint: Some("postgres://…".to_owned()),
                    generate: None,
                    scope: ScopeKind::Environment,
                    ci: false,
                },
                SecretAsk {
                    key: key("SESSION_SECRET"),
                    hint: Some("32+ random bytes".to_owned()),
                    generate: Some(Generate::Random(32)),
                    scope: ScopeKind::Project,
                    ci: false,
                },
                registry_user_ask(),
                registry_token_ask(),
            ]),
            Step::WriteCiSnippet(forgejo_snippet(APP_TEMPLATE)),
            Step::Commit {
                message: "chore: set up liftoff".to_owned(),
            },
            Step::Push,
            Step::CreateProject {
                project: NewProject {
                    tenant: None,
                    name: project("blog"),
                    git_host: host(World::GIT_HOST),
                    repo: repo("owner/blog"),
                    manifest: Some(APP_TEMPLATE.to_owned()),
                },
                template: Some(TemplateRef {
                    group: "web".to_owned(),
                    name: "acme-app".to_owned(),
                }),
            },
            Step::AddEnvironment {
                name: env("prod"),
                kind: DeployKind::Compose,
            },
            Step::AddDomain {
                host: host("blog.apps.test"),
                label: Some(Label::new("blog").unwrap()),
            },
            bind_web_3000("blog.apps.test"),
            Step::IssueToken,
            Step::StoreSecrets(vec![
                place("DATABASE_URL", ScopeKind::Environment, false),
                place("SESSION_SECRET", ScopeKind::Project, false),
                place(REGISTRY_USER_KEY, ScopeKind::Project, true),
                place(REGISTRY_TOKEN_KEY, ScopeKind::Project, true),
            ]),
            Step::SyncCi,
            Step::WriteLiftoffFile,
        ],
    }
}

/// `lof create blog --template git.example.com/acme/app` with `options`.
async fn plan_blog(world: &World, options: CreateOptions) -> Result<CreatePlan, Error> {
    publish(world, "acme/app", APP_TEMPLATE);
    world
        .create()
        .plan_from_template(&world.owner, project("blog"), on_forge("acme/app"), options)
        .await
}

/// `owner/site` on `git.example.com` with `manifest`, `owner` its admin.
fn site(world: &World, manifest: ExistingManifest) -> ExistingRepo {
    world
        .ports
        .git
        .grant(&world.owner.login, &repo("owner/site"), RepoRole::Admin);
    ExistingRepo {
        git_host: host(World::GIT_HOST),
        repo: repo("owner/site"),
        manifest,
    }
}

/// What the local steps of the blog plan report: default branch
/// `branch`, the two secrets of `acme-app` and the registry login.
fn outcome(branch: &str) -> LocalOutcome {
    LocalOutcome {
        default_branch: Branch::new(branch).unwrap(),
        secrets: BTreeMap::from([
            (key("DATABASE_URL"), value("postgres://db/blog")),
            (key("SESSION_SECRET"), value("s3cr3t")),
            (key(REGISTRY_USER_KEY), value(REGISTRY_USER_VALUE)),
            (key(REGISTRY_TOKEN_KEY), value(REGISTRY_TOKEN_VALUE)),
        ]),
    }
}

/// Runs the server steps of the blog plan as `owner`, whom the CLI's
/// `GenerateRepo` made admin of `owner/blog`.
async fn execute_blog(world: &World, branch: &str) -> Created {
    world
        .ports
        .git
        .grant(&world.owner.login, &repo("owner/blog"), RepoRole::Admin);
    world
        .create()
        .execute(&world.owner, &blog_plan(), outcome(branch))
        .await
        .unwrap()
}

/// The stored project `blog`.
async fn stored_blog(world: &World) -> Project {
    world
        .ports
        .store
        .project_by_repo(&host(World::GIT_HOST), &repo("owner/blog"))
        .await
        .unwrap()
        .unwrap()
}

/// The secrets of `scope` as `(key, ci, plaintext)`, by key.
async fn secrets_of(world: &World, scope: SecretScope) -> Vec<(String, bool, String)> {
    world
        .ports
        .store
        .secrets_of_scope(&scope)
        .await
        .unwrap()
        .iter()
        .map(|secret: &Secret| {
            (
                secret.key.to_string(),
                secret.ci,
                world.ports.secrets.decrypt(&secret.value).unwrap(),
            )
        })
        .collect()
}

/// A repository as files in memory.
struct Files(BTreeMap<&'static str, &'static str>);

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

mod from_template {
    use super::*;

    // TC: CRE-01
    #[tokio::test]
    async fn plans_every_step_in_spec_order_when_template_has_everything() {
        let world = World::new().await;
        let plan = plan_blog(&world, CreateOptions::default()).await.unwrap();
        assert_eq!(plan, blog_plan());
    }

    // TC: CRE-01 (the plan is deterministic: built twice, equal)
    #[tokio::test]
    async fn plans_the_same_when_built_twice() {
        let world = World::new().await;
        let first = plan_blog(&world, CreateOptions::default()).await.unwrap();
        let second = plan_blog(&world, CreateOptions::default()).await.unwrap();
        assert_eq!(first, second);
    }

    // TC: CRE-01 (`--public`, `--owner`)
    #[tokio::test]
    async fn generates_public_repo_of_owner_flag_when_options_say_so() {
        let world = World::new().await;
        let options = CreateOptions {
            public: true,
            owner: Some(Login::new("acme").unwrap()),
            ..CreateOptions::default()
        };
        let plan = plan_blog(&world, options).await.unwrap();
        assert_eq!(
            (plan.repo.clone(), plan.steps.first().cloned()),
            (
                repo("acme/blog"),
                Some(Step::GenerateRepo {
                    template: repo("acme/app"),
                    target: NewRepo {
                        repo: repo("acme/blog"),
                        private: false,
                    },
                })
            )
        );
    }

    // TC: CRE-01 (building a plan asks `authorize` as creating a project does)
    #[tokio::test]
    async fn asks_authorize_project_create_on_the_installation_when_built() {
        let world = World::new().await;
        let _plan = plan_blog(&world, CreateOptions::default()).await.unwrap();
        assert_eq!(
            world.log().authorized(),
            vec![(world.owner.id, Action::ProjectCreate, Resource::Admin)]
        );
    }

    // TC: CRE-01 (a refusal builds no plan)
    #[tokio::test]
    async fn forbidden_when_authorize_denies() {
        let world = World::new().await;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("no".to_owned()));
        let result = plan_blog(&world, CreateOptions::default()).await;
        assert!(matches!(result, Err(Error::Forbidden(_))), "{result:?}");
    }

    // TC: CRE-01 (a refusal reaches no forge: the template is not read)
    #[tokio::test]
    async fn reads_no_template_when_authorize_denies() {
        let world = World::new().await;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("no".to_owned()));
        let _refused = plan_blog(&world, CreateOptions::default()).await;
        let reads = world
            .ports
            .git
            .calls()
            .into_iter()
            .filter(|call| matches!(call, GitCall::ListTemplates { .. }))
            .count();
        assert_eq!(reads, 0);
    }

    // TC: CRE-03 (a template cannot take over a key liftoff sets itself)
    #[tokio::test]
    async fn validation_naming_the_key_when_manifest_asks_a_reserved_secret() {
        let world = World::new().await;
        for reserved in [
            "LIFTOFF_TOKEN",
            "REGISTRY_TOKEN",
            "REGISTRY_USER",
            "LIFTOFF_DOMAIN",
        ] {
            let toml = format!(
                "[service]\ncompose = \"deploy/compose.yaml\"\n\n[secrets]\n{reserved} = {{ hint = \"mine\" }}\n"
            );
            publish(&world, "acme/greedy", &toml);
            let result = world
                .create()
                .plan_from_template(
                    &world.owner,
                    project("blog"),
                    on_forge("acme/greedy"),
                    CreateOptions::default(),
                )
                .await;
            assert!(
                matches!(&result, Err(Error::Validation(v))
                    if v.field == "secrets" && v.to_string().contains(reserved)),
                "{reserved}: {result:?}"
            );
        }
    }

    // TC: CRE-17 (the tenant asked for is the tenant of the project the
    // plan creates)
    #[tokio::test]
    async fn creates_project_in_tenant_when_options_name_one() {
        let world = World::new().await;
        admit_tenant(&world);
        let options = CreateOptions {
            tenant: Some(TenantSlug::new("acme").unwrap()),
            ..CreateOptions::default()
        };
        let plan = plan_blog(&world, options).await.unwrap();
        assert_eq!(
            tenant_of(&plan),
            vec![Some(TenantSlug::new("acme").unwrap())]
        );
    }

    // TC: CRE-02
    #[tokio::test]
    async fn conflict_naming_the_project_when_name_is_taken() {
        let world = World::new().await;
        publish(&world, "acme/app", APP_TEMPLATE);
        let result = world
            .create()
            .plan_from_template(
                &world.owner,
                project("shop"),
                on_forge("acme/app"),
                CreateOptions::default(),
            )
            .await;
        assert!(
            matches!(&result, Err(Error::Conflict(why)) if why.contains("shop")),
            "{result:?}"
        );
    }

    // TC: CRE-06
    #[tokio::test]
    async fn adds_function_environment_without_binding_when_template_is_a_function() {
        let world = World::new().await;
        publish(&world, "acme/fn-ts", FN_TS);
        let plan = world
            .create()
            .plan_from_template(
                &world.owner,
                project("hello"),
                on_forge("acme/fn-ts"),
                CreateOptions::default(),
            )
            .await
            .unwrap();
        let environments: Vec<&Step> = plan
            .steps
            .iter()
            .filter(|step| matches!(step, Step::AddEnvironment { .. }))
            .collect();
        let binds = plan
            .steps
            .iter()
            .filter(|step| matches!(step, Step::Bind { .. }))
            .count();
        assert_eq!(
            (environments, binds),
            (
                vec![&Step::AddEnvironment {
                    name: env("prod"),
                    kind: DeployKind::Function,
                }],
                0
            )
        );
    }

    // TC: CRE-07 (`--domain` replaces `<name>.<base>` everywhere)
    #[tokio::test]
    async fn uses_domain_flag_when_given() {
        let world = World::new().await;
        let options = CreateOptions {
            domain: Some(host("www.blog.test")),
            ..CreateOptions::default()
        };
        let plan = plan_blog(&world, options).await.unwrap();
        let substituted = plan.steps.iter().find_map(|step| match step {
            Step::Substitute(substitution) => Some(substitution.domain.clone()),
            _ => None,
        });
        let added = plan.steps.iter().find_map(|step| match step {
            Step::AddDomain { host, .. } => Some(host.clone()),
            _ => None,
        });
        assert_eq!(
            (plan.domain, substituted, added),
            (
                host("www.blog.test"),
                Some(host("www.blog.test")),
                Some(host("www.blog.test"))
            )
        );
    }

    // TC: CRE-07 (batch: a taken `<name>.<base>` is a conflict)
    #[tokio::test]
    async fn conflict_naming_the_domain_when_default_domain_is_taken() {
        let world = World::new().await;
        let _taken = world
            .add_domain(&world.shop, "blog.apps.test")
            .await
            .unwrap();
        let result = plan_blog(&world, CreateOptions::default()).await;
        assert!(
            matches!(&result, Err(Error::Conflict(why)) if why.contains("blog.apps.test")),
            "{result:?}"
        );
    }

    // TC: CRE-07 (batch: a taken `--domain` is a conflict)
    #[tokio::test]
    async fn conflict_naming_the_domain_when_domain_flag_is_taken() {
        let world = World::new().await;
        let options = CreateOptions {
            domain: Some(host("shop.apps.test")),
            ..CreateOptions::default()
        };
        let result = plan_blog(&world, options).await;
        assert!(
            matches!(&result, Err(Error::Conflict(why)) if why.contains("shop.apps.test")),
            "{result:?}"
        );
    }
}

mod for_existing {
    use super::*;

    // TC: CRE-09 (a repository with a manifest: nothing to write but CI)
    #[tokio::test]
    async fn plans_ci_commit_and_server_steps_when_repo_has_a_manifest() {
        let world = World::new().await;
        let existing = site(&world, ExistingManifest::Committed(manifest(SITE)));
        let plan = world
            .create()
            .plan_for_existing(
                &world.owner,
                project("site"),
                existing,
                CreateOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            plan,
            CreatePlan {
                project: project("site"),
                git_host: host(World::GIT_HOST),
                repo: repo("owner/site"),
                domain: host("site.apps.test"),
                steps: vec![
                    Step::AskSecrets(vec![registry_user_ask(), registry_token_ask()]),
                    Step::WriteCiSnippet(forgejo_snippet(SITE)),
                    Step::Commit {
                        message: "chore: set up liftoff".to_owned(),
                    },
                    Step::Push,
                    Step::CreateProject {
                        project: NewProject {
                            tenant: None,
                            name: project("site"),
                            git_host: host(World::GIT_HOST),
                            repo: repo("owner/site"),
                            manifest: Some(SITE.to_owned()),
                        },
                        template: None,
                    },
                    Step::AddEnvironment {
                        name: env("prod"),
                        kind: DeployKind::Compose,
                    },
                    Step::AddDomain {
                        host: host("site.apps.test"),
                        label: Some(Label::new("site").unwrap()),
                    },
                    bind_web_3000("site.apps.test"),
                    Step::IssueToken,
                    Step::StoreSecrets(vec![
                        place(REGISTRY_USER_KEY, ScopeKind::Project, true),
                        place(REGISTRY_TOKEN_KEY, ScopeKind::Project, true),
                    ]),
                    Step::SyncCi,
                    Step::WriteLiftoffFile,
                ],
            }
        );
    }

    // TC: CRE-09
    #[tokio::test]
    async fn writes_and_commits_detected_manifest_when_repo_has_none() {
        let world = World::new().await;
        let tree = Files(BTreeMap::from([(
            "deploy/compose.yaml",
            "services:\n  web:\n    image: git.example.com/owner/site:${TAG}\n    expose:\n      - \"3000\"\n",
        )]));
        let detected = StackDetector::detect(&tree).manifest().unwrap();
        let existing = site(&world, ExistingManifest::Detected(detected));
        let plan = world
            .create()
            .plan_for_existing(
                &world.owner,
                project("site"),
                existing,
                CreateOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            plan.steps.get(..5).map(<[Step]>::to_vec),
            Some(vec![
                Step::WriteManifest {
                    content: SITE.to_owned(),
                },
                Step::AskSecrets(vec![registry_user_ask(), registry_token_ask()]),
                Step::WriteCiSnippet(forgejo_snippet(SITE)),
                Step::Commit {
                    message: "chore: set up liftoff".to_owned(),
                },
                Step::Push,
            ])
        );
    }

    // TC: CRE-17 (an existing repository's project goes to the tenant
    // too)
    #[tokio::test]
    async fn creates_project_in_tenant_when_options_name_one() {
        let world = World::new().await;
        admit_tenant(&world);
        let existing = site(&world, ExistingManifest::Committed(manifest(SITE)));
        let options = CreateOptions {
            tenant: Some(TenantSlug::new("acme").unwrap()),
            ..CreateOptions::default()
        };
        let plan = world
            .create()
            .plan_for_existing(&world.owner, project("site"), existing, options)
            .await
            .unwrap();
        assert_eq!(
            tenant_of(&plan),
            vec![Some(TenantSlug::new("acme").unwrap())]
        );
    }

    // TC: CRE-10
    #[tokio::test]
    async fn conflict_naming_the_project_when_repo_already_is_one() {
        let world = World::new().await;
        let existing = ExistingRepo {
            git_host: host(World::GIT_HOST),
            repo: repo("owner/shop"),
            manifest: ExistingManifest::Committed(manifest(SITE)),
        };
        let result = world
            .create()
            .plan_for_existing(
                &world.owner,
                project("shop2"),
                existing,
                CreateOptions::default(),
            )
            .await;
        assert!(
            matches!(&result, Err(Error::Conflict(why)) if why.contains("project shop")),
            "{result:?}"
        );
    }
}

mod generate {
    use super::*;

    // TC: LCL-03 (the forge generates the plan's repository from its template as the actor's own connection)
    #[tokio::test]
    async fn generates_target_from_template_when_plan_has_generate_step() {
        let world = World::new().await;
        let plan = plan_blog(&world, CreateOptions::default()).await.unwrap();
        let created = world.create().generate(&world.owner, &plan).await;
        let generated: Vec<GitCall> = world
            .ports
            .git
            .calls()
            .into_iter()
            .filter(|call| matches!(call, GitCall::GenerateFromTemplate { .. }))
            .collect();
        assert_eq!(
            (created.ok(), generated),
            (
                Some(repo("owner/blog")),
                vec![GitCall::GenerateFromTemplate {
                    login: world.owner.login.clone(),
                    template: repo("acme/app"),
                    target: "owner/blog".to_owned(),
                }]
            )
        );
    }

    // TC: LCL-03 (a refusal reaches no forge)
    #[tokio::test]
    async fn generates_nothing_when_authorize_denies() {
        let world = World::new().await;
        let plan = plan_blog(&world, CreateOptions::default()).await.unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("no".to_owned()));
        let refused = world.create().generate(&world.owner, &plan).await;
        let generated = world
            .ports
            .git
            .calls()
            .into_iter()
            .filter(|call| matches!(call, GitCall::GenerateFromTemplate { .. }))
            .count();
        assert_eq!(
            (matches!(refused, Err(Error::Forbidden(_))), generated),
            (true, 0),
            "{refused:?}"
        );
    }

    // TC: CRE-09 (an existing repository has nothing to generate)
    #[tokio::test]
    async fn answers_plan_repo_when_plan_has_no_generate_step() {
        let world = World::new().await;
        let mut plan = blog_plan();
        plan.steps
            .retain(|step| !matches!(step, Step::GenerateRepo { .. }));
        let created = world.create().generate(&world.owner, &plan).await;
        assert_eq!(created.ok(), Some(repo("owner/blog")));
    }
}

mod generate_from_template {
    use super::*;

    /// `generate_from_template` of `blog` from `acme/app` as `owner`.
    async fn generate_blog(world: &World, name: &str) -> Result<GeneratedRepo, Error> {
        publish(world, "acme/app", APP_TEMPLATE);
        world
            .create()
            .generate_from_template(
                &world.owner,
                project(name),
                on_forge("acme/app"),
                CreateOptions::default(),
            )
            .await
    }

    fn generations(world: &World) -> Vec<GitCall> {
        world
            .ports
            .git
            .calls()
            .into_iter()
            .filter(|call| matches!(call, GitCall::GenerateFromTemplate { .. }))
            .collect()
    }

    // TC: CRE-12 (the plan's repository, generated on its host as the actor's own connection)
    #[tokio::test]
    async fn generates_planned_repository_when_template_given() {
        let world = World::new().await;
        let generated = generate_blog(&world, "blog").await;
        assert_eq!(
            (generated.ok(), generations(&world)),
            (
                Some(GeneratedRepo {
                    git_host: host(World::GIT_HOST),
                    repo: repo("owner/blog"),
                }),
                vec![GitCall::GenerateFromTemplate {
                    login: world.owner.login.clone(),
                    template: repo("acme/app"),
                    target: "owner/blog".to_owned(),
                }]
            )
        );
    }

    // TC: CRE-02 (a name taken since the plan was shown is still a
    // conflict, and the forge generates nothing)
    #[tokio::test]
    async fn conflict_without_generating_when_name_is_taken() {
        let world = World::new().await;
        let generated = generate_blog(&world, "shop").await;
        assert_eq!(
            (
                matches!(&generated, Err(Error::Conflict(why)) if why.contains("shop")),
                generations(&world)
            ),
            (true, vec![]),
            "{generated:?}"
        );
    }
}

mod local_existing_steps {
    use super::*;

    // TC: CRE-09 (without a server a detected manifest is written, committed alone and pushed)
    #[test]
    fn writes_commits_and_pushes_manifest_when_detected() {
        assert_eq!(
            local_existing_steps(&ExistingManifest::Detected(manifest(SITE))),
            vec![
                Step::WriteManifest {
                    content: SITE.to_owned(),
                },
                Step::Commit {
                    message: ADD_MANIFEST_MESSAGE.to_owned(),
                },
                Step::Push,
            ]
        );
    }

    // TC: CRE-09 (a committed manifest is ready as it is)
    #[test]
    fn does_nothing_when_manifest_is_committed() {
        assert_eq!(
            local_existing_steps(&ExistingManifest::Committed(manifest(SITE))),
            vec![]
        );
    }

    // TC: CRE-09 (the commit of a detected manifest alone)
    #[test]
    fn names_manifest_in_its_commit_message() {
        assert_eq!(ADD_MANIFEST_MESSAGE, "chore: add liftoff.toml");
    }
}

mod side {
    use super::*;

    // TC: CRE-01, LCL-03 (local mode stops before the server steps)
    #[test]
    fn splits_forge_local_and_server_steps_when_plan_from_template() {
        let sides: Vec<Side> = blog_plan().steps.iter().map(Step::side).collect();
        assert_eq!(
            sides,
            vec![
                Side::Forge,
                Side::Local,
                Side::Local,
                Side::Local,
                Side::Local,
                Side::Local,
                Side::Local,
                Side::Server,
                Side::Server,
                Side::Server,
                Side::Server,
                Side::Server,
                Side::Server,
                Side::Server,
                Side::Local,
            ]
        );
    }

    // TC: CRE-09
    #[test]
    fn runs_write_manifest_locally() {
        let step = Step::WriteManifest {
            content: SITE.to_owned(),
        };
        assert_eq!(step.side(), Side::Local);
    }
}

mod execute {
    use super::*;

    // TC: CRE-01 (the task's criterion: a World-like picture)
    #[tokio::test]
    async fn creates_project_environment_domain_binding_token_and_secrets_when_executed() {
        let world = World::new().await;
        let created = execute_blog(&world, "main").await;
        let store = &world.ports.store;
        let blog = stored_blog(&world).await;
        let environments = store.environments_of_project(blog.id).await.unwrap();
        let domains = store.domains_of_project(blog.id).await.unwrap();
        let bindings = store
            .bindings_of_environment(created.environment.id)
            .await
            .unwrap();
        let token = store.live_deploy_token(blog.id).await.unwrap();
        assert_eq!(
            (
                (blog.name.to_string(), blog.template.clone()),
                environments
                    .iter()
                    .map(|e| e.name.to_string())
                    .collect::<Vec<_>>(),
                domains
                    .iter()
                    .map(|d| (d.host.to_string(), d.kind))
                    .collect::<Vec<_>>(),
                bindings
                    .iter()
                    .map(|b| (
                        b.service.to_string(),
                        b.container_port.get(),
                        b.domain_id == created.domain.id
                    ))
                    .collect::<Vec<_>>(),
                token.map(|t| t.id),
            ),
            (
                (
                    "blog".to_owned(),
                    Some(TemplateRef {
                        group: "web".to_owned(),
                        name: "acme-app".to_owned(),
                    })
                ),
                vec!["prod".to_owned()],
                vec![("blog.apps.test".to_owned(), DomainKind::Managed)],
                vec![("web".to_owned(), 3000, true)],
                Some(created.token.id),
            )
        );
    }

    // TC: CRE-01 (every server step goes through the service that owns
    // it, which asks `authorize` once about its own resource)
    #[tokio::test]
    async fn asks_authorize_for_every_server_step_when_executed() {
        let world = World::new().await;
        let created = execute_blog(&world, "main").await;
        let owner = world.owner.id;
        let blog = Resource::Project(created.project.id);
        let project_scope = Resource::Secret(SecretScope::Project(created.project.id));
        assert_eq!(
            world.log().authorized(),
            vec![
                (owner, Action::ProjectCreate, Resource::Admin),
                (owner, Action::EnvironmentAdd, blog.clone()),
                (owner, Action::DomainAddManaged, blog.clone()),
                (
                    owner,
                    Action::BindingBind,
                    Resource::Environment(created.environment.id)
                ),
                (owner, Action::DeployRotateToken, blog),
                (
                    owner,
                    Action::SecretSet,
                    Resource::Secret(SecretScope::Environment(created.environment.id))
                ),
                (owner, Action::SecretSet, project_scope.clone()),
                (owner, Action::SecretSet, project_scope.clone()),
                (owner, Action::SecretSet, project_scope.clone()),
                (owner, Action::SecretSync, project_scope),
            ]
        );
    }

    // TC: CRE-01 (a refused `CreateProject` creates nothing)
    #[tokio::test]
    async fn forbidden_and_no_project_stored_when_authorize_denies() {
        let world = World::new().await;
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("owner/blog"), RepoRole::Admin);
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("no".to_owned()));
        let result = world
            .create()
            .execute(&world.owner, &blog_plan(), outcome("main"))
            .await;
        let stored = world
            .ports
            .store
            .project_by_repo(&host(World::GIT_HOST), &repo("owner/blog"))
            .await
            .unwrap();
        assert!(
            matches!(&result, Err(Error::Forbidden(_))) && stored.is_none(),
            "{result:?}, {stored:?}"
        );
    }

    // TC: CRE-07 (`--domain` outside the base domain: an external domain,
    // checked like `domain add`, and the binding points at it)
    #[tokio::test]
    async fn adds_external_domain_and_binds_it_when_domain_flag_is_outside_the_base() {
        let world = World::new().await;
        world
            .ports
            .resolver
            .set_cname(&host("www.blog.test"), vec![host("apps.test")]);
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("owner/blog"), RepoRole::Admin);
        let options = CreateOptions {
            domain: Some(host("www.blog.test")),
            ..CreateOptions::default()
        };
        let create = world.create();
        let plan = plan_blog(&world, options).await.unwrap();
        let created = create
            .execute(&world.owner, &plan, outcome("main"))
            .await
            .unwrap();
        let domains = world
            .ports
            .store
            .domains_of_project(created.project.id)
            .await
            .unwrap();
        assert_eq!(
            (
                domains
                    .iter()
                    .map(|d| (d.host.to_string(), d.kind))
                    .collect::<Vec<_>>(),
                created.binding.map(|b| b.domain_id),
            ),
            (
                vec![("www.blog.test".to_owned(), DomainKind::External)],
                Some(created.domain.id)
            )
        );
    }

    // TC: CRE-06
    #[tokio::test]
    async fn pins_prod_to_default_branch_by_actor_when_executed() {
        let world = World::new().await;
        let created = execute_blog(&world, "trunk").await;
        assert_eq!(
            (
                created.environment.name.to_string(),
                created.environment.kind,
                created.environment.pin
            ),
            (
                "prod".to_owned(),
                EnvKind::Compose,
                Some(Pin {
                    branch: Branch::new("trunk").unwrap(),
                    pinned_by: world.owner.id,
                })
            )
        );
    }

    // TC: CRE-03 (the asked values land in the scopes of the manifest)
    #[tokio::test]
    async fn stores_secrets_in_their_scopes_when_executed() {
        let world = World::new().await;
        let created = execute_blog(&world, "main").await;
        let project_secrets: Vec<(String, bool, String)> =
            secrets_of(&world, SecretScope::Project(created.project.id))
                .await
                .into_iter()
                .filter(|(key, _, _)| {
                    ![DEPLOY_TOKEN_KEY, REGISTRY_USER_KEY, REGISTRY_TOKEN_KEY]
                        .contains(&key.as_str())
                })
                .collect();
        let prod_secrets =
            secrets_of(&world, SecretScope::Environment(created.environment.id)).await;
        assert_eq!(
            (project_secrets, prod_secrets),
            (
                vec![("SESSION_SECRET".to_owned(), false, "s3cr3t".to_owned())],
                vec![(
                    "DATABASE_URL".to_owned(),
                    false,
                    "postgres://db/blog".to_owned()
                )]
            )
        );
    }

    // TC: CRE-08
    #[tokio::test]
    async fn stores_deploy_token_as_ci_secret_when_executed() {
        let world = World::new().await;
        let created = execute_blog(&world, "main").await;
        let token = world
            .ports
            .store
            .secret(
                &SecretScope::Project(created.project.id),
                &key(DEPLOY_TOKEN_KEY),
            )
            .await
            .unwrap()
            .unwrap();
        let on_forge = world
            .ports
            .git
            .ci_secret(&repo("owner/blog"), &key(DEPLOY_TOKEN_KEY))
            .map(|value| value.expose().to_owned());
        assert_eq!(
            (token.ci, on_forge),
            (
                true,
                Some(world.ports.secrets.decrypt(&token.value).unwrap())
            )
        );
    }

    // TC: CRE-08 (`SyncCi` ran: it sent the registry login, the deploy
    // token was sent when it was issued)
    #[tokio::test]
    async fn reports_registry_login_synced_and_token_unchanged_when_executed() {
        let world = World::new().await;
        let created = execute_blog(&world, "main").await;
        assert_eq!(
            created.sync,
            SyncReport {
                synced: vec![key(REGISTRY_TOKEN_KEY), key(REGISTRY_USER_KEY)],
                unchanged: vec![key(DEPLOY_TOKEN_KEY)],
                failed: vec![],
            }
        );
    }

    // TC: CRE-08 (the Forgejo compose workflow logs in to the registry with the login the person gave)
    #[tokio::test]
    async fn sends_asked_registry_login_to_the_forge_when_forgejo_compose() {
        let world = World::new().await;
        let _created = execute_blog(&world, "main").await;
        let git = &world.ports.git;
        let plain = |name: &str| {
            git.ci_secret(&repo("owner/blog"), &key(name))
                .map(|value| value.expose().to_owned())
        };
        assert_eq!(
            (plain(REGISTRY_USER_KEY), plain(REGISTRY_TOKEN_KEY)),
            (
                Some(REGISTRY_USER_VALUE.to_owned()),
                Some(REGISTRY_TOKEN_VALUE.to_owned())
            )
        );
    }

    // TC: CRE-08 (the registry login is stored as `ci` project secrets)
    #[tokio::test]
    async fn stores_asked_registry_login_as_ci_project_secrets_when_executed() {
        let world = World::new().await;
        let created = execute_blog(&world, "main").await;
        let login: Vec<(String, bool, String)> =
            secrets_of(&world, SecretScope::Project(created.project.id))
                .await
                .into_iter()
                .filter(|(key, _, _)| key.starts_with("REGISTRY_"))
                .collect();
        assert_eq!(
            login,
            vec![
                (
                    REGISTRY_TOKEN_KEY.to_owned(),
                    true,
                    REGISTRY_TOKEN_VALUE.to_owned()
                ),
                (
                    REGISTRY_USER_KEY.to_owned(),
                    true,
                    REGISTRY_USER_VALUE.to_owned()
                ),
            ]
        );
    }

    // TC: CRE-08 (the actor's forge connection is used only on its behalf; no version of a project secret, which other members and every workflow of the repository can read, holds its token)
    #[tokio::test]
    async fn stores_no_connection_token_in_any_secret_when_executed() {
        let world = World::new().await;
        let created = execute_blog(&world, "main").await;
        let store = &world.ports.store;
        let mut leaked = Vec::new();
        for scope in [
            SecretScope::Project(created.project.id),
            SecretScope::Environment(created.environment.id),
        ] {
            for secret in store.secrets_of_scope(&scope).await.unwrap() {
                for version in store.secret_versions(secret.id).await.unwrap() {
                    if world.ports.secrets.decrypt(&version.value).unwrap() == World::OWNER_TOKEN {
                        leaked.push(secret.key.to_string());
                    }
                }
            }
        }
        let on_forge = world
            .ports
            .git
            .ci_secrets(&repo("owner/blog"))
            .into_iter()
            .filter(|(_, plain)| plain.expose() == World::OWNER_TOKEN)
            .count();
        assert_eq!((leaked, on_forge), (Vec::<String>::new(), 0));
    }

    // TC: CRE-08 (GitHub Actions log in with `GITHUB_TOKEN`)
    #[tokio::test]
    async fn sends_no_registry_login_when_forge_is_github() {
        let world = World::new().await;
        let sealed = world
            .ports
            .secrets
            .encrypt("github-token-of-owner")
            .unwrap();
        let mut connection = world
            .records
            .git_connection(&world.owner, "github.com", sealed)
            .unwrap();
        connection.provider = GitProvider::Github;
        world
            .ports
            .store
            .put_git_connection(&connection)
            .await
            .unwrap();
        world
            .ports
            .git
            .serve(&host("github.com"), GitProvider::Github);
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("owner/site"), RepoRole::Admin);
        let existing = ExistingRepo {
            git_host: host("github.com"),
            repo: repo("owner/site"),
            manifest: ExistingManifest::Committed(manifest(SITE)),
        };
        let create = world.create();
        let plan = create
            .plan_for_existing(
                &world.owner,
                project("site"),
                existing,
                CreateOptions::default(),
            )
            .await
            .unwrap();
        let created = create
            .execute(
                &world.owner,
                &plan,
                LocalOutcome {
                    default_branch: Branch::new("main").unwrap(),
                    secrets: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
        let git = &world.ports.git;
        assert_eq!(
            (
                created.sync,
                git.ci_secret(&repo("owner/site"), &key(REGISTRY_USER_KEY)),
                git.ci_secret(&repo("owner/site"), &key(REGISTRY_TOKEN_KEY)),
            ),
            (
                SyncReport {
                    synced: vec![],
                    unchanged: vec![key(DEPLOY_TOKEN_KEY)],
                    failed: vec![],
                },
                None,
                None
            )
        );
    }

    // TC: CRE-04 (a value missing in batch mode: nothing is created)
    #[tokio::test]
    async fn validation_naming_missing_keys_when_local_outcome_lacks_a_secret() {
        let world = World::new().await;
        let mut local = outcome("main");
        let _dropped = local.secrets.remove(&key("DATABASE_URL"));
        let result = world
            .create()
            .execute(&world.owner, &blog_plan(), local)
            .await;
        let stored = world
            .ports
            .store
            .project_by_repo(&host(World::GIT_HOST), &repo("owner/blog"))
            .await
            .unwrap();
        assert!(
            matches!(&result, Err(Error::Validation(v))
                if v.field == "secrets" && v.to_string().contains("DATABASE_URL"))
                && stored.is_none(),
            "{result:?}, {stored:?}"
        );
    }

    // TC: CRE-06 (a function plan adds a function environment and ties
    // the domain to it: there is no `[service] expose` to bind)
    #[tokio::test]
    async fn binds_domain_to_function_environment_when_plan_is_a_function() {
        let world = World::new().await;
        publish(&world, "acme/fn-ts", FN_TS);
        // What the CLI's `GenerateRepo` made of the owner.
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("owner/hello"), RepoRole::Admin);
        let create = world.create();
        let plan = create
            .plan_from_template(
                &world.owner,
                project("hello"),
                on_forge("acme/fn-ts"),
                CreateOptions::default(),
            )
            .await
            .unwrap();
        let created = create
            .execute(
                &world.owner,
                &plan,
                LocalOutcome {
                    default_branch: Branch::new("main").unwrap(),
                    secrets: BTreeMap::new(),
                },
            )
            .await
            .unwrap();
        let bound = world
            .ports
            .store
            .binding_by_domain(created.domain.id)
            .await
            .unwrap();
        assert_eq!(
            (
                created.environment.kind,
                bound.map(|binding| binding.environment_id),
                created.binding.map(|binding| binding.domain_id),
            ),
            (
                EnvKind::Function,
                Some(created.environment.id),
                Some(created.domain.id),
            )
        );
    }

    // TC: CRE-03 (a template cannot overwrite a secret its tenant already
    // shares with other projects)
    #[tokio::test]
    async fn keeps_existing_tenant_secret_when_executed() {
        let world = World::new().await;
        let tenant = TenantId::new(crate::ports::Rng::ulid(&world.ports.rng));
        world.ports.hooks.answer_project_tenant(Ok(Some(tenant)));
        let mut shop = world.shop.clone();
        shop.tenant_id = Some(tenant);
        world.ports.store.update_project(&shop).await.unwrap();
        let sealed = world.ports.secrets.encrypt("shared").unwrap();
        let mut shared = world
            .records
            .secret(
                SecretScope::Tenant(tenant),
                "SMTP_URL",
                sealed,
                &world.owner,
            )
            .unwrap();
        shared.ci = true;
        world.ports.store.put_secret(&shared).await.unwrap();
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("owner/blog"), RepoRole::Admin);
        let mut plan = blog_plan();
        for step in &mut plan.steps {
            if let Step::StoreSecrets(places) = step {
                places.push(place("SMTP_URL", ScopeKind::Tenant, false));
            }
        }
        let mut local = outcome("main");
        let _none = local
            .secrets
            .insert(key("SMTP_URL"), value("from-template"));
        let created = world
            .create()
            .execute(&world.owner, &plan, local)
            .await
            .unwrap();
        assert_eq!(
            (
                created.kept,
                secrets_of(&world, SecretScope::Tenant(tenant)).await
            ),
            (
                vec![key("SMTP_URL")],
                vec![("SMTP_URL".to_owned(), true, "shared".to_owned())]
            )
        );
    }

    // TC: CRE-08 (only `ci = true` reaches the forge)
    #[tokio::test]
    async fn sends_no_asked_secret_to_the_forge_when_executed() {
        let world = World::new().await;
        let _created = execute_blog(&world, "main").await;
        let git = &world.ports.git;
        assert_eq!(
            (
                git.ci_secret(&repo("owner/blog"), &key("SESSION_SECRET")),
                git.ci_secret(&repo("owner/blog"), &key("DATABASE_URL"))
            ),
            (None, None)
        );
    }
}

mod generate_value {
    use super::*;

    // TC: CRE-03 (URL-safe base64 without padding of the random bytes)
    #[test]
    fn encodes_random_bytes_as_unpadded_base64url_when_random_3() {
        let generated = generate_value(Generate::Random(3), |buf| buf.fill(0xAB));
        assert_eq!(generated.expose(), "q6ur");
    }

    // TC: CRE-03 (`random:<n>` draws exactly n bytes)
    #[test]
    fn draws_n_bytes_when_random_32() {
        let mut drawn = Vec::new();
        let generated = generate_value(Generate::Random(32), |buf| {
            drawn.push(buf.len());
            buf.fill(0);
        });
        assert_eq!(
            (drawn, generated.expose().to_owned()),
            (vec![32], "A".repeat(43))
        );
    }
}

mod answer_secrets {
    use super::*;

    fn ask(name: &str, generate: Option<Generate>) -> SecretAsk {
        SecretAsk {
            key: key(name),
            hint: None,
            generate,
            scope: ScopeKind::Project,
            ci: false,
        }
    }

    fn plain(answers: &BTreeMap<SecretKey, SecretValue>) -> Vec<(String, String)> {
        answers
            .iter()
            .map(|(key, value)| (key.to_string(), value.expose().to_owned()))
            .collect()
    }

    // TC: CRE-03 (a key with `generate` gets a generated value)
    #[test]
    fn generates_value_when_key_has_generate_and_none_given() {
        let asks = [ask("SESSION_SECRET", Some(Generate::Random(3)))];
        let answers = answer_secrets(&asks, BTreeMap::new(), |buf| buf.fill(0xAB)).unwrap();
        assert_eq!(
            plain(&answers),
            vec![("SESSION_SECRET".to_owned(), "q6ur".to_owned())]
        );
    }

    // TC: CRE-03 (`--secret K=V` wins over `generate`)
    #[test]
    fn takes_given_value_when_key_has_generate() {
        let asks = [ask("SESSION_SECRET", Some(Generate::Random(3)))];
        let given = BTreeMap::from([(key("SESSION_SECRET"), value("mine"))]);
        let answers = answer_secrets(&asks, given, |buf| buf.fill(0xAB)).unwrap();
        assert_eq!(
            plain(&answers),
            vec![("SESSION_SECRET".to_owned(), "mine".to_owned())]
        );
    }

    // TC: CRE-04
    #[test]
    fn lists_missing_required_keys_by_key_when_not_given() {
        let asks = [
            ask("SESSION_SECRET", Some(Generate::Random(3))),
            ask("STRIPE_KEY", None),
            ask("DATABASE_URL", None),
        ];
        let result = answer_secrets(&asks, BTreeMap::new(), |buf| buf.fill(0));
        assert_eq!(
            result,
            Err(MissingSecrets(vec![key("DATABASE_URL"), key("STRIPE_KEY")]))
        );
    }

    // TC: CRE-04 (what the CLI prints with exit 64)
    #[test]
    fn names_every_missing_key_when_displayed() {
        let missing = MissingSecrets(vec![key("DATABASE_URL"), key("STRIPE_KEY")]);
        assert_eq!(
            missing.to_string(),
            "missing values for the secrets DATABASE_URL, STRIPE_KEY"
        );
    }

    // TC: CRE-04
    #[test]
    fn answers_every_key_when_required_ones_are_given() {
        let asks = [ask("DATABASE_URL", None)];
        let given = BTreeMap::from([(key("DATABASE_URL"), value("postgres://db/blog"))]);
        let answers = answer_secrets(&asks, given, |buf| buf.fill(0)).unwrap();
        assert_eq!(
            plain(&answers),
            vec![("DATABASE_URL".to_owned(), "postgres://db/blog".to_owned())]
        );
    }
}

mod substitution {
    use super::*;

    // TC: LCL-03 (local mode: the domain is the server's, `{{domain}}`
    // stays)
    #[test]
    fn replaces_project_only_when_domain_is_not_known_yet() {
        let substitution = Substitution {
            files: vec![path("README.md")],
            project: project("blog"),
            domain: host("blog.apps.test"),
        };
        assert_eq!(
            substitution.apply_project_to(&path("README.md"), "# {{project}} at {{domain}}\n"),
            Some("# blog at {{domain}}\n".to_owned())
        );
    }

    // TC: LCL-03, CRE-05 (a file the manifest does not list is left alone)
    #[test]
    fn leaves_unlisted_file_alone_when_domain_is_not_known_yet() {
        let substitution = Substitution {
            files: vec![path("README.md")],
            project: project("blog"),
            domain: host("blog.apps.test"),
        };
        assert_eq!(
            substitution.apply_project_to(&path("package.json"), "{{project}}"),
            None
        );
    }

    fn blog() -> Substitution {
        Substitution {
            files: vec![path("README.md"), path("deploy/compose.yaml")],
            project: project("blog"),
            domain: host("blog.apps.test"),
        }
    }

    // TC: CRE-05
    #[test]
    fn replaces_project_and_domain_when_file_is_listed() {
        let replaced = blog().apply_to(
            &path("README.md"),
            "# {{project}}\n\nhttps://{{domain}}/ and {{project}} again\n",
        );
        assert_eq!(
            replaced,
            Some("# blog\n\nhttps://blog.apps.test/ and blog again\n".to_owned())
        );
    }

    // TC: CRE-05 (only the listed files)
    #[test]
    fn leaves_file_alone_when_not_listed() {
        let replaced = blog().apply_to(&path("package.json"), "{\"name\": \"{{project}}\"}\n");
        assert_eq!(replaced, None);
    }

    // TC: CRE-05 (no template engine: other braces stay)
    #[test]
    fn keeps_other_placeholders_when_applied() {
        let replaced = blog().apply("image: {{registry}}/{{project}}:${TAG}\n");
        assert_eq!(replaced, "image: {{registry}}/blog:${TAG}\n");
    }
}
