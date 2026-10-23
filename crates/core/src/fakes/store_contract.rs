//! The [`Store`] contract as one check per rule, generic over the
//! implementation, so `InMemoryStore` and the SQLite store are held to the
//! same rules.
//!
//! [`store_contract_tests!`](crate::store_contract_tests) turns every check
//! into its own `#[tokio::test]`, so a store that breaks several rules
//! reports each of them:
//!
//! ```ignore
//! // in liftoff-store's tests:
//! mod contract {
//!     liftoff_core::store_contract_tests!(|| SqliteStore::open_in_memory().unwrap());
//! }
//! ```
//!
//! Each check gets a fresh store, returns `Err` only when the store itself
//! fails, and panics with the rule (and its test-case id) when the store
//! answers wrongly.

use core::ops::RangeInclusive;

use time::Duration;

use super::records::Records;
use crate::domain::{
    Artifact, ArtifactFile, ArtifactId, ArtifactPath, ArtifactVersion, BlobHash, Branch,
    CachedTemplate, ConfigValue, DeployNumber, DeploySource, DeployStatus, DomainKind, EnvName,
    EnvState, Environment, GateCode, GateSession, GitConnection, Host, HostPort, ImageTag,
    InstalledModule, LinkTarget, Login, ModuleConfigRecord, ModuleName, ModuleVersion, OidcIssuer,
    OneTimeLink, Project, ProjectName, RepoRef, ReturnPath, SealedEnv, SecretId, SecretKey,
    SecretScope, SecretSyncRecord, Session, SyncTarget, TemplateRef, TenantId, Timestamp,
    TokenHash, Trigger, UpdatedBy, User, UserRole, UserSource, VersionNumber, Visibility,
};
use crate::error::Error;
use crate::ports::{ModuleStore, Store};

/// Generates one `#[tokio::test]` per [`Store`] contract rule.
///
/// Each test runs over a fresh store from `$new_store` (a closure or fn
/// path returning the store). Invoke it inside a test module; the calling
/// crate needs `tokio` with the `macros` and `rt` features as a
/// dev-dependency.
#[macro_export]
macro_rules! store_contract_tests {
    ($new_store:expr) => {
        $crate::store_contract_tests!(
            @rules $new_store;
            conflicts_when_user_login_taken,
            finds_user_when_subject_matches,
            conflicts_when_user_subject_taken,
            conflicts_when_project_repo_taken,
            finds_project_when_repo_matches,
            conflicts_when_project_name_taken_without_tenant,
            allows_same_project_name_when_tenants_differ,
            conflicts_when_environment_name_taken_in_project,
            conflicts_when_domain_host_taken,
            conflicts_when_binding_domain_or_port_taken,
            conflicts_when_update_takes_a_used_key,
            starts_at_range_start_when_no_binding_yet,
            skips_used_port_when_allocating,
            avoids_freed_port_when_binding_deleted,
            wraps_around_when_range_end_reached,
            finds_environments_when_branch_matches,
            conflicts_when_project_has_a_live_token,
            replaces_token_in_one_step_when_rotated,
            numbers_and_prunes_when_deploys_accumulate,
            lists_oldest_first_when_deploys_filtered_by_status,
            updates_status_when_deploy_updated,
            sorts_by_natural_key_when_listing,
            lists_every_environment_by_id_when_listing_all,
            restricts_domain_delete_when_bound,
            restricts_project_delete_until_empty,
            cascades_when_project_deleted,
            keeps_record_lost_at_when_domain_updated,
            sets_protected_flag_when_environment_protected,
            keeps_protected_flag_when_environment_updated,
            cascades_when_environment_deleted,
            finds_and_deletes_sessions_when_user_has_several,
            takes_gate_code_once_when_taken_twice,
            finds_gate_session_when_hash_matches,
            cascades_gate_records_when_session_deleted,
            prunes_expired_gate_records_when_pruned,
            exchanges_setup_token_once_when_hash_matches,
            keeps_setup_token_when_hash_differs,
            keeps_setup_token_when_session_rejected,
            keeps_setup_token_when_session_of_another_user,
            replaces_setup_token_when_put_again,
            not_found_when_setup_token_user_missing,
            not_found_when_row_or_parent_missing,
            replaces_git_connection_when_same_user_and_host,
            keeps_every_version_when_secret_put_again,
            conflicts_when_secret_key_taken_in_scope,
            keeps_tenant_secrets_apart_when_scopes_share_a_key,
            prunes_oldest_versions_when_more_than_kept,
            deletes_versions_when_secret_deleted,
            round_trips_every_field_when_stored,
            appends_in_order_when_audit_recorded,
            misses_key_of_other_module_when_only_one_set_it,
            keeps_module_keys_apart_when_two_modules_set_one_key,
            replaces_module_value_when_set_again,
            forgets_module_keys_when_purged,
            keeps_other_module_keys_when_one_is_purged,
            round_trips_module_with_sealed_config_when_stored,
            conflicts_when_module_name_taken,
            lists_modules_in_install_order_when_listed,
            replaces_module_when_updated,
            forgets_module_when_deleted,
            not_found_when_module_missing_on_update_or_delete,
            replaces_cached_template_when_put_again,
            keeps_cached_templates_apart_when_user_or_host_differs,
            round_trips_cached_template_without_manifest,
            not_found_when_cached_template_user_missing,
            replaces_secret_sync_when_put_again,
            round_trips_failed_secret_sync_when_stored,
            keeps_secret_syncs_apart_when_secrets_differ,
            forgets_secret_sync_when_secret_deleted,
            forgets_secret_sync_when_environment_deleted,
            forgets_secret_sync_when_project_deleted,
            not_found_when_secret_sync_secret_missing,
            round_trips_one_time_link_by_hash_when_stored,
            conflicts_when_one_time_link_hash_taken,
            not_found_when_one_time_link_user_missing,
            uses_one_time_link_once_when_used_twice,
            misses_one_time_link_when_hash_unknown,
            prunes_expired_one_time_links_when_pruned,
            forgets_one_time_links_of_user_when_sessions_of_user_deleted,
            round_trips_tunnel_environment_when_stored,
            round_trips_function_environment_when_stored,
            round_trips_component_hash_when_deploy_stored,
            keeps_kv_apart_when_environments_share_a_key,
            replaces_kv_value_when_set_again,
            forgets_kv_key_when_deleted,
            counts_kv_bytes_of_keys_and_values_when_asked,
            forgets_kv_when_environment_deleted,
            not_found_when_kv_environment_missing,
            round_trips_artifact_with_first_version_when_stored,
            replaces_artifact_row_when_updated,
            conflicts_when_artifact_id_taken,
            not_found_when_artifact_owner_or_artifact_missing,
            makes_added_version_current_when_version_added,
            conflicts_when_artifact_version_number_taken,
            lists_artifacts_newest_first_when_listed,
            deletes_only_versions_that_are_not_current_when_deleted,
            prunes_oldest_artifact_versions_but_never_current_when_pruned,
            cascades_versions_when_artifact_deleted,
            finds_only_expired_private_artifacts_when_asked,
        );
    };
    (@rules $new_store:expr; $($rule:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $rule() -> ::core::result::Result<(), $crate::Error> {
                $crate::fakes::store_contract::$rule(&($new_store)()).await
            }
        )+
    };
}

/// `environments.protected` is set by [`Store::set_environment_protected`].
///
/// Reads of one environment and of all of them agree (TEN-06; the core keeps the flag the `tenants` module sets).
///
/// # Errors
///
/// Whatever the store reports.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn sets_protected_flag_when_environment_protected<S: Store>(
    store: &S,
) -> Result<(), Error> {
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    store.set_environment_protected(prod.id, true).await?;
    let one = store
        .environment(prod.id)
        .await?
        .map(|stored| stored.protected);
    let all: Vec<bool> = store
        .environments()
        .await?
        .iter()
        .filter(|stored| stored.id == prod.id)
        .map(|stored| stored.protected)
        .collect();
    let missing = seed.records.environment(&seed.shop, "gone")?;
    assert_eq!(
        (
            one,
            all,
            is_not_found(&store.set_environment_protected(missing.id, true).await)
        ),
        (Some(true), vec![true], true),
        "TEN-06: set_environment_protected sets the flag of an existing environment"
    );
    Ok(())
}

/// [`Store::update_environment`] keeps `environments.protected`: a writer
/// with an older copy of the environment (a deploy finishing, `lof env
/// set`) does not take it from behind the gate (TEN-06).
///
/// # Errors
///
/// Whatever the store reports.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_protected_flag_when_environment_updated<S: Store>(
    store: &S,
) -> Result<(), Error> {
    let seed = seed(store).await?;
    let mut stale = env_of(store, &seed, "prod").await?;
    store.set_environment_protected(stale.id, true).await?;
    stale.protected = false;
    stale.state = EnvState::Failed;
    store.update_environment(&stale).await?;
    let stored = store.environment(stale.id).await?;
    assert_eq!(
        stored.map(|stored| (stored.protected, stored.state)),
        Some((true, EnvState::Failed)),
        "TEN-06: update_environment changes the rest and keeps protected"
    );
    Ok(())
}

/// Pruning the gate records deletes the codes and host sessions whose
/// `expires_at` is not after `now`, and only those (v1 SSO-13).
///
/// # Errors
///
/// Whatever the store reports.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn prunes_expired_gate_records_when_pruned<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let (session, _) = seed.records.session(&seed.bob);
    store.insert_session(&session).await?;
    let now = session.created_at;
    let mut stale_code = gate_code(&session, "code-old", "blog.apps.test")?;
    stale_code.expires_at = now;
    let live_code = gate_code(&session, "code-new", "blog.apps.test")?;
    let mut stale_gate = gate_session(&session, "gate-old", "blog.apps.test")?;
    stale_gate.expires_at = now.saturating_sub(Duration::seconds(1));
    let mut live_gate = gate_session(&session, "gate-new", "blog.apps.test")?;
    live_gate.expires_at = now.saturating_add(Duration::milliseconds(1));
    for code in [&stale_code, &live_code] {
        store.insert_gate_code(code).await?;
    }
    for gate in [&stale_gate, &live_gate] {
        store.insert_gate_session(gate).await?;
    }
    let pruned = store.prune_gate_records(now).await?;
    let left = (
        store.gate_session(&stale_gate.token_hash).await?.is_some(),
        store.gate_session(&live_gate.token_hash).await?.is_some(),
        store.take_gate_code(&stale_code.code_hash).await?.is_some(),
        store.take_gate_code(&live_code.code_hash).await?.is_some(),
    );
    assert_eq!(
        (pruned, left),
        ((1, 1), (false, true, false, true)),
        "SSO-13: expired gate records go, live ones stay"
    );
    Ok(())
}

/// A gate code of `session` for `host`, its hash that of `secret`.
fn gate_code(session: &Session, secret: &str, host: &str) -> Result<GateCode, Error> {
    Ok(GateCode {
        code_hash: TokenHash::of(secret),
        session_id: session.id,
        user_id: session.user_id,
        host: Host::new(host)?,
        rd: ReturnPath::new("/admin")?,
        expires_at: session.created_at.saturating_add(Duration::minutes(10)),
    })
}

/// A gate session handed over from `session` on `host`, its hash that of
/// `secret`.
fn gate_session(session: &Session, secret: &str, host: &str) -> Result<GateSession, Error> {
    Ok(GateSession {
        token_hash: TokenHash::of(secret),
        session_id: session.id,
        user_id: session.user_id,
        host: Host::new(host)?,
        expires_at: session.expires_at,
        created_at: session.created_at,
    })
}

/// A one-time code of the gate is taken once; its hash is unique and its
/// browser session must exist (TEN-07, v1 SSO-08 and SSO-14).
///
/// # Errors
///
/// Whatever the store reports.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn takes_gate_code_once_when_taken_twice<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let (session, _) = seed.records.session(&seed.owner);
    store.insert_session(&session).await?;
    let code = gate_code(&session, "code-1", "blog.apps.test")?;
    store.insert_gate_code(&code).await?;
    assert!(
        is_conflict(&store.insert_gate_code(&code).await),
        "gate_codes.code_hash is unique"
    );
    let (missing, _) = seed.records.session(&seed.owner);
    assert!(
        is_not_found(
            &store
                .insert_gate_code(&gate_code(&missing, "code-2", "blog.apps.test")?)
                .await
        ),
        "a gate code needs its session"
    );
    let first = store.take_gate_code(&code.code_hash).await?;
    let second = store.take_gate_code(&code.code_hash).await?;
    assert_eq!(
        (first, second),
        (Some(code), None),
        "a gate code is taken once"
    );
    Ok(())
}

/// A session on a protected host is found by its hash, which is unique;
/// its browser session must exist (TEN-07, v1 SSO-14).
///
/// # Errors
///
/// Whatever the store reports.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn finds_gate_session_when_hash_matches<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let (session, _) = seed.records.session(&seed.bob);
    store.insert_session(&session).await?;
    let gate = gate_session(&session, "gate-1", "blog.apps.test")?;
    store.insert_gate_session(&gate).await?;
    assert!(
        is_conflict(&store.insert_gate_session(&gate).await),
        "gate_sessions.token_hash is unique"
    );
    let (missing, _) = seed.records.session(&seed.bob);
    assert!(
        is_not_found(
            &store
                .insert_gate_session(&gate_session(&missing, "gate-2", "blog.apps.test")?)
                .await
        ),
        "a gate session needs its browser session"
    );
    assert_eq!(
        (
            store.gate_session(&gate.token_hash).await?,
            store.gate_session(&TokenHash::of("gate-3")).await?
        ),
        (Some(gate), None),
        "found by hash"
    );
    Ok(())
}

/// Deleting a session deletes the gate codes and sessions handed over
/// from it, and only those (TEN-08, v1 SSO-11 and SSO-12).
///
/// # Errors
///
/// Whatever the store reports.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn cascades_gate_records_when_session_deleted<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let (first, _) = seed.records.session(&seed.bob);
    let (second, _) = seed.records.session(&seed.bob);
    for session in [&first, &second] {
        store.insert_session(session).await?;
    }
    for (session, n) in [(&first, 1), (&second, 2)] {
        store
            .insert_gate_code(&gate_code(session, &format!("code-{n}"), "blog.apps.test")?)
            .await?;
        store
            .insert_gate_session(&gate_session(
                session,
                &format!("gate-{n}"),
                "blog.apps.test",
            )?)
            .await?;
    }
    store.delete_session(first.id).await?;
    let after_one = (
        store
            .gate_session(&TokenHash::of("gate-1"))
            .await?
            .is_some(),
        store
            .gate_session(&TokenHash::of("gate-2"))
            .await?
            .is_some(),
    );
    store.delete_sessions_of_user(seed.bob.id).await?;
    let after_all = store
        .gate_session(&TokenHash::of("gate-2"))
        .await?
        .is_some();
    let codes = (
        store
            .take_gate_code(&TokenHash::of("code-1"))
            .await?
            .is_some(),
        store
            .take_gate_code(&TokenHash::of("code-2"))
            .await?
            .is_some(),
    );
    assert_eq!(
        (after_one, after_all, codes),
        ((false, true), false, (false, false)),
        "deleting a session deletes what was handed over from it"
    );
    Ok(())
}

