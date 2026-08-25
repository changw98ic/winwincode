use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, ExecutionJob, ExecutionLeaseStamp, ExecutionLimits,
    ExecutionWorkspace, RepositoryScope, SchemaVersion, Scope, SessionBindingMessage,
    SessionBindingMessageKind, UserActor,
};
use winwincode_control_plane::delivery_execution::{
    DeliveryExecutionConfig, DeliveryExecutionPortError, ExecutionJobDispatcher,
    PendingDeliveryExecution, prepare_delivery_advance,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent,
};
use winwincode_delivery::application::stage::{
    AdvanceStageInput, NewStageIdentities, advance, test_support::active_lease_identity,
};
use winwincode_delivery::domain::{
    DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStatus, DeliveryTask, DeliveryTaskStatus,
    SessionBindingId,
};
use winwincode_delivery::store::{
    AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryJournalPort,
    DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
};
use winwincode_domain::{
    AttentionItemId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, ExecutionMessageId,
    FencingToken, Instant, LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Revision, Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId,
    WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-session-binding-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn delivery_before_advance(seed: u64) -> Delivery {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical fixture")
    .into_snapshot();
    let delivery_id = DeliveryId(canonical_id("dlv", seed));
    snapshot.id = delivery_id.clone();
    snapshot.spec.delivery_id = delivery_id.clone();
    snapshot.revision = 1;
    snapshot.status = DeliveryStatus::Executing;
    snapshot.tasks = vec![DeliveryTask {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: DeliveryTaskId(canonical_id("dtk", seed)),
        delivery_id,
        title: "Implement the approved task".into(),
        goal: "Implement the approved candidate change.".into(),
        acceptance_criterion_ids: vec![snapshot.spec.acceptance_criteria[0].id.clone()],
        blocked_by_task_ids: Vec::new(),
        owner: None,
        status: DeliveryTaskStatus::Pending,
    }];
    snapshot.stage_runs.clear();
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.updated_at_millis = snapshot.created_at_millis;
    Delivery::try_from_snapshot(snapshot).expect("Delivery before advance")
}

fn pending_execution(seed: u64) -> PendingDeliveryExecution {
    let delivery = delivery_before_advance(seed);
    let result = advance(
        &delivery,
        AdvanceStageInput {
            current_lease: None,
            rework_authorization: None,
            expected_revision: 1,
            product_session_id: ProductSessionId(canonical_id("psn", seed)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(canonical_id("run", seed)),
                execution_job_id: ExecutionJobId(canonical_id("job", seed)),
                session_binding_id: SessionBindingId::new(format!("binding-{seed}"))
                    .expect("binding id"),
                attention_item_id: AttentionItemId(canonical_id("att", seed)),
            },
            review: None,
            previous_outcome: None,
            now_millis: 1_800_000_000_100,
        },
    )
    .expect("stage advance");
    prepare_delivery_advance(
        RequestId(canonical_id("req", seed)),
        result,
        DeliveryExecutionConfig {
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            workspace: ExecutionWorkspace {
                checkout_revision: "original-checkout".into(),
                repository_id: RepositoryId(canonical_id("rep", seed)),
                write_mode: winwincode_api::generated::ExecutionWorkspaceWriteMode::Candidate,
            },
            limits: ExecutionLimits {
                deadline_at: Instant("2027-01-15T09:00:00.000Z".into()),
                max_artifact_bytes: 10_000_000,
                max_runtime_seconds: 3_600,
            },
        },
    )
    .expect("pending execution")
}

fn delivery_advance_command(seed: u64) -> CommandEnvelope {
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: CommandName::DeliveryAdvance,
        expected_revision: Revision(1),
        payload: serde_json::json!({"deliveryId": canonical_id("dlv", seed)}),
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(RepositoryScope {
            kind: winwincode_api::generated::RepositoryScopeKind::Repository,
            organization_id: OrganizationId(canonical_id("org", seed)),
            workspace_id: WorkspaceId(canonical_id("wsp", seed)),
            project_id: ProjectId(canonical_id("prj", seed)),
            repository_id: RepositoryId(canonical_id("rep", seed)),
        }),
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

fn seed_delivery(root: &PathBuf, delivery: &Delivery) {
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("c".repeat(64)),
            request_digest: "b".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery journal publication");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = capture
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("seed publication")
    else {
        panic!("seed must create the Delivery journal");
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
                    RequestId("c".repeat(64)),
                )
                .expect("seed identity"),
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("seed Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    format!("seed-event-{}", delivery.id().0),
                    "delivery.seeded",
                    b"seed".to_vec(),
                )],
            )
            .with_journal_publication(publication),
        )
        .expect("seed transaction");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("seed event acknowledgement");
    Box::new(storage).close().expect("seed storage close");
}

