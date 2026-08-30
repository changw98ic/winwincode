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

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, DeliveryAdvanceCommand, DeliveryAdvanceCommandCommand,
    DeliveryAdvancePayload, RepositoryScope, Scope, UserActor,
};
use winwincode_audit::{AuditEvent, AuditExecutionSubjectKind, AuditScope};
use winwincode_control_plane::{
    ArtifactEnterpriseQuotaAdmission, ArtifactEnterpriseQuotaSaga, CommitError, ControlPlane,
    ControlPlaneConfig, DeliveryTerminalOutcomeCommitError, DurableArtifactEnterpriseUsage,
    DurableEnterpriseQuotaAdmission, EventPublishError, EventPublisher, ExecutionPortService,
    LocalDeliveryAdapterConfig, OutboxEvent, StateChange,
};
use winwincode_delivery::{
    application::{
        stage::test_support::{
            active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
            terminal_outcome_metadata, terminal_worker_outcome,
        },
        verdict::{
            SubmitVerdictFacts,
            test_support::{VerdictFixtureOutcome, verdict_facts_fixture, verdict_fixture},
        },
    },
    domain::{Delivery, DeliveryStatus, DeliveryTaskStatus, StageRunStatus},
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, EnterprisePolicyId, ExecutionAckSequence,
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    SessionIdentity, Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId,
    WorkspaceId,
};
use winwincode_execution_port::generated::{
    ArtifactReference, DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, ExecutionJob,
    ExecutionLeaseStamp, ExecutionLimits, ExecutionOutcome, ExecutionOutcomeStatus,
    ExecutionOutcomeUsage, ExecutionPortError, ExecutionPortErrorCode, ExecutionScope,
    ExecutionWorkspace, ExecutionWorkspaceWriteMode, JobOutcomeMessage, JobOutcomeMessageKind,
};
use winwincode_storage::PublicEventSource;
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactChunk,
    ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance, ArtifactRetention,
    ArtifactStore, AuthenticatedWorkerPlacement, CandidateSourceManifest,
    EXECUTION_PROTOCOL_VERSION, EnterprisePolicyActor, EnterprisePolicyChildOverrideMode,
    EnterprisePolicyDefinition, EnterprisePolicyEffect, EnterprisePolicyInheritanceMode,
    EnterprisePolicyKind, EnterprisePolicyMode, EnterprisePolicyScope, EnterprisePolicyState,
    EnterprisePolicyVersionSource, EnterprisePolicyWrite, EnterpriseQuotaReleaseReason,
    EnterpriseQuotaReservationState, EnterpriseQuotaTerminal, ExecutionAdmissionBoundary,
    ExecutionAdmissionLimits, ExecutionAdmissionPolicy, ExecutionJobSubmission,
    ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, FakeArtifactObjectStore,
    LocalArtifactObjectStore, NewOutboxEvent, ProductStateStorage, PublicEventActor,
    ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit, WorkerAuthenticationIdentity,
    WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest,
    WorkerRegistryScope, WorkerSlotAuthority, WorkerSlotOpenRequest, WorkerSlotResourceLimits,
    WorkerSlotResources,
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
        kind: winwincode_api::generated::RepositoryScopeKind::Repository,
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
    let run = snapshot
        .stage_runs
        .iter_mut()
        .find(|run| run.role == "verifier")
        .expect("final verifier run");
    run.id = StageRunId(canonical_id("run", seed));
    run.status = StageRunStatus::Running;
    run.finished_at_millis = None;
    let binding = snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.id.0 == "binding-verifier-1")
        .expect("final verifier binding");
    binding.stage_run_id = run.id.clone();
    binding.product_session_id = ProductSessionId(canonical_id("psn", seed));
    binding.execution_job_id = ExecutionJobId(canonical_id("job", seed));
    binding.worker_session_id = Some(WorkerSessionId(canonical_id("wsn", seed)));
    binding.codex_thread_id = Some(CodexThreadId(canonical_id("cdx", seed)));
    let delivery = Delivery::try_from_snapshot(snapshot).expect("running final verifier Delivery");
    (delivery, fixture.candidate)
}

fn running_non_final_executor(seed: u64) -> Delivery {
    let (delivery, _candidate) = running_final_verifier(seed);
    let mut snapshot = delivery.into_snapshot();
    snapshot.status = DeliveryStatus::Executing;
    for task in &mut snapshot.tasks {
        task.status = DeliveryTaskStatus::Active;
    }
    let run = snapshot
        .stage_runs
        .iter_mut()
        .find(|run| run.id == StageRunId(canonical_id("run", seed)))
        .expect("active run");
    run.stage = winwincode_delivery::domain::DeliveryStage::Executing;
    run.role = "executor".into();
    Delivery::try_from_snapshot(snapshot).expect("running non-final executor")
}

