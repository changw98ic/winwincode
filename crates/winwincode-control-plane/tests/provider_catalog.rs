// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::Connection;
use winwincode_api::generated::{Actor, OrganizationScope, OrganizationScopeKind, Scope};
use winwincode_control_plane::{
    CatalogAvailability, CredentialLeakErrorKind, CredentialLeakGate, CredentialOutputBoundary,
    ModelCapability, ModelToolSupport, PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC,
    PROVIDER_CATALOG_VERSION_EVENT_TOPIC, ProductStateStorage, ProviderCatalogChange,
    ProviderCatalogErrorKind, ProviderCatalogRequest, ProviderCatalogService,
    ProviderCatalogVersionEvent, ProviderDescriptor, ResolvedSecret, StructuredOutputSupport,
    migrate_provider_catalogs_v1_to_v2,
};
use winwincode_domain::{CredentialReferenceId, OrganizationId, RequestId, Sha256Digest, UserId};
use winwincode_domain::{UserActor, UserActorKind};
use winwincode_storage::{
    AggregateJournalKey, CommitReceipt, LoadedAggregateJournal, OutboxEvent, ProjectionEventCursor,
    ProjectionEventStreamKey, ProjectionReadCut, ReceiptIdentity, SqliteStorage, StateCommit,
    StorageError, StoredState,
};

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
        structured_output_support: StructuredOutputSupport::Unsupported,
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
            // A structured-output capability change advances the model-local version.
            ModelCapability {
                structured_output_support: StructuredOutputSupport::JsonSchemaStrict,
                ..model(
                    "model-alpha",
                    128_000,
                    16_000,
                    ModelToolSupport::Parallel,
                    &["high", "medium"],
                )
            },
        ],
    }
}

fn legacy_catalog_payload(payload: &[u8]) -> Vec<u8> {
    let mut legacy: serde_json::Value =
        serde_json::from_slice(payload).expect("decode catalog state fixture");
    legacy["schema"] = serde_json::json!("winwincode.provider-catalog.v1");
    for provider in legacy["providers"]
        .as_object_mut()
        .expect("provider map")
        .values_mut()
    {
        for model in provider["models"]
            .as_object_mut()
            .expect("model map")
            .values_mut()
        {
            model
                .as_object_mut()
                .expect("model record")
                .remove("structuredOutputSupport");
        }
    }
    serde_json::to_vec(&legacy).expect("encode legacy state")
}

struct CatalogCasConflictStorage {
    inner: SqliteStorage,
    inject_once: bool,
    replay_once: bool,
}

