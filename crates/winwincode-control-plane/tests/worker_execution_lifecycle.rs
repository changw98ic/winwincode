// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use sha2::{Digest, Sha256};

use winwincode_control_plane::{
    DurableWorkerExecutionLifecycle, ExecutionPortService, ExecutionPortServiceError,
    WorkerEnterpriseQuotaClaim, WorkerExecutionRelease, WorkerExecutionUsageSettlement,
};
use winwincode_domain::{
    CodexThreadId, DeliveryId, EnterprisePolicyId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, SchemaVersion, SessionIdentity,
    Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, ExecutionJob,
    ExecutionLeaseStamp, ExecutionLimits, ExecutionOutcome, ExecutionOutcomeStatus,
    ExecutionOutcomeUsage, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
    JobOutcomeMessage, JobOutcomeMessageKind,
};
use winwincode_storage::{
    AuthenticatedWorkerPlacement, EXECUTION_PROTOCOL_VERSION, EnterprisePolicyActor,
    EnterprisePolicyChildOverrideMode, EnterprisePolicyDefinition, EnterprisePolicyEffect,
    EnterprisePolicyInheritanceMode, EnterprisePolicyKind, EnterprisePolicyMode,
    EnterprisePolicyScope, EnterprisePolicyState, EnterprisePolicyVersionSource,
    EnterprisePolicyWrite, EnterpriseQuotaBoundary, EnterpriseQuotaLimits, EnterpriseQuotaPolicy,
    EnterpriseQuotaReservationState, EnterpriseUsageFilter, EnterpriseUsageSourceKind,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionJobSubmission, ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, ExecutionReservationState,
    NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, StateCommit, WorkerAuthenticationIdentity, WorkerHeartbeatRequest,
    WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest, WorkerRegistryScope,
    WorkerSlotAuthority, WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-worker-execution-lifecycle-{name}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-08-12T08:00:{second:02}.000Z"))
}

fn scope() -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
        delivery_id: Some(DeliveryId(id("dlv", 5))),
        product_session_id: ProductSessionId(id("psn", 6)),
    }
}

fn pool() -> WorkerPoolId {
    WorkerPoolId(id("wpl", 7))
}

fn worker() -> WorkerId {
    WorkerId(id("wrk", 8))
}

fn instance() -> WorkerInstanceId {
    WorkerInstanceId(id("wki", 9))
}

fn job() -> ExecutionJobId {
    ExecutionJobId(id("job", 10))
}

fn digest() -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", "a".repeat(64)))
}

fn seed_worker_placement_deny(root: &PathBuf) {
    let definition = EnterprisePolicyDefinition {
        default_effect: EnterprisePolicyEffect::Deny,
        child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
        rules: Vec::new(),
    };
    let canonical = serde_json::to_value(&definition).expect("Policy value fixture");
    let definition_sha256 = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).expect("serialize Policy fixture"))
    ));
    SqliteStorage::open(root)
        .expect("Policy storage")
        .enterprise_policy_ledger()
        .expect("Policy ledger")
        .write(&EnterprisePolicyWrite {
            policy_id: EnterprisePolicyId(id("pol", 28)),
            policy_kind: EnterprisePolicyKind::WorkerPlacement,
            scope: EnterprisePolicyScope::Organization {
                organization_id: OrganizationId(id("org", 1)),
            },
            mode: EnterprisePolicyMode::Enforce,
            state: EnterprisePolicyState::Active,
            definition_sha256,
            definition,
            effective_at: at(1),
            inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
            base_version: None,
            expected_revision: 0,
            source: EnterprisePolicyVersionSource {
                actor: EnterprisePolicyActor::User {
                    id: UserId(id("usr", 14)),
                },
                request_id: RequestId(id("req", 28)),
            },
            updated_at: at(1),
        })
        .expect("write Worker Placement deny Policy");
}