/// The rows every rule starts from: users `owner` (admin) and `bob`, and
/// project `shop` (`git.example.com`, `owner/shop`) created by `owner`.
struct Seed {
    records: Records,
    owner: User,
    bob: User,
    shop: Project,
}

async fn seed<S: Store>(store: &S) -> Result<Seed, Error> {
    let records = Records::default();
    let owner = records.user("owner", UserRole::Admin)?;
    let bob = records.user("bob", UserRole::User)?;
    store.insert_user(&owner).await?;
    store.insert_user(&bob).await?;
    let shop = records.project("shop", "git.example.com", "owner/shop", &owner)?;
    store.insert_project(&shop).await?;
    Ok(Seed {
        records,
        owner,
        bob,
        shop,
    })
}

const fn is_conflict<T>(result: &Result<T, Error>) -> bool {
    matches!(result, Err(Error::Conflict(_)))
}

const fn is_not_found<T>(result: &Result<T, Error>) -> bool {
    matches!(result, Err(Error::NotFound(_)))
}

fn sealed(text: &str) -> SealedEnv {
    SealedEnv::new(format!("sealed:{text}"))
}

async fn env_of<S: Store>(store: &S, seed: &Seed, name: &str) -> Result<Environment, Error> {
    let environment = seed.records.environment(&seed.shop, name)?;
    store.insert_environment(&environment).await?;
    Ok(environment)
}

/// Inserts a domain `host` of `shop` bound to `environment` at `host_port`.
async fn bind_at<S: Store>(
    store: &S,
    seed: &Seed,
    environment: &Environment,
    host: &str,
    host_port: u16,
) -> Result<(), Error> {
    let domain = seed.records.domain(&seed.shop, host)?;
    store.insert_domain(&domain).await?;
    store
        .insert_binding(
            &seed
                .records
                .binding(environment, &domain, "web", 3000, host_port)?,
        )
        .await
}

fn ports(low: u16, high: u16) -> Result<RangeInclusive<HostPort>, Error> {
    Ok(HostPort::range(low, high)?)
}

/// `users.login` is unique; logins compare in lower case.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_user_login_taken<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let shouting = seed.records.user("BOB", UserRole::User)?;
    assert!(
        is_conflict(&store.insert_user(&shouting).await),
        "users.login is unique, case-insensitively"
    );
    let found = store.user_by_login(&Login::new("Bob")?).await?;
    assert_eq!(found.as_ref(), Some(&seed.bob), "user_by_login finds bob");
    Ok(())
}

/// A user of an OIDC provider is found by its `(source, subject)`; the
/// same subject of another provider, or a local user, is not it.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn finds_user_when_subject_matches<S: Store>(store: &S) -> Result<(), Error> {
    // TC: OIDC-02
    let seed = seed(store).await?;
    let issuer = UserSource::Oidc(OidcIssuer::new("https://id.example.com")?);
    let other = UserSource::Oidc(OidcIssuer::new("https://login.example.com")?);
    let mut carol = seed.records.user("carol", UserRole::User)?;
    carol.source = issuer.clone();
    carol.subject = Some("z1".to_owned());
    store.insert_user(&carol).await?;
    assert_eq!(
        (
            store.user_by_subject(&issuer, "z1").await?,
            store.user_by_subject(&other, "z1").await?,
            store.user_by_subject(&issuer, "z2").await?,
        ),
        (Some(carol), None, None),
        "user_by_subject matches source and subject"
    );
    Ok(())
}

/// `(users.source, users.subject)` is unique for users with a subject
/// (one row per account of a provider), on insert and on update.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_user_subject_taken<S: Store>(store: &S) -> Result<(), Error> {
    // TC: OIDC-02
    let seed = seed(store).await?;
    let issuer = UserSource::Oidc(OidcIssuer::new("https://id.example.com")?);
    let mut carol = seed.records.user("carol", UserRole::User)?;
    carol.source = issuer.clone();
    carol.subject = Some("z1".to_owned());
    store.insert_user(&carol).await?;
    let mut dave = seed.records.user("dave", UserRole::User)?;
    dave.source = issuer;
    dave.subject = Some("z1".to_owned());
    assert!(
        is_conflict(&store.insert_user(&dave).await),
        "a second user of the same subject is a conflict"
    );
    dave.subject = Some("z2".to_owned());
    store.insert_user(&dave).await?;
    dave.subject = Some("z1".to_owned());
    assert!(
        is_conflict(&store.update_user(&dave).await),
        "an update to a taken subject is a conflict"
    );
    Ok(())
}

/// `(projects.git_host, projects.repo)` is unique, whatever the case of the
/// repository.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_project_repo_taken<S: Store>(store: &S) -> Result<(), Error> {
    // TC: ENV-02
    let seed = seed(store).await?;
    let second = seed
        .records
        .project("shop-2", "git.example.com", "Owner/Shop", &seed.owner)?;
    assert!(
        is_conflict(&store.insert_project(&second).await),
        "a second project on owner/shop is a conflict (ENV-02)"
    );
    let elsewhere = seed
        .records
        .project("shop-gh", "github.com", "owner/shop", &seed.owner)?;
    assert!(
        store.insert_project(&elsewhere).await.is_ok(),
        "the same owner/shop on another forge host is another project"
    );
    Ok(())
}

/// `project_by_repo` matches the host exactly and the repository in lower
/// case; an unknown repository is `None`.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn finds_project_when_repo_matches<S: Store>(store: &S) -> Result<(), Error> {
    // TC: ENV-03
    let seed = seed(store).await?;
    let host = Host::new("git.example.com")?;
    assert_eq!(
        store
            .project_by_repo(&host, &RepoRef::new("Owner/Shop")?)
            .await?,
        Some(seed.shop.clone()),
        "finds shop by owner/shop in any case (ENV-03)"
    );
    assert_eq!(
        store
            .project_by_repo(&host, &RepoRef::new("owner/blog")?)
            .await?,
        None,
        "an unknown repository is None (ENV-03)"
    );
    assert_eq!(
        store
            .project_by_repo(&Host::new("github.com")?, &RepoRef::new("owner/shop")?)
            .await?,
        None,
        "another host is another repository"
    );
    Ok(())
}

/// Two projects without a tenant cannot share a name (SQL's `NULL`s must
/// not make the unique index miss it).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_project_name_taken_without_tenant<S: Store>(
    store: &S,
) -> Result<(), Error> {
    let seed = seed(store).await?;
    let same_name = seed
        .records
        .project("shop", "git.example.com", "bob/shop", &seed.bob)?;
    assert!(
        is_conflict(&store.insert_project(&same_name).await),
        "(tenant_id, name) is unique when tenant_id is None"
    );
    let found = store
        .project_by_name(None, &ProjectName::new("shop")?)
        .await?;
    assert_eq!(found, Some(seed.shop), "project_by_name finds shop");
    Ok(())
}

/// The same project name in two different tenants is fine.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn allows_same_project_name_when_tenants_differ<S: Store>(
    store: &S,
) -> Result<(), Error> {
    let seed = seed(store).await?;
    let (acme, beta) = (
        TenantId::new(ulid::Ulid::from_parts(1, 1)),
        TenantId::new(ulid::Ulid::from_parts(1, 2)),
    );
    let mut first = seed
        .records
        .project("blog", "git.example.com", "acme/blog", &seed.owner)?;
    first.tenant_id = Some(acme);
    let mut second = seed
        .records
        .project("blog", "git.example.com", "beta/blog", &seed.owner)?;
    second.tenant_id = Some(beta);
    store.insert_project(&first).await?;
    assert!(
        store.insert_project(&second).await.is_ok(),
        "same name in another tenant"
    );
    let name = ProjectName::new("blog")?;
    assert_eq!(
        store.project_by_name(Some(beta), &name).await?,
        Some(second),
        "project_by_name looks in the given tenant"
    );
    assert_eq!(
        store.project_by_name(None, &name).await?,
        None,
        "no tenantless blog"
    );
    Ok(())
}

/// `(environments.project_id, environments.name)` is unique; the same name
/// in another project is fine.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_environment_name_taken_in_project<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: ENV-05
    let seed = seed(store).await?;
    let qa = env_of(store, &seed, "qa").await?;
    let dup = seed.records.environment(&seed.shop, "qa")?;
    assert!(
        is_conflict(&store.insert_environment(&dup).await),
        "(project, name) is unique (ENV-05)"
    );
    let blog = seed
        .records
        .project("blog", "git.example.com", "owner/blog", &seed.owner)?;
    store.insert_project(&blog).await?;
    let other = seed.records.environment(&blog, "qa")?;
    assert!(
        store.insert_environment(&other).await.is_ok(),
        "same name in another project (ENV-05)"
    );
    assert_eq!(
        store
            .environment_by_name(seed.shop.id, &EnvName::new("qa")?)
            .await?,
        Some(qa),
        "environment_by_name looks in the given project"
    );
    Ok(())
}

/// `domains.host` is unique across projects.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_domain_host_taken<S: Store>(store: &S) -> Result<(), Error> {
    // TC: DOM-02
    let seed = seed(store).await?;
    store
        .insert_domain(&seed.records.domain(&seed.shop, "shop.apps.test")?)
        .await?;
    let blog = seed
        .records
        .project("blog", "git.example.com", "owner/blog", &seed.owner)?;
    store.insert_project(&blog).await?;
    let dup = seed.records.domain(&blog, "shop.apps.test")?;
    assert!(
        is_conflict(&store.insert_domain(&dup).await),
        "domains.host is unique across projects (DOM-02)"
    );
    Ok(())
}

/// `bindings.domain_id` and `bindings.host_port` are unique.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_binding_domain_or_port_taken<S: Store>(store: &S) -> Result<(), Error> {
    // TC: BND-01
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let a = seed.records.domain(&seed.shop, "a.apps.test")?;
    let b = seed.records.domain(&seed.shop, "b.apps.test")?;
    store.insert_domain(&a).await?;
    store.insert_domain(&b).await?;
    store
        .insert_binding(&seed.records.binding(&prod, &a, "web", 3000, 20_000)?)
        .await?;
    let same_domain = seed.records.binding(&prod, &a, "api", 8080, 20_001)?;
    assert!(
        is_conflict(&store.insert_binding(&same_domain).await),
        "bindings.domain_id is unique (BND-01)"
    );
    let same_port = seed.records.binding(&prod, &b, "api", 8080, 20_000)?;
    assert!(
        is_conflict(&store.insert_binding(&same_port).await),
        "bindings.host_port is unique"
    );
    Ok(())
}

/// Updates re-check uniqueness: renaming into a taken key is a conflict.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_update_takes_a_used_key<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let mut owner = seed.owner.clone();
    owner.login = seed.bob.login.clone();
    assert!(
        is_conflict(&store.update_user(&owner).await),
        "update_user re-checks users.login"
    );
    let mut blog = seed
        .records
        .project("blog", "git.example.com", "owner/blog", &seed.owner)?;
    store.insert_project(&blog).await?;
    blog.name = seed.shop.name.clone();
    assert!(
        is_conflict(&store.update_project(&blog).await),
        "update_project re-checks (tenant, name)"
    );
    blog.name = ProjectName::new("blog")?;
    blog.repo = seed.shop.repo.clone();
    assert!(
        is_conflict(&store.update_project(&blog).await),
        "update_project re-checks (git_host, repo)"
    );
    let _qa = env_of(store, &seed, "qa").await?;
    let mut prod = env_of(store, &seed, "prod").await?;
    prod.name = EnvName::new("qa")?;
    assert!(
        is_conflict(&store.update_environment(&prod).await),
        "update_environment re-checks (project, name)"
    );
    let a = seed.records.domain(&seed.shop, "a.apps.test")?;
    let mut b = seed.records.domain(&seed.shop, "b.apps.test")?;
    store.insert_domain(&a).await?;
    store.insert_domain(&b).await?;
    b.host = a.host.clone();
    assert!(
        is_conflict(&store.update_domain(&b).await),
        "update_domain re-checks domains.host"
    );
    Ok(())
}

/// With no binding yet, allocation starts at the start of the range it is
/// given, whatever that range is (`[domains] port_range`).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn starts_at_range_start_when_no_binding_yet<S: Store>(store: &S) -> Result<(), Error> {
    // TC: BND-02
    let _seed = seed(store).await?;
    assert_eq!(
        store.free_host_port(&ports(20_000, 29_999)?).await?,
        Some(HostPort::new(20_000)?),
        "an empty store starts at the range start (BND-02)"
    );
    assert_eq!(
        store.free_host_port(&ports(25_000, 25_009)?).await?,
        Some(HostPort::new(25_000)?),
        "a configured range is honoured"
    );
    Ok(())
}

/// A port some binding uses is never handed out.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn skips_used_port_when_allocating<S: Store>(store: &S) -> Result<(), Error> {
    // TC: BND-02
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    bind_at(store, &seed, &prod, "a.apps.test", 20_000).await?;
    assert_eq!(
        store.free_host_port(&ports(20_000, 29_999)?).await?,
        Some(HostPort::new(20_001)?),
        "a used port is skipped (BND-02)"
    );
    Ok(())
}

/// A port freed by deleting its binding is not handed out by the next
/// allocation.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn avoids_freed_port_when_binding_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: BND-03
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    bind_at(store, &seed, &prod, "a.apps.test", 20_000).await?;
    for binding in store.bindings_of_environment(prod.id).await? {
        store.delete_binding(binding.id).await?;
    }
    assert_eq!(
        store.bindings_of_environment(prod.id).await?,
        vec![],
        "unbind removes the binding (BND-03)"
    );
    assert_eq!(
        store.free_host_port(&ports(20_000, 29_999)?).await?,
        Some(HostPort::new(20_001)?),
        "the just-freed port is not reused at once (BND-02, BND-03)"
    );
    Ok(())
}

/// After the end of the range, allocation wraps to its start; it is
/// `None` only when every port of the range is bound.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn wraps_around_when_range_end_reached<S: Store>(store: &S) -> Result<(), Error> {
    // TC: BND-02
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let range = ports(20_000, 20_002)?;
    bind_at(store, &seed, &prod, "b.apps.test", 20_001).await?;
    bind_at(store, &seed, &prod, "c.apps.test", 20_002).await?;
    assert_eq!(
        store.free_host_port(&range).await?,
        Some(HostPort::new(20_000)?),
        "wraps to the start after the end of the range (BND-02)"
    );
    bind_at(store, &seed, &prod, "a.apps.test", 20_000).await?;
    assert_eq!(
        store.free_host_port(&range).await?,
        None,
        "None when every port is bound"
    );
    Ok(())
}

