// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, from_value};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, OrganizationId, ProjectId, RepositoryId, RepositoryScope,
    RepositoryScopeKind, Scope, UserActor, WorkspaceId,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DurableExecutionPortContext, DurableExecutionPortDelegate,
    DurableExecutionPortError, DurableExecutionPortIngress, DurableExecutionPortSupplement,
    EventPublishError, EventPublisher, OutboxEvent, StateChange,
};
use winwincode_delivery::{
    application::verdict::test_support::{VerdictFixtureOutcome, verdict_fixture},
    domain::{Delivery, StageRunStatus},
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionEventId, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, ProductSessionId,
    RequestId, Revision, SchemaVersion, SessionIdentity, Sha256Digest, StageRunId, UserId,
    WorkerSessionId,
};
use winwincode_execution_port::generated::{
    DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, ExecutionEventCategory,
    ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp, ExecutionLimits, ExecutionOutcome,
    ExecutionOutcomeStatus, ExecutionPortMessage, ExecutionScope, ExecutionWorkspace,
    ExecutionWorkspaceWriteMode, JobDispatchResultMessage, JobDispatchResultMessageKind,
    JobDispatchResultMessageStatus, JobOutcomeAckMessageStatus, JobOutcomeMessage,
    JobOutcomeMessageKind, LeaseWriteStatus, ProductSessionExecutionScope,
    ProductSessionExecutionScopeKind, RuntimeEventMessage, RuntimeEventMessageKind,
    WorkerRegisterMessage, WorkerRegistrationResultMessageStatus,
};
use winwincode_execution_port::transport::{
    EndpointSide, FrameDirection, LocalWorkerAdapter, RemoteTransportAdapter, TypedFrame,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionJobState, ExecutionJobSubmission, ExecutionJobTransitionRequest, ExecutionLeaseClaim,
    ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRequest,
    ExecutionReservationStart, NewOutboxEvent, ProductStateStorage, PublicEventScope,
    SqliteStorage, StateCommit, WorkerHeartbeatRequest, WorkerPoolId, WorkerSlotAuthority,
    WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources, receipt_scope_key,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-durable-execution-port-{name}-{}-{suffix}",
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

fn product_session_catalog_stream_id(scope: &RepositoryScope) -> String {
    let scope_key = receipt_scope_key(&PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    })
    .expect("repository receipt scope");
    format!(
        "product-sessions:{:x}",
        Sha256::digest(scope_key.as_bytes())
    )
}

fn seed_queue_lease(
    storage: &mut SqliteStorage,
    repository: &RepositoryScope,
    job: &ExecutionJob,
    claim: &ExecutionLeaseClaim,
    seed: u64,
) {
    let (product_session_id, delivery_id, stage_run_id) = match &job.scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            (scope.product_session_id.clone(), None, None)
        }
        ExecutionScope::DeliveryStageExecutionScope(scope) => (
            scope.product_session_id.clone(),
            Some(scope.delivery_id.clone()),
            Some(scope.stage_run_id.clone()),
        ),
    };
    let queue_scope = ExecutionQueueScope {
        organization_id: repository.organization_id.clone(),
        workspace_id: repository.workspace_id.clone(),
        project_id: repository.project_id.clone(),
        repository_id: repository.repository_id.clone(),
        product_session_id,
        delivery_id,
    };
    let submitted = storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 10_000)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(job).expect("job JSON"),
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id,
            submitted_at: Instant("2027-01-15T08:00:00.100Z".to_owned()),
        })
        .expect("queue submit");
    storage
        .execution_queue()
        .expect("queue")
        .transition(&ExecutionJobTransitionRequest {
            scope: queue_scope,
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 10_001)),
            expected_revision: submitted.job.revision,
            from: ExecutionJobState::Queued,
            to: ExecutionJobState::Leased,
            occurred_at: claim.issued_at.clone(),
        })
        .expect("queue lease");
}

fn worker_register() -> WorkerRegisterMessage {
    let ExecutionPortMessage::WorkerRegisterMessage(message) = execution_message("worker.register")
    else {
        panic!("worker.register variant");
    };
    message
}

fn execution_message(kind: &str) -> ExecutionPortMessage {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("ExecutionPort fixture");
    fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .cloned()
        .map_or_else(|| panic!("{kind} fixture"), from_value)
        .unwrap_or_else(|error| panic!("{kind} decode: {error}"))
}

struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
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
        *self.publication.lock().expect("journal publication lock") = Some(publication);
        Ok(())
    }
}

struct Fixture {
    root: PathBuf,
    control_plane: ControlPlane,
    storage: SqliteStorage,
    scope: RepositoryScope,
}