fn identity() -> WorkerAuthenticationIdentity {
    WorkerAuthenticationIdentity::TransportPrincipal {
        issuer: "enterprise-worker-identity".to_owned(),
        subject: "remote-worker-08".to_owned(),
        credential_fingerprint: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
    }
}

fn claim() -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: at(50),
        fencing_token: FencingToken("1".to_owned()),
        issued_at: at(5),
        job_id: job(),
        lease_id: LeaseId(id("lse", 11)),
        message_id: ExecutionMessageId(id("xmsg", 12)),
        payload_digest: digest(),
        request_id: RequestId(id("req", 13)),
        worker_id: worker(),
        worker_instance_id: instance(),
        attempt: 1,
    }
}

fn wire_job() -> ExecutionJob {
    ExecutionJob {
        attempt: 1,
        execution_profile: "executor".to_owned(),
        goal: "Exercise the authenticated enterprise dispatch path.".to_owned(),
        job_id: job(),
        limits: ExecutionLimits {
            deadline_at: at(50),
            max_artifact_bytes: 1_000_000,
            max_runtime_seconds: 30,
        },
        payload_digest: digest(),
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id: DeliveryId(id("dlv", 5)),
            delivery_task_id: None,
            kind: DeliveryStageExecutionScopeKind::DeliveryStage,
            product_session_id: ProductSessionId(id("psn", 6)),
            rework_authorization: None,
            stage_run_id: StageRunId(id("run", 25)),
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "fixture-revision".to_owned(),
            repository_id: RepositoryId(id("rep", 4)),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    }
}

fn commit_durable_dispatch_intent(root: &PathBuf) {
    let job = wire_job();
    let mut storage = SqliteStorage::open(root).expect("storage");
    let identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(b"worker-lifecycle-fixture".to_vec()).expect("receipt actor"),
        ReceiptScopeKey::from_encoded(format!("worker-job:{}", job.job_id.0).into_bytes())
            .expect("receipt scope"),
        RequestId(id("req", 26)),
    )
    .expect("receipt identity");
    storage
        .commit(&StateCommit::new(
            identity,
            Sha256Digest(format!("sha256:{}", "d".repeat(64))),
            format!("delivery-execution-intent:{}", job.job_id.0),
            0,
            b"{}".to_vec(),
            vec![NewOutboxEvent::internal(
                format!("execution-job:{}", job.job_id.0),
                "execution.job.dispatch",
                serde_json::to_vec(&job).expect("wire Job payload"),
            )],
        ))
        .expect("durable dispatch intent");
}

fn terminal_outcome(status: ExecutionOutcomeStatus) -> JobOutcomeMessage {
    let claim = claim();
    let worker_session_id = WorkerSessionId(id("wsn", 21));
    JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: claim.expires_at,
            fencing_token: claim.fencing_token,
            issued_at: claim.issued_at,
            job_id: claim.job_id,
            lease_id: claim.lease_id,
            worker_id: claim.worker_id,
            worker_instance_id: claim.worker_instance_id,
        },
        message_id: ExecutionMessageId(id("xmsg", 27)),
        outcome: ExecutionOutcome {
            artifacts: Vec::new(),
            codex_thread_id: Some(CodexThreadId(id("cdx", 22))),
            error: None,
            finished_at: at(7),
            last_event_sequence: ExecutionAckSequence(1),
            status,
            summary: "Worker terminal fixture".to_owned(),
            usage: Some(ExecutionOutcomeUsage {
                cost_microunits: 400,
                runtime_millis: 4_000,
                tokens: 40,
            }),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at(7),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(id("cdx", 22)),
            product_session_id: ProductSessionId(id("psn", 6)),
            stage_run_id: Some(StageRunId(id("run", 25))),
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    }
}

