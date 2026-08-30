// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::too_many_lines)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_api::generated::ModelRoute;
use winwincode_control_plane::{
    ApplyObserverCheckpointCommand, ContinueProductSessionCommand, CreateProductSessionCommand,
    ObserverCheckpointKind, ObserverDecisionCommandContext, ObserverDecisionInput,
    ObserverDecisionKind, ObserverDecisionService, ObserverDecisionServiceErrorCode,
    ObserverDecisionState, ObserverExecutionSource, ObserverRuntimeTraceRef,
    ObserverSafeCheckpoint, ProductSessionCommandContext, ProductSessionService,
    RecordObserverDecisionCommand,
};
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, CredentialReferenceId, EvidenceId, ExecutionEventId,
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    ModelExchangeId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::action_gateway::GateDecision;
use winwincode_execution_port::runtime_trace_outbox::TraceGateOutcome;
use winwincode_session::{
    ExecutionRoute, ProductSessionState, RuntimeRouteAuthority, SessionBindingIdentity,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, ExecutionReservationState,
    LeaseWriteStatus, PublicEventActor, PublicEventScope, ReceiptActorKey, ReceiptIdentity,
    ReceiptScopeKey, SqliteStorage, WorkerAuthenticationIdentity, WorkerHeartbeatRequest,
    WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest, WorkerSlotAuthority,
    WorkerSlotCloseRequest, WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources,
    WorkerSlotState,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-observer-decision-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct RunningFixture {
    scope_key: ReceiptScopeKey,
    execution_scope: ExecutionQueueScope,
    authority: WorkerSlotAuthority,
    source: ObserverExecutionSource,
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-02-16T08:00:{second:02}.000Z"))
}

fn digest(value: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", value.to_string().repeat(64)))
}

fn actor_key() -> ReceiptActorKey {
    winwincode_storage::receipt_actor_key(&public_actor()).expect("actor key")
}

fn repository_scope_key() -> ReceiptScopeKey {
    winwincode_storage::receipt_scope_key(&public_scope()).expect("scope key")
}

fn public_actor() -> PublicEventActor {
    PublicEventActor::User {
        id: UserId(id("usr", 1)),
    }
}

fn public_scope() -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
    }
}

fn assert_observer_receipts_are_internal(directory: &TestDirectory) {
    let connection =
        Connection::open(directory.0.join("control-plane.sqlite3")).expect("outbox database");
    let public_observer_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox \
             WHERE topic LIKE 'observer-decision.%' AND projection_stream_kind IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("public Observer row count");
    assert_eq!(public_observer_rows, 0);
    let internal_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox \
             WHERE topic = 'observer-decision.receipt.internal.v1' \
               AND projection_stream_kind IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("internal Observer row count");
    assert_eq!(internal_rows, 2);
}

fn model_route() -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        model_id: "fixture-model".into(),
        provider_id: "fixture-provider".into(),
    }
}

fn pool() -> WorkerPoolId {
    WorkerPoolId(id("wpl", 1))
}

fn product_context(
    scope: &ReceiptScopeKey,
    request: u64,
    expected_revision: u64,
    second: u64,
) -> ProductSessionCommandContext {
    ProductSessionCommandContext {
        receipt_identity: ReceiptIdentity::new(
            actor_key(),
            scope.clone(),
            RequestId(id("req", request)),
        )
        .expect("receipt identity"),
        expected_revision,
        event_id: ControlPlaneEventId(id("evt", request)),
        occurred_at: at(second),
        public_actor: public_actor(),
        public_scope: public_scope(),
    }
}

fn observer_context(
    scope: &ReceiptScopeKey,
    request: u64,
    expected_revision: u64,
    second: u64,
) -> ObserverDecisionCommandContext {
    ObserverDecisionCommandContext {
        receipt_identity: ReceiptIdentity::new(
            actor_key(),
            scope.clone(),
            RequestId(id("req", request)),
        )
        .expect("receipt identity"),
        expected_revision,
        event_id: ControlPlaneEventId(id("evt", request)),
        occurred_at: at(second),
    }
}

fn execution_scope() -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
        delivery_id: None,
        product_session_id: ProductSessionId(id("psn", 1)),
    }
}

fn admission_boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
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
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool(),
        },
    ]
}

