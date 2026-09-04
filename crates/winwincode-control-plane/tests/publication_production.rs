// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceRevokeCommand,
    CredentialReferenceRevokeCommandCommand, CredentialReferenceRevokePayload,
    PublicationPublishCommand, PublicationPublishCommandCommand, PublicationPublishPayload,
    PublicationTarget, PublicationTargetProvider, Scope,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, CredentialReferenceService, EventPublishError,
    EventPublisher, LocalDeliveryAdapterConfig, LocalGitHubProviderConfig,
    LocalPublicationAdapterConfig, LocalPublicationProviderRegistry, LocalSecretStoreAdapter,
    OutboxEvent, PublicationProviderRegistry, PublicationProviderRegistryErrorKind, ResolvedSecret,
};
use winwincode_domain::{
    CredentialReferenceId, GitHubRepositorySlug, OrganizationId, ProjectId, RepositoryId,
    RequestId, Revision, SchemaVersion, UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_storage::SqliteStorage;

const SECRET: &[u8] = b"GITHUB_PROVIDER_SECRET";
static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-publication-production-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn create_command(seed: u64, scope: &RepositoryScope) -> CredentialReferenceCreateCommand {
    CredentialReferenceCreateCommand {
        actor: actor(seed),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", seed)),
            display_name: "GitHub Publication".to_owned(),
            provider_id: "github".to_owned(),
            vault_locator: "local-secret-store://write-only".to_owned(),
        },
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope.clone()),
    }
}

fn revoke_command(
    create: &CredentialReferenceCreateCommand,
    request_seed: u64,
) -> CredentialReferenceRevokeCommand {
    CredentialReferenceRevokeCommand {
        actor: create.actor.clone(),
        command: CredentialReferenceRevokeCommandCommand::CredentialReferenceRevoke,
        expected_revision: Revision(1),
        payload: CredentialReferenceRevokePayload {
            credential_reference_id: create.payload.credential_reference_id.clone(),
        },
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: create.scope.clone(),
    }
}

fn target() -> PublicationTarget {
    PublicationTarget {
        base_branch: "main".to_owned(),
        head_branch: "winwincode/delivery".to_owned(),
        head_repository: GitHubRepositorySlug("example/widget".to_owned()),
        provider: PublicationTargetProvider::Github,
        repository: GitHubRepositorySlug("example/widget".to_owned()),
    }
}

