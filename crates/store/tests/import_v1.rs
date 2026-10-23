#![cfg(test)]
//! Import of a v1 database:
//! `import_v1` over the fixture `fixtures/v1.sql`, results read back
//! through the `Store` port of the SQLite store.
//!
//! The fixture (ids end in a letter naming the table and a number):
//!
//! - users `alice` (admin, `sub-alice`) and `bob` (user, `sub-bob`);
//! - tenants `acme` (owner alice, member bob) and `beta` (owner bob);
//! - repository `acme/shop` with targets `prod` (main, 0.5 CPU, 512 MiB,
//!   running, 22 deploys: 1..21 succeeded, 22 failed) and `qa` (dev, 0.25
//!   CPU, 256 MiB, running, deploy 1 succeeded, 2 failed); repository
//!   `beta/shop` with target `prod` (failed, one failed deploy);
//! - domains `shop.apps.test` → `prod` `web:3000` on 20000,
//!   `shop-qa.apps.test` → `qa` `web:3000` on 20001 (both managed), and
//!   `www.beta.example` (external, verified) → beta `prod` `app:8080` on
//!   20002;
//! - deploy tokens of `acme/shop` (one live, one revoked) and `beta/shop`.
//!
//! A rule the base rows do not reach runs on the fixture plus a few rows
//! of SQL ([`v1_db_with`]); each variant is a constant below.

use std::path::PathBuf;
use std::sync::Arc;

use liftoff_core::Error;
use liftoff_core::domain::{
    Branch, ComposeProject, Cpus, DeployId, DeployNumber, DeploySource, DeployStatus,
    DeployTokenId, DeployTokenSecret, DomainId, DomainKind, EnvKind, EnvState, Environment,
    EnvironmentId, Host, HostPort, ImageTag, Limits, Login, MemoryMb, OidcIssuer, Project, RepoRef,
    SecretScope, SliceName, TenantId, Timestamp, Trigger, UpdatedBy, UserId, UserRole, UserSource,
    VersionNumber, Visibility,
};
use liftoff_core::fakes::{FakePorts, FakeSecrets, FixedClock, Records, SeqRng, WithStore, World};
use liftoff_core::ports::{ProjectFile, Secrets as _, Store as _};
use liftoff_core::service::{CiDeployRequest, DeployService};
use liftoff_store::{
    ImportedDomain, ImportedMember, Renamed, SqliteStore, TenantRole, V1Import, import_v1,
};

/// The v1 fixture: schema of the v1 migrations plus the rows above.
const V1_SQL: &str = include_str!("fixtures/v1.sql");

/// The issuer the v1 users logged in with.
const ISSUER: &str = "https://auth.example.com";
/// The forge of the v1 repositories.
const GIT_HOST: &str = "git.example.com";

/// The live v1 deploy token of `acme/shop` (its hash is in the fixture).
const ACME_TOKEN: &str = "lf_AcmeShopDeployToken000000000000000000000";
/// The revoked v1 deploy token of `acme/shop`.
const ACME_REVOKED_TOKEN: &str = "lf_AcmeShopRevokedToken00000000000000000000";
/// The live v1 deploy token of `beta/shop`.
const BETA_TOKEN: &str = "lf_BetaShopDeployToken000000000000000000000";

const ALICE: &str = "01HV000000000000000000A001";
const BOB: &str = "01HV000000000000000000A002";
const ACME: &str = "01HV000000000000000000B001";
const BETA: &str = "01HV000000000000000000B002";
const ACME_PROD: &str = "01HV000000000000000000C001";
const ACME_QA: &str = "01HV000000000000000000C002";
const BETA_PROD: &str = "01HV000000000000000000C003";
const SHOP_DOMAIN: &str = "01HV000000000000000000D001";
const SHOP_QA_DOMAIN: &str = "01HV000000000000000000D002";
const BETA_DOMAIN: &str = "01HV000000000000000000D003";
const ACME_TOKEN_ID: &str = "01HV000000000000000000F001";
const ACME_REVOKED_TOKEN_ID: &str = "01HV000000000000000000F002";
const BETA_TOKEN_ID: &str = "01HV000000000000000000F003";
const ACME_PROD_DEPLOY_21: &str = "01HV00000000000000000G0021";
const ACME_QA_DEPLOY_1: &str = "01HV00000000000000000H0001";

/// Variant: acme gets two more repositories, `acme/web` (target
/// `staging`) and `acme/beta` (target `preview`, whose name tenant `beta`
/// holds), both newer than `acme/shop`.
const TWO_MORE_REPOS: &str = "
    INSERT INTO targets (id, tenant_id, name, repo, branch, pinned_by, cpus_hundredths, memory_mb, state, current_deploy_id, created_at, sso) VALUES
        ('01HV000000000000000000C004', '01HV000000000000000000B001', 'staging', 'acme/web',  'main', '01HV000000000000000000A001', 50, 512, 'empty', NULL, '2026-09-08T10:00:00Z', 0),
        ('01HV000000000000000000C005', '01HV000000000000000000B001', 'preview', 'acme/beta', 'main', '01HV000000000000000000A001', 50, 512, 'empty', NULL, '2026-09-09T10:00:00Z', 0);
";
const ACME_STAGING: &str = "01HV000000000000000000C004";
const ACME_PREVIEW: &str = "01HV000000000000000000C005";

/// Variant: beta also pins `acme/shop`, as target `qa` (the name of an
/// acme target of that repository), older than acme's `qa`.
const BETA_PINS_ACME_SHOP: &str = "
    UPDATE targets SET created_at = '2026-09-08T10:00:00Z' WHERE id = '01HV000000000000000000C002';
    INSERT INTO targets (id, tenant_id, name, repo, branch, pinned_by, cpus_hundredths, memory_mb, state, current_deploy_id, created_at, sso) VALUES
        ('01HV000000000000000000C008', '01HV000000000000000000B002', 'qa', 'acme/shop', 'beta', '01HV000000000000000000A002', 50, 512, 'empty', NULL, '2026-09-07T12:00:00Z', 0);
";
const BETA_QA: &str = "01HV000000000000000000C008";

/// Variant: the binding on port 20000 was deleted in v1; the cursor stays
/// at 20002.
const FREED_FIRST_PORT: &str = "
    DELETE FROM bindings WHERE id = '01HV000000000000000000E001';
";

/// Variant: acme gets `scratch`, an unpinned target that was never
/// deployed (v1 `target create` without `--repo`).
const UNPINNED_TARGET: &str = "
    INSERT INTO targets (id, tenant_id, name, repo, branch, pinned_by, cpus_hundredths, memory_mb, state, current_deploy_id, created_at, sso) VALUES
        ('01HV000000000000000000C006', '01HV000000000000000000B001', 'scratch', NULL, NULL, NULL, 50, 512, 'empty', NULL, '2026-09-08T10:00:00Z', 0);
";
const ACME_SCRATCH: &str = "01HV000000000000000000C006";

