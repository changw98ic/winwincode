use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use sha2::{Digest, Sha256};

use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, DeliveryResolveAttentionCommand,
    DeliveryResolveAttentionCommandCommand, DeliveryResolveAttentionPayload,
    DeliverySubmitVerdictCommand, DeliverySubmitVerdictCommandCommand,
    DeliverySubmitVerdictPayload, RepositoryScope, Scope, UserActor,
};
use winwincode_audit::{AuditEvent, AuditExecutionSubjectKind, AuditScope};
use winwincode_control_plane::delivery_execution::{
    DeliveryExecutionConfig, DeliveryExecutionPortError, ExecutionJobDispatcher,
    PendingDeliveryExecution, prepare_delivery_advance,
};
use winwincode_control_plane::{
    ArtifactMessageError, CandidateResolutionError, CommitError, ControlPlane, ControlPlaneConfig,
    DeliverySessionBindingCommitError, DurableExecutionPortIngress, EventPublishError,
    EventPublisher, LocalDeliveryAdapterConfig, OutboxEvent, RepositoryExecutionScheduler,
    StateChange, StorageErrorKind,
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
    ExecutionEventId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant,
    LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Revision,
    SchemaVersion, SessionBindingSourceIdentity, SessionBindingSourceIdentityKind, SessionIdentity,
    Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    ArtifactChunkMessage, ArtifactChunkMessageKind, ArtifactDescriptor, ArtifactKind,
    ArtifactOpenMessage, ArtifactOpenMessageKind, ArtifactReference, EncodedPayload,
    ExecutionEventCategory, ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp,
    ExecutionLimits, ExecutionOutcome, ExecutionOutcomeStatus, ExecutionOutcomeUsage,
    ExecutionPortErrorCode, ExecutionPortMessage, ExecutionScope, ExecutionWorkspace,
    JobCancelMessage, JobCancelMessageKind, JobCancelMessageReason, JobDispatchMessage,
    JobDispatchResultMessage, JobDispatchResultMessageKind, JobDispatchResultMessageStatus,
    JobOutcomeAckMessageStatus, JobOutcomeMessage, JobOutcomeMessageKind, LeaseWriteStatus,
    RuntimeEventMessage, RuntimeEventMessageKind, SessionBindingMessage, SessionBindingMessageKind,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactErrorKind,
    CandidateGitPinReceipt, CandidateGitReleaseAuthority, CandidateGitTerminalOutcome,
    CandidateSourceManifest, EXECUTION_PROTOCOL_VERSION, EnterpriseQuotaBoundary,
    EnterpriseQuotaLimits, EnterpriseQuotaPolicy, EnterpriseQuotaReservationState,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionJobState, ExecutionJobSubmission, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseRecovery, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    RepositorySchedulerClaimRequest, RepositorySchedulerScope, SqliteStorage, StateCommit,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources,
};
use winwincode_storage::{
    PublicEventActor, PublicEventScope, PublicEventSource, receipt_actor_key, receipt_scope_key,
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

fn latest_queued_delivery_job(root: &Path) -> ExecutionJob {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Delivery queue inspection");
    let payload = connection
        .query_row(
            "SELECT dispatch_payload FROM scheduler_execution_jobs ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .expect("read queued Delivery job");
    connection.close().expect("Delivery queue inspection close");
    serde_json::from_slice(&payload).expect("decode queued Delivery job")
}

fn load_candidate_pin(
    root: &Path,
    repositories: &Path,
    artifact_id: &ArtifactId,
) -> CandidateGitPinReceipt {
    let mut storage = SqliteStorage::open(root).expect("candidate pin storage");
    let pin = {
        let mut retention = storage
            .git_candidate_retention(repositories)
            .expect("candidate pin retention");
        retention
            .load_by_artifact(artifact_id)
            .expect("candidate pin lookup")
            .expect("candidate pin exists")
    };
    Box::new(storage)
        .close()
        .expect("candidate pin storage close");
    pin
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
            candidate_ref: None,
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
    lease_and_message_for_job(pending.job(), seed)
}

fn lease_and_message_for_job(
    job: &ExecutionJob,
    seed: u64,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    lease_and_message_for_job_at(
        job,
        seed,
        "2027-01-15T08:00:01.000Z",
        "2027-01-15T08:00:00.200Z",
        "2027-01-15T08:00:01.100Z",
    )
}

fn lease_and_message_for_job_at(
    job: &ExecutionJob,
    seed: u64,
    bound_at: &str,
    issued_at: &str,
    sent_at: &str,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
    let (stage_run_id, product_session_id) = match &job.scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => {
            (scope.stage_run_id.clone(), scope.product_session_id.clone())
        }
        ExecutionScope::ProductSessionExecutionScope(_) => {
            panic!("fixture must use a Delivery stage job")
        }
    };
    let lease = active_lease_identity(
        job.job_id.clone(),
        1,
        LeaseId(canonical_id("lse", seed)),
        FencingToken(seed.to_string()),
        WorkerId(canonical_id("wrk", seed)),
        WorkerInstanceId(canonical_id("wki", seed)),
        worker_session_id.clone(),
    );
    let message = SessionBindingMessage {
        attempt: 1,
        bound_at: Instant(bound_at.into()),
        codex_thread_id: CodexThreadId(canonical_id("cdx", seed)),
        fencing_token: FencingToken(seed.to_string()),
        kind: SessionBindingMessageKind::SessionBinding,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
            fencing_token: FencingToken(seed.to_string()),
            issued_at: Instant(issued_at.into()),
            job_id: job.job_id.clone(),
            lease_id: LeaseId(canonical_id("lse", seed)),
            worker_id: WorkerId(canonical_id("wrk", seed)),
            worker_instance_id: WorkerInstanceId(canonical_id("wki", seed)),
        },
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        product_session_id: product_session_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant(sent_at.into()),
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
        stage_run_id: Some(stage_run_id),
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

struct RuntimeEventBatch<'a> {
    control_plane: &'a mut ControlPlane,
    scope: &'a RepositoryScope,
    binding: &'a SessionBindingMessage,
    authority: &'a winwincode_delivery::application::stage::SessionBindingAuthority,
    seed: u64,
    candidate_ref: &'a str,
    delivery_spec_id: &'a str,
    delivery_spec_revision: u64,
    criterion_id: &'a str,
    finding_id: &'a str,
    occurred_at: &'a str,
    sent_at: &'a str,
}

fn accept_runtime_events(batch: RuntimeEventBatch<'_>) {
    let RuntimeEventBatch {
        control_plane,
        scope,
        binding,
        authority,
        seed,
        candidate_ref,
        delivery_spec_id,
        delivery_spec_revision,
        criterion_id,
        finding_id,
        occurred_at,
        sent_at,
    } = batch;
    for sequence in 1..=12_i64 {
        let event_seed = seed + u64::try_from(sequence).expect("runtime sequence");
        let (category, payload) = match sequence {
            1 => (
                ExecutionEventCategory::Lifecycle,
                Some(encoded_json(&serde_json::json!({
                    "protocol": "winwincode.verification-session-policy.v1",
                    "workspace_mode": "candidate-read-only",
                    "permission_profile": "candidate-read-only-restricted",
                    "candidate_ref": candidate_ref,
                }))),
            ),
            2 => (
                ExecutionEventCategory::Test,
                Some(encoded_json(&serde_json::json!({
                    "status": "succeeded",
                    "exit_code": 0,
                }))),
            ),
            3 => (
                ExecutionEventCategory::Activity,
                Some(encoded_json(&serde_json::json!({
                    "protocol": "winwincode.independent-verification-result.v1",
                    "delivery_spec_id": delivery_spec_id,
                    "delivery_spec_revision": delivery_spec_revision,
                    "candidate_ref": candidate_ref,
                    "findings": [{
                        "finding_id": finding_id,
                        "criterion_id": criterion_id,
                        "verdict": "pass",
                        "explanation": "The exact candidate passed the verification criterion.",
                        "evidence_sources": [{
                            "type": "test",
                            "event_id": canonical_id("xevt", seed + 2),
                        }],
                    }],
                }))),
            ),
            _ => (ExecutionEventCategory::Lifecycle, None),
        };
        let message = RuntimeEventMessage {
            codex_thread_id: binding.codex_thread_id.clone(),
            event: ExecutionEventRecord {
                category,
                event_id: ExecutionEventId(canonical_id("xevt", event_seed)),
                occurred_at: Instant(occurred_at.into()),
                payload,
                sequence: ExecutionSequence(sequence),
                summary: format!("verification event {sequence}"),
            },
            kind: RuntimeEventMessageKind::RuntimeEvent,
            lease: binding.lease.clone(),
            message_id: ExecutionMessageId(canonical_id(
                "xmsg",
                seed + 100 + u64::try_from(sequence).expect("runtime sequence"),
            )),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant(sent_at.into()),
            session_identity: binding.session_identity.clone(),
            worker_session_id: binding.worker_session_id.clone(),
        };
        let acknowledgement = control_plane
            .accept_runtime_event(scope, &message, authority, &message.sent_at)
            .expect("verification runtime event");
        assert_eq!(acknowledgement.status, LeaseWriteStatus::Accepted);
        assert_eq!(acknowledgement.ack_sequence.0, sequence);
    }
}

fn encoded_json(value: &serde_json::Value) -> EncodedPayload {
    let bytes = serde_json::to_vec(value).expect("JSON payload");
    EncodedPayload {
        content_type: "application/json".into(),
        data_base64: STANDARD.encode(&bytes),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    }
}

#[allow(clippy::too_many_arguments)]
fn accept_verification_artifact(
    control_plane: &mut ControlPlane,
    scope: &RepositoryScope,
    binding: &SessionBindingMessage,
    authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
    artifact_id: ArtifactId,
    digest: Sha256Digest,
    manifest: &[u8],
    seed: u64,
) {
    let open = ArtifactOpenMessage {
        artifact: ArtifactDescriptor {
            artifact_id: artifact_id.clone(),
            digest: digest.clone(),
            file_name: Some("verification-candidate.json".into()),
            kind: ArtifactKind::Candidate,
            media_type: "application/vnd.winwincode.git-candidate+json".into(),
            size_bytes: i64::try_from(manifest.len()).expect("verification manifest length"),
        },
        kind: ArtifactOpenMessageKind::ArtifactOpen,
        lease: binding.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: binding.sent_at.clone(),
        session_identity: binding.session_identity.clone(),
        worker_session_id: binding.worker_session_id.clone(),
    };
    control_plane
        .accept_artifact_open(scope, &open, authority)
        .expect("verification artifact.open");
    let chunk = ArtifactChunkMessage {
        artifact_id,
        is_final: true,
        kind: ArtifactChunkMessageKind::ArtifactChunk,
        lease: binding.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload: EncodedPayload {
            content_type: "application/octet-stream".into(),
            data_base64: STANDARD.encode(manifest),
            payload_digest: digest,
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: binding.sent_at.clone(),
        sequence: ExecutionSequence(1),
        session_identity: binding.session_identity.clone(),
        worker_session_id: binding.worker_session_id.clone(),
    };
    let acknowledgement = control_plane
        .accept_artifact_chunk(scope, &chunk, authority)
        .expect("verification artifact.chunk");
    assert_eq!(acknowledgement.status, LeaseWriteStatus::Accepted);
    assert_eq!(acknowledgement.ack_sequence, ExecutionAckSequence(1));
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

fn scheduler_delivery_fixture(seed: u64, name: &str) -> (PathBuf, PendingDeliveryExecution) {
    let root = temporary_directory(name);
    let pending = pending_execution(seed);
    seed_delivery(&root, &delivery_before_advance(seed));
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("scheduler fixture Control Plane");
    control_plane
        .commit_delivery_execution(
            &delivery_advance_command(seed),
            &pending,
            &mut RecordingDispatcher,
        )
        .expect("scheduler fixture Delivery execution");
    control_plane
        .shutdown()
        .expect("scheduler fixture shutdown");
    let mut storage = SqliteStorage::open(&root).expect("scheduler queue storage");
    let job = pending.job();
    let stage_run_id = match &job.scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => Some(scope.stage_run_id.clone()),
        ExecutionScope::ProductSessionExecutionScope(_) => None,
    };
    storage
        .execution_queue()
        .expect("scheduler queue")
        .submit(&ExecutionJobSubmission {
            scope: execution_queue_scope(seed, &pending),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 220)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(job).expect("canonical queued job"),
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id,
            submitted_at: attempt_time(1, 1),
        })
        .expect("scheduler job submit");
    Box::new(storage).close().expect("scheduler queue close");
    (root, pending)
}

fn scheduler_scope(seed: u64) -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
    }
}