fn publish_command(scope: RepositoryScope, seed: u64) -> PublicationPublishCommand {
    PublicationPublishCommand {
        actor: actor(seed),
        command: PublicationPublishCommandCommand::PublicationPublish,
        expected_revision: Revision(0),
        payload: PublicationPublishPayload {
            candidate_digest: winwincode_domain::Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            delivery_id: winwincode_domain::DeliveryId(id("dlv", seed)),
            publication_id: winwincode_domain::PublicationId(id("pub", seed)),
            target: target(),
        },
        request_id: RequestId(id("req", seed + 100)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

#[test]
fn provider_session_seals_one_current_secret_and_restart_observes_revocation() {
    let root = temporary_directory("credential-lifecycle");
    let metadata = root.join("metadata");
    let secrets = root.join("secrets");
    let scope = repository_scope(1);
    let create = create_command(1, &scope);

    let mut storage = SqliteStorage::open(&metadata).expect("open Credential metadata");
    CredentialReferenceService::new(&mut storage)
        .create(&create, 1_800_000_000_000)
        .expect("create Credential reference");
    let reference = CredentialReferenceService::new(&mut storage)
        .resolve(&create.scope, &create.payload.credential_reference_id)
        .expect("resolve created reference");
    LocalSecretStoreAdapter::open(&secrets)
        .expect("open protected SecretStore")
        .store(
            &reference,
            ResolvedSecret::from_bytes(SECRET.to_vec()).expect("valid test secret"),
        )
        .expect("store exact secret version");
    drop(storage);

    let config = LocalGitHubProviderConfig::new(
        create.payload.credential_reference_id.clone(),
        "https://api.github.com",
        "example/widget",
        create.scope.clone(),
    );
    let mut registry = LocalPublicationProviderRegistry::open(config, &metadata, &secrets)
        .expect("open production provider registry");
    let session = registry
        .resolve(&target())
        .expect("resolve one frozen Credential session");

    let mut restarted = SqliteStorage::open(&metadata).expect("restart Credential metadata");
    CredentialReferenceService::new(&mut restarted)
        .revoke(&revoke_command(&create, 2), 1_800_000_001_000)
        .expect("revoke current Credential reference");
    drop(restarted);
    drop(session);

    let Err(error) = registry.resolve(&target()) else {
        panic!("a new provider session must observe revocation");
    };
    assert_eq!(
        error.kind(),
        PublicationProviderRegistryErrorKind::PermissionDenied
    );
    drop(registry);
    fs::remove_dir_all(root).expect("remove fixture directory");
}

#[test]
fn missing_or_foreign_provider_fails_before_a_session_is_created() {
    let root = temporary_directory("missing-provider");
    let scope = repository_scope(3);
    let reference = CredentialReferenceId(id("crd", 3));
    let config = LocalGitHubProviderConfig::new(
        reference,
        "https://api.github.com",
        "example/widget",
        Scope::RepositoryScope(scope),
    );
    let mut registry =
        LocalPublicationProviderRegistry::open(config, root.join("metadata"), root.join("secrets"))
            .expect("open empty production provider registry");

    let Err(error) = registry.resolve(&target()) else {
        panic!("missing reference must fail closed");
    };
    assert_eq!(
        error.kind(),
        PublicationProviderRegistryErrorKind::NotConfigured
    );
    let mut foreign = target();
    foreign.repository = GitHubRepositorySlug("another/repository".to_owned());
    let Err(error) = registry.resolve(&foreign) else {
        panic!("foreign target must not select a provider");
    };
    assert_eq!(
        error.kind(),
        PublicationProviderRegistryErrorKind::NotConfigured
    );
    drop(registry);
    fs::remove_dir_all(root).expect("remove fixture directory");
}

#[test]
fn production_startup_restarts_and_missing_delivery_writes_no_publication() {
    let root = temporary_directory("startup-restart");
    let data = root.join("data");
    let repository = root.join("repository");
    fs::create_dir_all(&repository).expect("create repository");
    run_git(&repository, &["init", "-q", "-b", "main"]);
    run_git(
        &repository,
        &["config", "user.email", "fixture@example.invalid"],
    );
    run_git(&repository, &["config", "user.name", "Fixture"]);
    fs::write(repository.join("README.md"), "fixture\n").expect("write repository fixture");
    run_git(&repository, &["add", "."]);
    run_git(&repository, &["commit", "-q", "-m", "fixture"]);

    let scope = repository_scope(5);
    let delivery = LocalDeliveryAdapterConfig::new(&repository, scope.clone());
    let publication = LocalPublicationAdapterConfig::try_new(
        scope.clone(),
        "example/widget",
        CredentialReferenceId(id("crd", 5)),
        "https://api.github.com",
        root.join("secrets"),
        vec![id("usr", 5)],
        vec![UserId(id("usr", 5))],
        86_400_000,
    )
    .expect("production Publication configuration");
    let mut first = ControlPlane::start_local_with_production_adapters(
        ControlPlaneConfig::local(&data),
        Box::new(NoopPublisher),
        delivery.clone(),
        publication.clone(),
    )
    .expect("start all production adapters");
    let command = publish_command(scope, 5);
    assert_eq!(
        first
            .publication_publish(&command)
            .expect_err("unknown Delivery must fail before Publication persistence")
            .public_code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );
    assert!(
        first
            .load_state(&format!("publication:{}", command.payload.publication_id.0))
            .expect("read Publication state")
            .is_none()
    );
    first.shutdown().expect("first shutdown");

    ControlPlane::start_local_with_production_adapters(
        ControlPlaneConfig::local(&data),
        Box::new(NoopPublisher),
        delivery,
        publication,
    )
    .expect("restart all production adapters")
    .shutdown()
    .expect("restart shutdown");
    fs::remove_dir_all(root).expect("remove fixture directory");
}

fn run_git(repository: &std::path::Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("run Git fixture command");
    assert!(
        status.success(),
        "Git fixture command failed: {arguments:?}"
    );
}
