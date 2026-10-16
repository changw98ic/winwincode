use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

#[path = "../../../tests/support/git_candidate.rs"]
mod git_candidate;

#[path = "support/persisted_rework_rejection.rs"]
mod persisted_rework_rejection;
use git_candidate::candidate_bundle;

use sha2::{Digest, Sha256};
use winwincode_api::generated::{Actor, CommandEnvelope, CommandName, Scope};
use winwincode_audit::{AuditEvent, AuditExecutionSubjectKind, AuditScope};
use winwincode_control_plane::{
    CommitError, ControlPlane, ControlPlaneConfig, DeliveryTerminalOutcomeCommitError,
    DurableExecutionPortError, DurableExecutionPortIngress, EventPublishError, EventPublisher,
    ExecutionPortService, LocalDeliveryAdapterConfig, OutboxEvent, StateChange,
};
use winwincode_delivery::{
    application::{
        verdict::{
            SubmitVerdictFacts,
            test_support::{VerdictFixtureOutcome, verdict_facts_fixture, verdict_fixture},
        },
        workrun_execution::test_support::{
            active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
            terminal_outcome_metadata, terminal_worker_outcome,
        },
    },
    domain::{Delivery, DeliveryStatus},
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion, SessionIdentity,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, UserActor};
use winwincode_execution_port::generated::{
    ArtifactReference, ExecutionJob, ExecutionLeaseStamp, ExecutionLimits, ExecutionOutcome,
    ExecutionOutcomeStatus, ExecutionOutcomeUsage, ExecutionPortError, ExecutionPortErrorCode,
    ExecutionPortMessage, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
    JobDispatchResultMessage, JobDispatchResultMessageKind, JobDispatchResultMessageStatus,
    JobOutcomeAckMessageStatus, JobOutcomeMessage, JobOutcomeMessageKind, WorkRunExecutionScope,
    WorkRunExecutionScopeKind,
};
use winwincode_storage::PublicEventSource;
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactChunk,
    ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance, ArtifactRetention,
    ArtifactStore, AuthenticatedWorkerPlacement, EXECUTION_PROTOCOL_VERSION,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionJobState, ExecutionJobSubmission, ExecutionJobTransitionRequest, ExecutionLeaseClaim,
    ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRequest,
    ExecutionReservationStart, GitCandidateArtifactManifest, LocalArtifactObjectStore,
    NewOutboxEvent, ProductStateStorage, PublicEventActor, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, StateCommit, WorkerAuthenticationIdentity, WorkerHeartbeatRequest,
    WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest, WorkerRegistryScope,
    WorkerSlotAuthority, WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources,
};

use winwincode_storage::{
    CommitReceipt, DurableOutboxEvent, LoadedAggregateJournal, PendingAuditEvent,
    ProjectionEventCursor, ProjectionEventStreamKey, ProjectionReadCut, StorageError, StoredState,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-terminal-outcome-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
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

#[derive(Default)]
struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedDeliveryCatalogEntry<'entry> {
    schema_version: u8,
    repository_scope: &'entry RepositoryScope,
    delivery_id: &'entry DeliveryId,
}

struct FailingPublisher;

impl EventPublisher for FailingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Err(EventPublishError::new(
            "injected terminal publication failure",
        ))
    }
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: winwincode_domain::RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
    }
}

fn running_final_verifier(
    seed: u64,
) -> (
    Delivery,
    winwincode_delivery::domain::FrozenDeliveryCandidate,
) {
    let fixture = verdict_fixture(
        &DeliveryId(canonical_id("dlv", seed)),
        VerdictFixtureOutcome::Pass,
    );
    let mut snapshot = fixture.delivery.into_snapshot();
    let binding = snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.id.0 == "binding-verifier-1")
        .expect("final verifier binding");
    let previous_work_run_id = binding.work_run_id.clone();
    binding.work_run_id = winwincode_domain::WorkRunId(canonical_id("wrn", seed));
    binding.product_session_id = ProductSessionId(canonical_id("psn", seed));
    binding.execution_job_id = ExecutionJobId(canonical_id("job", seed));
    binding.worker_session_id = Some(WorkerSessionId(canonical_id("wsn", seed)));
    binding.codex_thread_id = Some(CodexThreadId(canonical_id("cdx", seed)));
    binding.worker_id = Some(WorkerId(canonical_id("wrk", seed)));
    binding
        .runtime_context
        .as_mut()
        .expect("verifier runtime context")
        .agent_identity
        .worker_id = binding.worker_id.clone().expect("Worker");
    binding.worker_instance_id = Some(WorkerInstanceId(canonical_id("wki", seed)));
    binding.lease_id = Some(LeaseId(canonical_id("lse", seed)));
    binding.fencing_token = Some(FencingToken(seed.to_string()));
    let accepted = snapshot
        .work_run_aggregate
        .runs
        .iter_mut()
        .find(|run| run.id == previous_work_run_id)
        .expect("canonical verifier WorkRun");
    accepted.id = binding.work_run_id.clone();
    accepted.execution_job_id = binding.execution_job_id.clone();
    accepted.product_session_id = Some(binding.product_session_id.clone());
    accepted.worker_session_id = binding.worker_session_id.clone().unwrap();
    accepted
        .codex_thread_id
        .clone_from(&binding.codex_thread_id);
    accepted.worker_id = binding.worker_id.clone().unwrap();
    accepted.worker_instance_id = binding.worker_instance_id.clone().unwrap();
    accepted.lease_id = binding.lease_id.clone().unwrap();
    accepted
        .fencing_token
        .clone_from(&binding.fencing_token.as_ref().unwrap().0);
    accepted.state = winwincode_domain::WorkRunState::Running;
    snapshot
        .work_run_aggregate
        .items
        .iter_mut()
        .find(|item| item.id == accepted.work_item_id)
        .unwrap()
        .state = winwincode_domain::WorkItemState::CandidateReady;

    let delivery = Delivery::try_from_snapshot(snapshot).expect("running final verifier Delivery");
    (delivery, fixture.candidate)
}

fn running_non_final_executor(seed: u64) -> Delivery {
    let (delivery, _candidate) = running_final_verifier(seed);
    let mut snapshot = delivery.into_snapshot();
    snapshot.status = DeliveryStatus::Ready;
    snapshot
        .work_run_aggregate
        .runs
        .retain(|run| run.execution_job_id.0 == canonical_id("job", seed));
    snapshot
        .session_bindings
        .retain(|binding| binding.execution_job_id.0 == canonical_id("job", seed));
    for item in &mut snapshot.work_run_aggregate.items {
        item.state = winwincode_domain::WorkItemState::InProgress;
    }
    let executor_binding = snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.execution_job_id.0 == canonical_id("job", seed))
        .expect("executor binding");
    executor_binding.execution_profile = Some("executor".into());
    executor_binding
        .runtime_context
        .as_mut()
        .expect("executor runtime context")
        .agent_identity
        .role = "executor".into();

    let item_id = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.execution_job_id.0 == canonical_id("job", seed))
        .unwrap()
        .work_item_id
        .clone();
    snapshot
        .work_run_aggregate
        .items
        .iter_mut()
        .find(|item| item.id == item_id)
        .unwrap()
        .state = winwincode_domain::WorkItemState::InProgress;
    Delivery::try_from_snapshot(snapshot).expect("running non-final executor")
}

fn execution_job(delivery: &Delivery, scope: &RepositoryScope) -> ExecutionJob {
    let run = delivery
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.state == winwincode_domain::WorkRunState::Running)
        .expect("canonical active run");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.work_run_id == run.id)
        .expect("exact active binding");
    ExecutionJob {
        attachments: None,
        model_selection: None,
        attempt: 1,
        execution_profile: binding.execution_profile.clone().expect("accepted profile"),
        goal: delivery
            .snapshot()
            .work_run_aggregate
            .items
            .iter()
            .find(|item| item.id == binding.work_item_id)
            .unwrap()
            .goal
            .clone(),
        job_id: binding.execution_job_id.clone(),
        limits: ExecutionLimits {
            deadline_at: Some(Instant("2027-01-15T09:00:00.000Z".into())),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: Some(3_600),
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::WorkRunExecutionScope(WorkRunExecutionScope {
            attempt: 1,
            kind: WorkRunExecutionScopeKind::WorkRun,
            product_session_id: binding.product_session_id.clone(),
            rework_authorization: None,
            work_contract_id: binding.work_contract_id.clone(),
            work_contract_revision: binding.work_contract_revision.clone(),
            work_item_id: binding.work_item_id.clone(),
            work_item_revision: binding.work_item_revision.clone(),
            work_run_id: binding.work_run_id.clone(),
        }),
        work_input: Some(winwincode_execution_port::generated::WorkRunInput {
            work_plan: None,
            device_target: None,
            delivery_spec_id: delivery.snapshot().spec.id.0.clone(),
            delivery_spec_revision: Revision(
                i64::try_from(delivery.snapshot().spec.revision)
                    .expect("fixture spec revision fits wire range"),
            ),
            candidate_ref: (binding.execution_profile.as_deref() == Some("verifier")).then(|| {
                verdict_fixture(delivery.id(), VerdictFixtureOutcome::Pass)
                    .candidate
                    .candidate_ref()
                    .to_owned()
            }),
            schema_version: SchemaVersion::WinwincodeV1,
            snapshot_id: None,
            work_contract: delivery.snapshot().work_run_aggregate.contract.clone(),
            work_item: delivery
                .snapshot()
                .work_run_aggregate
                .items
                .iter()
                .find(|item| item.id == binding.work_item_id)
                .unwrap()
                .clone(),
        }),
        workspace: ExecutionWorkspace {
            checkout_revision: if binding.execution_profile.as_deref() == Some("verifier") {
                verdict_fixture(delivery.id(), VerdictFixtureOutcome::Pass)
                    .candidate
                    .candidate_commit_id()
                    .to_owned()
            } else {
                "candidate-checkout".into()
            },
            repository_id: scope.repository_id.clone(),
            write_mode: if binding.execution_profile.as_deref() == Some("executor") {
                ExecutionWorkspaceWriteMode::Candidate
            } else {
                ExecutionWorkspaceWriteMode::ReadOnly
            },
        },
    }
}

fn seed_delivery_and_job(root: &Path, delivery: &Delivery, job: &ExecutionJob) {
    let seed = job
        .job_id
        .0
        .strip_prefix("job_")
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .expect("fixture job suffix");
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("seed-terminal-journal".into()),
            request_digest: "b".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed journal publication");
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
    let repository_scope = repository_scope(
        job.workspace
            .repository_id
            .0
            .strip_prefix("rep_")
            .and_then(|suffix| suffix.parse::<u64>().ok())
            .expect("fixture repository suffix"),
    );
    let scope_key = repository_receipt_scope(&repository_scope);
    let catalog_scope = serde_json::to_vec(&repository_scope).expect("catalog scope JSON");
    let catalog_stream = format!(
        "delivery-catalog:{:x}:{}",
        Sha256::digest(catalog_scope),
        delivery.id().0
    );
    let catalog_payload = serde_json::to_vec(&SeedDeliveryCatalogEntry {
        schema_version: 1,
        repository_scope: &repository_scope,
        delivery_id: delivery.id(),
    })
    .expect("catalog entry JSON");
    let mut storage = SqliteStorage::open(root).expect("seed storage");
    let receipt = storage
        .commit(
            &StateCommit::new(
                ReceiptIdentity::new(
                    winwincode_storage::receipt_actor_key(&PublicEventActor::User {
                        id: UserId(canonical_id("usr", seed)),
                    })
                    .expect("actor key"),
                    scope_key,
                    RequestId(canonical_id("req", seed + 5_000)),
                )
                .expect("receipt identity"),
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    format!("execution-job:{}", job.job_id.0),
                    "execution.job.dispatch",
                    serde_json::to_vec(job).expect("ExecutionJob JSON"),
                )],
            )
            .with_journal_publication(publication)
            .with_state_mutation(
                winwincode_storage::StateMutation::new(catalog_stream, 0, catalog_payload)
                    .expect("catalog mutation"),
            ),
        )
        .expect("seed Delivery and ExecutionJob");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("seed event acknowledgement");
    if job.execution_profile == "verifier" {
        seed_verification_snapshot(&mut storage, &repository_scope, delivery, job, seed);
    }
    Box::new(storage).close().expect("seed close");
    let message = terminal_message(job, delivery, seed, ExecutionOutcomeStatus::Succeeded);
    seed_authenticated_worker_execution(root, &repository_scope, job, &message, seed);
}

#[allow(clippy::too_many_lines)]
fn seed_verification_snapshot(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    delivery: &Delivery,
    job: &ExecutionJob,
    seed: u64,
) {
    use serde_json::json;
    use winwincode_domain::{Candidate, CanonicalSnapshot, seal_snapshot};
    use winwincode_execution_port::generated::SnapshotFreezeRequestMessage;
    use winwincode_storage::{
        SnapshotProductStaged, SnapshotVerificationBinding, commit_snapshot_product,
    };

    let fixture = verdict_fixture(delivery.id(), VerdictFixtureOutcome::Pass);
    let frozen = &fixture.candidate;
    let writer = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.work_run_id == *frozen.producer_work_run_id())
        .unwrap();
    let candidate: Candidate = serde_json::from_value(json!({
        "schemaVersion": "winwincode/v1", "id": frozen.candidate_id(),
        "candidateDigest": frozen.candidate_digest(), "candidateRef": frozen.candidate_ref(),
        "workContractId": writer.work_contract_id, "contractRevision": writer.work_contract_revision,
        "workItemId": writer.work_item_id, "workRunId": writer.work_run_id, "attempt": writer.attempt,
        "producerWorkerSessionId": writer.worker_session_id,
        "baseCommit": frozen.base_commit_id(), "candidateCommit": frozen.candidate_commit_id(),
        "candidateTree": frozen.candidate_tree_id(), "diffDigest": format!("sha256:{}", frozen.diff_sha256())
    })).unwrap();
    let mut snapshot = fixture.verification.snapshot().as_contract().clone();
    snapshot.repository_id = scope.repository_id.clone();
    snapshot.validation_seal = seal_snapshot(&snapshot);
    let snapshot = CanonicalSnapshot::try_from(snapshot).unwrap();
    let active = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.execution_job_id == job.job_id)
        .unwrap();
    let templates: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut request = templates["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["kind"] == "snapshot.freeze_request")
        .unwrap()
        .clone();
    request["requestId"] = json!(canonical_id("req", seed + 10_000));
    request["candidate"] = json!(candidate);
    request["repositoryId"] = json!(scope.repository_id);
    request["baseTreeId"] = json!(snapshot.as_contract().base_tree_id);
    request["contentDigest"] = json!(snapshot.as_contract().content_digest);
    request["lease"] = json!({
        "jobId": job.job_id, "attempt": job.attempt, "leaseId": active.lease_id,
        "fencingToken": active.fencing_token, "workerId": active.worker_id,
        "workerInstanceId": active.worker_instance_id,
        "issuedAt": "2027-01-15T08:00:00.200Z", "expiresAt": "2027-01-15T08:05:00.000Z"
    });
    request["dispatch"]["lease"] = request["lease"].clone();
    request["dispatch"]["job"] = json!(job);
    let request: SnapshotFreezeRequestMessage = serde_json::from_value(request).unwrap();
    winwincode_execution_port::snapshot_freeze::validate_freeze_request(&request).unwrap();
    let payload = serde_json::to_vec(&request).unwrap();
    let actor = winwincode_storage::receipt_actor_key(&PublicEventActor::User {
        id: UserId(canonical_id("usr", seed)),
    })
    .unwrap();
    let receipt_scope = repository_receipt_scope(scope);
    let receipt = storage
        .commit(&StateCommit::new(
            ReceiptIdentity::new(
                actor.clone(),
                receipt_scope.clone(),
                request.request_id.clone(),
            )
            .unwrap(),
            Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload))),
            format!("snapshot-freeze:v1:{}:{}", job.job_id.0, job.attempt),
            0,
            payload.clone(),
            vec![NewOutboxEvent::internal(
                format!("snapshot-freeze:{}", request.request_id.0),
                "verification.snapshot.freeze",
                payload,
            )],
        ))
        .unwrap();
    storage.mark_published(&receipt.events[0].event_id).unwrap();
    for (index, binding) in delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            matches!(
                binding.execution_profile.as_deref(),
                Some("reviewer" | "verifier")
            )
        })
        .enumerate()
    {
        let n = seed + 11_000 + u64::try_from(index).unwrap();
        let role = binding.execution_profile.as_ref().unwrap();
        let plan_id = canonical_id("vpl", n);
        let session_id = canonical_id("vsn", n);
        let binding: SnapshotVerificationBinding = serde_json::from_value(json!({
            "sessionBindingId": canonical_id("sbn", n), "workRunId": binding.work_run_id,
            "executionJobId": binding.execution_job_id, "productSessionId": binding.product_session_id,
            "workerSessionId": binding.worker_session_id, "codexThreadId": binding.codex_thread_id,
            "verificationRole": role, "attempt": binding.attempt,
            "verificationPlan": {"schemaVersion": "winwincode/v1", "id": plan_id,
                "candidateDigest": candidate.candidate_digest, "workContractId": binding.work_contract_id,
                "contractRevision": binding.work_contract_revision, "workItemId": binding.work_item_id,
                "workItemRevision": binding.work_item_revision, "workRunId": binding.work_run_id,
                "planRevision": 1, "criterionIds": delivery.snapshot().spec.acceptance_criteria.iter().map(|c| &c.id).collect::<Vec<_>>(),
                "commands": ["cargo test"], "permissionProfile": "candidate-read-only", "requiredRoles": [role]},
            "verificationSession": {"schemaVersion": "winwincode/v1", "id": session_id,
                "verificationSessionId": session_id, "verificationPlanId": plan_id,
                "snapshotId": snapshot.snapshot_id(), "candidateId": candidate.id,
                "workRunId": binding.work_run_id, "attempt": binding.attempt,
                "createdAt": "2027-01-15T08:00:00.200Z",
                "sessionIdentity": {"workRunId": binding.work_run_id, "productSessionId": binding.product_session_id,
                    "workerSessionId": binding.worker_session_id, "codexThreadId": binding.codex_thread_id}}
        })).unwrap();
        let staged =
            SnapshotProductStaged::new(candidate.clone(), snapshot.clone(), binding).unwrap();
        let receipt = commit_snapshot_product(
            storage,
            ReceiptIdentity::new(
                actor.clone(),
                receipt_scope.clone(),
                RequestId(canonical_id("req", n)),
            )
            .unwrap(),
            Sha256Digest(format!("sha256:{n:064x}")),
            &staged,
        )
        .unwrap();
        for event in receipt.receipt.events {
            storage.mark_published(&event.event_id).unwrap();
        }
    }
}