fn configure_admission(storage: &mut SqliteStorage, denied: bool) {
    let scope = scope();
    let pool = pool();
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 8,
        max_queued: 8,
        token_budget: 10_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 60_000,
    };
    let boundaries = vec![
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
            delivery_id: scope.delivery_id.clone().expect("delivery"),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool.clone(),
        },
    ];
    let mut admission = storage.execution_admission().expect("admission");
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(id("usr", 14)),
            worker_pool_id: pool.clone(),
            job_id: job(),
            request_id: RequestId(id("req", 15)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: at(3),
        })
        .expect("operational reserve");
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id: pool,
            job_id: job(),
            request_id: RequestId(id("req", 16)),
            expected_revision: 1,
            started_at: at(4),
        })
        .expect("operational start");
    if denied {
        storage
            .enterprise_quota_ledger()
            .expect("quota")
            .put_policy(&EnterpriseQuotaPolicy {
                boundary: EnterpriseQuotaBoundary::Organization {
                    organization_id: OrganizationId(id("org", 1)),
                },
                revision: 1,
                limits: EnterpriseQuotaLimits {
                    operations: Some(0),
                    ..EnterpriseQuotaLimits::default()
                },
            })
            .expect("quota policy");
    }
}

fn seed(root: &PathBuf, denied: bool) {
    let mut storage = SqliteStorage::open(root).expect("storage");
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: scope(),
            job_id: job(),
            request_id: RequestId(id("req", 17)),
            payload_digest: digest(),
            dispatch_payload: serde_json::to_vec(&wire_job()).expect("canonical wire Job"),
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id: Some(StageRunId(id("run", 25))),
            submitted_at: at(3),
        })
        .expect("job submit");
    configure_admission(&mut storage, denied);
    let registration_request_id = RequestId(id("req", 18));
    let registration = WorkerRegistrationRequest {
        authentication_identity: identity(),
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
        security_zone: "enterprise-default".to_owned(),
        max_slots: 2,
        message_id: ExecutionMessageId(id("xmsg", 19)),
        request_id: registration_request_id.clone(),
        sent_at: at(1),
        started_at: at(0),
        worker_id: worker(),
        worker_instance_id: instance(),
    };
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker_for_scope(
            &registration,
            &WorkerRegistryScope::Repository {
                organization_id: OrganizationId(id("org", 1)),
                workspace_id: WorkspaceId(id("wsp", 2)),
                project_id: ProjectId(id("prj", 3)),
                repository_id: RepositoryId(id("rep", 4)),
            },
        )
        .expect("registration");
    registry
        .record_authenticated_worker_placement(&AuthenticatedWorkerPlacement {
            worker_id: worker(),
            worker_instance_id: instance(),
            worker_pool_id: pool(),
            management_scope: WorkerRegistryScope::Repository {
                organization_id: OrganizationId(id("org", 1)),
                workspace_id: WorkspaceId(id("wsp", 2)),
                project_id: ProjectId(id("prj", 3)),
                repository_id: RepositoryId(id("rep", 4)),
            },
            authentication_identity: identity(),
            registration_request_id,
            placed_at: at(1),
        })
        .expect("authenticated placement");
    registry
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 2,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 2,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", 20)),
            observed_at: at(2),
            sent_at: at(2),
            worker_id: worker(),
            worker_instance_id: instance(),
        })
        .expect("heartbeat");
}

fn open_slot(root: &PathBuf, lease: &ExecutionLeaseClaim) -> WorkerSessionId {
    let worker_session_id = WorkerSessionId(id("wsn", 21));
    let mut storage = SqliteStorage::open(root).expect("storage");
    let mut slots = storage.worker_session_slots().expect("slots");
    slots
        .configure_resources(
            &worker(),
            &instance(),
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000_000,
                max_disk_bytes: 1_000_000,
                max_processes: 10,
            },
        )
        .expect("slot limits");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: WorkerSlotAuthority {
                worker_id: worker(),
                worker_instance_id: instance(),
                worker_session_id: worker_session_id.clone(),
                codex_thread_id: CodexThreadId(id("cdx", 22)),
                job_id: lease.job_id.clone(),
                lease_id: lease.lease_id.clone(),
                attempt: lease.attempt,
                fencing_token: lease.fencing_token.clone(),
            },
            resources: WorkerSlotResources {
                memory_bytes: 100,
                disk_bytes: 100,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 23)),
            opened_at: at(6),
        })
        .expect("slot open");
    worker_session_id
}