impl Fixture {
    fn open(name: &str, seed: u64) -> Self {
        let root = temporary_directory(name);
        let control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        let storage = SqliteStorage::open(&root).expect("Registry storage");
        Self {
            root,
            control_plane,
            storage,
            scope: repository_scope(seed),
        }
    }

    fn close(self) {
        Box::new(self.storage).close().expect("Registry close");
        self.control_plane.shutdown().expect("Control Plane close");
        fs::remove_dir_all(self.root).expect("fixture release");
    }

    fn restart(self) -> Self {
        let Self {
            root,
            control_plane,
            storage,
            scope,
        } = self;
        Box::new(storage).close().expect("Registry restart close");
        control_plane
            .shutdown()
            .expect("Control Plane restart close");
        let control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane restart");
        let storage = SqliteStorage::open(&root).expect("Registry restart");
        Self {
            root,
            control_plane,
            storage,
            scope,
        }
    }

    fn accept(
        &mut self,
        message: &ExecutionPortMessage,
        now: Instant,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        DurableExecutionPortIngress::new(
            &mut self.control_plane,
            &mut self.storage,
            &self.scope,
            now,
        )?
        .handle(message)
    }

    fn accept_with_delegate(
        &mut self,
        message: &ExecutionPortMessage,
        now: Instant,
        delegate: &mut dyn DurableExecutionPortDelegate,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        DurableExecutionPortIngress::with_delegate(
            &mut self.control_plane,
            &mut self.storage,
            &self.scope,
            now,
            delegate,
        )?
        .handle(message)
    }
}

struct ProductDispatch {
    job: ExecutionJob,
    lease: ExecutionLeaseStamp,
    product_session_id: ProductSessionId,
    worker_session_id: WorkerSessionId,
}

#[allow(
    clippy::too_many_lines,
    reason = "the production ingress fixture keeps the durable Job, Registry claim, and accepted dispatch visibly exact"
)]
fn install_product_dispatch(fixture: &mut Fixture, seed: u64) -> ProductDispatch {
    let register = worker_register();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .expect("Worker registration");
    let product_session_id = ProductSessionId(canonical_id("psn", seed));
    let job = ExecutionJob {
        attempt: 1,
        execution_profile: "codex".to_owned(),
        goal: "Advance ProductSession chat".to_owned(),
        job_id: ExecutionJobId(canonical_id("job", seed)),
        limits: ExecutionLimits {
            deadline_at: Instant("2027-01-15T09:00:00.000Z".to_owned()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
            kind: ProductSessionExecutionScopeKind::ProductSession,
            product_session_id: product_session_id.clone(),
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "fixture-checkout".to_owned(),
            repository_id: fixture.scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    };
    fixture
        .control_plane
        .commit(
            &CommandEnvelope {
                actor: Actor::UserActor(UserActor {
                    id: UserId(canonical_id("usr", seed)),
                    kind: winwincode_api::generated::UserActorKind::User,
                }),
                command: CommandName::SessionCancel,
                expected_revision: Revision(0),
                payload: serde_json::json!({"productSessionId": product_session_id}),
                request_id: RequestId(canonical_id("req", seed)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::RepositoryScope(fixture.scope.clone()),
            },
            StateChange::new(
                product_session_catalog_stream_id(&fixture.scope),
                b"product-session-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    format!("execution-job:{}", job.job_id.0),
                    "execution.job.dispatch",
                    serde_json::to_vec(&job).expect("job JSON"),
                )],
            ),
        )
        .expect("durable ProductSession job");
    let claim = ExecutionLeaseClaim {
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.200Z".to_owned()),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        worker_id: register.worker_id,
        worker_instance_id: register.worker_instance_id,
        attempt: 1,
    };
    seed_queue_lease(&mut fixture.storage, &fixture.scope, &job, &claim, seed);
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&claim)
        .expect("lease claim");
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: claim.expires_at,
        fencing_token: claim.fencing_token,
        issued_at: claim.issued_at,
        job_id: claim.job_id,
        lease_id: claim.lease_id,
        worker_id: claim.worker_id,
        worker_instance_id: claim.worker_instance_id,
    };
    let result = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: job.payload_digest.clone(),
        request_id: claim.request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.000Z".to_owned()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(worker_session_id.clone()),
    };
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(result),
            Instant("2027-01-15T08:00:01.100Z".to_owned()),
        )
        .expect("accepted dispatch");
    ProductDispatch {
        job,
        lease,
        product_session_id,
        worker_session_id,
    }
}

fn running_delivery(seed: u64, register: &WorkerRegisterMessage) -> Delivery {
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
    binding.worker_id = Some(register.worker_id.clone());
    binding.worker_instance_id = Some(register.worker_instance_id.clone());
    binding.worker_session_id = Some(WorkerSessionId(canonical_id("wsn", seed)));
    binding.codex_thread_id = Some(CodexThreadId(canonical_id("cdx", seed)));
    binding.lease_id = Some(LeaseId(canonical_id("lse", seed)));
    binding.fencing_token = Some(FencingToken(seed.to_string()));
    binding.attempt = 1;
    Delivery::try_from_snapshot(snapshot).expect("running verifier Delivery")
}

