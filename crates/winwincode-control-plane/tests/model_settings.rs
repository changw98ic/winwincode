// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use rusqlite::Connection;
use winwincode_api::generated::{
    Actor, EmptyParameters, ModelRoute, OrganizationScope, OrganizationScopeKind, PageRequest,
    ProjectScope, ProjectScopeKind, RepositoryScope, RepositoryScopeKind, Scope, SettingsGetQuery,
    SettingsGetQueryQuery, SettingsPatch, SettingsUpdateCommand, SettingsUpdateCommandCommand,
    SettingsUpdatePayload, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CredentialLeakGate, CredentialOutputBoundary, ModelCapability, ModelSettingsChange,
    ModelSettingsErrorKind, ModelSettingsRequest, ModelSettingsService, ModelSettingsTarget,
    ModelSettingsValues, ModelToolSupport, ProductStateStorage, ProviderCatalogRequest,
    ProviderCatalogService, ProviderDescriptor, ResolvedSecret,
};
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Revision, SchemaVersion, UserId, WorkspaceId,
};
use winwincode_storage::SqliteStorage;

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-model-settings-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn organization_scope(seed: u64) -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", seed)),
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

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn targets(seed: u64) -> [ModelSettingsTarget; 4] {
    let repository = repository_scope(seed);
    [
        ModelSettingsTarget::Organization {
            scope: organization_scope(seed),
        },
        ModelSettingsTarget::Project {
            scope: project_scope(seed),
        },
        ModelSettingsTarget::Repository {
            scope: repository.clone(),
        },
        ModelSettingsTarget::ProductSession {
            repository_scope: repository,
            product_session_id: ProductSessionId(id("psn", seed)),
        },
    ]
}

fn settings_request(
    request_seed: u64,
    target: ModelSettingsTarget,
    expected_revision: u64,
) -> ModelSettingsRequest {
    ModelSettingsRequest {
        actor: actor(),
        target,
        request_id: RequestId(id("req", request_seed)),
        expected_revision,
    }
}

fn settings_values(
    provider_id: &str,
    model_id: &str,
    credential_seed: u64,
    worker_concurrency_limit: u64,
) -> ModelSettingsValues {
    ModelSettingsValues {
        default_model_route: Some(ModelRoute {
            credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
        }),
        worker_concurrency_limit,
    }
}

fn cleared_settings(worker_concurrency_limit: u64) -> ModelSettingsValues {
    ModelSettingsValues {
        default_model_route: None,
        worker_concurrency_limit,
    }
}

fn update_command(
    request_seed: u64,
    command_scope: Scope,
    expected_revision: i64,
    default_model_route: Option<ModelRoute>,
    worker_concurrency_limit: i64,
) -> SettingsUpdateCommand {
    SettingsUpdateCommand {
        actor: actor(),
        command: SettingsUpdateCommandCommand::SettingsUpdate,
        expected_revision: Revision(expected_revision),
        payload: SettingsUpdatePayload {
            patch: SettingsPatch {
                default_model_route,
                worker_concurrency_limit,
            },
        },
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: command_scope,
    }
}

fn get_query(request_seed: u64, query_scope: Scope) -> SettingsGetQuery {
    SettingsGetQuery {
        actor: actor(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: EmptyParameters {},
        query: SettingsGetQueryQuery::SettingsGet,
        request_id: RequestId(id("req", request_seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: query_scope,
    }
}

fn descriptor(provider_id: &str, model_id: &str, credential_seed: u64) -> ProviderDescriptor {
    ProviderDescriptor {
        provider_id: provider_id.to_owned(),
        display_name: format!("{provider_id} display"),
        adapter_kind: "fixture-adapter".to_owned(),
        credential_reference_id: CredentialReferenceId(id("crd", credential_seed)),
        models: vec![ModelCapability {
            model_id: model_id.to_owned(),
            display_name: format!("{model_id} display"),
            context_window_tokens: 128_000,
            max_output_tokens: 16_000,
            tool_support: ModelToolSupport::Parallel,
            reasoning_efforts: vec!["high".to_owned(), "medium".to_owned()],
        }],
    }
}

fn register_provider(
    storage: &mut SqliteStorage,
    request_seed: u64,
    scope: Scope,
    provider_id: &str,
    model_id: &str,
    credential_seed: u64,
) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope,
                request_id: RequestId(id("req", request_seed)),
                expected_catalog_version: 0,
            },
            &descriptor(provider_id, model_id, credential_seed),
        )
        .expect("register Provider fixture");
}