impl ProductStateStorage for CatalogCasConflictStorage {
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        if self.replay_once && commit.stream_id.starts_with("provider-catalog:") {
            self.replay_once = false;
            self.inner.commit(commit)?;
            return self.inner.commit(commit);
        }
        if self.inject_once && commit.stream_id.starts_with("provider-catalog:") {
            self.inject_once = false;
            let current = self
                .inner
                .load_state(&commit.stream_id)?
                .ok_or_else(|| StorageError::adapter("missing catalog conflict fixture"))?;
            let next_revision = current
                .revision
                .checked_add(1)
                .ok_or_else(|| StorageError::adapter("catalog conflict revision overflow"))?;
            let mut concurrent: serde_json::Value = serde_json::from_slice(&current.payload)
                .map_err(|error| StorageError::adapter(error.to_string()))?;
            concurrent["catalogVersion"] = serde_json::json!(next_revision);
            let next_revision_sql = i64::try_from(next_revision)
                .map_err(|error| StorageError::adapter(error.to_string()))?;
            let current_revision_sql = i64::try_from(current.revision)
                .map_err(|error| StorageError::adapter(error.to_string()))?;
            let updated = Connection::open(self.inner.database_path())
                .and_then(|connection| {
                    connection.execute(
                        "UPDATE product_state SET revision = ?1, payload = ?2 \
                         WHERE stream_id = ?3 AND revision = ?4",
                        rusqlite::params![
                            next_revision_sql,
                            serde_json::to_vec(&concurrent).expect("encode concurrent state"),
                            commit.stream_id,
                            current_revision_sql,
                        ],
                    )
                })
                .map_err(|error| StorageError::adapter(error.to_string()))?;
            if updated != 1 {
                return Err(StorageError::adapter(
                    "catalog conflict fixture did not advance exactly one state row",
                ));
            }
        }
        self.inner.commit(commit)
    }

    fn load_receipt(
        &self,
        identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        self.inner.load_receipt(identity, command_digest)
    }

    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        self.inner.load_state(stream_id)
    }

    fn last_state_stream_id(&self, prefix: &str) -> Result<Option<String>, StorageError> {
        self.inner.last_state_stream_id(prefix)
    }

    fn scan_state_streams(
        &self,
        prefix: &str,
        after: &str,
        upper_bound: &str,
        limit: usize,
    ) -> Result<Vec<StoredState>, StorageError> {
        self.inner
            .scan_state_streams(prefix, after, upper_bound, limit)
    }

    fn load_projection_read_cut(
        &self,
        state_stream_ids: &[String],
        key: &ProjectionEventStreamKey,
        expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionReadCut, StorageError> {
        self.inner
            .load_projection_read_cut(state_stream_ids, key, expected)
    }

    fn load_journal(
        &self,
        key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
        self.inner.load_journal(key)
    }

    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        self.inner.pending_events()
    }

    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError> {
        self.inner.mark_published(event_id)
    }

    fn close(self: Box<Self>) -> Result<(), StorageError> {
        Box::new(self.inner).close()
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
        .upsert(&create_request, &initial_descriptor())
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
        .upsert(&create_request, &equivalent_replay)
        .expect("replay exact registration");
    assert_eq!(replay.catalog_version, 1);
    assert_eq!(replay.provider_version, 1);
    assert!(replay.idempotent_replay);

    let mut changed_replay = initial_descriptor();
    changed_replay.display_name = "Changed body".to_owned();
    let replay_error = ProviderCatalogService::new(&mut storage)
        .upsert(&create_request, &changed_replay)
        .expect_err("changed requestId input must conflict");
    assert_eq!(
        replay_error.kind(),
        ProviderCatalogErrorKind::RequestConflict
    );
    assert!(!replay_error.to_string().contains("Changed body"));

    let update_request = request(2, catalog_scope.clone(), 1);
    let updated = ProviderCatalogService::new(&mut storage)
        .upsert(&update_request, &updated_descriptor())
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
            ("model-alpha", 2, CatalogAvailability::Enabled),
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
    let strict_model = ProviderCatalogService::new(&mut storage)
        .resolve_model(&catalog_scope, "fixture-provider", "model-alpha")
        .expect("resolve strict structured-output model");
    assert_eq!(
        strict_model.model.structured_output_support,
        StructuredOutputSupport::JsonSchemaStrict
    );

    let disable_request = request(3, catalog_scope.clone(), 2);
    let disabled = ProviderCatalogService::new(&mut storage)
        .disable(&disable_request, "fixture-provider")
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
        .disable(&repeat_disable, "fixture-provider")
        .expect_err("a new disable command cannot create a fake version");
    assert_eq!(
        repeat_error.kind(),
        ProviderCatalogErrorKind::AlreadyDisabled
    );

    let stale = request(5, catalog_scope.clone(), 1);
    let stale_error = ProviderCatalogService::new(&mut storage)
        .upsert(&stale, &initial_descriptor())
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
        .expect("load catalog version events");
    assert_eq!(events.len(), 3);
    let decoded = events
        .iter()
        .map(|event| {
            assert_eq!(event.topic, PROVIDER_CATALOG_VERSION_EVENT_TOPIC);
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
        )
        .expect("register Provider before restart");
    ProviderCatalogService::new(&mut storage)
        .disable(&request(11, catalog_scope.clone(), 1), "fixture-provider")
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
        .expect("recover all version events after restart");
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
        .upsert(&request(21, catalog_scope, 1), &credential_shaped)
        .expect_err("credential-shaped description must fail closed");
    assert_eq!(rejected.kind(), ProviderCatalogErrorKind::CredentialLeak);
    assert!(!rejected.to_string().contains("sk-"));

    drop(connection);
    Box::new(storage)
        .close()
        .expect("close Provider catalog storage");
    fs::remove_dir_all(root).expect("remove leak-gate fixture");
}

#[test]
fn model_capability_structured_output_field_is_required_and_closed() {
    let capability = model(
        "model-exactness",
        128_000,
        16_000,
        ModelToolSupport::Parallel,
        &["high"],
    );
    let value = serde_json::to_value(&capability).expect("serialize capability");
    assert_eq!(
        value.get("structuredOutputSupport"),
        Some(&serde_json::json!("unsupported"))
    );
    assert_eq!(value.as_object().expect("capability object").len(), 7);
    assert_eq!(
        serde_json::from_value::<ModelCapability>(value.clone()).expect("round trip"),
        capability
    );

    let mut missing = value.clone();
    missing
        .as_object_mut()
        .expect("capability object")
        .remove("structuredOutputSupport");
    assert!(serde_json::from_value::<ModelCapability>(missing).is_err());

    let mut unknown = value.clone();
    unknown
        .as_object_mut()
        .expect("capability object")
        .insert("futureCapability".to_owned(), serde_json::json!(true));
    assert!(serde_json::from_value::<ModelCapability>(unknown).is_err());

    let mut illegal = value;
    illegal.as_object_mut().expect("capability object").insert(
        "structuredOutputSupport".to_owned(),
        serde_json::json!("json_schema_best_effort"),
    );
    assert!(serde_json::from_value::<ModelCapability>(illegal).is_err());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the migration test proves rejection, conversion, replay, restart, and event exactness"
)]
fn provider_catalog_v1_migrates_once_and_replays_as_v2_after_restart() {
    let root = temporary_directory("v1-to-v2-migration");
    let catalog_scope = scope(30);
    let mut storage = SqliteStorage::open(&root).expect("open Provider catalog storage");
    ProviderCatalogService::new(&mut storage)
        .upsert(
            &request(30, catalog_scope.clone(), 0),
            &initial_descriptor(),
        )
        .expect("seed current catalog before legacy fixture rewrite");

    let connection = Connection::open(storage.database_path()).expect("open migration fixture DB");
    let (stream_id, payload): (String, Vec<u8>) = connection
        .query_row(
            "SELECT stream_id, payload FROM product_state WHERE stream_id LIKE 'provider-catalog:%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load catalog state fixture");
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            rusqlite::params![legacy_catalog_payload(&payload), stream_id],
        )
        .expect("install legacy catalog state");
    drop(connection);

    assert_eq!(
        ProviderCatalogService::new(&mut storage)
            .project(&catalog_scope)
            .expect_err("normal reads accept only v2")
            .kind(),
        ProviderCatalogErrorKind::InvalidRequest
    );

    let migrated = migrate_provider_catalogs_v1_to_v2(&mut storage).expect("migrate v1 catalog");
    assert_eq!(migrated.migrated_catalogs, 1);
    assert_eq!(migrated.current_catalogs, 0);
    let projection = ProviderCatalogService::new(&mut storage)
        .project(&catalog_scope)
        .expect("project migrated catalog");
    assert_eq!(projection.catalog_version, 2);
    assert!(
        projection.providers[0].models.iter().all(|model| {
            model.structured_output_support == StructuredOutputSupport::Unsupported
        })
    );
    assert_eq!(
        storage
            .pending_events()
            .expect("load migration event")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        1
    );

    let same_process_replay =
        migrate_provider_catalogs_v1_to_v2(&mut storage).expect("repeat migration scan");
    assert_eq!(same_process_replay.migrated_catalogs, 0);
    assert_eq!(same_process_replay.current_catalogs, 1);
    Box::new(storage).close().expect("close migrated storage");

    let mut reopened = SqliteStorage::open(&root).expect("reopen migrated storage");
    let restart_replay =
        migrate_provider_catalogs_v1_to_v2(&mut reopened).expect("restart migration scan");
    assert_eq!(restart_replay.migrated_catalogs, 0);
    assert_eq!(restart_replay.current_catalogs, 1);
    assert_eq!(
        ProviderCatalogService::new(&mut reopened)
            .project(&catalog_scope)
            .expect("replay migrated projection"),
        projection
    );
    assert_eq!(
        reopened
            .pending_events()
            .expect("load replayed migration event")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        1
    );
    Box::new(reopened).close().expect("close reopened storage");
    fs::remove_dir_all(root).expect("remove migration fixture");
}