fn execution_job(delivery: &Delivery, scope: &RepositoryScope) -> ExecutionJob {
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.status == StageRunStatus::Running)
        .expect("active run");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .expect("final verifier binding");
    ExecutionJob {
        attempt: 1,
        execution_profile: "strongflow-verifier".into(),
        goal: "Verify the frozen candidate".into(),
        job_id: binding.execution_job_id.clone(),
        limits: ExecutionLimits {
            deadline_at: Instant("2027-01-15T09:00:00.000Z".into()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id: delivery.id().clone(),
            delivery_task_id: run.delivery_task_id.clone(),
            kind: DeliveryStageExecutionScopeKind::DeliveryStage,
            product_session_id: binding.product_session_id.clone(),
            rework_authorization: None,
            stage_run_id: run.id.clone(),
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "candidate-checkout".into(),
            repository_id: scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
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
    Box::new(storage).close().expect("seed close");
}

fn seed_verifier_deny_policy(root: &Path, scope: &RepositoryScope, seed: u64) {
    let definition = EnterprisePolicyDefinition {
        default_effect: EnterprisePolicyEffect::Deny,
        child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
        rules: Vec::new(),
    };
    let canonical = serde_json::to_value(&definition).expect("Policy value fixture");
    let definition_sha256 = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).expect("serialize Policy definition"))
    ));
    SqliteStorage::open(root)
        .expect("Policy storage")
        .enterprise_policy_ledger()
        .expect("Policy ledger")
        .write(&EnterprisePolicyWrite {
            policy_id: EnterprisePolicyId(canonical_id("pol", seed)),
            policy_kind: EnterprisePolicyKind::Verifier,
            scope: EnterprisePolicyScope::Organization {
                organization_id: scope.organization_id.clone(),
            },
            mode: EnterprisePolicyMode::Enforce,
            state: EnterprisePolicyState::Active,
            definition_sha256,
            definition,
            effective_at: Instant("2027-01-15T07:00:00.000Z".into()),
            inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
            base_version: None,
            expected_revision: 0,
            source: EnterprisePolicyVersionSource {
                actor: EnterprisePolicyActor::User {
                    id: UserId(canonical_id("usr", seed)),
                },
                request_id: RequestId(canonical_id("req", seed + 8_000)),
            },
            updated_at: Instant("2027-01-15T07:00:00.000Z".into()),
        })
        .expect("write Verifier deny Policy");
}

fn seed_authenticated_worker_execution(
    root: &Path,
    scope: &RepositoryScope,
    job: &ExecutionJob,
    message: &JobOutcomeMessage,
    seed: u64,
) {
    let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
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
    ExecutionPortService::new(&mut storage, claim.issued_at.clone())
        .claim_execution_job(job.clone(), claim)
        .expect("production enterprise dispatch");
    seed_worker_slot(&mut storage, message, seed);
}