#[test]
#[allow(clippy::too_many_lines)]
fn generated_update_and_get_atomically_project_route_concurrency_revision_and_defaults() {
    let root = temporary_directory("generated-projection");
    let organization = organization_scope(20);
    let organization_api_scope = Scope::OrganizationScope(organization.clone());
    let project = project_scope(20);
    let project_api_scope = Scope::ProjectScope(project.clone());
    let mut storage = SqliteStorage::open(&root).expect("open generated settings storage");

    let initial = ModelSettingsService::new(&mut storage)
        .get(&get_query(100, organization_api_scope.clone()))
        .expect("unconfigured settings have canonical defaults");
    assert_eq!(initial.result.revision, Revision(0));
    assert_eq!(initial.result.default_model_route, None);
    assert_eq!(initial.result.worker_concurrency_limit, 1);

    register_provider(
        &mut storage,
        101,
        organization_api_scope.clone(),
        "provider-settings",
        "model-settings",
        20,
    );
    let route = ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 20)),
        provider_id: "provider-settings".to_owned(),
        model_id: "model-settings".to_owned(),
    };
    let update = update_command(
        102,
        organization_api_scope.clone(),
        0,
        Some(route.clone()),
        7,
    );
    let updated = ModelSettingsService::new(&mut storage)
        .update_generated(&update)
        .expect("atomically update both canonical settings fields");
    assert_eq!(updated.previous_revision, Revision(0));
    assert_eq!(updated.current_revision, Revision(1));
    assert_eq!(updated.result.default_model_route, Some(route.clone()));
    assert_eq!(updated.result.worker_concurrency_limit, 7);
    assert_eq!(updated.result.revision, Revision(1));

    let projected = ModelSettingsService::new(&mut storage)
        .project(&ModelSettingsTarget::Organization {
            scope: organization.clone(),
        })
        .expect("project exact durable settings");
    assert_eq!(projected.revision, 1);
    assert_eq!(projected.worker_concurrency_limit, 7);
    assert_eq!(projected.default_model_route, Some(route.clone()));
    assert_eq!(
        projected.selection,
        Some(winwincode_control_plane::ModelSelection {
            provider_id: "provider-settings".to_owned(),
            model_id: "model-settings".to_owned(),
        })
    );

    let inherited = ModelSettingsService::new(&mut storage)
        .get(&get_query(103, project_api_scope.clone()))
        .expect("Project inherits only the effective model route");
    assert_eq!(inherited.result.revision, Revision(0));
    assert_eq!(inherited.result.default_model_route, Some(route.clone()));
    assert_eq!(inherited.result.worker_concurrency_limit, 1);
    let project_update = update_command(104, project_api_scope.clone(), 0, Some(route.clone()), 9);
    let project_updated = ModelSettingsService::new(&mut storage)
        .update_generated(&project_update)
        .expect("Project owns its concurrency and model override atomically");
    assert_eq!(project_updated.result.worker_concurrency_limit, 9);
    assert_eq!(project_updated.result.revision, Revision(1));
    let project_cleared = ModelSettingsService::new(&mut storage)
        .update_generated(&update_command(120, project_api_scope.clone(), 1, None, 10))
        .expect("null route and new concurrency replace the complete Project value");
    assert_eq!(
        project_cleared.result.default_model_route,
        Some(route.clone())
    );
    assert_eq!(project_cleared.result.worker_concurrency_limit, 10);
    assert_eq!(project_cleared.result.revision, Revision(2));
    let cleared_projection = ModelSettingsService::new(&mut storage)
        .project(&ModelSettingsTarget::Project {
            scope: project.clone(),
        })
        .expect("cleared Project override resolves the inherited route");
    assert_eq!(cleared_projection.selection, None);
    assert_eq!(cleared_projection.default_model_route, Some(route.clone()));
    assert_eq!(cleared_projection.worker_concurrency_limit, 10);

    let stale = ModelSettingsService::new(&mut storage)
        .update_generated(&update_command(
            105,
            organization_api_scope.clone(),
            0,
            Some(route.clone()),
            8,
        ))
        .expect_err("stale revision rejects both replacement fields");
    assert_eq!(stale.kind(), ModelSettingsErrorKind::RevisionConflict);
    for invalid_limit in [0, 10_001] {
        let error = ModelSettingsService::new(&mut storage)
            .update_generated(&update_command(
                106 + u64::try_from(invalid_limit).expect("limit"),
                organization_api_scope.clone(),
                1,
                Some(route.clone()),
                invalid_limit,
            ))
            .expect_err("concurrency stays inside canonical bounds");
        assert_eq!(error.kind(), ModelSettingsErrorKind::InvalidRequest);
    }
    let stale_reference = ModelSettingsService::new(&mut storage)
        .update_generated(&update_command(
            20_108,
            organization_api_scope.clone(),
            1,
            Some(ModelRoute {
                credential_reference_id: CredentialReferenceId(id("crd", 99)),
                ..route.clone()
            }),
            8,
        ))
        .expect_err("route reference must match the current catalog");
    assert_eq!(
        stale_reference.kind(),
        ModelSettingsErrorKind::InvalidRequest
    );

    Box::new(storage).close().expect("close before restart");
    let mut storage = SqliteStorage::open(&root).expect("reopen generated settings storage");
    let restarted = ModelSettingsService::new(&mut storage)
        .get(&get_query(109, organization_api_scope.clone()))
        .expect("durable settings survive restart");
    assert_eq!(restarted.result, updated.result);
    let exact_replay = ModelSettingsService::new(&mut storage)
        .update_generated(&update)
        .expect("exact generated update replay returns original projection");
    assert_eq!(exact_replay, updated);
    let changed_body = ModelSettingsService::new(&mut storage)
        .update_generated(&SettingsUpdateCommand {
            payload: SettingsUpdatePayload {
                patch: SettingsPatch {
                    default_model_route: Some(route.clone()),
                    worker_concurrency_limit: 8,
                },
            },
            ..update.clone()
        })
        .expect_err("same requestId with changed concurrency conflicts");
    assert_eq!(changed_body.kind(), ModelSettingsErrorKind::RequestConflict);

    ProviderCatalogService::new(&mut storage)
        .disable(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: organization_api_scope.clone(),
                request_id: RequestId(id("req", 110)),
                expected_catalog_version: 1,
            },
            "provider-settings",
        )
        .expect("disable configured Provider");
    let disabled = ModelSettingsService::new(&mut storage)
        .get(&get_query(111, organization_api_scope.clone()))
        .expect_err("current projection fails closed for disabled Provider");
    assert_eq!(disabled.kind(), ModelSettingsErrorKind::ProviderDisabled);
    let replay_after_disable = ModelSettingsService::new(&mut storage)
        .update_generated(&update)
        .expect("replay returns original response before current catalog lookup");
    assert_eq!(replay_after_disable, updated);

    let foreign = ModelSettingsService::new(&mut storage)
        .get(&get_query(
            112,
            Scope::OrganizationScope(organization_scope(21)),
        ))
        .expect("foreign scope has its own defaults");
    assert_eq!(foreign.result.revision, Revision(0));
    assert_eq!(foreign.result.default_model_route, None);
    assert_eq!(foreign.result.worker_concurrency_limit, 1);

    Box::new(storage)
        .close()
        .expect("close generated settings storage");
    fs::remove_dir_all(root).expect("remove generated settings fixture");
}