fn delivery_job(delivery: &Delivery, scope: &RepositoryScope) -> ExecutionJob {
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
        .expect("active binding");
    ExecutionJob {
        attempt: 1,
        execution_profile: "strongflow-verifier".to_owned(),
        goal: "Verify the frozen candidate".to_owned(),
        job_id: binding.execution_job_id.clone(),
        limits: ExecutionLimits {
            deadline_at: Instant("2027-01-15T09:00:00.000Z".to_owned()),
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
            checkout_revision: "candidate-checkout".to_owned(),
            repository_id: scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    }
}

fn seed_delivery_job(
    root: &PathBuf,
    scope: &RepositoryScope,
    delivery: &Delivery,
    job: &ExecutionJob,
) {
    let journal = CapturingJournal::default();
    DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId(canonical_id("req", 8_000)),
            request_digest: "b".repeat(64),
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
        .expect("journal lock")
        .expect("journal publication")
    else {
        panic!("Delivery seed must create journal");
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
    let actor = Actor::UserActor(UserActor {
        id: UserId(canonical_id("usr", 8_000)),
        kind: winwincode_api::generated::UserActorKind::User,
    });
    let request_id = RequestId(canonical_id("req", 8_001));
    let identity = winwincode_control_plane::command_receipt_identity(
        &actor,
        &Scope::RepositoryScope(scope.clone()),
        request_id,
    )
    .expect("seed receipt identity");
    let mut storage = SqliteStorage::open(root).expect("seed storage");
    storage
        .commit(
            &StateCommit::new(
                identity,
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
            .with_journal_publication(publication),
        )
        .expect("seed Delivery and Job");
    Box::new(storage).close().expect("seed storage close");
}

fn install_delivery_dispatch(
    fixture: &mut Fixture,
    register: &WorkerRegisterMessage,
    job: &ExecutionJob,
    seed: u64,
) -> ExecutionLeaseStamp {
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .expect("Worker registration");
    let claim = ExecutionLeaseClaim {
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken(seed.to_string()),
        issued_at: Instant("2027-01-15T08:00:00.200Z".to_owned()),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        worker_id: register.worker_id.clone(),
        worker_instance_id: register.worker_instance_id.clone(),
        attempt: 1,
    };
    seed_queue_lease(&mut fixture.storage, &fixture.scope, job, &claim, seed);
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&claim)
        .expect("lease claim");
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: claim.expires_at,
        fencing_token: claim.fencing_token,
        issued_at: claim.issued_at,
        job_id: claim.job_id,
        lease_id: claim.lease_id,
        worker_id: claim.worker_id,
        worker_instance_id: claim.worker_instance_id,
    };
    let result = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: job.payload_digest.clone(),
        request_id: claim.request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.000Z".to_owned()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(WorkerSessionId(canonical_id("wsn", seed))),
    };
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(result),
            Instant("2027-01-15T08:00:01.100Z".to_owned()),
        )
        .expect("accepted dispatch");
    lease
}

fn failed_delivery_outcome(
    delivery: &Delivery,
    lease: ExecutionLeaseStamp,
    seed: u64,
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
        .expect("active binding");
    let worker_session_id = binding.worker_session_id.clone().expect("WorkerSession");
    let codex_thread_id = binding.codex_thread_id.clone().expect("CodexThread");
    JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease,
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 3)),
        outcome: ExecutionOutcome {
            artifacts: Vec::new(),
            codex_thread_id: Some(codex_thread_id.clone()),
            error: None,
            finished_at: Instant("2027-01-15T08:01:00.000Z".to_owned()),
            last_event_sequence: ExecutionAckSequence(12),
            status: ExecutionOutcomeStatus::Failed,
            summary: "Verifier failed".to_owned(),
            usage: None,
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:01:00.100Z".to_owned()),
        session_identity: SessionIdentity {
            codex_thread_id,
            product_session_id: binding.product_session_id.clone(),
            stage_run_id: Some(run.id.clone()),
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    }
}

fn seed_delivery_terminal_resources(
    storage: &mut SqliteStorage,
    repository: &RepositoryScope,
    job: &ExecutionJob,
    message: &JobOutcomeMessage,
    seed: u64,
) {
    let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
        panic!("Delivery terminal fixture Job scope")
    };
    seed_delivery_terminal_admission(storage, repository, job, job_scope, seed);
    seed_delivery_terminal_worker_slot(storage, message, seed);
}