fn seed_authenticated_worker_execution(
    root: &Path,
    scope: &RepositoryScope,
    job: &ExecutionJob,
    message: &JobOutcomeMessage,
    seed: u64,
) {
    let ExecutionScope::WorkRunExecutionScope(job_scope) = &job.scope else {
        panic!("fixture Job must have Delivery scope");
    };
    let pool_id = WorkerPoolId(canonical_id("wpl", seed));
    let mut storage = SqliteStorage::open(root).expect("Worker lifecycle storage");
    seed_worker_execution_admission(&mut storage, scope, job, job_scope, &pool_id, seed);
    seed_authenticated_worker_registration(&mut storage, scope, message, pool_id, seed);
    let claim = ExecutionLeaseClaim {
        expires_at: message.lease.expires_at.clone(),
        fencing_token: message.lease.fencing_token.clone(),
        issued_at: message.lease.issued_at.clone(),
        job_id: message.lease.job_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 9_004)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 9_004)),
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
        attempt: u64::try_from(message.lease.attempt).expect("lease attempt"),
    };
    let dispatch = ExecutionPortService::new(&mut storage, claim.issued_at.clone())
        .claim_execution_job(job.clone(), claim)
        .expect("production dispatch");
    let dispatch_result = JobDispatchResultMessage {
        error: None,
        job_id: dispatch.job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: dispatch.lease,
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 9_007)),
        payload_digest: dispatch.job.payload_digest,
        request_id: dispatch.request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:00.250Z".into()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(message.worker_session_id.clone()),
    };
    let accepted = ExecutionPortService::new(&mut storage, dispatch_result.sent_at.clone())
        .accept_dispatch_result(dispatch_result)
        .expect("accepted production dispatch");
    assert_eq!(accepted.status, JobDispatchResultMessageStatus::Accepted);
    seed_worker_slot(&mut storage, message, seed);
}

fn worker_admission_boundaries(
    scope: &RepositoryScope,
    job_scope: &WorkRunExecutionScope,
    pool_id: &WorkerPoolId,
    delivery_id: &DeliveryId,
) -> Vec<ExecutionAdmissionBoundary> {
    vec![
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
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: job_scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool_id.clone(),
        },
    ]
}

fn seed_worker_execution_admission(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    job: &ExecutionJob,
    job_scope: &WorkRunExecutionScope,
    pool_id: &WorkerPoolId,
    seed: u64,
) {
    let queue_scope = ExecutionQueueScope {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
        delivery_id: Some(DeliveryId(canonical_id("dlv", seed))),
        product_session_id: job_scope.product_session_id.clone(),
    };
    let submitted_at = Instant("2027-01-15T08:00:00.000Z".into());
    let submitted = storage
        .execution_queue()
        .expect("execution queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 9_001)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(job).expect("dispatch payload"),
            attempt: u64::try_from(job.attempt).expect("attempt"),
            dependencies: Vec::new(),
            work_run_id: Some(job_scope.work_run_id.clone()),
            submitted_at: submitted_at.clone(),
        })
        .expect("execution queue submission");
    storage
        .execution_queue()
        .expect("execution queue")
        .transition(&ExecutionJobTransitionRequest {
            scope: queue_scope.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 9_008)),
            expected_revision: submitted.job.revision,
            from: ExecutionJobState::Queued,
            to: ExecutionJobState::Leased,
            occurred_at: Instant("2027-01-15T08:00:00.200Z".into()),
        })
        .expect("execution queue lease");
    let mut admission = storage.execution_admission().expect("execution admission");
    for boundary in worker_admission_boundaries(
        scope,
        job_scope,
        pool_id,
        queue_scope
            .delivery_id
            .as_ref()
            .expect("Delivery admission boundary"),
    ) {
        admission
            .configure_policy(&ExecutionAdmissionPolicy {
                boundary,
                limits: ExecutionAdmissionLimits {
                    max_concurrent: 4,
                    max_queued: 4,
                    token_budget: Some(10_000),
                    cost_budget_microunits: Some(100_000),
                    max_runtime_millis: Some(3_600_000),
                },
            })
            .expect("execution admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: queue_scope.clone(),
            user_id: UserId(canonical_id("usr", seed)),
            worker_pool_id: pool_id.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 9_002)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: Some(100),
            reserved_cost_microunits: Some(1_000),
            runtime_limit_millis: Some(120_000),
            submitted_at,
        })
        .expect("execution reservation");
    admission
        .start(&ExecutionReservationStart {
            scope: queue_scope,
            worker_pool_id: pool_id.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 9_003)),
            expected_revision: 1,
            started_at: Instant("2027-01-15T08:00:00.100Z".into()),
        })
        .expect("execution reservation start");
}

fn seed_authenticated_worker_registration(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    message: &JobOutcomeMessage,
    pool_id: WorkerPoolId,
    seed: u64,
) {
    let management_scope = WorkerRegistryScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    };
    let authentication_identity = WorkerAuthenticationIdentity::LocalEmbedded {
        control_plane_principal: format!("remote-worker-{seed}"),
    };
    let registration_request_id = RequestId(canonical_id("req", seed + 9_000));
    {
        let mut registry = storage.execution_registry().expect("execution registry");
        registry
            .register_worker_for_scope(
                &WorkerRegistrationRequest {
                    authentication_identity: authentication_identity.clone(),
                    protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
                    platform: WorkerPlatform::Aarch64AppleDarwin,
                    capabilities: vec!["codex".to_owned()],
                    capability_digest: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
                    security_zone: "remote-default".to_owned(),
                    max_slots: 1,
                    message_id: ExecutionMessageId(canonical_id("xmsg", seed + 9_000)),
                    request_id: registration_request_id.clone(),
                    sent_at: Instant("2027-01-15T08:00:00.000Z".into()),
                    started_at: Instant("2027-01-15T07:59:59.000Z".into()),
                    worker_id: message.lease.worker_id.clone(),
                    worker_instance_id: message.lease.worker_instance_id.clone(),
                },
                &management_scope,
            )
            .expect("transport Worker registration");
        registry
            .record_authenticated_worker_placement(&AuthenticatedWorkerPlacement {
                worker_id: message.lease.worker_id.clone(),
                worker_instance_id: message.lease.worker_instance_id.clone(),
                worker_pool_id: pool_id,
                management_scope,
                authentication_identity,
                registration_request_id,
                placed_at: Instant("2027-01-15T08:00:00.000Z".into()),
            })
            .expect("authenticated Worker placement");
        registry
            .record_heartbeat(&WorkerHeartbeatRequest {
                active_leases: Vec::new(),
                available_slots: 1,
                heartbeat_sequence: ExecutionSequence(1),
                max_slots: 1,
                running_slots: 0,
                message_id: ExecutionMessageId(canonical_id("xmsg", seed + 9_006)),
                observed_at: Instant("2027-01-15T08:00:00.150Z".into()),
                sent_at: Instant("2027-01-15T08:00:00.150Z".into()),
                worker_id: message.lease.worker_id.clone(),
                worker_instance_id: message.lease.worker_instance_id.clone(),
            })
            .expect("authenticated Worker heartbeat");
    }
}

fn seed_worker_slot(storage: &mut SqliteStorage, message: &JobOutcomeMessage, seed: u64) {
    let mut slots = storage.worker_session_slots().expect("Worker slots");
    slots
        .configure_resources(
            &message.lease.worker_id,
            &message.lease.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000_000,
                max_disk_bytes: 1_000_000,
                max_processes: 10,
            },
        )
        .expect("Worker resource limits");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: WorkerSlotAuthority {
                worker_id: message.lease.worker_id.clone(),
                worker_instance_id: message.lease.worker_instance_id.clone(),
                worker_session_id: message.worker_session_id.clone(),
                codex_thread_id: message.session_identity.codex_thread_id.clone(),
                job_id: message.lease.job_id.clone(),
                lease_id: message.lease.lease_id.clone(),
                attempt: u64::try_from(message.lease.attempt).expect("slot attempt"),
                fencing_token: message.lease.fencing_token.clone(),
            },
            resources: WorkerSlotResources {
                memory_bytes: 100,
                disk_bytes: 100,
                process_slots: 1,
            },
            request_id: RequestId(canonical_id("req", seed + 9_005)),
            opened_at: Instant("2027-01-15T08:00:00.300Z".into()),
        })
        .expect("Worker slot open");
}

fn worker_terminal_state(root: &Path, job_id: &ExecutionJobId) -> (String, i64) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Worker quota terminal inspection");
    let operational = connection
        .query_row(
            "SELECT state FROM execution_admission_reservations WHERE job_id = ?1",
            [&job_id.0],
            |row| row.get(0),
        )
        .expect("operational terminal state");
    let usage_sources = connection
        .query_row(
            "SELECT COUNT(*) FROM execution_admission_settlement_sources WHERE job_id = ?1",
            [&job_id.0],
            |row| row.get(0),
        )
        .expect("Worker Usage source count");
    connection.close().expect("terminal inspection close");
    (operational, usage_sources)
}

fn repository_receipt_scope(scope: &RepositoryScope) -> ReceiptScopeKey {
    fn field(encoded: &mut Vec<u8>, value: &[u8]) {
        encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
        encoded.extend_from_slice(value);
    }
    let mut encoded = Vec::new();
    field(&mut encoded, b"winwincode.command-receipt.scope.v1");
    field(&mut encoded, b"repository");
    for value in [
        &scope.organization_id.0,
        &scope.workspace_id.0,
        &scope.project_id.0,
        &scope.repository_id.0,
    ] {
        field(&mut encoded, value.as_bytes());
    }
    ReceiptScopeKey::from_encoded(encoded).expect("repository receipt scope")
}

#[allow(clippy::too_many_arguments)]
fn seed_candidate_artifact(
    root: &Path,
    repository: &Path,
    base_commit: &str,
    scope: &RepositoryScope,
    delivery: &Delivery,
    message: &mut JobOutcomeMessage,
    candidate_commit: &str,
    seed: u64,
) {
    let bytes = GitCandidateArtifactManifest::new(
        candidate_commit.to_owned(),
        candidate_bundle(repository, base_commit, candidate_commit),
    )
    .expect("candidate manifest")
    .encode()
    .expect("candidate manifest encoding");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    let artifact = message
        .outcome
        .artifacts
        .first_mut()
        .expect("candidate Artifact reference");
    artifact.digest = digest.clone();
    let provenance = ArtifactProvenance::execution_job(
        message.lease.job_id.clone(),
        u64::try_from(message.lease.attempt).expect("candidate attempt"),
        message.lease.lease_id.clone(),
        message.lease.fencing_token.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.worker_session_id.clone(),
    )
    .expect("candidate provenance");
    let object_store =
        LocalArtifactObjectStore::open(root.join("artifacts")).expect("candidate object store");
    let mut artifacts = ArtifactStore::open(root.join("artifact-catalog"), Box::new(object_store))
        .expect("candidate Artifact catalog");
    let scope_key = repository_receipt_scope(scope);
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope_key.clone(),
            ExecutionMessageId(canonical_id("xmsg", seed + 70_000)),
            RequestId(canonical_id("req", seed + 70_000)),
            artifact.artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            u64::try_from(bytes.len()).expect("candidate byte length"),
            Some("candidate.json".to_owned()),
            provenance.clone(),
            ArtifactMeteringAttribution {
                organization_id: scope.organization_id.clone(),
                workspace_id: scope.workspace_id.clone(),
                project_id: scope.project_id.clone(),
                repository_id: scope.repository_id.clone(),
                delivery_id: Some(delivery.id().clone()),
                product_session_id: Some(message.session_identity.product_session_id.clone()),
                user_id: UserId(canonical_id("usr", seed + 70_000)),
            },
            ArtifactRetention::Indefinite,
            1_800_000_059_000,
        ))
        .expect("candidate Artifact open");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            scope_key,
            ExecutionMessageId(canonical_id("xmsg", seed + 70_001)),
            artifact.artifact_id.clone(),
            provenance,
            1_800_000_059_500,
            1,
            "application/octet-stream",
            digest,
            bytes,
            true,
        ))
        .expect("candidate Artifact complete");
    artifacts.close().expect("candidate Artifact close");
}

fn install_terminal_failure(root: &Path, member: &str) {
    let sql = match member {
        "state" => {
            "CREATE TRIGGER fail_terminal_member BEFORE UPDATE ON product_state \
                    WHEN NEW.stream_id LIKE 'delivery:%' AND NEW.revision = 2 \
                    BEGIN SELECT RAISE(ABORT, 'injected terminal state failure'); END;"
        }
        "journal" => {
            "CREATE TRIGGER fail_terminal_member BEFORE INSERT ON aggregate_journal_records \
                      WHEN NEW.aggregate_type = 'delivery' AND NEW.sequence = 2 \
                      BEGIN SELECT RAISE(ABORT, 'injected terminal journal failure'); END;"
        }
        "receipt" => {
            "CREATE TRIGGER fail_terminal_member BEFORE INSERT ON command_receipts \
                      WHEN NEW.stream_id LIKE 'delivery:%' AND NEW.revision = 2 \
                      BEGIN SELECT RAISE(ABORT, 'injected terminal receipt failure'); END;"
        }
        "outbox" => {
            "CREATE TRIGGER fail_terminal_member BEFORE INSERT ON outbox \
                     WHEN NEW.topic = 'delivery.work_run.terminal' \
                     BEGIN SELECT RAISE(ABORT, 'injected terminal outbox failure'); END;"
        }
        _ => panic!("unknown terminal atomic member"),
    };
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("failure injector");
    connection.execute_batch(sql).expect("failure trigger");
    connection.close().expect("failure injector close");
}

fn durable_terminal_counts(root: &Path, delivery_id: &DeliveryId) -> (i64, i64, i64, i64) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("durable count connection");
    let revision = connection
        .query_row(
            "SELECT revision FROM product_state WHERE stream_id = ?1",
            [format!("delivery:{}", delivery_id.0)],
            |row| row.get(0),
        )
        .expect("state revision");
    let journal = connection
        .query_row(
            "SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [&delivery_id.0],
            |row| row.get(0),
        )
        .expect("journal count");
    let receipts = connection
        .query_row("SELECT COUNT(*) FROM command_receipts", [], |row| {
            row.get(0)
        })
        .expect("receipt count");
    let outbox = connection
        .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))
        .expect("outbox count");
    connection.close().expect("durable count close");
    (revision, journal, receipts, outbox)
}

fn assert_accepted_outcome_state(root: &Path, message: &JobOutcomeMessage, ready: bool) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("accepted outcome state connection");
    let (revision, bytes): (i64, Vec<u8>) = connection
        .query_row(
            "SELECT revision, payload FROM product_state WHERE stream_id = ?1",
            [format!(
                "execution-outcome:{}:{}",
                message.lease.job_id.0, message.lease.attempt
            )],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("accepted original outcome exists");
    let revision = u64::try_from(revision).expect("accepted outcome revision");
    let accepted: serde_json::Value =
        serde_json::from_slice(&bytes).expect("accepted original outcome JSON");
    assert_eq!(revision, if ready { 2 } else { 1 });
    assert_eq!(accepted["acknowledgementReady"], ready);
    assert_eq!(accepted["message"], serde_json::to_value(message).unwrap());
    connection.close().expect("accepted outcome state close");
}

fn execution_job_counts(root: &Path) -> (i64, i64) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("execution dispatch count connection");
    let queue = connection
        .query_row("SELECT COUNT(*) FROM scheduler_execution_jobs", [], |row| {
            row.get(0)
        })
        .expect("scheduler Job count");
    let outbox = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE event_id LIKE 'execution-job:%'",
            [],
            |row| row.get(0),
        )
        .expect("durable dispatch Job count");
    connection.close().unwrap();
    (queue, outbox)
}

fn audit_event_for_receipt(root: &Path, receipt: &CommitReceipt) -> AuditEvent {
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
        .expect("terminal audit event");
    connection.close().expect("audit event inspection close");
    serde_json::from_slice(&payload).expect("canonical terminal audit event JSON")
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

fn terminal_receipt_count(root: &Path, delivery_id: &DeliveryId) -> i64 {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("terminal receipt count connection");
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE stream_id = ?1 AND revision > 1",
            [format!("delivery:{}", delivery_id.0)],
            |row| row.get(0),
        )
        .expect("terminal receipt count");
    connection.close().expect("terminal receipt count close");
    count
}