#[test]
fn concurrent_exact_full_replacement_has_one_revision_and_one_projection() {
    const CALLERS: usize = 4;
    let root = temporary_directory("concurrent-generated-update");
    let organization_api_scope = Scope::OrganizationScope(organization_scope(30));
    let mut setup = SqliteStorage::open(&root).expect("open concurrent settings setup");
    register_provider(
        &mut setup,
        201,
        organization_api_scope.clone(),
        "provider-concurrent",
        "model-concurrent",
        30,
    );
    Box::new(setup).close().expect("close concurrent setup");
    let command = Arc::new(update_command(
        202,
        organization_api_scope.clone(),
        0,
        Some(ModelRoute {
            credential_reference_id: CredentialReferenceId(id("crd", 30)),
            provider_id: "provider-concurrent".to_owned(),
            model_id: "model-concurrent".to_owned(),
        }),
        12,
    ));
    let barrier = Arc::new(Barrier::new(CALLERS));
    let storages = (0..CALLERS)
        .map(|_| SqliteStorage::open(&root).expect("open concurrent settings connection"))
        .collect::<Vec<_>>();
    let handles = storages
        .into_iter()
        .map(|mut storage| {
            let barrier = Arc::clone(&barrier);
            let command = Arc::clone(&command);
            thread::spawn(move || {
                barrier.wait();
                let response = ModelSettingsService::new(&mut storage)
                    .update_generated(&command)
                    .expect("concurrent exact settings update");
                Box::new(storage)
                    .close()
                    .expect("close concurrent connection");
                response
            })
        })
        .collect::<Vec<_>>();
    let responses = handles
        .into_iter()
        .map(|handle| handle.join().expect("join concurrent settings update"))
        .collect::<Vec<_>>();
    assert!(responses.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(responses[0].current_revision, Revision(1));
    assert_eq!(responses[0].result.worker_concurrency_limit, 12);

    let mut storage = SqliteStorage::open(&root).expect("reopen concurrent settings storage");
    let recovered = ModelSettingsService::new(&mut storage)
        .get(&get_query(203, organization_api_scope))
        .expect("read one concurrent durable result");
    assert_eq!(recovered.result, responses[0].result);
    assert_eq!(storage.pending_events().expect("pending events").len(), 2);
    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("remove concurrent settings fixture");
}

#[test]
#[allow(clippy::too_many_lines)]
fn priority_clear_and_catalog_rejection_rules_are_stable() {
    let root = temporary_directory("priority");
    let [organization, project, repository, product_session] = targets(1);
    let mut storage = SqliteStorage::open(&root).expect("open model settings storage");
    register_provider(
        &mut storage,
        1,
        Scope::OrganizationScope(organization_scope(1)),
        "provider-org",
        "model-org",
        1,
    );
    register_provider(
        &mut storage,
        2,
        Scope::ProjectScope(project_scope(1)),
        "provider-project",
        "model-project",
        2,
    );

    let no_route = ModelSettingsService::new(&mut storage)
        .resolve(&product_session)
        .expect_err("unconfigured hierarchy has no route");
    assert_eq!(no_route.kind(), ModelSettingsErrorKind::NoConfiguredRoute);

    let organization_request = settings_request(10, organization.clone(), 0);
    ModelSettingsService::new(&mut storage)
        .update(
            &organization_request,
            settings_values("provider-org", "model-org", 1, 1),
        )
        .expect("set Organization route");
    let inherited_organization = ModelSettingsService::new(&mut storage)
        .resolve(&product_session)
        .expect("inherit Organization route");
    assert_eq!(inherited_organization.provider_id, "provider-org");
    assert_eq!(
        inherited_organization.credential_reference_id,
        CredentialReferenceId(id("crd", 1))
    );

    let project_request = settings_request(11, project.clone(), 0);
    ModelSettingsService::new(&mut storage)
        .update(
            &project_request,
            settings_values("provider-project", "model-project", 2, 1),
        )
        .expect("set Project route");
    let inherited_project = ModelSettingsService::new(&mut storage)
        .resolve(&product_session)
        .expect("Project overrides Organization");
    assert_eq!(inherited_project.provider_id, "provider-project");
    assert_eq!(
        inherited_project.credential_reference_id,
        CredentialReferenceId(id("crd", 2))
    );

    let repository_request = settings_request(12, repository.clone(), 0);
    ModelSettingsService::new(&mut storage)
        .update(
            &repository_request,
            settings_values("provider-org", "model-org", 1, 1),
        )
        .expect("set Repository route");
    let inherited_repository = ModelSettingsService::new(&mut storage)
        .resolve(&product_session)
        .expect("Repository overrides Project");
    assert_eq!(inherited_repository.provider_id, "provider-org");

    let session_request = settings_request(13, product_session.clone(), 0);
    ModelSettingsService::new(&mut storage)
        .update(
            &session_request,
            settings_values("provider-project", "model-project", 2, 1),
        )
        .expect("set ProductSession route");
    let direct_session = ModelSettingsService::new(&mut storage)
        .resolve(&product_session)
        .expect("ProductSession overrides Repository");
    assert_eq!(direct_session.provider_id, "provider-project");

    ModelSettingsService::new(&mut storage)
        .update(
            &settings_request(14, product_session.clone(), 1),
            cleared_settings(1),
        )
        .expect("clear ProductSession override");
    assert_eq!(
        ModelSettingsService::new(&mut storage)
            .resolve(&product_session)
            .expect("fall back to Repository")
            .provider_id,
        "provider-org"
    );
    ModelSettingsService::new(&mut storage)
        .update(
            &settings_request(15, repository.clone(), 1),
            cleared_settings(1),
        )
        .expect("clear Repository override");
    assert_eq!(
        ModelSettingsService::new(&mut storage)
            .resolve(&product_session)
            .expect("fall back to Project")
            .provider_id,
        "provider-project"
    );
    ModelSettingsService::new(&mut storage)
        .update(
            &settings_request(16, project.clone(), 1),
            cleared_settings(1),
        )
        .expect("clear Project override");
    assert_eq!(
        ModelSettingsService::new(&mut storage)
            .resolve(&product_session)
            .expect("fall back to Organization")
            .provider_id,
        "provider-org"
    );

    ModelSettingsService::new(&mut storage)
        .update(
            &settings_request(17, organization.clone(), 1),
            cleared_settings(1),
        )
        .expect("clear Organization override");
    let cleared = ModelSettingsService::new(&mut storage)
        .resolve(&product_session)
        .expect_err("clearing every scope leaves no route");
    assert_eq!(cleared.kind(), ModelSettingsErrorKind::NoConfiguredRoute);

    let invalid = ModelSettingsService::new(&mut storage)
        .update(
            &settings_request(18, organization.clone(), 2),
            settings_values("missing-provider", "missing-model", 1, 1),
        )
        .expect_err("unknown Provider setting is rejected before commit");
    assert_eq!(invalid.kind(), ModelSettingsErrorKind::ProviderNotFound);

    let active_project_request = settings_request(19, project.clone(), 2);
    let active = ModelSettingsService::new(&mut storage)
        .update(
            &active_project_request,
            settings_values("provider-project", "model-project", 2, 1),
        )
        .expect("restore Project override");
    assert_eq!(active.revision, 3);
    ProviderCatalogService::new(&mut storage)
        .disable(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::ProjectScope(project_scope(1)),
                request_id: RequestId(id("req", 20)),
                expected_catalog_version: 1,
            },
            "provider-project",
        )
        .expect("disable effective Provider");
    let disabled = ModelSettingsService::new(&mut storage)
        .resolve(&product_session)
        .expect_err("explicit disabled child route does not fall back to Organization");
    assert_eq!(disabled.kind(), ModelSettingsErrorKind::ProviderDisabled);

    let replay = ModelSettingsService::new(&mut storage)
        .update(
            &active_project_request,
            settings_values("provider-project", "model-project", 2, 1),
        )
        .expect("exact request replays before consulting changed catalog");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, 3);
    let changed_replay = ModelSettingsService::new(&mut storage)
        .update(
            &active_project_request,
            settings_values("provider-project", "another-model", 2, 1),
        )
        .expect_err("changed requestId body conflicts before route lookup");
    assert_eq!(
        changed_replay.kind(),
        ModelSettingsErrorKind::RequestConflict
    );

    ModelSettingsService::new(&mut storage)
        .update(
            &settings_request(21, project.clone(), 3),
            cleared_settings(1),
        )
        .expect("clear disabled Project override");
    let active_organization_request = settings_request(22, organization.clone(), 2);
    ModelSettingsService::new(&mut storage)
        .update(
            &active_organization_request,
            settings_values("provider-org", "model-org", 1, 1),
        )
        .expect("restore Organization override");
    let missing_model = ModelSettingsService::new(&mut storage)
        .update(
            &settings_request(23, organization.clone(), 3),
            settings_values("provider-org", "missing-model", 1, 1),
        )
        .expect_err("known Provider with unknown model is explicit");
    assert_eq!(missing_model.kind(), ModelSettingsErrorKind::ModelNotFound);
    ProviderCatalogService::new(&mut storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope(1)),
                request_id: RequestId(id("req", 24)),
                expected_catalog_version: 1,
            },
            &descriptor("provider-org", "replacement-model", 1),
        )
        .expect("remove the selected model from current discovery");
    let disabled_model = ModelSettingsService::new(&mut storage)
        .resolve(&product_session)
        .expect_err("removed selected model is explicitly disabled");
    assert_eq!(disabled_model.kind(), ModelSettingsErrorKind::ModelDisabled);
    let organization_replay = ModelSettingsService::new(&mut storage)
        .update(
            &active_organization_request,
            settings_values("provider-org", "model-org", 1, 1),
        )
        .expect("settings replay ignores later model removal");
    assert!(organization_replay.idempotent_replay);

    let foreign = targets(99)[3].clone();
    let foreign_result = ModelSettingsService::new(&mut storage)
        .resolve(&foreign)
        .expect_err("foreign hierarchy cannot inherit settings");
    assert_eq!(
        foreign_result.kind(),
        ModelSettingsErrorKind::NoConfiguredRoute
    );

    Box::new(storage)
        .close()
        .expect("close model settings storage");
    fs::remove_dir_all(root).expect("remove priority fixture");
}