fn seed_delivery_terminal_admission(
    storage: &mut SqliteStorage,
    repository: &RepositoryScope,
    job: &ExecutionJob,
    job_scope: &DeliveryStageExecutionScope,
    seed: u64,
) {
    let queue_scope = ExecutionQueueScope {
        organization_id: repository.organization_id.clone(),
        workspace_id: repository.workspace_id.clone(),
        project_id: repository.project_id.clone(),
        repository_id: repository.repository_id.clone(),
        product_session_id: job_scope.product_session_id.clone(),
        delivery_id: Some(job_scope.delivery_id.clone()),
    };
    let worker_pool_id = WorkerPoolId(canonical_id("wpl", seed));
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 4,
        max_queued: 4,
        token_budget: 10_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 3_600_000,
    };
    let boundaries = delivery_admission_boundaries(repository, job_scope, &worker_pool_id);
    let mut admission = storage.execution_admission().expect("execution admission");
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("execution admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: queue_scope.clone(),
            user_id: UserId(canonical_id("usr", seed)),
            worker_pool_id: worker_pool_id.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 20_000)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 120_000,
            submitted_at: Instant("2027-01-15T08:00:00.100Z".to_owned()),
        })
        .expect("execution reservation");
    admission
        .start(&ExecutionReservationStart {
            scope: queue_scope,
            worker_pool_id,
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 20_001)),
            expected_revision: 1,
            started_at: Instant("2027-01-15T08:00:00.200Z".to_owned()),
        })
        .expect("execution reservation start");
}

fn delivery_admission_boundaries(
    repository: &RepositoryScope,
    job_scope: &DeliveryStageExecutionScope,
    worker_pool_id: &WorkerPoolId,
) -> [ExecutionAdmissionBoundary; 6] {
    [
        ExecutionAdmissionBoundary::Organization {
            organization_id: repository.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: repository.organization_id.clone(),
            project_id: repository.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: repository.organization_id.clone(),
            project_id: repository.project_id.clone(),
            repository_id: repository.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::Delivery {
            organization_id: repository.organization_id.clone(),
            delivery_id: job_scope.delivery_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: repository.organization_id.clone(),
            project_id: repository.project_id.clone(),
            product_session_id: job_scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: repository.organization_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        },
    ]
}

fn seed_delivery_terminal_worker_slot(
    storage: &mut SqliteStorage,
    message: &JobOutcomeMessage,
    seed: u64,
) {
    storage
        .execution_registry()
        .expect("execution Registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 4,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 4,
            running_slots: 0,
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 20_003)),
            observed_at: Instant("2027-01-15T08:00:00.250Z".to_owned()),
            sent_at: Instant("2027-01-15T08:00:00.250Z".to_owned()),
            worker_id: message.lease.worker_id.clone(),
            worker_instance_id: message.lease.worker_instance_id.clone(),
        })
        .expect("Worker heartbeat");

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
            request_id: RequestId(canonical_id("req", seed + 20_002)),
            opened_at: Instant("2027-01-15T08:00:00.300Z".to_owned()),
        })
        .expect("Worker slot open");
}

fn registration_result(output: &[ExecutionPortMessage]) -> WorkerRegistrationResultMessageStatus {
    let [ExecutionPortMessage::WorkerRegistrationResultMessage(result)] = output else {
        panic!("ingress must return one worker.registration_result");
    };
    result.status.clone()
}

#[derive(Default)]
struct RecordingDelegate {
    seen: Vec<&'static str>,
}

impl DurableExecutionPortDelegate for RecordingDelegate {
    fn accept(
        &mut self,
        mut context: DurableExecutionPortContext<'_>,
        supplement: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        let control_plane_path = context
            .control_plane()
            .local_database_path()
            .expect("local Control Plane path")
            .to_path_buf();
        let storage_path = context.storage().database_path().to_path_buf();
        assert_eq!(control_plane_path, storage_path);
        assert_eq!(
            context.repository_scope().kind,
            RepositoryScopeKind::Repository
        );
        assert_eq!(context.server_time().0, "2027-01-15T08:00:02.100Z");
        match supplement {
            DurableExecutionPortSupplement::ProductSessionBinding {
                job,
                dispatch,
                message,
            } => {
                assert_eq!(job.job_id, message.lease.job_id);
                assert_eq!(dispatch.lease().job_id, job.job_id);
                self.seen.push("product-binding");
            }
            DurableExecutionPortSupplement::ProductSessionOutcome {
                job,
                dispatch,
                message,
            } => {
                assert_eq!(job.job_id, message.lease.job_id);
                assert_eq!(dispatch.lease().job_id, job.job_id);
                self.seen.push("product-outcome");
            }
            DurableExecutionPortSupplement::JobScopedWorkerMessage { dispatch, message } => {
                assert_eq!(dispatch.lease().job_id, delegated_job_id(message));
                self.seen.push(delegated_kind(message));
            }
            DurableExecutionPortSupplement::WorkerMessage(_) => self.seen.push("worker"),
        }
        Ok(Vec::new())
    }
}

