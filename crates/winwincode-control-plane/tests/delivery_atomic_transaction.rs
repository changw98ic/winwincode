use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, ExecutionJob, ExecutionLimits, ExecutionWorkspace,
    RepositoryScope, SchemaVersion, Scope, UserActor,
};
use winwincode_control_plane::delivery_execution::{
    DeliveryExecutionConfig, DeliveryExecutionPortError, ExecutionJobDispatcher,
    PendingDeliveryExecution, prepare_delivery_advance,
};
use winwincode_control_plane::{
    CommitError, ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent,
    StateChange, StorageErrorKind,
};
use winwincode_delivery::application::stage::{AdvanceStageInput, NewStageIdentities, advance};
use winwincode_delivery::domain::{
    DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStatus, DeliveryTask, DeliveryTaskStatus,
    SessionBindingId,
};
use winwincode_delivery::store::{
    AppendDelivery, AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
    DeliveryJournalPort, DeliveryMutationOperation, DeliveryQuery, DeliveryQueryPort,
    DeliveryStore, InMemoryDeliveryJournal, JournalBackendError, LoadedDeliveryJournal,
};
use winwincode_domain::{
    AttentionItemId, DeliveryId, DeliveryTaskId, ExecutionJobId, Instant, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, Sha256Digest, StageRunId,
    UserId, WorkspaceId,
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
        "winwincode-delivery-atomic-{name}-{}-{suffix}",
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

fn pending_execution(seed: u64, checkout_revision: &str) -> PendingDeliveryExecution {
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
                checkout_revision: checkout_revision.into(),
                repository_id: RepositoryId(canonical_id("rep", seed)),
                write_mode: "candidate".into(),
            },
            limits: ExecutionLimits {
                deadline_at: Instant("2026-08-25T12:00:00.000Z".into()),
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
            kind: "user".into(),
        }),
        command: CommandName::DeliveryAdvance,
        expected_revision: Revision(1),
        payload: serde_json::json!({"deliveryId": canonical_id("dlv", seed)}),
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(RepositoryScope {
            kind: "repository".into(),
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
        .execute(DeliveryCommand::Create(CreateDelivery {
            request_id: RequestId("c".repeat(64)),
            request_digest: "b".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery journal publication");
    let publication = capture
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("seed publication");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = publication
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
                vec![NewOutboxEvent::new(
                    "seed-event",
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
struct RecordingDispatcher {
    jobs: Vec<ExecutionJob>,
}

struct CommitInspectingDispatcher {
    database_path: PathBuf,
    delivery_id: DeliveryId,
    request_id: RequestId,
    jobs: Vec<ExecutionJob>,
}

struct FailingDispatcher;

impl ExecutionJobDispatcher for FailingDispatcher {
    fn dispatch(&mut self, _job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError> {
        Err(DeliveryExecutionPortError::new(
            "ExecutionPort is unavailable",
        ))
    }
}

struct CapturingPublisher {
    events: Arc<Mutex<Vec<OutboxEvent>>>,
}

impl EventPublisher for CapturingPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.events
            .lock()
            .expect("published event lock")
            .push(event.clone());
        Ok(())
    }
}

impl ExecutionJobDispatcher for RecordingDispatcher {
    fn dispatch(&mut self, job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError> {
        self.jobs.push(job.clone());
        Ok(())
    }
}

impl ExecutionJobDispatcher for CommitInspectingDispatcher {
    fn dispatch(&mut self, job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError> {
        let connection = rusqlite::Connection::open(&self.database_path)
            .expect("dispatcher should observe the committed database");
        let state_revision: i64 = connection
            .query_row(
                "SELECT revision FROM product_state WHERE stream_id = ?1",
                [format!("delivery:{}", self.delivery_id.0)],
                |row| row.get(0),
            )
            .expect("canonical Delivery state must exist before dispatch");
        let journal_records: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM aggregate_journal_records \
                 WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
                [self.delivery_id.0.as_str()],
                |row| row.get(0),
            )
            .expect("Delivery journal must exist before dispatch");
        let receipts: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM command_receipts WHERE request_id = ?1",
                [self.request_id.0.as_str()],
                |row| row.get(0),
            )
            .expect("command receipt must exist before dispatch");
        let (published, payload): (i64, Vec<u8>) = connection
            .query_row(
                "SELECT published, payload FROM outbox WHERE event_id = ?1",
                [format!("execution-job:{}", job.job_id.0)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("execution job outbox intent must exist before dispatch");
        assert_eq!(state_revision, 2);
        assert_eq!(journal_records, 2);
        assert_eq!(receipts, 1);
        assert_eq!(published, 0, "dispatch must happen before acknowledgement");
        assert_eq!(
            payload,
            serde_json::to_vec(job).expect("canonical execution job JSON")
        );
        connection
            .close()
            .expect("dispatcher inspection connection close");
        self.jobs.push(job.clone());
        Ok(())
    }
}

#[test]
fn delivery_advance_commits_every_durable_fact_before_dispatch() {
    let root = temporary_directory("commit-before-dispatch");
    let pending = pending_execution(1, "original-checkout");
    seed_delivery(&root, &delivery_before_advance(1));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane should start");
    let mut dispatcher = CommitInspectingDispatcher {
        database_path: root.join("control-plane.sqlite3"),
        delivery_id: pending.delivery().id().clone(),
        request_id: pending.request_id().clone(),
        jobs: Vec::new(),
    };

    let receipt = control_plane
        .commit_delivery_execution(&delivery_advance_command(1), &pending, &mut dispatcher)
        .expect("Delivery transaction should commit then dispatch");

    assert!(receipt.dispatched);
    assert!(!receipt.commit.replayed);
    assert_eq!(receipt.commit.committed_revision, 2);
    assert_eq!(receipt.commit.job, *pending.job());
    assert_eq!(dispatcher.jobs, [pending.job().clone()]);
    let state = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("Delivery state read")
        .expect("committed Delivery state");
    assert_eq!(state.revision, 2);
    assert_eq!(
        state.payload,
        pending.delivery().encode_json().expect("Delivery JSON")
    );

    control_plane.shutdown().expect("shutdown should succeed");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let journal_records: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM aggregate_journal_records \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [pending.delivery().id().0.as_str()],
            |row| row.get(0),
        )
        .expect("journal count");
    let published_job_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE topic = 'execution.job.dispatch' AND published = 1",
            [],
            |row| row.get(0),
        )
        .expect("published job event count");
    assert_eq!(journal_records, 2);
    assert_eq!(published_job_events, 1);
    connection.close().expect("inspection connection close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn raw_borrowed_journal_can_publish_before_an_unrelated_outer_commit_fails() {
    let journal = InMemoryDeliveryJournal::new();
    let store = DeliveryStore::borrowed(&journal);
    let before = delivery_before_advance(99);
    let pending = pending_execution(99, "partial-checkout");
    store
        .execute(DeliveryCommand::Create(CreateDelivery {
            request_id: RequestId("d".repeat(64)),
            request_digest: "d".repeat(64),
            snapshot: before,
        }))
        .expect("seed raw borrowed journal");
    store
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: pending.delivery().id().clone(),
            request_id: pending.request_id().clone(),
            request_digest: "e".repeat(64),
            operation: DeliveryMutationOperation::StageStarted,
            expected_revision: 1,
            snapshot: pending.delivery().clone(),
        }))
        .expect("borrowed journal publishes before the outer commit");
    let outer_commit: Result<(), &'static str> = Err("injected outer commit failure");
    assert!(outer_commit.is_err());

    let partially_published = store
        .query(DeliveryQuery::Get(pending.delivery().id().clone()))
        .expect("journal mutation remains despite outer failure");
    assert_eq!(partially_published.revision(), 2);
}

#[test]
fn generic_control_plane_commit_rejects_every_delivery_command_bypass() {
    let root = temporary_directory("reject-delivery-bypass");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane should start");
    let stream_id = "delivery:bypass-must-not-exist";

    for command_name in [
        CommandName::DeliveryCreate,
        CommandName::DeliveryUpdateSpec,
        CommandName::DeliveryApproveTaskBreakdown,
        CommandName::DeliveryAdvance,
        CommandName::DeliveryResolveAttention,
        CommandName::DeliverySubmitVerdict,
    ] {
        let mut command = delivery_advance_command(5);
        command.command = command_name;
        let error = control_plane
            .commit(
                &command,
                StateChange::new(
                    stream_id,
                    b"bypass-state".to_vec(),
                    vec![NewOutboxEvent::new(
                        "bypass-event",
                        "execution.job.dispatch",
                        b"bypass-job".to_vec(),
                    )],
                ),
            )
            .expect_err("every Delivery command must use an atomic Delivery path");

        assert!(matches!(
            error,
            CommitError::Storage(ref source) if source.kind() == StorageErrorKind::InvalidInput
        ));
    }
    assert!(
        control_plane
            .load_state(stream_id)
            .expect("bypass state read")
            .is_none()
    );
    control_plane.shutdown().expect("shutdown should succeed");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn mismatched_delivery_command_payload_fails_before_storage_or_dispatch() {
    let root = temporary_directory("mismatched-delivery-payload");
    let before = delivery_before_advance(8);
    let pending = pending_execution(8, "must-not-dispatch");
    seed_delivery(&root, &before);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane should start");
    let mut command = delivery_advance_command(8);
    command.payload = serde_json::json!({"deliveryId": canonical_id("dlv", 9)});
    let mut dispatcher = RecordingDispatcher::default();

    let error = control_plane
        .commit_delivery_execution(&command, &pending, &mut dispatcher)
        .expect_err("a command for another Delivery must fail closed");

    assert!(matches!(
        error,
        winwincode_control_plane::delivery_execution::DeliveryExecutionError::Commit(ref source)
            if source.to_string().contains("does not identify the pending Delivery exactly")
    ));
    assert!(dispatcher.jobs.is_empty());
    let state = control_plane
        .load_state(&format!("delivery:{}", before.id().0))
        .expect("Delivery state read")
        .expect("seed state remains");
    assert_eq!(state.revision, 1);
    control_plane.shutdown().expect("shutdown should succeed");
    let storage = SqliteStorage::open(&root).expect("inspection storage");
    let journal = storage
        .load_journal(
            &AggregateJournalKey::new("delivery", before.id().0.clone()).expect("journal key"),
        )
        .expect("journal read")
        .expect("seed journal remains");
    assert_eq!(journal.records.len(), 1);
    Box::new(storage).close().expect("inspection storage close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn scoped_request_replay_returns_the_original_committed_job_without_redispatch() {
    let root = temporary_directory("original-job-replay");
    let original = pending_execution(2, "original-checkout");
    let retry = pending_execution(2, "retry-must-not-replace-original");
    seed_delivery(&root, &delivery_before_advance(2));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane should start");
    let mut dispatcher = RecordingDispatcher::default();
    let command = delivery_advance_command(2);

    let first = control_plane
        .commit_delivery_execution(&command, &original, &mut dispatcher)
        .expect("original Delivery transaction");
    let replay = control_plane
        .commit_delivery_execution(&command, &retry, &mut dispatcher)
        .expect("scoped request replay");

    assert!(!first.commit.replayed);
    assert!(replay.commit.replayed);
    assert!(!replay.dispatched);
    assert_eq!(replay.commit.committed_revision, 2);
    assert_eq!(replay.commit.outbox_event_id, first.commit.outbox_event_id);
    assert_eq!(replay.commit.job, first.commit.job);
    assert_eq!(
        replay.commit.job.workspace.checkout_revision,
        "original-checkout"
    );
    assert_eq!(dispatcher.jobs, [original.job().clone()]);

    control_plane.shutdown().expect("shutdown should succeed");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let journal_records: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM aggregate_journal_records \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [original.delivery().id().0.as_str()],
            |row| row.get(0),
        )
        .expect("journal count");
    let job_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE topic = 'execution.job.dispatch'",
            [],
            |row| row.get(0),
        )
        .expect("job event count");
    let command_receipts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE request_id = ?1",
            [original.request_id().0.as_str()],
            |row| row.get(0),
        )
        .expect("command receipt count");
    assert_eq!(journal_records, 2);
    assert_eq!(job_events, 1);
    assert_eq!(command_receipts, 1);
    connection.close().expect("inspection connection close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn corrupted_durable_job_payload_is_rejected_without_recomputed_dispatch() {
    for (seed, corruption) in [(70, "unknown-field"), (71, "foreign-stage-binding")] {
        let root = temporary_directory(corruption);
        let pending = pending_execution(seed, "durable-checkout");
        let command = delivery_advance_command(seed);
        seed_delivery(&root, &delivery_before_advance(seed));
        let mut first_control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane should start");
        let mut first_dispatcher = RecordingDispatcher::default();
        let first = first_control_plane
            .commit_delivery_execution(&command, &pending, &mut first_dispatcher)
            .expect("original Delivery execution");
        first_control_plane
            .shutdown()
            .expect("original Control Plane shutdown");

        let mut corrupted = serde_json::to_value(&first.commit.job).expect("durable job value");
        match corruption {
            "unknown-field" => {
                corrupted
                    .as_object_mut()
                    .expect("execution job object")
                    .insert("unexpectedField".into(), serde_json::json!(true));
            }
            "foreign-stage-binding" => {
                corrupted["scope"]["stageRunId"] = serde_json::json!(canonical_id("run", seed + 1));
            }
            _ => panic!("unsupported corruption fixture"),
        }
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("corruption injection database");
        connection
            .execute(
                "UPDATE outbox SET payload = ?1 WHERE event_id = ?2",
                rusqlite::params![
                    serde_json::to_vec(&corrupted).expect("corrupted job JSON"),
                    first.commit.outbox_event_id
                ],
            )
            .expect("corrupt durable job payload");
        connection.close().expect("corruption injector close");

        let mut restarted = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("published job corruption must not affect startup replay");
        let mut retry_dispatcher = RecordingDispatcher::default();
        let error = restarted
            .commit_delivery_execution(&command, &pending, &mut retry_dispatcher)
            .expect_err("durable job corruption must fail closed");
        assert!(matches!(
            error,
            winwincode_control_plane::delivery_execution::DeliveryExecutionError::Commit(ref source)
                if source.to_string().contains("unknown or non-canonical fields")
                    || source.to_string().contains("does not match the committed Delivery binding")
        ));
        assert!(retry_dispatcher.jobs.is_empty());
        restarted.shutdown().expect("restart shutdown");

        let storage = SqliteStorage::open(&root).expect("inspection storage");
        let state = storage
            .load_state(&format!("delivery:{}", pending.delivery().id().0))
            .expect("state read")
            .expect("committed state");
        let journal = storage
            .load_journal(
                &AggregateJournalKey::new("delivery", pending.delivery().id().0.clone())
                    .expect("journal key"),
            )
            .expect("journal read")
            .expect("Delivery journal");
        assert_eq!(state.revision, 2);
        assert_eq!(journal.records.len(), 2);
        Box::new(storage).close().expect("inspection storage close");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn pre_commit_failure_at_each_atomic_member_rolls_back_every_fact_and_dispatch() {
    let failure_points = [
        (
            "state",
            "CREATE TRIGGER fail_delivery_state BEFORE INSERT ON product_state \
             WHEN NEW.stream_id LIKE 'delivery:%' \
             BEGIN SELECT RAISE(ABORT, 'injected Delivery state failure'); END;",
        ),
        (
            "journal",
            "CREATE TRIGGER fail_delivery_journal BEFORE INSERT ON aggregate_journal_records \
             WHEN NEW.sequence = 2 \
             BEGIN SELECT RAISE(ABORT, 'injected Delivery journal failure'); END;",
        ),
        (
            "receipt",
            "CREATE TRIGGER fail_delivery_receipt BEFORE INSERT ON command_receipts \
             WHEN NEW.request_id LIKE 'req_%' \
             BEGIN SELECT RAISE(ABORT, 'injected Delivery receipt failure'); END;",
        ),
        (
            "outbox",
            "CREATE TRIGGER fail_execution_job_outbox BEFORE INSERT ON outbox \
             WHEN NEW.topic = 'execution.job.dispatch' \
             BEGIN SELECT RAISE(ABORT, 'injected execution job outbox failure'); END;",
        ),
    ];

    for (offset, (member, failure_trigger)) in failure_points.into_iter().enumerate() {
        let seed = 30 + u64::try_from(offset).expect("failure point index");
        let root = temporary_directory(&format!("{member}-rollback"));
        let before = delivery_before_advance(seed);
        let pending = pending_execution(seed, "must-not-commit");
        seed_delivery(&root, &before);
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("failure injection database");
        connection
            .execute_batch(failure_trigger)
            .expect("atomic member failure trigger");
        connection.close().expect("failure injector close");
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane should start");
        let mut dispatcher = RecordingDispatcher::default();

        let error = control_plane
            .commit_delivery_execution(&delivery_advance_command(seed), &pending, &mut dispatcher)
            .expect_err("pre-commit failure must abort before dispatch");

        assert!(matches!(
            error,
            winwincode_control_plane::delivery_execution::DeliveryExecutionError::Commit(_)
        ));
        assert!(dispatcher.jobs.is_empty(), "failure at {member}");
        let state = control_plane
            .load_state(&format!("delivery:{}", before.id().0))
            .expect("Delivery state read")
            .expect("seed state remains");
        assert_eq!(state.revision, 1, "failure at {member}");
        assert_eq!(
            state.payload,
            before.encode_json().expect("seed JSON"),
            "failure at {member}"
        );

        control_plane.shutdown().expect("shutdown should succeed");
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("inspection database");
        let journal_records: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM aggregate_journal_records \
                 WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
                [before.id().0.as_str()],
                |row| row.get(0),
            )
            .expect("journal count");
        let job_events: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE topic = 'execution.job.dispatch'",
                [],
                |row| row.get(0),
            )
            .expect("job event count");
        let command_receipts: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM command_receipts WHERE request_id = ?1",
                [pending.request_id().0.as_str()],
                |row| row.get(0),
            )
            .expect("command receipt count");
        assert_eq!(journal_records, 1, "failure at {member}");
        assert_eq!(job_events, 0, "failure at {member}");
        assert_eq!(command_receipts, 0, "failure at {member}");
        connection.close().expect("inspection connection close");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn dispatch_failure_keeps_the_committed_job_pending_for_restart_replay() {
    let root = temporary_directory("dispatch-restart-replay");
    let pending = pending_execution(4, "restart-checkout");
    seed_delivery(&root, &delivery_before_advance(4));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane should start");

    let error = control_plane
        .commit_delivery_execution(
            &delivery_advance_command(4),
            &pending,
            &mut FailingDispatcher,
        )
        .expect_err("dispatch failure must report committed pending publication");
    let committed = error
        .committed_receipt()
        .expect("dispatch failure must carry the committed receipt");
    assert_eq!(committed.committed_revision, 2);
    assert_eq!(committed.job, *pending.job());
    drop(control_plane);

    let replayed_events = Arc::new(Mutex::new(Vec::new()));
    let publisher = CapturingPublisher {
        events: Arc::clone(&replayed_events),
    };
    let restarted =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("restart should replay the committed job intent");
    let replayed_events = replayed_events.lock().expect("replayed event lock");
    assert_eq!(replayed_events.len(), 1);
    assert_eq!(replayed_events[0].topic, "execution.job.dispatch");
    assert_eq!(replayed_events[0].event_id, committed.outbox_event_id);
    let replayed_job: serde_json::Value =
        serde_json::from_slice(&replayed_events[0].payload).expect("replayed job JSON");
    assert_eq!(
        replayed_job["jobId"],
        serde_json::Value::String(pending.job().job_id.0.clone())
    );
    assert_eq!(
        replayed_job["workspace"]["checkoutRevision"],
        "restart-checkout"
    );
    drop(replayed_events);

    restarted.shutdown().expect("restart shutdown");
    let storage = SqliteStorage::open(&root).expect("inspection storage");
    assert!(storage.pending_events().expect("pending events").is_empty());
    let journal = storage
        .load_journal(
            &AggregateJournalKey::new("delivery", pending.delivery().id().0.clone())
                .expect("journal key"),
        )
        .expect("journal read")
        .expect("Delivery journal");
    assert_eq!(journal.records.len(), 2);
    Box::new(storage).close().expect("inspection storage close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn acknowledgement_failure_keeps_the_dispatched_job_pending_for_restart_replay() {
    let root = temporary_directory("ack-restart-replay");
    let pending = pending_execution(6, "ack-restart-checkout");
    seed_delivery(&root, &delivery_before_advance(6));
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("ack failure injection database");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_execution_job_ack BEFORE UPDATE OF published ON outbox \
             WHEN OLD.topic = 'execution.job.dispatch' \
             BEGIN SELECT RAISE(ABORT, 'injected execution job ack failure'); END;",
        )
        .expect("ack failure trigger");
    connection.close().expect("ack failure injector close");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane should start");
    let mut dispatcher = RecordingDispatcher::default();

    let error = control_plane
        .commit_delivery_execution(&delivery_advance_command(6), &pending, &mut dispatcher)
        .expect_err("ack failure must preserve the committed pending event");
    assert!(matches!(
        error,
        winwincode_control_plane::delivery_execution::DeliveryExecutionError::AcknowledgeAfterDispatch { .. }
    ));
    assert_eq!(dispatcher.jobs, [pending.job().clone()]);
    drop(control_plane);

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("ack trigger cleanup database");
    connection
        .execute_batch("DROP TRIGGER fail_execution_job_ack;")
        .expect("ack failure trigger cleanup");
    connection.close().expect("ack trigger cleanup close");
    let replayed_events = Arc::new(Mutex::new(Vec::new()));
    let restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(CapturingPublisher {
            events: Arc::clone(&replayed_events),
        }),
    )
    .expect("restart should replay the unacknowledged job");
    let replayed_events = replayed_events.lock().expect("replayed event lock");
    assert_eq!(replayed_events.len(), 1);
    assert_eq!(replayed_events[0].topic, "execution.job.dispatch");
    drop(replayed_events);
    restarted.shutdown().expect("restart shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}
