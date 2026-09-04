#![cfg(feature = "test-support")]

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, ErrorCode, ErrorDetailValue, Scope,
};
use winwincode_control_plane::{
    CommitError, ControlPlane, ControlPlaneConfig, DeliveryCommandCommitError, EventPublishError,
    EventPublisher, OutboxEvent, StateChange, StorageErrorKind,
    test_support::{
        DeliveryRepositoryFactsFixture, DeliverySpecFactsFixture, delivery_advance_command_facts,
        delivery_attention_command_facts, delivery_spec_command_facts,
    },
};
use winwincode_delivery::store::{
    AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryJournalPort,
    DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
};
use winwincode_delivery::{
    application::{
        attention::{AttentionDecision, ResolveAttentionInput, resolve_attention},
        stage::{
            AdvanceStageInput, NewStageIdentities, ReviewAttentionSeed, StageAdvanceResult, advance,
        },
    },
    domain::{
        AttentionItemStatus, DELIVERY_SCHEMA_VERSION, Delivery, DeliverySourceRef, DeliveryStage,
        DeliveryStatus, RepositoryKind, RepositoryRef, SessionBindingId, StageRunActorType,
        StageRunStatus,
    },
};
use winwincode_domain::{
    AttentionItemId, DeliveryId, ExecutionJobId, OrganizationId, ProductSessionId, ProjectId,
    RepositoryId, RequestId, Revision, SchemaVersion, Sha256Digest, StageRunId, UserId,
    WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-delivery-command-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
    }
}

fn create_command(seed: u64) -> CommandEnvelope {
    let scope = repository_scope(seed);
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: UserActorKind::User,
        }),
        command: CommandName::DeliveryCreate,
        expected_revision: Revision(0),
        payload: serde_json::json!({
            "deliveryId": canonical_id("dlv", seed),
            "spec": {
                "title": "Create the Rust Delivery transaction",
                "goal": "Persist one canonical Delivery without a TypeScript business writer.",
                "repositoryId": scope.repository_id,
                "baseRevision": "abcdef0",
                "acceptanceCriteria": [{
                    "id": "criterion-required",
                    "title": "The Delivery is stored atomically.",
                    "required": true
                }],
                "publicationTarget": null
            },
            "tasks": []
        }),
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope),
    }
}

fn facts(command: &CommandEnvelope, seed: u64) -> winwincode_control_plane::DeliveryCommandFacts {
    facts_with_repository(
        command,
        repository_scope(seed),
        1_800_000_000_000 + seed,
        format!("/workspaces/repository-{seed}"),
    )
}

fn facts_with_repository(
    command: &CommandEnvelope,
    repository_scope: RepositoryScope,
    now_millis: u64,
    locator: String,
) -> winwincode_control_plane::DeliveryCommandFacts {
    facts_with_authority(command, repository_scope, now_millis, locator, None)
}

fn facts_with_authority(
    command: &CommandEnvelope,
    repository_scope: RepositoryScope,
    now_millis: u64,
    locator: String,
    source_ref: Option<DeliverySourceRef>,
) -> winwincode_control_plane::DeliveryCommandFacts {
    let criterion_verification_methods = command
        .payload
        .get("spec")
        .and_then(|spec| spec.get("acceptanceCriteria"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|criterion| {
            let id = criterion
                .get("id")
                .and_then(serde_json::Value::as_str)
                .expect("criterion fixture id")
                .to_owned();
            (id.clone(), format!("Verify acceptance criterion {id}."))
        })
        .collect::<Vec<_>>();
    delivery_spec_command_facts(
        command,
        DeliverySpecFactsFixture {
            repository_scope,
            now_millis,
            repository: RepositoryRef {
                schema_version: DELIVERY_SCHEMA_VERSION,
                kind: RepositoryKind::LocalGit,
                locator,
            },
            source_ref,
            scope: vec!["Implement and verify the accepted Delivery goal.".into()],
            out_of_scope: vec!["Unreviewed product changes.".into()],
            constraints: vec!["Preserve the canonical Control Plane authority boundary.".into()],
            max_rework_attempts: 2,
            criterion_verification_methods,
        },
    )
    .expect("trusted test Delivery facts")
}

fn stage_identities(seed: u64) -> NewStageIdentities {
    NewStageIdentities {
        stage_run_id: StageRunId(canonical_id("run", seed)),
        execution_job_id: ExecutionJobId(canonical_id("job", seed)),
        session_binding_id: SessionBindingId::new(format!("binding-{seed}"))
            .expect("session binding fixture id"),
        attention_item_id: AttentionItemId(canonical_id("att", seed)),
    }
}

fn repository_authority(
    seed: u64,
    locator: &str,
    source_ref: Option<DeliverySourceRef>,
) -> DeliveryRepositoryFactsFixture {
    DeliveryRepositoryFactsFixture {
        repository_scope: repository_scope(seed),
        repository: RepositoryRef {
            schema_version: DELIVERY_SCHEMA_VERSION,
            kind: RepositoryKind::LocalGit,
            locator: locator.into(),
        },
        source_ref,
    }
}

fn human_review_transition(
    delivery: &Delivery,
    actor: &str,
    now_millis: u64,
    seed: u64,
) -> StageAdvanceResult {
    advance(
        delivery,
        AdvanceStageInput {
            expected_revision: delivery.revision(),
            product_session_id: ProductSessionId(canonical_id("psn", seed)),
            identities: stage_identities(seed),
            review: Some(ReviewAttentionSeed {
                title: "Review the verified Delivery".into(),
                context: "Approve or return the verified Delivery.".into(),
                assigned_to: actor.into(),
            }),
            previous_outcome: None,
            current_lease: None,
            rework_authorization: None,
            now_millis,
        },
    )
    .expect("sealed human review transition")
}

fn command_digest(command: &CommandEnvelope) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(command).expect("canonical command JSON"))
    )
}