fn execution_queue_scope(seed: u64, pending: &PendingDeliveryExecution) -> ExecutionQueueScope {
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &pending.job().scope else {
        panic!("scheduler fixture requires a Delivery execution");
    };
    let repository = scheduler_scope(seed);
    ExecutionQueueScope {
        organization_id: repository.organization_id,
        workspace_id: repository.workspace_id,
        project_id: repository.project_id,
        repository_id: repository.repository_id,
        product_session_id: scope.product_session_id.clone(),
        delivery_id: Some(scope.delivery_id.clone()),
    }
}

fn scheduler_repository_scope(seed: u64) -> RepositoryScope {
    let scope = scheduler_scope(seed);
    RepositoryScope {
        kind: winwincode_domain::RepositoryScopeKind::Repository,
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
    }
}

fn attempt_time(attempt: u64, second: u64) -> Instant {
    Instant(format!(
        "2027-01-15T08:{:02}:{second:02}.000Z",
        attempt.saturating_sub(1)
    ))
}

fn register_attempt_worker(
    storage: &mut SqliteStorage,
    seed: u64,
    attempt: u64,
) -> (WorkerId, WorkerInstanceId) {
    let worker_id = WorkerId(canonical_id("wrk", seed + 100));
    let worker_instance_id = WorkerInstanceId(canonical_id("wki", seed + 100 + attempt));
    let receipt = storage
        .execution_registry()
        .expect("scheduler Registry")
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "replacement-fixture".into(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".into()],
            capability_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            security_zone: "local".into(),
            max_slots: 1,
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 200 + attempt * 10)),
            request_id: RequestId(canonical_id("req", seed + 200 + attempt * 10)),
            sent_at: attempt_time(attempt, 1),
            started_at: attempt_time(attempt, 0),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("register attempt Worker");
    assert_eq!(
        receipt.lease_recovery,
        if attempt == 1 {
            LeaseRecovery::NoActiveLeases
        } else {
            LeaseRecovery::ReacquireRequired
        }
    );
    storage
        .execution_registry()
        .expect("scheduler Registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 1,
            running_slots: 0,
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 201 + attempt * 10)),
            observed_at: attempt_time(attempt, 2),
            sent_at: attempt_time(attempt, 2),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("attempt heartbeat");
    (worker_id, worker_instance_id)
}

fn prepare_delivery_admission(
    storage: &mut SqliteStorage,
    seed: u64,
    pending: &PendingDeliveryExecution,
) {
    let scope = execution_queue_scope(seed, pending);
    let worker_pool_id = WorkerPoolId(canonical_id("wpl", seed));
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 2,
        max_queued: 2,
        token_budget: 10_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 60_000,
    };
    let boundaries = [
        ExecutionAdmissionBoundary::Organization {
            organization_id: scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: scope.delivery_id.clone().expect("Delivery queue scope"),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        },
    ];
    let mut admission = storage.execution_admission().expect("execution admission");
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(canonical_id("usr", seed)),
            worker_pool_id: worker_pool_id.clone(),
            job_id: pending.job().job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 230)),
            repository_access: ExecutionRepositoryAccess::IsolatedWrite {
                worktree_key: pending.job().job_id.0.clone(),
            },
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: attempt_time(1, 1),
        })
        .expect("admission reserve");
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id,
            job_id: pending.job().job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 231)),
            expected_revision: 1,
            started_at: attempt_time(1, 2),
        })
        .expect("admission start");
}

