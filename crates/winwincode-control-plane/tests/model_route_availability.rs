// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use winwincode_api::generated::{
    Actor, CredentialReferenceCreateCommand, CredentialReferenceCreateCommandCommand,
    CredentialReferenceCreatePayload, CredentialReferenceRevokeCommand,
    CredentialReferenceRevokeCommandCommand, CredentialReferenceRevokePayload, EmptyParameters,
    ModelRoute, ModelRouteAvailabilityListQuery, ModelRouteAvailabilityListQueryQuery,
    ModelRouteAvailabilityReason, ModelRouteAvailabilityStatus, OrganizationScope,
    OrganizationScopeKind, PageRequest, ProjectScope, ProjectScopeKind, RepositoryScope,
    RepositoryScopeKind, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CredentialReferenceService, ModelCapability, ModelRequestPoolConfig,
    ModelRouteAvailabilityErrorKind, ModelRouteAvailabilityService, ModelSettingsRequest,
    ModelSettingsService, ModelSettingsTarget, ModelSettingsValues, ModelToolSupport,
    ProviderCatalogRequest, ProviderCatalogService, ProviderDescriptor,
};
use winwincode_domain::{
    CredentialReferenceId, OpaqueCursor, OrganizationId, ProjectId, RepositoryId, RequestId,
    Revision, SchemaVersion, UserId, WorkspaceId,
};
use winwincode_storage::SqliteStorage;

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-model-route-availability-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn organization_scope(seed: u64) -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", seed)),
    }
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

fn project_scope(seed: u64) -> ProjectScope {
    ProjectScope {
        kind: ProjectScopeKind::Project,
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
    }
}

fn pool_config() -> ModelRequestPoolConfig {
    ModelRequestPoolConfig {
        max_routes: 8,
        max_active_per_route: 1,
        max_waiting_per_route: 1,
        max_exchange_records_per_route: 2,
        max_buffered_frames_per_stream: 8,
        max_buffered_bytes_per_stream: 8_192,
        resume_buffered_frames_per_stream: 2,
        resume_buffered_bytes_per_stream: 2_048,
    }
}

fn create_credential(
    storage: &mut SqliteStorage,
    scope: Scope,
    provider_id: &str,
    credential_seed: u64,
    request_seed: u64,
) -> CredentialReferenceCreateCommand {
    let command = CredentialReferenceCreateCommand {
        actor: actor(1),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
            display_name: format!("{provider_id} credential"),
            provider_id: provider_id.to_owned(),
            vault_locator: "local-fixture://SENSITIVE_MODEL_ROUTE_LOCATOR".to_owned(),
        },
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    };
    CredentialReferenceService::new(storage)
        .create(&command, 1_800_000_000_000)
        .expect("create Credential reference fixture");
    command
}

fn register_provider(
    storage: &mut SqliteStorage,
    scope: Scope,
    provider_id: &str,
    model_ids: &[&str],
    credential_seed: u64,
    request_seed: u64,
) {
    let descriptor = ProviderDescriptor {
        provider_id: provider_id.to_owned(),
        display_name: format!("{provider_id} display"),
        adapter_kind: "fixture-adapter".to_owned(),
        credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
        models: model_ids
            .iter()
            .map(|model_id| ModelCapability {
                model_id: (*model_id).to_owned(),
                display_name: format!("{model_id} display"),
                context_window_tokens: 128_000,
                max_output_tokens: 16_000,
                tool_support: ModelToolSupport::Parallel,
                reasoning_efforts: vec!["high".to_owned(), "medium".to_owned()],
            })
            .collect(),
    };
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(1),
                scope,
                request_id: RequestId(id("req", request_seed)),
                expected_catalog_version: 0,
            },
            &descriptor,
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("register Provider fixture");
}