fn terminal_message(
    job: &ExecutionJob,
    delivery: &Delivery,
    seed: u64,
    status: ExecutionOutcomeStatus,
) -> JobOutcomeMessage {
    let run = delivery
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.state == winwincode_domain::WorkRunState::Running)
        .expect("canonical active run");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.work_run_id == run.id)
        .expect("exact active binding");
    JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
            fencing_token: binding.fencing_token.clone().expect("canonical binding fence"),
            issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
            job_id: job.job_id.clone(),
            lease_id: LeaseId(canonical_id("lse", seed)),
            worker_id: WorkerId(canonical_id("wrk", seed)),
            worker_instance_id: WorkerInstanceId(canonical_id("wki", seed)),
        },
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        outcome: ExecutionOutcome {
            artifacts: vec![ArtifactReference {
                artifact_id: ArtifactId(canonical_id("art", seed)),
                digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            }],
            codex_thread_id: binding.codex_thread_id.clone(),
            error: None,
            finished_at: Instant("2027-01-15T08:01:00.000Z".into()),
            last_event_sequence: ExecutionAckSequence(12),
            status,
            summary: "Final verifier completed".into(),
            usage: Some(ExecutionOutcomeUsage {
                cost_microunits: Some(400),
                runtime_millis: 60_000,
                tokens: Some(40),
 known_tokens: 40,
 accounting_status: winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Known,
            }),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:01:00.100Z".into()),
        session_identity: SessionIdentity {
            codex_thread_id: binding
                .codex_thread_id
                .clone()
                .expect("CodexThread identity"),
            product_session_id: binding.product_session_id.clone(),
            work_run_id: Some(binding.work_run_id.clone()),
            worker_session_id: binding
                .worker_session_id
                .clone()
                .expect("WorkerSession identity"),
        },
        worker_session_id: binding.worker_session_id.clone().expect("WorkerSession"),
    }
}

fn outcome_facts(
    delivery: &Delivery,
    message: &JobOutcomeMessage,
) -> winwincode_delivery::application::workrun_execution::DeliveryTerminalOutcomeFacts {
    let run = delivery
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.execution_job_id == message.lease.job_id)
        .expect("exact terminal WorkRun");
    outcome_facts_for_stage(delivery, message, run.id.clone())
}

fn outcome_facts_for_stage(
    _delivery: &Delivery,
    message: &JobOutcomeMessage,
    work_run_id: winwincode_domain::WorkRunId,
) -> winwincode_delivery::application::workrun_execution::DeliveryTerminalOutcomeFacts {
    let lease = active_lease_identity(
        message.lease.job_id.clone(),
        u64::try_from(message.lease.attempt).expect("attempt"),
        message.lease.lease_id.clone(),
        message.lease.fencing_token.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.worker_session_id.clone(),
    );
    let authority = session_binding_authority(
        lease,
        message.lease.issued_at.clone(),
        message.lease.expires_at.clone(),
    );
    let metadata = terminal_outcome_metadata(
        message.outcome.codex_thread_id.clone(),
        1_800_000_060_000,
        message.outcome.last_event_sequence.clone(),
        message
            .outcome
            .artifacts
            .iter()
            .map(|artifact| {
                winwincode_delivery::application::workrun_execution::TerminalArtifactReference {
                    artifact_id: artifact.artifact_id.clone(),
                    digest: artifact.digest.clone(),
                }
            })
            .collect(),
    );
    let outcome = terminal_worker_outcome(
        work_run_id,
        message.lease.job_id.clone(),
        1,
        message.lease.lease_id.clone(),
        message.lease.fencing_token.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.worker_session_id.clone(),
        match message.outcome.status {
            ExecutionOutcomeStatus::Succeeded => {
                winwincode_delivery::application::workrun_execution::TerminalOutcomeStatus::Succeeded
            }
            ExecutionOutcomeStatus::Failed => {
                winwincode_delivery::application::workrun_execution::TerminalOutcomeStatus::Failed
            }
            ExecutionOutcomeStatus::InfrastructureError => {
                winwincode_delivery::application::workrun_execution::TerminalOutcomeStatus::InfrastructureError
            }
            ExecutionOutcomeStatus::Cancelled => {
                winwincode_delivery::application::workrun_execution::TerminalOutcomeStatus::Cancelled
            }
        },
        metadata,
    );
    delivery_terminal_outcome_facts(authority, outcome)
}

fn initialize_git_repository(repository: &Path) -> (String, String) {
    fs::create_dir_all(repository.join("src")).expect("create repository");
    fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname = \"terminal-handoff-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("write repository manifest");
    fs::write(repository.join("src/lib.rs"), "pub fn fixture() {}\n")
        .expect("write repository source");
    for arguments in [
        &["init", "-q"][..],
        &["config", "user.email", "fixture@example.invalid"][..],
        &["config", "user.name", "Fixture"][..],
        &["add", "."][..],
        &["commit", "-q", "-m", "fixture"][..],
    ] {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .status()
            .expect("run Git fixture command");
        assert!(
            status.success(),
            "Git fixture command failed: {arguments:?}"
        );
    }
    let base_commit = git_text(repository, &["rev-parse", "HEAD"]);
    fs::write(
        repository.join("src/lib.rs"),
        "pub fn fixture() {}\npub fn candidate() {}\n",
    )
    .expect("write candidate source");
    for arguments in [&["add", "."][..], &["commit", "-q", "-m", "candidate"][..]] {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .status()
            .expect("run candidate Git fixture command");
        assert!(
            status.success(),
            "candidate Git fixture command failed: {arguments:?}"
        );
    }
    let candidate_commit = git_text(repository, &["rev-parse", "HEAD"]);
    (base_commit, candidate_commit)
}

fn git_text(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .output()
        .expect("run Git fixture query");
    assert!(output.status.success(), "Git query failed: {arguments:?}");
    String::from_utf8(output.stdout)
        .expect("Git output")
        .trim()
        .to_owned()
}

fn repository_executor_delivery(seed: u64, repository: &Path, base_revision: &str) -> Delivery {
    let mut snapshot = running_non_final_executor(seed).into_snapshot();
    repository
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .expect("portable repository fixture locator")
        .clone_into(&mut snapshot.spec.repository.locator);
    base_revision.clone_into(&mut snapshot.spec.base_revision);
    Delivery::try_from_snapshot(snapshot).expect("expired-lease Delivery")
}

fn verdict_command(
    seed: u64,
    delivery: &Delivery,
    candidate: &winwincode_delivery::domain::FrozenDeliveryCandidate,
) -> CommandEnvelope {
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_domain::UserActorKind::User,
        }),
        command: CommandName::DeliverySubmitVerdict,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("revision")),
        payload: serde_json::json!({
            "deliveryId": delivery.id().0,
            "candidateDigest": candidate.candidate_digest(),
        }),
        request_id: RequestId(canonical_id("req", seed + 1000)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(repository_scope(seed)),
    }
}

#[test]
fn authenticated_worker_terminal_settles_or_releases_once_across_restart() {
    for (seed, name, status, expected_state, expected_sources) in [
        (
            70,
            "succeeded",
            ExecutionOutcomeStatus::Succeeded,
            "settled",
            1,
        ),
        (
            74,
            "unknown-cost",
            ExecutionOutcomeStatus::Succeeded,
            "settled",
            1,
        ),
        (
            78,
            "unknown-usage",
            ExecutionOutcomeStatus::Succeeded,
            "released",
            0,
        ),
        (71, "failed", ExecutionOutcomeStatus::Failed, "released", 0),
        (
            75,
            "failed-known-usage",
            ExecutionOutcomeStatus::Failed,
            "settled",
            1,
        ),
        (
            76,
            "cancelled-known-usage",
            ExecutionOutcomeStatus::Cancelled,
            "settled",
            1,
        ),
        (
            77,
            "infrastructure-known-usage",
            ExecutionOutcomeStatus::InfrastructureError,
            "settled",
            1,
        ),
        (
            72,
            "cancelled",
            ExecutionOutcomeStatus::Cancelled,
            "released",
            0,
        ),
    ] {
        assert_authenticated_worker_terminal_case(
            seed,
            name,
            status,
            expected_state,
            expected_sources,
        );
    }
}

fn assert_authenticated_worker_terminal_case(
    seed: u64,
    name: &str,
    status: ExecutionOutcomeStatus,
    expected_state: &str,
    expected_sources: i64,
) {
    let root = temporary_directory(&format!("Worker-quota-{name}"));
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let mut message = terminal_message(&job, &delivery, seed, status);
    if expected_state == "released" {
        message.outcome.usage = None;
    }
    if name.ends_with("known-usage") {
        message.outcome.usage = Some(ExecutionOutcomeUsage {
            tokens: Some(47),
            known_tokens: 47,
            accounting_status:
                winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Known,
            runtime_millis: 30_000,
            cost_microunits: None,
        });
    }
    if name == "unknown-usage" {
        message.outcome.usage = Some(ExecutionOutcomeUsage::unknown(30_000, 47));
    }
    if name == "unknown-cost" {
        message
            .outcome
            .usage
            .as_mut()
            .expect("usage")
            .cost_microunits = None;
    }
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    let first = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("authenticated Worker terminal commit");
    assert!(!first.receipt().idempotent_replay);
    control_plane.shutdown().expect("Control Plane shutdown");
    assert_eq!(
        worker_terminal_state(&root, &job.job_id),
        (expected_state.to_owned(), expected_sources)
    );

    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane restart");
    let replay = restarted
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("exact Worker terminal replay");
    assert!(replay.receipt().idempotent_replay);
    restarted.shutdown().expect("restart shutdown");
    assert_eq!(
        worker_terminal_state(&root, &job.job_id),
        (expected_state.to_owned(), expected_sources)
    );
    if let Some(usage) = &message.outcome.usage
        && expected_state == "settled"
    {
        let connection =
            rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("settled usage");
        let cost: Option<i64> = connection.query_row("SELECT actual_cost_microunits FROM execution_admission_reservations WHERE job_id = ?1", [&job.job_id.0], |row| row.get(0)).expect("actual charge");
        assert_eq!(cost, usage.cost_microunits);
        let tokens: i64 = connection
            .query_row(
                "SELECT actual_tokens FROM execution_admission_reservations WHERE job_id = ?1",
                [&job.job_id.0],
                |row| row.get(0),
            )
            .expect("actual tokens");
        assert_eq!(Some(tokens), usage.tokens);
    }
    if name == "unknown-usage" {
        assert_reconciled_terminal_keeps_original_outcome(
            &root, &scope, &job, &message, &facts, seed,
        );
    }
    fs::remove_dir_all(root).expect("directory release");
}

fn assert_reconciled_terminal_keeps_original_outcome(
    root: &Path,
    scope: &RepositoryScope,
    job: &ExecutionJob,
    message: &JobOutcomeMessage,
    facts: &winwincode_delivery::application::workrun_execution::DeliveryTerminalOutcomeFacts,
    seed: u64,
) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3")).unwrap();
    let terminal: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM outbox WHERE topic = 'delivery.work_run.terminal'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let terminal_value: serde_json::Value = serde_json::from_slice(&terminal).unwrap();
    assert_eq!(terminal_value["outcome"]["status"], "succeeded");
    let (terminal_request_id, unknown): (String, String) = connection.query_row(
        "SELECT terminal_request_id, terminal_usage_json FROM execution_admission_reconciliation WHERE job_id = ?1",
        [&job.job_id.0], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    let unknown: ExecutionOutcomeUsage = serde_json::from_str(&unknown).unwrap();
    assert_eq!(unknown, ExecutionOutcomeUsage::unknown(30_000, 47));
    drop(connection);
    let mut storage = SqliteStorage::open(root).unwrap();
    let settled = ExecutionOutcomeUsage {
        tokens: Some(54),
        known_tokens: 54,
        cost_microunits: Some(400),
        accounting_status:
            winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Known,
        runtime_millis: 30_000,
    };
    assert!(
        storage
            .execution_admission()
            .unwrap()
            .settle_reconciliation(
                &job.job_id,
                &RequestId(terminal_request_id.clone()),
                &RequestId(canonical_id("req", seed + 30_000)),
                &settled,
                &Instant("2027-01-15T08:02:00.000Z".into())
            )
            .unwrap()
    );
    assert!(
        !storage
            .execution_admission()
            .unwrap()
            .settle_reconciliation(
                &job.job_id,
                &RequestId(terminal_request_id),
                &RequestId(canonical_id("req", seed + 30_000)),
                &settled,
                &Instant("2027-01-15T08:02:00.000Z".into())
            )
            .unwrap()
    );
    drop(storage);
    let mut reconciled = ControlPlane::start_local(
        ControlPlaneConfig::local(root),
        Box::new(RecordingPublisher),
    )
    .unwrap();
    let replay = reconciled
        .commit_delivery_terminal_outcome(scope, message, facts, &message.sent_at)
        .unwrap();
    assert!(replay.receipt().idempotent_replay);
    reconciled.shutdown().unwrap();
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3")).unwrap();
    let replayed: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM outbox WHERE topic = 'delivery.work_run.terminal'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        replayed, terminal,
        "reconciliation cannot rewrite the business terminal"
    );
}