fn binding_for_dispatch(
    pending: &PendingDeliveryExecution,
    dispatch: &JobDispatchMessage,
    seed: u64,
    attempt: u64,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &dispatch.job.scope else {
        panic!("dispatch must be a Delivery stage");
    };
    assert_eq!(dispatch.job.job_id, pending.job().job_id);
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed + attempt * 10));
    let codex_thread_id = CodexThreadId(canonical_id("cdx", seed + attempt * 10));
    let message_id = ExecutionMessageId(canonical_id("xmsg", seed + 240 + attempt * 10));
    let bound_at = attempt_time(attempt, 5);
    let sent_at = attempt_time(attempt, 6);
    let message = SessionBindingMessage {
        attempt: dispatch.lease.attempt,
        bound_at,
        codex_thread_id: codex_thread_id.clone(),
        fencing_token: dispatch.lease.fencing_token.clone(),
        kind: SessionBindingMessageKind::SessionBinding,
        lease: dispatch.lease.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        message_id,
        product_session_id: scope.product_session_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at,
        session_identity: SessionIdentity {
            codex_thread_id: codex_thread_id.clone(),
            product_session_id: scope.product_session_id.clone(),
            stage_run_id: Some(scope.stage_run_id.clone()),
            worker_session_id: worker_session_id.clone(),
        },
        source_identity: SessionBindingSourceIdentity {
            kind: SessionBindingSourceIdentityKind::ExecutionWorker,
            lease_id: dispatch.lease.lease_id.clone(),
            worker_id: dispatch.lease.worker_id.clone(),
            worker_instance_id: dispatch.lease.worker_instance_id.clone(),
            worker_session_id: worker_session_id.clone(),
        },
        stage_run_id: Some(scope.stage_run_id.clone()),
        worker_id: dispatch.lease.worker_id.clone(),
        worker_session_id: worker_session_id.clone(),
    };
    let lease = active_lease_identity(
        dispatch.job.job_id.clone(),
        attempt,
        dispatch.lease.lease_id.clone(),
        dispatch.lease.fencing_token.clone(),
        dispatch.lease.worker_id.clone(),
        dispatch.lease.worker_instance_id.clone(),
        worker_session_id,
    );
    let authority = session_binding_authority(
        lease,
        dispatch.lease.issued_at.clone(),
        dispatch.lease.expires_at.clone(),
    );
    (authority, message)
}

fn claim_delivery_attempt_with_slot(
    root: &Path,
    pending: &PendingDeliveryExecution,
    seed: u64,
    attempt: u64,
    generation: &str,
    open_slot: bool,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    let mut storage = SqliteStorage::open(root).expect("scheduler attempt storage");
    let (worker_id, worker_instance_id) = register_attempt_worker(&mut storage, seed, attempt);
    let dispatch = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope: scheduler_scope(seed),
            request_id: RequestId(canonical_id("req", seed + 250 + attempt * 10)),
            scheduler_generation: generation.into(),
            worker_id,
            worker_instance_id,
            issued_at: attempt_time(attempt, 3),
            expires_at: attempt_time(attempt, 50),
        })
        .expect("scheduler claim")
        .expect("typed dispatch");
    assert_eq!(
        dispatch.job.attempt,
        i64::try_from(attempt).expect("attempt")
    );
    if attempt == 1 {
        assert!(dispatch.replacement_authority.is_none());
        prepare_delivery_admission(&mut storage, seed, pending);
    } else {
        let replacement = dispatch
            .replacement_authority
            .as_ref()
            .expect("typed replacement authority");
        assert_eq!(replacement.successor_lease, dispatch.lease);
    }
    let (authority, message) = binding_for_dispatch(pending, &dispatch, seed, attempt);
    let running = RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(
            &scheduler_repository_scope(seed),
            &JobDispatchResultMessage {
                error: None,
                job_id: dispatch.job.job_id.clone(),
                kind: JobDispatchResultMessageKind::JobDispatchResult,
                lease: dispatch.lease.clone(),
                message_id: ExecutionMessageId(canonical_id("xmsg", seed + 251 + attempt * 10)),
                payload_digest: dispatch.job.payload_digest.clone(),
                request_id: RequestId(canonical_id("req", seed + 251 + attempt * 10)),
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: attempt_time(attempt, 4),
                status: JobDispatchResultMessageStatus::Accepted,
                worker_session_id: Some(message.worker_session_id.clone()),
            },
            &attempt_time(attempt, 4),
        )
        .expect("accepted dispatch result");
    assert!(running.accepted);
    assert_eq!(running.job.state, ExecutionJobState::Running);
    let slot_authority = WorkerSlotAuthority {
        worker_id: dispatch.lease.worker_id.clone(),
        worker_instance_id: dispatch.lease.worker_instance_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        codex_thread_id: message.codex_thread_id.clone(),
        job_id: dispatch.job.job_id,
        lease_id: dispatch.lease.lease_id,
        attempt,
        fencing_token: dispatch.lease.fencing_token,
    };
    if open_slot {
        let mut slots = storage.worker_session_slots().expect("Worker slots");
        slots
            .configure_resources(
                &slot_authority.worker_id,
                &slot_authority.worker_instance_id,
                WorkerSlotResourceLimits {
                    max_memory_bytes: 100,
                    max_disk_bytes: 100,
                    max_processes: 1,
                },
            )
            .expect("slot resources");
        slots
            .open(&WorkerSlotOpenRequest {
                authority: slot_authority,
                resources: WorkerSlotResources {
                    memory_bytes: 10,
                    disk_bytes: 10,
                    process_slots: 1,
                },
                request_id: RequestId(canonical_id("req", seed + 252 + attempt * 10)),
                opened_at: attempt_time(attempt, 5),
            })
            .expect("Worker slot");
    }
    Box::new(storage).close().expect("scheduler attempt close");
    (authority, message)
}

fn claim_delivery_attempt(
    root: &Path,
    pending: &PendingDeliveryExecution,
    seed: u64,
    attempt: u64,
    generation: &str,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    claim_delivery_attempt_with_slot(root, pending, seed, attempt, generation, true)
}

fn claim_delivery_attempt_without_slot(
    root: &Path,
    pending: &PendingDeliveryExecution,
    seed: u64,
    attempt: u64,
    generation: &str,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    claim_delivery_attempt_with_slot(root, pending, seed, attempt, generation, false)
}

fn claim_running_delivery_attempt(
    root: &Path,
    pending: &PendingDeliveryExecution,
    seed: u64,
    attempt: u64,
    generation: &str,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    claim_delivery_attempt(root, pending, seed, attempt, generation)
}

fn claim_replacement_delivery_attempt(
    root: &Path,
    pending: &PendingDeliveryExecution,
    seed: u64,
    attempt: u64,
    generation: &str,
) -> (
    winwincode_delivery::application::stage::SessionBindingAuthority,
    SessionBindingMessage,
) {
    claim_delivery_attempt(root, pending, seed, attempt, generation)
}