fn delivery_command(
    seed: u64,
    command: CommandName,
    expected_revision: i64,
    payload: serde_json::Value,
) -> CommandEnvelope {
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: UserActorKind::User,
        }),
        command,
        expected_revision: Revision(expected_revision),
        payload,
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(repository_scope(seed)),
    }
}

#[derive(Default)]
struct CapturingJournal {
    publication: Mutex<Option<AtomicPublication>>,
}

impl DeliveryJournalPort for CapturingJournal {
    fn load(
        &self,
        _delivery_id: &DeliveryId,
    ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
        Ok(None)
    }

    fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
        *self.publication.lock().expect("publication lock") = Some(publication);
        Ok(())
    }
}

fn ready_to_deliver_fixture() -> Delivery {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical Delivery fixture")
    .into_snapshot();
    snapshot.revision = 1;
    Delivery::try_from_snapshot(snapshot).expect("ReadyToDeliver seed")
}

fn seed_ready_to_deliver(root: &PathBuf) -> Delivery {
    let delivery = ready_to_deliver_fixture();
    let journal = CapturingJournal::default();
    DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("seed-delivery-command-journal".into()),
            request_digest: "a".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery journal");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = journal
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("seed publication")
    else {
        panic!("seed must create one journal");
    };
    let publication = AggregateJournalPublication::Create {
        key: AggregateJournalKey::new("delivery", delivery_id.0).expect("journal key"),
        manifest,
        first_record: AggregateJournalRecord::new(
            first_record.sequence,
            first_record.digest,
            first_record.bytes,
        ),
    };
    let mut storage = SqliteStorage::open(root).expect("seed storage");
    let receipt = storage
        .commit(
            &StateCommit::new(
                ReceiptIdentity::new(
                    ReceiptActorKey::from_encoded(b"seed-actor".to_vec()).expect("seed actor"),
                    ReceiptScopeKey::from_encoded(b"seed-scope".to_vec()).expect("seed scope"),
                    RequestId("seed-delivery-command-state".into()),
                )
                .expect("seed identity"),
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("seed state"),
                vec![NewOutboxEvent::internal(
                    "seed-delivery-command-event",
                    "delivery.seeded",
                    b"seed".to_vec(),
                )],
            )
            .with_journal_publication(publication),
        )
        .expect("seed transaction");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("seed publication ack");
    Box::new(storage).close().expect("seed storage close");
    delivery
}

fn update_spec_command(seed: u64) -> CommandEnvelope {
    let scope = repository_scope(seed);
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: UserActorKind::User,
        }),
        command: CommandName::DeliveryUpdateSpec,
        expected_revision: Revision(1),
        payload: serde_json::json!({
            "deliveryId": canonical_id("dlv", seed),
            "spec": {
                "title": "Updated Rust Delivery transaction",
                "goal": "Replace the whole canonical specification before planning.",
                "repositoryId": scope.repository_id,
                "baseRevision": "abcdef1",
                "acceptanceCriteria": [{
                    "id": "criterion-updated",
                    "title": "The replacement specification is authoritative.",
                    "required": true
                }],
                "publicationTarget": null
            }
        }),
        request_id: RequestId(canonical_id("req", seed + 1_000)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope),
    }
}

struct CapturingPublisher {
    events: Arc<Mutex<Vec<OutboxEvent>>>,
}

struct FailingPublisher;

impl EventPublisher for FailingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Err(EventPublishError::new("injected publication failure"))
    }
}

impl EventPublisher for CapturingPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.events.lock().expect("event lock").push(event.clone());
        Ok(())
    }
}

