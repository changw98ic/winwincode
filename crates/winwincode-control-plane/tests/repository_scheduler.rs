#![allow(clippy::too_many_lines)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::RepositoryExecutionScheduler;
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, Instant, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RepositoryScope, RepositoryScopeKind, RequestId,
    SchemaVersion, Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionLimits, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
    JobDispatchResultMessage, JobDispatchResultMessageKind, JobDispatchResultMessageStatus,
    ProductSessionExecutionScope, ProductSessionExecutionScopeKind,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionJobState, ExecutionJobSubmission, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationStart,
    LeaseRecovery, RepositorySchedulerCancellationRequest, RepositorySchedulerClaimRequest,
    RepositorySchedulerRetryRequest, RepositorySchedulerScope, SchedulerRetryPolicy, SqliteStorage,
    StorageErrorKind, WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform,
    WorkerPoolId, WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn directory() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-cp-repository-scheduler-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-09-29T10:00:{second:02}.000Z"))
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
    }
}

fn scheduler_scope() -> RepositorySchedulerScope {
    let scope = repository_scope();
    RepositorySchedulerScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
    }
}

fn queue_scope() -> ExecutionQueueScope {
    let scope = scheduler_scope();
    ExecutionQueueScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        product_session_id: ProductSessionId(id("psn", 5)),
        delivery_id: None,
    }
}

fn execution_job() -> ExecutionJob {
    ExecutionJob {
        attempt: 1,
        execution_profile: "local-codex".into(),
        goal: "Return one exact local response".into(),
        job_id: ExecutionJobId(id("job", 6)),
        limits: ExecutionLimits {
            deadline_at: at(50),
            max_artifact_bytes: 1_024,
            max_runtime_seconds: 30,
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
            kind: ProductSessionExecutionScopeKind::ProductSession,
            product_session_id: queue_scope().product_session_id,
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "0123456789abcdef".into(),
            repository_id: scheduler_scope().repository_id,
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    }
}

fn register(storage: &mut SqliteStorage) -> (WorkerId, WorkerInstanceId) {
    let worker_id = WorkerId(id("wrk", 7));
    let worker_instance_id = WorkerInstanceId(id("wki", 8));
    storage
        .execution_registry()
        .expect("registry")
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "fixture-control-plane".into(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".into()],
            capability_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            security_zone: "local".into(),
            max_slots: 1,
            message_id: ExecutionMessageId(id("xmsg", 9)),
            request_id: RequestId(id("req", 9)),
            sent_at: at(1),
            started_at: at(0),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("register");
    storage
        .execution_registry()
        .expect("registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 1,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", 10)),
            observed_at: at(2),
            sent_at: at(2),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("heartbeat");
    (worker_id, worker_instance_id)
}

fn register_replacement(storage: &mut SqliteStorage) -> (WorkerId, WorkerInstanceId) {
    let worker_id = WorkerId(id("wrk", 7));
    let worker_instance_id = WorkerInstanceId(id("wki", 21));
    let receipt = storage
        .execution_registry()
        .expect("registry")
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "fixture-control-plane".into(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".into()],
            capability_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            security_zone: "local".into(),
            max_slots: 1,
            message_id: ExecutionMessageId(id("xmsg", 22)),
            request_id: RequestId(id("req", 22)),
            sent_at: at(6),
            started_at: at(6),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("replacement register");
    assert_eq!(receipt.lease_recovery, LeaseRecovery::ReacquireRequired);
    storage
        .execution_registry()
        .expect("registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 1,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", 23)),
            observed_at: at(7),
            sent_at: at(7),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("replacement heartbeat");
    (worker_id, worker_instance_id)
}

fn register_retry_worker(storage: &mut SqliteStorage) -> (WorkerId, WorkerInstanceId) {
    let worker_id = WorkerId(id("wrk", 7));
    let worker_instance_id = WorkerInstanceId(id("wki", 51));
    let receipt = storage
        .execution_registry()
        .expect("registry")
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "fixture-control-plane".into(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".into()],
            capability_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            security_zone: "local".into(),
            max_slots: 1,
            message_id: ExecutionMessageId(id("xmsg", 51)),
            request_id: RequestId(id("req", 51)),
            sent_at: at(5),
            started_at: at(5),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("retry register");
    assert_eq!(receipt.lease_recovery, LeaseRecovery::NoActiveLeases);
    storage
        .execution_registry()
        .expect("registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 1,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", 52)),
            observed_at: at(6),
            sent_at: at(6),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("retry heartbeat");
    (worker_id, worker_instance_id)
}

fn prepare_running_admission(storage: &mut SqliteStorage, job_id: &ExecutionJobId) {
    let scope = queue_scope();
    let worker_pool_id = WorkerPoolId(id("wpl", 18));
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
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        },
    ];
    let mut admission = storage.execution_admission().expect("admission");
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(id("usr", 19)),
            worker_pool_id: worker_pool_id.clone(),
            job_id: job_id.clone(),
            request_id: RequestId(id("req", 19)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: at(3),
        })
        .expect("reserve");
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id,
            job_id: job_id.clone(),
            request_id: RequestId(id("req", 20)),
            expected_revision: 1,
            started_at: at(4),
        })
        .expect("start");
}