#[test]
fn successful_terminal_without_immutable_usage_is_rejected_before_commit() {
    let seed = 73;
    let root = temporary_directory("successful-terminal-missing-usage");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let mut message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    message.outcome.usage = None;
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    let error = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect_err("successful terminal Usage is required");
    assert!(matches!(
        error,
        DeliveryTerminalOutcomeCommitError::Storage(ref source)
            if source.kind() == winwincode_control_plane::StorageErrorKind::InvalidInput
    ));
    control_plane.shutdown().expect("shutdown");
    assert_eq!(durable_terminal_counts(&root, delivery.id()).0, 1);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn committed_worker_resources_pending_recovers_exactly_after_restart() {
    let seed = 74;
    let root = temporary_directory("Worker-resources-pending-restart");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Worker resource failure injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_worker_resource_settlement
             BEFORE UPDATE ON execution_admission_reservations
             WHEN OLD.state = 'running' AND NEW.state = 'settled'
             BEGIN SELECT RAISE(ABORT, 'injected Worker resource settlement failure'); END;",
        )
        .expect("install Worker resource failure");
    connection.close().expect("failure injector close");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    let error = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect_err("committed Worker resource settlement must remain pending");
    assert!(
        matches!(
            &error,
            DeliveryTerminalOutcomeCommitError::CompletionPending { source, .. }
                if matches!(source.as_ref(), DurableExecutionPortError::WorkerLifecycle(cause)
                    if cause.kind() == winwincode_control_plane::WorkerExecutionLifecycleErrorKind::OperationalAdmission)
        ),
        "actual completion failure: {error}"
    );
    assert!(error.committed_receipt().is_some());
    assert_accepted_outcome_state(&root, &message, false);
    control_plane.shutdown().expect("crashed process shutdown");
    assert_eq!(
        worker_terminal_state(&root, &job.job_id),
        ("running".to_owned(), 0)
    );

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Worker resource failure remover");
    connection
        .execute_batch("DROP TRIGGER fail_worker_resource_settlement;")
        .expect("remove Worker resource failure");
    connection.close().expect("failure remover close");
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane restart");
    let replay = restarted
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("pending Worker resource recovery");
    assert!(replay.receipt().idempotent_replay);
    assert_accepted_outcome_state(&root, &message, true);
    restarted.shutdown().expect("restart shutdown");
    assert_eq!(
        worker_terminal_state(&root, &job.job_id),
        ("settled".to_owned(), 1)
    );
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_verifier_outcome_is_durable_before_verdict() {
    let seed = 1;
    let root = temporary_directory("final-verifier");
    let scope = repository_scope(seed);
    let (delivery, candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let stale_fixture = verdict_fixture(delivery.id(), VerdictFixtureOutcome::Pass);
    control_plane
        .commit_delivery_verdict(
            &verdict_command(seed, &delivery, &candidate),
            SubmitVerdictFacts {
                expected_revision: delivery.revision(),
                candidate: &candidate,
                verification: &stale_fixture.verification,
                evidence: &stale_fixture.evidence,
                produced_at_millis: 1_800_000_060_100,
            },
        )
        .expect_err("a Running final verifier cannot produce a verdict");

    let commit = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("terminal outcome commit");
    assert!(!commit.receipt().idempotent_replay);
    assert_eq!(commit.receipt().revision, delivery.revision() + 1);
    assert_eq!(commit.receipt().events.len(), 3);
    for event in commit
        .receipt()
        .events
        .iter()
        .filter(|event| event.public_context.is_some())
    {
        let context = event.public_context.as_ref().expect("public event context");
        assert_eq!(context.occurred_at(), &message.sent_at);
        assert_eq!(
            context.source(),
            &PublicEventSource::SessionExecutionWorker {
                worker_id: message.lease.worker_id.clone(),
                worker_session_id: message.worker_session_id.clone(),
                lease_id: message.lease.lease_id.clone(),
                codex_thread_id: message.session_identity.codex_thread_id.clone(),
                session_identity: message.session_identity.clone(),
            }
        );
    }

    let audit_event = audit_event_for_receipt(&root, commit.receipt());
    assert_eq!(audit_event_count(&root), 1);
    assert_eq!(
        audit_event.subject().execution_kind(),
        Some(AuditExecutionSubjectKind::Terminal)
    );
    let identity = audit_event
        .subject()
        .execution()
        .expect("terminal execution identity");
    assert_eq!(
        identity.product_session_id(),
        &message.session_identity.product_session_id
    );
    assert_eq!(identity.worker_session_id(), &message.worker_session_id);
    assert_eq!(
        identity.codex_thread_id(),
        message
            .outcome
            .codex_thread_id
            .as_ref()
            .expect("CodexThread")
    );
    assert_eq!(
        Some(identity.work_run_id()),
        message.session_identity.work_run_id.as_ref()
    );
    assert_eq!(identity.execution_job_id(), &message.lease.job_id);
    assert_eq!(identity.delivery_id(), delivery.id());
    assert_eq!(identity.source_sequence().expect("terminal sequence").0, 12);
    let audit_access = AuditScope::repository(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .expect("canonical terminal audit scope")
    .into_access();
    let audit = control_plane
        .read_audit(&audit_access, 0, 20, 2_000_000_000_000)
        .expect("terminal outcome is visible through the canonical AuditStore");
    assert!(audit.records().iter().any(|record| {
        record.event().is_some_and(|event| {
            event.event_id() == audit_event.event_id()
                && event.subject().execution_kind() == Some(AuditExecutionSubjectKind::Terminal)
        })
    }));

    let stored = control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("state read")
        .expect("Delivery state");
    let settled = Delivery::decode_json(&stored.payload).expect("settled Delivery");
    assert_eq!(settled.revision(), delivery.revision() + 1);
    let aggregate = &settled.snapshot().work_run_aggregate;
    let run = aggregate
        .runs
        .iter()
        .find(|run| Some(&run.id) == message.session_identity.work_run_id.as_ref())
        .expect("exact producer run");
    assert_eq!(run.state, winwincode_domain::WorkRunState::Settled);
    let item = aggregate
        .items
        .iter()
        .find(|item| item.id == run.work_item_id)
        .expect("exact producer item");
    assert_eq!(item.state, winwincode_domain::WorkItemState::CandidateReady);

    let verdict_facts = verdict_facts_fixture(&settled, &candidate, VerdictFixtureOutcome::Pass);
    control_plane
        .commit_delivery_verdict(
            &verdict_command(seed, &settled, &candidate),
            SubmitVerdictFacts {
                expected_revision: settled.revision(),
                candidate: &candidate,
                verification: verdict_facts.verification(),
                evidence: verdict_facts.evidence(),
                produced_at_millis: 1_800_000_060_100,
            },
        )
        .expect("verdict after terminal outcome");

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn terminal_event_excludes_raw_worker_text_and_lease_authority() {
    let seed = 101;
    let root = temporary_directory("secret-safe-terminal-event");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let mut message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Failed);
    message.outcome.summary = "authorization=terminal-summary-secret".into();
    message.outcome.error = Some(ExecutionPortError {
        code: ExecutionPortErrorCode::ExecutionFailed,
        message: "credential=terminal-error-secret".into(),
        retryable: false,
    });
    message.lease.fencing_token = FencingToken("9876543210987654321".into());
    let mut snapshot = delivery.into_snapshot();
    let run = snapshot
        .work_run_aggregate
        .runs
        .iter_mut()
        .find(|run| run.execution_job_id == message.lease.job_id)
        .unwrap();
    run.fencing_token = message.lease.fencing_token.0.clone();
    snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.work_run_id == run.id)
        .unwrap()
        .fencing_token = Some(message.lease.fencing_token.clone());
    let delivery = Delivery::try_from_snapshot(snapshot).expect("exact secret-bearing lease");
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let receipt = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("terminal outcome commit");
    let terminal_event = receipt
        .receipt()
        .events
        .iter()
        .find(|event| event.topic == "delivery.work_run.terminal")
        .expect("terminal event");
    let durable_payload = String::from_utf8_lossy(&terminal_event.payload);
    for forbidden in [
        "terminal-summary-secret",
        "terminal-error-secret",
        "9876543210987654321",
        "\"message\"",
        "\"lease\"",
        "\"fencingToken\"",
        "\"summary\"",
        "\"error\"",
    ] {
        assert!(
            !durable_payload.contains(forbidden),
            "terminal event leaked raw Worker or lease field {forbidden}: {durable_payload}"
        );
    }

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn exact_replay_precedes_current_state_journal_job_and_replacement_facts() {
    let seed = 2;
    let root = temporary_directory("receipt-first-replay");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    let first = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("initial terminal outcome");

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("corruption injector");
    connection
        .execute(
            "UPDATE product_state SET payload = X'00' WHERE stream_id = ?1",
            [format!("delivery:{}", delivery.id().0)],
        )
        .expect("break current state");
    connection
        .execute(
            "DELETE FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [&delivery.id().0],
        )
        .expect("remove current journal");
    connection
        .execute(
            "DELETE FROM outbox WHERE event_id = ?1",
            [format!("execution-job:{}", job.job_id.0)],
        )
        .expect("remove durable job");
    connection.close().expect("corruption injector close");

    let mut replacement_message = message.clone();
    replacement_message.lease.lease_id = LeaseId(canonical_id("lse", seed + 100));
    let replacement_facts = outcome_facts(&delivery, &replacement_message);
    let replay = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &replacement_facts, &message.sent_at)
        .expect("receipt-first replay");

    assert!(replay.receipt().idempotent_replay);
    assert_eq!(replay.receipt().revision, first.receipt().revision);
    assert_eq!(replay.receipt().events, first.receipt().events);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn same_message_identity_with_changed_body_is_a_request_conflict() {
    let seed = 3;
    let root = temporary_directory("message-conflict");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("initial terminal outcome");
    let mut changed = message.clone();
    changed.outcome.summary = "changed body under the same messageId".into();

    let error = control_plane
        .commit_delivery_terminal_outcome(&scope, &changed, &facts, &changed.sent_at)
        .expect_err("same messageId cannot authorize another body");
    assert!(matches!(
        error,
        DeliveryTerminalOutcomeCommitError::Storage(ref source)
            if source.kind() == winwincode_control_plane::StorageErrorKind::RequestConflict
    ));
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn a_new_message_cannot_resettle_an_already_terminal_work_run() {
    let seed = 5;
    let root = temporary_directory("stale-new-message");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let before = durable_terminal_counts(&root, delivery.id());
    let after = (before.0 + 1, before.1 + 1, before.2 + 2, before.3 + 4);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("initial terminal outcome");
    let mut stale = message.clone();
    stale.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 100));

    control_plane
        .commit_delivery_terminal_outcome(&scope, &stale, &facts, &stale.sent_at)
        .expect_err("a new message cannot settle the same WorkRun twice");
    assert_eq!(durable_terminal_counts(&root, delivery.id()), after);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

fn concurrently_accept_terminal_message(
    root: &Arc<PathBuf>,
    scope: &RepositoryScope,
    message: &JobOutcomeMessage,
) -> Vec<JobOutcomeAckMessageStatus> {
    let barrier = Arc::new(Barrier::new(8));
    let startup = Arc::new(Mutex::new(()));
    (0..8)
        .map(|_| {
            let root = Arc::clone(root);
            let barrier = Arc::clone(&barrier);
            let startup = Arc::clone(&startup);
            let scope = scope.clone();
            let message = message.clone();
            thread::spawn(move || {
                let (mut control_plane, mut storage) = {
                    let _guard = startup.lock().expect("fixture initialization lock");
                    let control_plane = ControlPlane::start_local(
                        ControlPlaneConfig::local(root.as_path()),
                        Box::new(RecordingPublisher),
                    )
                    .expect("Control Plane start");
                    let storage = SqliteStorage::open(root.as_path()).expect("ingress storage");
                    (control_plane, storage)
                };
                // Race the eight terminal requests after every CP is ready.
                barrier.wait();
                let response = DurableExecutionPortIngress::new(
                    &mut control_plane,
                    &mut storage,
                    &scope,
                    message.sent_at.clone(),
                )
                .expect("durable ingress")
                .handle(&ExecutionPortMessage::JobOutcomeMessage(message))
                .expect("concurrent terminal outcome");
                let [ExecutionPortMessage::JobOutcomeAckMessage(ack)] = response.as_slice() else {
                    panic!("one terminal acknowledgement")
                };
                let status = ack.status.clone();
                Box::new(storage).close().expect("ingress storage close");
                control_plane.shutdown().expect("shutdown");
                status
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().expect("terminal outcome thread"))
        .collect()
}

fn changed_terminal_replay_status(
    root: &Path,
    scope: &RepositoryScope,
    message: &JobOutcomeMessage,
) -> JobOutcomeAckMessageStatus {
    let mut changed = message.clone();
    changed.outcome.summary = "changed body under the committed messageId".into();
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root),
        Box::new(RecordingPublisher),
    )
    .expect("changed replay Control Plane");
    let mut storage = SqliteStorage::open(root).expect("changed replay ingress storage");
    let response = DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        scope,
        changed.sent_at.clone(),
    )
    .expect("changed replay ingress")
    .handle(&ExecutionPortMessage::JobOutcomeMessage(changed))
    .expect("changed replay response");
    let [ExecutionPortMessage::JobOutcomeAckMessage(ack)] = response.as_slice() else {
        panic!("one changed replay acknowledgement")
    };
    let status = ack.status.clone();
    Box::new(storage)
        .close()
        .expect("changed replay storage close");
    control_plane
        .shutdown()
        .expect("changed replay Control Plane shutdown");
    status
}

fn assert_one_terminal_resource_transition(
    root: &Path,
    delivery: &Delivery,
    job: &ExecutionJob,
    message: &JobOutcomeMessage,
) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("terminal resource inspection");
    let queue: (String, i64) = connection
        .query_row(
            "SELECT state, revision FROM scheduler_execution_jobs WHERE job_id = ?1",
            [&job.job_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("terminal scheduler Job");
    let slot: (String, i64) = connection
        .query_row(
            "SELECT state, revision FROM worker_session_slots WHERE worker_session_id = ?1",
            [&message.worker_session_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("terminal Worker slot");
    let operational: (String, i64) = connection
        .query_row(
            "SELECT state, revision FROM execution_admission_reservations WHERE job_id = ?1",
            [&job.job_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("terminal operational admission");
    let lease_terminals: (i64, String) = connection
        .query_row(
            "SELECT COUNT(*), outcome FROM execution_lease_terminals WHERE job_id = ?1",
            [&job.job_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("terminal execution lease");
    let usage_sources: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM execution_admission_settlement_sources WHERE job_id = ?1",
            [&job.job_id.0],
            |row| row.get(0),
        )
        .expect("terminal Usage source");
    assert_eq!(queue, ("completed".into(), 4));
    assert_eq!(slot, ("completed".into(), 2));
    assert_eq!(operational, ("settled".into(), 3));
    assert_eq!(lease_terminals, (1, "completed".into()));
    assert_eq!(usage_sources, 1);
    connection.close().expect("terminal resource close");

    let storage = SqliteStorage::open(root).expect("terminal Delivery inspection");
    let state = storage
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("terminal Delivery state")
        .expect("terminal Delivery exists");
    let current = Delivery::decode_json(&state.payload).expect("terminal Delivery JSON");
    let ExecutionScope::WorkRunExecutionScope(job_scope) = &job.scope else {
        panic!("terminal Delivery job scope")
    };
    let run = current
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.id == job_scope.work_run_id)
        .expect("terminal WorkRun");
    let bindings = current
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.work_run_id == run.id)
        .collect::<Vec<_>>();
    assert_eq!(run.state, winwincode_domain::WorkRunState::Settled);
    assert_eq!(current.revision(), delivery.revision() + 1);
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].execution_job_id, job.job_id);
    Box::new(storage)
        .close()
        .expect("terminal Delivery inspection close");
}

fn assert_concurrent_exact_terminal_round(seed: u64) {
    let root = temporary_directory("concurrent-exact-message");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    seed_delivery_and_job(&root, &delivery, &job);

    let root = Arc::new(root);
    let statuses = concurrently_accept_terminal_message(&root, &scope, &message);
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == JobOutcomeAckMessageStatus::Accepted)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == JobOutcomeAckMessageStatus::Duplicate)
            .count(),
        7
    );
    assert_eq!(
        changed_terminal_replay_status(root.as_path(), &scope, &message),
        JobOutcomeAckMessageStatus::RejectedConflict
    );
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("durable count connection");
    let state_revision: i64 = connection
        .query_row(
            "SELECT revision FROM product_state WHERE stream_id = ?1",
            [format!("delivery:{}", delivery.id().0)],
            |row| row.get(0),
        )
        .expect("state revision");
    let journal_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [&delivery.id().0],
            |row| row.get(0),
        )
        .expect("journal count");
    let terminal_receipts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE stream_id = ?1 AND revision = 2",
            [format!("delivery:{}", delivery.id().0)],
            |row| row.get(0),
        )
        .expect("terminal receipt count");
    let terminal_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE topic IN ('delivery.work_run.terminal', 'delivery.changed.v1', 'runtime-projection.invalidated.v1')",
            [],
            |row| row.get(0),
        )
        .expect("terminal event count");
    assert_eq!(
        (
            state_revision,
            journal_count,
            terminal_receipts,
            terminal_events
        ),
        (2, 2, 1, 3)
    );
    connection.close().expect("durable count close");
    assert_one_terminal_resource_transition(root.as_path(), &delivery, &job, &message);
    if seed == 4 {
        assert_corrupt_terminal_replay(root.as_path(), &scope, &message);
    }
    fs::remove_dir_all(root.as_path()).expect("database directory release");
}

fn assert_corrupt_terminal_replay(
    root: &Path,
    scope: &RepositoryScope,
    message: &JobOutcomeMessage,
) {
    let stream = format!("execution-outcome:{}:1", message.lease.job_id.0);
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3")).unwrap();
    let original: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = ?1",
            [&stream],
            |row| row.get(0),
        )
        .unwrap();
    let mut invalid_decision: serde_json::Value = serde_json::from_slice(&original).unwrap();
    invalid_decision["decision"]["status"] = serde_json::json!("failed");
    for (revision, payload) in [
        (2, vec![0]),
        (3, original.clone()),
        (2, serde_json::to_vec(&invalid_decision).unwrap()),
    ] {
        connection
            .execute(
                "UPDATE product_state SET revision = ?1, payload = ?2 WHERE stream_id = ?3",
                rusqlite::params![revision, payload, &stream],
            )
            .unwrap();
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(root),
            Box::new(RecordingPublisher),
        )
        .unwrap();
        let mut storage = SqliteStorage::open(root).unwrap();
        for changed in [false, true] {
            let mut replay = message.clone();
            if changed {
                replay.outcome.summary = "changed replay with corrupt accepted state".into();
            }
            let error = DurableExecutionPortIngress::new(
                &mut control_plane,
                &mut storage,
                scope,
                replay.sent_at.clone(),
            )
            .unwrap()
            .handle(&ExecutionPortMessage::JobOutcomeMessage(replay))
            .unwrap_err();
            assert!(
                matches!(error, DurableExecutionPortError::Storage(ref source)
                if source.kind() == winwincode_control_plane::StorageErrorKind::Adapter),
                "corrupt accepted state must remain a storage failure: {error:?}"
            );
        }
        Box::new(storage).close().unwrap();
        control_plane.shutdown().unwrap();
    }
    connection
        .execute(
            "UPDATE product_state SET revision = 2, payload = ?1 WHERE stream_id = ?2",
            rusqlite::params![original, stream],
        )
        .unwrap();
    connection.close().unwrap();
}

#[test]
fn concurrent_exact_message_returns_one_commit_and_only_durable_replays() {
    for seed in 4..14 {
        assert_concurrent_exact_terminal_round(seed);
    }
}