fn delegated_job_id(message: &ExecutionPortMessage) -> ExecutionJobId {
    match message {
        ExecutionPortMessage::ModelOpenMessage(message) => message.lease.job_id.clone(),
        ExecutionPortMessage::JobCancelAckMessage(message) => message.lease.job_id.clone(),
        ExecutionPortMessage::ActionEnforcementRequestMessage(message) => {
            message.lease.job_id.clone()
        }
        _ => panic!("unexpected delegated message"),
    }
}

fn delegated_kind(message: &ExecutionPortMessage) -> &'static str {
    match message {
        ExecutionPortMessage::ModelOpenMessage(_) => "model.open",
        ExecutionPortMessage::JobCancelAckMessage(_) => "job.cancel_ack",
        ExecutionPortMessage::ActionEnforcementRequestMessage(_) => "action.enforcement_request",
        _ => panic!("unexpected delegated message"),
    }
}

fn exact_session_identity(dispatch: &ProductDispatch) -> SessionIdentity {
    SessionIdentity {
        codex_thread_id: CodexThreadId(canonical_id("cdx", 88)),
        product_session_id: dispatch.product_session_id.clone(),
        stage_run_id: None,
        worker_session_id: dispatch.worker_session_id.clone(),
    }
}

fn align_delegated_message(
    message: &mut ExecutionPortMessage,
    dispatch: &ProductDispatch,
    sequence: u64,
) {
    let sent_at = Instant("2027-01-15T08:00:02.100Z".to_owned());
    let message_id = ExecutionMessageId(canonical_id("xmsg", 100 + sequence));
    let identity = exact_session_identity(dispatch);
    match message {
        ExecutionPortMessage::ModelOpenMessage(message) => {
            message.lease.clone_from(&dispatch.lease);
            message
                .worker_session_id
                .clone_from(&dispatch.worker_session_id);
            message.session_identity.clone_from(&identity);
            message.sent_at = sent_at;
            message.message_id = message_id;
        }
        ExecutionPortMessage::JobCancelAckMessage(message) => {
            message.lease.clone_from(&dispatch.lease);
            message
                .worker_session_id
                .clone_from(&dispatch.worker_session_id);
            message.session_identity.clone_from(&identity);
            message.sent_at = sent_at;
            message.message_id = message_id;
        }
        ExecutionPortMessage::ActionEnforcementRequestMessage(message) => {
            message.job_id.clone_from(&dispatch.job.job_id);
            message.lease.clone_from(&dispatch.lease);
            message
                .worker_session_id
                .clone_from(&dispatch.worker_session_id);
            message.session_identity.clone_from(&identity);
            message.sent_at = sent_at;
            message.message_id = message_id;
        }
        ExecutionPortMessage::JobOutcomeMessage(message) => {
            message.lease.clone_from(&dispatch.lease);
            message
                .worker_session_id
                .clone_from(&dispatch.worker_session_id);
            message.session_identity = identity;
            message.sent_at = sent_at;
            message.message_id = message_id;
        }
        _ => panic!("unexpected aligned message"),
    }
}