/// `environments_on_branch` matches the project and the branch exactly.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn finds_environments_when_branch_matches<S: Store>(store: &S) -> Result<(), Error> {
    // TC: DEP-02
    let seed = seed(store).await?;
    let mut prod = seed.records.environment(&seed.shop, "prod")?;
    prod.pin = Some(Records::pin("main", &seed.owner)?);
    let mut qa = seed.records.environment(&seed.shop, "qa")?;
    qa.pin = Some(Records::pin("dev", &seed.owner)?);
    let unpinned = seed.records.environment(&seed.shop, "scratch")?;
    let blog = seed
        .records
        .project("blog", "git.example.com", "owner/blog", &seed.owner)?;
    store.insert_project(&blog).await?;
    let mut blog_prod = seed.records.environment(&blog, "prod")?;
    blog_prod.pin = Some(Records::pin("main", &seed.owner)?);
    for environment in [&prod, &qa, &unpinned, &blog_prod] {
        store.insert_environment(environment).await?;
    }
    assert_eq!(
        store
            .environments_on_branch(seed.shop.id, &Branch::new("main")?)
            .await?,
        vec![prod],
        "only shop's environment pinned to main"
    );
    assert_eq!(
        store
            .environments_on_branch(seed.shop.id, &Branch::new("feature/x")?)
            .await?,
        vec![],
        "no environment for an unpinned branch (DEP-02)"
    );
    Ok(())
}

/// One live token per project; a revoked token keeps its row with
/// `revoked_at` set and is no longer live.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_project_has_a_live_token<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let (old, old_secret) = seed.records.deploy_token(&seed.shop, &seed.owner);
    store.insert_deploy_token(&old).await?;
    let (second, second_secret) = seed.records.deploy_token(&seed.shop, &seed.owner);
    assert!(
        is_conflict(&store.insert_deploy_token(&second).await),
        "one live token per project"
    );
    let revoked_at = old.created_at.saturating_add(Duration::minutes(1));
    store.revoke_deploy_token(old.id, revoked_at).await?;
    assert_eq!(
        store.deploy_token(old.id).await?.and_then(|t| t.revoked_at),
        Some(revoked_at),
        "the revoked row keeps revoked_at"
    );
    assert!(
        store.insert_deploy_token(&second).await.is_ok(),
        "a revoked token frees the project"
    );
    assert_eq!(
        store.live_deploy_token_by_hash(&old_secret.hash()).await?,
        None,
        "a revoked token is not live"
    );
    assert_eq!(
        store
            .live_deploy_token_by_hash(&second_secret.hash())
            .await?,
        Some(second.clone()),
        "the new token is found by its hash"
    );
    assert_eq!(
        store.live_deploy_token(seed.shop.id).await?,
        Some(second),
        "the new token is the live one"
    );
    Ok(())
}

/// `replace_deploy_token` revokes the old token and stores the new one in
/// one step; when the new one cannot be stored, the old one stays live.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_token_in_one_step_when_rotated<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let (old, _) = seed.records.deploy_token(&seed.shop, &seed.owner);
    store.insert_deploy_token(&old).await?;
    let (new, _) = seed.records.deploy_token(&seed.shop, &seed.owner);
    let at = old.created_at.saturating_add(Duration::hours(1));
    store.replace_deploy_token(old.id, &new, at).await?;
    assert_eq!(
        (
            store.deploy_token(old.id).await?.and_then(|t| t.revoked_at),
            store.live_deploy_token(seed.shop.id).await?,
        ),
        (Some(at), Some(new.clone())),
        "old revoked at `at`, new live"
    );
    let (newer, _) = seed.records.deploy_token(&seed.shop, &seed.owner);
    let mut clash = newer.clone();
    clash.id = new.id;
    assert!(
        store
            .replace_deploy_token(new.id, &clash, at)
            .await
            .is_err(),
        "a new token whose id is taken cannot be stored"
    );
    assert_eq!(
        store.live_deploy_token(seed.shop.id).await?,
        Some(new),
        "a failed replace leaves the old token live"
    );
    Ok(())
}

/// Deploy numbers are unique per environment and count up from 1; pruning
/// keeps the newest `keep` and the current deploy.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn numbers_and_prunes_when_deploys_accumulate<S: Store>(store: &S) -> Result<(), Error> {
    // TC: DEP-09 (history trimmed, current kept)
    let seed = seed(store).await?;
    let mut prod = env_of(store, &seed, "prod").await?;
    assert_eq!(
        store.next_deploy_number(prod.id).await?,
        DeployNumber::FIRST,
        "numbering starts at 1"
    );
    let mut deploys = Vec::new();
    for number in 1..=5 {
        let deploy = seed
            .records
            .deploy(&prod, number, DeployStatus::Succeeded, &seed.owner)?;
        store.insert_deploy(&deploy).await?;
        deploys.push(deploy);
    }
    let dup = seed
        .records
        .deploy(&prod, 5, DeployStatus::Queued, &seed.owner)?;
    assert!(
        is_conflict(&store.insert_deploy(&dup).await),
        "(environment, number) is unique"
    );
    assert_eq!(
        store.next_deploy_number(prod.id).await?,
        DeployNumber::new(6)?,
        "next is max + 1"
    );
    prod.current_deploy_id = deploys.first().map(|d| d.id);
    store.update_environment(&prod).await?;
    assert_eq!(
        store.prune_deploys(prod.id, 2).await?,
        2,
        "keeps the newest 2 and the current deploy (DEP-09)"
    );
    let left: Vec<u32> = store
        .deploys_of_environment(prod.id)
        .await?
        .iter()
        .map(|d| d.number.get())
        .collect();
    assert_eq!(left, vec![5, 4, 1], "newest first, current #1 kept");
    let last = store
        .last_succeeded_deploy(prod.id)
        .await?
        .map(|d| d.number.get());
    assert_eq!(last, Some(5), "last succeeded is the newest succeeded");
    Ok(())
}

/// `deploys_with_status` returns matching deploys of every environment,
/// oldest first (creation order, not per-environment number order).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn lists_oldest_first_when_deploys_filtered_by_status<S: Store>(
    store: &S,
) -> Result<(), Error> {
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let qa = env_of(store, &seed, "qa").await?;
    let rows = [
        (&prod, 1, DeployStatus::Running),
        (&qa, 1, DeployStatus::Queued),
        (&prod, 2, DeployStatus::Succeeded),
        (&prod, 3, DeployStatus::Queued),
    ];
    let mut expected = Vec::new();
    for (environment, number, status) in rows {
        let deploy = seed
            .records
            .deploy(environment, number, status, &seed.owner)?;
        store.insert_deploy(&deploy).await?;
        if status != DeployStatus::Succeeded {
            expected.push(deploy.id);
        }
    }
    let found: Vec<_> = store
        .deploys_with_status(&[DeployStatus::Queued, DeployStatus::Running])
        .await?
        .iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(found, expected, "oldest first across environments");
    Ok(())
}

/// `update_deploy` replaces every mutable field of a deploy: status, log,
/// error, rollback target and timestamps.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn updates_status_when_deploy_updated<S: Store>(store: &S) -> Result<(), Error> {
    // TC: DEP-09 (status transitions are stored)
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let queued = seed
        .records
        .deploy(&prod, 2, DeployStatus::Queued, &seed.owner)?;
    store.insert_deploy(&queued).await?;
    let finished = seed
        .records
        .deploy(&prod, 2, DeployStatus::Failed, &seed.owner)?;
    let failed = crate::domain::Deploy {
        id: queued.id,
        log: "pulling\ncontainer web is unhealthy\n".to_owned(),
        error: Some("container web is unhealthy".to_owned()),
        rolled_back_to: Some(DeployNumber::FIRST),
        ..finished
    };
    assert_ne!(
        failed.started_at, queued.started_at,
        "the rule changes the timestamps"
    );
    store.update_deploy(&failed).await?;
    assert_eq!(
        store.deploy(queued.id).await?,
        Some(failed),
        "every changed field round-trips"
    );
    Ok(())
}

/// `environments` lists the environments of every project, by id.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn lists_every_environment_by_id_when_listing_all<S: Store>(
    store: &S,
) -> Result<(), Error> {
    let seed = seed(store).await?;
    let blog = seed
        .records
        .project("blog", "git.example.com", "owner/blog", &seed.owner)?;
    store.insert_project(&blog).await?;
    let built = [
        seed.records.environment(&seed.shop, "qa")?,
        seed.records.environment(&blog, "prod")?,
        seed.records.environment(&seed.shop, "dev")?,
    ];
    for environment in built.iter().rev() {
        store.insert_environment(environment).await?;
    }
    let mut expected: Vec<_> = built.iter().map(|e| e.id).collect();
    expected.sort();
    let found: Vec<_> = store.environments().await?.iter().map(|e| e.id).collect();
    assert_eq!(found, expected, "every project's environments, by id");
    Ok(())
}

/// Lists are sorted by their natural key whatever the insertion order.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
#[expect(clippy::too_many_lines, reason = "one rule over every table it covers")]
pub async fn sorts_by_natural_key_when_listing<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    for login in ["zed", "alice"] {
        store
            .insert_user(&seed.records.user(login, UserRole::User)?)
            .await?;
    }
    let logins: Vec<String> = store
        .users()
        .await?
        .iter()
        .map(|u| u.login.to_string())
        .collect();
    assert_eq!(logins, ["alice", "bob", "owner", "zed"], "users by login");

    for (name, repo) in [("zeta", "owner/zeta"), ("blog", "owner/blog")] {
        store
            .insert_project(
                &seed
                    .records
                    .project(name, "git.example.com", repo, &seed.owner)?,
            )
            .await?;
    }
    let names: Vec<String> = store
        .projects()
        .await?
        .iter()
        .map(|p| p.name.to_string())
        .collect();
    assert_eq!(names, ["blog", "shop", "zeta"], "projects by name");

    let mut prod = None;
    for name in ["qa", "prod", "dev"] {
        let environment = env_of(store, &seed, name).await?;
        if name == "prod" {
            prod = Some(environment);
        }
    }
    let names: Vec<String> = store
        .environments_of_project(seed.shop.id)
        .await?
        .iter()
        .map(|e| e.name.to_string())
        .collect();
    assert_eq!(names, ["dev", "prod", "qa"], "environments by name");

    let prod = prod.ok_or_else(|| Error::internal_msg("prod was inserted"))?;
    bind_at(store, &seed, &prod, "z.apps.test", 20_005).await?;
    bind_at(store, &seed, &prod, "a.apps.test", 20_009).await?;
    bind_at(store, &seed, &prod, "m.apps.test", 20_001).await?;
    let hosts: Vec<String> = store
        .domains_of_project(seed.shop.id)
        .await?
        .iter()
        .map(|d| d.host.to_string())
        .collect();
    assert_eq!(
        hosts,
        ["a.apps.test", "m.apps.test", "z.apps.test"],
        "domains by host"
    );
    let all_hosts: Vec<String> = store
        .domains()
        .await?
        .iter()
        .map(|d| d.host.to_string())
        .collect();
    assert_eq!(all_hosts, hosts, "every domain by host");
    let host_ports: Vec<u16> = store
        .bindings()
        .await?
        .iter()
        .map(|b| b.host_port.get())
        .collect();
    assert_eq!(
        host_ports,
        [20_001, 20_005, 20_009],
        "bindings by host port"
    );

    let scope = SecretScope::Project(seed.shop.id);
    for key in ["LOG_LEVEL", "API_KEY", "DATABASE_URL"] {
        store
            .put_secret(&seed.records.secret(scope, key, sealed(key), &seed.owner)?)
            .await?;
    }
    let keys: Vec<String> = store
        .secrets_of_scope(&scope)
        .await?
        .iter()
        .map(|s| s.key.to_string())
        .collect();
    assert_eq!(
        keys,
        ["API_KEY", "DATABASE_URL", "LOG_LEVEL"],
        "secrets by key"
    );

    for host in ["github.com", "git.example.com"] {
        store
            .put_git_connection(
                &seed
                    .records
                    .git_connection(&seed.owner, host, sealed("token"))?,
            )
            .await?;
    }
    let connection_hosts: Vec<String> = store
        .git_connections_of_user(seed.owner.id)
        .await?
        .iter()
        .map(|c| c.host.to_string())
        .collect();
    assert_eq!(
        connection_hosts,
        ["git.example.com", "github.com"],
        "git connections by host"
    );
    Ok(())
}

/// A domain with a binding cannot be deleted; without one it can.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn restricts_domain_delete_when_bound<S: Store>(store: &S) -> Result<(), Error> {
    // TC: DOM-06
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    bind_at(store, &seed, &prod, "shop.apps.test", 20_000).await?;
    let bound = store
        .domain_by_host(&Host::new("shop.apps.test")?)
        .await?
        .ok_or_else(|| Error::internal_msg("the bound domain was inserted"))?;
    assert!(
        is_conflict(&store.delete_domain(bound.id).await),
        "a bound domain is a conflict (DOM-06)"
    );
    let free = seed.records.domain(&seed.shop, "free.apps.test")?;
    store.insert_domain(&free).await?;
    store.delete_domain(free.id).await?;
    assert_eq!(
        store.domain(free.id).await?,
        None,
        "an unbound domain is deleted (DOM-06)"
    );
    Ok(())
}

/// A project with an environment or a domain is not deleted; once it has
/// neither, it is.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn restricts_project_delete_until_empty<S: Store>(store: &S) -> Result<(), Error> {
    // TC: REST-03
    let seed = seed(store).await?;
    let qa = env_of(store, &seed, "qa").await?;
    assert!(
        is_conflict(&store.delete_project(seed.shop.id).await),
        "a project with an environment is a conflict"
    );
    store.delete_environment(qa.id).await?;
    let domain = seed.records.domain(&seed.shop, "shop.apps.test")?;
    store.insert_domain(&domain).await?;
    assert!(
        is_conflict(&store.delete_project(seed.shop.id).await),
        "a project with a domain is a conflict"
    );
    assert_eq!(
        store.project(seed.shop.id).await?.map(|p| p.id),
        Some(seed.shop.id),
        "a refused delete keeps the project"
    );
    store.delete_domain(domain.id).await?;
    store.delete_project(seed.shop.id).await?;
    assert_eq!(store.project(seed.shop.id).await?, None, "the row is gone");
    assert!(
        is_not_found(&store.delete_project(seed.shop.id).await),
        "a missing project is not found"
    );
    Ok(())
}