fn worker_registration() -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "observer-control-plane".into(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["artifact".into(), "codex".into()],
        capability_digest: digest('a'),
        security_zone: "local".into(),
        max_slots: 1,
        message_id: ExecutionMessageId(id("xmsg", 1)),
        request_id: RequestId(id("req", 10_001)),
        sent_at: at(2),
        started_at: at(1),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn worker_heartbeat() -> WorkerHeartbeatRequest {
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 1,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 1,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", 2)),
        observed_at: at(3),
        sent_at: at(3),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn prepare_running_fixture(storage: &mut SqliteStorage) -> RunningFixture {
    let scope_key = repository_scope_key();
    let scope = execution_scope();
    ProductSessionService::new(storage)
        .create(&CreateProductSessionCommand {
            context: product_context(&scope_key, 1, 0, 1),
            product_session_id: scope.product_session_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
            title: "Observer fixture".into(),
            model_route: model_route(),
        })
        .expect("ProductSession create");
    {
        let mut registry = storage.execution_registry().expect("registry");
        registry
            .register_worker(&worker_registration())
            .expect("Worker registration");
        assert_eq!(
            registry
                .record_heartbeat(&worker_heartbeat())
                .expect("Worker heartbeat")
                .status,
            LeaseWriteStatus::Accepted
        );
    }
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 1,
        max_queued: 1,
        token_budget: 10_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 60_000,
    };
    {
        let mut admission = storage.execution_admission().expect("admission");
        for boundary in admission_boundaries(&scope) {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("policy");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: scope.clone(),
                user_id: UserId(id("usr", 1)),
                worker_pool_id: pool(),
                job_id: ExecutionJobId(id("job", 1)),
                request_id: RequestId(id("req", 10)),
                repository_access: ExecutionRepositoryAccess::IsolatedWrite {
                    worktree_key: "worktree-observer-1".into(),
                },
                reserved_tokens: 100,
                reserved_cost_microunits: 1_000,
                runtime_limit_millis: 30_000,
                submitted_at: at(4),
            })
            .expect("reservation");
        admission
            .start(&ExecutionReservationStart {
                scope: scope.clone(),
                worker_pool_id: pool(),
                job_id: ExecutionJobId(id("job", 1)),
                request_id: RequestId(id("req", 11)),
                expected_revision: 1,
                started_at: at(5),
            })
            .expect("reservation start");
    }
    let lease = ExecutionLeaseClaim {
        expires_at: at(59),
        fencing_token: FencingToken("1".into()),
        issued_at: at(5),
        job_id: ExecutionJobId(id("job", 1)),
        lease_id: LeaseId(id("lse", 1)),
        message_id: ExecutionMessageId(id("xmsg", 3)),
        payload_digest: digest('b'),
        request_id: RequestId(id("req", 12)),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
        attempt: 1,
    };
    storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&lease)
        .expect("lease");
    let authority = WorkerSlotAuthority {
        worker_id: lease.worker_id,
        worker_instance_id: lease.worker_instance_id,
        worker_session_id: WorkerSessionId(id("wsn", 1)),
        codex_thread_id: CodexThreadId(id("cdx", 1)),
        job_id: lease.job_id,
        lease_id: lease.lease_id,
        attempt: lease.attempt,
        fencing_token: lease.fencing_token,
    };
    let slot = {
        let mut slots = storage.worker_session_slots().expect("slots");
        slots
            .configure_resources(
                &authority.worker_id,
                &authority.worker_instance_id,
                WorkerSlotResourceLimits {
                    max_memory_bytes: 1_000,
                    max_disk_bytes: 1_000,
                    max_processes: 1,
                },
            )
            .expect("slot limits");
        slots
            .open(&WorkerSlotOpenRequest {
                authority: authority.clone(),
                resources: WorkerSlotResources {
                    memory_bytes: 10,
                    disk_bytes: 20,
                    process_slots: 1,
                },
                request_id: RequestId(id("req", 13)),
                opened_at: at(6),
            })
            .expect("slot open")
            .slot
    };
    let session = ProductSessionService::new(storage)
        .continue_session(&ContinueProductSessionCommand {
            context: product_context(&scope_key, 2, 1, 7),
            product_session_id: scope.product_session_id.clone(),
            binding_identity: SessionBindingIdentity::product_session(
                scope.product_session_id.clone(),
                authority.job_id.clone(),
            )
            .expect("binding"),
            runtime_authority: authority.clone(),
            execution_scope: scope.clone(),
            worker_pool_id: pool(),
            model_exchange_id: ModelExchangeId(id("mdl", 1)),
        })
        .expect("ProductSession continue");
    let route = ExecutionRoute {
        product_session_id: scope.product_session_id.clone(),
        stage_run_id: None,
        execution_job_id: authority.job_id.clone(),
        job_revision: 1,
        runtime: Some(RuntimeRouteAuthority {
            lease_id: authority.lease_id.clone(),
            worker_id: authority.worker_id.clone(),
            worker_instance_id: authority.worker_instance_id.clone(),
            worker_session_id: authority.worker_session_id.clone(),
            attempt: authority.attempt,
            fencing_token: authority.fencing_token.clone(),
        }),
        worker_slot_revision: Some(slot.revision),
        model_exchange_id: None,
    };
    let source = ObserverExecutionSource::from_interaction_route(
        &route,
        session.record.session().revision(),
        authority.codex_thread_id.clone(),
        scope.clone(),
        pool(),
        ObserverRuntimeTraceRef {
            event_id: ExecutionEventId(id("xevt", 1)),
            sequence: 1,
            digest: digest('c'),
        },
    )
    .expect("Observer source");
    RunningFixture {
        scope_key,
        execution_scope: scope,
        authority,
        source,
    }
}