fn worker_admission_boundaries(
    scope: &RepositoryScope,
    job_scope: &DeliveryStageExecutionScope,
    pool_id: &WorkerPoolId,
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
            delivery_id: job_scope.delivery_id.clone(),
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
    job_scope: &DeliveryStageExecutionScope,
    pool_id: &WorkerPoolId,
    seed: u64,
) {
    let queue_scope = ExecutionQueueScope {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
        delivery_id: Some(job_scope.delivery_id.clone()),
        product_session_id: job_scope.product_session_id.clone(),
    };
    let submitted_at = Instant("2027-01-15T08:00:00.000Z".into());
    storage
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
            stage_run_id: Some(job_scope.stage_run_id.clone()),
            submitted_at: submitted_at.clone(),
        })
        .expect("execution queue submission");
    let mut admission = storage.execution_admission().expect("execution admission");
    for boundary in worker_admission_boundaries(scope, job_scope, pool_id) {
        admission
            .configure_policy(&ExecutionAdmissionPolicy {
                boundary,
                limits: ExecutionAdmissionLimits {
                    max_concurrent: 4,
                    max_queued: 4,
                    token_budget: 10_000,
                    cost_budget_microunits: 100_000,
                    max_runtime_millis: 3_600_000,
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
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 120_000,
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
    let authentication_identity = WorkerAuthenticationIdentity::TransportPrincipal {
        issuer: "terminal-fixture-worker-identity".to_owned(),
        subject: format!("remote-worker-{seed}"),
        credential_fingerprint: Sha256Digest(format!("sha256:{}", "e".repeat(64))),
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
                    security_zone: "enterprise-default".to_owned(),
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

fn worker_quota_terminal_state(root: &Path, job_id: &ExecutionJobId) -> (String, String, i64) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Worker quota terminal inspection");
    let operational = connection
        .query_row(
            "SELECT state FROM execution_admission_reservations WHERE job_id = ?1",
            [&job_id.0],
            |row| row.get(0),
        )
        .expect("operational terminal state");
    let enterprise = connection
        .query_row(
            "SELECT state FROM enterprise_quota_reservations",
            [],
            |row| row.get(0),
        )
        .expect("enterprise terminal state");
    let usage_sources = connection
        .query_row(
            "SELECT COUNT(*) FROM execution_admission_settlement_sources WHERE job_id = ?1",
            [&job_id.0],
            |row| row.get(0),
        )
        .expect("Worker Usage source count");
    connection.close().expect("terminal inspection close");
    (operational, enterprise, usage_sources)
}

fn seed_unfinished_artifact_quota(
    root: &Path,
    scope: &RepositoryScope,
    job: &ExecutionJob,
    message: &JobOutcomeMessage,
    seed: u64,
) -> RequestId {
    let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
        panic!("fixture Job must have Delivery scope");
    };
    let request_id = RequestId(canonical_id("req", seed + 8_000));
    let open = ArtifactOpen::new(
        ReceiptScopeKey::from_encoded(b"repository:terminal-artifact-quota".to_vec())
            .expect("Artifact scope"),
        ExecutionMessageId(canonical_id("xmsg", seed + 8_000)),
        request_id.clone(),
        ArtifactId(canonical_id("art", seed + 8_000)),
        "report",
        "application/octet-stream",
        Sha256Digest(format!("sha256:{}", "d".repeat(64))),
        5,
        None,
        ArtifactProvenance::execution_job(
            job.job_id.clone(),
            1,
            message.lease.lease_id.clone(),
            message.lease.fencing_token.clone(),
            message.lease.worker_id.clone(),
            message.lease.worker_instance_id.clone(),
            message.worker_session_id.clone(),
        )
        .expect("Artifact provenance"),
        ArtifactMeteringAttribution {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
            delivery_id: Some(job_scope.delivery_id.clone()),
            product_session_id: Some(job_scope.product_session_id.clone()),
            user_id: UserId(canonical_id("usr", seed)),
        },
        ArtifactRetention::Indefinite,
        1_800_000_000_200,
    );
    let mut artifacts = ArtifactStore::open(
        root.join("artifact-catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("Artifact catalog");
    let mut quota = DurableEnterpriseQuotaAdmission::new(
        SqliteStorage::open(root).expect("Artifact quota storage"),
    );
    let mut usage = DurableArtifactEnterpriseUsage::new(
        SqliteStorage::open(root).expect("Artifact Usage storage"),
    );
    assert!(matches!(
        ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage)
            .reserve_open(&open, &message.lease.issued_at)
            .expect("Artifact quota reservation"),
        ArtifactEnterpriseQuotaAdmission::Admitted(_)
    ));
    artifacts
        .open_artifact(open)
        .expect("unfinished Artifact catalog open");
    artifacts.close().expect("Artifact catalog close");
    quota.close().expect("Artifact quota close");
    usage.close().expect("Artifact Usage close");
    request_id
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

fn seed_candidate_artifact(
    root: &Path,
    scope: &RepositoryScope,
    delivery: &Delivery,
    message: &mut JobOutcomeMessage,
    candidate_commit: &str,
    seed: u64,
) {
    let bytes = CandidateSourceManifest::new(candidate_commit.to_owned())
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
                     WHEN NEW.topic = 'delivery.stage.terminal' \
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

fn install_handoff_consumption_failure(root: &Path) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("handoff failure injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_terminal_handoff_consumption \
             BEFORE UPDATE ON product_state \
             WHEN OLD.stream_id LIKE 'delivery-terminal-authority:%' \
              AND OLD.revision = 1 AND NEW.revision = 2 \
             BEGIN SELECT RAISE(ABORT, 'injected terminal handoff failure'); END;",
        )
        .expect("install handoff failure trigger");
    connection.close().expect("handoff injector close");
}

fn remove_handoff_consumption_failure(root: &Path) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("handoff failure removal");
    connection
        .execute_batch("DROP TRIGGER fail_terminal_handoff_consumption;")
        .expect("remove handoff failure trigger");
    connection.close().expect("handoff failure removal close");
}

fn queued_delivery_job_count(root: &Path) -> i64 {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Delivery queue inspection");
    let count = connection
        .query_row("SELECT COUNT(*) FROM scheduler_execution_jobs", [], |row| {
            row.get(0)
        })
        .expect("count queued Delivery jobs");
    connection.close().expect("Delivery queue inspection close");
    count
}

fn queued_delivery_job(root: &Path) -> ExecutionJob {
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
        .stage_runs
        .iter()
        .find(|run| run.status == StageRunStatus::Running)
        .expect("active run");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .expect("final verifier binding");
    JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
            fencing_token: FencingToken(seed.to_string()),
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
                cost_microunits: 400,
                runtime_millis: 60_000,
                tokens: 40,
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
            stage_run_id: Some(run.id.clone()),
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
) -> winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts {
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.role == "verifier")
        .expect("final verifier run");
    outcome_facts_for_stage(delivery, message, run.id.clone())
}

fn outcome_facts_for_stage(
    _delivery: &Delivery,
    message: &JobOutcomeMessage,
    stage_run_id: StageRunId,
) -> winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts {
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
            .map(
                |artifact| winwincode_delivery::application::stage::TerminalArtifactReference {
                    artifact_id: artifact.artifact_id.clone(),
                    digest: artifact.digest.clone(),
                },
            )
            .collect(),
    );
    let outcome = terminal_worker_outcome(
        stage_run_id,
        message.lease.job_id.clone(),
        1,
        message.lease.lease_id.clone(),
        message.lease.fencing_token.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.worker_session_id.clone(),
        match message.outcome.status {
            ExecutionOutcomeStatus::Succeeded => {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::Succeeded
            }
            ExecutionOutcomeStatus::Failed => {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::Failed
            }
            ExecutionOutcomeStatus::InfrastructureError => {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::InfrastructureError
            }
            ExecutionOutcomeStatus::Cancelled => {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::Cancelled
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

fn advance_command(
    scope: RepositoryScope,
    delivery: &Delivery,
    seed: u64,
) -> DeliveryAdvanceCommand {
    DeliveryAdvanceCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: DeliveryAdvanceCommandCommand::DeliveryAdvance,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("revision")),
        payload: DeliveryAdvancePayload {
            delivery_id: delivery.id().clone(),
        },
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope,
    }
}

fn expired_executor_delivery(seed: u64, repository: &Path, base_revision: &str) -> Delivery {
    let mut snapshot = running_non_final_executor(seed).into_snapshot();
    repository
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .expect("portable repository fixture locator")
        .clone_into(&mut snapshot.spec.repository.locator);
    base_revision.clone_into(&mut snapshot.spec.base_revision);
    snapshot.updated_at_millis = 1_800_000_360_000;
    let active_run_id = snapshot
        .stage_runs
        .iter()
        .find(|run| run.status == StageRunStatus::Running)
        .expect("active executor")
        .id
        .clone();
    let binding = snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.stage_run_id == active_run_id)
        .expect("active executor binding");
    binding.worker_id = Some(WorkerId(canonical_id("wrk", seed)));
    binding.worker_instance_id = Some(WorkerInstanceId(canonical_id("wki", seed)));
    binding.lease_id = Some(LeaseId(canonical_id("lse", seed)));
    binding.fencing_token = Some(FencingToken(seed.to_string()));
    binding.attempt = 1;
    Delivery::try_from_snapshot(snapshot).expect("expired-lease Delivery")
}

fn assert_handoff_consumption_rolls_back(
    control_plane: &mut ControlPlane,
    root: &Path,
    delivery: &Delivery,
    job: &ExecutionJob,
    advance: &DeliveryAdvanceCommand,
) {
    install_handoff_consumption_failure(root);
    control_plane
        .delivery_advance(advance)
        .expect_err("secondary handoff failure rolls back the whole advance");
    assert_eq!(durable_terminal_counts(root, delivery.id()), (1, 1, 2, 3));
    assert_eq!(queued_delivery_job_count(root), 0);
    let still_active = control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("active Delivery read")
        .expect("active Delivery state");
    assert_eq!(still_active.revision, 1);
    let pending_authority = control_plane
        .load_state(&format!("delivery-terminal-authority:{}", job.job_id.0))
        .expect("pending authority read")
        .expect("pending authority state");
    assert_eq!(pending_authority.revision, 1);
    let pending_json: serde_json::Value =
        serde_json::from_slice(&pending_authority.payload).expect("pending authority JSON");
    assert_eq!(pending_json["disposition"]["kind"], "pending_handoff");
    remove_handoff_consumption_failure(root);
}

fn assert_consumed_authority(
    control_plane: &ControlPlane,
    root: &Path,
    job: &ExecutionJob,
    advance: &DeliveryAdvanceCommand,
) {
    let authority = control_plane
        .load_state(&format!("delivery-terminal-authority:{}", job.job_id.0))
        .expect("terminal authority read")
        .expect("terminal authority state");
    assert_eq!(authority.revision, 2);
    let authority_json: serde_json::Value =
        serde_json::from_slice(&authority.payload).expect("terminal authority JSON");
    assert_eq!(authority_json["disposition"]["kind"], "consumed");
    assert_eq!(
        authority_json["disposition"]["advance_request_id"],
        advance.request_id.0
    );
    assert_eq!(authority_json["disposition"]["delivery_revision"], 2);
    assert_eq!(queued_delivery_job_count(root), 1);
}

fn verdict_command(
    seed: u64,
    delivery: &Delivery,
    candidate: &winwincode_delivery::domain::FrozenDeliveryCandidate,
) -> CommandEnvelope {
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: CommandName::DeliverySubmitVerdict,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("revision")),
        payload: serde_json::json!({
            "deliveryId": delivery.id().0,
            "candidateDigest": candidate.candidate_ref()
                .strip_prefix("git-candidate:")
                .expect("candidate digest"),
        }),
        request_id: RequestId(canonical_id("req", seed + 1000)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(repository_scope(seed)),
    }
}

#[test]
fn verifier_policy_denies_before_terminal_mutation_and_replays_one_audit() {
    let seed = 69;
    let root = temporary_directory("verifier-policy-denial");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let original_revision = delivery.revision();
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    seed_verifier_deny_policy(&root, &scope, seed);

    for _ in 0..2 {
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
            .expect_err("Verifier Policy must deny before terminal commit");
        control_plane.shutdown().expect("Control Plane shutdown");
    }
    let mut storage = SqliteStorage::open(&root).expect("restart Policy storage");
    assert_eq!(
        storage
            .enterprise_policy_evaluation_ledger()
            .expect("Policy audit")
            .scan_audit(None, 10)
            .expect("scan Policy audit")
            .entries
            .len(),
        1,
        "exact replay must not duplicate Verifier Policy audit"
    );
    let stored = storage
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("load Delivery")
        .expect("Delivery exists");
    let current = Delivery::decode_json(&stored.payload).expect("decode Delivery");
    assert_eq!(current.revision(), original_revision);
    assert_eq!(terminal_receipt_count(&root, delivery.id()), 0);
    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn authenticated_worker_terminal_quota_settles_or_releases_once_across_restart() {
    for (seed, name, status, expected_state, expected_sources) in [
        (
            70,
            "succeeded",
            ExecutionOutcomeStatus::Succeeded,
            "settled",
            1,
        ),
        (71, "failed", ExecutionOutcomeStatus::Failed, "released", 0),
        (
            72,
            "cancelled",
            ExecutionOutcomeStatus::Cancelled,
            "released",
            0,
        ),
    ] {
        let root = temporary_directory(&format!("Worker-quota-{name}"));
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, status);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        seed_authenticated_worker_execution(&root, &scope, &job, &message, seed);

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
            worker_quota_terminal_state(&root, &job.job_id),
            (
                expected_state.to_owned(),
                expected_state.to_owned(),
                expected_sources
            )
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
            worker_quota_terminal_state(&root, &job.job_id),
            (
                expected_state.to_owned(),
                expected_state.to_owned(),
                expected_sources
            )
        );
        fs::remove_dir_all(root).expect("directory release");
    }
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
fn committed_worker_quota_pending_recovers_exactly_after_restart() {
    let seed = 74;
    let root = temporary_directory("Worker-quota-pending-restart");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    seed_authenticated_worker_execution(&root, &scope, &job, &message, seed);
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Worker quota failure injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_worker_quota_settlement
             BEFORE UPDATE ON enterprise_quota_reservations
             WHEN OLD.state = 'active' AND NEW.state = 'settled'
             BEGIN SELECT RAISE(ABORT, 'injected Worker quota settlement failure'); END;",
        )
        .expect("install Worker quota failure");
    connection.close().expect("failure injector close");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    let error = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect_err("committed Worker quota settlement must remain pending");
    assert!(matches!(
        error,
        DeliveryTerminalOutcomeCommitError::WorkerQuotaPending { .. }
    ));
    assert!(error.committed_receipt().is_some());
    control_plane.shutdown().expect("crashed process shutdown");
    assert_eq!(
        worker_quota_terminal_state(&root, &job.job_id),
        ("settled".to_owned(), "active".to_owned(), 1)
    );

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("Worker quota failure remover");
    connection
        .execute_batch("DROP TRIGGER fail_worker_quota_settlement;")
        .expect("remove Worker quota failure");
    connection.close().expect("failure remover close");
    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane restart");
    let replay = restarted
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("pending Worker quota recovery");
    assert!(replay.receipt().idempotent_replay);
    restarted.shutdown().expect("restart shutdown");
    assert_eq!(
        worker_quota_terminal_state(&root, &job.job_id),
        ("settled".to_owned(), "settled".to_owned(), 1)
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
    assert_eq!(commit.receipt().revision, 2);
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
        Some(identity.stage_run_id()),
        message.session_identity.stage_run_id.as_ref()
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
    let final_verifier = settled
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == StageRunId(canonical_id("run", seed)))
        .expect("final verifier");
    assert_eq!(final_verifier.status, StageRunStatus::Succeeded);
    assert_eq!(final_verifier.finished_at_millis, Some(1_800_000_060_000));

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
        .find(|event| event.topic == "delivery.stage.terminal")
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
fn a_new_message_cannot_resettle_an_already_terminal_stage_run() {
    let seed = 5;
    let root = temporary_directory("stale-new-message");
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
    let mut stale = message.clone();
    stale.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 100));

    control_plane
        .commit_delivery_terminal_outcome(&scope, &stale, &facts, &stale.sent_at)
        .expect_err("a new message cannot settle the same StageRun twice");
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (2, 2, 2, 4));
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn concurrent_exact_message_returns_one_commit_and_only_durable_replays() {
    let seed = 4;
    let root = temporary_directory("concurrent-exact-message");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);

    let root = Arc::new(root);
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            let scope = scope.clone();
            let message = message.clone();
            let facts = facts.clone();
            thread::spawn(move || {
                barrier.wait();
                let mut control_plane = ControlPlane::start_local(
                    ControlPlaneConfig::local(root.as_path()),
                    Box::new(RecordingPublisher),
                )
                .expect("Control Plane start");
                let receipt = control_plane
                    .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
                    .expect("concurrent terminal outcome");
                let replayed = receipt.receipt().idempotent_replay;
                control_plane.shutdown().expect("shutdown");
                replayed
            })
        })
        .collect::<Vec<_>>();
    let replayed = handles
        .into_iter()
        .map(|handle| handle.join().expect("terminal outcome thread"))
        .collect::<Vec<_>>();

    assert_eq!(replayed.iter().filter(|value| !**value).count(), 1);
    assert_eq!(replayed.iter().filter(|value| **value).count(), 7);
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
            "SELECT COUNT(*) FROM outbox WHERE topic IN ('delivery.stage.terminal', 'delivery.changed.v1', 'runtime-projection.invalidated.v1')",
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
    fs::remove_dir_all(root.as_path()).expect("database directory release");
}