/// Deleting a project deletes its deploy tokens and its secrets with their
/// versions; other projects keep theirs.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn cascades_when_project_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: REST-03
    let seed = seed(store).await?;
    let blog = seed
        .records
        .project("blog", "git.example.com", "owner/blog", &seed.owner)?;
    store.insert_project(&blog).await?;
    let (token, secret_token) = seed.records.deploy_token(&seed.shop, &seed.owner);
    store.insert_deploy_token(&token).await?;
    let (blog_token, _) = seed.records.deploy_token(&blog, &seed.owner);
    store.insert_deploy_token(&blog_token).await?;
    let secret = seed.records.secret(
        SecretScope::Project(seed.shop.id),
        "DATABASE_URL",
        sealed("shop"),
        &seed.owner,
    )?;
    store.put_secret(&secret).await?;
    let kept = seed.records.secret(
        SecretScope::Project(blog.id),
        "DATABASE_URL",
        sealed("blog"),
        &seed.owner,
    )?;
    store.put_secret(&kept).await?;

    store.delete_project(seed.shop.id).await?;

    assert_eq!(
        (
            store.deploy_token(token.id).await?,
            store
                .live_deploy_token_by_hash(&secret_token.hash())
                .await?,
        ),
        (None, None),
        "its deploy tokens are deleted"
    );
    assert_eq!(
        (
            store
                .secrets_of_scope(&SecretScope::Project(seed.shop.id))
                .await?,
            store.secret_versions(secret.id).await?,
        ),
        (vec![], vec![]),
        "its secrets are deleted with their versions"
    );
    assert_eq!(
        (
            store.live_deploy_token(blog.id).await?.map(|t| t.id),
            store
                .secrets_of_scope(&SecretScope::Project(blog.id))
                .await?,
        ),
        (Some(blog_token.id), vec![kept]),
        "another project keeps its token and secrets"
    );
    Ok(())
}

/// `record_lost_at` of a domain (the DOM-07 warning) reads back as it was
/// written, and clears again.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_record_lost_at_when_domain_updated<S: Store>(store: &S) -> Result<(), Error> {
    // TC: DOM-07
    let seed = seed(store).await?;
    let lost = seed
        .owner
        .created_at
        .saturating_add(Duration::milliseconds(90_061_234));
    let mut domain = seed.records.domain(&seed.shop, "www.shop.example")?;
    domain.kind = DomainKind::External;
    store.insert_domain(&domain).await?;
    domain.record_lost_at = Some(lost);
    store.update_domain(&domain).await?;
    assert_eq!(
        store
            .domain(domain.id)
            .await?
            .and_then(|d| d.record_lost_at),
        Some(lost),
        "record_lost_at is stored (DOM-07)"
    );
    domain.record_lost_at = None;
    store.update_domain(&domain).await?;
    assert_eq!(
        store
            .domain(domain.id)
            .await?
            .and_then(|d| d.record_lost_at),
        None,
        "record_lost_at clears (DOM-07)"
    );
    Ok(())
}

/// Deleting an environment deletes its bindings, deploys and secrets with
/// their versions; its domains and the project's secrets stay.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn cascades_when_environment_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: ENV-07, SEC-09
    let seed = seed(store).await?;
    let mut qa = env_of(store, &seed, "qa").await?;
    let domain = seed.records.domain(&seed.shop, "shop-qa.apps.test")?;
    store.insert_domain(&domain).await?;
    store
        .insert_binding(&seed.records.binding(&qa, &domain, "web", 3000, 20_000)?)
        .await?;
    let deploy = seed
        .records
        .deploy(&qa, 1, DeployStatus::Succeeded, &seed.owner)?;
    store.insert_deploy(&deploy).await?;
    qa.current_deploy_id = Some(deploy.id);
    qa.state = EnvState::Running;
    store.update_environment(&qa).await?;
    let env_secret = seed.records.secret(
        SecretScope::Environment(qa.id),
        "DATABASE_URL",
        sealed("qa-1"),
        &seed.owner,
    )?;
    store.put_secret(&env_secret).await?;
    store
        .put_secret(&seed.records.revise(&env_secret, sealed("qa-2"))?)
        .await?;
    let project_secret = seed.records.secret(
        SecretScope::Project(seed.shop.id),
        "DATABASE_URL",
        sealed("shop"),
        &seed.owner,
    )?;
    store.put_secret(&project_secret).await?;

    store.delete_environment(qa.id).await?;

    assert_eq!(store.environment(qa.id).await?, None, "the row is gone");
    assert_eq!(
        store.binding_by_domain(domain.id).await?,
        None,
        "its bindings are deleted (ENV-07)"
    );
    assert_eq!(
        store.domain(domain.id).await?,
        Some(domain),
        "its domains stay (ENV-07)"
    );
    assert_eq!(
        store.deploys_of_environment(qa.id).await?,
        vec![],
        "its deploys are deleted"
    );
    assert_eq!(
        store
            .secrets_of_scope(&SecretScope::Environment(qa.id))
            .await?,
        vec![],
        "its secrets are deleted (SEC-09)"
    );
    assert_eq!(
        store.secret_versions(env_secret.id).await?,
        vec![],
        "with their versions (SEC-09)"
    );
    assert_eq!(
        store
            .secrets_of_scope(&SecretScope::Project(seed.shop.id))
            .await?,
        vec![project_secret],
        "the project's secrets stay (SEC-09)"
    );
    Ok(())
}

/// The live setup token is exchanged once: the first
/// `exchange_setup_token` with its hash returns it and stores the session,
/// the next returns `None` and stores nothing (LOGN-01).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn exchanges_setup_token_once_when_hash_matches<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: LOGN-01
    let seed = seed(store).await?;
    let (token, secret) = seed.records.setup_token(&seed.owner);
    let (first, first_secret) = seed.records.session(&seed.owner);
    let (second, second_secret) = seed.records.session(&seed.owner);
    store.put_setup_token(&token).await?;
    assert_eq!(
        store.setup_token(&secret.hash()).await?,
        Some(token.clone()),
        "the live token is found by its hash"
    );
    assert_eq!(
        store.exchange_setup_token(&secret.hash(), &first).await?,
        Some(token),
        "the live token is handed back with its user"
    );
    assert_eq!(
        store.session_by_hash(&first_secret.hash()).await?,
        Some(first),
        "and its session is stored"
    );
    assert_eq!(
        (
            store.setup_token(&secret.hash()).await?,
            store.exchange_setup_token(&secret.hash(), &second).await?,
            store.session_by_hash(&second_secret.hash()).await?,
        ),
        (None, None, None),
        "the token is gone afterwards and a second exchange stores nothing (LOGN-01)"
    );
    Ok(())
}

/// A hash that is not the live token's exchanges nothing and leaves the
/// live token in place.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_setup_token_when_hash_differs<S: Store>(store: &S) -> Result<(), Error> {
    // TC: LOGN-01
    let seed = seed(store).await?;
    let (token, secret) = seed.records.setup_token(&seed.owner);
    let (_, stranger) = seed.records.setup_token(&seed.owner);
    let (guess, guess_secret) = seed.records.session(&seed.owner);
    let (live, _) = seed.records.session(&seed.owner);
    store.put_setup_token(&token).await?;
    assert_eq!(
        (
            store.setup_token(&stranger.hash()).await?,
            store.exchange_setup_token(&stranger.hash(), &guess).await?,
            store.session_by_hash(&guess_secret.hash()).await?,
        ),
        (None, None, None),
        "an unknown hash finds and exchanges nothing"
    );
    assert_eq!(
        store.exchange_setup_token(&secret.hash(), &live).await?,
        Some(token),
        "the live token survives a wrong guess"
    );
    Ok(())
}

/// A session the store refuses (here: its hash is taken) leaves the setup
/// token live: the exchange is one transaction (LOGN-01).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_setup_token_when_session_rejected<S: Store>(store: &S) -> Result<(), Error> {
    // TC: LOGN-01
    let seed = seed(store).await?;
    let (token, secret) = seed.records.setup_token(&seed.owner);
    let (existing, _) = seed.records.session(&seed.owner);
    let (mut clashing, _) = seed.records.session(&seed.owner);
    clashing.token_hash = existing.token_hash;
    store.insert_session(&existing).await?;
    store.put_setup_token(&token).await?;
    assert!(
        is_conflict(&store.exchange_setup_token(&secret.hash(), &clashing).await),
        "a session whose hash is taken is refused"
    );
    assert_eq!(
        store.setup_token(&secret.hash()).await?,
        Some(token),
        "and the setup token stays live"
    );
    Ok(())
}

/// The token opens a session of its own user only: a session of another
/// user exchanges nothing.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_setup_token_when_session_of_another_user<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: LOGN-01
    let seed = seed(store).await?;
    let (token, secret) = seed.records.setup_token(&seed.owner);
    let (bobs, _) = seed.records.session(&seed.bob);
    store.put_setup_token(&token).await?;
    assert_eq!(
        (
            store.exchange_setup_token(&secret.hash(), &bobs).await?,
            store.sessions_of_user(seed.bob.id).await?,
            store.setup_token(&secret.hash()).await?,
        ),
        (None, vec![], Some(token)),
        "the owner's token opens no session of bob"
    );
    Ok(())
}

/// At most one setup token is live: putting another replaces it.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_setup_token_when_put_again<S: Store>(store: &S) -> Result<(), Error> {
    // TC: LOGN-02
    let seed = seed(store).await?;
    let (first, first_secret) = seed.records.setup_token(&seed.owner);
    let (second, second_secret) = seed.records.setup_token(&seed.owner);
    store.put_setup_token(&first).await?;
    store.put_setup_token(&second).await?;
    assert_eq!(
        (
            store.setup_token(&first_secret.hash()).await?,
            store.setup_token(&second_secret.hash()).await?,
        ),
        (None, Some(second)),
        "the replaced token is dead and the newest is live"
    );
    Ok(())
}

/// A setup token of a user that does not exist is refused.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn not_found_when_setup_token_user_missing<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let ghost = seed.records.user("ghost", UserRole::Admin)?;
    let (token, _) = seed.records.setup_token(&ghost);
    assert!(
        is_not_found(&store.put_setup_token(&token).await),
        "the user of a setup token must exist"
    );
    Ok(())
}

/// Sessions are found by hash, listed per user and deleted per user.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn finds_and_deletes_sessions_when_user_has_several<S: Store>(
    store: &S,
) -> Result<(), Error> {
    let seed = seed(store).await?;
    let (first, first_secret) = seed.records.session(&seed.owner);
    let (second, _) = seed.records.session(&seed.owner);
    let (bobs, _) = seed.records.session(&seed.bob);
    for session in [&first, &second, &bobs] {
        store.insert_session(session).await?;
    }
    let mut dup = seed.records.session(&seed.bob).0;
    dup.token_hash = first.token_hash;
    assert!(
        is_conflict(&store.insert_session(&dup).await),
        "sessions.token_hash is unique"
    );
    assert_eq!(
        store.session_by_hash(&first_secret.hash()).await?,
        Some(first.clone()),
        "found by hash"
    );
    assert_eq!(
        store.sessions_of_user(seed.owner.id).await?,
        vec![first.clone(), second],
        "a user's sessions by id"
    );
    let mut extended = first.clone();
    extended.expires_at = first.expires_at.saturating_add(Duration::days(1));
    extended.last_used_at = first.last_used_at.saturating_add(Duration::hours(1));
    store.update_session(&extended).await?;
    assert_eq!(
        store.session_by_hash(&first_secret.hash()).await?,
        Some(extended),
        "update_session extends (LOGN-03)"
    );
    assert_eq!(
        store.delete_sessions_of_user(seed.owner.id).await?,
        2,
        "both of owner's sessions"
    );
    assert_eq!(
        store.sessions_of_user(seed.bob.id).await?,
        vec![bobs.clone()],
        "bob's session stays"
    );
    store.delete_session(bobs.id).await?;
    assert_eq!(
        store.sessions_of_user(seed.bob.id).await?,
        vec![],
        "delete_session removes one (LOGN-04)"
    );
    Ok(())
}

/// Rows with a missing parent and changes of missing rows are `NotFound`.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
#[expect(clippy::too_many_lines, reason = "one rule over every table it covers")]
pub async fn not_found_when_row_or_parent_missing<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let ghost_user = seed.records.user("ghost", UserRole::User)?;
    let ghost_project =
        seed.records
            .project("ghost", "git.example.com", "owner/ghost", &seed.owner)?;
    let ghost_env = seed.records.environment(&seed.shop, "ghost")?;
    let orphan_env = seed.records.environment(&ghost_project, "qa")?;
    let ghost_domain = seed.records.domain(&seed.shop, "ghost.apps.test")?;
    let prod = env_of(store, &seed, "prod").await?;
    let domain = seed.records.domain(&seed.shop, "shop.apps.test")?;
    store.insert_domain(&domain).await?;

    assert!(
        is_not_found(&store.insert_environment(&orphan_env).await),
        "environment of a missing project"
    );
    assert!(
        is_not_found(
            &store
                .insert_domain(&seed.records.domain(&ghost_project, "x.apps.test")?)
                .await
        ),
        "domain of a missing project"
    );
    assert!(
        is_not_found(
            &store
                .insert_binding(
                    &seed
                        .records
                        .binding(&ghost_env, &domain, "web", 3000, 20_000)?
                )
                .await
        ),
        "binding of a missing environment"
    );
    assert!(
        is_not_found(
            &store
                .insert_binding(
                    &seed
                        .records
                        .binding(&prod, &ghost_domain, "web", 3000, 20_001)?
                )
                .await
        ),
        "binding of a missing domain"
    );
    assert!(
        is_not_found(
            &store
                .insert_deploy(&seed.records.deploy(
                    &ghost_env,
                    1,
                    DeployStatus::Queued,
                    &seed.owner
                )?)
                .await
        ),
        "deploy of a missing environment"
    );
    assert!(
        is_not_found(
            &store
                .insert_session(&seed.records.session(&ghost_user).0)
                .await
        ),
        "session of a missing user"
    );
    assert!(
        is_not_found(
            &store
                .put_git_connection(&seed.records.git_connection(
                    &ghost_user,
                    "git.example.com",
                    sealed("t")
                )?)
                .await
        ),
        "git connection of a missing user"
    );
    assert!(
        is_not_found(
            &store
                .insert_deploy_token(&seed.records.deploy_token(&ghost_project, &seed.owner).0)
                .await
        ),
        "deploy token of a missing project"
    );
    for scope in [
        SecretScope::Project(ghost_project.id),
        SecretScope::Environment(ghost_env.id),
        // No project is in this tenant: the core knows no other trace of it.
        SecretScope::Tenant(TenantId::new(ulid::Ulid::from_parts(9, 9))),
    ] {
        assert!(
            is_not_found(
                &store
                    .put_secret(
                        &seed
                            .records
                            .secret(scope, "KEY", sealed("v"), &seed.owner)?
                    )
                    .await
            ),
            "secret of a missing {:?} scope",
            scope.kind()
        );
    }
    assert!(
        is_not_found(&store.update_user(&ghost_user).await),
        "update_user of a missing id"
    );
    assert!(
        is_not_found(&store.update_project(&ghost_project).await),
        "update_project of a missing id"
    );
    assert!(
        is_not_found(&store.update_environment(&ghost_env).await),
        "update_environment of a missing id"
    );
    assert!(
        is_not_found(&store.update_domain(&ghost_domain).await),
        "update_domain of a missing id"
    );
    assert!(
        is_not_found(
            &store
                .update_deploy(&seed.records.deploy(
                    &prod,
                    1,
                    DeployStatus::Succeeded,
                    &seed.owner
                )?)
                .await
        ),
        "update_deploy of a missing id"
    );
    assert!(
        is_not_found(
            &store
                .update_session(&seed.records.session(&seed.owner).0)
                .await
        ),
        "update_session of a missing id"
    );
    assert!(
        is_not_found(&store.delete_environment(ghost_env.id).await),
        "delete_environment of a missing id"
    );
    assert!(
        is_not_found(&store.delete_domain(ghost_domain.id).await),
        "delete_domain of a missing id"
    );
    let (token, _) = seed.records.deploy_token(&seed.shop, &seed.owner);
    assert!(
        is_not_found(&store.revoke_deploy_token(token.id, token.created_at).await),
        "revoke_deploy_token of a missing id"
    );
    let ghost_secret = seed.records.secret(
        SecretScope::Project(seed.shop.id),
        "GHOST",
        sealed("v"),
        &seed.owner,
    )?;
    assert!(
        is_not_found(&store.delete_secret(ghost_secret.id).await),
        "delete_secret of a missing id"
    );
    assert!(
        is_not_found(
            &store
                .delete_git_connection(seed.owner.id, &Host::new("git.example.com")?)
                .await
        ),
        "delete_git_connection without a connection"
    );
    Ok(())
}

