use std::fs;
use std::path::{Path, PathBuf};
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
    CommitError, ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent,
    StateChange, StorageErrorKind,
};
use winwincode_delivery::application::stage::{
    AdvanceStageInput, NewStageIdentities, advance,
    test_support::{active_lease_identity, session_binding_authority},
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

fn lease_and_message(
    pending: &PendingDeliveryExecution,
    seed: u64,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
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
        worker_session_id,
    };
    let authority = session_binding_authority(
        lease,
        message.lease.issued_at.clone(),
        message.lease.expires_at.clone(),
    );
    (authority, message)
}

fn running_fixture(
    seed: u64,
    name: &str,
) -> (
    PathBuf,
    ControlPlane,
    PendingDeliveryExecution,
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    let root = temporary_directory(name);
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
    let (authority, message) = lease_and_message(&pending, seed);
    (root, control_plane, pending, authority, message)
}

fn install_binding_failure(root: &Path, member: &str, target_revision: u64) {
    let target = i64::try_from(target_revision).expect("test revision");
    let sql = match member {
        "state" => format!(
            "CREATE TRIGGER fail_binding_member BEFORE UPDATE ON product_state \
             WHEN NEW.stream_id LIKE 'delivery:%' AND NEW.revision = {target} \
             BEGIN SELECT RAISE(ABORT, 'injected binding state failure'); END;"
        ),
        "journal" => format!(
            "CREATE TRIGGER fail_binding_member BEFORE INSERT ON aggregate_journal_records \
             WHEN NEW.aggregate_type = 'delivery' AND NEW.sequence = {target} \
             BEGIN SELECT RAISE(ABORT, 'injected binding journal failure'); END;"
        ),
        "receipt" => format!(
            "CREATE TRIGGER fail_binding_member BEFORE INSERT ON command_receipts \
             WHEN NEW.stream_id LIKE 'delivery:%' AND NEW.revision = {target} \
             BEGIN SELECT RAISE(ABORT, 'injected binding receipt failure'); END;"
        ),
        "outbox" => format!(
            "CREATE TRIGGER fail_binding_member BEFORE INSERT ON outbox \
             WHEN NEW.topic = 'runtime-projection.invalidated.v1' AND \
                  (SELECT revision FROM command_receipts \
                   WHERE actor_key = NEW.receipt_actor_key \
                     AND scope_key = NEW.receipt_scope_key \
                     AND request_id = NEW.request_id) = {target} \
             BEGIN SELECT RAISE(ABORT, 'injected binding outbox failure'); END;"
        ),
        _ => panic!("unknown atomic member"),
    };
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("failure injector");
    connection.execute_batch(&sql).expect("failure trigger");
    connection.close().expect("failure injector close");
}

