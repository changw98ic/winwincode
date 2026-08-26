use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, RepositoryScope, Scope, UserActor,
};
use winwincode_audit::{AuditEvent, AuditExecutionSubjectKind, AuditScope};
use winwincode_control_plane::delivery_execution::{
    DeliveryExecutionConfig, DeliveryExecutionPortError, ExecutionJobDispatcher,
    PendingDeliveryExecution, prepare_delivery_advance,
};
use winwincode_control_plane::{
    CandidateResolutionError, CommitError, ControlPlane, ControlPlaneConfig,
    DeliverySessionBindingCommitError, EventPublishError, EventPublisher, OutboxEvent, StateChange,
    StorageErrorKind,
};
use winwincode_delivery::application::stage::{
    AdvanceStageInput, NewStageIdentities, TerminalArtifactReference, TerminalOutcomeStatus,
    advance,
    test_support::{
        active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
        terminal_outcome_metadata, terminal_worker_outcome, verify_terminal_outcome,
    },
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
    ArtifactId, AttentionItemId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence,
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    SessionBindingSourceIdentity, SessionBindingSourceIdentityKind, SessionIdentity, Sha256Digest,
    StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    ArtifactChunkMessage, ArtifactChunkMessageKind, ArtifactDescriptor, ArtifactKind,
    ArtifactOpenMessage, ArtifactOpenMessageKind, EncodedPayload, ExecutionJob,
    ExecutionLeaseStamp, ExecutionLimits, ExecutionPortErrorCode, ExecutionScope,
    ExecutionWorkspace, LeaseWriteStatus, SessionBindingMessage, SessionBindingMessageKind,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactErrorKind,
    CandidateSourceManifest, LocalGitSourceResolver, NewOutboxEvent, ProductStateStorage,
    ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit,
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

fn git(root: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_text(value: Vec<u8>) -> String {
    String::from_utf8(value)
        .expect("git UTF-8")
        .trim()
        .to_owned()
}

fn git_commit(root: &Path, message: &str, timestamp: &str) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["commit", "-q", "-m", message])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "WinWinCode Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_COMMITTER_NAME", "WinWinCode Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_AUTHOR_DATE", timestamp)
        .env("GIT_COMMITTER_DATE", timestamp)
        .status()
        .expect("git commit");
    assert!(status.success());
}

fn git_candidate_repository(root: &Path) -> (String, String) {
    fs::create_dir_all(root).expect("repository root");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q", "-b", "main"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git init")
            .success()
    );
    fs::write(root.join("source.txt"), b"base\n").expect("base source");
    git(root, &["add", "--", "source.txt"]);
    git_commit(root, "base", "2026-08-25T00:00:00Z");
    let base = git_text(git(root, &["rev-parse", "HEAD"]));
    fs::write(root.join("source.txt"), b"base\ncandidate\n").expect("candidate source");
    git(root, &["add", "--", "source.txt"]);
    git_commit(root, "candidate", "2026-08-25T00:01:00Z");
    let candidate = git_text(git(root, &["rev-parse", "HEAD"]));
    (base, candidate)
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
                write_mode:
                    winwincode_execution_port::generated::ExecutionWorkspaceWriteMode::Candidate,
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

fn audit_repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: winwincode_api::generated::RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
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
    let (stage_run_id, product_session_id) = match &pending.job().scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => {
            (scope.stage_run_id.clone(), scope.product_session_id.clone())
        }
        ExecutionScope::ProductSessionExecutionScope(_) => {
            panic!("fixture must use a Delivery stage job")
        }
    };
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
        attempt: 1,
        bound_at: Instant("2027-01-15T08:00:01.000Z".into()),
        codex_thread_id: CodexThreadId(canonical_id("cdx", seed)),
        fencing_token: FencingToken(seed.to_string()),
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
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        product_session_id: product_session_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.100Z".into()),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(canonical_id("cdx", seed)),
            product_session_id: product_session_id.clone(),
            stage_run_id: Some(stage_run_id.clone()),
            worker_session_id: worker_session_id.clone(),
        },
        source_identity: SessionBindingSourceIdentity {
            kind: SessionBindingSourceIdentityKind::ExecutionWorker,
            lease_id: LeaseId(canonical_id("lse", seed)),
            worker_id: WorkerId(canonical_id("wrk", seed)),
            worker_instance_id: WorkerInstanceId(canonical_id("wki", seed)),
            worker_session_id: worker_session_id.clone(),
        },
        stage_run_id,
        worker_id: WorkerId(canonical_id("wrk", seed)),
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