#[test]
fn failed_infrastructure_and_cancelled_outcomes_settle_without_advancing_delivery() {
    for (offset, status, expected_run) in [
        (
            0_u64,
            ExecutionOutcomeStatus::Failed,
            winwincode_domain::WorkRunState::Failed,
        ),
        (
            1,
            ExecutionOutcomeStatus::InfrastructureError,
            winwincode_domain::WorkRunState::Failed,
        ),
        (
            2,
            ExecutionOutcomeStatus::Cancelled,
            winwincode_domain::WorkRunState::Cancelled,
        ),
    ] {
        let seed = 10 + offset;
        let root = temporary_directory("unsuccessful-status");
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, status);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");

        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
            .expect("unsuccessful terminal outcome");
        let replay = control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
            .expect("terminal outcome replay");
        assert!(replay.receipt().idempotent_replay);
        let stored = control_plane
            .load_state(&format!("delivery:{}", delivery.id().0))
            .expect("state read")
            .expect("Delivery state");
        let settled = Delivery::decode_json(&stored.payload).expect("settled Delivery");
        let run = settled
            .snapshot()
            .work_run_aggregate
            .runs
            .iter()
            .find(|run| run.execution_job_id == job.job_id)
            .expect("final verifier run");
        assert_eq!(run.state, expected_run);
        assert_eq!(
            settled.snapshot().work_run_aggregate.summary_state(false),
            if expected_run == winwincode_domain::WorkRunState::Cancelled {
                winwincode_domain::WorkItemState::Cancelled
            } else {
                winwincode_domain::WorkItemState::Failed
            },
            "a failed read-only consumer must not leave candidate_ready as the public state"
        );
        assert!(
            settled
                .snapshot()
                .work_run_aggregate
                .items
                .iter()
                .all(|item| item.state != winwincode_domain::WorkItemState::Done)
        );
        assert_eq!(settled.snapshot().status, DeliveryStatus::Ready);
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn terminal_commit_restarts_and_releases_worker_resources_after_a_write_crash() {
    let seed = 13;
    let root = temporary_directory("worker-resource-release-restart");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let mut message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Cancelled);
    message.outcome.usage = None;
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Worker resource release failure injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_worker_resource_release
             BEFORE UPDATE ON execution_admission_reservations
             WHEN OLD.state = 'running' AND NEW.state = 'released'
             BEGIN SELECT RAISE(ABORT, 'injected Worker resource release failure'); END;",
        )
        .expect("install Worker resource release failure");
    connection.close().expect("failure injector close");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let error = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect_err("terminal commit must surface pending Worker resource release");
    assert!(
        matches!(
            &error,
            DeliveryTerminalOutcomeCommitError::CompletionPending { source, .. }
                if matches!(source.as_ref(), DurableExecutionPortError::WorkerLifecycle(cause)
                    if cause.kind() == winwincode_control_plane::WorkerExecutionLifecycleErrorKind::OperationalAdmission)
        ),
        "actual completion failure: {error}"
    );
    assert!(error.committed_receipt().is_some());
    assert_accepted_outcome_state(&root, &message, false);
    control_plane.shutdown().expect("crashed process shutdown");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Worker resource release failure remover");
    connection
        .execute_batch("DROP TRIGGER fail_worker_resource_release;")
        .expect("remove Worker resource release failure");
    connection.close().expect("failure remover close");

    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart Control Plane");
    let replay = restarted
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("receipt-first retry releases Worker resources");
    assert!(replay.receipt().idempotent_replay);
    assert_accepted_outcome_state(&root, &message, true);
    assert_eq!(
        worker_terminal_state(&root, &job.job_id),
        ("released".to_owned(), 0)
    );
    restarted.shutdown().expect("restart shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn failure_at_each_atomic_member_rolls_back_terminal_outcome() {
    for (offset, member) in ["state", "journal", "receipt", "outbox"]
        .into_iter()
        .enumerate()
    {
        let seed = 20 + u64::try_from(offset).expect("small atomic member index");
        let root = temporary_directory(member);
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Failed);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let before = durable_terminal_counts(&root, delivery.id());
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        install_terminal_failure(&root, member);

        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
            .expect_err("injected atomic member failure");
        assert_eq!(
            durable_terminal_counts(&root, delivery.id()),
            before,
            "{member}"
        );
        assert_eq!(audit_event_count(&root), 0, "{member}");
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn publication_failure_keeps_terminal_commit_for_restart_and_receipt_replay() {
    let seed = 30;
    let root = temporary_directory("publication-restart");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let before = durable_terminal_counts(&root, delivery.id());
    let after = (before.0 + 1, before.1 + 1, before.2 + 1, before.3 + 3);
    let mut failing =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(FailingPublisher))
            .expect("Control Plane start");

    let error = failing
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect_err("publication must fail after commit");
    assert!(matches!(
        &error,
        DeliveryTerminalOutcomeCommitError::CompletionPending { source, .. }
            if matches!(source.as_ref(), DurableExecutionPortError::Publication(_))
    ));
    assert_accepted_outcome_state(&root, &message, false);
    let committed = error
        .committed_receipt()
        .expect("publication error carries committed terminal receipt");
    assert_eq!(committed.receipt().revision, delivery.revision() + 1);
    assert_eq!(durable_terminal_counts(&root, delivery.id()), after);
    assert_eq!(audit_event_count(&root), 1);
    failing
        .shutdown()
        .expect_err("failing publisher leaves durable events pending");

    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart publishes pending terminal events");
    let replay = restarted
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("receipt replay after restart");
    assert!(replay.receipt().idempotent_replay);
    assert_accepted_outcome_state(&root, &message, true);
    assert_eq!(
        durable_terminal_counts(&root, delivery.id()),
        (after.0, after.1, after.2 + 1, after.3 + 1)
    );
    assert_eq!(audit_event_count(&root), 1);
    let audit_event = audit_event_for_receipt(&root, replay.receipt());
    let audit_access = AuditScope::repository(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .expect("canonical restarted terminal audit scope")
    .into_access();
    let audit = restarted
        .read_audit(&audit_access, 0, 20, 2_000_000_000_000)
        .expect("terminal audit remains readable after restart");
    assert!(audit.records().iter().any(|record| {
        record
            .event()
            .is_some_and(|event| event.event_id() == audit_event.event_id())
    }));
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("published event count connection");
    let pending: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE published = 0
             AND topic != 'execution-outcome.acknowledgement-ready.v1'",
            [],
            |row| row.get(0),
        )
        .expect("pending event count");
    assert_eq!(pending, 0);
    let ready_markers: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE published = 0
             AND topic = 'execution-outcome.acknowledgement-ready.v1'",
            [],
            |row| row.get(0),
        )
        .expect("pending readiness marker count");
    assert_eq!(ready_markers, 1);
    connection.close().expect("published event count close");
    restarted.shutdown().expect("shutdown");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3")).unwrap();
    let pending: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE published = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending, 0,
        "shutdown publishes the internal readiness marker"
    );
    connection.close().unwrap();
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn stale_or_foreign_lease_binding_metadata_and_artifacts_fail_closed() {
    let seed = 40;
    let root = temporary_directory("foreign-terminal-facts");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let before = durable_terminal_counts(&root, delivery.id());
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let mut cases = Vec::new();
    let mut changed = message.clone();
    changed.lease.attempt = 2;
    cases.push(("attempt", changed));
    let mut changed = message.clone();
    changed.lease.lease_id = LeaseId(canonical_id("lse", seed + 1));
    cases.push(("lease", changed));
    let mut changed = message.clone();
    changed.lease.fencing_token = FencingToken((seed + 1).to_string());
    cases.push(("fence", changed));
    let mut changed = message.clone();
    changed.lease.worker_id = WorkerId(canonical_id("wrk", seed + 1));
    cases.push(("worker", changed));
    let mut changed = message.clone();
    changed.lease.worker_instance_id = WorkerInstanceId(canonical_id("wki", seed + 1));
    cases.push(("worker-instance", changed));
    let mut changed = message.clone();
    changed.worker_session_id = WorkerSessionId(canonical_id("wsn", seed + 1));
    cases.push(("worker-session", changed));
    let mut changed = message.clone();
    changed.lease.issued_at = Instant("2027-01-15T08:00:00.300Z".into());
    cases.push(("issued-at", changed));
    let mut changed = message.clone();
    changed.lease.expires_at = Instant("2027-01-15T08:06:00.000Z".into());
    cases.push(("expires-at", changed));
    let mut changed = message.clone();
    changed.outcome.codex_thread_id = Some(CodexThreadId(canonical_id("cdx", seed + 1)));
    cases.push(("codex-thread", changed));
    let mut changed = message.clone();
    changed.outcome.finished_at = Instant("2027-01-15T08:01:01.000Z".into());
    changed.sent_at = Instant("2027-01-15T08:01:01.100Z".into());
    cases.push(("finished-at", changed));
    let mut changed = message.clone();
    changed.outcome.last_event_sequence = ExecutionAckSequence(13);
    cases.push(("last-sequence", changed));
    let mut changed = message.clone();
    changed.outcome.artifacts[0].digest = Sha256Digest(format!("sha256:{}", "d".repeat(64)));
    cases.push(("artifact-digest", changed));
    let mut changed = message.clone();
    changed
        .outcome
        .artifacts
        .push(changed.outcome.artifacts[0].clone());
    cases.push(("duplicate-artifact", changed));

    for (name, changed) in cases {
        assert!(
            control_plane
                .commit_delivery_terminal_outcome(&scope, &changed, &facts, &changed.sent_at)
                .is_err(),
            "foreign {name} must fail closed"
        );
        assert_eq!(
            durable_terminal_counts(&root, delivery.id()),
            before,
            "{name}"
        );
        assert_eq!(audit_event_count(&root), 0, "{name}");
    }

    let foreign_stage_facts = outcome_facts_for_stage(
        &delivery,
        &message,
        winwincode_domain::WorkRunId(canonical_id("wrn", seed + 1)),
    );
    control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &foreign_stage_facts, &message.sent_at)
        .expect_err("foreign stage authority must fail closed");
    assert_eq!(durable_terminal_counts(&root, delivery.id()), before);
    assert_eq!(audit_event_count(&root), 0);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn outcome_error_must_match_the_generated_schema_before_persistence() {
    for (offset, error_message) in [(0_u64, String::new()), (1, "x".repeat(501))] {
        let seed = 45 + offset;
        let root = temporary_directory("invalid-outcome-error");
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let mut message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Failed);
        message.outcome.error = Some(ExecutionPortError {
            code: ExecutionPortErrorCode::ExecutionFailed,
            message: error_message,
            retryable: false,
        });
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let before = durable_terminal_counts(&root, delivery.id());
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");

        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
            .expect_err("schema-invalid outcome.error must be rejected");
        assert_eq!(durable_terminal_counts(&root, delivery.id()), before);
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn missing_wrong_topic_corrupt_foreign_or_wrong_scope_execution_job_fails_closed() {
    for (offset, corruption) in [
        "missing",
        "wrong-topic",
        "unknown-field",
        "foreign-work-run",
        "wrong-repository-scope",
    ]
    .into_iter()
    .enumerate()
    {
        let seed = 50 + u64::try_from(offset).expect("small corruption index");
        let root = temporary_directory(corruption);
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let event_id = format!("execution-job:{}", job.job_id.0);
        if corruption != "wrong-repository-scope" {
            let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
                .expect("ExecutionJob corruption injector");
            match corruption {
                "missing" => {
                    connection
                        .execute("DELETE FROM outbox WHERE event_id = ?1", [&event_id])
                        .expect("delete ExecutionJob");
                }
                "wrong-topic" => {
                    connection
                        .execute(
                            "UPDATE outbox SET topic = 'foreign.job' WHERE event_id = ?1",
                            [&event_id],
                        )
                        .expect("replace ExecutionJob topic");
                }
                "unknown-field" | "foreign-work-run" => {
                    let mut value = serde_json::to_value(&job).expect("ExecutionJob value");
                    if corruption == "unknown-field" {
                        value
                            .as_object_mut()
                            .expect("ExecutionJob object")
                            .insert("unknownField".into(), serde_json::json!(true));
                    } else {
                        value["scope"]["workRunId"] =
                            serde_json::json!(canonical_id("wrn", seed + 1));
                    }
                    connection
                        .execute(
                            "UPDATE outbox SET payload = ?1 WHERE event_id = ?2",
                            rusqlite::params![
                                serde_json::to_vec(&value).expect("ExecutionJob bytes"),
                                event_id
                            ],
                        )
                        .expect("replace ExecutionJob payload");
                }
                _ => unreachable!(),
            }
            connection
                .close()
                .expect("ExecutionJob corruption injector close");
        }
        let submitted_scope = if corruption == "wrong-repository-scope" {
            repository_scope(seed + 100)
        } else {
            scope
        };
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");

        control_plane
            .commit_delivery_terminal_outcome(&submitted_scope, &message, &facts, &message.sent_at)
            .expect_err("foreign durable ExecutionJob must fail closed");
        assert_eq!(
            control_plane
                .load_state(&format!("delivery:{}", delivery.id().0))
                .expect("state read")
                .expect("Delivery state")
                .revision,
            1,
            "{corruption}"
        );
        assert_eq!(
            terminal_receipt_count(&root, delivery.id()),
            0,
            "{corruption}"
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn persisted_successful_candidate_rejection_is_readable_and_restart_idempotent() {
    assert_persisted_candidate_rejection(85, None, false);
}

#[test]
fn temporary_candidate_source_failure_does_not_become_a_permanent_rejection() {
    assert_persisted_candidate_rejection(86, Some("artifact"), false);
    assert_persisted_candidate_rejection(87, Some("resolver"), false);
}

#[test]
fn persisted_candidate_rejection_preserves_history_and_allows_new_spec() {
    assert_persisted_candidate_rejection(88, None, true);
}

struct HistoricalCandidateFixture {
    seed: u64,
    root: PathBuf,
    repository: PathBuf,
    scope: RepositoryScope,
    delivery: Delivery,
    job: ExecutionJob,
    message: JobOutcomeMessage,
    authority_stream: String,
    accepted: StoredState,
}

impl HistoricalCandidateFixture {
    fn start_control_plane(&self) -> ControlPlane {
        ControlPlane::start_local_with_delivery_adapters(
            ControlPlaneConfig::local(&self.root),
            Box::new(RecordingPublisher),
            LocalDeliveryAdapterConfig::new(&self.repository, self.scope.clone()),
        )
        .expect("production Control Plane restart")
    }
}

fn assert_persisted_candidate_rejection(
    seed: u64,
    temporary_failure: Option<&str>,
    check_history_and_new_spec: bool,
) {
    let fixture = seed_historical_candidate_success(seed);
    let rejected_cut =
        assert_historical_refusal_restarts(&fixture, temporary_failure, check_history_and_new_spec);
    assert_historical_candidate_usage(&fixture);
    if check_history_and_new_spec {
        assert_explicit_spec_restart(
            &fixture,
            rejected_cut.expect("the original rejected read cut"),
        );
    }
    fs::remove_dir_all(fixture.root).expect("fixture cleanup");
}

fn seed_historical_candidate_success(seed: u64) -> HistoricalCandidateFixture {
    let root = temporary_directory("candidate-rejected-after-success");
    let repository = root.join("repository");
    let (base_commit, candidate_commit) = initialize_git_repository(&repository);
    let repository = fs::canonicalize(repository).expect("canonical repository");
    let scope = repository_scope(seed);
    let mut snapshot =
        repository_executor_delivery(seed, &repository, &base_commit).into_snapshot();
    let binding = snapshot
        .session_bindings
        .first_mut()
        .expect("writer binding");
    binding.execution_profile = Some("remediator".into());
    binding
        .runtime_context
        .as_mut()
        .expect("runtime context")
        .agent_identity
        .role = "remediator".into();
    snapshot.status = DeliveryStatus::Reworking;
    let delivery = Delivery::try_from_snapshot(snapshot).expect("historical remediator");
    let mut job = execution_job(&delivery, &scope);
    job.workspace.write_mode = ExecutionWorkspaceWriteMode::Candidate;
    let mut message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    message.outcome.summary = "Historical remediator uploaded its candidate".into();
    seed_candidate_artifact(
        &root,
        &repository,
        &base_commit,
        &scope,
        &delivery,
        &mut message,
        &candidate_commit,
        seed,
    );
    seed_delivery_and_job(&root, &delivery, &job);
    let facts = outcome_facts_for_stage(
        &delivery,
        &message,
        winwincode_domain::WorkRunId(canonical_id("wrn", seed)),
    );
    let mut first = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("first Control Plane");
    first
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("Worker success durably accepted before candidate validation");
    let authority_stream = format!("delivery-terminal-authority:{}", job.job_id.0);
    let accepted = first
        .load_state(&authority_stream)
        .expect("authority read")
        .expect("durable successful terminal");
    let terminal: serde_json::Value =
        serde_json::from_slice(&accepted.payload).expect("terminal JSON");
    assert_eq!(terminal["status"], "succeeded");
    assert_eq!(
        worker_terminal_state(&root, &job.job_id),
        ("settled".into(), 1)
    );
    first.shutdown().expect("first shutdown");

    HistoricalCandidateFixture {
        seed,
        root,
        repository,
        scope,
        delivery,
        job,
        message,
        authority_stream,
        accepted,
    }
}

fn assert_historical_refusal_restarts(
    fixture: &HistoricalCandidateFixture,
    temporary_failure: Option<&str>,
    check_history_and_new_spec: bool,
) -> Option<winwincode_api::generated::StrongFlowReadCursor> {
    let mut last_state = None;
    let mut last_rejected_cut = None;
    for reopen in 0..2 {
        let mut control_plane = fixture.start_control_plane();
        let mut ingress_storage = SqliteStorage::open(&fixture.root).expect("ingress storage");
        if reopen == 0
            && let Some(failure) = temporary_failure
        {
            assert_temporary_candidate_outage(
                fixture,
                &mut control_plane,
                &mut ingress_storage,
                failure,
            );
        }
        replay_historical_candidate_twice(fixture, &mut control_plane, &mut ingress_storage);
        let (state, rejected) = assert_historical_refusal_state(fixture, &control_plane);
        let read_cut = assert_historical_refusal_read(
            fixture,
            &mut control_plane,
            &state,
            &rejected,
            check_history_and_new_spec,
        );
        if read_cut.is_some() {
            last_rejected_cut = read_cut;
        }
        if reopen == 0 {
            last_state = Some(state);
        } else {
            assert_eq!(Some(state), last_state, "restart replays one refusal only");
        }
        Box::new(ingress_storage).close().expect("ingress close");
        control_plane.shutdown().expect("shutdown");
    }
    last_rejected_cut
}

fn assert_temporary_candidate_outage(
    fixture: &HistoricalCandidateFixture,
    control_plane: &mut ControlPlane,
    ingress_storage: &mut SqliteStorage,
    temporary_failure: &str,
) {
    let before = control_plane
        .load_state(&format!("delivery:{}", fixture.delivery.id().0))
        .expect("state before outage")
        .expect("Delivery before outage");
    let digest = fixture.message.outcome.artifacts[0]
        .digest
        .0
        .strip_prefix("sha256:")
        .expect("candidate digest");
    let object = fixture
        .root
        .join("artifacts/objects/sha256")
        .join(&digest[..2])
        .join(&digest[2..]);
    let parked_object = object.with_extension("unavailable");
    let blocked_repository = fixture.root.join("delivery-sources/repository");
    match temporary_failure {
        "artifact" => fs::rename(&object, &parked_object).expect("temporary object outage"),
        "resolver" => fs::write(&blocked_repository, b"temporary resolver outage")
            .expect("temporary Git command outage"),
        _ => unreachable!(),
    }
    let failure = DurableExecutionPortIngress::new(
        control_plane,
        ingress_storage,
        &fixture.scope,
        fixture.message.sent_at.clone(),
    )
    .expect("outage ingress")
    .handle(&ExecutionPortMessage::JobOutcomeMessage(
        fixture.message.clone(),
    ));
    assert!(
        failure.is_err(),
        "temporary source failure must remain retryable"
    );
    assert_eq!(
        control_plane
            .load_state(&format!("delivery:{}", fixture.delivery.id().0))
            .expect("state during source outage")
            .expect("Delivery retained"),
        before,
        "an unavailable source cannot create a permanent rejection or Attention"
    );
    match temporary_failure {
        "artifact" => fs::rename(parked_object, object).expect("restore exact object"),
        "resolver" => fs::remove_file(blocked_repository).expect("restore resolver"),
        _ => unreachable!(),
    }
}

fn replay_historical_candidate_twice(
    fixture: &HistoricalCandidateFixture,
    control_plane: &mut ControlPlane,
    ingress_storage: &mut SqliteStorage,
) {
    for _ in 0..2 {
        let response = DurableExecutionPortIngress::new(
            control_plane,
            ingress_storage,
            &fixture.scope,
            fixture.message.sent_at.clone(),
        )
        .expect("durable Worker ingress")
        .handle(&ExecutionPortMessage::JobOutcomeMessage(
            fixture.message.clone(),
        ))
        .expect("candidate refusal must converge to a readable product failure");
        let [ExecutionPortMessage::JobOutcomeAckMessage(ack)] = response.as_slice() else {
            panic!("one terminal acknowledgement");
        };
        assert_eq!(
            ack.status,
            JobOutcomeAckMessageStatus::Duplicate,
            "the exact successful terminal was already durably accepted"
        );
    }
}

fn assert_historical_refusal_state(
    fixture: &HistoricalCandidateFixture,
    control_plane: &ControlPlane,
) -> (StoredState, Delivery) {
    let state = control_plane
        .load_state(&format!("delivery:{}", fixture.delivery.id().0))
        .expect("current product state")
        .expect("Delivery retained");
    let rejected = Delivery::decode_json(&state.payload).expect("readable rejected Delivery");
    assert_eq!(rejected.snapshot().status, DeliveryStatus::NeedsAttention);
    assert_eq!(
        rejected
            .snapshot()
            .attention_items
            .iter()
            .filter(|item| item.blocking
                && item.status == winwincode_delivery::domain::AttentionItemStatus::Open)
            .count(),
        1
    );
    assert_eq!(
        rejected.snapshot().work_run_aggregate.runs.len(),
        1,
        "a rejected candidate must not dispatch a Reviewer"
    );
    assert_eq!(
        rejected.snapshot().work_run_aggregate.runs[0].state,
        winwincode_domain::WorkRunState::CandidateReady,
        "the original successful Worker fact remains unchanged"
    );
    assert_eq!(
        control_plane
            .load_state(&fixture.authority_stream)
            .expect("terminal authority")
            .expect("success retained")
            .payload,
        fixture.accepted.payload
    );
    (state, rejected)
}

fn assert_historical_refusal_read(
    fixture: &HistoricalCandidateFixture,
    control_plane: &mut ControlPlane,
    state: &StoredState,
    rejected: &Delivery,
    check_history_and_new_spec: bool,
) -> Option<winwincode_api::generated::StrongFlowReadCursor> {
    let query = winwincode_api::generated::DeliveryGetQuery {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", fixture.seed)),
            kind: winwincode_domain::UserActorKind::User,
        }),
        page: winwincode_api::generated::PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: winwincode_api::generated::DeliveryGetParameters {
            at_cursor: None,
            delivery_id: fixture.delivery.id().clone(),
        },
        query: winwincode_api::generated::DeliveryGetQueryQuery::DeliveryGet,
        request_id: RequestId(canonical_id("req", fixture.seed + 20_000)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.scope.clone(),
    };
    let response = winwincode_control_plane::strongflow_projection::StrongFlowProjectionQueryPort::delivery_get(
        control_plane, &query,
    ).expect("delivery.get stays available after the exact candidate refusal");
    let winwincode_api::generated::QueryResultResponse::DeliveryGetResultResponse(response) =
        response
    else {
        panic!("Delivery detail response");
    };
    assert!(response.result.current_candidate.is_none());
    assert_eq!(response.result.attention.len(), 1);
    if check_history_and_new_spec {
        assert_rejected_candidate_history(
            control_plane,
            &query,
            response.result.read_cursor.clone(),
        );
        assert_generic_refusal_resolution_blocked(fixture, control_plane, &query, state, rejected);
        Some(response.result.read_cursor)
    } else {
        None
    }
}

fn assert_generic_refusal_resolution_blocked(
    fixture: &HistoricalCandidateFixture,
    control_plane: &mut ControlPlane,
    query: &winwincode_api::generated::DeliveryGetQuery,
    state: &StoredState,
    rejected: &Delivery,
) {
    for (offset, decision) in [(1, "resolve"), (2, "dismiss")] {
        let attempted = control_plane.delivery_resolve_attention(
            &winwincode_api::generated::DeliveryResolveAttentionCommand {
                actor: query.actor.clone(), scope: fixture.scope.clone(), schema_version: SchemaVersion::WinwincodeV1,
                command: winwincode_api::generated::DeliveryResolveAttentionCommandCommand::DeliveryResolveAttention,
                expected_revision: Revision(i64::try_from(rejected.revision()).unwrap()),
                request_id: RequestId(canonical_id("req", fixture.seed + 21_000 + offset)),
                payload: winwincode_api::generated::DeliveryResolveAttentionPayload {
                    delivery_id: fixture.delivery.id().clone(), attention_item_id: rejected.snapshot().attention_items[0].id.clone(),
                    decision: decision.into(), remediation: None, resolution: "Attempt generic resume".into(),
                },
            });
        let blocked =
            attempted.expect_err("a rejected source cannot obtain a generic resume authorization");
        assert!(
            blocked
                .to_string()
                .contains("a rejected candidate cannot resume"),
            "the exact typed refusal, rather than an unrelated validation failure, blocks generic resume: {blocked}"
        );
        assert_eq!(
            control_plane
                .load_state(&format!("delivery:{}", fixture.delivery.id().0))
                .expect("state after rejected resume")
                .expect("retained refusal"),
            *state
        );
    }
}

fn assert_historical_candidate_usage(fixture: &HistoricalCandidateFixture) {
    assert_eq!(
        changed_terminal_replay_status(&fixture.root, &fixture.scope, &fixture.message),
        JobOutcomeAckMessageStatus::RejectedConflict
    );
    let connection = rusqlite::Connection::open(fixture.root.join("control-plane.sqlite3"))
        .expect("usage audit");
    let usage: (i64, i64) = connection.query_row(
        "SELECT actual_tokens, actual_cost_microunits FROM execution_admission_reservations WHERE job_id = ?1",
        [&fixture.job.job_id.0], |row| Ok((row.get(0)?, row.get(1)?)),
    ).expect("original successful usage");
    assert_eq!(usage, (40, 400));
    connection.close().expect("usage audit close");
}

fn assert_explicit_spec_restart(
    fixture: &HistoricalCandidateFixture,
    old_cut: winwincode_api::generated::StrongFlowReadCursor,
) {
    let mut control_plane = fixture.start_control_plane();
    let next = replace_refused_spec(fixture, &mut control_plane);
    assert_old_terminal_preserves_new_spec(fixture, &mut control_plane, &next);
    assert_new_spec_and_old_cut_reads(fixture, &control_plane, old_cut);
    control_plane.shutdown().expect("new Spec shutdown");
    assert_rejected_source_retained(fixture);
}

fn restarted_spec_input(
    fixture: &HistoricalCandidateFixture,
    refused: &Delivery,
) -> winwincode_api::generated::DeliverySpecInput {
    let spec = &refused.snapshot().spec;
    winwincode_api::generated::DeliverySpecInput {
        verification_command: None,
        title: "Start a new specification after candidate refusal".into(),
        goal: spec.goal.clone(),
        scope: spec.scope.clone(),
        out_of_scope: spec.out_of_scope.clone(),
        constraints: spec.constraints.clone(),
        base_revision: spec.base_revision.clone(),
        source_product_session_id: spec.source_product_session_id.clone(),
        publication_target: None,
        repository_id: fixture.scope.repository_id.clone(),
        acceptance_criteria: spec
            .acceptance_criteria
            .iter()
            .map(
                |criterion| winwincode_api::generated::AcceptanceCriterionInput {
                    id: criterion.id.0.clone(),
                    title: criterion.description.clone(),
                    required: criterion.required,
                },
            )
            .collect(),
    }
}

fn replace_refused_spec(
    fixture: &HistoricalCandidateFixture,
    control_plane: &mut ControlPlane,
) -> Delivery {
    let state = control_plane
        .load_state(&format!("delivery:{}", fixture.delivery.id().0))
        .expect("refused state")
        .expect("retained refused state");
    let refused = Delivery::decode_json(&state.payload).expect("refused Delivery");
    let spec = &refused.snapshot().spec;
    control_plane
        .delivery_update_spec(&winwincode_api::generated::DeliveryUpdateSpecCommand {
            actor: Actor::UserActor(UserActor {
                id: UserId(canonical_id("usr", fixture.seed)),
                kind: winwincode_domain::UserActorKind::User,
            }),
            scope: fixture.scope.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            command:
                winwincode_api::generated::DeliveryUpdateSpecCommandCommand::DeliveryUpdateSpec,
            expected_revision: Revision(i64::try_from(refused.revision()).unwrap()),
            request_id: RequestId(canonical_id("req", fixture.seed + 22_000)),
            payload: winwincode_api::generated::DeliveryUpdateSpecPayload {
                delivery_id: fixture.delivery.id().clone(),
                spec: restarted_spec_input(fixture, &refused),
            },
        })
        .expect(
            "public Spec update can explicitly start again without accepting the refused candidate",
        );
    let next = control_plane
        .load_state(&format!("delivery:{}", fixture.delivery.id().0))
        .expect("new Spec state")
        .expect("new Spec retained");
    let next = Delivery::decode_json(&next.payload).expect("new Spec Delivery");
    assert_eq!(next.snapshot().status, DeliveryStatus::Ready);
    assert_eq!(next.snapshot().spec.revision, spec.revision + 1);
    assert!(next.snapshot().session_bindings.is_empty());
    assert!(next.snapshot().work_run_aggregate.runs.is_empty());
    assert!(next.snapshot().attention_items.is_empty());
    assert!(next.snapshot().verdict.is_none());
    assert!(next.snapshot().evidence.is_empty());
    assert_eq!(
        control_plane
            .load_state(&fixture.authority_stream)
            .expect("old authority retained")
            .expect("old success retained")
            .payload,
        fixture.accepted.payload
    );
    next
}

fn assert_old_terminal_preserves_new_spec(
    fixture: &HistoricalCandidateFixture,
    control_plane: &mut ControlPlane,
    next: &Delivery,
) {
    let expected_new_spec = next.encode_json().expect("new Spec bytes");
    let before = durable_terminal_counts(&fixture.root, fixture.delivery.id());
    let facts = outcome_facts_for_stage(
        &fixture.delivery,
        &fixture.message,
        fixture
            .message
            .session_identity
            .work_run_id
            .clone()
            .unwrap(),
    );
    let public_replay = control_plane
        .commit_delivery_terminal_outcome(
            &fixture.scope,
            &fixture.message,
            &facts,
            &Instant("2027-01-15T09:00:00.000Z".into()),
        )
        .expect(
            "public exact terminal replay reads historical acceptance before replacement bindings",
        );
    assert!(public_replay.receipt().idempotent_replay);
    assert_accepted_outcome_state(&fixture.root, &fixture.message, true);
    assert_eq!(
        durable_terminal_counts(&fixture.root, fixture.delivery.id()),
        before
    );
    let mut replay_storage = SqliteStorage::open(&fixture.root).expect("new Spec replay storage");
    let replay = DurableExecutionPortIngress::new(
        control_plane,
        &mut replay_storage,
        &fixture.scope,
        fixture.message.sent_at.clone(),
    )
    .expect("old terminal ingress after Spec replacement")
    .handle(&ExecutionPortMessage::JobOutcomeMessage(
        fixture.message.clone(),
    ))
    .expect("old terminal exact replay must stay idempotent after its bindings were replaced");
    let [ExecutionPortMessage::JobOutcomeAckMessage(ack)] = replay.as_slice() else {
        panic!("one old terminal Ack");
    };
    assert_eq!(ack.status, JobOutcomeAckMessageStatus::Duplicate);
    assert_eq!(
        control_plane
            .load_state(&format!("delivery:{}", fixture.delivery.id().0))
            .expect("new Spec after old terminal replay")
            .expect("new Spec remains retained")
            .payload,
        expected_new_spec,
        "a late old terminal cannot change the current Spec or dispatch another run"
    );
    Box::new(replay_storage)
        .close()
        .expect("new Spec replay storage close");
}

fn assert_new_spec_and_old_cut_reads(
    fixture: &HistoricalCandidateFixture,
    control_plane: &ControlPlane,
    old_cut: winwincode_api::generated::StrongFlowReadCursor,
) {
    let query = winwincode_api::generated::DeliveryGetQuery {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", fixture.seed)),
            kind: winwincode_domain::UserActorKind::User,
        }),
        page: winwincode_api::generated::PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: winwincode_api::generated::DeliveryGetParameters {
            at_cursor: None,
            delivery_id: fixture.delivery.id().clone(),
        },
        query: winwincode_api::generated::DeliveryGetQueryQuery::DeliveryGet,
        request_id: RequestId(canonical_id("req", fixture.seed + 23_000)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: fixture.scope.clone(),
    };
    let response = winwincode_control_plane::strongflow_projection::StrongFlowProjectionQueryPort::delivery_get(control_plane, &query)
        .expect("new Spec detail readable");
    let winwincode_api::generated::QueryResultResponse::DeliveryGetResultResponse(response) =
        response
    else {
        panic!("Delivery detail");
    };
    assert!(response.result.current_candidate.is_none());
    assert_rejected_candidate_history(control_plane, &query, response.result.read_cursor);
    let mut old_query = query.clone();
    old_query.parameters.at_cursor = Some(old_cut.clone());
    let old_response = winwincode_control_plane::strongflow_projection::StrongFlowProjectionQueryPort::delivery_get(control_plane, &old_query)
        .expect("the original rejected cut remains readable after public Spec replacement");
    let winwincode_api::generated::QueryResultResponse::DeliveryGetResultResponse(old_response) =
        old_response
    else {
        panic!("historical rejected detail");
    };
    assert_eq!(old_response.result.read_cursor, old_cut);
    assert!(old_response.result.current_candidate.is_none());
    assert_eq!(old_response.result.attention.len(), 1);
    assert_rejected_candidate_history(control_plane, &old_query, old_cut);
}

fn assert_rejected_source_retained(fixture: &HistoricalCandidateFixture) {
    let digest = &fixture.message.outcome.artifacts[0].digest;
    let digest_hex = digest
        .0
        .strip_prefix("sha256:")
        .expect("retained source digest");
    let source_bytes = fs::read(
        fixture
            .root
            .join("artifacts/objects/sha256")
            .join(&digest_hex[..2])
            .join(&digest_hex[2..]),
    )
    .expect("original source bytes retained after new Spec");
    assert_eq!(
        format!("sha256:{:x}", Sha256::digest(source_bytes)),
        digest.0,
        "the failed old candidate source is never rewritten"
    );
    let objects = LocalArtifactObjectStore::open(fixture.root.join("artifacts"))
        .expect("old Artifact objects");
    let artifacts = ArtifactStore::open(fixture.root.join("artifact-catalog"), Box::new(objects))
        .expect("old Artifact catalog");
    artifacts
        .close()
        .expect("old Artifact catalog remains openable");
    assert_eq!(
        worker_terminal_state(&fixture.root, &fixture.job.job_id),
        ("settled".into(), 1)
    );
}

fn assert_rejected_candidate_history(
    control_plane: &ControlPlane,
    read: &winwincode_api::generated::DeliveryGetQuery,
    cursor: winwincode_api::generated::StrongFlowReadCursor,
) {
    let history = winwincode_control_plane::strongflow_projection::StrongFlowProjectionQueryPort::candidate_history_list(
        control_plane, &winwincode_api::generated::CandidateHistoryListQuery {
            actor: read.actor.clone(), scope: read.scope.clone(), page: read.page.clone(), schema_version: read.schema_version.clone(),
            request_id: read.request_id.clone(), query: winwincode_api::generated::CandidateHistoryListQueryQuery::CandidateList,
            parameters: winwincode_api::generated::CandidateHistoryListParameters {
                at_cursor: cursor, delivery_id: read.parameters.delivery_id.clone(), read_page_limit: read.page.limit,
            },
        },
    ).expect("candidate.list must remain readable without inventing a Candidate for a rejected historical success");
    let winwincode_api::generated::QueryResultResponse::CandidateHistoryListResultResponse(history) =
        history
    else {
        panic!("Candidate history");
    };
    assert!(
        history.result.items.is_empty(),
        "an invalid successful output is never a legal Candidate"
    );
}

#[test]
fn successful_writer_settles_candidate_ready_and_replays_after_lease_expiry() {
    let seed = 60;
    let root = temporary_directory("successful-non-final");
    let repository = root.join("repository");
    let (base_commit, candidate_commit) = initialize_git_repository(&repository);
    let repository = fs::canonicalize(repository).expect("canonical repository");
    let scope = repository_scope(seed);
    let delivery = repository_executor_delivery(seed, &repository, &base_commit);
    let job = execution_job(&delivery, &scope);
    let mut message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    seed_candidate_artifact(
        &root,
        &repository,
        &base_commit,
        &scope,
        &delivery,
        &mut message,
        &candidate_commit,
        seed,
    );
    let facts = outcome_facts_for_stage(
        &delivery,
        &message,
        winwincode_domain::WorkRunId(canonical_id("wrn", seed)),
    );
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let pending = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("persist successful executor handoff");
    assert_eq!(pending.receipt().revision, delivery.revision() + 1);
    assert_eq!(pending.receipt().events.len(), 3);
    assert_eq!(
        pending.receipt().stream_id,
        format!("delivery:{}", delivery.id().0)
    );
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (2, 2, 3, 5));
    assert_accepted_outcome_state(&root, &message, true);
    control_plane.shutdown().expect("shutdown");

    let mut restarted = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .expect("restart with production Delivery adapters");
    let terminal_replay = restarted
        .commit_delivery_terminal_outcome(
            &scope,
            &message,
            &facts,
            &Instant("2027-01-15T09:00:00.000Z".into()),
        )
        .expect("terminal receipt replay after restart and lease expiry");
    assert!(terminal_replay.receipt().idempotent_replay);
    assert_eq!(terminal_replay.receipt().events, pending.receipt().events);

    let delivery_state = restarted
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("Delivery state read after expired lease")
        .expect("Delivery state remains present");
    assert_eq!(delivery_state.revision, delivery.revision() + 1);
    let settled = Delivery::decode_json(&delivery_state.payload).expect("settled Delivery");
    let aggregate = &settled.snapshot().work_run_aggregate;
    let run = aggregate
        .runs
        .iter()
        .find(|run| Some(&run.id) == message.session_identity.work_run_id.as_ref())
        .expect("exact producer run");
    assert_eq!(run.state, winwincode_domain::WorkRunState::CandidateReady);
    let item = aggregate
        .items
        .iter()
        .find(|item| item.id == run.work_item_id)
        .expect("exact producer item");
    assert_eq!(item.state, winwincode_domain::WorkItemState::CandidateReady);
    assert_eq!(
        restarted
            .load_state(&format!("delivery-terminal-authority:{}", job.job_id.0))
            .expect("terminal authority read")
            .expect("terminal authority state")
            .revision,
        1
    );
    restarted.shutdown().expect("second shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn generic_control_plane_commit_cannot_forge_terminal_delivery_state() {
    let seed = 61;
    let root = temporary_directory("generic-terminal-bypass");
    let scope = repository_scope(seed);
    let (delivery, candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    seed_delivery_and_job(&root, &delivery, &job);
    let before = durable_terminal_counts(&root, delivery.id());
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    let mut command = verdict_command(seed, &delivery, &candidate);
    command.command = CommandName::SessionCancel;
    command.request_id = RequestId(canonical_id("req", seed + 2_000));
    command.payload = serde_json::json!({});

    let error = control_plane
        .commit(
            &command,
            StateChange::new(
                format!("delivery:{}", delivery.id().0),
                b"forged-terminal-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    "forged-terminal-event",
                    "delivery.work_run.terminal",
                    b"forged".to_vec(),
                )],
            ),
        )
        .expect_err("generic commit must not write a reserved Delivery stream");
    assert!(matches!(
        error,
        CommitError::Storage(ref source)
            if source.kind() == winwincode_control_plane::StorageErrorKind::InvalidInput
    ));
    command.expected_revision = Revision(0);
    command.request_id = RequestId(canonical_id("req", seed + 2_001));
    let error = control_plane
        .commit(
            &command,
            StateChange::new(
                "worker:terminal-topic-bypass",
                b"unrelated-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    "forged-terminal-topic",
                    "delivery.work_run.terminal",
                    b"forged".to_vec(),
                )],
            ),
        )
        .expect_err("generic commit must not publish a reserved terminal topic");
    assert!(matches!(
        error,
        CommitError::Storage(ref source)
            if source.kind() == winwincode_control_plane::StorageErrorKind::InvalidInput
    ));
    assert!(
        control_plane
            .load_state("worker:terminal-topic-bypass")
            .expect("generic bypass state read")
            .is_none()
    );
    assert_eq!(durable_terminal_counts(&root, delivery.id()), before);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn receipt_replay_rejects_changed_digest_event_membership_or_terminal_payload() {
    for (offset, corruption) in ["digest", "event-membership", "terminal-payload"]
        .into_iter()
        .enumerate()
    {
        let seed = 70 + u64::try_from(offset).expect("small corruption index");
        let root = temporary_directory(corruption);
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
            .expect("initial terminal outcome");
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("receipt corruption injector");
        match corruption {
            "digest" => {
                connection
                    .execute(
                        "UPDATE command_receipts SET command_digest = ?1 WHERE stream_id = ?2 AND revision = 2",
                        rusqlite::params![
                            format!("sha256:{}", "f".repeat(64)),
                            format!("delivery:{}", delivery.id().0)
                        ],
                    )
                    .expect("replace terminal receipt digest");
            }
            "event-membership" => {
                connection
                    .execute(
                        "UPDATE outbox SET \
                           receipt_actor_key = (SELECT actor_key FROM command_receipts WHERE stream_id = ?1 AND revision = 1), \
                           receipt_scope_key = (SELECT scope_key FROM command_receipts WHERE stream_id = ?1 AND revision = 1), \
                           request_id = (SELECT request_id FROM command_receipts WHERE stream_id = ?1 AND revision = 1) \
                         WHERE topic = 'delivery.work_run.terminal'",
                        [format!("delivery:{}", delivery.id().0)],
                    )
                    .expect("move terminal event to seed receipt");
            }
            "terminal-payload" => {
                connection
                    .execute(
                        "UPDATE outbox SET payload = X'7b7d' WHERE topic = 'delivery.work_run.terminal'",
                        [],
                    )
                    .expect("replace terminal event payload");
            }
            _ => unreachable!(),
        }
        connection
            .close()
            .expect("receipt corruption injector close");

        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
            .expect_err("corrupt terminal receipt must fail closed");
        assert_eq!(
            control_plane
                .load_state(&format!("delivery:{}", delivery.id().0))
                .expect("state read")
                .expect("Delivery state")
                .revision,
            delivery.revision() + 1,
            "{corruption}"
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

struct TerminalDeliveryRaceStorage {
    inner: SqliteStorage,
    delivery_stream: String,
    injected: bool,
    fail_post_commit_audit_read: bool,
}

impl ProductStateStorage for TerminalDeliveryRaceStorage {
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        self.inner.commit(commit)
    }

    fn commit_with_execution_terminal(
        &mut self,
        commit: &StateCommit,
        guard: &winwincode_storage::ExecutionAuthorityCommitGuard,
        terminal: &winwincode_storage::RepositorySchedulerTerminalRequest,
    ) -> Result<CommitReceipt, StorageError> {
        if !self.injected && commit.stream_id == self.delivery_stream {
            self.injected = true;
            let changed = rusqlite::Connection::open(self.inner.database_path())
                .and_then(|connection| {
                    connection.execute(
                        "UPDATE product_state SET revision = revision + 1, \
                         payload = CAST(json_set(CAST(payload AS TEXT), '$.revision', revision + 1) AS BLOB) \
                         WHERE stream_id = ?1",
                        [&self.delivery_stream],
                    )
                })
                .map_err(|error| StorageError::adapter(error.to_string()))?;
            assert_eq!(changed, 1, "concurrent Delivery update must actually occur");
        }
        self.inner
            .commit_with_execution_terminal(commit, guard, terminal)
    }

    fn load_execution_job_record(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<winwincode_storage::ExecutionJobRecord>, StorageError> {
        self.inner.load_execution_job_record(job_id)
    }

    fn load_execution_dispatch_authority(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<winwincode_storage::ExecutionDispatchAuthority>, StorageError> {
        self.inner.load_execution_dispatch_authority(job_id)
    }

    fn load_execution_terminal_resources(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<winwincode_storage::ExecutionTerminalResources>, StorageError> {
        self.inner.load_execution_terminal_resources(job_id)
    }

    fn load_receipt(
        &self,
        identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        self.inner.load_receipt(identity, command_digest)
    }

    fn load_pending_audit_event(
        &self,
        identity: &ReceiptIdentity,
    ) -> Result<Option<PendingAuditEvent>, StorageError> {
        if self.fail_post_commit_audit_read {
            return Err(StorageError::adapter(
                "injected accepted pending-audit read failure",
            ));
        }
        self.inner.load_pending_audit_event(identity)
    }

    fn pending_audit_events(&self) -> Result<Vec<PendingAuditEvent>, StorageError> {
        self.inner.pending_audit_events()
    }

    fn load_outbox_event(
        &self,
        event_id: &str,
    ) -> Result<Option<DurableOutboxEvent>, StorageError> {
        self.inner.load_outbox_event(event_id)
    }

    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        self.inner.load_state(stream_id)
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
fn successful_report_rejects_delivery_changed_between_authority_read_and_commit() {
    let seed = 801;
    let root = temporary_directory("terminal-delivery-race");
    let scope = repository_scope(seed);
    let delivery = running_non_final_executor(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut storage = TerminalDeliveryRaceStorage {
        inner: SqliteStorage::open(&root).expect("race storage"),
        delivery_stream: format!("delivery:{}", delivery.id().0),
        injected: false,
        fail_post_commit_audit_read: false,
    };
    let before = durable_terminal_counts(&root, delivery.id());
    let audits_before = storage.pending_audit_events().expect("audit baseline");
    let result = winwincode_control_plane::test_support::commit_terminal_transaction(
        &mut storage,
        &scope,
        &message,
        &facts,
        &message.sent_at,
    );
    assert!(
        storage.injected,
        "test must reach the guarded commit before failing"
    );
    assert_eq!(
        storage
            .load_state(&format!("delivery:{}", delivery.id().0))
            .expect("Delivery read")
            .expect("Delivery exists")
            .revision,
        delivery.revision() + 1,
        "test must reach the concurrent write: {result:?}"
    );
    assert!(
        storage
            .load_state(&format!("delivery-terminal-authority:{}", job.job_id.0))
            .expect("terminal authority read")
            .is_none(),
        "stale report must not be committed"
    );
    assert!(
        result.is_err(),
        "concurrent Delivery update must reject the stale report"
    );
    assert_eq!(terminal_receipt_count(&root, delivery.id()), 0);
    assert_eq!(
        durable_terminal_counts(&root, delivery.id()),
        (before.0 + 1, before.1, before.2, before.3),
        "rejected report must not write a journal, receipt or outbox event"
    );
    assert_eq!(
        storage.pending_audit_events().expect("audit after race"),
        audits_before
    );
    Box::new(storage).close().expect("close storage");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn terminal_resources_use_the_original_reservation_and_exact_worker_slot() {
    let seed = 802;
    let root = temporary_directory("terminal-resource-authority");
    let scope = repository_scope(seed);
    let delivery = running_non_final_executor(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut storage = SqliteStorage::open(&root).expect("resource storage");
    let resources = storage
        .load_execution_terminal_resources(&job.job_id)
        .expect("canonical resources")
        .expect("reservation and slot exist");
    let reservation = storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job.job_id)
        .unwrap()
        .unwrap();
    let slot = storage
        .worker_session_slots()
        .unwrap()
        .load(&message.worker_session_id)
        .unwrap()
        .unwrap();
    assert_eq!(resources.scope, reservation.scope);
    assert_eq!(resources.worker_pool_id, reservation.worker_pool_id);
    assert_eq!(resources.authority, slot.authority);
    assert_eq!(resources.scope.delivery_id.as_ref(), Some(delivery.id()));
    assert_eq!(
        resources.worker_pool_id,
        WorkerPoolId(canonical_id("wpl", seed))
    );
    let before = durable_terminal_counts(&root, delivery.id());
    let connection = rusqlite::Connection::open(storage.database_path()).unwrap();
    connection
        .execute(
            "UPDATE worker_session_slots SET worker_instance_id=?1 WHERE worker_session_id=?2",
            rusqlite::params![canonical_id("wki", seed + 1), message.worker_session_id.0],
        )
        .unwrap();
    assert!(
        storage
            .load_execution_terminal_resources(&job.job_id)
            .is_err(),
        "a canonical but foreign Worker instance cannot become cleanup authority"
    );
    connection
        .execute(
            "UPDATE worker_session_slots SET worker_instance_id=?1 WHERE worker_session_id=?2",
            rusqlite::params![
                message.lease.worker_instance_id.0,
                message.worker_session_id.0
            ],
        )
        .unwrap();
    assert_eq!(
        storage
            .load_execution_terminal_resources(&job.job_id)
            .unwrap(),
        Some(resources)
    );
    assert_eq!(
        durable_terminal_counts(&root, delivery.id()),
        before,
        "resource reads append no product, receipt, journal or event facts"
    );
    assert_eq!(
        storage
            .load_execution_terminal_resources(&ExecutionJobId(canonical_id("job", seed + 1)))
            .unwrap(),
        None
    );
    drop(connection);
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "One cancellation fixture checks effective product state and exact original replay together"
)]
fn earlier_cancellation_owns_delivery_terminal_while_the_original_worker_frame_stays_exact() {
    let seed = 803;
    let root = temporary_directory("delivery-cancellation-before-success");
    let scope = repository_scope(seed);
    let repository = root.join("repository");
    let (base_commit, _) = initialize_git_repository(&repository);
    let repository = fs::canonicalize(repository).unwrap();
    let delivery = repository_executor_delivery(seed, &repository, &base_commit);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut storage = SqliteStorage::open(&root).unwrap();
    let queued = storage
        .load_execution_job_record(&job.job_id)
        .unwrap()
        .unwrap();
    storage
        .execution_queue()
        .unwrap()
        .request_cancellation(&winwincode_storage::ExecutionJobCancellationRequest {
            scope: queued.scope,
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 30_000)),
            expected_revision: queued.revision,
            requested_at: Instant("2027-01-15T08:01:02.000Z".into()),
        })
        .unwrap();
    Box::new(storage).close().unwrap();
    let accepted_at = Instant("2027-01-15T08:01:03.000Z".into());
    let before_dispatch = execution_job_counts(&root);
    let mut control_plane = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .unwrap();
    let first = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &accepted_at)
        .unwrap();
    let stored = control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .unwrap()
        .unwrap();
    let settled = Delivery::decode_json(&stored.payload).unwrap();
    let run = settled
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.execution_job_id == job.job_id)
        .unwrap();
    assert_eq!(run.state, winwincode_domain::WorkRunState::Cancelled);
    assert!(
        settled
            .snapshot()
            .work_run_aggregate
            .items
            .iter()
            .all(|item| item.state != winwincode_domain::WorkItemState::Done)
    );
    let accepted = control_plane
        .load_state(&format!("execution-outcome:{}:1", job.job_id.0))
        .unwrap()
        .unwrap();
    let frame: serde_json::Value = serde_json::from_slice(&accepted.payload).unwrap();
    assert_eq!(
        accepted.revision, 2,
        "resource cleanup and publication finish before readiness"
    );
    assert_eq!(frame["message"], serde_json::to_value(&message).unwrap());
    assert_eq!(
        frame["decision"]["status"],
        serde_json::json!(ExecutionOutcomeStatus::Cancelled)
    );
    assert_eq!(
        frame["decision"]["terminalAt"],
        serde_json::json!(accepted_at)
    );
    assert_eq!(
        frame["followUp"]["completedProfile"],
        serde_json::json!(job.execution_profile)
    );
    assert_eq!(
        frame["followUp"]["controllerRequired"], true,
        "the original accepting host owns automatic progression"
    );
    assert_eq!(
        execution_job_counts(&root),
        before_dispatch,
        "effective cancellation never starts Controller work"
    );
    assert_eq!(
        first.receipt().command_digest,
        Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&message).unwrap())
        ))
    );
    let event = first
        .receipt()
        .events
        .iter()
        .find(|event| event.topic == "delivery.work_run.terminal")
        .unwrap();
    let terminal: serde_json::Value = serde_json::from_slice(&event.payload).unwrap();
    assert_eq!(
        terminal["outcome"]["status"],
        serde_json::json!(ExecutionOutcomeStatus::Cancelled)
    );
    assert_eq!(
        terminal["outcome"]["finishedAtMillis"],
        1_800_000_063_000_u64
    );
    let before = durable_terminal_counts(&root, delivery.id());
    let replay = control_plane
        .commit_delivery_terminal_outcome(
            &scope,
            &message,
            &facts,
            &Instant("2027-01-15T08:10:00.000Z".into()),
        )
        .unwrap();
    assert!(replay.receipt().idempotent_replay);
    assert_eq!(replay.receipt().events, first.receipt().events);
    assert_eq!(durable_terminal_counts(&root, delivery.id()), before);
    let mut changed = message.clone();
    changed.outcome.status = ExecutionOutcomeStatus::Cancelled;
    assert!(
        control_plane
            .commit_delivery_terminal_outcome(&scope, &changed, &facts, &accepted_at,)
            .is_err(),
        "the effective decision never replaces the exact raw replay frame"
    );
    assert_eq!(durable_terminal_counts(&root, delivery.id()), before);
    control_plane.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn accepted_delivery_cleanup_failure_blocks_controller_until_exact_restart_recovery() {
    assert_controller_completion_cut(804, true);
}

