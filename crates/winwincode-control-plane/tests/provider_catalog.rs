// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::Connection;
use winwincode_api::generated::{
    Actor, OrganizationScope, OrganizationScopeKind, Scope, UserActor, UserActorKind,
};
use winwincode_control_plane::{
    CatalogAvailability, CredentialLeakErrorKind, CredentialLeakGate, CredentialOutputBoundary,
    ModelCapability, ModelToolSupport, PROVIDER_CATALOG_VERSION_EVENT_TOPIC, ProductStateStorage,
    ProviderCatalogChange, ProviderCatalogErrorKind, ProviderCatalogRequest,
    ProviderCatalogService, ProviderCatalogVersionEvent, ProviderDescriptor, ResolvedSecret,
};
use winwincode_domain::{CredentialReferenceId, OrganizationId, RequestId, UserId};
use winwincode_storage::SqliteStorage;

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-provider-catalog-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn scope(seed: u64) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", seed)),
    })
}

fn actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn request(
    request_seed: u64,
    request_scope: Scope,
    expected_catalog_version: u64,
) -> ProviderCatalogRequest {
    ProviderCatalogRequest {
        actor: actor(1),
        scope: request_scope,
        request_id: RequestId(id("req", request_seed)),
        expected_catalog_version,
    }
}

fn model(
    model_id: &str,
    context_window_tokens: u64,
    max_output_tokens: u64,
    tool_support: ModelToolSupport,
    reasoning_efforts: &[&str],
) -> ModelCapability {
    ModelCapability {
        model_id: model_id.to_owned(),
        display_name: format!("{model_id} display"),
        context_window_tokens,
        max_output_tokens,
        tool_support,
        reasoning_efforts: reasoning_efforts
            .iter()
            .map(|effort| (*effort).to_owned())
            .collect(),
    }
}

fn initial_descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        provider_id: "fixture-provider".to_owned(),
        display_name: "Fixture Provider".to_owned(),
        adapter_kind: "openai-responses".to_owned(),
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        // Deliberately reverse input order; every durable/projection boundary is sorted.
        models: vec![
            model("model-beta", 64_000, 8_000, ModelToolSupport::Serial, &[]),
            model(
                "model-alpha",
                128_000,
                16_000,
                ModelToolSupport::Parallel,
                &["medium", "high"],
            ),
        ],
    }
}