#[test]
fn create_commits_the_canonical_empty_delivery_and_public_event() {
    let root = temporary_directory("create");
    let published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("Control Plane start");
    let command = create_command(1);

    let receipt = control_plane
        .commit_delivery_command(&command, &facts(&command, 1))
        .expect("canonical Delivery create");
    let delivery_id = DeliveryId(canonical_id("dlv", 1));
    let state = control_plane
        .load_state(&format!("delivery:{}", delivery_id.0))
        .expect("state read")
        .expect("Delivery state");
    let delivery = Delivery::decode_json(&state.payload).expect("canonical Delivery JSON");

    assert_eq!(receipt.revision, 1);
    assert_eq!(receipt.events.len(), 1);
    assert_eq!(receipt.events[0].topic, "delivery.changed.v1");
    assert_eq!(delivery.id(), &delivery_id);
    assert_eq!(delivery.revision(), 1);
    assert_eq!(delivery.snapshot().status, DeliveryStatus::Draft);
    assert!(delivery.snapshot().tasks.is_empty());
    assert!(delivery.snapshot().stage_runs.is_empty());
    assert!(delivery.snapshot().session_bindings.is_empty());
    assert!(delivery.snapshot().attention_items.is_empty());
    assert!(delivery.snapshot().evidence.is_empty());
    assert!(delivery.snapshot().verdict.is_none());
    assert_eq!(delivery.snapshot().spec.revision, 1);
    assert_eq!(
        delivery.snapshot().spec.repository.kind,
        RepositoryKind::LocalGit
    );
    assert_eq!(
        delivery.snapshot().spec.repository.locator,
        "/workspaces/repository-1"
    );
    assert_eq!(published.lock().expect("published events").len(), 1);

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn concurrent_exact_create_returns_one_commit_and_only_durable_replays() {
    const CONTENDER_COUNT: usize = 8;

    let root = temporary_directory("concurrent-create-replay");
    let command = create_command(80);
    let trusted_facts = facts(&command, 80);
    let published = Arc::new(Mutex::new(Vec::new()));
    let planes = (0..CONTENDER_COUNT)
        .map(|_| {
            ControlPlane::start_local(
                ControlPlaneConfig::local(&root),
                Box::new(CapturingPublisher {
                    events: Arc::clone(&published),
                }),
            )
            .expect("Control Plane start")
        })
        .collect::<Vec<_>>();
    let start = Arc::new(Barrier::new(CONTENDER_COUNT + 1));
    let handles = planes
        .into_iter()
        .map(|mut control_plane| {
            let command = command.clone();
            let trusted_facts = trusted_facts.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                let result = control_plane.commit_delivery_command(&command, &trusted_facts);
                control_plane.shutdown().expect("shutdown");
                result
            })
        })
        .collect::<Vec<_>>();
    start.wait();
    let receipts = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("create thread")
                .expect("exact concurrent create")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| !receipt.idempotent_replay)
            .count(),
        1
    );
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| receipt.idempotent_replay)
            .count(),
        CONTENDER_COUNT - 1
    );
    for receipt in &receipts[1..] {
        assert_eq!(receipt.receipt_identity, receipts[0].receipt_identity);
        assert_eq!(receipt.stream_id, receipts[0].stream_id);
        assert_eq!(receipt.revision, receipts[0].revision);
        assert_eq!(receipt.events, receipts[0].events);
    }

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let counts: (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_id = ?1), \
               (SELECT COUNT(*) FROM product_state WHERE stream_id = ?2), \
               (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?3), \
               (SELECT COUNT(*) FROM outbox WHERE request_id = ?3)",
            (
                canonical_id("dlv", 80),
                format!("delivery:{}", canonical_id("dlv", 80)),
                command.request_id.0,
            ),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("atomic member counts");
    assert_eq!(counts, (1, 1, 1, 1));
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn update_spec_replaces_only_the_canonical_spec_and_replays_the_exact_receipt() {
    let root = temporary_directory("update-spec");
    let published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("Control Plane start");
    control_plane
        .commit_delivery_command(&create_command(2), &facts(&create_command(2), 2))
        .expect("Delivery create");
    let command = update_spec_command(2);

    let first = control_plane
        .commit_delivery_command(&command, &facts(&command, 2))
        .expect("canonical Spec replacement");
    let unrelated_replay_command = create_command(9_999);
    let replay = control_plane
        .commit_delivery_command(&command, &facts(&unrelated_replay_command, 9_999))
        .expect("exact scoped replay");
    let state = control_plane
        .load_state(&format!("delivery:{}", canonical_id("dlv", 2)))
        .expect("state read")
        .expect("Delivery state");
    let delivery = Delivery::decode_json(&state.payload).expect("canonical Delivery JSON");

    assert!(!first.idempotent_replay);
    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.events, first.events);
    assert_eq!(delivery.revision(), 2);
    assert_eq!(delivery.snapshot().status, DeliveryStatus::Ready);
    assert_eq!(delivery.snapshot().spec.revision, 2);
    assert_eq!(
        delivery.snapshot().spec.title,
        "Updated Rust Delivery transaction"
    );
    assert_eq!(delivery.snapshot().spec.base_revision, "abcdef1");
    assert!(delivery.snapshot().tasks.is_empty());
    assert!(delivery.snapshot().stage_runs.is_empty());
    assert!(delivery.snapshot().session_bindings.is_empty());
    assert!(delivery.snapshot().attention_items.is_empty());
    assert!(delivery.snapshot().evidence.is_empty());
    assert!(delivery.snapshot().verdict.is_none());
    assert_eq!(published.lock().expect("published events").len(), 2);

    let mut outdated_command = update_spec_command(2);
    outdated_command.request_id = RequestId(canonical_id("req", 3_002));
    outdated_command.expected_revision = Revision(1);
    let error = control_plane
        .commit_delivery_command(&outdated_command, &facts(&outdated_command, 2))
        .expect_err("stale Spec replacement must fail");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::RevisionConflict
    ));

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn missing_delivery_is_a_public_resource_not_found_for_every_base_mutation() {
    let root = temporary_directory("missing-delivery");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");

    let update = update_spec_command(81);
    let update_error = control_plane
        .commit_delivery_command(&update, &facts(&update, 81))
        .expect_err("Spec replacement requires an existing Delivery");

    let source = ready_to_deliver_fixture();
    let advance = delivery_command(
        82,
        CommandName::DeliveryAdvance,
        1,
        serde_json::json!({"deliveryId": canonical_id("dlv", 82)}),
    );
    let advance_facts = delivery_advance_command_facts(
        &advance,
        repository_authority(82, "/workspace/repository", None),
        human_review_transition(
            &source,
            &canonical_id("usr", 82),
            source.snapshot().updated_at_millis + 1,
            82,
        ),
    )
    .expect("sealed advance facts");
    let advance_error = control_plane
        .commit_delivery_command(&advance, &advance_facts)
        .expect_err("human advance requires an existing Delivery");

    let reviewing_transition = human_review_transition(
        &source,
        &canonical_id("usr", 83),
        source.snapshot().updated_at_millis + 1,
        83,
    );
    let reviewing = reviewing_transition.delivery;
    let attention = reviewing
        .snapshot()
        .attention_items
        .last()
        .expect("review Attention");
    let resolve = delivery_command(
        83,
        CommandName::DeliveryResolveAttention,
        2,
        serde_json::json!({
            "deliveryId": canonical_id("dlv", 83),
            "attentionItemId": attention.id,
            "decision": "resolve",
            "resolution": "Resolve a missing Delivery only through its public error.",
            "remediation": null
        }),
    );
    let resolve_transition = resolve_attention(
        &reviewing,
        ResolveAttentionInput {
            expected_revision: reviewing.revision(),
            attention_item_id: attention.id.clone(),
            stage_run_id: attention.stage_run_id.clone().expect("review StageRun"),
            expected_context: attention.context.clone(),
            actor: canonical_id("usr", 83),
            decision: AttentionDecision::Resolved,
            resolution: "Resolve a missing Delivery only through its public error.".into(),
            now_millis: source.snapshot().updated_at_millis + 2,
        },
    )
    .expect("sealed Attention transition");
    let resolve_facts = delivery_attention_command_facts(
        &resolve,
        repository_authority(83, "/workspace/repository", None),
        resolve_transition,
    )
    .expect("sealed Attention facts");
    let resolve_error = control_plane
        .commit_delivery_command(&resolve, &resolve_facts)
        .expect_err("Attention resolution requires an existing Delivery");

    for (error, delivery_id) in [
        (update_error, canonical_id("dlv", 81)),
        (advance_error, canonical_id("dlv", 82)),
        (resolve_error, canonical_id("dlv", 83)),
    ] {
        assert_eq!(error.public_code(), ErrorCode::ResourceNotFound);
        assert!(!error.retryable());
        assert_eq!(
            error.public_details().get("field"),
            Some(&ErrorDetailValue::Variant4("deliveryId".into()))
        );
        assert_eq!(
            error.public_details().get("deliveryId"),
            Some(&ErrorDetailValue::Variant4(delivery_id))
        );
        assert!(error.committed_receipt().is_none());
    }

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
#[allow(clippy::too_many_lines)]
fn human_advance_and_attention_resolution_use_sealed_application_transitions() {
    let root = temporary_directory("human-review");
    let source = seed_ready_to_deliver(&root);
    let delivery_id = source.id().0.clone();
    let published = Arc::new(Mutex::new(Vec::new()));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("Control Plane start");
    let advance_command = delivery_command(
        30,
        CommandName::DeliveryAdvance,
        1,
        serde_json::json!({"deliveryId": delivery_id}),
    );
    let advance_transition = human_review_transition(
        &source,
        &canonical_id("usr", 30),
        source.snapshot().updated_at_millis + 1,
        30,
    );
    let advance_facts = delivery_advance_command_facts(
        &advance_command,
        repository_authority(30, "/workspace/repository", None),
        advance_transition,
    )
    .expect("sealed advance facts");

    let advance_receipt = control_plane
        .commit_delivery_command(&advance_command, &advance_facts)
        .expect("human Delivery review advance");
    let state = control_plane
        .load_state(&format!("delivery:{}", source.id().0))
        .expect("review state")
        .expect("review Delivery");
    let reviewing = Delivery::decode_json(&state.payload).expect("reviewing Delivery");
    let run = reviewing.snapshot().stage_runs.last().expect("review run");
    let attention = reviewing
        .snapshot()
        .attention_items
        .last()
        .expect("review Attention");

    assert_eq!(advance_receipt.events.len(), 1);
    assert_eq!(reviewing.revision(), 2);
    assert_eq!(reviewing.snapshot().status, DeliveryStatus::NeedsAttention);
    assert_eq!(run.stage, DeliveryStage::DeliveryReview);
    assert_eq!(run.actor_type, StageRunActorType::Human);
    assert_eq!(run.status, StageRunStatus::Waiting);
    assert_eq!(attention.status, AttentionItemStatus::Open);
    assert_eq!(
        attention.assigned_to.as_deref(),
        Some(canonical_id("usr", 30).as_str())
    );
    assert_eq!(
        reviewing.snapshot().session_bindings.len(),
        source.snapshot().session_bindings.len()
    );

    let mut resolve_command = delivery_command(
        30,
        CommandName::DeliveryResolveAttention,
        2,
        serde_json::json!({
            "deliveryId": source.id().0,
            "attentionItemId": attention.id,
            "decision": "resolve",
            "resolution": "Approve the verified Delivery for final handoff.",
            "remediation": null
        }),
    );
    resolve_command.request_id = RequestId(canonical_id("req", 31));
    let resolution = resolve_command
        .payload
        .get("resolution")
        .and_then(serde_json::Value::as_str)
        .expect("resolution")
        .to_owned();
    let resolve_transition = resolve_attention(
        &reviewing,
        ResolveAttentionInput {
            expected_revision: reviewing.revision(),
            attention_item_id: attention.id.clone(),
            stage_run_id: attention.stage_run_id.clone().expect("review StageRun"),
            expected_context: attention.context.clone(),
            actor: canonical_id("usr", 30),
            decision: AttentionDecision::Resolved,
            resolution,
            now_millis: source.snapshot().updated_at_millis + 2,
        },
    )
    .expect("sealed Attention transition");
    let resolve_facts = delivery_attention_command_facts(
        &resolve_command,
        repository_authority(30, "/workspace/repository", None),
        resolve_transition,
    )
    .expect("sealed Attention facts");
    let resolve_receipt = control_plane
        .commit_delivery_command(&resolve_command, &resolve_facts)
        .expect("resolve Delivery review Attention");
    let state = control_plane
        .load_state(&format!("delivery:{}", source.id().0))
        .expect("delivered state")
        .expect("delivered Delivery");
    let delivered = Delivery::decode_json(&state.payload).expect("delivered Delivery");

    assert_eq!(resolve_receipt.events.len(), 1);
    assert_eq!(delivered.revision(), 3);
    assert_eq!(delivered.snapshot().status, DeliveryStatus::Delivered);
    assert_eq!(
        delivered
            .snapshot()
            .attention_items
            .last()
            .expect("Attention")
            .resolved_by
            .as_deref(),
        Some(canonical_id("usr", 30).as_str())
    );
    assert_eq!(published.lock().expect("published events").len(), 2);

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
#[allow(clippy::too_many_lines)]
fn foreign_repository_scope_cannot_reuse_sealed_advance_or_attention_authority() {
    let root = temporary_directory("foreign-stage-scope");
    let source = seed_ready_to_deliver(&root);
    let delivery_id = source.id().clone();
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    let advance_command = delivery_command(
        30,
        CommandName::DeliveryAdvance,
        1,
        serde_json::json!({"deliveryId": delivery_id}),
    );
    let transition = human_review_transition(
        &source,
        &canonical_id("usr", 30),
        source.snapshot().updated_at_millis + 1,
        35,
    );
    let advance_facts = delivery_advance_command_facts(
        &advance_command,
        repository_authority(30, "/workspace/repository", None),
        transition,
    )
    .expect("sealed advance facts");
    let mut foreign_advance = advance_command.clone();
    foreign_advance.scope = Scope::RepositoryScope(repository_scope(31));
    foreign_advance.request_id = RequestId(canonical_id("req", 3_100));

    let error = control_plane
        .commit_delivery_command(&foreign_advance, &advance_facts)
        .expect_err("foreign scope cannot reuse stage authority");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::InvalidInput
    ));
    let state = control_plane
        .load_state(&format!("delivery:{}", source.id().0))
        .expect("state read")
        .expect("seed state");
    assert_eq!(state.revision, 1);

    control_plane
        .commit_delivery_command(&advance_command, &advance_facts)
        .expect("canonical human advance");
    let state = control_plane
        .load_state(&format!("delivery:{}", source.id().0))
        .expect("review state")
        .expect("review Delivery");
    let reviewing = Delivery::decode_json(&state.payload).expect("reviewing Delivery");
    let attention = reviewing
        .snapshot()
        .attention_items
        .last()
        .expect("review Attention");
    let resolve_command = delivery_command(
        30,
        CommandName::DeliveryResolveAttention,
        2,
        serde_json::json!({
            "deliveryId": source.id().0,
            "attentionItemId": attention.id,
            "decision": "resolve",
            "resolution": "Approve this exact reviewed Delivery.",
            "remediation": null
        }),
    );
    let transition = resolve_attention(
        &reviewing,
        ResolveAttentionInput {
            expected_revision: 2,
            attention_item_id: attention.id.clone(),
            stage_run_id: attention.stage_run_id.clone().expect("StageRun"),
            expected_context: attention.context.clone(),
            actor: canonical_id("usr", 30),
            decision: AttentionDecision::Resolved,
            resolution: "Approve this exact reviewed Delivery.".into(),
            now_millis: source.snapshot().updated_at_millis + 2,
        },
    )
    .expect("sealed resolution");
    let resolve_facts = delivery_attention_command_facts(
        &resolve_command,
        repository_authority(30, "/workspace/repository", None),
        transition,
    )
    .expect("sealed Attention facts");
    let mut foreign_resolve = resolve_command.clone();
    foreign_resolve.scope = Scope::RepositoryScope(repository_scope(31));
    foreign_resolve.request_id = RequestId(canonical_id("req", 3_101));

    let error = control_plane
        .commit_delivery_command(&foreign_resolve, &resolve_facts)
        .expect_err("foreign scope cannot reuse Attention authority");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::InvalidInput
    ));
    let state = control_plane
        .load_state(&format!("delivery:{}", source.id().0))
        .expect("state read")
        .expect("review state");
    assert_eq!(state.revision, 2);
    control_plane.shutdown().expect("shutdown");

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let counts: (i64, i64, i64) = connection
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_id = ?1), \
               (SELECT COUNT(*) FROM command_receipts WHERE request_id IN (?2, ?3)), \
               (SELECT COUNT(*) FROM outbox WHERE request_id IN (?2, ?3))",
            (
                source.id().0.as_str(),
                foreign_advance.request_id.0.as_str(),
                foreign_resolve.request_id.0.as_str(),
            ),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("foreign scope counts");
    assert_eq!(counts, (2, 0, 0));
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn create_rejects_caller_tasks_and_accepts_a_target_without_an_issue_source() {
    let rejected_root = temporary_directory("caller-tasks");
    let mut rejected_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&rejected_root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    let mut caller_tasks = create_command(40);
    caller_tasks.payload["tasks"] = serde_json::json!([{
        "id": canonical_id("tsk", 40),
        "title": "Caller-owned task",
        "goal": "Bypass task-breakdown review",
        "acceptanceCriterionIds": ["criterion-required"],
        "blockedByTaskIds": [],
        "ownerActorId": null
    }]);

    let error = rejected_plane
        .commit_delivery_command(&caller_tasks, &facts(&caller_tasks, 40))
        .expect_err("create tasks must stay behind task-breakdown approval");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::InvalidInput
    ));
    assert!(
        rejected_plane
            .load_state(&format!("delivery:{}", canonical_id("dlv", 40)))
            .expect("state read")
            .is_none()
    );
    rejected_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(rejected_root).expect("database cleanup");

    let target_root = temporary_directory("target-without-source");
    let mut target_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&target_root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    let mut targeted = create_command(41);
    targeted.payload["spec"]["publicationTarget"] = serde_json::json!({
        "provider": "github",
        "repository": "example/repository",
        "baseBranch": "main",
        "headRepository": "example/repository",
        "headBranch": "winwincode/delivery-41"
    });

    target_plane
        .commit_delivery_command(&targeted, &facts(&targeted, 41))
        .expect("publication target does not imply an issue source");
    let state = target_plane
        .load_state(&format!("delivery:{}", canonical_id("dlv", 41)))
        .expect("state read")
        .expect("Delivery state");
    let delivery = Delivery::decode_json(&state.payload).expect("canonical Delivery");
    assert!(delivery.snapshot().spec.source_ref.is_none());
    assert_eq!(
        delivery
            .snapshot()
            .spec
            .publication_target
            .as_ref()
            .expect("publication target")
            .head_branch,
        "winwincode/delivery-41"
    );
    assert!(
        delivery
            .snapshot()
            .spec
            .acceptance_criteria
            .iter()
            .all(|criterion| {
                criterion
                    .verification_method
                    .as_deref()
                    .is_some_and(|method| !method.is_empty())
            })
    );
    target_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(target_root).expect("database cleanup");
}