#[test]
fn product_terminal_model_cancel_and_action_share_one_sealed_delegate_core() {
    let mut fixture = Fixture::open("closed-delegate", 88);
    let dispatch = install_product_dispatch(&mut fixture, 88);
    let mut delegate = RecordingDelegate::default();
    for (sequence, kind) in [
        "job.outcome",
        "model.open",
        "job.cancel_ack",
        "action.enforcement_request",
    ]
    .into_iter()
    .enumerate()
    {
        let mut message = execution_message(kind);
        align_delegated_message(
            &mut message,
            &dispatch,
            u64::try_from(sequence).expect("sequence"),
        );
        assert!(
            fixture
                .accept_with_delegate(
                    &message,
                    Instant("2027-01-15T08:00:02.100Z".to_owned()),
                    &mut delegate,
                )
                .expect("sealed delegate")
                .is_empty()
        );
    }
    assert_eq!(
        delegate.seen,
        [
            "product-outcome",
            "model.open",
            "job.cancel_ack",
            "action.enforcement_request"
        ]
    );

    let mut stale = execution_message("model.open");
    align_delegated_message(&mut stale, &dispatch, 9);
    let ExecutionPortMessage::ModelOpenMessage(stale) = &mut stale else {
        panic!("model.open fixture");
    };
    stale.lease.fencing_token = FencingToken("6".to_owned());
    let error = fixture
        .accept_with_delegate(
            &ExecutionPortMessage::ModelOpenMessage(stale.clone()),
            Instant("2027-01-15T08:00:02.100Z".to_owned()),
            &mut delegate,
        )
        .expect_err("stale delegate message must fail before its owner");
    assert!(matches!(error, DurableExecutionPortError::Storage(_)));
    assert_eq!(delegate.seen.len(), 4);
    fixture.close();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the fault-injected restart test keeps receipt, Registry, and zero-write assertions together"
)]
fn committed_delivery_outcome_replay_finishes_registry_once_after_restart_gap() {
    let seed = 97;
    let register = worker_register();
    let mut fixture = Fixture::open("terminal-registry-gap", seed);
    let delivery = running_delivery(seed, &register);
    let job = delivery_job(&delivery, &fixture.scope);
    seed_delivery_job(&fixture.root, &fixture.scope, &delivery, &job);
    let lease = install_delivery_dispatch(&mut fixture, &register, &job, seed);
    let outcome = failed_delivery_outcome(&delivery, lease, seed);
    seed_delivery_terminal_resources(&mut fixture.storage, &fixture.scope, &job, &outcome, seed);

    let connection = rusqlite::Connection::open(fixture.root.join("control-plane.sqlite3"))
        .expect("Registry failure injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_registry_terminal_after_delivery_commit
             BEFORE INSERT ON execution_lease_terminals
             BEGIN SELECT RAISE(ABORT, 'injected Registry terminal failure'); END;",
        )
        .expect("install Registry failure");
    connection.close().expect("failure injector close");

    let first_error = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            outcome.sent_at.clone(),
        )
        .expect_err("Registry settlement fails after Delivery commit");
    assert!(matches!(first_error, DurableExecutionPortError::Storage(_)));
    let committed = fixture
        .control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("committed Delivery read")
        .expect("committed Delivery state");
    assert_eq!(committed.revision, 2);
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 0);

    let connection = rusqlite::Connection::open(fixture.root.join("control-plane.sqlite3"))
        .expect("Registry failure removal");
    connection
        .execute_batch("DROP TRIGGER fail_registry_terminal_after_delivery_commit;")
        .expect("remove Registry failure");
    connection.close().expect("failure removal close");

    fixture = fixture.restart();
    let expired_server_time = Instant("2027-01-15T08:06:00.000Z".to_owned());
    assert_eq!(
        outcome_ack_status(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                    expired_server_time.clone(),
                )
                .expect("exact terminal recovery replay"),
        ),
        JobOutcomeAckMessageStatus::Duplicate
    );
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 1);

    let mut changed = outcome.clone();
    changed.outcome.summary = "changed replay body".to_owned();
    assert_eq!(
        outcome_ack_status(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(changed),
                    expired_server_time.clone(),
                )
                .expect("changed terminal replay rejection"),
        ),
        JobOutcomeAckMessageStatus::RejectedConflict
    );
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 1);

    let mut forged = outcome.clone();
    forged.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 30));
    assert_eq!(
        outcome_ack_status(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(forged),
                    expired_server_time,
                )
                .expect("expired fresh terminal rejection"),
        ),
        JobOutcomeAckMessageStatus::RejectedConflict
    );
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 1);

    let mut premature = outcome;
    premature.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 31));
    assert_eq!(
        outcome_ack_status(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(premature),
                    Instant("2027-01-15T07:59:59.000Z".to_owned()),
                )
                .expect("premature fresh terminal rejection"),
        ),
        JobOutcomeAckMessageStatus::RejectedConflict
    );
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 1);
    fixture.close();
}

fn outcome_ack_status(output: Vec<ExecutionPortMessage>) -> JobOutcomeAckMessageStatus {
    let mut output = output.into_iter();
    let Some(ExecutionPortMessage::JobOutcomeAckMessage(acknowledgement)) = output.next() else {
        panic!("job.outcome_ack response");
    };
    assert!(output.next().is_none(), "one job.outcome_ack response");
    acknowledgement.status
}

fn lease_terminal_count(root: &Path, job_id: &ExecutionJobId) -> i64 {
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("Registry inspect");
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM execution_lease_terminals WHERE job_id = ?1",
            [&job_id.0],
            |row| row.get(0),
        )
        .expect("Registry terminal count");
    connection.close().expect("Registry inspect close");
    count
}