fn pause_command(
    fixture: &RunningFixture,
    request: u64,
    expected_revision: u64,
    safe_checkpoint: Option<ObserverSafeCheckpoint>,
) -> RecordObserverDecisionCommand {
    RecordObserverDecisionCommand {
        context: observer_context(&fixture.scope_key, request, expected_revision, 10),
        source: fixture.source.clone(),
        decision: ObserverDecisionInput::from_gate_decision(
            &GateDecision::PauseForHuman {
                reason: "Repository action needs an explicit product decision".into(),
            },
            vec![EvidenceId("evidence-observer-1".into())],
        )
        .expect("Pause maps to Observer"),
        safe_checkpoint,
    }
}

#[test]
fn pause_waits_for_safe_checkpoint_and_preserves_all_runtime_resources_across_restart() {
    let directory = TestDirectory::new("pause-checkpoint");
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let fixture = prepare_running_fixture(&mut storage);
    let pending = ObserverDecisionService::new(&mut storage)
        .record_decision(&pause_command(&fixture, 20, 0, None))
        .expect("pending pause");
    assert_eq!(pending.projection.kind(), ObserverDecisionKind::Pause);
    assert_eq!(
        pending.projection.state(),
        ObserverDecisionState::PausePending
    );
    assert!(pending.projection.state().blocks_new_actions());
    assert!(pending.projection.safe_checkpoint().is_none());
    assert!(pending.projection.preserves_session_worker_and_worktree());
    assert_eq!(
        pending
            .projection
            .retained_resources()
            .worktree_key
            .as_deref(),
        Some("worktree-observer-1")
    );

    let checkpoint = ApplyObserverCheckpointCommand {
        context: observer_context(&fixture.scope_key, 21, 1, 11),
        product_session_id: fixture.execution_scope.product_session_id.clone(),
        decision_event_id: fixture.source.runtime_trace.event_id.clone(),
        safe_checkpoint: ObserverSafeCheckpoint {
            kind: ObserverCheckpointKind::RuntimeCheckpoint,
            runtime_trace: ObserverRuntimeTraceRef {
                event_id: ExecutionEventId(id("xevt", 2)),
                sequence: 2,
                digest: digest('d'),
            },
            observed_at: at(11),
        },
    };
    let paused = ObserverDecisionService::new(&mut storage)
        .apply_checkpoint(&checkpoint)
        .expect("pause checkpoint");
    assert_eq!(paused.projection.state(), ObserverDecisionState::Paused);
    assert_eq!(paused.projection.session_revision(), 2);
    assert_eq!(
        paused
            .projection
            .safe_checkpoint()
            .expect("checkpoint")
            .kind,
        ObserverCheckpointKind::RuntimeCheckpoint
    );

    let session = ProductSessionService::new(&mut storage)
        .get(
            &fixture.scope_key,
            &fixture.execution_scope.product_session_id,
        )
        .expect("ProductSession read")
        .expect("ProductSession");
    assert_eq!(session.session().state(), ProductSessionState::Running);
    assert_eq!(session.bindings().len(), 1);
    let slot = storage
        .worker_session_slots()
        .expect("slots")
        .load(&fixture.authority.worker_session_id)
        .expect("slot read")
        .expect("slot");
    assert_eq!(slot.state, WorkerSlotState::Running);
    let reservation = storage
        .execution_admission()
        .expect("admission")
        .load_reservation(&fixture.execution_scope, &pool(), &fixture.authority.job_id)
        .expect("reservation read")
        .expect("reservation");
    assert_eq!(reservation.state, ExecutionReservationState::Running);
    assert_eq!(
        reservation.repository_access,
        ExecutionRepositoryAccess::IsolatedWrite {
            worktree_key: "worktree-observer-1".into()
        }
    );
    drop(storage);
    assert_observer_receipts_are_internal(&directory);

    let mut reopened = SqliteStorage::open(&directory.0).expect("storage restart");
    let restored = ObserverDecisionService::new(&mut reopened)
        .get_current(
            &fixture.scope_key,
            &fixture.execution_scope.product_session_id,
        )
        .expect("Observer read")
        .expect("Observer projection");
    assert_eq!(restored, paused.projection);
}