#[test]
fn authenticated_claim_and_exact_usage_settlement_survive_restart() {
    let root = root("settle-restart");
    seed(&root, false);
    let lifecycle = DurableWorkerExecutionLifecycle::open(&root).expect("lifecycle");
    let claim = claim();
    let claimed = lifecycle.claim(&claim).expect("quota and Registry claim");
    let WorkerEnterpriseQuotaClaim::Claimed { operational, .. } = claimed else {
        panic!("expected accepted claim")
    };
    assert_eq!(
        operational.lease.as_ref().expect("lease").lease_id,
        claim.lease_id
    );
    let worker_session_id = open_slot(&root, &claim);
    let settlement = WorkerExecutionUsageSettlement {
        job_id: job(),
        worker_session_id,
        request_id: RequestId(id("req", 24)),
        actual_tokens: 40,
        actual_cost_microunits: 400,
        actual_runtime_millis: 4_000,
        completed_at: at(8),
    };
    let terminal = lifecycle
        .settle_usage(&settlement)
        .expect("terminal usage settlement");
    assert_eq!(
        terminal.operational.reservation.state,
        ExecutionReservationState::Settled
    );
    assert_eq!(
        terminal.enterprise.state,
        EnterpriseQuotaReservationState::Settled
    );
    drop(lifecycle);

    let restarted = DurableWorkerExecutionLifecycle::open(&root).expect("restart lifecycle");
    let replay = restarted
        .settle_usage(&settlement)
        .expect("exact terminal replay");
    assert!(replay.operational.replayed);
    assert_eq!(replay.enterprise, terminal.enterprise);

    let mut storage = SqliteStorage::open(&root).expect("storage reopen");
    let placement = storage
        .execution_registry()
        .expect("registry")
        .load_lease_placement(&job())
        .expect("placement load")
        .expect("placement");
    assert_eq!(placement.worker_pool_id, pool());
    let usage = storage
        .enterprise_usage_ledger()
        .expect("usage")
        .scan(
            &EnterpriseUsageFilter {
                source_kind: Some(EnterpriseUsageSourceKind::Worker),
                ..EnterpriseUsageFilter::default()
            },
            None,
            10,
        )
        .expect("usage scan");
    assert_eq!(usage.entries.len(), 1);

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn execution_port_authenticated_dispatch_uses_the_enterprise_lifecycle() {
    let root = root("production-dispatch");
    seed(&root, false);
    commit_durable_dispatch_intent(&root);
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let expected_claim = claim();
    let dispatch = ExecutionPortService::new(&mut storage, expected_claim.issued_at.clone())
        .claim_execution_job(wire_job(), expected_claim.clone())
        .expect("production authenticated dispatch");
    assert_eq!(dispatch.lease.lease_id, expected_claim.lease_id);
    assert_eq!(dispatch.job.job_id, expected_claim.job_id);
    drop(storage);

    let mut restarted = SqliteStorage::open(&root).expect("restart storage");
    let replay = ExecutionPortService::new(&mut restarted, expected_claim.issued_at.clone())
        .claim_execution_job(wire_job(), expected_claim.clone())
        .expect("exact production dispatch replay after restart");
    assert_eq!(replay, dispatch);
    let frozen = restarted
        .execution_registry()
        .expect("registry")
        .load_lease_placement(&expected_claim.job_id)
        .expect("lease placement read")
        .expect("authenticated lifecycle must freeze placement beside the lease");
    assert_eq!(frozen.worker_pool_id, pool());
    assert_eq!(frozen.worker_id, worker());
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn worker_placement_policy_denies_before_registry_claim_and_replays_one_audit() {
    let root = root("placement-policy-denial");
    seed(&root, false);
    seed_worker_placement_deny(&root);
    commit_durable_dispatch_intent(&root);
    let expected_claim = claim();
    for _ in 0..2 {
        let mut storage = SqliteStorage::open(&root).expect("storage");
        let error = ExecutionPortService::new(&mut storage, expected_claim.issued_at.clone())
            .claim_execution_job(wire_job(), expected_claim.clone())
            .expect_err("Worker Placement Policy must deny dispatch");
        assert!(matches!(
            error,
            ExecutionPortServiceError::WorkerLifecycle(_)
        ));
    }
    let mut storage = SqliteStorage::open(&root).expect("restart storage");
    assert!(
        storage
            .execution_registry()
            .expect("registry")
            .load_lease(&expected_claim.job_id)
            .expect("lease read")
            .is_none(),
        "Policy denial must precede the Registry lease write"
    );
    assert_eq!(
        storage
            .enterprise_policy_evaluation_ledger()
            .expect("Policy audit")
            .scan_audit(None, 10)
            .expect("scan Policy audit")
            .entries
            .len(),
        1,
        "exact replay must not duplicate Policy audit"
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn execution_port_quota_denial_prevents_the_registry_dispatch() {
    let root = root("production-dispatch-denial");
    seed(&root, true);
    commit_durable_dispatch_intent(&root);
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let expected_claim = claim();
    let error = ExecutionPortService::new(&mut storage, expected_claim.issued_at.clone())
        .claim_execution_job(wire_job(), expected_claim.clone())
        .expect_err("quota denial must stop dispatch");
    assert!(matches!(
        error,
        ExecutionPortServiceError::EnterpriseQuotaRejected
    ));
    drop(storage);

    let mut restarted = SqliteStorage::open(&root).expect("restart storage");
    let replay_error = ExecutionPortService::new(&mut restarted, expected_claim.issued_at.clone())
        .claim_execution_job(wire_job(), expected_claim.clone())
        .expect_err("quota denial must replay after restart");
    assert!(matches!(
        replay_error,
        ExecutionPortServiceError::EnterpriseQuotaRejected
    ));
    assert!(
        restarted
            .execution_registry()
            .expect("registry")
            .load_lease(&expected_claim.job_id)
            .expect("lease read")
            .is_none()
    );
    assert_eq!(
        restarted
            .execution_admission()
            .expect("admission")
            .load_reservation_by_job(&expected_claim.job_id)
            .expect("reservation read")
            .expect("reservation")
            .state,
        ExecutionReservationState::Released
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn quota_denial_releases_operational_admission_before_registry_side_effect() {
    let root = root("denial");
    seed(&root, true);
    let lifecycle = DurableWorkerExecutionLifecycle::open(&root).expect("lifecycle");
    assert!(matches!(
        lifecycle.claim(&claim()).expect("bounded denial"),
        WorkerEnterpriseQuotaClaim::Denied
    ));
    let mut storage = SqliteStorage::open(&root).expect("storage reopen");
    assert!(
        storage
            .execution_registry()
            .expect("registry")
            .load_lease(&job())
            .expect("lease load")
            .is_none()
    );
    let reservation = storage
        .execution_admission()
        .expect("admission")
        .load_reservation_by_job(&job())
        .expect("reservation load")
        .expect("reservation");
    assert_eq!(reservation.state, ExecutionReservationState::Released);

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn cancellation_releases_both_reservations_and_replays_after_restart() {
    let root = root("cancel-restart");
    seed(&root, false);
    let lifecycle = DurableWorkerExecutionLifecycle::open(&root).expect("lifecycle");
    assert!(matches!(
        lifecycle.claim(&claim()).expect("accepted claim"),
        WorkerEnterpriseQuotaClaim::Claimed { .. }
    ));
    let command = WorkerExecutionRelease {
        job_id: job(),
        request_id: RequestId(id("req", 25)),
        reason: winwincode_storage::ExecutionReservationReleaseReason::Cancelled,
        released_at: at(7),
    };
    let terminal = lifecycle
        .release(&command)
        .expect("release both reservations");
    assert_eq!(
        terminal.operational.reservation.state,
        ExecutionReservationState::Released
    );
    assert_eq!(
        terminal.enterprise.state,
        EnterpriseQuotaReservationState::Released
    );
    drop(lifecycle);

    let restarted = DurableWorkerExecutionLifecycle::open(&root).expect("restart lifecycle");
    let replay = restarted.release(&command).expect("release replay");
    assert!(replay.operational.replayed);
    assert_eq!(replay.enterprise, terminal.enterprise);

    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn immutable_failed_and_cancelled_outcomes_release_exactly_after_restart() {
    for (name, status) in [
        ("failed", ExecutionOutcomeStatus::Failed),
        ("cancelled", ExecutionOutcomeStatus::Cancelled),
    ] {
        let root = root(name);
        seed(&root, false);
        let lifecycle = DurableWorkerExecutionLifecycle::open(&root).expect("lifecycle");
        assert!(matches!(
            lifecycle.claim(&claim()).expect("accepted claim"),
            WorkerEnterpriseQuotaClaim::Claimed { .. }
        ));
        let outcome = terminal_outcome(status);
        let terminal = lifecycle
            .release_terminal_outcome(&outcome)
            .expect("terminal outcome release")
            .expect("authenticated terminal release");
        assert_eq!(
            terminal.operational.reservation.state,
            ExecutionReservationState::Released
        );
        assert_eq!(
            terminal.enterprise.state,
            EnterpriseQuotaReservationState::Released
        );
        drop(lifecycle);

        let restarted = DurableWorkerExecutionLifecycle::open(&root).expect("restart lifecycle");
        let replay = restarted
            .release_terminal_outcome(&outcome)
            .expect("terminal outcome release replay")
            .expect("authenticated terminal release replay");
        assert!(replay.operational.replayed);
        assert_eq!(replay.enterprise, terminal.enterprise);
        fs::remove_dir_all(root).expect("cleanup");
    }
}

#[test]
fn immutable_success_outcome_settles_exact_usage_once_after_restart() {
    let root = root("successful-terminal-outcome");
    seed(&root, false);
    let lifecycle = DurableWorkerExecutionLifecycle::open(&root).expect("lifecycle");
    assert!(matches!(
        lifecycle.claim(&claim()).expect("accepted claim"),
        WorkerEnterpriseQuotaClaim::Claimed { .. }
    ));
    open_slot(&root, &claim());
    let outcome = terminal_outcome(ExecutionOutcomeStatus::Succeeded);
    let terminal = lifecycle
        .settle_terminal_outcome(&outcome)
        .expect("terminal outcome settlement")
        .expect("authenticated terminal settlement");
    assert_eq!(terminal.operational.reservation.actual_tokens, Some(40));
    assert_eq!(
        terminal.operational.reservation.actual_cost_microunits,
        Some(400)
    );
    assert_eq!(
        terminal.operational.reservation.actual_runtime_millis,
        Some(4_000)
    );
    assert_eq!(
        terminal.enterprise.state,
        EnterpriseQuotaReservationState::Settled
    );
    drop(lifecycle);

    let restarted = DurableWorkerExecutionLifecycle::open(&root).expect("restart lifecycle");
    let replay = restarted
        .settle_terminal_outcome(&outcome)
        .expect("terminal outcome settlement replay")
        .expect("authenticated terminal settlement replay");
    assert!(replay.operational.replayed);
    assert_eq!(replay.enterprise, terminal.enterprise);
    fs::remove_dir_all(root).expect("cleanup");
}