/// Tenant `gamma` (owner bob) with one unpinned target `site`.
const GAMMA_SITE: &str = "
    INSERT INTO tenants (id, slug, name, created_by, max_targets, target_cpus_hundredths, target_memory_mb, created_at) VALUES
        ('01HV000000000000000000B003', 'gamma', 'Gamma', '01HV000000000000000000A002', 3, 100, 1024, '2026-09-09T10:00:00Z');
    INSERT INTO memberships (tenant_id, user_id, role) VALUES ('01HV000000000000000000B003', '01HV000000000000000000A002', 'owner');
    INSERT INTO targets (id, tenant_id, name, repo, branch, pinned_by, cpus_hundredths, memory_mb, state, current_deploy_id, created_at, sso) VALUES
        ('01HV000000000000000000C007', '01HV000000000000000000B003', 'site', NULL, NULL, NULL, 50, 512, 'running', NULL, '2026-09-09T11:00:00Z', 0);
";
const GAMMA_SITE_ID: &str = "01HV000000000000000000C007";

/// A deploy of gamma's `site`: number, source, repo (SQL) and status.
fn gamma_deploy(number: u32, source: &str, repo: &str) -> String {
    format!(
        "INSERT INTO deploys (id, target_id, number, source, repo, git_ref, tag, compose, env, status, log, error, rolled_back_to, trigger_kind, trigger_id, started_at, finished_at) VALUES
            ('01HV00000000000000000M000{number}', '01HV000000000000000000C007', {number}, '{source}', {repo}, NULL, 'sha-g{number}',
             'services: {{}}', 'sealed:', 'succeeded', '', NULL, NULL, 'user', '01HV000000000000000000A002', NULL, NULL);"
    )
}

/// Variant: gamma pins nothing; its `site` was deployed by CI from
/// `gamma/old`, then from `gamma/site`, then from the CLI (no repo).
fn gamma_deployed_from_ci() -> String {
    [
        GAMMA_SITE.to_owned(),
        gamma_deploy(1, "ci", "'gamma/old'"),
        gamma_deploy(2, "ci", "'gamma/site'"),
        gamma_deploy(3, "cli", "NULL"),
    ]
    .concat()
}

/// Variant: gamma pins nothing and its `site` was only deployed from the
/// CLI: no deploy names a repository.
fn gamma_deployed_from_cli() -> String {
    [GAMMA_SITE.to_owned(), gamma_deploy(1, "cli", "NULL")].concat()
}

/// Variant: acme owns `spare.apps.test`, bound to nothing.
const UNBOUND_DOMAIN: &str = "
    INSERT INTO domains (id, tenant_id, host, kind, proxied, cf_record_id, verified_at, created_at) VALUES
        ('01HV000000000000000000D004', '01HV000000000000000000B001', 'spare.apps.test', 'managed', 1, 'cf-record-4', NULL, '2026-09-09T11:00:00Z');
";

/// Variant: tenant gamma has no targets, only the domain
/// `gamma.apps.test`.
const DOMAIN_WITHOUT_TARGETS: &str = "
    INSERT INTO tenants (id, slug, name, created_by, max_targets, target_cpus_hundredths, target_memory_mb, created_at) VALUES
        ('01HV000000000000000000B003', 'gamma', 'Gamma', '01HV000000000000000000A002', 3, 100, 1024, '2026-09-09T10:00:00Z');
    INSERT INTO domains (id, tenant_id, host, kind, proxied, cf_record_id, verified_at, created_at) VALUES
        ('01HV000000000000000000D005', '01HV000000000000000000B003', 'gamma.apps.test', 'managed', 1, NULL, NULL, '2026-09-09T11:00:00Z');
";

/// Variant: a live deploy token of `acme/old`, a repository no target
/// pins.
const STRAY_TOKEN: &str = "
    INSERT INTO deploy_tokens (id, repo, token_hash, created_by, created_at, revoked_at) VALUES
        ('01HV000000000000000000F004', 'acme/old', X'00000000000000000000000000000000000000000000000000000000000000ff', '01HV000000000000000000A001', '2026-09-09T12:00:00Z', NULL);
";

/// Variant: the current deploy of acme `prod` is its first, older than
/// the newest 20 (a rollback that stuck).
const OLD_CURRENT_DEPLOY: &str = "
    UPDATE targets SET current_deploy_id = '01HV00000000000000000G0001' WHERE id = '01HV000000000000000000C001';
";

/// Variant: the env v1 accepted has keys v2 does not (`node_env` in acme
/// `prod`, `_FOO` in acme `qa`, both in their last succeeded deploy).
const INVALID_ENV_KEYS: &str = "
    UPDATE deploys SET env = 'sealed:' || lower(hex('node_env=production' || char(10)))
        WHERE id = '01HV00000000000000000G0021';
    UPDATE deploys SET env = 'sealed:' || lower(hex('_FOO=1' || char(10)))
        WHERE id = '01HV00000000000000000H0001';
";

/// A v1 database file built from the fixture, in its own directory.
struct V1Db {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn v1_db() -> V1Db {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("liftoff-v1.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(V1_SQL)
        .unwrap();
    V1Db { _dir: dir, path }
}

/// The import options of every test: fake secrets (the fixture's env files
/// are sealed the fake's way), a counting rng and the fixed clock.
fn options() -> V1Import<FakeSecrets, SeqRng, FixedClock> {
    V1Import {
        issuer: OidcIssuer::new(ISSUER).unwrap(),
        git_host: host(GIT_HOST),
        secrets: FakeSecrets::new(),
        rng: SeqRng::new(),
        clock: FixedClock::default(),
    }
}

/// An empty v2 store with the fixture imported.
async fn imported() -> SqliteStore {
    let v1 = v1_db();
    let store = SqliteStore::open_in_memory().unwrap();
    import_v1(&v1.path, &store, options()).await.unwrap();
    store
}

/// The fixture with `extra` run after it: a variant for one rule.
fn v1_db_with(extra: &str) -> V1Db {
    let v1 = v1_db();
    rusqlite::Connection::open(&v1.path)
        .unwrap()
        .execute_batch(extra)
        .unwrap();
    v1
}

/// An empty v2 store with the fixture plus `extra` imported.
async fn imported_with(extra: &str) -> SqliteStore {
    let v1 = v1_db_with(extra);
    let store = SqliteStore::open_in_memory().unwrap();
    import_v1(&v1.path, &store, options()).await.unwrap();
    store
}

/// The text of the internal error the import of the fixture plus `extra`
/// is refused with, and the store it left.
async fn refused_with(extra: &str) -> (String, SqliteStore) {
    let v1 = v1_db_with(extra);
    let store = SqliteStore::open_in_memory().unwrap();
    match import_v1(&v1.path, &store, options()).await {
        Err(Error::Internal(source)) => (source.to_string(), store),
        other => panic!("expected an internal refusal, got {other:?}"),
    }
}

/// The compose project and slice environment `id` runs as.
async fn compose_of(store: &SqliteStore, id: &str) -> (String, String) {
    let environment = environment(store, id).await;
    let project = store
        .project(environment.project_id)
        .await
        .unwrap()
        .unwrap();
    (
        ComposeProject::new(&project.name, &environment.name).to_string(),
        SliceName::new(&project.name, &environment.name).to_string(),
    )
}

fn host(raw: &str) -> Host {
    Host::new(raw).unwrap()
}

fn repo(raw: &str) -> RepoRef {
    RepoRef::new(raw).unwrap()
}

fn user_id(raw: &str) -> UserId {
    UserId::parse(raw).unwrap()
}

fn env_id(raw: &str) -> EnvironmentId {
    EnvironmentId::parse(raw).unwrap()
}

fn domain_id(raw: &str) -> DomainId {
    DomainId::parse(raw).unwrap()
}

fn token_id(raw: &str) -> DeployTokenId {
    DeployTokenId::parse(raw).unwrap()
}

fn at(raw: &str) -> Timestamp {
    Timestamp::parse(raw, &time::format_description::well_known::Rfc3339).unwrap()
}

fn number(raw: u32) -> DeployNumber {
    DeployNumber::new(raw).unwrap()
}

async fn project_of(store: &SqliteStore, raw_repo: &str) -> Project {
    store
        .project_by_repo(&host(GIT_HOST), &repo(raw_repo))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no project of {raw_repo}"))
}

async fn environment(store: &SqliteStore, id: &str) -> Environment {
    store
        .environment(env_id(id))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no environment {id}"))
}

/// The environment secrets of `id` as `(key, plaintext)`, by key.
async fn env_secrets(store: &SqliteStore, id: &str) -> Vec<(String, String)> {
    let secrets = FakeSecrets::new();
    store
        .secrets_of_scope(&SecretScope::Environment(env_id(id)))
        .await
        .unwrap()
        .into_iter()
        .map(|secret| {
            (
                secret.key.as_str().to_owned(),
                secrets.decrypt(&secret.value).unwrap(),
            )
        })
        .collect()
}

mod projects {
    use super::*;