#[test]
fn provider_catalog_migration_cas_conflict_is_zero_write_and_restart_resumable() {
    let root = temporary_directory("migration-cas-conflict");
    let catalog_scope = scope(40);
    let mut storage = SqliteStorage::open(&root).expect("open Provider catalog storage");
    ProviderCatalogService::new(&mut storage)
        .upsert(
            &request(40, catalog_scope.clone(), 0),
            &initial_descriptor(),
        )
        .expect("seed catalog before conflict fixture rewrite");
    let connection = Connection::open(storage.database_path()).expect("open conflict fixture DB");
    let (stream_id, payload): (String, Vec<u8>) = connection
        .query_row(
            "SELECT stream_id, payload FROM product_state WHERE stream_id LIKE 'provider-catalog:%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load catalog state fixture");
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            rusqlite::params![legacy_catalog_payload(&payload), stream_id],
        )
        .expect("install legacy catalog state");
    drop(connection);

    let mut conflicted = CatalogCasConflictStorage {
        inner: storage,
        inject_once: true,
        replay_once: false,
    };
    assert_eq!(
        migrate_provider_catalogs_v1_to_v2(&mut conflicted)
            .expect_err("a stale migration compare-and-swap must fail")
            .kind(),
        ProviderCatalogErrorKind::VersionConflict
    );
    assert_eq!(
        conflicted
            .pending_events()
            .expect("load conflict events")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        0
    );
    assert_eq!(
        ProviderCatalogService::new(&mut conflicted)
            .project(&catalog_scope)
            .expect_err("normal reads still reject the concurrently advanced v1 state")
            .kind(),
        ProviderCatalogErrorKind::InvalidRequest
    );

    let resumed = migrate_provider_catalogs_v1_to_v2(&mut conflicted)
        .expect("restart resumes from the concurrent v1 revision");
    assert_eq!(resumed.migrated_catalogs, 1);
    assert_eq!(resumed.current_catalogs, 0);
    let projection = ProviderCatalogService::new(&mut conflicted)
        .project(&catalog_scope)
        .expect("project resumed v2 catalog");
    assert_eq!(projection.catalog_version, 3);
    assert_eq!(
        conflicted
            .pending_events()
            .expect("load resumed migration event")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        1
    );
    Box::new(conflicted)
        .close()
        .expect("close conflict fixture storage");
    fs::remove_dir_all(root).expect("remove conflict fixture");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the partial-failure test proves per-stream resume and outbox exactly-once across three startup attempts"
)]
fn provider_catalog_partial_migration_resumes_without_duplicate_outbox_events() {
    let root = temporary_directory("migration-partial-resume");
    let first_scope = scope(50);
    let second_scope = scope(51);
    let mut storage = SqliteStorage::open(&root).expect("open Provider catalog storage");
    for (seed, catalog_scope) in [(50, first_scope.clone()), (51, second_scope.clone())] {
        ProviderCatalogService::new(&mut storage)
            .upsert(&request(seed, catalog_scope, 0), &initial_descriptor())
            .expect("seed catalog before partial migration fixture rewrite");
    }

    let connection = Connection::open(storage.database_path()).expect("open partial fixture DB");
    let mut statement = connection
        .prepare(
            "SELECT stream_id, payload FROM product_state \
             WHERE stream_id LIKE 'provider-catalog:%' ORDER BY stream_id",
        )
        .expect("prepare catalog state scan");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("scan catalog state")
        .collect::<Result<Vec<_>, _>>()
        .expect("read catalog state");
    assert_eq!(rows.len(), 2);
    drop(statement);
    for (index, (stream_id, payload)) in rows.iter().enumerate() {
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&legacy_catalog_payload(payload)).expect("decode legacy state");
        if index == 1 {
            legacy
                .as_object_mut()
                .expect("legacy catalog object")
                .insert("unexpected".to_owned(), serde_json::json!(true));
        }
        connection
            .execute(
                "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
                rusqlite::params![
                    serde_json::to_vec(&legacy).expect("encode legacy state"),
                    stream_id
                ],
            )
            .expect("install partial legacy state");
    }
    drop(connection);

    assert_eq!(
        migrate_provider_catalogs_v1_to_v2(&mut storage)
            .expect_err("the malformed second stream must stop startup")
            .kind(),
        ProviderCatalogErrorKind::InvalidRequest
    );
    assert_eq!(
        storage
            .pending_events()
            .expect("load first migration event")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        1
    );

    assert_eq!(
        migrate_provider_catalogs_v1_to_v2(&mut storage)
            .expect_err("repeated startup still rejects the malformed stream")
            .kind(),
        ProviderCatalogErrorKind::InvalidRequest
    );
    assert_eq!(
        storage
            .pending_events()
            .expect("load repeated migration events")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        1,
        "the already migrated stream must not emit again"
    );

    let connection = Connection::open(storage.database_path()).expect("reopen partial fixture DB");
    let broken_payload: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = ?1",
            [&rows[1].0],
            |row| row.get(0),
        )
        .expect("load malformed legacy stream");
    let mut repaired: serde_json::Value =
        serde_json::from_slice(&broken_payload).expect("decode malformed legacy stream");
    repaired
        .as_object_mut()
        .expect("legacy catalog object")
        .remove("unexpected");
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            rusqlite::params![
                serde_json::to_vec(&repaired).expect("encode repaired state"),
                rows[1].0
            ],
        )
        .expect("repair malformed legacy stream");
    drop(connection);

    let resumed = migrate_provider_catalogs_v1_to_v2(&mut storage)
        .expect("the next startup resumes the remaining stream");
    assert_eq!(resumed.current_catalogs, 1);
    assert_eq!(resumed.migrated_catalogs, 1);
    assert_eq!(
        storage
            .pending_events()
            .expect("load completed migration events")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        2
    );
    for catalog_scope in [&first_scope, &second_scope] {
        let projection = ProviderCatalogService::new(&mut storage)
            .project(catalog_scope)
            .expect("every catalog is now v2");
        assert_eq!(projection.catalog_version, 2);
    }
    let final_replay =
        migrate_provider_catalogs_v1_to_v2(&mut storage).expect("completed migration replay");
    assert_eq!(final_replay.current_catalogs, 2);
    assert_eq!(final_replay.migrated_catalogs, 0);
    assert_eq!(
        storage
            .pending_events()
            .expect("load final replay events")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        2
    );
    Box::new(storage)
        .close()
        .expect("close partial fixture storage");
    fs::remove_dir_all(root).expect("remove partial fixture");
}