fn stale_delivery_runtime(message: &SessionBindingMessage, seed: u64) -> RuntimeEventMessage {
    RuntimeEventMessage {
        codex_thread_id: message.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Lifecycle,
            event_id: ExecutionEventId(canonical_id("xevt", seed)),
            occurred_at: attempt_time(2, 8),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "stale predecessor runtime event".into(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: message.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: attempt_time(2, 8),
        session_identity: message.session_identity.clone(),
        worker_session_id: message.worker_session_id.clone(),
    }
}

fn stale_delivery_outcome(message: &SessionBindingMessage, seed: u64) -> JobOutcomeMessage {
    JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: message.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        outcome: ExecutionOutcome {
            artifacts: Vec::new(),
            codex_thread_id: Some(message.codex_thread_id.clone()),
            error: None,
            finished_at: attempt_time(2, 8),
            last_event_sequence: ExecutionAckSequence(0),
            status: ExecutionOutcomeStatus::Failed,
            summary: "stale predecessor outcome".into(),
            usage: None,
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: attempt_time(2, 8),
        session_identity: message.session_identity.clone(),
        worker_session_id: message.worker_session_id.clone(),
    }
}

fn stale_delivery_cancel(message: &SessionBindingMessage, seed: u64) -> JobCancelMessage {
    JobCancelMessage {
        kind: JobCancelMessageKind::JobCancel,
        lease: message.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        reason: JobCancelMessageReason::Superseded,
        requested_at: attempt_time(2, 8),
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: attempt_time(2, 8),
        session_identity: message.session_identity.clone(),
        worker_session_id: message.worker_session_id.clone(),
    }
}

fn delivery_authority_bytes(
    storage: &mut SqliteStorage,
    pending: &PendingDeliveryExecution,
    seed: u64,
    sessions: &[&WorkerSessionId],
) -> Vec<u8> {
    let state = storage
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("Delivery state")
        .expect("Delivery state exists");
    let queue = storage
        .execution_queue()
        .expect("queue")
        .load_job(&execution_queue_scope(seed, pending), &pending.job().job_id)
        .expect("queue job")
        .expect("queue job exists");
    let lease = storage
        .execution_registry()
        .expect("Registry")
        .load_lease(&pending.job().job_id)
        .expect("Registry lease");
    let slots = sessions
        .iter()
        .map(|session| {
            storage
                .worker_session_slots()
                .expect("slots")
                .load(session)
                .expect("slot")
        })
        .collect::<Vec<_>>();
    serde_json::to_vec(&(
        state.stream_id,
        state.revision,
        state.payload,
        queue,
        lease,
        slots,
    ))
    .expect("Delivery authority snapshot")
}

fn assert_stale_delivery_ingress_is_read_only(
    control_plane: &mut ControlPlane,
    storage: &mut SqliteStorage,
    repository_scope: &RepositoryScope,
    pending: &PendingDeliveryExecution,
    seed: u64,
    sessions: &[&WorkerSessionId],
    message: &ExecutionPortMessage,
) {
    let before = delivery_authority_bytes(storage, pending, seed, sessions);
    let result = DurableExecutionPortIngress::new(
        control_plane,
        storage,
        repository_scope,
        attempt_time(2, 9),
    )
    .expect("Delivery durable ingress")
    .handle(message);
    if let Ok(output) = result {
        assert!(
            output.iter().all(|message| match message {
                ExecutionPortMessage::JobOutcomeAckMessage(ack) => !matches!(
                    ack.status,
                    JobOutcomeAckMessageStatus::Accepted | JobOutcomeAckMessageStatus::Duplicate
                ),
                ExecutionPortMessage::RuntimeAckMessage(ack) => !matches!(
                    ack.status,
                    LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
                ),
                _ => false,
            }),
            "stale predecessor Delivery ingress returned success"
        );
    }
    assert_eq!(
        delivery_authority_bytes(storage, pending, seed, sessions),
        before,
        "rejected predecessor changed Delivery, queue, Registry, or slot bytes"
    );
}

fn assert_delivery_predecessor_is_fenced(
    root: &Path,
    control_plane: &mut ControlPlane,
    pending: &PendingDeliveryExecution,
    seed: u64,
    old_message: &SessionBindingMessage,
    replacement_message: &SessionBindingMessage,
) {
    let old_session_id = old_message.worker_session_id.clone();
    let replacement_session_id = replacement_message.worker_session_id.clone();
    let sessions = [&old_session_id, &replacement_session_id];
    let repository_scope = scheduler_repository_scope(seed);
    let mut storage = SqliteStorage::open(root).expect("Delivery ingress storage");
    let messages = [
        ExecutionPortMessage::RuntimeEventMessage(stale_delivery_runtime(old_message, seed + 280)),
        ExecutionPortMessage::JobOutcomeMessage(stale_delivery_outcome(old_message, seed + 281)),
        ExecutionPortMessage::JobCancelMessage(stale_delivery_cancel(old_message, seed + 282)),
    ];
    for message in &messages {
        assert_stale_delivery_ingress_is_read_only(
            control_plane,
            &mut storage,
            &repository_scope,
            pending,
            seed,
            &sessions,
            message,
        );
    }
    Box::new(storage)
        .close()
        .expect("Delivery ingress storage close");
}

fn artifact_open_message(seed: u64, binding: &SessionBindingMessage) -> ArtifactOpenMessage {
    ArtifactOpenMessage {
        artifact: ArtifactDescriptor {
            artifact_id: ArtifactId(canonical_id("art", seed)),
            digest: Sha256Digest(
                "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
            ),
            file_name: Some("candidate.json".into()),
            kind: ArtifactKind::Candidate,
            media_type: "application/vnd.winwincode.git-candidate+json".into(),
            size_bytes: 5,
        },
        kind: ArtifactOpenMessageKind::ArtifactOpen,
        lease: binding.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        request_id: RequestId(canonical_id("req", seed + 1)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:02.000Z".into()),
        session_identity: binding.session_identity.clone(),
        worker_session_id: binding.worker_session_id.clone(),
    }
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedDeliveryCatalogEntry<'scope> {
    schema_version: u8,
    repository_scope: &'scope RepositoryScope,
    delivery_id: &'scope DeliveryId,
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

fn seed_delivery_catalog(root: &Path, scope: &RepositoryScope, delivery: &Delivery) {
    let payload = serde_json::to_vec(&SeedDeliveryCatalogEntry {
        schema_version: 1,
        repository_scope: scope,
        delivery_id: delivery.id(),
    })
    .expect("Delivery catalog entry JSON");
    let stream_id = format!(
        "delivery-catalog:{:x}:{}",
        Sha256::digest(serde_json::to_vec(scope).expect("catalog scope JSON")),
        delivery.id().0
    );
    let scope_key = receipt_scope_key(&PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    })
    .expect("catalog receipt scope");
    let actor_key = receipt_actor_key(&PublicEventActor::User {
        id: UserId(canonical_id("usr", 90_000 + delivery.revision())),
    })
    .expect("catalog receipt actor");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload)));
    let mut storage = SqliteStorage::open(root).expect("catalog storage");
    let receipt = storage
        .commit(&StateCommit::new(
            ReceiptIdentity::new(
                actor_key,
                scope_key,
                RequestId(canonical_id("req", 90_000 + delivery.revision())),
            )
            .expect("catalog receipt identity"),
            digest,
            stream_id,
            0,
            payload,
            vec![NewOutboxEvent::internal(
                format!("catalog-seeded:{}", delivery.id().0),
                "delivery.catalog.seeded",
                b"{}".to_vec(),
            )],
        ))
        .expect("seed Delivery catalog");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("mark catalog seed published");
    Box::new(storage).close().expect("catalog storage close");
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
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
        for event in &receipt.events {
            let context = event
                .public_context
                .as_ref()
                .expect("SessionBinding public event context");
            assert_eq!(context.occurred_at(), &message.sent_at);
            assert_eq!(
                context.source(),
                &PublicEventSource::SessionExecutionWorker {
                    worker_id: message.lease.worker_id.clone(),
                    worker_session_id: message.worker_session_id.clone(),
                    lease_id: message.lease.lease_id.clone(),
                    codex_thread_id: message.codex_thread_id.clone(),
                    session_identity: message.session_identity.clone(),
                }
            );
        }
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
    assert_eq!(Some(identity.stage_run_id()), message.stage_run_id.as_ref());
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
fn running_scheduler_replacement_rotates_the_delivery_owner_before_new_binding() {
    let seed = 31;
    let (root, pending) = scheduler_delivery_fixture(seed, "running-owner-replacement");
    let (old_authority, old_message) =
        claim_running_delivery_attempt(&root, &pending, seed, 1, "boot-old");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("old binding Control Plane");
    let old = control_plane
        .commit_delivery_session_binding(&old_message, &old_authority, &old_message.sent_at)
        .expect("old running binding");
    assert_eq!(old.codex_thread_receipt().revision, 4);
    control_plane.shutdown().expect("old binding shutdown");

    let (replacement_authority, replacement_message) =
        claim_replacement_delivery_attempt(&root, &pending, seed, 2, "boot-new");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("replacement binding Control Plane");
    let replacement = control_plane
        .commit_delivery_session_binding(
            &replacement_message,
            &replacement_authority,
            &replacement_message.sent_at,
        )
        .expect("replacement running binding");
    assert_eq!(replacement.worker_session_receipt().revision, 6);
    assert_eq!(replacement.codex_thread_receipt().revision, 7);
    let state = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("replacement Delivery state")
        .expect("replacement Delivery");
    let delivery = Delivery::decode_json(&state.payload).expect("replacement snapshot");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.execution_job_id == pending.job().job_id)
        .expect("replacement binding");
    assert_eq!(binding.attempt, 2);
    assert_eq!(
        binding.worker_session_id.as_ref(),
        Some(&replacement_message.worker_session_id)
    );
    assert_eq!(
        binding.codex_thread_id.as_ref(),
        Some(&replacement_message.codex_thread_id)
    );

    let before_old_replay = state.payload;
    control_plane
        .commit_delivery_session_binding(&old_message, &old_authority, &old_message.sent_at)
        .expect_err("old attempt must remain fenced");
    let after_old_replay = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("state after old replay")
        .expect("Delivery after old replay");
    assert_eq!(after_old_replay.payload, before_old_replay);

    assert_delivery_predecessor_is_fenced(
        &root,
        &mut control_plane,
        &pending,
        seed,
        &old_message,
        &replacement_message,
    );

    control_plane.shutdown().expect("replacement shutdown");
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("replacement replay Control Plane");
    let replay = restarted
        .commit_delivery_session_binding(
            &replacement_message,
            &replacement_authority,
            &Instant("2027-01-15T08:59:59.000Z".into()),
        )
        .expect("replacement receipt-first replay after expiry");
    assert!(replay.worker_session_receipt().idempotent_replay);
    assert!(replay.codex_thread_receipt().idempotent_replay);
    assert_eq!(replay.worker_session_receipt().revision, 6);
    assert_eq!(replay.codex_thread_receipt().revision, 7);
    restarted.shutdown().expect("replacement replay shutdown");
    fs::remove_dir_all(root).expect("replacement fixture release");
}