/// `put_git_connection` keeps one connection per `(user, host)`: the second
/// one for the same host replaces the first.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_git_connection_when_same_user_and_host<S: Store>(
    store: &S,
) -> Result<(), Error> {
    let seed = seed(store).await?;
    let host = Host::new("git.example.com")?;
    let first = seed
        .records
        .git_connection(&seed.owner, "git.example.com", sealed("old"))?;
    store.put_git_connection(&first).await?;
    let second = seed
        .records
        .git_connection(&seed.owner, "git.example.com", sealed("new"))?;
    store.put_git_connection(&second).await?;
    assert_eq!(
        store.git_connections_of_user(seed.owner.id).await?,
        vec![second.clone()],
        "one connection per (user, host)"
    );
    assert_eq!(
        store.git_connection(seed.owner.id, &host).await?,
        Some(second),
        "the newer connection wins"
    );
    assert_eq!(
        store.git_connection(seed.bob.id, &host).await?,
        None,
        "another user has none"
    );
    store.delete_git_connection(seed.owner.id, &host).await?;
    assert_eq!(
        store.git_connection(seed.owner.id, &host).await?,
        None,
        "deleted"
    );
    Ok(())
}

/// Putting a secret again with the next version replaces the current
/// value and keeps both versions, newest first; a version already in the
/// history is a conflict.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_every_version_when_secret_put_again<S: Store>(store: &S) -> Result<(), Error> {
    // TC: SEC-02
    let seed = seed(store).await?;
    let scope = SecretScope::Project(seed.shop.id);
    let v1 = seed
        .records
        .secret(scope, "DATABASE_URL", sealed("one"), &seed.owner)?;
    store.put_secret(&v1).await?;
    let v2 = seed.records.revise(&v1, sealed("two"))?;
    store.put_secret(&v2).await?;
    let key = SecretKey::new("DATABASE_URL")?;
    assert_eq!(
        store.secret(&scope, &key).await?,
        Some(v2.clone()),
        "the current value is version 2 (SEC-02)"
    );
    let versions: Vec<(u32, SealedEnv)> = store
        .secret_versions(v1.id)
        .await?
        .into_iter()
        .map(|v| (v.version.get(), v.value))
        .collect();
    assert_eq!(
        versions,
        vec![(2, sealed("two")), (1, sealed("one"))],
        "both versions in secret_versions, newest first (SEC-02)"
    );
    let clash = crate::domain::Secret {
        value: sealed("three"),
        ..v2.clone()
    };
    assert!(
        is_conflict(&store.put_secret(&clash).await),
        "(secret_id, version) is unique"
    );
    assert_eq!(
        store.secret(&scope, &key).await?,
        Some(v2),
        "a conflicting put leaves the current value alone (atomic)"
    );
    let versions: Vec<u32> = store
        .secret_versions(v1.id)
        .await?
        .iter()
        .map(|v| v.version.get())
        .collect();
    assert_eq!(versions, vec![2, 1], "and adds no version");
    Ok(())
}

/// A tenant scope holds its own secrets.
///
/// Once a project is in the tenant, the same key in the tenant and in a
/// project of it are two secrets, each read back from its own scope, and
/// deleting the project keeps the tenant's (they belong to every project
/// of the tenant).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_tenant_secrets_apart_when_scopes_share_a_key<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: TEN-11
    let seed = seed(store).await?;
    let acme = TenantId::new(ulid::Ulid::from_parts(1, 1));
    let mut blog = seed
        .records
        .project("blog", "git.example.com", "acme/blog", &seed.owner)?;
    blog.tenant_id = Some(acme);
    store.insert_project(&blog).await?;
    let tenant = seed.records.secret(
        SecretScope::Tenant(acme),
        "REGION",
        sealed("eu"),
        &seed.owner,
    )?;
    store.put_secret(&tenant).await?;
    let project = seed.records.secret(
        SecretScope::Project(blog.id),
        "REGION",
        sealed("us"),
        &seed.owner,
    )?;
    store.put_secret(&project).await?;
    let key = SecretKey::new("REGION")?;
    assert_eq!(
        (
            store.secret(&SecretScope::Tenant(acme), &key).await?,
            store
                .secrets_of_scope(&SecretScope::Project(blog.id))
                .await?,
        ),
        (Some(tenant.clone()), vec![project]),
        "each scope reads back its own secret"
    );
    store.delete_project(blog.id).await?;
    assert_eq!(
        store.secrets_of_scope(&SecretScope::Tenant(acme)).await?,
        vec![tenant],
        "the tenant's secrets outlive a project of it"
    );
    Ok(())
}

/// `(scope, scope_id, key)` is unique; the same key in another scope is
/// fine.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_secret_key_taken_in_scope<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let qa = env_of(store, &seed, "qa").await?;
    let project = SecretScope::Project(seed.shop.id);
    store
        .put_secret(
            &seed
                .records
                .secret(project, "DATABASE_URL", sealed("a"), &seed.owner)?,
        )
        .await?;
    let other_id = seed
        .records
        .secret(project, "DATABASE_URL", sealed("b"), &seed.owner)?;
    assert!(
        is_conflict(&store.put_secret(&other_id).await),
        "(scope, key) is unique"
    );
    let in_env = seed.records.secret(
        SecretScope::Environment(qa.id),
        "DATABASE_URL",
        sealed("c"),
        &seed.owner,
    )?;
    assert!(
        store.put_secret(&in_env).await.is_ok(),
        "the same key in an environment scope"
    );
    Ok(())
}

/// Pruning keeps the newest `keep` versions.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn prunes_oldest_versions_when_more_than_kept<S: Store>(store: &S) -> Result<(), Error> {
    // TC: SEC-03
    let seed = seed(store).await?;
    let mut secret = seed.records.secret(
        SecretScope::Project(seed.shop.id),
        "TOKEN",
        sealed("1"),
        &seed.owner,
    )?;
    store.put_secret(&secret).await?;
    for n in 2..=22 {
        secret = seed.records.revise(&secret, sealed(&n.to_string()))?;
        store.put_secret(&secret).await?;
    }
    assert_eq!(
        store.prune_secret_versions(secret.id, 20).await?,
        2,
        "22 versions, 20 kept (SEC-03)"
    );
    let numbers: Vec<u32> = store
        .secret_versions(secret.id)
        .await?
        .iter()
        .map(|v| v.version.get())
        .collect();
    assert_eq!(
        numbers,
        (3..=22).rev().collect::<Vec<u32>>(),
        "versions 22 down to 3 (SEC-03)"
    );
    Ok(())
}

/// Deleting a secret deletes its versions.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn deletes_versions_when_secret_deleted<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let scope = SecretScope::Project(seed.shop.id);
    let v1 = seed
        .records
        .secret(scope, "TOKEN", sealed("1"), &seed.owner)?;
    store.put_secret(&v1).await?;
    store
        .put_secret(&seed.records.revise(&v1, sealed("2"))?)
        .await?;
    store.delete_secret(v1.id).await?;
    assert_eq!(
        (
            store.secret(&scope, &SecretKey::new("TOKEN")?).await?,
            store.secret_versions(v1.id).await?,
        ),
        (None, vec![]),
        "the secret and its versions are gone"
    );
    Ok(())
}

/// Every field of every record reads back as it was written, optional and
/// sub-second ones included.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
#[expect(clippy::too_many_lines, reason = "one rule over every table it covers")]
pub async fn round_trips_every_field_when_stored<S: Store>(store: &S) -> Result<(), Error> {
    let seed = seed(store).await?;
    let later = seed
        .owner
        .created_at
        .saturating_add(Duration::milliseconds(1_234));

    let mut carol = seed.records.user("carol", UserRole::User)?;
    carol.source = UserSource::Oidc(OidcIssuer::new("https://id.example.com")?);
    carol.subject = Some("sub-carol".to_owned());
    store.insert_user(&carol).await?;
    assert_eq!(store.user(carol.id).await?, Some(carol.clone()), "user");

    let mut blog = seed
        .records
        .project("blog", "git.example.com", "owner/blog", &carol)?;
    blog.tenant_id = Some(TenantId::new(ulid::Ulid::from_parts(7, 7)));
    blog.manifest = Some(r#"{"service":"web","port":3000}"#.to_owned());
    blog.template = Some(TemplateRef {
        group: "web".to_owned(),
        name: "acme-app".to_owned(),
    });
    store.insert_project(&blog).await?;
    assert_eq!(store.project(blog.id).await?, Some(blog.clone()), "project");

    let mut prod = seed.records.environment(&blog, "prod")?;
    prod.pin = Some(Records::pin("release/1.x", &carol)?);
    prod.owner_user_id = Some(carol.id);
    prod.limits.cpus = crate::domain::Cpus::parse("0.5")?;
    prod.limits.memory_mb = crate::domain::MemoryMb::new(256)?;
    prod.visibility = Visibility::Private;
    store.insert_environment(&prod).await?;
    let mut deploy = seed
        .records
        .deploy(&prod, 3, DeployStatus::Failed, &carol)?;
    deploy.source = DeploySource::Ci;
    deploy.git_ref = Some(Branch::new("release/1.x")?);
    deploy.tag = ImageTag::new("sha-abc")?;
    "pull\nup\n".clone_into(&mut deploy.log);
    deploy.error = Some("container web is unhealthy".to_owned());
    deploy.rolled_back_to = Some(DeployNumber::new(2)?);
    let (token, _) = seed.records.deploy_token(&blog, &carol);
    deploy.triggered_by = Trigger::Token(token.id);
    deploy.started_at = Some(later);
    deploy.finished_at = Some(later.saturating_add(Duration::seconds(5)));
    store.insert_deploy_token(&token).await?;
    store.insert_deploy(&deploy).await?;
    prod.state = EnvState::Failed;
    prod.current_deploy_id = Some(deploy.id);
    store.update_environment(&prod).await?;
    assert_eq!(
        store.environment(prod.id).await?,
        Some(prod.clone()),
        "environment"
    );
    assert_eq!(
        store.deploy(deploy.id).await?,
        Some(deploy.clone()),
        "deploy"
    );
    assert_eq!(
        store
            .deploy_by_number(prod.id, DeployNumber::new(3)?)
            .await?,
        Some(deploy),
        "deploy by number"
    );
    assert_eq!(
        store.deploy_token(token.id).await?,
        Some(token),
        "deploy token"
    );

    let mut domain = seed.records.domain(&blog, "blog.example.org")?;
    domain.tenant_id = blog.tenant_id;
    domain.kind = DomainKind::External;
    domain.verified_at = Some(later);
    domain.proxied = true;
    store.insert_domain(&domain).await?;
    assert_eq!(
        store.domain(domain.id).await?,
        Some(domain.clone()),
        "domain"
    );
    let binding = seed.records.binding(&prod, &domain, "api", 8080, 21_000)?;
    store.insert_binding(&binding).await?;
    assert_eq!(
        store.binding_by_domain(domain.id).await?,
        Some(binding),
        "binding"
    );

    let (mut session, _) = seed.records.session(&carol);
    session.kind = crate::domain::SessionKind::Browser;
    session.last_used_at = later;
    store.insert_session(&session).await?;
    assert_eq!(
        store.session_by_hash(&session.token_hash).await?,
        Some(session),
        "session"
    );

    let mut connection: GitConnection =
        seed.records
            .git_connection(&carol, "github.com", sealed("gh-token"))?;
    connection.provider = crate::domain::GitProvider::Github;
    connection.account_login = Login::new("carol-gh")?;
    connection.kind = crate::domain::CredentialKind::Oauth;
    connection.expires_at = Some(later);
    store.put_git_connection(&connection).await?;
    assert_eq!(
        store
            .git_connection(carol.id, &Host::new("github.com")?)
            .await?,
        Some(connection),
        "git connection"
    );

    for (scope, updated_by) in [
        (SecretScope::Project(blog.id), UpdatedBy::Import),
        (SecretScope::Environment(prod.id), UpdatedBy::Ci),
    ] {
        let mut secret = seed
            .records
            .secret(scope, "LIFTOFF_TOKEN", sealed("lf_x"), &carol)?;
        secret.ci = true;
        secret.updated_by = updated_by;
        secret.updated_at = later;
        store.put_secret(&secret).await?;
        assert_eq!(
            store.secret(&scope, &secret.key).await?,
            Some(secret.clone()),
            "secret in {:?} scope",
            scope.kind()
        );
        let versions: Vec<_> = store
            .secret_versions(secret.id)
            .await?
            .into_iter()
            .map(|v| (v.secret_id, v.version, v.value, v.updated_by, v.updated_at))
            .collect();
        assert_eq!(
            versions,
            vec![(secret.id, secret.version, secret.value, updated_by, later)],
            "secret version in {:?} scope",
            scope.kind()
        );
    }
    Ok(())
}

/// Audit entries come back in the order they were written.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn appends_in_order_when_audit_recorded<S: Store>(store: &S) -> Result<(), Error> {
    // TC: SEC-08 (audit rows carry no value)
    let seed = seed(store).await?;
    let first = seed
        .records
        .audit(Some(&seed.owner), "secret.set", "project:shop/DATABASE_URL");
    let second = seed
        .records
        .audit(None, "deploy.create", "environment:shop/prod");
    store.insert_audit(&first).await?;
    store.insert_audit(&second).await?;
    assert_eq!(
        store.audit_log().await?,
        vec![first, second],
        "oldest first"
    );
    Ok(())
}