fn audit_event_for_receipt(root: &Path, receipt: &winwincode_storage::CommitReceipt) -> AuditEvent {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("audit event inspection database");
    let payload = connection
        .query_row(
            "SELECT payload FROM audit_outbox \
             WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3",
            rusqlite::params![
                receipt.receipt_identity.actor_key().as_bytes(),
                receipt.receipt_identity.scope_key().as_bytes(),
                receipt.receipt_identity.request_id().0,
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .expect("accepted binding audit event");
    connection.close().expect("audit event inspection close");
    serde_json::from_slice(&payload).expect("canonical accepted binding audit event JSON")
}

fn audit_event_count(root: &Path) -> i64 {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("audit event count database");
    let count = connection
        .query_row("SELECT COUNT(*) FROM audit_outbox", [], |row| row.get(0))
        .expect("audit event count");
    connection.close().expect("audit event count close");
    count
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
#[allow(clippy::too_many_lines)]
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

    let audit_event = audit_event_for_receipt(&root, committed.codex_thread_receipt());
    assert_eq!(audit_event_count(&root), 1);
    assert_eq!(
        audit_event.subject().execution_kind(),
        Some(AuditExecutionSubjectKind::AcceptedBinding)
    );
    let identity = audit_event
        .subject()
        .execution()
        .expect("accepted binding execution identity");
    assert_eq!(identity.product_session_id(), &message.product_session_id);
    assert_eq!(identity.worker_session_id(), &message.worker_session_id);
    assert_eq!(identity.codex_thread_id(), &message.codex_thread_id);
    assert_eq!(identity.stage_run_id(), &message.stage_run_id);
    assert_eq!(identity.execution_job_id(), &pending.job().job_id);
    assert_eq!(identity.delivery_id(), pending.delivery().id());
    assert!(identity.source_sequence().is_none());
    assert_eq!(
        identity
            .binding_source()
            .expect("typed binding source")
            .message_id(),
        &message.message_id
    );
    let scope = audit_repository_scope(seed);
    let audit_access = AuditScope::repository(
        scope.organization_id,
        scope.workspace_id,
        scope.project_id,
        scope.repository_id,
    )
    .expect("canonical execution audit scope")
    .into_access();
    let audit = control_plane
        .read_audit(&audit_access, 0, 20, 2_000_000_000_000)
        .expect("execution binding is visible through the canonical AuditStore");
    assert!(audit.records().iter().any(|record| {
        record.event().is_some_and(|event| {
            event.event_id() == audit_event.event_id()
                && event.subject().execution_kind()
                    == Some(AuditExecutionSubjectKind::AcceptedBinding)
        })
    }));

    let audit_event_id = audit_event.event_id().clone();
    control_plane.shutdown().expect("shutdown");
    let restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart Control Plane");
    let audit_after_restart = restarted
        .read_audit(&audit_access, 0, 20, 2_000_000_000_000)
        .expect("binding audit remains readable after restart");
    assert!(audit_after_restart.records().iter().any(|record| {
        record
            .event()
            .is_some_and(|event| event.event_id() == &audit_event_id)
    }));
    restarted.shutdown().expect("restart shutdown");
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
    assert_eq!(audit_event_count(&root), 0);
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
fn concurrent_exact_session_binding_messages_all_resolve_to_the_two_durable_phases() {
    const CALLER_COUNT: usize = 8;

    let (root, control_plane, pending, authority, message) =
        running_fixture(58, "concurrent-exact-message");
    control_plane.shutdown().expect("fixture shutdown");
    let control_planes = (0..CALLER_COUNT)
        .map(|_| {
            ControlPlane::start_local(
                ControlPlaneConfig::local(&root),
                Box::new(RecordingPublisher),
            )
            .expect("concurrent Control Plane connection")
        })
        .collect::<Vec<_>>();
    let barrier = Arc::new(Barrier::new(CALLER_COUNT));
    let callers = control_planes
        .into_iter()
        .map(|mut control_plane| {
            let barrier = Arc::clone(&barrier);
            let authority = authority.clone();
            let message = message.clone();
            thread::spawn(move || {
                barrier.wait();
                let result = control_plane
                    .commit_delivery_session_binding(&message, &authority)
                    .map(|receipt| {
                        (
                            receipt.worker_session_receipt().idempotent_replay,
                            receipt.codex_thread_receipt().idempotent_replay,
                        )
                    })
                    .map_err(|error| error.to_string());
                control_plane.shutdown().expect("concurrent shutdown");
                result
            })
        })
        .collect::<Vec<_>>();
    let receipts = callers
        .into_iter()
        .map(|caller| {
            caller
                .join()
                .expect("concurrent caller thread")
                .expect("exact concurrent message must resolve through durable receipts")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        receipts
            .iter()
            .filter(|(worker_replay, _)| !worker_replay)
            .count(),
        1
    );
    assert_eq!(
        receipts
            .iter()
            .filter(|(_, codex_replay)| !codex_replay)
            .count(),
        1
    );
    assert_eq!(
        durable_binding_counts(&root, pending.delivery().id()),
        (4, 4, 2, 4)
    );
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
fn complete_replay_rejects_non_canonical_or_foreign_durable_execution_job_facts() {
    for (seed, corruption) in [(55, "unknown-field"), (56, "foreign-task")] {
        let (root, mut control_plane, pending, authority, message) =
            running_fixture(seed, &format!("complete-replay-{corruption}"));
        control_plane
            .commit_delivery_session_binding(&message, &authority)
            .expect("initial SessionBinding transaction");
        let mut foreign_job = serde_json::to_value(pending.job()).expect("ExecutionJob JSON");
        if corruption == "unknown-field" {
            foreign_job
                .as_object_mut()
                .expect("ExecutionJob object")
                .insert("unknownField".into(), serde_json::json!(true));
        } else {
            foreign_job["scope"]["deliveryTaskId"] = serde_json::json!(canonical_id("dtk", 5_600));
        }
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("ExecutionJob mutation injector");
        connection
            .execute(
                "UPDATE outbox SET payload = ?1 WHERE event_id = ?2",
                rusqlite::params![
                    serde_json::to_vec(&foreign_job).expect("foreign ExecutionJob bytes"),
                    format!("execution-job:{}", pending.job().job_id.0)
                ],
            )
            .expect("replace durable ExecutionJob payload");
        connection.close().expect("mutation injector close");

        control_plane
            .commit_delivery_session_binding(&message, &authority)
            .expect_err("complete replay must revalidate its exact durable ExecutionJob");
        assert_eq!(
            control_plane
                .load_state(&format!("delivery:{}", pending.delivery().id().0))
                .expect("state read")
                .expect("state")
                .revision,
            4,
            "{corruption}"
        );

        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn the_same_session_binding_message_identity_with_changed_payload_is_a_request_conflict() {
    let (root, mut control_plane, pending, authority, message) =
        running_fixture(57, "session-binding-message-conflict");
    control_plane
        .commit_delivery_session_binding(&message, &authority)
        .expect("initial SessionBinding transaction");
    let mut changed_thread = message.clone();
    changed_thread.codex_thread_id = CodexThreadId(canonical_id("cdx", 5_700));
    let mut changed_session = message.clone();
    changed_session.worker_session_id = WorkerSessionId(canonical_id("wsn", 5_700));

    for changed in [changed_thread, changed_session] {
        let error = control_plane
            .commit_delivery_session_binding(&changed, &authority)
            .expect_err("one message identity cannot authorize a changed binding payload");
        assert!(matches!(
            error,
            DeliverySessionBindingCommitError::Storage(ref source)
                if source.kind() == StorageErrorKind::RequestConflict
        ));
    }
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
    assert_eq!(audit_event_count(&root), 0);
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
                assert_eq!(audit_event_count(&root), 0, "{member}");
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
                assert_eq!(audit_event_count(&root), 0, "{member}");
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

#[test]
#[allow(clippy::too_many_lines)]
fn generated_artifact_messages_use_the_exact_durable_job_and_binding_authority() {
    let seed = 1_401;
    let (root, mut control_plane, _pending, authority, binding_message) =
        running_fixture(seed, "artifact-stream");
    control_plane
        .commit_delivery_session_binding(&binding_message, &authority)
        .expect("complete SessionBinding");
    let Scope::RepositoryScope(scope) = delivery_advance_command(seed).scope else {
        panic!("fixture must use repository scope");
    };
    let artifact_id = ArtifactId(canonical_id("art", seed));
    let digest = Sha256Digest(
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
    );
    let open = ArtifactOpenMessage {
        artifact: ArtifactDescriptor {
            artifact_id: artifact_id.clone(),
            digest: digest.clone(),
            file_name: Some("candidate.json".into()),
            kind: ArtifactKind::Candidate,
            media_type: "application/vnd.winwincode.git-candidate+json".into(),
            size_bytes: 5,
        },
        kind: ArtifactOpenMessageKind::ArtifactOpen,
        lease: binding_message.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        request_id: RequestId(canonical_id("req", seed + 1)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:02.000Z".into()),
        session_identity: binding_message.session_identity.clone(),
        worker_session_id: binding_message.worker_session_id.clone(),
    };
    let opened = control_plane
        .accept_artifact_open(&scope, &open, &authority)
        .expect("artifact.open");
    assert_eq!(opened.status, LeaseWriteStatus::Accepted);
    assert_eq!(opened.ack_sequence.0, 0);
    assert_eq!(opened.artifact_id, artifact_id);

    let mut expired_open = open.clone();
    expired_open.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 20));
    expired_open.request_id = RequestId(canonical_id("req", seed + 20));
    expired_open.artifact.artifact_id = ArtifactId(canonical_id("art", seed + 20));
    expired_open.sent_at = expired_open.lease.expires_at.clone();
    let expired = control_plane
        .accept_artifact_open(&scope, &expired_open, &authority)
        .expect("expired Artifact write acknowledgement");
    assert_eq!(expired.status, LeaseWriteStatus::RejectedExpiredLease);
    assert_eq!(expired.ack_sequence.0, 0);
    assert_eq!(
        expired.error.expect("expired lease error").code,
        ExecutionPortErrorCode::LeaseExpired
    );
    let mut after_expiry = expired_open;
    after_expiry.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 21));
    after_expiry.request_id = RequestId(canonical_id("req", seed + 21));
    after_expiry.sent_at = Instant("2027-01-15T08:00:04.000Z".into());
    let accepted_after_expiry = control_plane
        .accept_artifact_open(&scope, &after_expiry, &authority)
        .expect("expired Artifact message must not reserve metadata");
    assert_eq!(accepted_after_expiry.status, LeaseWriteStatus::Accepted);

    let mut crossed_open_identities = open.clone();
    crossed_open_identities.request_id = RequestId(canonical_id("req", seed + 21));
    crossed_open_identities.artifact.artifact_id = ArtifactId(canonical_id("art", seed + 24));
    let crossed_open_identities = control_plane
        .accept_artifact_open(&scope, &crossed_open_identities, &authority)
        .expect("crossed open identities conflict acknowledgement");
    assert_eq!(
        crossed_open_identities.status,
        LeaseWriteStatus::RejectedConflict
    );
    assert_eq!(crossed_open_identities.ack_sequence.0, 0);
    assert_eq!(
        crossed_open_identities
            .error
            .expect("crossed open identity error")
            .code,
        ExecutionPortErrorCode::MessageConflict
    );

    let mut stale_fence_open = open.clone();
    stale_fence_open.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 22));
    stale_fence_open.request_id = RequestId(canonical_id("req", seed + 22));
    stale_fence_open.lease.fencing_token = FencingToken("1".into());
    let stale_fence = control_plane
        .accept_artifact_open(&scope, &stale_fence_open, &authority)
        .expect("stale fencing token acknowledgement");
    assert_eq!(
        stale_fence.status,
        LeaseWriteStatus::RejectedStaleFencingToken
    );
    assert_eq!(
        stale_fence.error.expect("stale fence error").code,
        ExecutionPortErrorCode::StaleFencingToken
    );

    let mut replaced_worker_open = open.clone();
    replaced_worker_open.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 23));
    replaced_worker_open.request_id = RequestId(canonical_id("req", seed + 23));
    replaced_worker_open.lease.worker_instance_id =
        WorkerInstanceId(canonical_id("wki", seed + 23));
    let replaced_worker = control_plane
        .accept_artifact_open(&scope, &replaced_worker_open, &authority)
        .expect("replaced Worker acknowledgement");
    assert_eq!(
        replaced_worker.status,
        LeaseWriteStatus::RejectedWorkerInstance
    );
    assert_eq!(
        replaced_worker.error.expect("Worker instance error").code,
        ExecutionPortErrorCode::WorkerInstanceChanged
    );

    let mut reused_message = open.clone();
    reused_message.artifact.artifact_id = ArtifactId(canonical_id("art", seed + 88));
    let conflict = control_plane
        .accept_artifact_open(&scope, &reused_message, &authority)
        .expect("changed artifact.open message conflict acknowledgement");
    assert_eq!(conflict.status, LeaseWriteStatus::RejectedConflict);
    assert_eq!(conflict.ack_sequence.0, 0);
    assert_eq!(
        conflict.error.expect("message identity conflict").code,
        ExecutionPortErrorCode::MessageConflict
    );

    let duplicate_open = control_plane
        .accept_artifact_open(&scope, &open, &authority)
        .expect("exact artifact.open replay");
    assert_eq!(duplicate_open.status, LeaseWriteStatus::Duplicate);
    let mut conflicting_open = open.clone();
    conflicting_open.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 5));
    conflicting_open.request_id = RequestId(canonical_id("req", seed + 5));
    conflicting_open.artifact.kind = ArtifactKind::Report;
    let conflict = control_plane
        .accept_artifact_open(&scope, &conflicting_open, &authority)
        .expect("Artifact descriptor conflict acknowledgement");
    assert_eq!(conflict.status, LeaseWriteStatus::RejectedConflict);
    assert_eq!(conflict.ack_sequence.0, 0);
    assert_eq!(
        conflict.error.expect("descriptor conflict error").code,
        ExecutionPortErrorCode::MessageConflict
    );

    let chunk = ArtifactChunkMessage {
        artifact_id: artifact_id.clone(),
        is_final: true,
        kind: ArtifactChunkMessageKind::ArtifactChunk,
        lease: binding_message.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload: EncodedPayload {
            content_type: "application/octet-stream".into(),
            data_base64: "aGVsbG8=".into(),
            payload_digest: digest,
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:03.000Z".into()),
        sequence: ExecutionSequence(1),
        session_identity: binding_message.session_identity.clone(),
        worker_session_id: binding_message.worker_session_id.clone(),
    };
    let mut invalid_transport_chunk = chunk.clone();
    invalid_transport_chunk.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 6));
    invalid_transport_chunk.payload.content_type.clear();
    control_plane
        .accept_artifact_chunk(&scope, &invalid_transport_chunk, &authority)
        .expect_err("generated EncodedPayload constraints must be revalidated at the Rust seam");

    let mut gap_chunk = chunk.clone();
    gap_chunk.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 3));
    gap_chunk.sequence = ExecutionSequence(2);
    let gap = control_plane
        .accept_artifact_chunk(&scope, &gap_chunk, &authority)
        .expect("Artifact sequence gap acknowledgement");
    assert_eq!(gap.status, LeaseWriteStatus::Gap);
    assert_eq!(gap.ack_sequence.0, 0);
    assert_eq!(gap.replay_from_sequence, Some(ExecutionSequence(1)));
    let gap_error = gap.error.expect("gap error");
    assert_eq!(gap_error.code, ExecutionPortErrorCode::SequenceGap);
    assert!(gap_error.retryable);

    let mut digest_mismatch = chunk.clone();
    digest_mismatch.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 7));
    digest_mismatch.payload.payload_digest = Sha256Digest(
        "sha256:486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7".into(),
    );
    let digest_rejection = control_plane
        .accept_artifact_chunk(&scope, &digest_mismatch, &authority)
        .expect("Artifact digest mismatch acknowledgement");
    assert_eq!(digest_rejection.status, LeaseWriteStatus::RejectedConflict);
    assert_eq!(digest_rejection.ack_sequence.0, 0);
    assert_eq!(
        digest_rejection
            .error
            .expect("Artifact digest mismatch error")
            .code,
        ExecutionPortErrorCode::ArtifactDigestMismatch
    );

    let completed = control_plane
        .accept_artifact_chunk(&scope, &chunk, &authority)
        .expect("artifact.chunk");
    assert_eq!(completed.status, LeaseWriteStatus::Accepted);
    assert_eq!(completed.ack_sequence.0, 1);
    let duplicate_chunk = control_plane
        .accept_artifact_chunk(&scope, &chunk, &authority)
        .expect("exact artifact.chunk replay");
    assert_eq!(duplicate_chunk.status, LeaseWriteStatus::Duplicate);
    assert_eq!(duplicate_chunk.ack_sequence.0, 1);

    let mut changed_chunk_transport = chunk.clone();
    changed_chunk_transport.payload.content_type = "application/json".into();
    changed_chunk_transport.sent_at = Instant("2027-01-15T08:00:04.000Z".into());
    let changed_chunk_transport = control_plane
        .accept_artifact_chunk(&scope, &changed_chunk_transport, &authority)
        .expect("changed artifact.chunk transport body conflict acknowledgement");
    assert_eq!(
        changed_chunk_transport.status,
        LeaseWriteStatus::RejectedConflict
    );
    assert_eq!(changed_chunk_transport.ack_sequence.0, 1);
    assert_eq!(
        changed_chunk_transport
            .error
            .expect("changed chunk transport body error")
            .code,
        ExecutionPortErrorCode::MessageConflict
    );

    let mut reused_chunk_message = chunk.clone();
    reused_chunk_message.artifact_id = ArtifactId(canonical_id("art", seed + 99));
    let reused_chunk = control_plane
        .accept_artifact_chunk(&scope, &reused_chunk_message, &authority)
        .expect("changed artifact.chunk identity conflict acknowledgement");
    assert_eq!(reused_chunk.status, LeaseWriteStatus::RejectedConflict);
    assert_eq!(reused_chunk.ack_sequence.0, 0);
    assert_eq!(
        reused_chunk.error.expect("chunk identity conflict").code,
        ExecutionPortErrorCode::MessageConflict
    );

    let mut conflict_chunk = chunk;
    conflict_chunk.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 4));
    conflict_chunk.payload.data_base64 = "d29ybGQ=".into();
    conflict_chunk.payload.payload_digest = Sha256Digest(
        "sha256:486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7".into(),
    );
    let conflict = control_plane
        .accept_artifact_chunk(&scope, &conflict_chunk, &authority)
        .expect("Artifact changed-message conflict acknowledgement");
    assert_eq!(conflict.status, LeaseWriteStatus::RejectedConflict);
    assert_eq!(conflict.ack_sequence.0, 1);
    assert_eq!(conflict.replay_from_sequence, None);
    let conflict_error = conflict.error.expect("conflict error");
    assert_eq!(conflict_error.code, ExecutionPortErrorCode::MessageConflict);
    assert!(!conflict_error.retryable);

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
#[allow(clippy::too_many_lines)]
fn control_plane_rebuilds_the_candidate_from_its_exact_artifact_and_successful_outcome() {
    let seed = 1_402;
    let root = temporary_directory("candidate-source");
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base_commit, candidate_commit) = git_candidate_repository(&repository);
    let mut initial_snapshot = delivery_before_advance(seed).into_snapshot();
    initial_snapshot.spec.repository.locator = "project-one".into();
    initial_snapshot.spec.base_revision.clone_from(&base_commit);
    let initial = Delivery::try_from_snapshot(initial_snapshot).expect("local Git Delivery");
    let first_transition = advance(
        &initial,
        AdvanceStageInput {
            current_lease: None,
            rework_authorization: None,
            expected_revision: initial.revision(),
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
    .expect("executor advance");
    let first_request_id = RequestId(canonical_id("req", seed));
    let first_pending = prepare_delivery_advance(
        first_request_id,
        first_transition,
        DeliveryExecutionConfig {
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            workspace: ExecutionWorkspace {
                checkout_revision: base_commit.clone(),
                repository_id: RepositoryId(canonical_id("rep", seed)),
                write_mode:
                    winwincode_execution_port::generated::ExecutionWorkspaceWriteMode::Candidate,
            },
            limits: ExecutionLimits {
                deadline_at: Instant("2027-01-15T09:00:00.000Z".into()),
                max_artifact_bytes: 10_000_000,
                max_runtime_seconds: 3_600,
            },
        },
    )
    .expect("pending executor");
    seed_delivery(&root, &initial);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    control_plane
        .commit_delivery_execution(
            &delivery_advance_command(seed),
            &first_pending,
            &mut RecordingDispatcher,
        )
        .expect("executor dispatch commit");
    let (authority, binding_message) = lease_and_message(&first_pending, seed);
    control_plane
        .commit_delivery_session_binding(&binding_message, &authority)
        .expect("complete SessionBinding");
    let Scope::RepositoryScope(scope) = delivery_advance_command(seed).scope else {
        panic!("fixture must use repository scope");
    };

    let artifact_id = ArtifactId(canonical_id("art", seed));
    let manifest = CandidateSourceManifest::new(candidate_commit.clone())
        .expect("candidate manifest")
        .encode()
        .expect("manifest encoding");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&manifest)));
    let open = ArtifactOpenMessage {
        artifact: ArtifactDescriptor {
            artifact_id: artifact_id.clone(),
            digest: digest.clone(),
            file_name: Some("candidate.json".into()),
            kind: ArtifactKind::Candidate,
            media_type: "application/vnd.winwincode.git-candidate+json".into(),
            size_bytes: i64::try_from(manifest.len()).expect("manifest length"),
        },
        kind: ArtifactOpenMessageKind::ArtifactOpen,
        lease: binding_message.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        request_id: RequestId(canonical_id("req", seed + 1)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:02.000Z".into()),
        session_identity: binding_message.session_identity.clone(),
        worker_session_id: binding_message.worker_session_id.clone(),
    };
    control_plane
        .accept_artifact_open(&scope, &open, &authority)
        .expect("artifact.open");
    let chunk = ArtifactChunkMessage {
        artifact_id: artifact_id.clone(),
        is_final: true,
        kind: ArtifactChunkMessageKind::ArtifactChunk,
        lease: binding_message.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload: EncodedPayload {
            content_type: "application/octet-stream".into(),
            data_base64: STANDARD.encode(&manifest),
            payload_digest: digest.clone(),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:03.000Z".into()),
        sequence: ExecutionSequence(1),
        session_identity: binding_message.session_identity.clone(),
        worker_session_id: binding_message.worker_session_id.clone(),
    };
    control_plane
        .accept_artifact_chunk(&scope, &chunk, &authority)
        .expect("artifact.chunk");

    let active = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("Delivery state")
        .map(|state| Delivery::decode_json(&state.payload).expect("active Delivery"))
        .expect("active Delivery exists");
    let terminal_metadata = terminal_outcome_metadata(
        Some(binding_message.codex_thread_id.clone()),
        1_800_000_060_000,
        ExecutionAckSequence(12),
        vec![TerminalArtifactReference {
            artifact_id: artifact_id.clone(),
            digest: digest.clone(),
        }],
    );
    let terminal = terminal_worker_outcome(
        StageRunId(canonical_id("run", seed)),
        first_pending.job().job_id.clone(),
        1,
        binding_message.lease.lease_id.clone(),
        binding_message.lease.fencing_token.clone(),
        binding_message.lease.worker_id.clone(),
        binding_message.lease.worker_instance_id.clone(),
        binding_message.worker_session_id.clone(),
        TerminalOutcomeStatus::Succeeded,
        terminal_metadata,
    );
    let verified = verify_terminal_outcome(&active, authority.active_lease(), terminal.clone())
        .expect("successful executor outcome");
    let terminal_facts = delivery_terminal_outcome_facts(authority.clone(), terminal);
    let next_request_id = RequestId(canonical_id("req", seed + 3));
    let next_transition = advance(
        &active,
        AdvanceStageInput {
            current_lease: Some(authority.active_lease().clone()),
            rework_authorization: None,
            expected_revision: active.revision(),
            product_session_id: ProductSessionId(canonical_id("psn", seed + 3)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(canonical_id("run", seed + 3)),
                execution_job_id: ExecutionJobId(canonical_id("job", seed + 3)),
                session_binding_id: SessionBindingId::new(format!("binding-{}", seed + 3))
                    .expect("next binding id"),
                attention_item_id: AttentionItemId(canonical_id("att", seed + 3)),
            },
            review: None,
            previous_outcome: Some(verified),
            now_millis: 1_800_000_060_100,
        },
    )
    .expect("reviewer handoff");
    let next_pending = prepare_delivery_advance(
        next_request_id.clone(),
        next_transition,
        DeliveryExecutionConfig {
            payload_digest: Sha256Digest(format!("sha256:{}", "d".repeat(64))),
            workspace: ExecutionWorkspace {
                checkout_revision: candidate_commit.clone(),
                repository_id: scope.repository_id.clone(),
                write_mode:
                    winwincode_execution_port::generated::ExecutionWorkspaceWriteMode::Candidate,
            },
            limits: ExecutionLimits {
                deadline_at: Instant("2027-01-15T10:00:00.000Z".into()),
                max_artifact_bytes: 10_000_000,
                max_runtime_seconds: 3_600,
            },
        },
    )
    .expect("pending reviewer");
    let mut next_command = delivery_advance_command(seed);
    next_command.expected_revision = Revision(i64::try_from(active.revision()).expect("revision"));
    next_command.request_id = next_request_id;
    control_plane
        .commit_delivery_execution(&next_command, &next_pending, &mut RecordingDispatcher)
        .expect("reviewer dispatch commit");
    let replayed_open = control_plane
        .accept_artifact_open(&scope, &open, &authority)
        .expect("settled StageRun must still replay its durable artifact.open acknowledgement");
    assert_eq!(replayed_open.status, LeaseWriteStatus::Duplicate);
    let replayed_chunk = control_plane
        .accept_artifact_chunk(&scope, &chunk, &authority)
        .expect("settled StageRun must still replay its durable artifact.chunk acknowledgement");
    assert_eq!(replayed_chunk.status, LeaseWriteStatus::Duplicate);
    let mut new_open_after_settlement = open.clone();
    new_open_after_settlement.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 40));
    new_open_after_settlement.request_id = RequestId(canonical_id("req", seed + 40));
    new_open_after_settlement.artifact.artifact_id = ArtifactId(canonical_id("art", seed + 40));
    control_plane
        .accept_artifact_open(&scope, &new_open_after_settlement, &authority)
        .expect_err(
            "settled StageRun may replay durable messages but cannot create a new Artifact",
        );
    control_plane
        .install_git_source_resolver(Box::new(
            LocalGitSourceResolver::open(&repositories).expect("Git source resolver"),
        ))
        .expect("install source resolver");
    let candidate = control_plane
        .resolve_delivery_candidate(&scope, initial.id(), &artifact_id, &digest, &terminal_facts)
        .expect("candidate resolution");
    assert_eq!(candidate.base_commit_id(), base_commit);
    assert_eq!(candidate.candidate_commit_id(), candidate_commit);
    assert_eq!(candidate.producer_artifact_ref(), artifact_id.0);

    let wrong_digest = Sha256Digest(format!("sha256:{}", "f".repeat(64)));
    let error = control_plane
        .resolve_delivery_candidate(
            &scope,
            initial.id(),
            &artifact_id,
            &wrong_digest,
            &terminal_facts,
        )
        .expect_err("candidate digest cannot be rebound");
    assert!(matches!(
        error,
        CandidateResolutionError::Artifact(error)
            if error.kind() == ArtifactErrorKind::PermissionDenied
    ));

    let mut foreign_scope = scope.clone();
    foreign_scope.repository_id = RepositoryId(canonical_id("rep", seed + 99));
    let error = control_plane
        .resolve_delivery_candidate(
            &foreign_scope,
            initial.id(),
            &artifact_id,
            &digest,
            &terminal_facts,
        )
        .expect_err("foreign repository scope cannot read candidate bytes");
    assert!(matches!(
        error,
        CandidateResolutionError::Artifact(error)
            if error.kind() == ArtifactErrorKind::PermissionDenied
    ));

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}