    // TC: IMP-01 (the project of a tenant's only repository takes the tenant's slug, so `lf_<tenant>_<target>` stays)
    #[tokio::test]
    async fn names_project_after_tenant_when_tenant_has_one_repo() {
        let store = imported().await;
        let shop = project_of(&store, "acme/shop").await;
        assert_eq!(
            (
                shop.name.as_str(),
                shop.git_host.as_str(),
                shop.repo.to_string().as_str()
            ),
            ("acme", "git.example.com", "acme/shop")
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn imports_one_project_per_repo_when_two_repos() {
        let store = imported().await;
        let repos: Vec<String> = store
            .projects()
            .await
            .unwrap()
            .into_iter()
            .map(|project| project.repo.to_string())
            .collect();
        assert_eq!(repos, ["acme/shop", "beta/shop"]);
    }

    // TC: IMP-01
    #[tokio::test]
    async fn names_project_after_tenant_when_repo_names_clash_across_tenants() {
        let store = imported().await;
        assert_eq!(project_of(&store, "beta/shop").await.name.as_str(), "beta");
    }

    // TC: IMP-01 (the tenant's slug goes to the repository of its oldest
    // target; the others are named after the repository)
    #[tokio::test]
    async fn names_other_repos_after_repo_when_tenant_has_several() {
        let store = imported_with(TWO_MORE_REPOS).await;
        assert_eq!(
            (
                project_of(&store, "acme/shop")
                    .await
                    .name
                    .as_str()
                    .to_owned(),
                project_of(&store, "acme/web")
                    .await
                    .name
                    .as_str()
                    .to_owned(),
            ),
            ("acme".to_owned(), "web".to_owned())
        );
    }

    // TC: IMP-01 (conflict → tenant suffix)
    #[tokio::test]
    async fn suffixes_tenant_slug_when_repo_name_is_taken() {
        let store = imported_with(TWO_MORE_REPOS).await;
        assert_eq!(
            project_of(&store, "acme/beta").await.name.as_str(),
            "beta-acme"
        );
    }
}

/// compose projects keep running across the switch-over, so an
/// imported environment keeps the compose project and slice of its v1
/// target (`lf_<tenant>_<target>`).
mod compose_names {
    use super::*;

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_v1_compose_project_and_slice_when_tenant_has_one_repo() {
        let store = imported().await;
        let mut names = Vec::new();
        for id in [ACME_PROD, ACME_QA, BETA_PROD] {
            names.push(compose_of(&store, id).await);
        }
        let expected: Vec<(String, String)> = [
            ("lf_acme_prod", "apps-lf-acme-prod.slice"),
            ("lf_acme_qa", "apps-lf-acme-qa.slice"),
            ("lf_beta_prod", "apps-lf-beta-prod.slice"),
        ]
        .into_iter()
        .map(|(compose, slice)| (compose.to_owned(), slice.to_owned()))
        .collect();
        assert_eq!(names, expected);
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_v1_compose_project_of_oldest_repo_when_tenant_has_several() {
        let store = imported_with(TWO_MORE_REPOS).await;
        assert_eq!(compose_of(&store, ACME_PROD).await.0, "lf_acme_prod");
    }

    // TC: IMP-01
    #[tokio::test]
    async fn reports_no_renamed_environment_when_every_tenant_has_one_repo() {
        let v1 = v1_db();
        let store = SqliteStore::open_in_memory().unwrap();
        let report = import_v1(&v1.path, &store, options()).await.unwrap();
        assert_eq!(report.renamed, []);
    }

    // TC: IMP-01 (what the operator has to bring up again under its new
    // name)
    #[tokio::test]
    async fn reports_renamed_environments_when_tenant_has_several_repos() {
        let v1 = v1_db_with(TWO_MORE_REPOS);
        let store = SqliteStore::open_in_memory().unwrap();
        let report = import_v1(&v1.path, &store, options()).await.unwrap();
        let renamed: Vec<(EnvironmentId, String, String)> = report
            .renamed
            .into_iter()
            .map(
                |Renamed {
                     environment,
                     v1,
                     v2,
                 }| (environment, v1, v2.to_string()),
            )
            .collect();
        assert_eq!(
            renamed,
            [
                (
                    env_id(ACME_STAGING),
                    "lf_acme_staging".to_owned(),
                    "lf_web_staging".to_owned()
                ),
                (
                    env_id(ACME_PREVIEW),
                    "lf_acme_preview".to_owned(),
                    "lf_beta-acme_preview".to_owned()
                ),
            ]
        );
    }

    // TC: IMP-01 (a repository pinned by two tenants: the target of the
    // tenant the project is named after keeps its name, the other one gets
    // the tenant's slug as suffix)
    #[tokio::test]
    async fn suffixes_tenant_slug_when_env_names_clash_in_shared_repo() {
        let store = imported_with(BETA_PINS_ACME_SHOP).await;
        let mut names = Vec::new();
        for id in [ACME_QA, BETA_QA] {
            names.push(compose_of(&store, id).await.0);
        }
        assert_eq!(names, ["lf_acme_qa", "lf_acme_qa-beta"]);
    }

    // TC: IMP-01
    #[tokio::test]
    async fn reports_renamed_environment_when_env_names_clash_in_shared_repo() {
        let v1 = v1_db_with(BETA_PINS_ACME_SHOP);
        let store = SqliteStore::open_in_memory().unwrap();
        let report = import_v1(&v1.path, &store, options()).await.unwrap();
        let renamed: Vec<(EnvironmentId, String, String)> = report
            .renamed
            .into_iter()
            .map(|renamed| (renamed.environment, renamed.v1, renamed.v2.to_string()))
            .collect();
        assert_eq!(
            renamed,
            [(
                env_id(BETA_QA),
                "lf_beta_qa".to_owned(),
                "lf_acme_qa-beta".to_owned()
            )]
        );
    }

    // TC: IMP-05 (a tenant holds the shared repository's project too)
    #[tokio::test]
    async fn lists_shared_project_under_every_tenant_when_repo_spans_tenants() {
        let store = imported_with(BETA_PINS_ACME_SHOP).await;
        let acme = project_of(&store, "acme/shop").await;
        let beta = project_of(&store, "beta/shop").await;
        let mut expected = vec![acme.id, beta.id];
        expected.sort_unstable();
        let projects: Vec<(String, Vec<_>)> = store
            .import_v1_tenants()
            .await
            .unwrap()
            .into_iter()
            .map(|tenant| (tenant.slug, tenant.projects))
            .collect();
        assert_eq!(
            projects,
            [
                ("acme".to_owned(), vec![acme.id]),
                ("beta".to_owned(), expected)
            ]
        );
    }
}

mod environments {
    use super::*;

    // TC: IMP-01
    #[tokio::test]
    async fn imports_targets_of_one_repo_as_environments_of_one_project() {
        let store = imported().await;
        let shop = project_of(&store, "acme/shop").await;
        let environments: Vec<(String, EnvironmentId)> = store
            .environments_of_project(shop.id)
            .await
            .unwrap()
            .into_iter()
            .map(|environment| (environment.name.as_str().to_owned(), environment.id))
            .collect();
        assert_eq!(
            environments,
            [
                ("prod".to_owned(), env_id(ACME_PROD)),
                ("qa".to_owned(), env_id(ACME_QA)),
            ]
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn makes_every_environment_compose_when_imported() {
        let store = imported().await;
        let kinds: Vec<EnvKind> = store
            .environments()
            .await
            .unwrap()
            .into_iter()
            .map(|environment| environment.kind)
            .collect();
        assert_eq!(
            kinds,
            [EnvKind::Compose, EnvKind::Compose, EnvKind::Compose]
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_pin_when_target_pinned() {
        let store = imported().await;
        let pin = environment(&store, ACME_QA).await.pin.unwrap();
        assert_eq!(
            (pin.branch, pin.pinned_by),
            (Branch::new("dev").unwrap(), user_id(ALICE))
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_limits_when_imported() {
        let store = imported().await;
        assert_eq!(
            environment(&store, ACME_PROD).await.limits,
            Limits {
                cpus: Cpus::from_hundredths(50).unwrap(),
                memory_mb: MemoryMb::new(512).unwrap(),
            }
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_state_and_current_deploy_when_running() {
        let store = imported().await;
        let prod = environment(&store, ACME_PROD).await;
        assert_eq!(
            (prod.state, prod.current_deploy_id),
            (
                EnvState::Running,
                Some(DeployId::parse(ACME_PROD_DEPLOY_21).unwrap())
            )
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_current_deploy_when_newer_deploy_failed() {
        let store = imported().await;
        assert_eq!(
            environment(&store, ACME_QA).await.current_deploy_id,
            Some(DeployId::parse(ACME_QA_DEPLOY_1).unwrap())
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_failed_state_when_nothing_succeeded() {
        let store = imported().await;
        let prod = environment(&store, BETA_PROD).await;
        assert_eq!(
            (prod.state, prod.current_deploy_id),
            (EnvState::Failed, None)
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn belongs_to_project_of_its_repo_when_repo_names_clash() {
        let store = imported().await;
        let beta = project_of(&store, "beta/shop").await;
        assert_eq!(environment(&store, BETA_PROD).await.project_id, beta.id);
    }

    // TC: IMP-01 (a target behind the v1 SSO gate stays behind login)
    #[tokio::test]
    async fn keeps_sso_target_private_when_imported() {
        let store = imported().await;
        assert_eq!(
            (
                environment(&store, ACME_QA).await.visibility,
                environment(&store, ACME_PROD).await.visibility,
            ),
            (Visibility::Private, Visibility::Shared)
        );
    }

    // TC: IMP-01, TEN-06 (a target behind the v1 SSO gate stays behind the gate of v2, `environments.protected`)
    #[tokio::test]
    async fn keeps_sso_target_protected_when_imported() {
        let store = imported().await;
        assert_eq!(
            (
                environment(&store, ACME_QA).await.protected,
                environment(&store, ACME_PROD).await.protected,
            ),
            (true, false)
        );
    }

    // TC: IMP-01 (an unpinned target joins the tenant's slug project, so
    // its compose name stays too)
    #[tokio::test]
    async fn puts_unpinned_target_in_project_of_tenant_when_tenant_pins_a_repo() {
        let store = imported_with(UNPINNED_TARGET).await;
        let scratch = environment(&store, ACME_SCRATCH).await;
        assert_eq!(
            (
                scratch.project_id,
                scratch.pin,
                compose_of(&store, ACME_SCRATCH).await.0
            ),
            (
                project_of(&store, "acme/shop").await.id,
                None,
                "lf_acme_scratch".to_owned()
            )
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn takes_repo_of_newest_ci_deploy_when_tenant_pins_nothing() {
        let store = imported_with(&gamma_deployed_from_ci()).await;
        let site = project_of(&store, "gamma/site").await;
        assert_eq!(
            (
                environment(&store, GAMMA_SITE_ID).await.project_id,
                site.name.as_str()
            ),
            (site.id, "gamma")
        );
    }

    // TC: IMP-06 (everything or nothing: a target with no repository to
    // belong to refuses the import and names it)
    #[tokio::test]
    async fn refuses_import_when_no_repo_is_known_for_a_target() {
        let (why, store) = refused_with(&gamma_deployed_from_cli()).await;
        assert_eq!(
            (why.contains(GAMMA_SITE_ID), store.users().await.unwrap()),
            (true, vec![]),
            "{why}"
        );
    }
}

mod deploys {
    use super::*;

    // TC: IMP-01 (the newest 20 deploys per environment)
    #[tokio::test]
    async fn keeps_newest_20_when_target_has_22() {
        let store = imported().await;
        let numbers: Vec<u32> = store
            .deploys_of_environment(env_id(ACME_PROD))
            .await
            .unwrap()
            .into_iter()
            .map(|deploy| deploy.number.get())
            .collect();
        assert_eq!(numbers, (3..=22).rev().collect::<Vec<u32>>());
    }

    // TC: IMP-01 (the current deploy stays, so a rollback has its compose)
    #[tokio::test]
    async fn keeps_current_deploy_when_older_than_newest_20() {
        let store = imported_with(OLD_CURRENT_DEPLOY).await;
        let numbers: Vec<u32> = store
            .deploys_of_environment(env_id(ACME_PROD))
            .await
            .unwrap()
            .into_iter()
            .map(|deploy| deploy.number.get())
            .collect();
        assert_eq!(numbers, (3..=22).rev().chain([1]).collect::<Vec<u32>>());
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_fields_when_deploy_failed() {
        let store = imported().await;
        let deploy = store
            .deploy_by_number(env_id(ACME_PROD), number(22))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                deploy.id,
                deploy.source,
                deploy.git_ref,
                deploy.tag,
                deploy.status,
                deploy.error.as_deref(),
                deploy.triggered_by,
                deploy.started_at,
                deploy.finished_at,
            ),
            (
                DeployId::parse("01HV00000000000000000G0022").unwrap(),
                DeploySource::Ci,
                Some(Branch::new("main").unwrap()),
                ImageTag::new("sha-22").unwrap(),
                DeployStatus::Failed,
                Some("container web is unhealthy"),
                Trigger::Token(token_id(ACME_TOKEN_ID)),
                Some(at("2026-09-10T10:22:00Z")),
                Some(at("2026-09-10T10:22:30Z")),
            )
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_compose_and_log_when_imported() {
        let store = imported().await;
        let deploy = store
            .deploy_by_number(env_id(ACME_PROD), number(21))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (deploy.compose.as_str(), deploy.log.as_str()),
            (
                "services:\n  web:\n    image: git.example.com/acme/shop:${IMAGE_TAG}\n",
                "deploy 21\n"
            )
        );
    }

    // TC: IMP-01
    #[tokio::test]
    async fn keeps_user_trigger_and_cli_source_when_deployed_from_cli() {
        let store = imported().await;
        let deploy = store
            .deploy_by_number(env_id(ACME_QA), number(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (deploy.source, deploy.git_ref, deploy.triggered_by),
            (DeploySource::Cli, None, Trigger::User(user_id(ALICE)))
        );
    }
}

mod report {
    use super::*;

    // TC: IMP-01
    #[tokio::test]
    async fn counts_what_was_written_when_fixture_imported() {
        let v1 = v1_db();
        let store = SqliteStore::open_in_memory().unwrap();
        let report = import_v1(&v1.path, &store, options()).await.unwrap();
        assert_eq!(
            report,
            liftoff_store::ImportReport {
                users: 2,
                projects: 2,
                environments: 3,
                domains: 3,
                bindings: 3,
                deploy_tokens: 3,
                deploys: 23,
                secrets: 3,
                tenants: 2,
                renamed: vec![],
            }
        );
    }
}

mod secrets {
    use super::*;

    // TC: IMP-02
    #[tokio::test]
    async fn stores_env_of_last_succeeded_deploy_when_newer_one_failed() {
        let store = imported().await;
        assert_eq!(
            env_secrets(&store, ACME_PROD).await,
            [
                ("DATABASE_URL".to_owned(), "postgres://db/acme".to_owned()),
                ("LOG_LEVEL".to_owned(), "info".to_owned()),
            ]
        );
    }

    // TC: IMP-02
    #[tokio::test]
    async fn ignores_env_of_failed_deploy_when_it_adds_a_key() {
        let store = imported().await;
        assert_eq!(
            env_secrets(&store, ACME_QA).await,
            [(
                "DATABASE_URL".to_owned(),
                "postgres://db/acme_qa".to_owned()
            )]
        );
    }

    // TC: IMP-02
    #[tokio::test]
    async fn stores_no_secrets_when_no_deploy_succeeded() {
        let store = imported().await;
        assert_eq!(env_secrets(&store, BETA_PROD).await, []);
    }

    // TC: IMP-02
    #[tokio::test]
    async fn writes_version_1_by_import_when_imported() {
        let store = imported().await;
        let secrets = store
            .secrets_of_scope(&SecretScope::Environment(env_id(ACME_PROD)))
            .await
            .unwrap();
        let marks: Vec<(VersionNumber, UpdatedBy, bool)> = secrets
            .iter()
            .map(|secret| (secret.version, secret.updated_by, secret.ci))
            .collect();
        assert_eq!(
            marks,
            [
                (VersionNumber::FIRST, UpdatedBy::Import, false),
                (VersionNumber::FIRST, UpdatedBy::Import, false),
            ]
        );
    }

    // TC: IMP-02
    #[tokio::test]
    async fn records_history_by_import_when_imported() {
        let store = imported().await;
        let secret = store
            .secret(
                &SecretScope::Environment(env_id(ACME_PROD)),
                &liftoff_core::domain::SecretKey::new("DATABASE_URL").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        let history: Vec<(VersionNumber, UpdatedBy)> = store
            .secret_versions(secret.id)
            .await
            .unwrap()
            .into_iter()
            .map(|version| (version.version, version.updated_by))
            .collect();
        assert_eq!(history, [(VersionNumber::FIRST, UpdatedBy::Import)]);
    }

    // TC: IMP-02 (v1 accepted `[A-Za-z_][A-Za-z0-9_]*`; the refusal names
    // every key that does not fit, in every environment, at once)
    #[tokio::test]
    async fn names_every_invalid_key_when_env_keys_do_not_fit() {
        let (why, _store) = refused_with(INVALID_ENV_KEYS).await;
        assert_eq!(
            (why.contains("node_env"), why.contains("_FOO")),
            (true, true),
            "{why}"
        );
    }

    // TC: IMP-06
    #[tokio::test]
    async fn imports_nothing_when_an_env_key_is_invalid() {
        let (_why, store) = refused_with(INVALID_ENV_KEYS).await;
        assert_eq!(
            (
                store.users().await.unwrap(),
                store.import_v1_tenants().await.unwrap()
            ),
            (vec![], vec![])
        );
    }

    // TC: IMP-02
    #[tokio::test]
    async fn stores_no_project_secrets_when_imported() {
        let store = imported().await;
        let shop = project_of(&store, "acme/shop").await;
        assert_eq!(
            store
                .secrets_of_scope(&SecretScope::Project(shop.id))
                .await
                .unwrap(),
            []
        );
    }
}

mod domains {
    use super::*;

    // TC: IMP-03
    #[tokio::test]
    async fn keeps_hosts_when_imported() {
        let store = imported().await;
        let hosts: Vec<String> = store
            .domains()
            .await
            .unwrap()
            .into_iter()
            .map(|domain| domain.host.as_str().to_owned())
            .collect();
        assert_eq!(
            hosts,
            ["shop-qa.apps.test", "shop.apps.test", "www.beta.example"]
        );
    }

    // TC: IMP-03
    #[tokio::test]
    async fn keeps_kind_verification_and_proxied_when_external() {
        let store = imported().await;
        let domain = store.domain(domain_id(BETA_DOMAIN)).await.unwrap().unwrap();
        assert_eq!(
            (domain.kind, domain.verified_at, domain.proxied),
            (
                DomainKind::External,
                Some(at("2026-09-07T12:00:00Z")),
                false
            )
        );
    }

    // TC: IMP-03
    #[tokio::test]
    async fn keeps_managed_kind_when_managed() {
        let store = imported().await;
        let domain = store
            .domain_by_host(&host("shop.apps.test"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (domain.id, domain.kind, domain.proxied),
            (domain_id(SHOP_DOMAIN), DomainKind::Managed, true)
        );
    }

    // TC: IMP-03
    #[tokio::test]
    async fn belongs_to_project_of_bound_target_when_imported() {
        let store = imported().await;
        let shop = project_of(&store, "acme/shop").await;
        let hosts: Vec<String> = store
            .domains_of_project(shop.id)
            .await
            .unwrap()
            .into_iter()
            .map(|domain| domain.host.as_str().to_owned())
            .collect();
        assert_eq!(hosts, ["shop-qa.apps.test", "shop.apps.test"]);
    }

    // TC: IMP-03
    #[tokio::test]
    async fn puts_unbound_domain_in_project_of_tenant_when_imported() {
        let store = imported_with(UNBOUND_DOMAIN).await;
        let shop = project_of(&store, "acme/shop").await;
        let domain = store
            .domain_by_host(&host("spare.apps.test"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(domain.project_id, shop.id);
    }

    // TC: IMP-06 (a domain with no project to belong to refuses the
    // import and names it)
    #[tokio::test]
    async fn refuses_import_when_tenant_of_unbound_domain_has_no_targets() {
        let (why, store) = refused_with(DOMAIN_WITHOUT_TARGETS).await;
        assert_eq!(
            (
                why.contains("gamma.apps.test"),
                store.domains().await.unwrap()
            ),
            (true, vec![]),
            "{why}"
        );
    }
}

mod bindings {
    use super::*;

    // TC: IMP-03
    #[tokio::test]
    async fn keeps_service_and_ports_when_bound() {
        let store = imported().await;
        let binding = store
            .binding_by_domain(domain_id(BETA_DOMAIN))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                binding.environment_id,
                binding.service.as_str(),
                binding.container_port.get(),
                binding.host_port.get(),
            ),
            (env_id(BETA_PROD), "app", 8080, 20002)
        );
    }

    // TC: IMP-03
    #[tokio::test]
    async fn keeps_every_host_port_when_imported() {
        let store = imported().await;
        let ports: Vec<(DomainId, u16)> = store
            .bindings()
            .await
            .unwrap()
            .into_iter()
            .map(|binding| (binding.domain_id, binding.host_port.get()))
            .collect();
        assert_eq!(
            ports,
            [
                (domain_id(SHOP_DOMAIN), 20000),
                (domain_id(SHOP_QA_DOMAIN), 20001),
                (domain_id(BETA_DOMAIN), 20002),
            ]
        );
    }

    // TC: IMP-03 (the v1 port cursor goes on: no port is handed out twice)
    #[tokio::test]
    async fn allocates_after_last_imported_port_when_next_binding_made() {
        let store = imported().await;
        assert_eq!(
            store.free_host_port(&HostPort::full_range()).await.unwrap(),
            Some(HostPort::new(20003).unwrap())
        );
    }

    // TC: IMP-03 (the cursor, not the lowest free port: a port freed in
    // v1 is not handed out again before the range wraps)
    #[tokio::test]
    async fn skips_port_freed_before_cursor_when_next_binding_made() {
        let store = imported_with(FREED_FIRST_PORT).await;
        assert_eq!(
            store.free_host_port(&HostPort::full_range()).await.unwrap(),
            Some(HostPort::new(20003).unwrap())
        );
    }
}

mod deploy_tokens {
    use super::*;

    // TC: IMP-04
    #[tokio::test]
    async fn finds_live_token_by_hash_of_v1_secret() {
        let store = imported().await;
        let hash = DeployTokenSecret::parse(ACME_TOKEN).unwrap().hash();
        let token = store
            .live_deploy_token_by_hash(&hash)
            .await
            .unwrap()
            .unwrap();
        let shop = project_of(&store, "acme/shop").await;
        assert_eq!(
            (token.id, token.project_id, token.created_by),
            (token_id(ACME_TOKEN_ID), shop.id, user_id(ALICE))
        );
    }

    // TC: IMP-04
    #[tokio::test]
    async fn moves_token_to_project_of_its_repo_when_imported() {
        let store = imported().await;
        let beta = project_of(&store, "beta/shop").await;
        assert_eq!(
            store
                .live_deploy_token(beta.id)
                .await
                .unwrap()
                .map(|token| token.id),
            Some(token_id(BETA_TOKEN_ID))
        );
    }

    // TC: IMP-04 (a token of a repository no target pins has no project)
    #[tokio::test]
    async fn skips_token_when_repo_has_no_targets() {
        let store = imported_with(STRAY_TOKEN).await;
        assert_eq!(
            (
                store
                    .deploy_token(token_id("01HV000000000000000000F004"))
                    .await
                    .unwrap(),
                store
                    .deploy_token(token_id(ACME_TOKEN_ID))
                    .await
                    .unwrap()
                    .map(|token| token.id),
            ),
            (None, Some(token_id(ACME_TOKEN_ID)))
        );
    }

    // TC: IMP-04
    #[tokio::test]
    async fn keeps_revoked_token_revoked_when_imported() {
        let store = imported().await;
        let hash = DeployTokenSecret::parse(ACME_REVOKED_TOKEN).unwrap().hash();
        let revoked = store
            .deploy_token(token_id(ACME_REVOKED_TOKEN_ID))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                revoked.revoked_at,
                store.live_deploy_token_by_hash(&hash).await.unwrap()
            ),
            (Some(at("2026-09-08T12:00:00Z")), None)
        );
    }
}

/// A CI deploy with the v1 token into the imported environment, on the
/// fakes over the imported SQLite store.
mod deploy_after_import {
    use super::*;

    /// An image from Docker Hub: no forge connection has to vouch for it.
    const COMPOSE: &str = "services:\n  web:\n    image: nginx:${IMAGE_TAG}\n";

    fn request(token_repo: &str) -> CiDeployRequest {
        CiDeployRequest {
            repo: repo(token_repo),
            git_ref: Branch::new("main").unwrap(),
            tag: ImageTag::new("1.27").unwrap(),
            compose: COMPOSE.to_owned(),
            env: None,
        }
    }

    async fn service(
        store: SqliteStore,
    ) -> (
        Arc<WithStore<SqliteStore>>,
        DeployService<WithStore<SqliteStore>>,
    ) {
        let settings = World::new().await.settings;
        let ports = Arc::new(WithStore::new(Arc::new(FakePorts::new()), store));
        (Arc::clone(&ports), DeployService::new(ports, settings))
    }

    // TC: IMP-04
    #[tokio::test]
    async fn accepts_v1_token_when_ci_deploys_after_import() {
        let (_ports, deploys) = service(imported().await).await;
        let token = DeployTokenSecret::parse(ACME_TOKEN).unwrap();
        let outcome = deploys
            .request_ci(&token, request("acme/shop"))
            .await
            .unwrap();
        let queued: Vec<(String, String, u32)> = outcome
            .deploys
            .iter()
            .map(|queued| {
                (
                    queued.project.as_str().to_owned(),
                    queued.environment.as_str().to_owned(),
                    queued.number.get(),
                )
            })
            .collect();
        assert_eq!(queued, [("acme".to_owned(), "prod".to_owned(), 23)]);
    }

    // TC: IMP-04
    #[tokio::test]
    async fn succeeds_with_imported_secrets_when_imported_environment_deployed() {
        let (ports, deploys) = service(imported().await).await;
        let token = DeployTokenSecret::parse(ACME_TOKEN).unwrap();
        let outcome = deploys
            .request_ci(&token, request("acme/shop"))
            .await
            .unwrap();
        let done = deploys.run(outcome.deploys[0].deploy_id).await.unwrap();
        let prod = environment(&ports.store, ACME_PROD).await;
        let shop = project_of(&ports.store, "acme/shop").await;
        let project = ComposeProject::new(&shop.name, &prod.name);
        let env = ports
            .fakes
            .files
            .file(&project, ProjectFile::Env)
            .unwrap_or_default();
        assert_eq!(
            (
                done.status,
                env.lines()
                    .any(|line| line == "DATABASE_URL=postgres://db/acme"),
                env.lines().any(|line| line == "LOG_LEVEL=info"),
            ),
            (DeployStatus::Succeeded, true, true),
            "deploy log: {}",
            done.log
        );
    }

    // TC: IMP-04
    #[tokio::test]
    async fn refuses_revoked_v1_token_when_ci_deploys_after_import() {
        let (_ports, deploys) = service(imported().await).await;
        let token = DeployTokenSecret::parse(ACME_REVOKED_TOKEN).unwrap();
        assert!(matches!(
            deploys.request_ci(&token, request("acme/shop")).await,
            Err(Error::Unauthenticated(_))
        ));
    }

    // TC: IMP-04
    #[tokio::test]
    async fn refuses_other_repo_when_v1_token_of_beta_used_for_acme() {
        let (_ports, deploys) = service(imported().await).await;
        let token = DeployTokenSecret::parse(BETA_TOKEN).unwrap();
        assert!(matches!(
            deploys.request_ci(&token, request("acme/shop")).await,
            Err(Error::Forbidden(_))
        ));
    }
}

mod users {
    use super::*;

    // TC: IMP-05 (members reference the imported users)
    #[tokio::test]
    async fn imports_users_as_oidc_with_subject_when_v1_has_sub() {
        let store = imported().await;
        let alice = store
            .user_by_login(&Login::new("alice").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                alice.id,
                alice.email.as_str(),
                alice.role,
                alice.source,
                alice.subject.as_deref(),
                alice.created_at,
            ),
            (
                user_id(ALICE),
                "alice@example.com",
                UserRole::Admin,
                UserSource::Oidc(OidcIssuer::new(ISSUER).unwrap()),
                Some("sub-alice"),
                at("2026-09-01T10:00:00Z"),
            )
        );
    }

    // TC: IMP-05
    #[tokio::test]
    async fn keeps_user_role_when_not_admin() {
        let store = imported().await;
        let bob = store.user(user_id(BOB)).await.unwrap().unwrap();
        assert_eq!(
            (bob.login.as_str(), bob.role, bob.subject.as_deref()),
            ("bob", UserRole::User, Some("sub-bob"))
        );
    }

    // TC: IMP-05 (v1 sessions are not carried over: users log in again)
    #[tokio::test]
    async fn imports_no_session_when_v1_had_one() {
        let store = imported().await;
        assert_eq!(store.sessions_of_user(user_id(ALICE)).await.unwrap(), []);
    }
}

mod tenants {
    use super::*;

    /// An imported tenant without its members, projects and domains.
    type TenantRow = (TenantId, String, String, UserId, u32, Limits, Timestamp);

    // TC: IMP-05
    #[tokio::test]
    async fn puts_tenants_aside_when_imported() {
        let store = imported().await;
        let tenants: Vec<TenantRow> = store
            .import_v1_tenants()
            .await
            .unwrap()
            .into_iter()
            .map(|tenant| {
                (
                    tenant.id,
                    tenant.slug,
                    tenant.name,
                    tenant.created_by,
                    tenant.max_targets,
                    tenant.limits,
                    tenant.created_at,
                )
            })
            .collect();
        assert_eq!(
            tenants,
            [
                (
                    TenantId::parse(ACME).unwrap(),
                    "acme".to_owned(),
                    "Acme Inc.".to_owned(),
                    user_id(ALICE),
                    10,
                    Limits {
                        cpus: Cpus::from_hundredths(200).unwrap(),
                        memory_mb: MemoryMb::new(2048).unwrap(),
                    },
                    at("2026-09-03T10:00:00Z"),
                ),
                (
                    TenantId::parse(BETA).unwrap(),
                    "beta".to_owned(),
                    "Beta Team".to_owned(),
                    user_id(BOB),
                    3,
                    Limits {
                        cpus: Cpus::from_hundredths(100).unwrap(),
                        memory_mb: MemoryMb::new(1024).unwrap(),
                    },
                    at("2026-09-04T10:00:00Z"),
                ),
            ]
        );
    }

    // TC: IMP-05
    #[tokio::test]
    async fn puts_memberships_aside_when_imported() {
        let store = imported().await;
        let members: Vec<(String, Vec<ImportedMember>)> = store
            .import_v1_tenants()
            .await
            .unwrap()
            .into_iter()
            .map(|tenant| (tenant.slug, tenant.members))
            .collect();
        assert_eq!(
            members,
            [
                (
                    "acme".to_owned(),
                    vec![
                        ImportedMember {
                            user_id: user_id(ALICE),
                            role: TenantRole::Owner,
                        },
                        ImportedMember {
                            user_id: user_id(BOB),
                            role: TenantRole::Member,
                        },
                    ]
                ),
                (
                    "beta".to_owned(),
                    vec![ImportedMember {
                        user_id: user_id(BOB),
                        role: TenantRole::Owner,
                    }]
                ),
            ]
        );
    }

    // TC: IMP-05
    #[tokio::test]
    async fn remembers_tenant_of_projects_and_domains_when_imported() {
        let store = imported().await;
        let acme = project_of(&store, "acme/shop").await;
        let beta = project_of(&store, "beta/shop").await;
        let owned: Vec<(String, Vec<_>, Vec<DomainId>)> = store
            .import_v1_tenants()
            .await
            .unwrap()
            .into_iter()
            .map(|tenant| {
                let domains = tenant.domains.into_iter().map(|domain| domain.id).collect();
                (tenant.slug, tenant.projects, domains)
            })
            .collect();
        assert_eq!(
            owned,
            [
                (
                    "acme".to_owned(),
                    vec![acme.id],
                    vec![domain_id(SHOP_DOMAIN), domain_id(SHOP_QA_DOMAIN)]
                ),
                (
                    "beta".to_owned(),
                    vec![beta.id],
                    vec![domain_id(BETA_DOMAIN)]
                ),
            ]
        );
    }

    // TC: IMP-05 (the v1 Cloudflare record, for the `dns.cloudflare`
    // module)
    #[tokio::test]
    async fn keeps_cf_record_of_each_domain_when_imported() {
        let store = imported().await;
        let domains: Vec<ImportedDomain> = store
            .import_v1_tenants()
            .await
            .unwrap()
            .into_iter()
            .flat_map(|tenant| tenant.domains)
            .collect();
        assert_eq!(
            domains,
            [
                ImportedDomain {
                    id: domain_id(SHOP_DOMAIN),
                    cf_record_id: Some("cf-record-1".to_owned()),
                },
                ImportedDomain {
                    id: domain_id(SHOP_QA_DOMAIN),
                    cf_record_id: Some("cf-record-2".to_owned()),
                },
                ImportedDomain {
                    id: domain_id(BETA_DOMAIN),
                    cf_record_id: None,
                },
            ]
        );
    }

    // TC: IMP-05 (without the module tenant_id stays empty)
    #[tokio::test]
    async fn leaves_tenant_id_empty_when_no_tenants_module() {
        let store = imported().await;
        let projects: Vec<_> = store
            .projects()
            .await
            .unwrap()
            .into_iter()
            .map(|project| project.tenant_id)
            .collect();
        let domains: Vec<_> = store
            .domains()
            .await
            .unwrap()
            .into_iter()
            .map(|domain| domain.tenant_id)
            .collect();
        assert_eq!(
            (projects, domains),
            (vec![None, None], vec![None, None, None])
        );
    }

    // TC: IMP-05
    #[tokio::test]
    async fn has_no_tenants_aside_when_not_imported() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.import_v1_tenants().await.unwrap(), []);
    }
}

mod idempotence {
    use super::*;

    // TC: IMP-06
    #[tokio::test]
    async fn conflict_when_imported_twice() {
        let v1 = v1_db();
        let store = SqliteStore::open_in_memory().unwrap();
        import_v1(&v1.path, &store, options()).await.unwrap();
        assert!(matches!(
            import_v1(&v1.path, &store, options()).await,
            Err(Error::Conflict(_))
        ));
    }

    // TC: IMP-06
    #[tokio::test]
    async fn changes_nothing_when_second_import_refused() {
        let v1 = v1_db();
        let store = SqliteStore::open_in_memory().unwrap();
        import_v1(&v1.path, &store, options()).await.unwrap();
        let _refused = import_v1(&v1.path, &store, options()).await;
        assert_eq!(
            (
                store.users().await.unwrap().len(),
                store.projects().await.unwrap().len(),
                store.environments().await.unwrap().len(),
                store.import_v1_tenants().await.unwrap().len(),
            ),
            (2, 2, 3, 2)
        );
    }

    // TC: IMP-06
    #[tokio::test]
    async fn conflict_when_database_already_has_a_user() {
        let v1 = v1_db();
        let store = SqliteStore::open_in_memory().unwrap();
        let owner = Records::default().user("owner", UserRole::Admin).unwrap();
        store.insert_user(&owner).await.unwrap();
        let refused = import_v1(&v1.path, &store, options()).await;
        assert!(
            matches!(refused, Err(Error::Conflict(_))),
            "got {refused:?}"
        );
    }

    // TC: IMP-06
    #[tokio::test]
    async fn imports_nothing_when_database_already_has_a_user() {
        let v1 = v1_db();
        let store = SqliteStore::open_in_memory().unwrap();
        let owner = Records::default().user("owner", UserRole::Admin).unwrap();
        store.insert_user(&owner).await.unwrap();
        let _refused = import_v1(&v1.path, &store, options()).await;
        assert_eq!(
            (
                store.users().await.unwrap(),
                store.projects().await.unwrap().len()
            ),
            (vec![owner], 0)
        );
    }

    // TC: IMP-06 (a v2 database is not a v1 one)
    #[tokio::test]
    async fn internal_when_source_is_not_a_v1_database() {
        let dir = tempfile::tempdir().unwrap();
        let v2 = dir.path().join("liftoff-v2.db");
        drop(SqliteStore::open(&v2).unwrap());
        let store = SqliteStore::open_in_memory().unwrap();
        let refused = import_v1(&v2, &store, options()).await;
        assert!(
            matches!(refused, Err(Error::Internal(_))),
            "got {refused:?}"
        );
    }

    // TC: IMP-06 (everything or nothing: an env that does not open with
    // the key refuses the whole import)
    #[tokio::test]
    async fn imports_nothing_when_an_env_does_not_open() {
        let v1 = v1_db();
        let store = SqliteStore::open_in_memory().unwrap();
        let wrong_key = V1Import {
            issuer: OidcIssuer::new(ISSUER).unwrap(),
            git_host: host(GIT_HOST),
            secrets: liftoff_core::crypto::MasterKey::from_key_bytes(&[7; 32]).unwrap(),
            rng: SeqRng::new(),
            clock: FixedClock::default(),
        };
        let refused = import_v1(&v1.path, &store, wrong_key).await;
        assert_eq!(
            (
                matches!(refused, Err(Error::Internal(_))),
                store.users().await.unwrap(),
                store.import_v1_tenants().await.unwrap()
            ),
            (true, vec![], vec![])
        );
    }

    // TC: IMP-06 (everything or nothing: a write that fails after users,
    // projects and environments are written rolls them back too)
    #[tokio::test]
    async fn imports_nothing_when_a_write_fails_half_way() {
        let v1 = v1_db();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("liftoff.db");
        let store = SqliteStore::open(&path).unwrap();
        // A port cursor the import's own cursor row collides with: it is
        // written after users, projects, environments, domains and bindings.
        let _row = rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "INSERT INTO port_cursor (id, host_port) VALUES (1, 20500)",
                [],
            )
            .unwrap();
        let refused = import_v1(&v1.path, &store, options()).await;
        assert_eq!(
            (
                matches!(refused, Err(Error::Internal(_))),
                store.users().await.unwrap(),
                store.projects().await.unwrap().len(),
                store.import_v1_tenants().await.unwrap()
            ),
            (true, vec![], 0, vec![])
        );
    }

    // TC: IMP-06 (the source is only read: a refused or finished import
    // leaves the v1 file as it was)
    #[tokio::test]
    async fn leaves_v1_database_unchanged_when_imported() {
        let v1 = v1_db();
        let before = std::fs::read(&v1.path).unwrap();
        let store = SqliteStore::open_in_memory().unwrap();
        import_v1(&v1.path, &store, options()).await.unwrap();
        assert!(
            std::fs::read(&v1.path).unwrap() == before,
            "the v1 file changed"
        );
    }
}

/// A v1 file is not a v2 database: both carry `user_version = 3`, so the
/// store tells them apart by the v1 tables and refuses to migrate one.
mod open_v1_as_v2 {
    use super::*;

    // TC: IMP-06
    #[test]
    fn internal_when_v1_database_opened_as_v2() {
        let v1 = v1_db();
        let refused = SqliteStore::open(&v1.path);
        let why = match refused {
            Err(Error::Internal(source)) => source.to_string(),
            other => panic!("expected an internal error, got {other:?}"),
        };
        assert!(why.contains("v1") && why.contains("--import"), "{why}");
    }

    // TC: IMP-06 (not even the journal mode changes)
    #[test]
    fn leaves_v1_database_unchanged_when_opened_as_v2() {
        let v1 = v1_db();
        let before = std::fs::read(&v1.path).unwrap();
        let _refused = SqliteStore::open(&v1.path);
        assert!(
            std::fs::read(&v1.path).unwrap() == before,
            "the v1 file changed"
        );
    }
}