#[test]
fn accepted_delivery_publication_failure_blocks_controller_until_exact_restart_recovery() {
    assert_controller_completion_cut(805, false);
}

#[allow(
    clippy::too_many_lines,
    reason = "A real writer checks one durable cut through absent-Controller and configured-Controller restarts"
)]
fn assert_controller_completion_cut(seed: u64, fail_resources: bool) {
    let root = temporary_directory("delivery-controller-completion-cut");
    let repository = root.join("repository");
    let (base_commit, candidate_commit) = initialize_git_repository(&repository);
    let repository = fs::canonicalize(repository).unwrap();
    let scope = repository_scope(seed);
    let delivery = repository_executor_delivery(seed, &repository, &base_commit);
    let job = execution_job(&delivery, &scope);
    let mut message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    seed_candidate_artifact(
        &root,
        &repository,
        &base_commit,
        &scope,
        &delivery,
        &mut message,
        &candidate_commit,
        seed,
    );
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let before_dispatch = execution_job_counts(&root);
    if fail_resources {
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_controller_resource_cut
             BEFORE UPDATE ON execution_admission_reservations
             WHEN OLD.state = 'running' AND NEW.state = 'settled'
             BEGIN SELECT RAISE(ABORT, 'injected Controller resource cut'); END;",
            )
            .unwrap();
        connection.close().unwrap();
    }
    let publisher: Box<dyn EventPublisher> = if fail_resources {
        Box::new(RecordingPublisher)
    } else {
        Box::new(FailingPublisher)
    };
    let mut first = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        publisher,
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .unwrap();
    let pending = first
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect_err("injected completion cut must occur after acceptance");
    assert!(
        matches!(&pending,
            DeliveryTerminalOutcomeCommitError::CompletionPending { source, .. }
                if if fail_resources {
                matches!(source.as_ref(), DurableExecutionPortError::WorkerLifecycle(cause)
                    if cause.kind() == winwincode_control_plane::WorkerExecutionLifecycleErrorKind::OperationalAdmission)
                } else {
                    matches!(source.as_ref(), DurableExecutionPortError::Publication(_))
                }
        ),
        "actual completion failure: {pending}"
    );
    let original_receipt = pending.committed_receipt().unwrap().receipt().clone();
    assert_accepted_outcome_state(&root, &message, false);
    let accepted = first
        .load_state(&format!("execution-outcome:{}:1", job.job_id.0))
        .unwrap()
        .unwrap();
    let frozen: serde_json::Value = serde_json::from_slice(&accepted.payload).unwrap();
    assert_eq!(frozen["followUp"]["controllerRequired"], true);
    assert_eq!(frozen["followUp"]["completedProfile"], "executor");
    assert_eq!(
        execution_job_counts(&root),
        before_dispatch,
        "no follow-up dispatch precedes resource completion and required publication"
    );
    if fail_resources {
        first.shutdown().unwrap();
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3")).unwrap();
        connection
            .execute_batch("DROP TRIGGER fail_controller_resource_cut;")
            .unwrap();
        connection.close().unwrap();
    } else {
        first
            .shutdown()
            .expect_err("publication cut leaves required events pending");
    }
    let mut manual = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .unwrap();
    let pending = manual
        .commit_delivery_terminal_outcome(
            &scope,
            &message,
            &facts,
            &Instant("2027-01-15T08:10:00.000Z".into()),
        )
        .expect_err("frozen automatic continuation still requires the original Controller mode");
    assert!(matches!(&pending,
        DeliveryTerminalOutcomeCommitError::CompletionPending { source, .. }
            if matches!(source.as_ref(), DurableExecutionPortError::Controller(_))
    ));
    assert_eq!(
        pending.committed_receipt().unwrap().receipt().events,
        original_receipt.events
    );
    assert_accepted_outcome_state(&root, &message, false);
    assert_eq!(manual.recover_accepted_execution_outcomes().unwrap(), 0);
    assert_accepted_outcome_state(&root, &message, false);
    assert_eq!(execution_job_counts(&root), before_dispatch);
    manual.shutdown().unwrap();
    let mut restored = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .unwrap();
    assert_eq!(restored.recover_accepted_execution_outcomes().unwrap(), 1);
    let replay = restored
        .commit_delivery_terminal_outcome(
            &scope,
            &message,
            &facts,
            &Instant("2027-01-15T08:10:00.000Z".into()),
        )
        .expect("configured production Controller retains the recovered original follow-up");
    assert!(replay.receipt().idempotent_replay);
    assert_eq!(replay.receipt().events, original_receipt.events);
    assert_accepted_outcome_state(&root, &message, true);
    assert_eq!(
        execution_job_counts(&root),
        (before_dispatch.0 + 1, before_dispatch.1 + 1),
        "exactly one production reviewer Job is queued"
    );
    let delivery_after = restored
        .load_state(&format!("delivery:{}", delivery.id().0))
        .unwrap()
        .unwrap();
    let after = durable_terminal_counts(&root, delivery.id());
    restored
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .unwrap();
    assert_eq!(durable_terminal_counts(&root, delivery.id()), after);
    assert_eq!(
        restored
            .load_state(&format!("delivery:{}", delivery.id().0))
            .unwrap()
            .unwrap(),
        delivery_after
    );
    assert_eq!(
        execution_job_counts(&root),
        (before_dispatch.0 + 1, before_dispatch.1 + 1)
    );
    restored.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn accepted_terminal_audit_read_failure_retains_original_commit_until_exact_replay() {
    let seed = 811;
    let root = temporary_directory("accepted-terminal-audit-read");
    let scope = repository_scope(seed);
    let delivery = running_non_final_executor(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Failed);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut storage = TerminalDeliveryRaceStorage {
        inner: SqliteStorage::open(&root).unwrap(),
        delivery_stream: format!("delivery:{}", delivery.id().0),
        injected: true,
        fail_post_commit_audit_read: true,
    };
    let pending = winwincode_control_plane::test_support::commit_terminal_transaction(
        &mut storage,
        &scope,
        &message,
        &facts,
        &message.sent_at,
    )
    .unwrap_err();
    let DeliveryTerminalOutcomeCommitError::CompletionPending { commit, source } = pending else {
        panic!("accepted outcome must preserve its original commit")
    };
    let DurableExecutionPortError::Storage(source) = source.as_ref() else {
        panic!("accepted audit failure must preserve its typed storage cause")
    };
    assert_eq!(
        source.to_string(),
        "injected accepted pending-audit read failure"
    );
    assert_eq!(
        commit.receipt().command_digest,
        Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&message).unwrap())
        ))
    );
    assert!(!commit.receipt().idempotent_replay);
    let original = storage
        .load_state(&format!("execution-outcome:{}:1", job.job_id.0))
        .unwrap()
        .unwrap();
    let retained: serde_json::Value = serde_json::from_slice(&original.payload).unwrap();
    assert_eq!(original.revision, 1);
    assert_eq!(retained["message"], serde_json::to_value(&message).unwrap());
    assert_eq!(retained["acknowledgementReady"], false);
    let counts = durable_terminal_counts(&root, delivery.id());
    let retry = winwincode_control_plane::test_support::commit_terminal_transaction(
        &mut storage,
        &scope,
        &message,
        &facts,
        &message.sent_at,
    )
    .unwrap_err();
    let retry_commit = retry
        .committed_receipt()
        .expect("exact pending replay retains original receipt");
    assert_eq!(
        retry_commit.receipt().receipt_identity,
        commit.receipt().receipt_identity
    );
    assert_eq!(durable_terminal_counts(&root, delivery.id()), counts);
    storage.fail_post_commit_audit_read = false;
    let replay = winwincode_control_plane::test_support::commit_terminal_transaction(
        &mut storage,
        &scope,
        &message,
        &facts,
        &message.sent_at,
    )
    .unwrap();
    assert!(replay.receipt().idempotent_replay);
    assert_eq!(
        replay.receipt().receipt_identity,
        commit.receipt().receipt_identity
    );
    assert_eq!(
        storage
            .load_state(&format!("execution-outcome:{}:1", job.job_id.0))
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(durable_terminal_counts(&root, delivery.id()), counts);
    Box::new(storage).close().unwrap();
    assert_exact_ingress_preserves_corrupt_accepted_audit(
        &root,
        &scope,
        &message,
        commit.receipt(),
        &original,
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn delivery_cancel_ack_uses_original_scheduler_control_without_terminal_effects() {
    use winwincode_control_plane::{RepositoryExecutionScheduler, WorkRunCancellationRequest};
    use winwincode_execution_port::generated::{
        JobCancelAckMessage, JobCancelAckMessageKind, JobCancelAckMessageStatus,
    };
    use winwincode_storage::RepositorySchedulerScope;

    for (index, status) in [
        JobCancelAckMessageStatus::Accepted,
        JobCancelAckMessageStatus::RejectedExpiredLease,
        JobCancelAckMessageStatus::RejectedStaleFencingToken,
        JobCancelAckMessageStatus::RejectedWorkerInstance,
    ]
    .into_iter()
    .enumerate()
    {
        let seed = 160 + index as u64;
        let root = temporary_directory("delivery-cancel-control-ack");
        let scope = repository_scope(seed);
        let delivery = running_non_final_executor(seed);
        let job = execution_job(&delivery, &scope);
        let outcome = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Cancelled);
        seed_delivery_and_job(&root, &delivery, &job);
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .unwrap();
        let mut storage = SqliteStorage::open(&root).unwrap();
        let stream = format!("delivery:{}", delivery.id().0);
        let state = storage.load_state(&stream).unwrap().unwrap();
        let cancel = RepositoryExecutionScheduler::new(&mut storage)
            .request_cancellation_for_work_run(&WorkRunCancellationRequest {
                scope: RepositorySchedulerScope {
                    organization_id: scope.organization_id.clone(),
                    workspace_id: scope.workspace_id.clone(),
                    project_id: scope.project_id.clone(),
                    repository_id: scope.repository_id.clone(),
                },
                delivery_id: delivery.id().clone(),
                work_run_id: outcome.session_identity.work_run_id.clone().unwrap(),
                request_id: RequestId(canonical_id("req", seed + 19_000)),
                expected_delivery_revision: state.revision,
                requested_at: Instant("2027-01-15T08:00:01.000Z".into()),
                public_command_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            })
            .unwrap()
            .unwrap();
        let ack = JobCancelAckMessage {
            error: None,
            kind: JobCancelAckMessageKind::JobCancelAck,
            lease: cancel.lease.clone(),
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 19_001)),
            request_id: cancel.request_id.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:02.000Z".into()),
            session_identity: cancel.session_identity.clone(),
            status,
            worker_session_id: cancel.worker_session_id.clone(),
        };
        let before = delivery_cancel_ack_resources(&mut storage, &stream, &outcome);
        let connection = rusqlite::Connection::open(storage.database_path()).unwrap();
        let counts = delivery_cancel_ack_counts(&connection);
        let message = ExecutionPortMessage::JobCancelAckMessage(ack.clone());
        assert!(
            DurableExecutionPortIngress::new(
                &mut control_plane,
                &mut storage,
                &scope,
                ack.sent_at.clone()
            )
            .unwrap()
            .handle(&message)
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            delivery_cancel_ack_resources(&mut storage, &stream, &outcome),
            before
        );
        assert_eq!(
            delivery_cancel_ack_counts(&connection),
            (counts.0 + 1, counts.1 + 1, counts.2)
        );
        let retained: Vec<u8> = connection.query_row("SELECT payload FROM product_state WHERE stream_id LIKE 'execution-cancellation-ack:%'", [], |row| row.get(0)).unwrap();
        let retained: serde_json::Value = serde_json::from_slice(&retained).unwrap();
        assert_eq!(retained["message"], serde_json::to_value(&ack).unwrap());
        assert_eq!(
            retained["originalCancelMessageId"],
            serde_json::to_value(&cancel.message_id).unwrap()
        );
        assert_eq!(
            storage
                .load_execution_job_record(&job.job_id)
                .unwrap()
                .unwrap()
                .state,
            ExecutionJobState::Cancelling
        );
        let replay_counts = delivery_cancel_ack_counts(&connection);
        assert!(
            DurableExecutionPortIngress::new(
                &mut control_plane,
                &mut storage,
                &scope,
                Instant("2027-01-15T10:00:00.000Z".into())
            )
            .unwrap()
            .handle(&message)
            .unwrap()
            .is_empty()
        );
        assert_eq!(delivery_cancel_ack_counts(&connection), replay_counts);
        assert_eq!(
            delivery_cancel_ack_resources(&mut storage, &stream, &outcome),
            before
        );
        drop(connection);
        Box::new(storage).close().unwrap();
        control_plane.shutdown().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

fn delivery_cancel_ack_resources(
    storage: &mut SqliteStorage,
    stream: &str,
    outcome: &JobOutcomeMessage,
) -> Vec<u8> {
    let state = storage.load_state(stream).unwrap().unwrap();
    serde_json::to_vec(&(
        (state.stream_id, state.revision, state.payload),
        storage
            .load_execution_job_record(&outcome.lease.job_id)
            .unwrap()
            .unwrap(),
        storage
            .execution_registry()
            .unwrap()
            .load_lease(&outcome.lease.job_id)
            .unwrap()
            .unwrap(),
        storage
            .execution_admission()
            .unwrap()
            .load_reservation_by_job(&outcome.lease.job_id)
            .unwrap()
            .unwrap(),
        storage
            .worker_session_slots()
            .unwrap()
            .load(&outcome.worker_session_id)
            .unwrap()
            .unwrap(),
    ))
    .unwrap()
}

fn delivery_cancel_ack_counts(connection: &rusqlite::Connection) -> (i64, i64, i64) {
    connection.query_row("SELECT (SELECT COUNT(*) FROM command_receipts), (SELECT COUNT(*) FROM outbox), (SELECT COUNT(*) FROM execution_lease_terminals)", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
}

#[allow(
    clippy::too_many_lines,
    reason = "one accepted receipt crosses startup cleanup, audit corruption and exact replay in the same fixture"
)]
fn assert_exact_ingress_preserves_corrupt_accepted_audit(
    root: &Path,
    scope: &RepositoryScope,
    message: &JobOutcomeMessage,
    original_receipt: &CommitReceipt,
    original_state: &StoredState,
) {
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root),
        Box::new(RecordingPublisher),
    )
    .unwrap();
    let mut storage = SqliteStorage::open(root).unwrap();
    let recovered = storage
        .load_state(&original_state.stream_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered.revision, 2,
        "startup finishes the already accepted cleanup"
    );
    let before: serde_json::Value = serde_json::from_slice(&original_state.payload).unwrap();
    let after: serde_json::Value = serde_json::from_slice(&recovered.payload).unwrap();
    assert_eq!(after["message"], before["message"]);
    assert_eq!(after["decision"], before["decision"]);
    assert_eq!(after["acknowledgementReady"], true);
    let connection = rusqlite::Connection::open(storage.database_path()).unwrap();
    let (event_id, payload): (String, Vec<u8>) = connection
        .query_row(
            "SELECT event_id, payload FROM audit_outbox WHERE request_id=?1",
            [&original_receipt.receipt_identity.request_id().0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE audit_outbox SET payload=X'7b7d' WHERE event_id=?1",
                [&event_id]
            )
            .unwrap(),
        1
    );
    let counts = delivery_cancel_ack_counts(&connection);
    let frame = ExecutionPortMessage::JobOutcomeMessage(message.clone());
    let error = DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        scope,
        Instant("2027-01-15T10:00:00.000Z".into()),
    )
    .unwrap()
    .handle(&frame)
    .unwrap_err();
    let DurableExecutionPortError::Terminal(error) = error else {
        panic!("exact ingress must preserve typed terminal failure")
    };
    let DeliveryTerminalOutcomeCommitError::CompletionPending { commit, source } = error else {
        panic!("exact ingress must preserve original commit")
    };
    assert_eq!(
        commit.receipt().receipt_identity,
        original_receipt.receipt_identity
    );
    assert_eq!(
        commit.receipt().command_digest,
        original_receipt.command_digest
    );
    assert!(matches!(
        source.as_ref(),
        DurableExecutionPortError::Storage(_)
    ));
    assert_eq!(
        delivery_cancel_ack_counts(&connection),
        counts,
        "corrupt accepted audit cannot add terminal facts or emit successful ACK"
    );
    assert_eq!(
        storage
            .load_state(&original_state.stream_id)
            .unwrap()
            .unwrap(),
        recovered
    );
    connection
        .execute(
            "UPDATE audit_outbox SET payload=?1 WHERE event_id=?2",
            rusqlite::params![payload, event_id],
        )
        .unwrap();
    let result = DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        scope,
        Instant("2027-01-15T10:00:00.000Z".into()),
    )
    .unwrap()
    .handle(&frame)
    .unwrap();
    assert!(
        matches!(&result[0], ExecutionPortMessage::JobOutcomeAckMessage(ack) if ack.status==JobOutcomeAckMessageStatus::Duplicate)
    );
    let ready = storage
        .load_state(&original_state.stream_id)
        .unwrap()
        .unwrap();
    assert_eq!(ready.revision, 2);
    let retained: serde_json::Value = serde_json::from_slice(&ready.payload).unwrap();
    assert_eq!(retained["message"], serde_json::to_value(message).unwrap());
    assert_eq!(retained["acknowledgementReady"], true);
    drop(connection);
    Box::new(storage).close().unwrap();
    control_plane.shutdown().unwrap();
}