fn module(name: &str) -> Result<ModuleName, Error> {
    Ok(ModuleName::new(name)?)
}

/// A key one module set is not there for another (`store-get` of a
/// module sees its own key space only).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn misses_key_of_other_module_when_only_one_set_it<S: ModuleStore>(
    store: &S,
) -> Result<(), Error> {
    // TC: MOD-01
    store
        .module_set(&module("probe")?, "greeting", b"hi")
        .await?;
    assert_eq!(
        store.module_get(&module("other")?, "greeting").await?,
        None,
        "another module's key"
    );
    Ok(())
}

/// Two modules setting the same key each read back their own value.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_module_keys_apart_when_two_modules_set_one_key<S: ModuleStore>(
    store: &S,
) -> Result<(), Error> {
    // TC: MOD-01
    let probe = module("probe")?;
    let other = module("other")?;
    store.module_set(&probe, "greeting", b"hi").await?;
    store.module_set(&other, "greeting", b"bye").await?;
    assert_eq!(
        (
            store.module_get(&probe, "greeting").await?,
            store.module_get(&other, "greeting").await?,
        ),
        (Some(b"hi".to_vec()), Some(b"bye".to_vec())),
        "each module's own value"
    );
    Ok(())
}

/// Setting a key again replaces its value.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_module_value_when_set_again<S: ModuleStore>(store: &S) -> Result<(), Error> {
    // TC: MOD-01 (`store-set` replaces)
    let probe = module("probe")?;
    store.module_set(&probe, "greeting", b"hi").await?;
    store.module_set(&probe, "greeting", b"hello").await?;
    assert_eq!(
        store.module_get(&probe, "greeting").await?,
        Some(b"hello".to_vec()),
        "the newer value"
    );
    Ok(())
}

/// Forgets every key of `module` when its storage is purged (`remove
/// --purge`).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn forgets_module_keys_when_purged<S: ModuleStore>(store: &S) -> Result<(), Error> {
    // TC: REG-07
    let probe = module("probe")?;
    store.module_set(&probe, "greeting", b"hi").await?;
    store.module_purge(&probe).await?;
    assert_eq!(
        store.module_get(&probe, "greeting").await?,
        None,
        "a purged module's key"
    );
    Ok(())
}

/// Purging one module leaves another module's keys.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_other_module_keys_when_one_is_purged<S: ModuleStore>(
    store: &S,
) -> Result<(), Error> {
    // TC: REG-07 (purge drops only the module's own storage)
    let probe = module("probe")?;
    let other = module("other")?;
    store.module_set(&probe, "greeting", b"hi").await?;
    store.module_set(&other, "greeting", b"bye").await?;
    store.module_purge(&probe).await?;
    assert_eq!(
        store.module_get(&other, "greeting").await?,
        Some(b"bye".to_vec()),
        "another module's key"
    );
    Ok(())
}

/// The module `name` at `version`, enabled, installed `seconds` after the
/// epoch, configured with a plain `greeting = "hi"` and a sealed `token`.
fn installed(name: &str, version: &str, seconds: i64) -> Result<InstalledModule, Error> {
    let mut config = ModuleConfigRecord::new();
    let _none = config.insert(
        "greeting".to_owned(),
        ConfigValue::Plain(serde_json::Value::from("hi")),
    );
    let _none = config.insert(
        "token".to_owned(),
        ConfigValue::Sealed(sealed("sealed:token")),
    );
    Ok(InstalledModule {
        name: module(name)?,
        version: ModuleVersion::new(version)?,
        sha256: "ab".repeat(32),
        enabled: true,
        config,
        error: None,
        installed_at: Timestamp::UNIX_EPOCH.saturating_add(Duration::seconds(seconds)),
    })
}

/// What a test compares of an installed module: name, version, SHA-256,
/// enabled, error, installation time and every config field
/// (`plain:<json>` or `sealed:<ciphertext>`).
type ModuleSummary = (
    String,
    String,
    String,
    bool,
    Option<String>,
    i64,
    Vec<(String, String)>,
);

fn summary(module: &InstalledModule) -> ModuleSummary {
    (
        module.name.to_string(),
        module.version.to_string(),
        module.sha256.clone(),
        module.enabled,
        module.error.clone(),
        module.installed_at.unix_timestamp(),
        module
            .config
            .iter()
            .map(|(field, value)| {
                let text = match value {
                    ConfigValue::Plain(json) => format!("plain:{json}"),
                    ConfigValue::Sealed(sealed) => format!("sealed:{}", sealed.as_str()),
                };
                (field.clone(), text)
            })
            .collect(),
    )
}

/// An installed module reads back as stored, its sealed config field
/// still sealed.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn round_trips_module_with_sealed_config_when_stored<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: REG-05 (the secret field stays sealed in `modules.config`)
    let probe = installed("probe", "0.1.0", 10)?;
    store.insert_module(&probe).await?;
    let read = store.module(&probe.name).await?;
    assert_eq!(
        read.as_ref().map(summary),
        Some(summary(&probe)),
        "the stored module"
    );
    Ok(())
}

/// A second module of the same name is a conflict.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_module_name_taken<S: Store>(store: &S) -> Result<(), Error> {
    // TC: REG-06 (one row per module name)
    store
        .insert_module(&installed("probe", "0.1.0", 10)?)
        .await?;
    let again = store.insert_module(&installed("probe", "0.2.0", 20)?).await;
    assert!(matches!(again, Err(Error::Conflict(_))), "{again:?}");
    Ok(())
}

/// Modules are listed in installation order, not by name.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn lists_modules_in_install_order_when_listed<S: Store>(store: &S) -> Result<(), Error> {
    // TC: REG-06 (hooks run in installation order, which
    // the daemon restores from the table)
    store
        .insert_module(&installed("tenants", "0.1.0", 10)?)
        .await?;
    store
        .insert_module(&installed("oidc", "0.1.0", 20)?)
        .await?;
    store
        .insert_module(&installed("dns.cloudflare", "0.1.0", 20)?)
        .await?;
    let names: Vec<String> = store
        .modules()
        .await?
        .iter()
        .map(|module| module.name.to_string())
        .collect();
    assert_eq!(
        names,
        vec!["tenants", "oidc", "dns.cloudflare"],
        "installation order (ties in insertion order)"
    );
    Ok(())
}

/// An update replaces `version`, `enabled`, `config` and `error`.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_module_when_updated<S: Store>(store: &S) -> Result<(), Error> {
    // TC: MOD-09 (`enabled` and `error` survive a restart in
    // `modules`)
    let probe = installed("probe", "0.1.0", 10)?;
    store.insert_module(&probe).await?;
    let mut changed = installed("probe", "0.1.1", 10)?;
    changed.sha256 = "cd".repeat(32);
    changed.enabled = false;
    changed.error = Some("module probe trapped".to_owned());
    let _old = changed.config.remove("token");
    store.update_module(&changed).await?;
    assert_eq!(
        store.module(&probe.name).await?.as_ref().map(summary),
        Some(summary(&changed)),
        "the updated module"
    );
    Ok(())
}

/// A deleted module is gone from lookups and the list.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn forgets_module_when_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: REG-07 (`remove` deletes the row)
    let probe = installed("probe", "0.1.0", 10)?;
    store.insert_module(&probe).await?;
    store.delete_module(&probe.name).await?;
    assert_eq!(
        (
            store.module(&probe.name).await?.is_none(),
            store.modules().await?.len()
        ),
        (true, 0),
        "the deleted module"
    );
    Ok(())
}

/// Updating or deleting a module that is not installed is not found.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn not_found_when_module_missing_on_update_or_delete<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: REG-07
    let probe = installed("probe", "0.1.0", 10)?;
    let updated = store.update_module(&probe).await;
    let deleted = store.delete_module(&probe.name).await;
    assert!(
        matches!(updated, Err(Error::NotFound(_))) && matches!(deleted, Err(Error::NotFound(_))),
        "{updated:?} {deleted:?}"
    );
    Ok(())
}

// ---- template cache ---------------------------------

/// 2026-10-13T00:00:00Z plus `minutes`, in whole seconds (what every
/// store keeps exactly).
fn minute(minutes: i64) -> Result<Timestamp, Error> {
    let start = time::OffsetDateTime::from_unix_timestamp(super::FixedClock::START_UNIX)
        .map_err(Error::internal)?;
    Ok(start.saturating_add(Duration::minutes(minutes)))
}

fn cached(
    user: &User,
    host: &str,
    repo: &str,
    manifest: Option<&str>,
    minutes: i64,
) -> Result<CachedTemplate, Error> {
    Ok(CachedTemplate {
        user_id: user.id,
        host: Host::new(host)?,
        repo: RepoRef::new(repo)?,
        manifest: manifest.map(ToOwned::to_owned),
        fetched_at: minute(minutes)?,
    })
}

/// Putting the cached read of the same `(user, host, repo)` again replaces
/// it (TPL-05: the catalogue refreshes an expired entry in place).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_cached_template_when_put_again<S: Store>(store: &S) -> Result<(), Error> {
    // TC: TPL-05
    let seed = seed(store).await?;
    let first = cached(&seed.owner, "git.example.com", "acme/app", Some("old"), 0)?;
    store.put_cached_template(&first).await?;
    let second = cached(&seed.owner, "git.example.com", "acme/app", Some("new"), 11)?;
    store.put_cached_template(&second).await?;
    assert_eq!(
        store
            .cached_template(seed.owner.id, &first.host, &first.repo)
            .await?,
        Some(second),
        "the newer read replaces the older one"
    );
    Ok(())
}

/// Entries of another user or another host never answer for each other
/// (TPL-05: a private template one user may read is not served to
/// another from the cache).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_cached_templates_apart_when_user_or_host_differs<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: TPL-05
    let seed = seed(store).await?;
    let owners = cached(
        &seed.owner,
        "git.example.com",
        "acme/app",
        Some("owner's"),
        0,
    )?;
    store.put_cached_template(&owners).await?;
    let repo = RepoRef::new("acme/app")?;
    assert_eq!(
        (
            store
                .cached_template(seed.bob.id, &Host::new("git.example.com")?, &repo)
                .await?,
            store
                .cached_template(seed.owner.id, &Host::new("github.com")?, &repo)
                .await?,
            store
                .cached_template(seed.owner.id, &Host::new("git.example.com")?, &repo)
                .await?,
        ),
        (None, None, Some(owners)),
        "one entry per (user, host, repo)"
    );
    Ok(())
}

/// A repository without `liftoff.toml` is cached as such (TPL-04, TPL-05:
/// the forge is not asked again within ten minutes either).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn round_trips_cached_template_without_manifest<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: TPL-04, TPL-05
    let seed = seed(store).await?;
    let entry = cached(&seed.owner, "git.example.com", "acme/plain", None, 3)?;
    store.put_cached_template(&entry).await?;
    assert_eq!(
        store
            .cached_template(seed.owner.id, &entry.host, &entry.repo)
            .await?,
        Some(entry),
        "no manifest round-trips as None"
    );
    Ok(())
}

/// Caching a read for a user who does not exist is
/// [`Error::NotFound`].
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn not_found_when_cached_template_user_missing<S: Store>(store: &S) -> Result<(), Error> {
    // TC: TPL-05
    let seed = seed(store).await?;
    let ghost = seed.records.user("ghost", UserRole::User)?;
    let result = store
        .put_cached_template(&cached(&ghost, "git.example.com", "acme/app", None, 0)?)
        .await;
    assert!(is_not_found(&result), "missing user: {result:?}");
    Ok(())
}

// ---- secret syncs ------------------------------------

/// A sync of `secret` to CI that sent `version` at `minutes`.
fn synced(secret: SecretId, version: u32, minutes: i64) -> Result<SecretSyncRecord, Error> {
    Ok(SecretSyncRecord {
        secret_id: secret,
        target: SyncTarget::Ci,
        version: Some(VersionNumber::new(version)?),
        synced_at: Some(minute(minutes)?),
        error: None,
    })
}

/// Stores the project secret `key` of `shop`.
async fn project_secret<S: Store>(
    store: &S,
    seed: &Seed,
    key: &str,
) -> Result<crate::domain::Secret, Error> {
    let secret = seed.records.secret(
        SecretScope::Project(seed.shop.id),
        key,
        sealed(key),
        &seed.owner,
    )?;
    store.put_secret(&secret).await?;
    Ok(secret)
}

/// Putting the sync of the same `(secret, target)` again replaces it
/// (SYNC-03: the row remembers the version the forge holds now).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_secret_sync_when_put_again<S: Store>(store: &S) -> Result<(), Error> {
    // TC: SYNC-03
    let seed = seed(store).await?;
    let secret = project_secret(store, &seed, "LIFTOFF_TOKEN").await?;
    store.put_secret_sync(&synced(secret.id, 1, 0)?).await?;
    let second = synced(secret.id, 2, 5)?;
    store.put_secret_sync(&second).await?;
    assert_eq!(
        store.secret_sync(secret.id, SyncTarget::Ci).await?,
        Some(second),
        "one row per (secret, target): the newer sync replaces the older"
    );
    Ok(())
}

/// A failed attempt reads back with its error and without a version
/// (SYNC-02: `secret_syncs.error`).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn round_trips_failed_secret_sync_when_stored<S: Store>(store: &S) -> Result<(), Error> {
    // TC: SYNC-02
    let seed = seed(store).await?;
    let secret = project_secret(store, &seed, "LIFTOFF_TOKEN").await?;
    let failed = SecretSyncRecord {
        secret_id: secret.id,
        target: SyncTarget::Ci,
        version: None,
        synced_at: None,
        error: Some("forge: 502 Bad Gateway".to_owned()),
    };
    store.put_secret_sync(&failed).await?;
    assert_eq!(
        store.secret_sync(secret.id, SyncTarget::Ci).await?,
        Some(failed),
        "the error and the absent version round-trip"
    );
    Ok(())
}