#[test]
#[allow(clippy::too_many_lines)]
fn spec_authority_requires_exact_ordered_verification_methods_and_preserves_all_semantics() {
    let root = temporary_directory("spec-authority");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    let mut command = create_command(45);
    command.payload["spec"]["acceptanceCriteria"] = serde_json::json!([
        {
            "id": "criterion-first",
            "title": "The first behavior is verified.",
            "required": true
        },
        {
            "id": "criterion-second",
            "title": "The second behavior is verified.",
            "required": true
        }
    ]);
    let fixture = |methods: Vec<(String, String)>| DeliverySpecFactsFixture {
        repository_scope: repository_scope(45),
        now_millis: 1_800_000_000_045,
        repository: RepositoryRef {
            schema_version: DELIVERY_SCHEMA_VERSION,
            kind: RepositoryKind::LocalGit,
            locator: "/trusted/repository-45".into(),
        },
        source_ref: None,
        scope: vec!["Implement both accepted behaviors.".into()],
        out_of_scope: vec!["A third unreviewed behavior.".into()],
        constraints: vec!["Keep both checks deterministic.".into()],
        max_rework_attempts: 3,
        criterion_verification_methods: methods,
    };
    let first = (
        "criterion-first".to_owned(),
        "Run the first acceptance test.".to_owned(),
    );
    let second = (
        "criterion-second".to_owned(),
        "Run the second acceptance test.".to_owned(),
    );
    for (case, methods) in [
        ("missing", vec![first.clone()]),
        ("reordered", vec![second.clone(), first.clone()]),
        ("duplicate", vec![first.clone(), first.clone()]),
        (
            "foreign",
            vec![
                first.clone(),
                ("criterion-foreign".into(), "Run a foreign test.".into()),
            ],
        ),
    ] {
        let sealed = delivery_spec_command_facts(&command, fixture(methods))
            .expect("repository authority is exact");
        let error = control_plane
            .commit_delivery_command(&command, &sealed)
            .expect_err("criterion authority must match exactly");
        assert!(
            matches!(
                error,
                DeliveryCommandCommitError::Storage(ref source)
                    if source.kind() == StorageErrorKind::InvalidInput
            ),
            "{case}"
        );
        assert!(
            control_plane
                .load_state(&format!("delivery:{}", canonical_id("dlv", 45)))
                .expect("state read")
                .is_none(),
            "{case}"
        );
    }

    let sealed = delivery_spec_command_facts(&command, fixture(vec![first, second]))
        .expect("complete Spec authority");
    control_plane
        .commit_delivery_command(&command, &sealed)
        .expect("complete Spec authority commits");
    let state = control_plane
        .load_state(&format!("delivery:{}", canonical_id("dlv", 45)))
        .expect("state read")
        .expect("Delivery state");
    let delivery = Delivery::decode_json(&state.payload).expect("Delivery JSON");
    assert_eq!(
        delivery.snapshot().spec.scope,
        ["Implement both accepted behaviors."]
    );
    assert_eq!(
        delivery.snapshot().spec.out_of_scope,
        ["A third unreviewed behavior."]
    );
    assert_eq!(
        delivery.snapshot().spec.constraints,
        ["Keep both checks deterministic."]
    );
    assert_eq!(delivery.snapshot().spec.max_rework_attempts, 3);
    assert_eq!(
        delivery.snapshot().spec.acceptance_criteria[0]
            .verification_method
            .as_deref(),
        Some("Run the first acceptance test.")
    );
    assert_eq!(
        delivery.snapshot().spec.acceptance_criteria[1]
            .verification_method
            .as_deref(),
        Some("Run the second acceptance test.")
    );
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn duplicate_create_is_wrong_state_and_does_not_write_another_atomic_member() {
    let root = temporary_directory("duplicate-create");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    let first = create_command(50);
    control_plane
        .commit_delivery_command(&first, &facts(&first, 50))
        .expect("first create");
    let mut duplicate = first.clone();
    duplicate.request_id = RequestId(canonical_id("req", 5_050));

    let error = control_plane
        .commit_delivery_command(
            &duplicate,
            &facts_with_repository(
                &duplicate,
                repository_scope(50),
                1_800_000_000_051,
                "/workspaces/repository-50".into(),
            ),
        )
        .expect_err("a different request cannot create an existing Delivery");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::AlreadyExists { ref delivery_id }
            if delivery_id.0 == canonical_id("dlv", 50)
    ));
    assert_eq!(error.public_code(), ErrorCode::WrongState);
    assert!(!error.retryable());
    let details = error.public_details();
    assert_eq!(
        details.get("field"),
        Some(&ErrorDetailValue::Variant4("deliveryId".into()))
    );
    assert_eq!(
        details.get("deliveryId"),
        Some(&ErrorDetailValue::Variant4(canonical_id("dlv", 50)))
    );
    let state = control_plane
        .load_state(&format!("delivery:{}", canonical_id("dlv", 50)))
        .expect("state read")
        .expect("Delivery state");
    assert_eq!(state.revision, 1);
    control_plane.shutdown().expect("shutdown");

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let counts: (i64, i64, i64) = connection
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_id = ?1), \
               (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?2), \
               (SELECT COUNT(*) FROM outbox WHERE request_id = ?2)",
            [canonical_id("dlv", 50), duplicate.request_id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("duplicate counts");
    assert_eq!(counts, (1, 0, 0));
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn generic_commit_cannot_bypass_the_delivery_command_transaction() {
    let root = temporary_directory("generic-bypass");
    let command = create_command(51);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");

    let error = control_plane
        .commit(
            &command,
            StateChange::new(
                format!("delivery:{}", canonical_id("dlv", 51)),
                b"caller-authored-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    "caller-authored-delivery-event",
                    "delivery.changed.v1",
                    b"caller-authored-event".to_vec(),
                )],
            ),
        )
        .expect_err("generic Delivery commit must fail closed");
    assert!(matches!(error, CommitError::Storage(_)));
    assert!(
        control_plane
            .load_state(&format!("delivery:{}", canonical_id("dlv", 51)))
            .expect("state read")
            .is_none()
    );
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn replay_uses_only_the_durable_receipt_even_when_payload_state_journal_and_facts_are_unreadable() {
    let root = temporary_directory("receipt-first");
    let original = create_command(60);
    let mut first_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    let first = first_plane
        .commit_delivery_command(&original, &facts(&original, 60))
        .expect("initial create");
    first_plane.shutdown().expect("shutdown");

    // Model a command written by a future payload parser while keeping the
    // exact original durable result. Receipt replay must not deserialize that
    // payload or consult mutable aggregate/state projections.
    let mut future_command = original.clone();
    future_command.payload["futureSchemaField"] = serde_json::json!({"version": 2});
    let future_digest = command_digest(&future_command);
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("corruption database");
    connection
        .execute(
            "UPDATE command_receipts SET command_digest = ?1 WHERE request_id = ?2",
            (&future_digest, &future_command.request_id.0),
        )
        .expect("future command digest");
    connection
        .execute(
            "UPDATE product_state SET payload = x'00' WHERE stream_id = ?1",
            [format!("delivery:{}", canonical_id("dlv", 60))],
        )
        .expect("corrupt mutable state");
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = x'00' \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [canonical_id("dlv", 60)],
        )
        .expect("corrupt aggregate journal");
    connection.close().expect("corruption close");

    let published = Arc::new(Mutex::new(Vec::new()));
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("restart");
    let unrelated = create_command(61);
    let replay = restarted
        .commit_delivery_command(&future_command, &facts(&unrelated, 61))
        .expect("receipt-first replay");

    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.events, first.events);
    assert!(published.lock().expect("published events").is_empty());
    restarted.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