#[test]
fn accepted_dispatch_without_a_slot_replaces_with_a_fresh_delivery_binding() {
    let seed = 32;
    let (root, pending) = scheduler_delivery_fixture(seed, "accepted-no-slot-replacement");
    let (_, old_message) =
        claim_delivery_attempt_without_slot(&root, &pending, seed, 1, "boot-old-no-slot");
    let (replacement_authority, replacement_message) =
        claim_replacement_delivery_attempt(&root, &pending, seed, 2, "boot-new-clean");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("replacement binding Control Plane");
    let committed = control_plane
        .commit_delivery_session_binding(
            &replacement_message,
            &replacement_authority,
            &replacement_message.sent_at,
        )
        .expect("fresh successor Delivery binding");
    assert!(!committed.worker_session_receipt().idempotent_replay);
    assert_delivery_predecessor_is_fenced(
        &root,
        &mut control_plane,
        &pending,
        seed,
        &old_message,
        &replacement_message,
    );

    control_plane.shutdown().expect("replacement shutdown");
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("replacement replay Control Plane");
    let replay = restarted
        .commit_delivery_session_binding(
            &replacement_message,
            &replacement_authority,
            &Instant("2027-01-15T08:59:59.000Z".into()),
        )
        .expect("expired fresh replacement replay");
    assert!(replay.worker_session_receipt().idempotent_replay);
    assert!(replay.codex_thread_receipt().idempotent_replay);
    restarted.shutdown().expect("replacement replay shutdown");
    fs::remove_dir_all(root).expect("replacement fixture release");
}