/// The sync of one secret never answers for another.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_secret_syncs_apart_when_secrets_differ<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: SYNC-03
    let seed = seed(store).await?;
    let token = project_secret(store, &seed, "LIFTOFF_TOKEN").await?;
    let other = project_secret(store, &seed, "REGISTRY_TOKEN").await?;
    let record = synced(token.id, 1, 0)?;
    store.put_secret_sync(&record).await?;
    assert_eq!(
        (
            store.secret_sync(token.id, SyncTarget::Ci).await?,
            store.secret_sync(other.id, SyncTarget::Ci).await?,
        ),
        (Some(record), None),
        "one row per secret"
    );
    Ok(())
}

/// Deleting a secret deletes its syncs.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn forgets_secret_sync_when_secret_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: SYNC-03
    let seed = seed(store).await?;
    let secret = project_secret(store, &seed, "LIFTOFF_TOKEN").await?;
    store.put_secret_sync(&synced(secret.id, 1, 0)?).await?;
    store.delete_secret(secret.id).await?;
    assert_eq!(
        store.secret_sync(secret.id, SyncTarget::Ci).await?,
        None,
        "the sync went with its secret"
    );
    Ok(())
}

/// Deleting an environment deletes the syncs of the secrets of its scope.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn forgets_secret_sync_when_environment_deleted<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: SYNC-03
    let seed = seed(store).await?;
    let qa = env_of(store, &seed, "qa").await?;
    let secret = seed.records.secret(
        SecretScope::Environment(qa.id),
        "DATABASE_URL",
        sealed("db"),
        &seed.owner,
    )?;
    store.put_secret(&secret).await?;
    store.put_secret_sync(&synced(secret.id, 1, 0)?).await?;
    store.delete_environment(qa.id).await?;
    assert_eq!(
        store.secret_sync(secret.id, SyncTarget::Ci).await?,
        None,
        "the sync went with the environment's secret"
    );
    Ok(())
}

/// Deleting a project deletes the syncs of its secrets; another
/// project's stay.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn forgets_secret_sync_when_project_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: SYNC-03
    let seed = seed(store).await?;
    let blog = seed
        .records
        .project("blog", "git.example.com", "owner/blog", &seed.owner)?;
    store.insert_project(&blog).await?;
    let secret = project_secret(store, &seed, "LIFTOFF_TOKEN").await?;
    let kept = seed.records.secret(
        SecretScope::Project(blog.id),
        "LIFTOFF_TOKEN",
        sealed("blog"),
        &seed.owner,
    )?;
    store.put_secret(&kept).await?;
    store.put_secret_sync(&synced(secret.id, 1, 0)?).await?;
    store.put_secret_sync(&synced(kept.id, 1, 0)?).await?;
    store.delete_project(seed.shop.id).await?;
    assert_eq!(
        (
            store.secret_sync(secret.id, SyncTarget::Ci).await?,
            store.secret_sync(kept.id, SyncTarget::Ci).await?,
        ),
        (None, Some(synced(kept.id, 1, 0)?)),
        "the sync went with the project's secret, the other project's stayed"
    );
    Ok(())
}

/// Recording the sync of a secret that does not exist is
/// [`Error::NotFound`].
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn not_found_when_secret_sync_secret_missing<S: Store>(store: &S) -> Result<(), Error> {
    // TC: SYNC-02
    let seed = seed(store).await?;
    let ghost = seed.records.secret(
        SecretScope::Project(seed.shop.id),
        "GHOST",
        sealed("ghost"),
        &seed.owner,
    )?;
    let result = store.put_secret_sync(&synced(ghost.id, 1, 0)?).await;
    assert!(is_not_found(&result), "missing secret: {result:?}");
    Ok(())
}

// ---- functions and tunnels ----------------------------------------------

/// A one-time link of `user` to `target`, issued at `at`, its hash that
/// of `secret`.
fn link(user: &User, secret: &str, target: LinkTarget, at: Timestamp) -> OneTimeLink {
    OneTimeLink {
        token_hash: TokenHash::of(secret),
        user_id: user.id,
        target,
        expires_at: at.saturating_add(Duration::minutes(10)),
        used_at: None,
        created_at: at,
    }
}

/// A one-time link reads back by the hash of its token, every field as
/// written (OTL-05).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn round_trips_one_time_link_by_hash_when_stored<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: OTL-05
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let at = seed
        .owner
        .created_at
        .saturating_add(Duration::milliseconds(1_234));
    let written = link(&seed.bob, "lfl_one", LinkTarget::Environment(prod.id), at);
    store.insert_one_time_link(&written).await?;
    assert_eq!(
        store.one_time_link(&TokenHash::of("lfl_one")).await?,
        Some(written),
        "OTL-05: the link is found by the hash of its token"
    );
    Ok(())
}

/// `one_time_links.token_hash` is unique (OTL-05).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_one_time_link_hash_taken<S: Store>(store: &S) -> Result<(), Error> {
    // TC: OTL-05
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let at = seed.owner.created_at;
    store
        .insert_one_time_link(&link(
            &seed.owner,
            "lfl_same",
            LinkTarget::Environment(prod.id),
            at,
        ))
        .await?;
    let again = store
        .insert_one_time_link(&link(
            &seed.bob,
            "lfl_same",
            LinkTarget::Environment(prod.id),
            at,
        ))
        .await;
    assert!(
        is_conflict(&again),
        "OTL-05: one row per token hash: {again:?}"
    );
    Ok(())
}

/// A one-time link needs its user (OTL-05).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn not_found_when_one_time_link_user_missing<S: Store>(store: &S) -> Result<(), Error> {
    // TC: OTL-05
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let ghost = seed.records.user("ghost", UserRole::User)?;
    let result = store
        .insert_one_time_link(&link(
            &ghost,
            "lfl_ghost",
            LinkTarget::Environment(prod.id),
            seed.owner.created_at,
        ))
        .await;
    assert!(is_not_found(&result), "missing user: {result:?}");
    Ok(())
}

/// [`Store::use_one_time_link`] marks a link used once: the first use
/// gets it with `used_at`, the second nothing, and the stored link keeps
/// the first `used_at` (OTL-02).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn uses_one_time_link_once_when_used_twice<S: Store>(store: &S) -> Result<(), Error> {
    // TC: OTL-02
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let at = seed.owner.created_at;
    let written = link(
        &seed.owner,
        "lfl_once",
        LinkTarget::Environment(prod.id),
        at,
    );
    store.insert_one_time_link(&written).await?;
    let first_use = at.saturating_add(Duration::seconds(30));
    let first = store
        .use_one_time_link(&written.token_hash, first_use)
        .await?;
    let second = store
        .use_one_time_link(
            &written.token_hash,
            at.saturating_add(Duration::seconds(31)),
        )
        .await?;
    let used = OneTimeLink {
        used_at: Some(first_use),
        ..written.clone()
    };
    assert_eq!(
        (
            first,
            second,
            store.one_time_link(&written.token_hash).await?
        ),
        (Some(used.clone()), None, Some(used)),
        "OTL-02: a link is used once"
    );
    Ok(())
}

/// Neither lookup nor use finds a link that was never stored.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn misses_one_time_link_when_hash_unknown<S: Store>(store: &S) -> Result<(), Error> {
    // TC: OTL-02
    let seed = seed(store).await?;
    let hash = TokenHash::of("lfl_never");
    assert_eq!(
        (
            store.one_time_link(&hash).await?,
            store
                .use_one_time_link(&hash, seed.owner.created_at)
                .await?
        ),
        (None, None),
        "an unknown hash finds nothing"
    );
    Ok(())
}

/// Pruning deletes the links whose `expires_at` is not after `now`, used
/// or not, and only those (OTL-01).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn prunes_expired_one_time_links_when_pruned<S: Store>(store: &S) -> Result<(), Error> {
    // TC: OTL-01
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let target = LinkTarget::Environment(prod.id);
    let now = seed.owner.created_at.saturating_add(Duration::hours(1));
    let mut expired = link(&seed.owner, "lfl_expired", target, now);
    expired.expires_at = now;
    let mut used_expired = link(&seed.owner, "lfl_used", target, now);
    used_expired.expires_at = now.saturating_sub(Duration::seconds(1));
    used_expired.used_at = Some(now.saturating_sub(Duration::minutes(5)));
    let mut live = link(&seed.owner, "lfl_live", target, now);
    live.expires_at = now.saturating_add(Duration::milliseconds(1));
    for written in [&expired, &used_expired, &live] {
        store.insert_one_time_link(written).await?;
    }
    let pruned = store.prune_one_time_links(now).await?;
    let left = (
        store.one_time_link(&expired.token_hash).await?.is_some(),
        store
            .one_time_link(&used_expired.token_hash)
            .await?
            .is_some(),
        store.one_time_link(&live.token_hash).await?.is_some(),
    );
    assert_eq!(
        (pruned, left),
        (2, (false, false, true)),
        "OTL-01: expired links go, live ones stay"
    );
    Ok(())
}

/// Revoking a user's sessions deletes their one-time links too.
///
/// [`Store::delete_sessions_of_user`] (`lof admin sessions revoke`)
/// deletes only that user's links: an unredeemed link is a session to
/// come (OTL-02).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn forgets_one_time_links_of_user_when_sessions_of_user_deleted<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: OTL-02
    let seed = seed(store).await?;
    let prod = env_of(store, &seed, "prod").await?;
    let target = LinkTarget::Environment(prod.id);
    let at = seed.owner.created_at;
    let bobs = link(&seed.bob, "lfl_bob", target, at);
    let owners = link(&seed.owner, "lfl_owner", target, at);
    for written in [&bobs, &owners] {
        store.insert_one_time_link(written).await?;
    }
    store.delete_sessions_of_user(seed.bob.id).await?;
    let left = (
        store.one_time_link(&bobs.token_hash).await?.is_some(),
        store.one_time_link(&owners.token_hash).await?.is_some(),
    );
    assert_eq!(
        left,
        (false, true),
        "OTL-02: a revoked user's links go with their sessions"
    );
    Ok(())
}

/// A tunnel environment reads back with its owner, `private`, `offline`
/// and without a pin (VIS-01).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn round_trips_tunnel_environment_when_stored<S: Store>(store: &S) -> Result<(), Error> {
    // TC: VIS-01
    let seed = seed(store).await?;
    let mut tunnel = seed.records.tunnel(&seed.shop, "shop-bob", &seed.bob)?;
    store.insert_environment(&tunnel).await?;
    tunnel.state = EnvState::Offline;
    store.update_environment(&tunnel).await?;
    let stored = store.environment(tunnel.id).await?;
    assert_eq!(
        (
            stored.clone(),
            stored.map(|e| (e.kind, e.owner_user_id, e.visibility, e.pin))
        ),
        (
            Some(tunnel),
            Some((
                crate::domain::EnvKind::Tunnel,
                Some(seed.bob.id),
                Visibility::Private,
                None
            ))
        ),
        "VIS-01: a tunnel keeps its kind, owner and visibility"
    );
    Ok(())
}

/// A function environment reads back without a pin.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn round_trips_function_environment_when_stored<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: VIS-02 (a function is an environment the access check guards
    // too)
    let seed = seed(store).await?;
    let mut function = seed.records.function(&seed.shop, "api")?;
    function.visibility = Visibility::Private;
    store.insert_environment(&function).await?;
    assert_eq!(
        store.environment(function.id).await?,
        Some(function),
        "a function environment round-trips"
    );
    Ok(())
}

/// A function deploy keeps its `component_hash`.
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn round_trips_component_hash_when_deploy_stored<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: FDEP-05 (redeploy --to N activates the component_hash of N)
    let seed = seed(store).await?;
    let function = seed.records.function(&seed.shop, "api")?;
    store.insert_environment(&function).await?;
    let mut deploy = seed
        .records
        .deploy(&function, 1, DeployStatus::Succeeded, &seed.owner)?;
    deploy.component_hash = Some(crate::domain::ComponentHash::of(b"\0asm component"));
    store.insert_deploy(&deploy).await?;
    assert_eq!(
        store.deploy(deploy.id).await?,
        Some(deploy),
        "FDEP-05: the component hash round-trips"
    );
    Ok(())
}

/// Two environments with the same key keep their own values (FN-03).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn keeps_kv_apart_when_environments_share_a_key<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: FN-03
    let seed = seed(store).await?;
    let api = env_of(store, &seed, "api").await?;
    let qa = env_of(store, &seed, "qa").await?;
    let at = seed.owner.created_at;
    store.kv_set(api.id, "counter", b"1", at).await?;
    store.kv_set(qa.id, "counter", b"7", at).await?;
    let prod = env_of(store, &seed, "prod").await?;
    assert_eq!(
        (
            store.kv_get(api.id, "counter").await?,
            store.kv_get(qa.id, "counter").await?,
            store.kv_get(prod.id, "counter").await?,
        ),
        (Some(b"1".to_vec()), Some(b"7".to_vec()), None),
        "FN-03: keys of environments are apart"
    );
    Ok(())
}

/// [`Store::kv_set`] replaces the value of a key (FN-03).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_kv_value_when_set_again<S: Store>(store: &S) -> Result<(), Error> {
    // TC: FN-03
    let seed = seed(store).await?;
    let api = env_of(store, &seed, "api").await?;
    let at = seed.owner.created_at;
    store.kv_set(api.id, "counter", b"1", at).await?;
    store
        .kv_set(
            api.id,
            "counter",
            b"2",
            at.saturating_add(Duration::seconds(1)),
        )
        .await?;
    assert_eq!(
        store.kv_get(api.id, "counter").await?,
        Some(b"2".to_vec()),
        "FN-03: set replaces"
    );
    Ok(())
}

/// [`Store::kv_delete`] removes a key; deleting a missing key is no error
/// (FN-03).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn forgets_kv_key_when_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: FN-03
    let seed = seed(store).await?;
    let api = env_of(store, &seed, "api").await?;
    store
        .kv_set(api.id, "counter", b"1", seed.owner.created_at)
        .await?;
    store.kv_delete(api.id, "counter").await?;
    let again = store.kv_delete(api.id, "counter").await;
    assert_eq!(
        (store.kv_get(api.id, "counter").await?, again.is_ok()),
        (None, true),
        "FN-03: delete removes the key, twice is no error"
    );
    Ok(())
}