#[test]
fn disconnect_replay_returns_the_original_pause_without_a_second_mutation() {
    let directory = TestDirectory::new("disconnect-replay");
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let fixture = prepare_running_fixture(&mut storage);
    let command = pause_command(
        &fixture,
        30,
        0,
        Some(ObserverSafeCheckpoint {
            kind: ObserverCheckpointKind::BeforeAction,
            runtime_trace: fixture.source.runtime_trace.clone(),
            observed_at: at(10),
        }),
    );
    let first = ObserverDecisionService::new(&mut storage)
        .record_decision(&command)
        .expect("pause");
    assert_eq!(first.projection.state(), ObserverDecisionState::Paused);
    assert!(!first.replayed);

    storage
        .worker_session_slots()
        .expect("slots")
        .close(&WorkerSlotCloseRequest {
            authority: fixture.authority.clone(),
            request_id: RequestId(id("req", 31)),
            expected_revision: 1,
            outcome: WorkerSlotState::Failed,
            closed_at: at(12),
        })
        .expect("simulated disconnect");
    drop(storage);

    let mut reopened = SqliteStorage::open(&directory.0).expect("storage restart");
    let replay = ObserverDecisionService::new(&mut reopened)
        .record_decision(&command)
        .expect("disconnect replay");
    assert!(replay.replayed);
    assert_eq!(replay.projection, first.projection);
    let history = ObserverDecisionService::new(&mut reopened)
        .history(
            &fixture.scope_key,
            &fixture.execution_scope.product_session_id,
        )
        .expect("history");
    assert_eq!(history, vec![first.projection.clone()]);

    let mut changed = command;
    changed.decision = ObserverDecisionInput::codex(
        ObserverDecisionKind::Pause,
        "changed.request",
        "Changed semantic body",
        Vec::new(),
    );
    assert_eq!(
        ObserverDecisionService::new(&mut reopened)
            .record_decision(&changed)
            .expect_err("changed replay must conflict")
            .code(),
        ObserverDecisionServiceErrorCode::RequestConflict
    );
}

#[test]
fn source_and_checkpoint_validation_rejects_foreign_or_unsafe_facts() {
    let directory = TestDirectory::new("validation");
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let fixture = prepare_running_fixture(&mut storage);
    assert_eq!(
        ObserverDecisionInput::from_trace_gate_outcome(
            TraceGateOutcome::AllowWithWatch,
            "Continue while the observer watches",
            Vec::new(),
        )
        .expect("Watch trace")
        .kind(),
        ObserverDecisionKind::Watch
    );
    assert!(
        ObserverDecisionInput::from_trace_gate_outcome(
            TraceGateOutcome::DenyAction,
            "Action denied",
            Vec::new(),
        )
        .is_none()
    );

    let mut foreign = pause_command(&fixture, 40, 0, None);
    foreign.source.codex_thread_id = CodexThreadId(id("cdx", 999));
    assert_eq!(
        ObserverDecisionService::new(&mut storage)
            .record_decision(&foreign)
            .expect_err("foreign CodexThread")
            .code(),
        ObserverDecisionServiceErrorCode::SourceMismatch
    );

    ObserverDecisionService::new(&mut storage)
        .record_decision(&pause_command(&fixture, 41, 0, None))
        .expect("pending pause");
    let unsafe_checkpoint = ApplyObserverCheckpointCommand {
        context: observer_context(&fixture.scope_key, 42, 1, 11),
        product_session_id: fixture.execution_scope.product_session_id.clone(),
        decision_event_id: fixture.source.runtime_trace.event_id.clone(),
        safe_checkpoint: ObserverSafeCheckpoint {
            kind: ObserverCheckpointKind::RuntimeCheckpoint,
            runtime_trace: ObserverRuntimeTraceRef {
                event_id: ExecutionEventId(id("xevt", 999)),
                sequence: 0,
                digest: digest('f'),
            },
            observed_at: at(11),
        },
    };
    assert_eq!(
        ObserverDecisionService::new(&mut storage)
            .apply_checkpoint(&unsafe_checkpoint)
            .expect_err("zero trace sequence")
            .code(),
        ObserverDecisionServiceErrorCode::InvalidInput
    );
    let current = ObserverDecisionService::new(&mut storage)
        .get_current(
            &fixture.scope_key,
            &fixture.execution_scope.product_session_id,
        )
        .expect("current")
        .expect("decision");
    assert_eq!(current.state(), ObserverDecisionState::PausePending);
    assert_eq!(current.session_revision(), 1);
}