fn durable_binding_counts(root: &Path, delivery_id: &DeliveryId) -> (i64, i64, i64, i64) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let counts = connection
        .query_row(
            "SELECT \
                 (SELECT revision FROM product_state WHERE stream_id = ?1), \
                 (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?2), \
                 (SELECT COUNT(*) FROM command_receipts WHERE stream_id = ?1 AND revision > 2), \
                 (SELECT COUNT(*) FROM outbox o JOIN command_receipts r \
                    ON r.actor_key = o.receipt_actor_key AND r.scope_key = o.receipt_scope_key \
                   AND r.request_id = o.request_id WHERE r.stream_id = ?1 AND r.revision > 2)",
            rusqlite::params![format!("delivery:{}", delivery_id.0), delivery_id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("binding durable counts");
    connection.close().expect("inspection close");
    counts
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

fn seed_delivery(root: &Path, delivery: &Delivery) {
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
    let (root, mut control_plane, pending, authority, message) =
        running_fixture(seed, "two-consecutive-mutations");
    let worker_session_id = message.worker_session_id.clone();

    let committed = control_plane
        .commit_delivery_session_binding(&message, &authority)
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

#[test]
fn exact_message_replay_returns_both_original_receipts_without_new_writes() {
    let (root, mut control_plane, pending, authority, message) =
        running_fixture(2, "receipt-first-replay");
    let first = control_plane
        .commit_delivery_session_binding(&message, &authority)
        .expect("first SessionBinding transaction");
    let replay = control_plane
        .commit_delivery_session_binding(&message, &authority)
        .expect("receipt-first replay");

    assert_eq!(replay.worker_session_receipt().revision, 3);
    assert_eq!(replay.codex_thread_receipt().revision, 4);
    assert!(replay.worker_session_receipt().idempotent_replay);
    assert!(replay.codex_thread_receipt().idempotent_replay);
    assert_eq!(
        replay.worker_session_receipt().events,
        first.worker_session_receipt().events
    );
    assert_eq!(
        replay.codex_thread_receipt().events,
        first.codex_thread_receipt().events
    );
    let state = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("state read")
        .expect("state");
    assert_eq!(state.revision, 4);

    control_plane.shutdown().expect("shutdown");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let counts: (i64, i64, i64) = connection
        .query_row(
            "SELECT \
                 (SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1), \
                 (SELECT COUNT(*) FROM command_receipts WHERE stream_id = ?2), \
                 (SELECT COUNT(*) FROM outbox WHERE request_id IN \
                    (SELECT request_id FROM command_receipts WHERE stream_id = ?2))",
            rusqlite::params![
                pending.delivery().id().0,
                format!("delivery:{}", pending.delivery().id().0)
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("durable counts");
    assert_eq!(counts, (4, 4, 7));
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn scheduler_authority_rejects_a_worker_supplied_lease_window_before_writes() {
    let (root, mut control_plane, pending, authority, mut message) =
        running_fixture(3, "foreign-lease-window");
    message.lease.expires_at = Instant("2027-01-15T08:06:00.000Z".into());

    let error = control_plane
        .commit_delivery_session_binding(&message, &authority)
        .expect_err("the Worker cannot extend its scheduler-owned lease");

    assert!(error.to_string().contains("scheduler-owned lease window"));
    let state = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("state read")
        .expect("state");
    assert_eq!(state.revision, 2);
    control_plane.shutdown().expect("shutdown");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("inspection database");
    let phase_receipts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE revision > 2",
            [],
            |row| row.get(0),
        )
        .expect("phase receipt count");
    assert_eq!(phase_receipts, 0);
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn retry_continues_from_the_durable_worker_session_receipt_after_phase_two_failure() {
    let (root, mut control_plane, pending, authority, message) =
        running_fixture(4, "phase-two-resume");
    let database_path = root.join("control-plane.sqlite3");
    let connection = rusqlite::Connection::open(&database_path).expect("failure injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_codex_thread_state BEFORE UPDATE ON product_state \
             WHEN NEW.stream_id LIKE 'delivery:%' AND NEW.revision = 4 \
             BEGIN SELECT RAISE(ABORT, 'injected CodexThread phase failure'); END;",
        )
        .expect("phase-two failure trigger");
    connection.close().expect("failure injector close");

    let error = control_plane
        .commit_delivery_session_binding(&message, &authority)
        .expect_err("phase two should fail after phase one commits");
    let worker_receipt = error
        .committed_worker_session_receipt()
        .expect("phase-one durable receipt");
    assert_eq!(worker_receipt.revision, 3);
    assert!(!worker_receipt.idempotent_replay);
    let partial = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("partial state read")
        .expect("partial state");
    assert_eq!(partial.revision, 3);
    let partial_delivery = Delivery::decode_json(&partial.payload).expect("partial Delivery");
    let partial_binding = partial_delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.execution_job_id == pending.job().job_id)
        .expect("partial binding");
    assert_eq!(
        partial_binding.worker_session_id.as_ref(),
        Some(&message.worker_session_id)
    );
    assert!(partial_binding.codex_thread_id.is_none());

    let connection = rusqlite::Connection::open(&database_path).expect("failure remover");
    connection
        .execute_batch("DROP TRIGGER fail_codex_thread_state;")
        .expect("drop phase-two failure trigger");
    connection.close().expect("failure remover close");
    let resumed = control_plane
        .commit_delivery_session_binding(&message, &authority)
        .expect("receipt-first retry should finish phase two");
    assert!(resumed.worker_session_receipt().idempotent_replay);
    assert!(!resumed.codex_thread_receipt().idempotent_replay);
    assert_eq!(resumed.codex_thread_receipt().revision, 4);

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn complete_replay_uses_sealed_receipts_before_replacement_authority_or_current_state() {
    let (root, mut control_plane, pending, authority, message) =
        running_fixture(5, "sealed-receipt-first");
    control_plane
        .commit_delivery_session_binding(&message, &authority)
        .expect("initial SessionBinding transaction");
    let replacement_authority = session_binding_authority(
        authority.active_lease().clone(),
        Instant("2027-01-15T07:00:00.000Z".into()),
        Instant("2027-01-15T10:00:00.000Z".into()),
    );
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("corruption injector");
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            rusqlite::params![
                b"corrupt-current-state".as_slice(),
                format!("delivery:{}", pending.delivery().id().0)
            ],
        )
        .expect("corrupt current state");
    connection
        .execute(
            "UPDATE aggregate_journal_records SET payload = ?1 \
             WHERE aggregate_type = 'delivery' AND aggregate_id = ?2 AND sequence = 4",
            rusqlite::params![
                b"corrupt-current-journal".as_slice(),
                pending.delivery().id().0
            ],
        )
        .expect("corrupt current journal");
    connection.close().expect("corruption injector close");

    let replay = control_plane
        .commit_delivery_session_binding(&message, &replacement_authority)
        .expect("complete receipts must resolve before replacement current facts");

    assert!(replay.worker_session_receipt().idempotent_replay);
    assert!(replay.codex_thread_receipt().idempotent_replay);
    assert_eq!(replay.worker_session_receipt().revision, 3);
    assert_eq!(replay.codex_thread_receipt().revision, 4);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn generic_control_plane_commit_cannot_bypass_the_typed_session_binding_transaction() {
    let (root, mut control_plane, pending, _authority, _message) =
        running_fixture(6, "generic-bypass");
    let mut command = delivery_advance_command(6);
    command.command = CommandName::SessionCancel;
    command.expected_revision = Revision(2);
    command.request_id = RequestId(canonical_id("req", 600));

    let error = control_plane
        .commit(
            &command,
            StateChange::new(
                format!("delivery:{}", pending.delivery().id().0),
                b"forged-session-bound-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    "forged-session-bound-event",
                    "session.bound",
                    b"forged".to_vec(),
                )],
            ),
        )
        .expect_err("generic state commit must not write a Delivery stream");

    assert!(matches!(
        error,
        CommitError::Storage(ref source) if source.kind() == StorageErrorKind::InvalidInput
    ));
    let state = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("state read")
        .expect("state");
    assert_eq!(state.revision, 2);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn missing_wrong_topic_corrupt_or_foreign_execution_job_event_is_rejected_before_writes() {
    for (offset, corruption) in ["missing", "wrong-topic", "unknown-field", "foreign-binding"]
        .into_iter()
        .enumerate()
    {
        let seed = 10 + u64::try_from(offset).expect("small corruption index");
        let (root, mut control_plane, pending, authority, message) =
            running_fixture(seed, corruption);
        let event_id = format!("execution-job:{}", pending.job().job_id.0);
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("corruption injector");
        match corruption {
            "missing" => {
                connection
                    .execute("DELETE FROM outbox WHERE event_id = ?1", [&event_id])
                    .expect("delete durable job event");
            }
            "wrong-topic" => {
                connection
                    .execute(
                        "UPDATE outbox SET topic = 'foreign.job' WHERE event_id = ?1",
                        [&event_id],
                    )
                    .expect("replace durable job topic");
            }
            "unknown-field" | "foreign-binding" => {
                let mut value = serde_json::to_value(pending.job()).expect("job JSON");
                if corruption == "unknown-field" {
                    value
                        .as_object_mut()
                        .expect("job object")
                        .insert("unknownField".into(), serde_json::json!(true));
                } else {
                    value["scope"]["stageRunId"] =
                        serde_json::json!(canonical_id("run", seed + 100));
                }
                connection
                    .execute(
                        "UPDATE outbox SET payload = ?1 WHERE event_id = ?2",
                        rusqlite::params![
                            serde_json::to_vec(&value).expect("corrupt job bytes"),
                            event_id
                        ],
                    )
                    .expect("replace durable job payload");
            }
            _ => unreachable!(),
        }
        connection.close().expect("corruption injector close");

        control_plane
            .commit_delivery_session_binding(&message, &authority)
            .expect_err("foreign durable ExecutionJob facts must fail closed");
        let state = control_plane
            .load_state(&format!("delivery:{}", pending.delivery().id().0))
            .expect("state read")
            .expect("state");
        assert_eq!(state.revision, 2, "{corruption}");
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn foreign_lease_job_session_and_time_identities_are_rejected_before_writes() {
    let (root, mut control_plane, pending, authority, message) =
        running_fixture(20, "foreign-message-identities");
    let mut cases = Vec::new();
    let mut changed = message.clone();
    changed.lease.attempt = 2;
    cases.push(("attempt", changed));
    let mut changed = message.clone();
    changed.lease.lease_id = LeaseId(canonical_id("lse", 21));
    cases.push(("lease", changed));
    let mut changed = message.clone();
    changed.lease.fencing_token = FencingToken("2".into());
    cases.push(("fence", changed));
    let mut changed = message.clone();
    changed.lease.worker_id = WorkerId(canonical_id("wrk", 21));
    cases.push(("worker", changed));
    let mut changed = message.clone();
    changed.lease.worker_instance_id = WorkerInstanceId(canonical_id("wki", 21));
    cases.push(("worker-instance", changed));
    let mut changed = message.clone();
    changed.worker_session_id = WorkerSessionId(canonical_id("wsn", 21));
    cases.push(("worker-session", changed));
    let mut changed = message.clone();
    changed.lease.issued_at = Instant("2027-01-15T08:00:00.300Z".into());
    cases.push(("issued-at", changed));
    let mut changed = message.clone();
    changed.lease.expires_at = Instant("2027-01-15T08:06:00.000Z".into());
    cases.push(("expires-at", changed));
    let mut changed = message.clone();
    changed.product_session_id = ProductSessionId(canonical_id("psn", 21));
    cases.push(("product-session", changed));
    let mut changed = message.clone();
    changed.bound_at = Instant("2027-01-15T08:06:00.000Z".into());
    changed.sent_at = Instant("2027-01-15T08:06:00.100Z".into());
    cases.push(("bound-after-expiry", changed));

    for (name, changed) in cases {
        assert!(
            control_plane
                .commit_delivery_session_binding(&changed, &authority)
                .is_err(),
            "foreign {name} must fail closed"
        );
    }
    let state = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("state read")
        .expect("state");
    assert_eq!(state.revision, 2);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn every_atomic_member_rolls_back_within_each_session_binding_phase() {
    for (phase_offset, target_revision) in [(0_u64, 3_u64), (100_u64, 4_u64)] {
        for (member_offset, member) in ["state", "journal", "receipt", "outbox"]
            .into_iter()
            .enumerate()
        {
            let seed = 30
                + phase_offset
                + u64::try_from(member_offset).expect("small atomic member index");
            let (root, mut control_plane, pending, authority, message) =
                running_fixture(seed, &format!("{member}-phase-{target_revision}"));
            install_binding_failure(&root, member, target_revision);

            let error = control_plane
                .commit_delivery_session_binding(&message, &authority)
                .expect_err("injected atomic member failure");

            if target_revision == 3 {
                assert!(
                    error.committed_worker_session_receipt().is_none(),
                    "{member}"
                );
                assert_eq!(
                    durable_binding_counts(&root, pending.delivery().id()),
                    (2, 2, 0, 0),
                    "{member}"
                );
            } else {
                assert_eq!(
                    error
                        .committed_worker_session_receipt()
                        .expect("phase-one receipt")
                        .revision,
                    3,
                    "{member}"
                );
                assert_eq!(
                    durable_binding_counts(&root, pending.delivery().id()),
                    (3, 3, 1, 2),
                    "{member}"
                );
            }
            control_plane.shutdown().expect("shutdown");
            fs::remove_dir_all(root).expect("database directory release");
        }
    }
}

#[test]
fn replay_rejects_changed_receipt_digest_or_event_membership() {
    for (offset, corruption) in ["digest", "event-membership"].into_iter().enumerate() {
        let seed = 50 + u64::try_from(offset).expect("small corruption index");
        let (root, mut control_plane, pending, authority, message) =
            running_fixture(seed, corruption);
        control_plane
            .commit_delivery_session_binding(&message, &authority)
            .expect("initial SessionBinding transaction");
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("receipt corruption injector");
        if corruption == "digest" {
            connection
                .execute(
                    "UPDATE command_receipts SET command_digest = ?1 \
                     WHERE stream_id = ?2 AND revision = 3",
                    rusqlite::params![
                        format!("sha256:{}", "f".repeat(64)),
                        format!("delivery:{}", pending.delivery().id().0)
                    ],
                )
                .expect("replace phase command digest");
        } else {
            connection
                .execute(
                    "UPDATE outbox SET request_id = \
                       (SELECT request_id FROM command_receipts WHERE stream_id = ?1 AND revision = 4) \
                     WHERE topic = 'runtime-projection.invalidated.v1' AND request_id = \
                       (SELECT request_id FROM command_receipts WHERE stream_id = ?1 AND revision = 3)",
                    [format!("delivery:{}", pending.delivery().id().0)],
                )
                .expect("move event to foreign phase receipt");
        }
        connection
            .close()
            .expect("receipt corruption injector close");

        assert!(
            control_plane
                .commit_delivery_session_binding(&message, &authority)
                .is_err(),
            "{corruption}"
        );
        assert_eq!(
            control_plane
                .load_state(&format!("delivery:{}", pending.delivery().id().0))
                .expect("state read")
                .expect("state")
                .revision,
            4
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}