/// [`Store::kv_usage`] counts the bytes of an environment's keys and
/// values, and only its own (FN-04).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn counts_kv_bytes_of_keys_and_values_when_asked<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: FN-04
    let seed = seed(store).await?;
    let api = env_of(store, &seed, "api").await?;
    let qa = env_of(store, &seed, "qa").await?;
    let at = seed.owner.created_at;
    store.kv_set(api.id, "counter", b"12", at).await?;
    store.kv_set(api.id, "name", b"shop", at).await?;
    store.kv_set(qa.id, "other", b"0123456789", at).await?;
    store.kv_set(api.id, "counter", b"123", at).await?;
    assert_eq!(
        store.kv_usage(api.id).await?,
        // "counter" 7 + "123" 3 + "name" 4 + "shop" 4
        18,
        "FN-04: usage is the keys and values of the environment"
    );
    Ok(())
}

/// Deleting an environment deletes its keys (FN-03).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn forgets_kv_when_environment_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: FN-03
    let seed = seed(store).await?;
    let api = env_of(store, &seed, "api").await?;
    store
        .kv_set(api.id, "counter", b"1", seed.owner.created_at)
        .await?;
    store.delete_environment(api.id).await?;
    assert_eq!(
        (
            store.kv_get(api.id, "counter").await?,
            store.kv_usage(api.id).await?
        ),
        (None, 0),
        "FN-03: the keys go with their environment"
    );
    Ok(())
}

/// A key of an environment that does not exist is refused (FN-03).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn not_found_when_kv_environment_missing<S: Store>(store: &S) -> Result<(), Error> {
    // TC: FN-03
    let seed = seed(store).await?;
    let missing = seed.records.environment(&seed.shop, "gone")?;
    let result = store
        .kv_set(missing.id, "counter", b"1", seed.owner.created_at)
        .await;
    assert!(is_not_found(&result), "missing environment: {result:?}");
    Ok(())
}

// ---- artifacts -----------------------------------------

/// The artifact `raw` of `owner`, private until an hour after `at`, at
/// version `current`.
fn artifact_row(raw: &str, owner: &User, at: Timestamp, current: u32) -> Result<Artifact, Error> {
    Ok(Artifact {
        id: ArtifactId::parse(raw)?,
        owner_user_id: owner.id,
        title: format!("Report {raw}"),
        visibility: Visibility::Private,
        current_version: VersionNumber::new(current)?,
        expires_at: Some(at.saturating_add(Duration::hours(1))),
        created_at: at,
        updated_at: at,
    })
}

/// Version `number` of `artifact` by `by` at `at`: `index.html` and
/// `css/site.css`.
fn artifact_version_row(
    artifact: &Artifact,
    number: u32,
    by: &User,
    at: Timestamp,
) -> Result<ArtifactVersion, Error> {
    let files = vec![
        ArtifactFile {
            name: ArtifactPath::index(),
            hash: BlobHash::parse(
                "c5484ff4f6706fb9d2418ffdc2b0ba515106dc32c68fbffb365b303bd9a6427e",
            )?,
            size: 11,
        },
        ArtifactFile {
            name: ArtifactPath::new("css/site.css")?,
            hash: BlobHash::parse(
                "7c98040a541657584690ae2a1cc3b42a8b53b159cc60c5d3abbfecbaeac6c94a",
            )?,
            size: 6,
        },
    ];
    Ok(ArtifactVersion {
        artifact_id: artifact.id,
        number: VersionNumber::new(number)?,
        title: format!("{} v{number}", artifact.title),
        size_bytes: 17,
        created_by: by.id,
        created_at: at,
        files,
    })
}

/// Stores the artifact `raw` of `owner` with versions `1..=count`, the
/// last one current; returns it as stored and its versions.
async fn artifact_with_versions<S: Store>(
    store: &S,
    owner: &User,
    raw: &str,
    at: Timestamp,
    count: u32,
) -> Result<(Artifact, Vec<ArtifactVersion>), Error> {
    let mut artifact = artifact_row(raw, owner, at, 1)?;
    let first = artifact_version_row(&artifact, 1, owner, at)?;
    store.insert_artifact(&artifact, &first).await?;
    let mut versions = vec![first];
    for number in 2..=count {
        let when = at.saturating_add(Duration::minutes(i64::from(number)));
        artifact = Artifact {
            current_version: VersionNumber::new(number)?,
            updated_at: when,
            ..artifact
        };
        let version = artifact_version_row(&artifact, number, owner, when)?;
        store.add_artifact_version(&artifact, &version).await?;
        versions.push(version);
    }
    Ok((artifact, versions))
}

/// The numbers of the stored versions of `id`, oldest first.
async fn artifact_numbers<S: Store>(store: &S, id: ArtifactId) -> Result<Vec<u32>, Error> {
    Ok(store
        .artifact_versions(id)
        .await?
        .iter()
        .map(|version| version.number.get())
        .collect())
}

/// An artifact and its first version read back as written, every field
/// and every file in order (ART-01).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn round_trips_artifact_with_first_version_when_stored<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: ART-01
    let seed = seed(store).await?;
    let at = seed
        .owner
        .created_at
        .saturating_add(Duration::milliseconds(1_234));
    let artifact = artifact_row("aB3dE6gH9jK2", &seed.bob, at, 1)?;
    let first = artifact_version_row(&artifact, 1, &seed.bob, at)?;
    store.insert_artifact(&artifact, &first).await?;
    assert_eq!(
        (
            store.artifact(artifact.id).await?,
            store.artifact_versions(artifact.id).await?,
            store
                .artifact_version(artifact.id, VersionNumber::FIRST)
                .await?,
        ),
        (Some(artifact), vec![first.clone()], Some(first)),
        "ART-01: the artifact and its version 1 read back as written"
    );
    Ok(())
}

/// A shared artifact (no `expires_at`) reads back as written (ART-06).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn replaces_artifact_row_when_updated<S: Store>(store: &S) -> Result<(), Error> {
    // TC: ART-06
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let (artifact, _versions) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", at, 1).await?;
    let shared = Artifact {
        visibility: Visibility::Shared,
        expires_at: None,
        title: "Shared report".to_owned(),
        updated_at: at.saturating_add(Duration::minutes(3)),
        ..artifact
    };
    store.update_artifact(&shared).await?;
    let missing = Artifact {
        id: ArtifactId::parse("zzzzzzzzzzzz")?,
        ..shared.clone()
    };
    let result = store.update_artifact(&missing).await;
    assert_eq!(
        (store.artifact(shared.id).await?, is_not_found(&result)),
        (Some(shared), true),
        "ART-06: the row is replaced; a missing one is NotFound: {result:?}"
    );
    Ok(())
}

/// `artifacts.id` is unique (ART-01).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_artifact_id_taken<S: Store>(store: &S) -> Result<(), Error> {
    // TC: ART-01
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", at, 1).await?;
    let again = artifact_row("aaaaaaaaaaa1", &seed.bob, at, 1)?;
    let first = artifact_version_row(&again, 1, &seed.bob, at)?;
    let result = store.insert_artifact(&again, &first).await;
    assert!(is_conflict(&result), "ART-01: one row per id: {result:?}");
    Ok(())
}

/// An artifact needs its owner, a version its artifact and its author
/// (ART-01, ART-04).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn not_found_when_artifact_owner_or_artifact_missing<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: ART-01, ART-04
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let stranger = seed.records.user("stranger", UserRole::User)?;
    let orphan = artifact_row("aaaaaaaaaaa1", &stranger, at, 1)?;
    let orphan_first = artifact_version_row(&orphan, 1, &stranger, at)?;
    let no_owner = store.insert_artifact(&orphan, &orphan_first).await;
    let missing = artifact_row("aaaaaaaaaaa2", &seed.owner, at, 2)?;
    let missing_second = artifact_version_row(&missing, 2, &seed.owner, at)?;
    let no_artifact = store.add_artifact_version(&missing, &missing_second).await;
    let (kept, _versions) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa3", at, 1).await?;
    let by_stranger = artifact_version_row(&kept, 2, &stranger, at)?;
    let no_author = store
        .add_artifact_version(
            &Artifact {
                current_version: VersionNumber::new(2)?,
                ..kept.clone()
            },
            &by_stranger,
        )
        .await;
    assert_eq!(
        (
            is_not_found(&no_owner),
            is_not_found(&no_artifact),
            is_not_found(&no_author),
            store.artifact(orphan.id).await?,
            artifact_numbers(store, kept.id).await?,
        ),
        (true, true, true, None, vec![1]),
        "ART-01: missing parents are NotFound and write nothing: \
         {no_owner:?} {no_artifact:?} {no_author:?}"
    );
    Ok(())
}

/// A new version and the artifact row that makes it current are written
/// together (ART-04).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn makes_added_version_current_when_version_added<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: ART-04
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let (artifact, versions) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", at, 2).await?;
    assert_eq!(
        (
            store.artifact(artifact.id).await?,
            store.artifact_versions(artifact.id).await?,
        ),
        (Some(artifact), versions),
        "ART-04: version 2 is current, version 1 is kept"
    );
    Ok(())
}

/// `(artifact_id, number)` of a version is unique (ART-04).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn conflicts_when_artifact_version_number_taken<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: ART-04
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let (artifact, _versions) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", at, 2).await?;
    let again = artifact_version_row(&artifact, 2, &seed.owner, at)?;
    let result = store.add_artifact_version(&artifact, &again).await;
    assert!(
        is_conflict(&result),
        "ART-04: one row per (artifact, number): {result:?}"
    );
    Ok(())
}

/// Artifacts list newest first, of one owner or of everybody (ART-12).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn lists_artifacts_newest_first_when_listed<S: Store>(store: &S) -> Result<(), Error> {
    // TC: ART-12
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let hour = |n: i64| at.saturating_add(Duration::hours(n));
    let (oldest, _) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", hour(0), 1).await?;
    let (bobs, _) = artifact_with_versions(store, &seed.bob, "aaaaaaaaaaa2", hour(1), 1).await?;
    let (newest, _) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa3", hour(2), 1).await?;
    let ids = |listed: Vec<Artifact>| listed.into_iter().map(|a| a.id).collect::<Vec<_>>();
    assert_eq!(
        (
            ids(store.artifacts(Some(seed.owner.id)).await?),
            ids(store.artifacts(Some(seed.bob.id)).await?),
            ids(store.artifacts(None).await?),
        ),
        (
            vec![newest.id, oldest.id],
            vec![bobs.id],
            vec![newest.id, bobs.id, oldest.id]
        ),
        "ART-12: own artifacts, or everybody's, newest first"
    );
    Ok(())
}

/// The current version cannot be deleted; another one goes with its
/// files; a missing one is [`Error::NotFound`] (ART-10).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn deletes_only_versions_that_are_not_current_when_deleted<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: ART-10
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let (artifact, _versions) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", at, 2).await?;
    let current = store
        .delete_artifact_version(artifact.id, VersionNumber::new(2)?)
        .await;
    store
        .delete_artifact_version(artifact.id, VersionNumber::FIRST)
        .await?;
    let missing = store
        .delete_artifact_version(artifact.id, VersionNumber::new(9)?)
        .await;
    assert_eq!(
        (
            is_conflict(&current),
            is_not_found(&missing),
            artifact_numbers(store, artifact.id).await?,
            store
                .artifact_version(artifact.id, VersionNumber::FIRST)
                .await?,
        ),
        (true, true, vec![2], None),
        "ART-10: {current:?} {missing:?}"
    );
    Ok(())
}

/// Pruning keeps the newest `keep` versions and the current one, and
/// answers what it deleted (ART-05).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn prunes_oldest_artifact_versions_but_never_current_when_pruned<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: ART-05
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let (newest, _) = artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", at, 5).await?;
    let (old_current, _) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa2", at, 5).await?;
    store
        .update_artifact(&Artifact {
            current_version: VersionNumber::FIRST,
            ..old_current.clone()
        })
        .await?;
    let numbers = |pruned: Vec<VersionNumber>| pruned.iter().map(|n| n.get()).collect::<Vec<_>>();
    let pruned_newest = numbers(store.prune_artifact_versions(newest.id, 3).await?);
    let pruned_old = numbers(store.prune_artifact_versions(old_current.id, 2).await?);
    assert_eq!(
        (
            pruned_newest,
            artifact_numbers(store, newest.id).await?,
            pruned_old,
            artifact_numbers(store, old_current.id).await?,
        ),
        (vec![1, 2], vec![3, 4, 5], vec![2, 3], vec![1, 4, 5]),
        "ART-05: the newest `keep` and the current version stay"
    );
    Ok(())
}

/// Deleting an artifact deletes its versions; a missing one is [`Error::NotFound`]
/// (ART-14).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn cascades_versions_when_artifact_deleted<S: Store>(store: &S) -> Result<(), Error> {
    // TC: ART-14
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let (gone, _) = artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", at, 3).await?;
    let (kept, _) = artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa2", at, 1).await?;
    store.delete_artifact(gone.id).await?;
    let again = store.delete_artifact(gone.id).await;
    assert_eq!(
        (
            store.artifact(gone.id).await?,
            artifact_numbers(store, gone.id).await?,
            is_not_found(&again),
            artifact_numbers(store, kept.id).await?,
        ),
        (None, Vec::new(), true, vec![1]),
        "ART-14: its versions go with it, others stay: {again:?}"
    );
    Ok(())
}

/// The expired artifacts are the private ones whose `expires_at` is not
/// after `now`, oldest first; never a shared one (ART-08).
///
/// # Errors
///
/// When the store itself fails.
///
/// # Panics
///
/// When the store breaks the rule.
pub async fn finds_only_expired_private_artifacts_when_asked<S: Store>(
    store: &S,
) -> Result<(), Error> {
    // TC: ART-08
    let seed = seed(store).await?;
    let at = seed.owner.created_at;
    let hour = |n: i64| at.saturating_add(Duration::hours(n));
    // Private until one hour after creation: at hour 1 and hour 2.
    let (first, _) = artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa1", hour(0), 1).await?;
    let (second, _) = artifact_with_versions(store, &seed.bob, "aaaaaaaaaaa2", hour(1), 1).await?;
    let (shared, _) =
        artifact_with_versions(store, &seed.owner, "aaaaaaaaaaa3", hour(0), 1).await?;
    store
        .update_artifact(&Artifact {
            visibility: Visibility::Shared,
            expires_at: None,
            ..shared
        })
        .await?;
    let ids = |listed: Vec<Artifact>| listed.into_iter().map(|a| a.id).collect::<Vec<_>>();
    assert_eq!(
        (
            ids(store
                .expired_artifacts(hour(1).saturating_sub(Duration::milliseconds(1)))
                .await?),
            ids(store.expired_artifacts(hour(1)).await?),
            ids(store.expired_artifacts(hour(9)).await?),
        ),
        (Vec::new(), vec![first.id], vec![first.id, second.id]),
        "ART-08: expired private artifacts, oldest first, never a shared one"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    mod in_memory_store {
        use crate::fakes::InMemoryStore;

        crate::store_contract_tests!(InMemoryStore::new);
    }
}