#[derive(Default)]
struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingDispatcher;

impl ExecutionJobDispatcher for RecordingDispatcher {
    fn dispatch(&mut self, _job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError> {
        Ok(())
    }
}

#[test]
fn exact_session_binding_message_commits_two_consecutive_durable_mutations() {
    let seed = 1;
    let root = temporary_directory("two-consecutive-mutations");
    let pending = pending_execution(seed);
    seed_delivery(&root, &delivery_before_advance(seed));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    control_plane
        .commit_delivery_execution(
            &delivery_advance_command(seed),
            &pending,
            &mut RecordingDispatcher,
        )
        .expect("Delivery execution commit");

    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
    let lease = active_lease_identity(
        pending.job().job_id.clone(),
        1,
        LeaseId(canonical_id("lse", seed)),
        FencingToken(seed.to_string()),
        WorkerId(canonical_id("wrk", seed)),
        WorkerInstanceId(canonical_id("wki", seed)),
        worker_session_id.clone(),
    );
    let message = SessionBindingMessage {
        bound_at: Instant("2027-01-15T08:00:01.000Z".into()),
        codex_thread_id: CodexThreadId(canonical_id("cdx", seed)),
        kind: SessionBindingMessageKind::SessionBinding,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
            fencing_token: FencingToken(seed.to_string()),
            issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
            job_id: pending.job().job_id.clone(),
            lease_id: LeaseId(canonical_id("lse", seed)),
            worker_id: WorkerId(canonical_id("wrk", seed)),
            worker_instance_id: WorkerInstanceId(canonical_id("wki", seed)),
        },
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        product_session_id: ProductSessionId(canonical_id("psn", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.100Z".into()),
        worker_session_id: worker_session_id.clone(),
    };

    let committed = control_plane
        .commit_delivery_session_binding(&message, &lease)
        .expect("typed SessionBinding transaction");

    assert_eq!(committed.worker_session_receipt().revision, 3);
    assert_eq!(committed.codex_thread_receipt().revision, 4);
    assert!(!committed.worker_session_receipt().idempotent_replay);
    assert!(!committed.codex_thread_receipt().idempotent_replay);
    for receipt in [
        committed.worker_session_receipt(),
        committed.codex_thread_receipt(),
    ] {
        assert_eq!(receipt.events.len(), 2);
        assert!(
            receipt
                .events
                .iter()
                .any(|event| event.topic == "delivery.changed.v1")
        );
        assert!(
            receipt
                .events
                .iter()
                .any(|event| event.topic == "runtime-projection.invalidated.v1")
        );
    }
    let state = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("Delivery state read")
        .expect("Delivery state");
    assert_eq!(state.revision, 4);
    let delivery = Delivery::decode_json(&state.payload).expect("Delivery snapshot");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.execution_job_id == pending.job().job_id)
        .expect("exact SessionBinding");
    assert_eq!(binding.worker_session_id.as_ref(), Some(&worker_session_id));
    assert_eq!(
        binding.codex_thread_id.as_ref(),
        Some(&message.codex_thread_id)
    );

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}