#[allow(clippy::too_many_lines)]
fn command_digest_scope_revision_and_trusted_spec_facts_fail_closed_independently() {
    let root = temporary_directory("authority-conflicts");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    let create = create_command(70);
    control_plane
        .commit_delivery_command(&create, &facts(&create, 70))
        .expect("create");
    let update = update_spec_command(70);

    let another = update_spec_command(71);
    let mismatched_facts = facts(&another, 71);
    let error = control_plane
        .commit_delivery_command(&update, &mismatched_facts)
        .expect_err("facts are sealed to the exact command and repository scope");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::InvalidInput
    ));

    let foreign_repository = facts_with_repository(
        &update,
        repository_scope(70),
        1_800_000_000_071,
        "/workspaces/foreign-repository".into(),
    );
    let error = control_plane
        .commit_delivery_command(&update, &foreign_repository)
        .expect_err("trusted repository must equal the Delivery repository");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::InvalidInput
    ));

    let foreign_source = DeliverySourceRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        provider: "github".into(),
        kind: "issue".into(),
        repository: "example/foreign".into(),
        number: 7,
    };
    let source_facts = facts_with_authority(
        &update,
        repository_scope(70),
        1_800_000_000_071,
        "/workspaces/repository-70".into(),
        Some(foreign_source),
    );
    let error = control_plane
        .commit_delivery_command(&update, &source_facts)
        .expect_err("trusted source must equal the Delivery source");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::InvalidInput
    ));

    let backwards_time = facts_with_repository(
        &update,
        repository_scope(70),
        1,
        "/workspaces/repository-70".into(),
    );
    let error = control_plane
        .commit_delivery_command(&update, &backwards_time)
        .expect_err("trusted mutation time cannot move backwards");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::InvalidInput
    ));

    let mut outdated_command = update.clone();
    outdated_command.expected_revision = Revision(0);
    outdated_command.request_id = RequestId(canonical_id("req", 7_070));
    let error = control_plane
        .commit_delivery_command(
            &outdated_command,
            &facts_with_repository(
                &outdated_command,
                repository_scope(70),
                1_800_000_000_072,
                "/workspaces/repository-70".into(),
            ),
        )
        .expect_err("stale revision");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::RevisionConflict
    ));

    control_plane
        .commit_delivery_command(
            &update,
            &facts_with_repository(
                &update,
                repository_scope(70),
                1_800_000_000_073,
                "/workspaces/repository-70".into(),
            ),
        )
        .expect("valid update reserves its scoped request receipt");
    let mut reused = update.clone();
    reused.payload["spec"]["goal"] = serde_json::json!("A different command body.");
    let error = control_plane
        .commit_delivery_command(&reused, &mismatched_facts)
        .expect_err("same scoped request with another digest");
    assert!(matches!(
        error,
        DeliveryCommandCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::RequestConflict
    ));

    let state = control_plane
        .load_state(&format!("delivery:{}", canonical_id("dlv", 70)))
        .expect("state read")
        .expect("Delivery state");
    assert_eq!(state.revision, 2);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn corrupt_journal_query_maps_to_service_unavailable() {
    let root = temporary_directory("corrupt-journal");
    let create = create_command(75);
    let mut first_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("Control Plane start");
    first_plane
        .commit_delivery_command(&create, &facts(&create, 75))
        .expect("create");
    first_plane.shutdown().expect("shutdown");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("corruption database");
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = x'00' \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [canonical_id("dlv", 75)],
        )
        .expect("corrupt journal");
    connection.close().expect("corruption close");

    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .expect("restart");
    let update = update_spec_command(75);
    let error = restarted
        .commit_delivery_command(
            &update,
            &facts_with_repository(
                &update,
                repository_scope(75),
                1_800_000_000_076,
                "/workspaces/repository-75".into(),
            ),
        )
        .expect_err("corrupt journal cannot be queried");
    assert_eq!(error.public_code(), ErrorCode::ServiceUnavailable);
    assert!(error.retryable());
    restarted.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}