#[test]
fn exact_message_replay_returns_both_original_receipts_without_new_writes() {
    let (root, mut control_plane, pending, authority, message) =
        running_fixture(2, "receipt-first-replay");
    let first = control_plane
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
        .expect("first SessionBinding transaction");
    let replay = control_plane
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
fn trusted_server_time_is_checked_only_after_session_binding_receipt_resolution() {
    let (root, mut control_plane, pending, authority, message) =
        running_fixture(202, "trusted-server-time");
    control_plane
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
        .expect("initial SessionBinding transaction");
    control_plane.shutdown().expect("initial shutdown");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart Control Plane");
    let expired_server_time = Instant("2027-01-15T08:06:00.000Z".to_owned());
    let replay = control_plane
        .commit_delivery_session_binding(&message, &authority, &expired_server_time)
        .expect("exact replay survives lease expiry");
    assert!(replay.worker_session_receipt().idempotent_replay);
    assert!(replay.codex_thread_receipt().idempotent_replay);

    let mut changed = message.clone();
    changed.codex_thread_id = CodexThreadId(canonical_id("cdx", 20_200));
    let changed_error = control_plane
        .commit_delivery_session_binding(&changed, &authority, &expired_server_time)
        .expect_err("changed body retains its request conflict after expiry");
    assert!(matches!(
        changed_error,
        DeliverySessionBindingCommitError::Storage(ref source)
            if source.kind() == StorageErrorKind::RequestConflict
    ));

    let mut forged = message.clone();
    forged.message_id = ExecutionMessageId(canonical_id("xmsg", 20_201));
    let expired_error = control_plane
        .commit_delivery_session_binding(&forged, &authority, &expired_server_time)
        .expect_err("first-seen message cannot backdate sentAt after expiry");
    assert!(
        expired_error
            .to_string()
            .contains("Server time is outside its active lease")
    );

    forged.message_id = ExecutionMessageId(canonical_id("xmsg", 20_202));
    let premature_error = control_plane
        .commit_delivery_session_binding(
            &forged,
            &authority,
            &Instant("2027-01-15T07:59:59.000Z".to_owned()),
        )
        .expect_err("first-seen message cannot arrive before lease issuance");
    assert!(
        premature_error
            .to_string()
            .contains("Server time is outside its active lease")
    );

    let state = control_plane
        .load_state(&format!("delivery:{}", pending.delivery().id().0))
        .expect("state read")
        .expect("state");
    assert_eq!(state.revision, 4);
    control_plane.shutdown().expect("restart shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn scheduler_authority_rejects_a_worker_supplied_lease_window_before_writes() {
    let (root, mut control_plane, pending, authority, mut message) =
        running_fixture(3, "foreign-lease-window");
    message.lease.expires_at = Instant("2027-01-15T08:06:00.000Z".into());

    let error = control_plane
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
                    .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
        .commit_delivery_session_binding(&message, &replacement_authority, &message.sent_at)
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
            .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
            .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
        .commit_delivery_session_binding(&message, &authority, &message.sent_at)
        .expect("initial SessionBinding transaction");
    let mut changed_thread = message.clone();
    changed_thread.codex_thread_id = CodexThreadId(canonical_id("cdx", 5_700));
    let mut changed_session = message.clone();
    changed_session.worker_session_id = WorkerSessionId(canonical_id("wsn", 5_700));

    for changed in [changed_thread, changed_session] {
        let error = control_plane
            .commit_delivery_session_binding(&changed, &authority, &changed.sent_at)
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
            .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
                .commit_delivery_session_binding(&changed, &authority, &changed.sent_at)
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
                .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
            .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
                .commit_delivery_session_binding(&message, &authority, &message.sent_at)
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
fn production_artifact_quota_denies_before_catalog_or_object_write() {
    let seed = 1_400;
    let (root, mut control_plane, _pending, authority, binding_message) =
        running_fixture(seed, "artifact-quota-denial");
    control_plane
        .commit_delivery_session_binding(&binding_message, &authority, &binding_message.sent_at)
        .expect("complete SessionBinding");
    let Scope::RepositoryScope(scope) = delivery_advance_command(seed).scope else {
        panic!("fixture must use repository scope");
    };
    let mut quota_storage = SqliteStorage::open(&root).expect("quota configuration storage");
    quota_storage
        .enterprise_quota_ledger()
        .expect("enterprise quota ledger")
        .put_policy(&EnterpriseQuotaPolicy {
            boundary: EnterpriseQuotaBoundary::Organization {
                organization_id: scope.organization_id.clone(),
            },
            revision: 1,
            limits: EnterpriseQuotaLimits {
                storage_bytes: Some(4),
                ..EnterpriseQuotaLimits::default()
            },
        })
        .expect("storage quota policy");
    Box::new(quota_storage)
        .close()
        .expect("quota configuration close");

    let open = artifact_open_message(seed, &binding_message);
    assert!(matches!(
        control_plane.accept_artifact_open(&scope, &open, &authority),
        Err(ArtifactMessageError::EnterpriseQuotaDenied)
    ));
    control_plane.shutdown().expect("shutdown");

    let catalog =
        rusqlite::Connection::open(root.join("artifact-catalog/artifact-catalog.sqlite3"))
            .expect("Artifact catalog inspection");
    let stored: i64 = catalog
        .query_row(
            "SELECT COUNT(*) FROM artifacts WHERE artifact_id = ?1",
            [&open.artifact.artifact_id.0],
            |row| row.get(0),
        )
        .expect("Artifact catalog count");
    assert_eq!(stored, 0);
    catalog.close().expect("Artifact catalog close");
    let mut quota_storage = SqliteStorage::open(&root).expect("quota inspection storage");
    assert!(
        quota_storage
            .enterprise_quota_ledger()
            .expect("enterprise quota ledger")
            .load_reservation(&open.request_id)
            .expect("quota reservation lookup")
            .is_none()
    );
    Box::new(quota_storage)
        .close()
        .expect("quota inspection close");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
#[allow(clippy::too_many_lines)]
fn generated_artifact_messages_use_the_exact_durable_job_and_binding_authority() {
    let seed = 1_401;
    let (root, mut control_plane, _pending, authority, binding_message) =
        running_fixture(seed, "artifact-stream");
    control_plane
        .commit_delivery_session_binding(&binding_message, &authority, &binding_message.sent_at)
        .expect("complete SessionBinding");
    let Scope::RepositoryScope(scope) = delivery_advance_command(seed).scope else {
        panic!("fixture must use repository scope");
    };
    let open = artifact_open_message(seed, &binding_message);
    let artifact_id = open.artifact.artifact_id.clone();
    let digest = open.artifact.digest.clone();
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
    let mut quota_storage = SqliteStorage::open(&root).expect("quota inspection storage");
    let quota_record = quota_storage
        .enterprise_quota_ledger()
        .expect("enterprise quota ledger")
        .load_reservation(&open.request_id)
        .expect("Artifact quota reservation lookup")
        .expect("Artifact quota reservation");
    assert_eq!(quota_record.state, EnterpriseQuotaReservationState::Settled);
    assert_eq!(quota_record.revision, 2);
    Box::new(quota_storage)
        .close()
        .expect("quota inspection close");

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
            candidate_ref: None,
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
    let Scope::RepositoryScope(scope) = delivery_advance_command(seed).scope.clone() else {
        panic!("fixture must use repository scope");
    };
    seed_delivery_catalog(&root, &scope, &initial);
    let mut control_plane = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
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
        .commit_delivery_session_binding(&binding_message, &authority, &binding_message.sent_at)
        .expect("complete SessionBinding");

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
    let final_ack = control_plane
        .accept_artifact_chunk(&scope, &chunk, &authority)
        .expect("artifact.chunk");
    assert_eq!(final_ack.status, LeaseWriteStatus::Accepted);
    assert_eq!(final_ack.ack_sequence, ExecutionAckSequence(1));

    // The final Artifact acknowledgement is not observable until the
    // retention ledger has durably reached Pinned and the stable ref points
    // at the exact candidate commit.  Re-open the canonical store here to
    // exercise the same restart/recovery boundary used by a fresh process.
    let pin = {
        let mut retention_storage =
            SqliteStorage::open(&root).expect("candidate retention storage");
        let pin = {
            let mut retention = retention_storage
                .git_candidate_retention(&repositories)
                .expect("candidate retention");
            retention
                .load_by_artifact(&artifact_id)
                .expect("candidate pin lookup")
                .expect("final Artifact acknowledgement must pin candidate")
        };
        Box::new(retention_storage)
            .close()
            .expect("candidate retention close");
        pin
    };
    assert_eq!(
        pin.state(),
        winwincode_storage::CandidateGitRetentionState::Pinned
    );
    assert_eq!(pin.delivery_id(), initial.id());
    assert_eq!(
        git_text(git(
            &repository,
            &["rev-parse", "--verify", pin.reference_name()]
        )),
        candidate_commit
    );

    let forged_release = CandidateGitReleaseAuthority::delivery_final_without_future_reads(
        initial.id().clone(),
        CandidateGitTerminalOutcome::Delivered,
        Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        Sha256Digest(format!("sha256:{}", "e".repeat(64))),
    )
    .expect("release authority fixture");
    control_plane
        .release_candidate_git_after_delivery_final(&pin, &forged_release)
        .expect_err("release requires a durable Delivery terminal/read-closure receipt");
    assert_eq!(
        git_text(git(
            &repository,
            &["rev-parse", "--verify", pin.reference_name()]
        )),
        candidate_commit
    );

    // Move the source branch away and aggressively prune unreachable objects.
    // The stable retention ref is the only authority that keeps this commit
    // available for later verification/rework checkout.
    git(
        &repository,
        &["update-ref", "refs/heads/main", base_commit.as_str()],
    );
    git(&repository, &["reflog", "expire", "--expire=now", "--all"]);
    git(&repository, &["gc", "--prune=now"]);
    assert_eq!(
        git_text(git(
            &repository,
            &["rev-parse", "--verify", pin.reference_name()]
        )),
        candidate_commit
    );

    // A fresh Control Plane instance must reconcile the pin before accepting
    // an exact final-chunk retry.  The retry is a Duplicate acknowledgement,
    // and its pin receipt remains one exact durable record.
    control_plane
        .shutdown()
        .expect("Control Plane restart boundary");
    let mut control_plane = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .expect("Control Plane restart");
    let replayed_final_ack = control_plane
        .accept_artifact_chunk(&scope, &chunk, &authority)
        .expect("exact final Artifact retry");
    assert_eq!(replayed_final_ack.status, LeaseWriteStatus::Duplicate);
    let replayed_pin = {
        let mut retention_storage =
            SqliteStorage::open(&root).expect("candidate replay retention storage");
        let pin = {
            let mut retention = retention_storage
                .git_candidate_retention(&repositories)
                .expect("candidate replay retention");
            retention
                .load_by_artifact(&artifact_id)
                .expect("candidate replay pin lookup")
                .expect("candidate replay pin")
        };
        Box::new(retention_storage)
            .close()
            .expect("candidate replay retention close");
        pin
    };
    assert_eq!(
        replayed_pin.state(),
        winwincode_storage::CandidateGitRetentionState::Pinned
    );
    assert_eq!(replayed_pin.receipt_digest(), pin.receipt_digest());

    // A moved stable ref is a tamper/conflict, not a new candidate.  The
    // duplicate final frame therefore fails before another acknowledgement is
    // returned and leaves the foreign ref untouched.
    git(
        &repository,
        &["update-ref", pin.reference_name(), base_commit.as_str()],
    );
    let tampered = control_plane
        .accept_artifact_chunk(&scope, &chunk, &authority)
        .expect_err("tampered candidate ref must fail closed");
    assert!(matches!(tampered, ArtifactMessageError::Storage(_)));
    assert_eq!(
        git_text(git(
            &repository,
            &["rev-parse", "--verify", pin.reference_name()]
        )),
        base_commit
    );
    git(
        &repository,
        &[
            "update-ref",
            pin.reference_name(),
            candidate_commit.as_str(),
        ],
    );

    let active = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("Delivery state")
        .map(|state| Delivery::decode_json(&state.payload).expect("active Delivery"))
        .expect("active Delivery exists");
    let terminal_metadata = terminal_outcome_metadata(
        Some(binding_message.codex_thread_id.clone()),
        1_800_000_001_000,
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
    let _verified = verify_terminal_outcome(&active, authority.active_lease(), terminal.clone())
        .expect("successful executor outcome");
    let terminal_facts = delivery_terminal_outcome_facts(authority.clone(), terminal);
    let outcome_message = JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: binding_message.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 3)),
        outcome: ExecutionOutcome {
            artifacts: vec![ArtifactReference {
                artifact_id: artifact_id.clone(),
                digest: digest.clone(),
            }],
            codex_thread_id: Some(binding_message.codex_thread_id.clone()),
            error: None,
            finished_at: Instant("2027-01-15T08:00:01.000Z".into()),
            last_event_sequence: ExecutionAckSequence(12),
            status: ExecutionOutcomeStatus::Succeeded,
            summary: "executor completed".into(),
            usage: Some(ExecutionOutcomeUsage {
                cost_microunits: 100,
                runtime_millis: 60_000,
                tokens: 20,
            }),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.200Z".into()),
        session_identity: binding_message.session_identity.clone(),
        worker_session_id: binding_message.worker_session_id.clone(),
    };
    control_plane
        .commit_delivery_terminal_outcome(
            &scope,
            &outcome_message,
            &terminal_facts,
            &outcome_message.sent_at,
        )
        .expect("persist successful executor handoff");
    let next_request_id = RequestId(canonical_id("req", seed + 3));
    let next_command = winwincode_api::generated::DeliveryAdvanceCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: winwincode_api::generated::DeliveryAdvanceCommandCommand::DeliveryAdvance,
        expected_revision: Revision(i64::try_from(active.revision()).expect("revision")),
        payload: winwincode_api::generated::DeliveryAdvancePayload {
            delivery_id: initial.id().clone(),
        },
        request_id: next_request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    };
    control_plane
        .delivery_advance(&next_command)
        .expect("production authority must resolve the exact frozen candidate");
    let verifying_state = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("Verifying state")
        .expect("Verifying Delivery");
    let verifying = Delivery::decode_json(&verifying_state.payload).expect("Verifying Delivery");
    assert_eq!(verifying.snapshot().status, DeliveryStatus::Verifying);
    let verification_job = latest_queued_delivery_job(&root);
    let (verification_authority, verification_binding) = lease_and_message_for_job_at(
        &verification_job,
        seed + 5,
        "2027-01-15T08:00:01.001Z",
        "2027-01-15T08:00:01.000Z",
        "2027-01-15T08:00:01.002Z",
    );
    control_plane
        .commit_delivery_session_binding(
            &verification_binding,
            &verification_authority,
            &verification_binding.sent_at,
        )
        .expect("bind the real verification Worker session");
    let verifying = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("Verifying state after binding")
        .map(|state| {
            Delivery::decode_json(&state.payload).expect("Verifying Delivery after binding")
        })
        .expect("Verifying Delivery after binding");
    let verification_stage_run_id = match &verification_job.scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => scope.stage_run_id.clone(),
        ExecutionScope::ProductSessionExecutionScope(_) => {
            panic!("verification Job must use a Delivery StageRun")
        }
    };
    let candidate_ref = control_plane
        .resolve_delivery_candidate(&scope, initial.id(), &artifact_id, &digest, &terminal_facts)
        .expect("freeze the candidate before verification")
        .candidate_ref()
        .to_owned();
    let verification_artifact_id = ArtifactId(canonical_id("art", seed + 50));
    accept_verification_artifact(
        &mut control_plane,
        &scope,
        &verification_binding,
        &verification_authority,
        verification_artifact_id.clone(),
        digest.clone(),
        &manifest,
        seed + 50,
    );
    accept_runtime_events(RuntimeEventBatch {
        control_plane: &mut control_plane,
        scope: &scope,
        binding: &verification_binding,
        authority: &verification_authority,
        seed: seed + 5,
        candidate_ref: &candidate_ref,
        delivery_spec_id: &initial.snapshot().spec.id.0,
        delivery_spec_revision: initial.snapshot().spec.revision,
        criterion_id: &initial.snapshot().spec.acceptance_criteria[0].id.0,
        finding_id: "finding-reviewer",
        occurred_at: "2027-01-15T08:00:01.001Z",
        sent_at: "2027-01-15T08:00:01.002Z",
    });
    let verification_metadata = terminal_outcome_metadata(
        Some(verification_binding.codex_thread_id.clone()),
        1_800_000_001_002,
        ExecutionAckSequence(12),
        vec![TerminalArtifactReference {
            artifact_id: verification_artifact_id.clone(),
            digest: digest.clone(),
        }],
    );
    let verification_terminal = terminal_worker_outcome(
        verification_stage_run_id,
        verification_job.job_id.clone(),
        1,
        verification_binding.lease.lease_id.clone(),
        verification_binding.lease.fencing_token.clone(),
        verification_binding.lease.worker_id.clone(),
        verification_binding.lease.worker_instance_id.clone(),
        verification_binding.worker_session_id.clone(),
        TerminalOutcomeStatus::Succeeded,
        verification_metadata,
    );
    verify_terminal_outcome(
        &verifying,
        verification_authority.active_lease(),
        verification_terminal.clone(),
    )
    .expect("successful verification Worker outcome");
    let verification_facts =
        delivery_terminal_outcome_facts(verification_authority.clone(), verification_terminal);
    let verification_outcome = JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: verification_binding.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 5)),
        outcome: ExecutionOutcome {
            artifacts: vec![ArtifactReference {
                artifact_id: verification_artifact_id.clone(),
                digest: digest.clone(),
            }],
            codex_thread_id: Some(verification_binding.codex_thread_id.clone()),
            error: None,
            finished_at: Instant("2027-01-15T08:00:01.002Z".into()),
            last_event_sequence: ExecutionAckSequence(12),
            status: ExecutionOutcomeStatus::Succeeded,
            summary: "verification completed".into(),
            usage: Some(ExecutionOutcomeUsage {
                cost_microunits: 100,
                runtime_millis: 60_000,
                tokens: 20,
            }),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.003Z".into()),
        session_identity: verification_binding.session_identity.clone(),
        worker_session_id: verification_binding.worker_session_id.clone(),
    };
    control_plane
        .commit_delivery_terminal_outcome(
            &scope,
            &verification_outcome,
            &verification_facts,
            &verification_outcome.sent_at,
        )
        .expect("persist successful verification outcome");
    let review_advance = winwincode_api::generated::DeliveryAdvanceCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: winwincode_api::generated::DeliveryAdvanceCommandCommand::DeliveryAdvance,
        expected_revision: Revision(i64::try_from(verifying.revision()).expect("revision")),
        payload: winwincode_api::generated::DeliveryAdvancePayload {
            delivery_id: initial.id().clone(),
        },
        request_id: RequestId(canonical_id("req", seed + 6)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    };
    control_plane
        .delivery_advance(&review_advance)
        .expect("advance successful verification into Delivery review");
    let second_verifying_state = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("second Verifying state")
        .expect("second Verifying Delivery");
    let second_verifying =
        Delivery::decode_json(&second_verifying_state.payload).expect("second Verifying Delivery");
    assert_eq!(
        second_verifying.snapshot().status,
        DeliveryStatus::Verifying
    );
    let second_verification_job = latest_queued_delivery_job(&root);
    let (second_verification_authority, second_verification_binding) = lease_and_message_for_job_at(
        &second_verification_job,
        seed + 1_000,
        "2027-01-15T08:00:01.002Z",
        "2027-01-15T08:00:01.001Z",
        "2027-01-15T08:00:01.003Z",
    );
    control_plane
        .commit_delivery_session_binding(
            &second_verification_binding,
            &second_verification_authority,
            &second_verification_binding.sent_at,
        )
        .expect("bind the second verification Worker session");
    let second_verifying = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("second Verifying state after binding")
        .map(|state| {
            Delivery::decode_json(&state.payload).expect("second Verifying Delivery after binding")
        })
        .expect("second Verifying Delivery after binding");
    let second_stage_run_id = match &second_verification_job.scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => scope.stage_run_id.clone(),
        ExecutionScope::ProductSessionExecutionScope(_) => {
            panic!("second verification Job must use a Delivery StageRun")
        }
    };
    accept_runtime_events(RuntimeEventBatch {
        control_plane: &mut control_plane,
        scope: &scope,
        binding: &second_verification_binding,
        authority: &second_verification_authority,
        seed: seed + 1_000,
        candidate_ref: &candidate_ref,
        delivery_spec_id: &initial.snapshot().spec.id.0,
        delivery_spec_revision: initial.snapshot().spec.revision,
        criterion_id: &initial.snapshot().spec.acceptance_criteria[0].id.0,
        finding_id: "finding-verifier",
        occurred_at: "2027-01-15T08:00:01.002Z",
        sent_at: "2027-01-15T08:00:01.003Z",
    });
    let second_verification_artifact_id = ArtifactId(canonical_id("art", seed + 60));
    accept_verification_artifact(
        &mut control_plane,
        &scope,
        &second_verification_binding,
        &second_verification_authority,
        second_verification_artifact_id.clone(),
        digest.clone(),
        &manifest,
        seed + 60,
    );
    let second_verification_terminal = terminal_worker_outcome(
        second_stage_run_id,
        second_verification_job.job_id.clone(),
        1,
        second_verification_binding.lease.lease_id.clone(),
        second_verification_binding.lease.fencing_token.clone(),
        second_verification_binding.lease.worker_id.clone(),
        second_verification_binding.lease.worker_instance_id.clone(),
        second_verification_binding.worker_session_id.clone(),
        TerminalOutcomeStatus::Succeeded,
        terminal_outcome_metadata(
            Some(second_verification_binding.codex_thread_id.clone()),
            1_800_000_001_003,
            ExecutionAckSequence(12),
            vec![TerminalArtifactReference {
                artifact_id: second_verification_artifact_id.clone(),
                digest: digest.clone(),
            }],
        ),
    );
    verify_terminal_outcome(
        &second_verifying,
        second_verification_authority.active_lease(),
        second_verification_terminal.clone(),
    )
    .expect("second successful verification Worker outcome");
    let second_verification_facts = delivery_terminal_outcome_facts(
        second_verification_authority.clone(),
        second_verification_terminal,
    );
    let second_verification_outcome = JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: second_verification_binding.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1_001)),
        outcome: ExecutionOutcome {
            artifacts: vec![ArtifactReference {
                artifact_id: second_verification_artifact_id.clone(),
                digest: digest.clone(),
            }],
            codex_thread_id: Some(second_verification_binding.codex_thread_id.clone()),
            error: None,
            finished_at: Instant("2027-01-15T08:00:01.003Z".into()),
            last_event_sequence: ExecutionAckSequence(12),
            status: ExecutionOutcomeStatus::Succeeded,
            summary: "second verification completed".into(),
            usage: Some(ExecutionOutcomeUsage {
                cost_microunits: 100,
                runtime_millis: 60_000,
                tokens: 20,
            }),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.004Z".into()),
        session_identity: second_verification_binding.session_identity.clone(),
        worker_session_id: second_verification_binding.worker_session_id.clone(),
    };
    control_plane
        .commit_delivery_terminal_outcome(
            &scope,
            &second_verification_outcome,
            &second_verification_facts,
            &second_verification_outcome.sent_at,
        )
        .expect("persist second successful verification outcome");
    let current_verdict_state = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("current Delivery state")
        .expect("current Delivery state exists");
    let submit_verdict = DeliverySubmitVerdictCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: DeliverySubmitVerdictCommandCommand::DeliverySubmitVerdict,
        expected_revision: Revision(
            i64::try_from(current_verdict_state.revision).expect("verdict revision"),
        ),
        payload: DeliverySubmitVerdictPayload {
            candidate_digest: Sha256Digest(
                candidate_ref
                    .strip_prefix("git-candidate:")
                    .expect("candidate ref prefix")
                    .to_owned(),
            ),
            delivery_id: initial.id().clone(),
        },
        request_id: RequestId(canonical_id("req", seed + 9)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    };
    control_plane
        .delivery_submit_verdict(&submit_verdict)
        .expect("compute and persist the production Delivery verdict");
    let verdict_state = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("Delivery state after verdict")
        .expect("Delivery state after verdict exists");
    let verdict_delivery =
        Delivery::decode_json(&verdict_state.payload).expect("Delivery after verdict");
    let delivery_review_advance = winwincode_api::generated::DeliveryAdvanceCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: winwincode_api::generated::DeliveryAdvanceCommandCommand::DeliveryAdvance,
        expected_revision: Revision(
            i64::try_from(verdict_delivery.revision()).expect("second review revision"),
        ),
        payload: winwincode_api::generated::DeliveryAdvancePayload {
            delivery_id: initial.id().clone(),
        },
        request_id: RequestId(canonical_id("req", seed + 8)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    };
    control_plane
        .delivery_advance(&delivery_review_advance)
        .expect("advance all successful verification into Delivery review");
    let candidate = control_plane
        .resolve_delivery_candidate(&scope, initial.id(), &artifact_id, &digest, &terminal_facts)
        .expect("candidate resolution from the pinned Artifact and canonical Git root");
    assert_eq!(candidate.base_commit_id(), base_commit);
    assert_eq!(candidate.candidate_commit_id(), candidate_commit);
    assert_eq!(candidate.producer_artifact_ref(), artifact_id.0);
    let replayed_open = control_plane
        .accept_artifact_open(&scope, &open, &authority)
        .expect("settled StageRun must still replay its durable artifact.open acknowledgement");
    assert_eq!(replayed_open.status, LeaseWriteStatus::Duplicate);
    let replayed_chunk = control_plane
        .accept_artifact_chunk(&scope, &chunk, &authority)
        .expect("settled StageRun must still replay its durable artifact.chunk acknowledgement");
    assert_eq!(replayed_chunk.status, LeaseWriteStatus::Duplicate);

    // The production Delivery authority now closes its review Attention.  We
    // recover the exact generic command receipt that was committed by this
    // typed API and use it as the only terminal fact for candidate release.
    let review_state = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("Delivery review state");
    let review = review_state
        .as_ref()
        .map(|state| Delivery::decode_json(&state.payload).expect("review Delivery"))
        .expect("Delivery review exists");
    assert_eq!(review.snapshot().status, DeliveryStatus::NeedsAttention);
    let review_attention = review
        .snapshot()
        .attention_items
        .iter()
        .find(|item| item.status == winwincode_delivery::domain::AttentionItemStatus::Open)
        .expect("open Delivery review Attention");
    let resolve_attention = DeliveryResolveAttentionCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: DeliveryResolveAttentionCommandCommand::DeliveryResolveAttention,
        expected_revision: Revision(
            i64::try_from(review.revision()).expect("review revision in public range"),
        ),
        payload: DeliveryResolveAttentionPayload {
            attention_item_id: review_attention.id.clone(),
            decision: "resolve".into(),
            delivery_id: initial.id().clone(),
            remediation: None,
            resolution: "Approve the exact frozen candidate after all readers close.".into(),
        },
        request_id: RequestId(canonical_id("req", seed + 4)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    };
    control_plane
        .delivery_resolve_attention(&resolve_attention)
        .expect("production Delivery review resolution");
    let delivered_state = control_plane
        .load_state(&format!("delivery:{}", initial.id().0))
        .expect("Delivered state")
        .expect("Delivered Delivery");
    let delivered = Delivery::decode_json(&delivered_state.payload).expect("Delivered Delivery");
    assert_eq!(delivered.snapshot().status, DeliveryStatus::Delivered);
    let terminal_command = CommandEnvelope {
        actor: resolve_attention.actor.clone(),
        command: CommandName::DeliveryResolveAttention,
        expected_revision: resolve_attention.expected_revision.clone(),
        payload: serde_json::to_value(&resolve_attention.payload).expect("resolve payload"),
        request_id: resolve_attention.request_id.clone(),
        schema_version: resolve_attention.schema_version.clone(),
        scope: Scope::RepositoryScope(resolve_attention.scope.clone()),
    };
    let terminal_identity = ReceiptIdentity::new(
        receipt_actor_key(&PublicEventActor::User {
            id: UserId(canonical_id("usr", seed)),
        })
        .expect("terminal receipt actor"),
        receipt_scope_key(&PublicEventScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        })
        .expect("terminal receipt scope"),
        terminal_command.request_id.clone(),
    )
    .expect("terminal receipt identity");
    let terminal_digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&terminal_command).expect("terminal command"))
    ));
    let terminal_receipt = {
        let storage = SqliteStorage::open(&root).expect("terminal receipt storage");
        let receipt = storage
            .load_receipt(&terminal_identity, &terminal_digest)
            .expect("terminal receipt lookup")
            .expect("durable Delivered terminal receipt");
        Box::new(storage)
            .close()
            .expect("terminal receipt storage close");
        receipt
    };
    let reads_closed = control_plane
        .commit_candidate_git_reads_closed(
            initial.id(),
            &terminal_receipt,
            CandidateGitTerminalOutcome::Delivered,
        )
        .expect("durable candidate read closure");
    assert_eq!(reads_closed.delivery_id(), initial.id());
    assert_eq!(reads_closed.delivery_revision(), delivered.revision());
    let release = control_plane
        .release_candidate_git_after_delivery_reads_closed(&pin, &reads_closed)
        .expect("receipt-first candidate release");
    assert_eq!(
        release.state(),
        winwincode_storage::CandidateGitRetentionState::Released
    );
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["rev-parse", "--verify", pin.reference_name()])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("released reference lookup")
            .success()
    );
    for verification_artifact_id in [&verification_artifact_id, &second_verification_artifact_id] {
        let verification_pin = load_candidate_pin(&root, &repositories, verification_artifact_id);
        let verification_release = control_plane
            .release_candidate_git_after_delivery_reads_closed(&verification_pin, &reads_closed)
            .expect("receipt-first verification candidate release");
        assert_eq!(
            verification_release.state(),
            winwincode_storage::CandidateGitRetentionState::Released
        );
        assert!(
            !Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(["rev-parse", "--verify", verification_pin.reference_name()])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .expect("released verification reference lookup")
                .success()
        );
        let replay = control_plane
            .release_candidate_git_after_delivery_reads_closed(&verification_pin, &reads_closed)
            .expect("exact verification release retry");
        assert!(replay.is_idempotent_replay());
        assert_eq!(
            replay.receipt_digest(),
            verification_release.receipt_digest()
        );
    }
    let reads_closed_replay = control_plane
        .commit_candidate_git_reads_closed(
            initial.id(),
            &terminal_receipt,
            CandidateGitTerminalOutcome::Delivered,
        )
        .expect("exact read-closure retry");
    assert_eq!(reads_closed_replay, reads_closed);
    let release_replay = control_plane
        .release_candidate_git_after_delivery_reads_closed(&pin, &reads_closed_replay)
        .expect("exact release retry");
    assert!(release_replay.is_idempotent_replay());
    assert_eq!(release_replay.receipt_digest(), release.receipt_digest());
    let mut new_open_after_settlement = open.clone();
    new_open_after_settlement.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 40));
    new_open_after_settlement.request_id = RequestId(canonical_id("req", seed + 40));
    new_open_after_settlement.artifact.artifact_id = ArtifactId(canonical_id("art", seed + 40));
    control_plane
        .accept_artifact_open(&scope, &new_open_after_settlement, &authority)
        .expect_err(
            "settled StageRun may replay durable messages but cannot create a new Artifact",
        );
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