#[test]
fn local_and_remote_adapters_share_the_same_durable_ingress_core() {
    let message = worker_register();
    let frame = TypedFrame::new(
        FrameDirection::WorkerToControlPlane,
        ExecutionPortMessage::WorkerRegisterMessage(message.clone()),
    )
    .expect("Worker frame");
    let encoded = RemoteTransportAdapter::<DurableExecutionPortIngress<'_>>::encode(&frame)
        .expect("remote frame encoding");
    let mut local = Fixture::open("local", 1);
    let mut remote = Fixture::open("remote", 1);

    let local_output = {
        let mut ingress = DurableExecutionPortIngress::new(
            &mut local.control_plane,
            &mut local.storage,
            &local.scope,
            message.sent_at.clone(),
        )
        .expect("local ingress");
        LocalWorkerAdapter::new(&mut ingress, EndpointSide::ControlPlane)
            .accept(&frame)
            .expect("local ingress response")
    };
    let remote_output = {
        let mut ingress = DurableExecutionPortIngress::new(
            &mut remote.control_plane,
            &mut remote.storage,
            &remote.scope,
            message.sent_at.clone(),
        )
        .expect("remote ingress");
        RemoteTransportAdapter::new(&mut ingress, EndpointSide::ControlPlane)
            .accept(&encoded)
            .expect("remote ingress response")
    };

    assert_eq!(local_output, remote_output);
    assert_eq!(
        registration_result(&local_output),
        WorkerRegistrationResultMessageStatus::Accepted
    );
    assert!(
        local
            .storage
            .execution_registry()
            .expect("local registry")
            .load_worker(&message.worker_id)
            .expect("local Worker load")
            .is_some()
    );
    assert!(
        remote
            .storage
            .execution_registry()
            .expect("remote registry")
            .load_worker(&message.worker_id)
            .expect("remote Worker load")
            .is_some()
    );

    local.close();
    remote.close();
}

#[test]
fn ingress_rejects_a_second_database_before_processing_worker_input() {
    let mut canonical = Fixture::open("canonical", 2);
    let foreign_root = temporary_directory("foreign");
    let mut foreign = SqliteStorage::open(&foreign_root).expect("foreign storage");
    let Err(error) = DurableExecutionPortIngress::new(
        &mut canonical.control_plane,
        &mut foreign,
        &canonical.scope,
        worker_register().sent_at,
    ) else {
        panic!("foreign database must be rejected");
    };
    assert!(matches!(error, DurableExecutionPortError::Configuration));

    Box::new(foreign).close().expect("foreign storage close");
    fs::remove_dir_all(foreign_root).expect("foreign fixture release");
    canonical.close();
}