#[test]
fn create_failure_at_each_atomic_member_rolls_back_all_four_members() {
    let failure_points = [
        (
            "product-state",
            "CREATE TRIGGER fail_delivery_state BEFORE INSERT ON product_state \
             WHEN NEW.stream_id LIKE 'delivery:%' \
             BEGIN SELECT RAISE(ABORT, 'injected Delivery state failure'); END;",
        ),
        (
            "aggregate-journal",
            "CREATE TRIGGER fail_delivery_journal BEFORE INSERT ON aggregate_journal_records \
             WHEN NEW.aggregate_type = 'delivery' \
             BEGIN SELECT RAISE(ABORT, 'injected Delivery journal failure'); END;",
        ),
        (
            "command-receipt",
            "CREATE TRIGGER fail_delivery_receipt BEFORE INSERT ON command_receipts \
             WHEN NEW.request_id LIKE 'req_%' \
             BEGIN SELECT RAISE(ABORT, 'injected Delivery receipt failure'); END;",
        ),
        (
            "public-outbox",
            "CREATE TRIGGER fail_delivery_outbox BEFORE INSERT ON outbox \
             WHEN NEW.topic = 'delivery.changed.v1' \
             BEGIN SELECT RAISE(ABORT, 'injected Delivery outbox failure'); END;",
        ),
    ];

    for (offset, (member, trigger)) in failure_points.into_iter().enumerate() {
        let root = temporary_directory(member);
        Box::new(SqliteStorage::open(&root).expect("initialize schema"))
            .close()
            .expect("initialize close");
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("failure injection database");
        connection.execute_batch(trigger).expect("failure trigger");
        connection.close().expect("injector close");
        let published = Arc::new(Mutex::new(Vec::new()));
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(CapturingPublisher {
                events: Arc::clone(&published),
            }),
        )
        .expect("Control Plane start");
        let seed = 80 + u64::try_from(offset).expect("failure point index");
        let command = create_command(seed);

        let error = control_plane
            .commit_delivery_command(&command, &facts(&command, seed))
            .expect_err("injected atomic member failure");
        assert!(
            matches!(error, DeliveryCommandCommitError::Storage(_)),
            "{member}"
        );
        assert!(
            control_plane
                .load_state(&format!("delivery:{}", canonical_id("dlv", seed)))
                .expect("state read")
                .is_none(),
            "{member}"
        );
        assert!(published.lock().expect("published events").is_empty());
        control_plane.shutdown().expect("shutdown");

        let storage = SqliteStorage::open(&root).expect("inspection storage");
        assert!(
            storage
                .load_journal(
                    &AggregateJournalKey::new("delivery", canonical_id("dlv", seed))
                        .expect("journal key")
                )
                .expect("journal read")
                .is_none(),
            "{member}"
        );
        assert!(storage.pending_events().expect("outbox read").is_empty());
        Box::new(storage).close().expect("inspection close");

        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("inspection database");
        let counts: (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT \
                   (SELECT COUNT(*) FROM product_state WHERE stream_id = ?1), \
                   (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_id = ?2), \
                   (SELECT COUNT(*) FROM command_receipts WHERE request_id = ?3), \
                   (SELECT COUNT(*) FROM outbox WHERE request_id = ?3)",
                (
                    format!("delivery:{}", canonical_id("dlv", seed)),
                    canonical_id("dlv", seed),
                    command.request_id.0,
                ),
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("atomic rollback counts");
        assert_eq!(counts, (0, 0, 0, 0), "{member}");
        connection.close().expect("inspection close");
        fs::remove_dir_all(root).expect("database cleanup");
    }
}