fn configure_default(
    storage: &mut SqliteStorage,
    organization: OrganizationScope,
    provider_id: &str,
    model_id: &str,
    credential_seed: u64,
    request_seed: u64,
) {
    ModelSettingsService::new(storage)
        .update(
            &ModelSettingsRequest {
                actor: actor(1),
                target: ModelSettingsTarget::Organization {
                    scope: organization,
                },
                request_id: RequestId(id("req", request_seed)),
                expected_revision: 0,
            },
            ModelSettingsValues {
                default_model_route: Some(ModelRoute {
                    provider_id: provider_id.to_owned(),
                    model_id: model_id.to_owned(),
                    credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
                }),
                worker_concurrency_limit: 1,
            },
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("configure default ModelRoute fixture");
}

fn query(
    request_seed: u64,
    scope: RepositoryScope,
    limit: i64,
    cursor: Option<OpaqueCursor>,
) -> ModelRouteAvailabilityListQuery {
    ModelRouteAvailabilityListQuery {
        actor: actor(1),
        page: PageRequest { cursor, limit },
        parameters: EmptyParameters {},
        query: ModelRouteAvailabilityListQueryQuery::ModelRouteAvailabilityList,
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn seed_ready_routes(
    storage: &mut SqliteStorage,
    seed: u64,
    model_ids: &[&str],
    default_model_id: &str,
) -> CredentialReferenceCreateCommand {
    let organization = organization_scope(seed);
    let scope = Scope::OrganizationScope(organization.clone());
    let credential = create_credential(storage, scope.clone(), "provider-main", seed, seed * 10);
    register_provider(
        storage,
        scope,
        "provider-main",
        model_ids,
        seed,
        seed * 10 + 1,
    );
    configure_default(
        storage,
        organization,
        "provider-main",
        default_model_id,
        seed,
        seed * 10 + 2,
    );
    credential
}

#[test]
fn inherited_sources_multiple_routes_pagination_and_tenant_boundaries_are_deterministic() {
    let root = temporary_directory("inherited");
    let mut storage = SqliteStorage::open(&root).expect("open availability storage");
    seed_ready_routes(
        &mut storage,
        1,
        &["model-zeta", "model-alpha"],
        "model-zeta",
    );

    let first = ModelRouteAvailabilityService::new(&mut storage, Some(pool_config()))
        .list(&query(100, repository_scope(1), 1, None))
        .expect("read first ModelRoute page");
    assert_eq!(first.result.settings_revision, Some(Revision(1)));
    assert_eq!(first.result.request_pool_source, project_scope(1));
    assert_eq!(first.result.request_pool_revision, Revision(0));
    assert_eq!(
        first.result.settings_source,
        Some(Scope::OrganizationScope(organization_scope(1)))
    );
    assert_eq!(first.result.status, ModelRouteAvailabilityStatus::Enabled);
    assert_eq!(first.result.reason, ModelRouteAvailabilityReason::Ready);
    assert_eq!(first.result.items.len(), 1);
    assert_eq!(first.result.items[0].route.model_id, "model-alpha");
    assert_eq!(
        first.result.items[0].status,
        ModelRouteAvailabilityStatus::Enabled
    );
    assert_eq!(
        first.result.items[0].reason,
        ModelRouteAvailabilityReason::Ready
    );
    assert!(!first.result.items[0].is_default);
    assert_eq!(
        first.result.items[0].catalog_source,
        Scope::OrganizationScope(organization_scope(1))
    );
    assert!(first.page.has_more);
    let second_cursor = first.page.next_cursor.clone().expect("second page cursor");

    let mut crossed_actor = query(101, repository_scope(1), 1, Some(second_cursor.clone()));
    crossed_actor.actor = actor(2);
    let rejected = ModelRouteAvailabilityService::new(&mut storage, Some(pool_config()))
        .list(&crossed_actor)
        .expect_err("cursor is bound to the original actor");
    assert_eq!(
        rejected.kind(),
        ModelRouteAvailabilityErrorKind::InvalidRequest
    );

    let continuation = query(101, repository_scope(1), 1, Some(second_cursor));
    let second = ModelRouteAvailabilityService::new(&mut storage, Some(pool_config()))
        .list(&continuation)
        .expect("read second ModelRoute page");
    assert_eq!(second.result.items[0].route.model_id, "model-zeta");
    assert!(second.result.items[0].is_default);
    assert!(!second.page.has_more);

    let foreign = ModelRouteAvailabilityService::new(&mut storage, Some(pool_config()))
        .list(&query(102, repository_scope(2), 20, None))
        .expect("foreign tenant returns only its own empty facts");
    assert!(foreign.result.items.is_empty());
    assert_eq!(
        foreign.result.reason,
        ModelRouteAvailabilityReason::NoProvider
    );

    let encoded = serde_json::to_string(&first).expect("serialize secret-safe response");
    assert!(!encoded.contains("SENSITIVE_MODEL_ROUTE_LOCATOR"));
    assert!(!encoded.contains("vaultLocator"));
    assert!(!encoded.contains("queueDepth"));
    fs::remove_dir_all(root).expect("remove availability fixture");
}

#[test]
fn page_and_candidate_reasons_are_closed_for_missing_default_credential_and_pool() {
    let root = temporary_directory("closed-reasons");
    let mut storage = SqliteStorage::open(&root).expect("open availability storage");

    let empty = ModelRouteAvailabilityService::new(&mut storage, Some(pool_config()))
        .list(&query(200, repository_scope(3), 20, None))
        .expect("read empty authority");
    assert_eq!(
        empty.result.reason,
        ModelRouteAvailabilityReason::NoProvider
    );

    let organization = organization_scope(3);
    let organization_api = Scope::OrganizationScope(organization.clone());
    create_credential(
        &mut storage,
        organization_api.clone(),
        "provider-main",
        3,
        201,
    );
    register_provider(
        &mut storage,
        organization_api,
        "provider-main",
        &["model-main"],
        3,
        202,
    );
    let missing_default = ModelRouteAvailabilityService::new(&mut storage, Some(pool_config()))
        .list(&query(203, repository_scope(3), 20, None))
        .expect("read catalog without configured default");
    assert_eq!(
        missing_default.result.reason,
        ModelRouteAvailabilityReason::DefaultRouteInvalid
    );
    assert_eq!(
        missing_default.result.items[0].reason,
        ModelRouteAvailabilityReason::Ready
    );

    configure_default(
        &mut storage,
        organization,
        "provider-main",
        "model-main",
        3,
        204,
    );
    let unavailable_pool = ModelRouteAvailabilityService::new(&mut storage, None)
        .list(&query(205, repository_scope(3), 20, None))
        .expect("read without configured pool authority");
    assert_eq!(
        unavailable_pool.result.reason,
        ModelRouteAvailabilityReason::RequestPoolUnavailable
    );
    assert_eq!(
        unavailable_pool.result.items[0].reason,
        ModelRouteAvailabilityReason::RequestPoolUnavailable
    );
    fs::remove_dir_all(root).expect("remove availability fixture");
}

#[test]
fn credential_revocation_and_catalog_disable_fail_closed_without_leaking_details() {
    let root = temporary_directory("lifecycle");
    let mut storage = SqliteStorage::open(&root).expect("open availability storage");
    let credential = seed_ready_routes(&mut storage, 4, &["model-main"], "model-main");

    let revoke = CredentialReferenceRevokeCommand {
        actor: credential.actor,
        command: CredentialReferenceRevokeCommandCommand::CredentialReferenceRevoke,
        expected_revision: Revision(1),
        payload: CredentialReferenceRevokePayload {
            credential_reference_id: credential.payload.credential_reference_id,
        },
        request_id: RequestId(id("req", 403)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: credential.scope,
    };
    CredentialReferenceService::new(&mut storage)
        .revoke(&revoke, 1_800_000_000_001)
        .expect("revoke Credential fixture");
    let revoked = ModelRouteAvailabilityService::new(&mut storage, Some(pool_config()))
        .list(&query(404, repository_scope(4), 20, None))
        .expect("read revoked Credential route");
    assert_eq!(
        revoked.result.reason,
        ModelRouteAvailabilityReason::CredentialMissingOrRevoked
    );
    assert_eq!(revoked.result.items[0].credential_rotation_version, None);

    ProviderCatalogService::new(&mut storage)
        .disable(
            &ProviderCatalogRequest {
                actor: actor(1),
                scope: Scope::OrganizationScope(organization_scope(4)),
                request_id: RequestId(id("req", 405)),
                expected_catalog_version: 1,
            },
            "provider-main",
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("disable Provider fixture");
    let disabled = ModelRouteAvailabilityService::new(&mut storage, Some(pool_config()))
        .list(&query(406, repository_scope(4), 20, None))
        .expect("read disabled Provider route");
    assert_eq!(
        disabled.result.reason,
        ModelRouteAvailabilityReason::ProviderOrModelDisabled
    );
    assert_eq!(
        disabled.result.items[0].reason,
        ModelRouteAvailabilityReason::ProviderOrModelDisabled
    );

    let encoded = serde_json::to_string(&disabled).expect("serialize disabled route response");
    assert!(!encoded.contains("revokedAt"));
    assert!(!encoded.contains("vault"));
    assert!(!encoded.contains("activeCount"));
    fs::remove_dir_all(root).expect("remove availability fixture");
}