fn updated_descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        provider_id: "fixture-provider".to_owned(),
        display_name: "Fixture Provider v2".to_owned(),
        adapter_kind: "openai-responses".to_owned(),
        credential_reference_id: CredentialReferenceId(id("crd", 2)),
        models: vec![
            model(
                "model-gamma",
                256_000,
                32_000,
                ModelToolSupport::Parallel,
                &["low", "high"],
            ),
            // Unchanged capability keeps its model-local version.
            model(
                "model-alpha",
                128_000,
                16_000,
                ModelToolSupport::Parallel,
                &["high", "medium"],
            ),
        ],
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn hot_update_disable_scope_query_and_replay_are_deterministic() {
    let root = temporary_directory("lifecycle");
    let catalog_scope = scope(1);
    let mut storage = SqliteStorage::open(&root).expect("open Provider catalog storage");

    let create_request = request(1, catalog_scope.clone(), 0);
    let created = ProviderCatalogService::new(&mut storage)
        .upsert(
            &create_request,
            &initial_descriptor(),
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("register Provider descriptor");
    assert_eq!(created.change, ProviderCatalogChange::Upserted);
    assert_eq!(created.previous_catalog_version, 0);
    assert_eq!(created.catalog_version, 1);
    assert_eq!(created.provider_version, 1);
    assert!(!created.idempotent_replay);

    let first_projection = ProviderCatalogService::new(&mut storage)
        .project(&catalog_scope)
        .expect("project first catalog version");
    assert_eq!(first_projection.catalog_version, 1);
    assert_eq!(first_projection.providers.len(), 1);
    let first_provider = &first_projection.providers[0];
    assert_eq!(first_provider.provider_id, "fixture-provider");
    assert_eq!(first_provider.version, 1);
    assert_eq!(first_provider.availability, CatalogAvailability::Enabled);
    assert_eq!(
        first_provider
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<Vec<_>>(),
        ["model-alpha", "model-beta"]
    );
    assert_eq!(
        first_provider.models[0].reasoning_efforts,
        ["high", "medium"]
    );

    let mut equivalent_replay = initial_descriptor();
    equivalent_replay.models.reverse();
    equivalent_replay.models[0].reasoning_efforts.reverse();
    let replay = ProviderCatalogService::new(&mut storage)
        .upsert(
            &create_request,
            &equivalent_replay,
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("replay exact registration");
    assert_eq!(replay.catalog_version, 1);
    assert_eq!(replay.provider_version, 1);
    assert!(replay.idempotent_replay);

    let mut changed_replay = initial_descriptor();
    changed_replay.display_name = "Changed body".to_owned();
    let replay_error = ProviderCatalogService::new(&mut storage)
        .upsert(
            &create_request,
            &changed_replay,
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect_err("changed requestId input must conflict");
    assert_eq!(
        replay_error.kind(),
        ProviderCatalogErrorKind::RequestConflict
    );
    assert!(!replay_error.to_string().contains("Changed body"));

    let update_request = request(2, catalog_scope.clone(), 1);
    let updated = ProviderCatalogService::new(&mut storage)
        .upsert(
            &update_request,
            &updated_descriptor(),
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("hot-update Provider descriptor");
    assert_eq!(updated.previous_catalog_version, 1);
    assert_eq!(updated.catalog_version, 2);
    assert_eq!(updated.provider_version, 2);

    let projection = ProviderCatalogService::new(&mut storage)
        .project(&catalog_scope)
        .expect("project updated catalog");
    let provider = &projection.providers[0];
    assert_eq!(
        provider.credential_reference_id,
        CredentialReferenceId(id("crd", 2))
    );
    assert_eq!(
        provider
            .models
            .iter()
            .map(|model| (model.model_id.as_str(), model.version, model.availability))
            .collect::<Vec<_>>(),
        [
            ("model-alpha", 1, CatalogAvailability::Enabled),
            ("model-beta", 2, CatalogAvailability::Disabled),
            ("model-gamma", 1, CatalogAvailability::Enabled),
        ]
    );

    let removed_model = ProviderCatalogService::new(&mut storage)
        .resolve_model(&catalog_scope, "fixture-provider", "model-beta")
        .expect_err("removed model remains explicitly disabled");
    assert_eq!(
        removed_model.kind(),
        ProviderCatalogErrorKind::ModelDisabled
    );
    let missing_model = ProviderCatalogService::new(&mut storage)
        .resolve_model(&catalog_scope, "fixture-provider", "model-missing")
        .expect_err("unknown model must be explicit");
    assert_eq!(
        missing_model.kind(),
        ProviderCatalogErrorKind::ModelNotFound
    );

    let resolved = ProviderCatalogService::new(&mut storage)
        .resolve_model(&catalog_scope, "fixture-provider", "model-gamma")
        .expect("resolve enabled model without secret access");
    assert_eq!(resolved.catalog_version, 2);
    assert_eq!(resolved.provider_version, 2);
    assert_eq!(
        resolved.credential_reference_id,
        CredentialReferenceId(id("crd", 2))
    );
    assert_eq!(resolved.model.model_id, "model-gamma");

    let disable_request = request(3, catalog_scope.clone(), 2);
    let disabled = ProviderCatalogService::new(&mut storage)
        .disable(
            &disable_request,
            "fixture-provider",
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("disable Provider");
    assert_eq!(disabled.change, ProviderCatalogChange::Disabled);
    assert_eq!(disabled.catalog_version, 3);
    assert_eq!(disabled.provider_version, 3);
    let disabled_provider = ProviderCatalogService::new(&mut storage)
        .resolve_model(&catalog_scope, "fixture-provider", "model-gamma")
        .expect_err("disabled Provider rejects otherwise enabled model");
    assert_eq!(
        disabled_provider.kind(),
        ProviderCatalogErrorKind::ProviderDisabled
    );

    let repeat_disable = request(4, catalog_scope.clone(), 3);
    let repeat_error = ProviderCatalogService::new(&mut storage)
        .disable(
            &repeat_disable,
            "fixture-provider",
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect_err("a new disable command cannot create a fake version");
    assert_eq!(
        repeat_error.kind(),
        ProviderCatalogErrorKind::AlreadyDisabled
    );

    let stale = request(5, catalog_scope.clone(), 1);
    let stale_error = ProviderCatalogService::new(&mut storage)
        .upsert(
            &stale,
            &initial_descriptor(),
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect_err("stale hot update must fail");
    assert_eq!(
        stale_error.kind(),
        ProviderCatalogErrorKind::VersionConflict
    );

    let foreign_scope = scope(99);
    let foreign_projection = ProviderCatalogService::new(&mut storage)
        .project(&foreign_scope)
        .expect("query exact foreign scope");
    assert_eq!(foreign_projection.catalog_version, 0);
    assert!(foreign_projection.providers.is_empty());
    let foreign_resolution = ProviderCatalogService::new(&mut storage)
        .resolve_model(&foreign_scope, "fixture-provider", "model-gamma")
        .expect_err("foreign scope cannot discover Provider existence");
    assert_eq!(
        foreign_resolution.kind(),
        ProviderCatalogErrorKind::ProviderNotFound
    );

    let events = storage
        .pending_events()
        .expect("load catalog version events")
        .into_iter()
        .filter(|event| event.topic == PROVIDER_CATALOG_VERSION_EVENT_TOPIC)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    let decoded = events
        .iter()
        .map(|event| {
            serde_json::from_slice::<ProviderCatalogVersionEvent>(&event.payload)
                .expect("decode catalog version event")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        decoded
            .iter()
            .map(|event| (event.previous_catalog_version, event.catalog_version))
            .collect::<Vec<_>>(),
        [(0, 1), (1, 2), (2, 3)]
    );
    assert_eq!(decoded[1].models[0].model_id, "model-alpha");
    assert_eq!(decoded[1].models[1].model_id, "model-beta");
    assert_eq!(decoded[1].models[2].model_id, "model-gamma");

    Box::new(storage)
        .close()
        .expect("close Provider catalog storage");
    fs::remove_dir_all(root).expect("remove Provider catalog fixture");
}

#[test]
fn catalog_state_and_versions_survive_restart() {
    let root = temporary_directory("restart");
    let catalog_scope = scope(10);
    let mut storage = SqliteStorage::open(&root).expect("open Provider catalog storage");
    ProviderCatalogService::new(&mut storage)
        .upsert(
            &request(10, catalog_scope.clone(), 0),
            &initial_descriptor(),
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("register Provider before restart");
    ProviderCatalogService::new(&mut storage)
        .disable(
            &request(11, catalog_scope.clone(), 1),
            "fixture-provider",
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("disable Provider before restart");
    Box::new(storage).close().expect("close before restart");

    let mut reopened = SqliteStorage::open(&root).expect("reopen Provider catalog storage");
    let recovered = ProviderCatalogService::new(&mut reopened)
        .project(&catalog_scope)
        .expect("recover Provider catalog projection");
    assert_eq!(recovered.catalog_version, 2);
    assert_eq!(
        recovered.providers[0].availability,
        CatalogAvailability::Disabled
    );
    assert_eq!(recovered.providers[0].version, 2);

    let reenabled = ProviderCatalogService::new(&mut reopened)
        .upsert(
            &request(12, catalog_scope.clone(), 2),
            &initial_descriptor(),
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("hot update from recovered version");
    assert_eq!(reenabled.catalog_version, 3);
    assert_eq!(reenabled.provider_version, 3);
    let resolved = ProviderCatalogService::new(&mut reopened)
        .resolve_model(&catalog_scope, "fixture-provider", "model-alpha")
        .expect("resolve re-enabled model after restart");
    assert_eq!(resolved.catalog_version, 3);

    let events = reopened
        .pending_events()
        .expect("recover all version events after restart")
        .into_iter()
        .filter(|event| event.topic == PROVIDER_CATALOG_VERSION_EVENT_TOPIC)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    Box::new(reopened).close().expect("close reopened storage");
    fs::remove_dir_all(root).expect("remove restart fixture");
}

#[test]
fn catalog_boundaries_hold_references_but_no_secret_material() {
    const SECRET: &[u8] = b"fixture-random-provider-secret-value-4419";
    let root = temporary_directory("leak-gate");
    let catalog_scope = scope(20);
    let mut storage = SqliteStorage::open(&root).expect("open Provider catalog storage");
    let created = ProviderCatalogService::new(&mut storage)
        .upsert(
            &request(20, catalog_scope.clone(), 0),
            &initial_descriptor(),
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect("register secret-free descriptor");
    let projection = ProviderCatalogService::new(&mut storage)
        .project(&catalog_scope)
        .expect("load secret-free projection");
    let resolved = ProviderCatalogService::new(&mut storage)
        .resolve_model(&catalog_scope, "fixture-provider", "model-alpha")
        .expect("resolve reference-only model");
    let events = storage.pending_events().expect("load version event");

    let connection = Connection::open(storage.database_path()).expect("open read-only fixture DB");
    let stored_payloads = connection
        .prepare("SELECT payload FROM product_state ORDER BY stream_id")
        .expect("prepare state scan")
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("scan catalog state")
        .collect::<Result<Vec<_>, _>>()
        .expect("read catalog state");

    let secret = ResolvedSecret::from_bytes(SECRET.to_vec()).expect("build tracked secret");
    let mut gate = CredentialLeakGate::new();
    gate.track_secret(&secret);
    for bytes in [
        serde_json::to_vec(&created).expect("encode mutation receipt"),
        serde_json::to_vec(&projection).expect("encode catalog projection"),
        serde_json::to_vec(&resolved).expect("encode model resolution"),
    ]
    .into_iter()
    .chain(events.into_iter().map(|event| event.payload))
    .chain(stored_payloads)
    {
        gate.inspect_json_bytes(CredentialOutputBoundary::Persistence, &bytes)
            .expect("reference-only catalog boundary passes leak gate");
        assert!(!bytes.windows(SECRET.len()).any(|window| window == SECRET));
    }
    let positive_fixture = serde_json::json!({
        "providerId": "fixture-provider",
        "message": String::from_utf8_lossy(SECRET),
    });
    let positive_error = gate
        .inspect_serializable(CredentialOutputBoundary::Event, &positive_fixture)
        .expect_err("positive secret fixture must fail the gate");
    assert_eq!(positive_error.kind(), CredentialLeakErrorKind::ExactSecret);

    let mut credential_shaped = initial_descriptor();
    credential_shaped.display_name = "sk-1234567890abcdef1234567890".to_owned();
    let rejected = ProviderCatalogService::new(&mut storage)
        .upsert(
            &request(21, catalog_scope, 1),
            &credential_shaped,
            winwincode_domain::Instant("2026-09-02T00:00:00.000Z".to_owned()),
        )
        .expect_err("credential-shaped description must fail closed");
    assert_eq!(rejected.kind(), ProviderCatalogErrorKind::CredentialLeak);
    assert!(!rejected.to_string().contains("sk-"));

    drop(connection);
    Box::new(storage)
        .close()
        .expect("close Provider catalog storage");
    fs::remove_dir_all(root).expect("remove leak-gate fixture");
}