#[test]
fn typed_dispatch_cancel_and_restart_replay_use_only_durable_authority() {
    let root = directory();
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let job = execution_job();
    let dispatch_payload = serde_json::to_vec(&job).expect("canonical job");
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope(),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 11)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload,
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id: None,
            submitted_at: at(1),
        })
        .expect("submit");
    let (worker_id, worker_instance_id) = register(&mut storage);
    let dispatch = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope: scheduler_scope(),
            request_id: RequestId(id("req", 12)),
            scheduler_generation: "boot-A".into(),
            worker_id,
            worker_instance_id,
            issued_at: at(3),
            expires_at: at(40),
        })
        .expect("claim")
        .expect("dispatch");
    assert_eq!(dispatch.job, job);
    assert_eq!(dispatch.sent_at, dispatch.lease.issued_at);
    assert!(dispatch.replacement_authority.is_none());

    let worker_session_id = WorkerSessionId(id("wsn", 13));
    let running = RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(
            &repository_scope(),
            &JobDispatchResultMessage {
                error: None,
                job_id: dispatch.job.job_id.clone(),
                kind: JobDispatchResultMessageKind::JobDispatchResult,
                lease: dispatch.lease.clone(),
                message_id: ExecutionMessageId(id("xmsg", 14)),
                payload_digest: dispatch.job.payload_digest.clone(),
                request_id: RequestId(id("req", 14)),
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: at(4),
                status: JobDispatchResultMessageStatus::Accepted,
                worker_session_id: Some(worker_session_id.clone()),
            },
            &at(4),
        )
        .expect("dispatch result");
    assert!(running.accepted);
    assert_eq!(running.job.state, ExecutionJobState::Running);

    let slot_authority = WorkerSlotAuthority {
        worker_id: dispatch.lease.worker_id.clone(),
        worker_instance_id: dispatch.lease.worker_instance_id.clone(),
        worker_session_id: worker_session_id.clone(),
        codex_thread_id: CodexThreadId(id("cdx", 15)),
        job_id: dispatch.job.job_id.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        attempt: 1,
        fencing_token: dispatch.lease.fencing_token.clone(),
    };
    prepare_running_admission(&mut storage, &dispatch.job.job_id);
    {
        let mut slots = storage.worker_session_slots().expect("slots");
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
            .expect("resources");
        slots
            .open(&WorkerSlotOpenRequest {
                authority: slot_authority.clone(),
                resources: WorkerSlotResources {
                    memory_bytes: 10,
                    disk_bytes: 10,
                    process_slots: 1,
                },
                request_id: RequestId(id("req", 16)),
                opened_at: at(5),
            })
            .expect("slot");
    }

    let cancellation = RepositorySchedulerCancellationRequest {
        scope: scheduler_scope(),
        job_id: dispatch.job.job_id.clone(),
        request_id: RequestId(id("req", 17)),
        expected_revision: running.job.revision,
        requested_at: at(6),
    };
    let cancel = RepositoryExecutionScheduler::new(&mut storage)
        .request_cancellation(&cancellation)
        .expect("cancel")
        .expect("typed cancel");
    assert_eq!(cancel.worker_session_id, worker_session_id);
    assert_eq!(
        cancel.session_identity.codex_thread_id,
        slot_authority.codex_thread_id
    );
    assert_eq!(
        cancel.session_identity.product_session_id,
        queue_scope().product_session_id
    );
    assert!(cancel.session_identity.stage_run_id.is_none());

    drop(storage);
    let mut restarted = SqliteStorage::open(&root).expect("restart");
    let pending = RepositoryExecutionScheduler::new(&mut restarted)
        .pending_cancellations(&scheduler_scope())
        .expect("pending cancellations");
    assert_eq!(pending, vec![cancel]);

    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn typed_dispatch_rotates_attempt_and_lease_for_a_new_worker_process() {
    let root = directory();
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let job = execution_job();
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope(),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 31)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(&job).expect("canonical job"),
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id: None,
            submitted_at: at(1),
        })
        .expect("submit");
    let (worker_id, worker_instance_id) = register(&mut storage);
    let original = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope: scheduler_scope(),
            request_id: RequestId(id("req", 32)),
            scheduler_generation: "boot-old".into(),
            worker_id,
            worker_instance_id,
            issued_at: at(3),
            expires_at: at(40),
        })
        .expect("claim")
        .expect("dispatch");

    let (worker_id, worker_instance_id) = register_replacement(&mut storage);
    assert!(
        RepositoryExecutionScheduler::new(&mut storage)
            .claim_next(&RepositorySchedulerClaimRequest {
                scope: scheduler_scope(),
                request_id: RequestId(id("req", 330)),
                scheduler_generation: "boot-new-too-early".into(),
                worker_id: worker_id.clone(),
                worker_instance_id: worker_instance_id.clone(),
                issued_at: at(39),
                expires_at: at(58),
            })
            .expect("pre-expiry wait")
            .is_none()
    );
    let replacement = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope: scheduler_scope(),
            request_id: RequestId(id("req", 33)),
            scheduler_generation: "boot-new".into(),
            worker_id,
            worker_instance_id: worker_instance_id.clone(),
            issued_at: at(40),
            expires_at: at(59),
        })
        .expect("replacement claim")
        .expect("replacement dispatch");
    let mut expected_job = job;
    expected_job.attempt = 2;
    assert_eq!(replacement.job, expected_job);
    assert_eq!(replacement.lease.attempt, 2);
    assert_eq!(replacement.lease.worker_instance_id, worker_instance_id);
    assert_ne!(replacement.lease.lease_id, original.lease.lease_id);
    assert_ne!(
        replacement.lease.fencing_token,
        original.lease.fencing_token
    );
    assert_ne!(replacement.message_id, original.message_id);
    let authority = replacement
        .replacement_authority
        .as_ref()
        .expect("sealed replacement authority");
    assert_eq!(authority.predecessor_lease, original.lease);
    assert_eq!(authority.successor_lease, replacement.lease);
    assert_eq!(authority.scope, replacement.job.scope);
    assert!(authority.predecessor_session_identity.is_none());
    assert_eq!(authority.created_at, at(40));

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn typed_failed_retry_is_policy_gated_receipt_first_and_restart_exact() {
    let root = directory();
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let job = execution_job();
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope(),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 41)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(&job).expect("canonical job"),
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id: None,
            submitted_at: at(1),
        })
        .expect("submit");
    let (worker_id, worker_instance_id) = register(&mut storage);
    let first = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope: scheduler_scope(),
            request_id: RequestId(id("req", 42)),
            scheduler_generation: "boot-failed".into(),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
            issued_at: at(3),
            expires_at: at(40),
        })
        .expect("claim")
        .expect("dispatch");
    let failed = RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(
            &repository_scope(),
            &JobDispatchResultMessage {
                error: None,
                job_id: first.job.job_id.clone(),
                kind: JobDispatchResultMessageKind::JobDispatchResult,
                lease: first.lease.clone(),
                message_id: ExecutionMessageId(id("xmsg", 43)),
                payload_digest: first.job.payload_digest.clone(),
                request_id: first.request_id.clone(),
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: at(4),
                status: JobDispatchResultMessageStatus::RejectedCapacity,
                worker_session_id: None,
            },
            &at(4),
        )
        .expect("failed dispatch");
    assert_eq!(failed.job.state, ExecutionJobState::Failed);
    let (retry_worker_id, retry_worker_instance_id) = register_retry_worker(&mut storage);
    let policy = SchedulerRetryPolicy {
        max_attempts: 3,
        initial_backoff_ticks: 5,
        max_backoff_ticks: 20,
    };
    let ineligible = RepositorySchedulerRetryRequest {
        scope: scheduler_scope(),
        job_id: job.job_id.clone(),
        request_id: RequestId(id("req", 44)),
        scheduler_generation: "boot-retry".into(),
        worker_id: retry_worker_id,
        worker_instance_id: retry_worker_instance_id,
        retryable_failure: true,
        failed_at_tick: 100,
        now_tick: 104,
        policy,
        issued_at: at(5),
        expires_at: at(45),
    };
    assert!(
        RepositoryExecutionScheduler::new(&mut storage)
            .retry_failed(&ineligible)
            .expect("ineligible retry")
            .is_none()
    );
    let request = RepositorySchedulerRetryRequest {
        request_id: RequestId(id("req", 45)),
        now_tick: 105,
        issued_at: at(6),
        expires_at: at(46),
        ..ineligible
    };
    let retry = RepositoryExecutionScheduler::new(&mut storage)
        .retry_failed(&request)
        .expect("eligible retry")
        .expect("typed retry dispatch");
    assert_eq!(retry.job.attempt, 2);
    assert_eq!(retry.lease.attempt, 2);
    assert_eq!(retry.job.job_id, first.job.job_id);
    let replacement = retry
        .replacement_authority
        .as_ref()
        .expect("sealed retry authority");
    assert_eq!(replacement.predecessor_lease, first.lease);
    assert_eq!(replacement.successor_lease, retry.lease);
    assert!(replacement.predecessor_session_identity.is_none());

    drop(storage);
    let mut restarted = SqliteStorage::open(&root).expect("restart");
    let replay = RepositoryExecutionScheduler::new(&mut restarted)
        .retry_failed(&request)
        .expect("restart exact retry")
        .expect("restart typed dispatch");
    assert_eq!(replay, retry);
    let mut changed = request;
    changed.now_tick += 1;
    let error = RepositoryExecutionScheduler::new(&mut restarted)
        .retry_failed(&changed)
        .expect_err("changed retry body");
    let winwincode_control_plane::RepositoryExecutionSchedulerError::Storage(error) = error else {
        panic!("changed retry must fail in the receipt owner");
    };
    assert_eq!(error.kind(), StorageErrorKind::RequestConflict);

    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}