#[test]
fn provider_catalog_migration_accepts_exact_commit_replay_without_duplicate_outbox() {
    let root = temporary_directory("migration-exact-commit-replay");
    let catalog_scope = scope(60);
    let mut storage = SqliteStorage::open(&root).expect("open Provider catalog storage");
    ProviderCatalogService::new(&mut storage)
        .upsert(
            &request(60, catalog_scope.clone(), 0),
            &initial_descriptor(),
        )
        .expect("seed catalog before exact replay fixture rewrite");
    let connection = Connection::open(storage.database_path()).expect("open replay fixture DB");
    let (stream_id, payload): (String, Vec<u8>) = connection
        .query_row(
            "SELECT stream_id, payload FROM product_state WHERE stream_id LIKE 'provider-catalog:%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load replay catalog state");
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            rusqlite::params![legacy_catalog_payload(&payload), stream_id],
        )
        .expect("install replay legacy state");
    drop(connection);

    let mut replayed = CatalogCasConflictStorage {
        inner: storage,
        inject_once: false,
        replay_once: true,
    };
    let report = migrate_provider_catalogs_v1_to_v2(&mut replayed)
        .expect("an exact storage receipt replay is migration success");
    assert_eq!(report.migrated_catalogs, 1);
    assert_eq!(report.current_catalogs, 0);
    assert_eq!(
        ProviderCatalogService::new(&mut replayed)
            .project(&catalog_scope)
            .expect("project exactly replayed migration")
            .catalog_version,
        2
    );
    assert_eq!(
        replayed
            .pending_events()
            .expect("load exact replay outbox")
            .iter()
            .filter(|event| event.topic == PROVIDER_CATALOG_MIGRATION_EVENT_TOPIC)
            .count(),
        1
    );
    Box::new(replayed)
        .close()
        .expect("close exact replay fixture storage");
    fs::remove_dir_all(root).expect("remove exact replay fixture");
}