#[test]
fn production_execution_port_contract_has_no_worker_time_authority_fallback() {
    let control_plane = include_str!("../src/lib.rs");
    let service = include_str!("../src/execution_port_service.rs");
    let ingress = include_str!("../src/durable_execution_port.rs");

    assert!(!control_plane.contains("pub fn accept_runtime_event_at"));
    assert!(!control_plane.contains("pub fn commit_delivery_terminal_outcome_at"));
    assert!(!control_plane.contains("pub fn commit_delivery_session_binding_at"));
    assert!(
        !control_plane
            .contains("session_binding_transaction::execute(storage, message, authority)")
    );
    assert!(service.contains("server_time: Instant"));
    assert!(service.contains(
        ".accept_runtime_event(route.scope(), message, route.authority(), &self.server_time)"
    ));
    assert!(
        ingress
            .contains(".commit_delivery_session_binding(binding, &authority, &self.server_time)")
    );
    assert!(ingress.contains("validate_first_seen_dispatch"));
    assert!(ingress.contains("Exact receipt replay must"));
    assert!(ingress.contains("must not be copied into the owner request identity, digest"));
    assert!(ingress.contains("The trusted clock never enters the owner digest"));
    assert!(ingress.contains("Worker-controlled `sentAt` is an audited fact, never authorization"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn accepted_dispatch_seals_product_session_runtime_and_replays_exactly() {
    let seed = 44;
    let mut fixture = Fixture::open("product-runtime", seed);
    let register = worker_register();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .expect("Worker registration");

    let product_session_id = ProductSessionId(canonical_id("psn", seed));
    let job = ExecutionJob {
        attempt: 1,
        execution_profile: "codex".to_owned(),
        goal: "Advance ProductSession chat".to_owned(),
        job_id: ExecutionJobId(canonical_id("job", seed)),
        limits: ExecutionLimits {
            deadline_at: Instant("2027-01-15T09:00:00.000Z".to_owned()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
            kind: ProductSessionExecutionScopeKind::ProductSession,
            product_session_id: product_session_id.clone(),
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "fixture-checkout".to_owned(),
            repository_id: fixture.scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    };
    fixture
        .control_plane
        .commit(
            &CommandEnvelope {
                actor: Actor::UserActor(UserActor {
                    id: UserId(canonical_id("usr", seed)),
                    kind: winwincode_api::generated::UserActorKind::User,
                }),
                command: CommandName::SessionCancel,
                expected_revision: Revision(0),
                payload: serde_json::json!({"productSessionId": product_session_id}),
                request_id: RequestId(canonical_id("req", seed)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::RepositoryScope(fixture.scope.clone()),
            },
            StateChange::new(
                product_session_catalog_stream_id(&fixture.scope),
                b"product-session-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    format!("execution-job:{}", job.job_id.0),
                    "execution.job.dispatch",
                    serde_json::to_vec(&job).expect("job JSON"),
                )],
            ),
        )
        .expect("durable ProductSession job");

    let lease = ExecutionLeaseClaim {
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.200Z".to_owned()),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        worker_id: register.worker_id.clone(),
        worker_instance_id: register.worker_instance_id.clone(),
        attempt: 1,
    };
    seed_queue_lease(&mut fixture.storage, &fixture.scope, &job, &lease, seed);
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&lease)
        .expect("lease claim");
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
    let lease_stamp = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: lease.expires_at.clone(),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
    };
    let dispatch_result = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease_stamp.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: job.payload_digest.clone(),
        request_id: lease.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.000Z".to_owned()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(worker_session_id.clone()),
    };
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::JobDispatchResultMessage(dispatch_result),
                Instant("2027-01-15T08:00:01.100Z".to_owned()),
            )
            .expect("dispatch-result ingress")
            .is_empty()
    );

    let codex_thread_id = CodexThreadId(canonical_id("cdx", seed));
    let runtime = RuntimeEventMessage {
        codex_thread_id: codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Lifecycle,
            event_id: ExecutionEventId(canonical_id("xevt", seed)),
            occurred_at: Instant("2027-01-15T08:00:02.000Z".to_owned()),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "ProductSession Worker started".to_owned(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: lease_stamp,
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 3)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:02.100Z".to_owned()),
        session_identity: SessionIdentity {
            codex_thread_id: codex_thread_id.clone(),
            product_session_id,
            stage_run_id: None,
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    };
    let first = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(runtime.clone()),
            runtime.sent_at.clone(),
        )
        .expect("runtime ingress");
    let [ExecutionPortMessage::RuntimeAckMessage(first)] = first.as_slice() else {
        panic!("runtime.ack response");
    };
    assert_eq!(
        first.status,
        LeaseWriteStatus::Accepted,
        "{:?}",
        first.error
    );

    let replay = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(runtime.clone()),
            runtime.sent_at.clone(),
        )
        .expect("runtime replay");
    let [ExecutionPortMessage::RuntimeAckMessage(replay)] = replay.as_slice() else {
        panic!("runtime replay ack");
    };
    assert_eq!(replay.status, LeaseWriteStatus::Duplicate);

    let expired_time = Instant("2027-01-15T08:06:00.000Z".to_owned());
    let expired_replay = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(runtime.clone()),
            expired_time.clone(),
        )
        .expect("expired exact runtime replay");
    let [ExecutionPortMessage::RuntimeAckMessage(expired_replay)] = expired_replay.as_slice()
    else {
        panic!("expired runtime replay ack");
    };
    assert_eq!(expired_replay.status, LeaseWriteStatus::Duplicate);

    let mut changed = runtime.clone();
    changed.event.summary = "changed replay body".to_owned();
    let changed = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(changed),
            expired_time.clone(),
        )
        .expect("changed runtime replay rejection");
    let [ExecutionPortMessage::RuntimeAckMessage(changed)] = changed.as_slice() else {
        panic!("changed runtime replay ack");
    };
    assert_eq!(changed.status, LeaseWriteStatus::RejectedConflict);

    let mut forged = runtime.clone();
    forged.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 30));
    forged.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 30));
    forged.event.sequence = ExecutionSequence(2);
    let forged = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(forged),
            expired_time,
        )
        .expect("fresh expired runtime rejection");
    let [ExecutionPortMessage::RuntimeAckMessage(forged)] = forged.as_slice() else {
        panic!("fresh expired runtime ack");
    };
    assert_eq!(forged.status, LeaseWriteStatus::RejectedExpiredLease);

    let mut premature = runtime.clone();
    premature.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 31));
    premature.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 31));
    premature.event.sequence = ExecutionSequence(2);
    let premature = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(premature),
            Instant("2027-01-15T07:59:59.000Z".to_owned()),
        )
        .expect("premature runtime rejection");
    let [ExecutionPortMessage::RuntimeAckMessage(premature)] = premature.as_slice() else {
        panic!("premature runtime ack");
    };
    assert_eq!(premature.status, LeaseWriteStatus::RejectedConflict);

    let mut stale = runtime;
    stale.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 4));
    stale.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 4));
    stale.event.sequence = ExecutionSequence(2);
    stale.lease.fencing_token = FencingToken("6".to_owned());
    let rejected = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(stale.clone()),
            stale.sent_at,
        )
        .expect("stale runtime rejection");
    let [ExecutionPortMessage::RuntimeAckMessage(rejected)] = rejected.as_slice() else {
        panic!("stale runtime ack");
    };
    assert_eq!(rejected.status, LeaseWriteStatus::RejectedStaleFencingToken);
    fixture.close();
}