#[test]
fn failed_infrastructure_and_cancelled_outcomes_settle_without_advancing_delivery() {
    for (offset, status, expected_run, expected_release) in [
        (
            0_u64,
            ExecutionOutcomeStatus::Failed,
            StageRunStatus::Failed,
            EnterpriseQuotaReleaseReason::Failed,
        ),
        (
            1,
            ExecutionOutcomeStatus::InfrastructureError,
            StageRunStatus::Failed,
            EnterpriseQuotaReleaseReason::Failed,
        ),
        (
            2,
            ExecutionOutcomeStatus::Cancelled,
            StageRunStatus::Cancelled,
            EnterpriseQuotaReleaseReason::Cancelled,
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
        let artifact_reservation =
            seed_unfinished_artifact_quota(&root, &scope, &job, &message, seed);
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
            .expect("terminal outcome and Artifact quota release replay");
        assert!(replay.receipt().idempotent_replay);
        let stored = control_plane
            .load_state(&format!("delivery:{}", delivery.id().0))
            .expect("state read")
            .expect("Delivery state");
        let settled = Delivery::decode_json(&stored.payload).expect("settled Delivery");
        let run = settled
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| run.id == StageRunId(canonical_id("run", seed)))
            .expect("final verifier run");
        assert_eq!(run.status, expected_run);
        assert_eq!(settled.snapshot().status, DeliveryStatus::Verifying);
        assert!(
            settled
                .snapshot()
                .tasks
                .iter()
                .all(|task| task.status == DeliveryTaskStatus::Verifying)
        );
        let mut quota_storage = SqliteStorage::open(&root).expect("quota inspection storage");
        let quota_record = quota_storage
            .enterprise_quota_ledger()
            .expect("enterprise quota ledger")
            .load_reservation(&artifact_reservation)
            .expect("Artifact reservation lookup")
            .expect("Artifact reservation");
        assert_eq!(
            quota_record.state,
            EnterpriseQuotaReservationState::Released
        );
        assert_eq!(quota_record.revision, 2);
        assert!(matches!(
            quota_record.terminal,
            Some(EnterpriseQuotaTerminal::Released { reason, .. }) if reason == expected_release
        ));
        Box::new(quota_storage)
            .close()
            .expect("quota inspection close");
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn terminal_commit_restarts_and_releases_artifact_quota_after_the_release_write_crashes() {
    let seed = 13;
    let root = temporary_directory("artifact-quota-release-restart");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Cancelled);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let artifact_reservation = seed_unfinished_artifact_quota(&root, &scope, &job, &message, seed);
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("quota release failure injector");
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER fail_artifact_quota_release
             BEFORE UPDATE ON enterprise_quota_reservations
             WHEN OLD.reservation_id = '{}'
             BEGIN SELECT RAISE(ABORT, 'injected Artifact quota release failure'); END;",
            artifact_reservation.0
        ))
        .expect("install quota release failure");
    connection.close().expect("failure injector close");
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let error = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect_err("terminal commit must surface pending Artifact quota release");
    assert!(matches!(
        &error,
        DeliveryTerminalOutcomeCommitError::ArtifactQuotaPending { .. }
    ));
    assert!(error.committed_receipt().is_some());
    control_plane.shutdown().expect("crashed process shutdown");
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("quota release failure remover");
    connection
        .execute_batch("DROP TRIGGER fail_artifact_quota_release;")
        .expect("remove quota release failure");
    connection.close().expect("failure remover close");

    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart Control Plane");
    let replay = restarted
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("receipt-first retry releases Artifact quota");
    assert!(replay.receipt().idempotent_replay);
    let mut quota_storage = SqliteStorage::open(&root).expect("quota inspection storage");
    let released = quota_storage
        .enterprise_quota_ledger()
        .expect("enterprise quota ledger")
        .load_reservation(&artifact_reservation)
        .expect("reservation lookup")
        .expect("Artifact reservation");
    assert_eq!(released.state, EnterpriseQuotaReservationState::Released);
    assert_eq!(released.revision, 2);
    Box::new(quota_storage)
        .close()
        .expect("quota inspection close");
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
        let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
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
            (1, 1, 1, 1),
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
    let mut failing =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(FailingPublisher))
            .expect("Control Plane start");

    let error = failing
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect_err("publication must fail after commit");
    let committed = error
        .committed_receipt()
        .expect("publication error carries committed terminal receipt");
    assert_eq!(committed.receipt().revision, 2);
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (2, 2, 2, 4));
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
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (2, 2, 2, 4));
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
            "SELECT COUNT(*) FROM outbox WHERE published = 0",
            [],
            |row| row.get(0),
        )
        .expect("pending event count");
    assert_eq!(pending, 0);
    connection.close().expect("published event count close");
    restarted.shutdown().expect("shutdown");
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
    let mut changed = message.clone();
    changed.sent_at = Instant("2027-01-15T08:06:00.000Z".into());
    cases.push(("sent-after-expiry", changed));

    for (name, changed) in cases {
        assert!(
            control_plane
                .commit_delivery_terminal_outcome(&scope, &changed, &facts, &changed.sent_at)
                .is_err(),
            "foreign {name} must fail closed"
        );
        assert_eq!(
            durable_terminal_counts(&root, delivery.id()),
            (1, 1, 1, 1),
            "{name}"
        );
        assert_eq!(audit_event_count(&root), 0, "{name}");
    }

    let foreign_stage_facts = outcome_facts_for_stage(
        &delivery,
        &message,
        StageRunId(canonical_id("run", seed + 1)),
    );
    control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &foreign_stage_facts, &message.sent_at)
        .expect_err("foreign stage authority must fail closed");
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (1, 1, 1, 1));
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
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");

        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
            .expect_err("schema-invalid outcome.error must be rejected");
        assert_eq!(durable_terminal_counts(&root, delivery.id()), (1, 1, 1, 1));
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
        "foreign-stage",
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
                "unknown-field" | "foreign-stage" => {
                    let mut value = serde_json::to_value(&job).expect("ExecutionJob value");
                    if corruption == "unknown-field" {
                        value
                            .as_object_mut()
                            .expect("ExecutionJob object")
                            .insert("unknownField".into(), serde_json::json!(true));
                    } else {
                        value["scope"]["stageRunId"] =
                            serde_json::json!(canonical_id("run", seed + 1));
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
fn successful_non_final_outcome_survives_restart_and_is_consumed_once_after_lease_expiry() {
    let seed = 60;
    let root = temporary_directory("successful-non-final");
    let repository = root.join("repository");
    let (base_commit, candidate_commit) = initialize_git_repository(&repository);
    let repository = fs::canonicalize(repository).expect("canonical repository");
    let scope = repository_scope(seed);
    let delivery = expired_executor_delivery(seed, &repository, &base_commit);
    let job = execution_job(&delivery, &scope);
    let mut message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    seed_candidate_artifact(
        &root,
        &scope,
        &delivery,
        &mut message,
        &candidate_commit,
        seed,
    );
    let facts = outcome_facts_for_stage(&delivery, &message, StageRunId(canonical_id("run", seed)));
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let pending = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("persist successful executor handoff");
    assert_eq!(pending.receipt().revision, 1);
    assert_eq!(pending.receipt().events.len(), 2);
    assert_eq!(
        pending.receipt().stream_id,
        format!("delivery-terminal-authority:{}", job.job_id.0)
    );
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (1, 1, 2, 3));
    control_plane.shutdown().expect("shutdown");

    let mut restarted = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .expect("restart with production Delivery adapters");
    let terminal_replay = restarted
        .commit_delivery_terminal_outcome(&scope, &message, &facts, &message.sent_at)
        .expect("terminal receipt replay after restart");
    assert!(terminal_replay.receipt().idempotent_replay);
    assert_eq!(terminal_replay.receipt().events, pending.receipt().events);

    let advance = advance_command(scope, &delivery, seed + 1_000);
    assert_handoff_consumption_rolls_back(&mut restarted, &root, &delivery, &job, &advance);

    let advanced = restarted
        .delivery_advance(&advance)
        .expect("consume the expired successful handoff");
    assert_eq!(advanced.current_revision, Revision(2));
    assert_consumed_authority(&restarted, &root, &job, &advance);
    let verification_job = queued_delivery_job(&root);
    assert_eq!(
        verification_job.workspace.checkout_revision,
        candidate_commit
    );
    assert!(
        verification_job
            .stage_input
            .as_ref()
            .and_then(|input| input.candidate_ref.as_deref())
            .is_some_and(|candidate| candidate.starts_with("git-candidate:sha256:"))
    );

    restarted.shutdown().expect("second shutdown");
    let mut replay_host = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, repository_scope(seed)),
    )
    .expect("restart consumed handoff host");
    assert_eq!(
        replay_host
            .delivery_advance(&advance)
            .expect("exact advance replay after restart"),
        advanced
    );
    let stale_advance = advance_command(repository_scope(seed), &delivery, seed + 1_001);
    replay_host
        .delivery_advance(&stale_advance)
        .expect_err("a new stale advance cannot consume the settled handoff");
    assert_eq!(
        replay_host
            .load_state(&format!("delivery-terminal-authority:{}", job.job_id.0))
            .expect("terminal authority replay read")
            .expect("terminal authority replay state")
            .revision,
        2
    );
    assert_eq!(queued_delivery_job_count(&root), 1);
    replay_host.shutdown().expect("replay host shutdown");
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
                    "delivery.stage.terminal",
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
                    "delivery.stage.terminal",
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
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (1, 1, 1, 1));
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
                         WHERE topic = 'delivery.stage.terminal'",
                        [format!("delivery:{}", delivery.id().0)],
                    )
                    .expect("move terminal event to seed receipt");
            }
            "terminal-payload" => {
                connection
                    .execute(
                        "UPDATE outbox SET payload = X'7b7d' WHERE topic = 'delivery.stage.terminal'",
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
            2,
            "{corruption}"
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}