#[test]
fn publication_failure_keeps_the_committed_delivery_event_for_restart_replay() {
    let root = temporary_directory("publication-replay");
    let command = create_command(90);
    let mut control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(FailingPublisher))
            .expect("Control Plane start");

    let error = control_plane
        .commit_delivery_command(&command, &facts(&command, 90))
        .expect_err("publication fails after the atomic commit");
    assert_eq!(error.public_code(), ErrorCode::ServiceUnavailable);
    assert!(error.retryable());
    let receipt = match error {
        DeliveryCommandCommitError::PublicationPending { receipt, .. } => *receipt,
        other => panic!("expected PublicationPending, got {other:?}"),
    };
    assert_eq!(receipt.events.len(), 1);
    control_plane
        .shutdown()
        .expect_err("failing publisher keeps the event pending");

    let storage = SqliteStorage::open(&root).expect("inspection storage");
    assert_eq!(
        storage.pending_events().expect("pending events"),
        receipt.events
    );
    let journal = storage
        .load_journal(
            &AggregateJournalKey::new("delivery", canonical_id("dlv", 90)).expect("journal key"),
        )
        .expect("journal read")
        .expect("Delivery journal");
    assert_eq!(journal.records.len(), 1);
    Box::new(storage).close().expect("inspection close");

    let published = Arc::new(Mutex::new(Vec::new()));
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&published),
        }),
    )
    .expect("outbox replay start");
    let unrelated = create_command(91);
    let replay = restarted
        .commit_delivery_command(&command, &facts(&unrelated, 91))
        .expect("same command returns its durable receipt before facts");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, receipt.revision);
    assert_eq!(replay.events, receipt.events);
    assert_eq!(published.lock().expect("published events").len(), 1);
    restarted.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database cleanup");
}