#[test]
fn legacy_route_migrates_once_discards_old_reference_and_survives_restart() {
    const SECRET: &[u8] = b"model-settings-secret-fixture-88217";
    let root = temporary_directory("legacy");
    let organization = targets(10)[0].clone();
    let mut storage = SqliteStorage::open(&root).expect("open model settings storage");
    register_provider(
        &mut storage,
        30,
        Scope::OrganizationScope(organization_scope(10)),
        "legacy-provider",
        "legacy-model",
        10,
    );
    let migration_request = settings_request(31, organization.clone(), 0);
    let old_reference = CredentialReferenceId(id("crd", 99));
    let legacy = ModelRoute {
        credential_reference_id: old_reference.clone(),
        provider_id: "legacy-provider".to_owned(),
        model_id: "legacy-model".to_owned(),
    };
    let migrated = ModelSettingsService::new(&mut storage)
        .migrate_legacy_once(&migration_request, Some(&legacy))
        .expect("migrate old ModelRoute once");
    assert_eq!(migrated.change, ModelSettingsChange::LegacyMigrated);
    assert_eq!(migrated.revision, 1);
    assert!(!migrated.idempotent_replay);

    let route = ModelSettingsService::new(&mut storage)
        .resolve(&organization)
        .expect("resolve migrated route through current catalog");
    assert_eq!(route.provider_id, "legacy-provider");
    assert_eq!(route.model_id, "legacy-model");
    assert_eq!(
        route.credential_reference_id,
        CredentialReferenceId(id("crd", 10))
    );
    assert_ne!(route.credential_reference_id, old_reference);

    let semantically_same_legacy = ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 98)),
        ..legacy.clone()
    };
    let replay = ModelSettingsService::new(&mut storage)
        .migrate_legacy_once(&migration_request, Some(&semantically_same_legacy))
        .expect("discarded legacy reference does not change migration identity");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, 1);

    let mut changed_legacy = legacy.clone();
    changed_legacy.model_id = "another-model".to_owned();
    let conflict = ModelSettingsService::new(&mut storage)
        .migrate_legacy_once(&migration_request, Some(&changed_legacy))
        .expect_err("changed legacy model conflicts on the same requestId");
    assert_eq!(conflict.kind(), ModelSettingsErrorKind::RequestConflict);
    let repeated = ModelSettingsService::new(&mut storage)
        .migrate_legacy_once(
            &settings_request(32, organization.clone(), 1),
            Some(&legacy),
        )
        .expect_err("a second migration command is rejected");
    assert_eq!(repeated.kind(), ModelSettingsErrorKind::AlreadyMigrated);

    let connection = Connection::open(storage.database_path()).expect("open fixture database");
    let settings_payload = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id LIKE 'model-settings:%'",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .expect("load canonical model settings state");
    let payload_text = String::from_utf8(settings_payload.clone()).expect("state is JSON");
    assert!(!payload_text.contains(&old_reference.0));
    assert!(!payload_text.contains("credentialReferenceId"));

    let secret = ResolvedSecret::from_bytes(SECRET.to_vec()).expect("build tracked secret");
    let mut gate = CredentialLeakGate::new();
    gate.track_secret(&secret);
    gate.inspect_json_bytes(CredentialOutputBoundary::Persistence, &settings_payload)
        .expect("settings state is secret-free");
    gate.inspect_serializable(CredentialOutputBoundary::Serialization, &route)
        .expect("resolved ModelRoute contains only a reference");

    drop(connection);
    Box::new(storage).close().expect("close before restart");
    let mut reopened = SqliteStorage::open(&root).expect("reopen model settings storage");
    let recovered = ModelSettingsService::new(&mut reopened)
        .resolve(&organization)
        .expect("resolve migrated route after restart");
    assert_eq!(recovered, route);
    let still_migrated = ModelSettingsService::new(&mut reopened)
        .migrate_legacy_once(&settings_request(33, organization, 1), Some(&legacy))
        .expect_err("migration marker survives restart");
    assert_eq!(
        still_migrated.kind(),
        ModelSettingsErrorKind::AlreadyMigrated
    );

    Box::new(reopened).close().expect("close reopened storage");
    fs::remove_dir_all(root).expect("remove legacy fixture");
}
