use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use serde_json::Value;
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, Instant, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    DispatchResultRequest, DispatchResultStatus, EXECUTION_PROTOCOL_VERSION,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionJobState, ExecutionJobSubmission, ExecutionLeaseTerminalOutcome,
    ExecutionLeaseTerminalRequest, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseRecovery,
    RepositorySchedulerCancellationRequest, RepositorySchedulerClaimReceipt,
    RepositorySchedulerClaimRequest, RepositorySchedulerDispatchResultRequest,
    RepositorySchedulerRetryRequest, RepositorySchedulerScope, RepositorySchedulerTerminalRequest,
    SchedulerRetryDecision, SchedulerRetryPolicy, SqliteStorage, StorageErrorKind,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotErrorCode, WorkerSlotEventAdvance,
    WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources, WorkerSlotState,
    scheduler_retry_decision,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-repository-replacement-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-10-01T10:00:{second:02}.000Z"))
}

fn repository() -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
    }
}

fn queue_scope() -> ExecutionQueueScope {
    let repository = repository();
    ExecutionQueueScope {
        organization_id: repository.organization_id,
        workspace_id: repository.workspace_id,
        project_id: repository.project_id,
        repository_id: repository.repository_id,
        product_session_id: ProductSessionId(id("psn", 5)),
        delivery_id: None,
    }
}

fn submit(storage: &mut SqliteStorage) {
    let job_id = ExecutionJobId(id("job", 6));
    let payload_digest = Sha256Digest(format!("sha256:{}", "a".repeat(64)));
    let dispatch_payload = serde_json::to_vec(&serde_json::json!({
        "attempt": 1,
        "executionProfile": "local-codex",
        "goal": "resume the exact durable execution",
        "jobId": job_id,
        "payloadDigest": payload_digest,
    }))
    .expect("dispatch payload");
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope(),
            job_id: ExecutionJobId(id("job", 6)),
            request_id: RequestId(id("req", 6)),
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            dispatch_payload,
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id: None,
            submitted_at: at(1),
        })
        .expect("submit");
}

fn register_instance(
    storage: &mut SqliteStorage,
    instance_seed: u64,
    request_seed: u64,
    started_second: u64,
) -> LeaseRecovery {
    let worker_id = WorkerId(id("wrk", 7));
    let worker_instance_id = WorkerInstanceId(id("wki", instance_seed));
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
            message_id: ExecutionMessageId(id("xmsg", request_seed)),
            request_id: RequestId(id("req", request_seed)),
            sent_at: at(started_second),
            started_at: at(started_second),
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
            message_id: ExecutionMessageId(id("xmsg", request_seed + 1)),
            observed_at: at(started_second + 1),
            sent_at: at(started_second + 1),
            worker_id,
            worker_instance_id,
        })
        .expect("heartbeat");
    receipt.lease_recovery
}

fn claim(
    request_seed: u64,
    generation: &str,
    instance_seed: u64,
    issued_second: u64,
) -> RepositorySchedulerClaimRequest {
    RepositorySchedulerClaimRequest {
        scope: repository(),
        request_id: RequestId(id("req", request_seed)),
        scheduler_generation: generation.into(),
        worker_id: WorkerId(id("wrk", 7)),
        worker_instance_id: WorkerInstanceId(id("wki", instance_seed)),
        issued_at: at(issued_second),
        expires_at: at(issued_second + 20),
    }
}

fn dispatch_result(
    claim: &RepositorySchedulerClaimReceipt,
    request_seed: u64,
    worker_session_seed: u64,
    second: u64,
) -> DispatchResultRequest {
    DispatchResultRequest {
        checked_at: at(second),
        expires_at: claim.lease.expires_at.clone(),
        fencing_token: claim.lease.fencing_token.clone(),
        issued_at: claim.lease.issued_at.clone(),
        job_id: claim.job.job_id.clone(),
        lease_id: claim.lease.lease_id.clone(),
        message_id: claim.message_id.clone(),
        payload_digest: claim.job.payload_digest.clone(),
        request_id: RequestId(id("req", request_seed)),
        sent_at: at(second),
        status: DispatchResultStatus::Accepted,
        attempt: claim.job.attempt,
        error: None,
        worker_id: claim.lease.worker_id.clone(),
        worker_instance_id: claim.lease.worker_instance_id.clone(),
        worker_session_id: Some(WorkerSessionId(id("wsn", worker_session_seed))),
    }
}

fn prepare_running_admission(storage: &mut SqliteStorage, job_id: &ExecutionJobId) {
    let scope = queue_scope();
    let worker_pool_id = WorkerPoolId(id("wpl", 300));
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
            user_id: UserId(id("usr", 301)),
            worker_pool_id: worker_pool_id.clone(),
            job_id: job_id.clone(),
            request_id: RequestId(id("req", 301)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: at(4),
        })
        .expect("reserve");
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id,
            job_id: job_id.clone(),
            request_id: RequestId(id("req", 302)),
            expected_revision: 1,
            started_at: at(5),
        })
        .expect("start");
}

struct RunningReplacementFixture {
    original: RepositorySchedulerClaimReceipt,
    original_dispatch: DispatchResultRequest,
    old_slot: WorkerSlotAuthority,
    resources: WorkerSlotResources,
}

fn running_replacement_fixture(storage: &mut SqliteStorage) -> RunningReplacementFixture {
    submit(storage);
    register_instance(storage, 8, 400, 2);
    let original = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(410, "boot-old", 8, 4))
        .expect("claim")
        .expect("dispatch");
    let original_dispatch = dispatch_result(&original, 411, 412, 5);
    let running = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: original_dispatch.clone(),
        })
        .expect("accepted dispatch");
    assert_eq!(running.job.state, ExecutionJobState::Running);
    prepare_running_admission(storage, &original.job.job_id);
    let old_slot = WorkerSlotAuthority {
        worker_id: original.lease.worker_id.clone(),
        worker_instance_id: original.lease.worker_instance_id.clone(),
        worker_session_id: original_dispatch
            .worker_session_id
            .clone()
            .expect("session"),
        codex_thread_id: CodexThreadId(id("cdx", 413)),
        job_id: original.job.job_id.clone(),
        lease_id: original.lease.lease_id.clone(),
        attempt: original.lease.attempt,
        fencing_token: original.lease.fencing_token.clone(),
    };
    let resources = WorkerSlotResources {
        memory_bytes: 10,
        disk_bytes: 10,
        process_slots: 1,
    };
    storage
        .worker_session_slots()
        .expect("slots")
        .configure_resources(
            &old_slot.worker_id,
            &old_slot.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 100,
                max_disk_bytes: 100,
                max_processes: 1,
            },
        )
        .expect("old resources");
    storage
        .worker_session_slots()
        .expect("slots")
        .open(&WorkerSlotOpenRequest {
            authority: old_slot.clone(),
            resources,
            request_id: RequestId(id("req", 414)),
            opened_at: at(6),
        })
        .expect("old slot");
    RunningReplacementFixture {
        original,
        original_dispatch,
        old_slot,
        resources,
    }
}

fn assert_old_authority_is_fenced(
    storage: &mut SqliteStorage,
    fixture: &RunningReplacementFixture,
    replacement: &RepositorySchedulerClaimReceipt,
) {
    let fenced_slot = storage
        .worker_session_slots()
        .expect("slots")
        .load(&fixture.old_slot.worker_session_id)
        .expect("old slot load")
        .expect("old slot");
    assert_eq!(fenced_slot.state, WorkerSlotState::RecoveryFailed);
    let mut late_dispatch = fixture.original_dispatch.clone();
    late_dispatch.request_id = RequestId(id("req", 431));
    late_dispatch.message_id = ExecutionMessageId(id("xmsg", 431));
    late_dispatch.sent_at = at(10);
    late_dispatch.checked_at = at(10);
    let late = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: late_dispatch,
        })
        .expect("late dispatch rejection");
    assert_eq!(
        late.dispatch.status,
        DispatchResultStatus::RejectedWorkerInstance
    );
    assert_eq!(late.job, replacement.job);
    let old_lease = &fixture.original.lease;
    let late_terminal = storage
        .execution_registry()
        .expect("registry")
        .finish_execution_lease(&ExecutionLeaseTerminalRequest {
            job_id: old_lease.job_id.clone(),
            lease_id: old_lease.lease_id.clone(),
            worker_id: old_lease.worker_id.clone(),
            worker_instance_id: old_lease.worker_instance_id.clone(),
            attempt: old_lease.attempt,
            fencing_token: old_lease.fencing_token.clone(),
            outcome: ExecutionLeaseTerminalOutcome::Completed,
            terminal_at: at(10),
            request_id: RequestId(id("req", 432)),
        })
        .expect_err("late terminal must be fenced");
    assert_eq!(late_terminal.kind(), StorageErrorKind::InvalidInput);
    let late_slot = storage
        .worker_session_slots()
        .expect("slots")
        .advance_event_cursor(&WorkerSlotEventAdvance {
            authority: fixture.old_slot.clone(),
            request_id: RequestId(id("req", 433)),
            expected_cursor: 0,
            next_cursor: 1,
            observed_at: at(10),
        })
        .expect_err("late slot event must be fenced");
    assert_eq!(late_slot.code(), WorkerSlotErrorCode::StateConflict);
}

fn accept_new_worker_session(
    storage: &mut SqliteStorage,
    fixture: &RunningReplacementFixture,
    replacement: &RepositorySchedulerClaimReceipt,
) {
    let replacement_dispatch = dispatch_result(replacement, 440, 441, 25);
    let replacement_running = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: replacement_dispatch.clone(),
        })
        .expect("replacement accepted");
    assert_eq!(replacement_running.job.state, ExecutionJobState::Running);
    let new_slot = WorkerSlotAuthority {
        worker_id: replacement.lease.worker_id.clone(),
        worker_instance_id: replacement.lease.worker_instance_id.clone(),
        worker_session_id: replacement_dispatch.worker_session_id.expect("new session"),
        codex_thread_id: CodexThreadId(id("cdx", 442)),
        job_id: replacement.job.job_id.clone(),
        lease_id: replacement.lease.lease_id.clone(),
        attempt: replacement.lease.attempt,
        fencing_token: replacement.lease.fencing_token.clone(),
    };
    storage
        .worker_session_slots()
        .expect("slots")
        .configure_resources(
            &new_slot.worker_id,
            &new_slot.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 100,
                max_disk_bytes: 100,
                max_processes: 1,
            },
        )
        .expect("new resources");
    let opened = storage
        .worker_session_slots()
        .expect("slots")
        .open(&WorkerSlotOpenRequest {
            authority: new_slot.clone(),
            resources: fixture.resources,
            request_id: RequestId(id("req", 443)),
            opened_at: at(26),
        })
        .expect("new slot");
    assert_eq!(opened.slot.authority, new_slot);
    assert_eq!(opened.slot.state, WorkerSlotState::Running);
}

#[test]
fn new_worker_instance_replaces_a_leased_job_once_and_replays_the_same_attempt() {
    let root = directory("leased");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    assert_eq!(
        register_instance(&mut storage, 8, 100, 2),
        LeaseRecovery::NoActiveLeases
    );
    let original = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(110, "boot-old", 8, 4))
        .expect("claim")
        .expect("dispatch");
    assert_eq!(original.job.state, ExecutionJobState::Leased);
    assert_eq!(original.job.attempt, 1);

    assert_eq!(
        register_instance(&mut storage, 9, 120, 6),
        LeaseRecovery::ReacquireRequired
    );
    let replacement_request = claim(130, "boot-new", 9, 24);
    let replacement = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&replacement_request)
        .expect("replacement claim")
        .expect("replacement dispatch");
    assert_eq!(replacement.job.job_id, original.job.job_id);
    assert_eq!(replacement.job.state, ExecutionJobState::Leased);
    assert_eq!(replacement.job.attempt, 2);
    assert_eq!(replacement.lease.attempt, 2);
    assert_eq!(
        replacement.lease.worker_instance_id,
        WorkerInstanceId(id("wki", 9))
    );
    assert_ne!(replacement.lease.lease_id, original.lease.lease_id);
    assert_ne!(
        replacement.lease.fencing_token,
        original.lease.fencing_token
    );
    assert_ne!(replacement.message_id, original.message_id);
    let payload: Value =
        serde_json::from_slice(&replacement.job.dispatch_payload).expect("replacement payload");
    assert_eq!(payload["attempt"], 2);

    drop(storage);
    let mut restarted = SqliteStorage::open(&root).expect("restart");
    let replay = restarted
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&replacement_request)
        .expect("exact replay")
        .expect("replacement replay");
    assert!(replay.replayed);
    assert_eq!(replay.job, replacement.job);
    assert_eq!(replay.lease, replacement.lease);
    assert_eq!(replay.message_id, replacement.message_id);

    let second_restart = restarted
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(131, "boot-newer", 9, 25))
        .expect("restart recovery")
        .expect("same attempt dispatch");
    assert_eq!(second_restart.job.attempt, 2);
    assert_eq!(second_restart.lease, replacement.lease);
    assert_eq!(second_restart.message_id, replacement.message_id);

    let mut changed = replacement_request;
    changed.expires_at = at(29);
    assert_eq!(
        restarted
            .repository_scheduler()
            .expect("scheduler")
            .claim_next(&changed)
            .expect_err("changed replay")
            .kind(),
        StorageErrorKind::RequestConflict
    );

    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn leased_replacement_waits_for_the_exact_lease_expiry_without_writing() {
    let root = directory("leased-expiry-gate");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    register_instance(&mut storage, 8, 140, 2);
    let original = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(141, "boot-old-expiry", 8, 4))
        .expect("claim")
        .expect("dispatch");
    register_instance(&mut storage, 9, 142, 6);
    let observer = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("observer connection");
    let before: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .expect("data version before wait");

    assert!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .claim_next(&claim(143, "boot-new-too-early", 9, 23))
            .expect("pre-expiry wait")
            .is_none()
    );
    let after: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .expect("data version after wait");
    assert_eq!(after, before, "pre-expiry wait committed a database write");

    let replacement = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(144, "boot-new-at-expiry", 9, 24))
        .expect("boundary replacement")
        .expect("replacement dispatch");
    assert_eq!(replacement.job.attempt, 2);
    assert_eq!(
        replacement.lease.worker_instance_id,
        WorkerInstanceId(id("wki", 9))
    );
    assert_eq!(original.lease.expires_at, at(24));

    drop(observer);
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn running_replacement_fences_old_authority_and_accepts_a_new_worker_session() {
    let root = directory("running");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let fixture = running_replacement_fixture(&mut storage);
    assert_eq!(
        register_instance(&mut storage, 9, 420, 7),
        LeaseRecovery::ReacquireRequired
    );
    let replacement = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(430, "boot-new", 9, 24))
        .expect("replacement")
        .expect("dispatch");
    assert_eq!(replacement.job.state, ExecutionJobState::Leased);
    assert_eq!(replacement.job.attempt, 2);
    assert_old_authority_is_fenced(&mut storage, &fixture, &replacement);
    accept_new_worker_session(&mut storage, &fixture, &replacement);

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn running_replacement_waits_for_the_exact_lease_expiry_without_writing() {
    let root = directory("running-expiry-gate");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let fixture = running_replacement_fixture(&mut storage);
    register_instance(&mut storage, 9, 450, 7);
    let observer = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("observer connection");
    let before: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .expect("data version before wait");

    assert!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .claim_next(&claim(451, "boot-running-too-early", 9, 23))
            .expect("pre-expiry wait")
            .is_none()
    );
    let after: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .expect("data version after wait");
    assert_eq!(after, before, "pre-expiry wait committed a database write");

    let replacement = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(452, "boot-running-at-expiry", 9, 24))
        .expect("boundary replacement")
        .expect("replacement dispatch");
    assert_eq!(fixture.original.lease.expires_at, at(24));
    assert_eq!(replacement.job.attempt, 2);
    assert_old_authority_is_fenced(&mut storage, &fixture, &replacement);

    drop(observer);
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn concurrent_exact_replacement_requests_commit_one_attempt_and_one_fence() {
    let root = directory("concurrent");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    register_instance(&mut storage, 8, 500, 2);
    storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(510, "boot-old", 8, 4))
        .expect("claim")
        .expect("dispatch");
    assert_eq!(
        register_instance(&mut storage, 9, 520, 6),
        LeaseRecovery::ReacquireRequired
    );
    drop(storage);

    let barrier = Arc::new(Barrier::new(3));
    let handles = [(530, "boot-new"), (530, "boot-new")]
        .into_iter()
        .map(|(request_seed, generation)| {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(&root).expect("concurrent storage");
                barrier.wait();
                storage
                    .repository_scheduler()
                    .expect("scheduler")
                    .claim_next(&claim(request_seed, generation, 9, 24))
                    .expect("concurrent claim")
                    .expect("concurrent dispatch")
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("replacement thread"))
        .collect::<Vec<_>>();
    assert_eq!(receipts[0].job.attempt, 2);
    assert_eq!(receipts[1].job.attempt, 2);
    assert_eq!(receipts[0].job, receipts[1].job);
    assert_eq!(receipts[0].lease, receipts[1].lease);
    assert_eq!(receipts[0].message_id, receipts[1].message_id);
    assert_ne!(receipts[0].replayed, receipts[1].replayed);
    assert_ne!(receipts[0].request_id, RequestId(id("req", 510)));

    let mut reopened = SqliteStorage::open(&root).expect("reopen");
    let durable = reopened
        .execution_queue()
        .expect("queue")
        .load_job(&queue_scope(), &ExecutionJobId(id("job", 6)))
        .expect("load")
        .expect("job");
    assert_eq!(durable.attempt, 2);
    assert_eq!(durable.state, ExecutionJobState::Leased);

    drop(reopened);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn completed_job_is_not_replaced() {
    let root = directory("completed");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    register_instance(&mut storage, 8, 600, 2);
    let completed_claim = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(610, "boot-completed", 8, 4))
        .expect("claim")
        .expect("dispatch");
    let completed_dispatch = dispatch_result(&completed_claim, 611, 612, 5);
    storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: completed_dispatch,
        })
        .expect("accepted dispatch");
    let completed = storage
        .repository_scheduler()
        .expect("scheduler")
        .settle_terminal(&RepositorySchedulerTerminalRequest {
            scope: repository(),
            terminal: ExecutionLeaseTerminalRequest {
                job_id: completed_claim.lease.job_id.clone(),
                lease_id: completed_claim.lease.lease_id.clone(),
                worker_id: completed_claim.lease.worker_id.clone(),
                worker_instance_id: completed_claim.lease.worker_instance_id.clone(),
                attempt: completed_claim.lease.attempt,
                fencing_token: completed_claim.lease.fencing_token.clone(),
                outcome: ExecutionLeaseTerminalOutcome::Completed,
                terminal_at: at(6),
                request_id: RequestId(id("req", 613)),
            },
        })
        .expect("completed terminal");
    assert_eq!(completed.job.state, ExecutionJobState::Completed);
    assert_eq!(
        register_instance(&mut storage, 9, 620, 7),
        LeaseRecovery::NoActiveLeases
    );
    assert!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .claim_next(&claim(621, "boot-after-completed", 9, 9))
            .expect("terminal claim")
            .is_none()
    );
    let completed_retry = RepositorySchedulerRetryRequest {
        scope: repository(),
        job_id: completed.job.job_id,
        request_id: RequestId(id("req", 622)),
        scheduler_generation: "retry-after-completed".into(),
        worker_id: WorkerId(id("wrk", 7)),
        worker_instance_id: WorkerInstanceId(id("wki", 9)),
        retryable_failure: true,
        failed_at_tick: 100,
        now_tick: 105,
        policy: SchedulerRetryPolicy {
            max_attempts: 3,
            initial_backoff_ticks: 5,
            max_backoff_ticks: 20,
        },
        issued_at: at(10),
        expires_at: at(30),
    };
    assert_eq!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .retry_failed(&completed_retry)
            .expect_err("completed retry")
            .kind(),
        StorageErrorKind::InvalidInput
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn cancelled_job_is_not_replaced() {
    let root = directory("cancelled");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    let cancelled = storage
        .repository_scheduler()
        .expect("scheduler")
        .request_cancellation(&RepositorySchedulerCancellationRequest {
            scope: repository(),
            job_id: ExecutionJobId(id("job", 6)),
            request_id: RequestId(id("req", 630)),
            expected_revision: 1,
            requested_at: at(2),
        })
        .expect("queued cancellation");
    assert_eq!(cancelled.job.state, ExecutionJobState::Failed);
    register_instance(&mut storage, 8, 631, 3);
    assert!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .claim_next(&claim(632, "boot-after-cancelled", 8, 5))
            .expect("cancelled claim")
            .is_none()
    );
    assert_eq!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .retry_failed(&RepositorySchedulerRetryRequest {
                scope: repository(),
                job_id: cancelled.job.job_id,
                request_id: RequestId(id("req", 633)),
                scheduler_generation: "retry-after-cancelled".into(),
                worker_id: WorkerId(id("wrk", 7)),
                worker_instance_id: WorkerInstanceId(id("wki", 8)),
                retryable_failure: true,
                failed_at_tick: 100,
                now_tick: 105,
                policy: SchedulerRetryPolicy {
                    max_attempts: 3,
                    initial_backoff_ticks: 5,
                    max_backoff_ticks: 20,
                },
                issued_at: at(6),
                expires_at: at(26),
            })
            .expect_err("cancelled retry")
            .kind(),
        StorageErrorKind::InvalidInput
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn failed_job_requires_the_existing_explicit_retry_policy() {
    let root = directory("failed");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    register_instance(&mut storage, 8, 640, 2);
    let failed_claim = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(650, "boot-failed", 8, 4))
        .expect("claim")
        .expect("dispatch");
    let mut rejected = dispatch_result(&failed_claim, 651, 652, 5);
    rejected.status = DispatchResultStatus::RejectedCapacity;
    rejected.worker_session_id = None;
    let failed = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: rejected,
        })
        .expect("rejected dispatch");
    assert_eq!(failed.job.state, ExecutionJobState::Failed);
    assert_eq!(
        scheduler_retry_decision(
            &failed.job,
            true,
            100,
            SchedulerRetryPolicy {
                max_attempts: 3,
                initial_backoff_ticks: 5,
                max_backoff_ticks: 20,
            },
        )
        .expect("retry policy"),
        SchedulerRetryDecision::Retry {
            next_attempt: 2,
            eligible_at_tick: 105,
        }
    );
    assert_eq!(
        register_instance(&mut storage, 9, 660, 7),
        LeaseRecovery::NoActiveLeases
    );
    assert!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .claim_next(&claim(661, "boot-after-failed", 9, 9))
            .expect("failed replacement claim")
            .is_none()
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn running_replacement_seals_the_old_and_new_scope_binding_authority() {
    let root = directory("scope-authority");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    register_instance(&mut storage, 8, 700, 2);
    let original = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(701, "boot-old-scope", 8, 4))
        .expect("claim")
        .expect("dispatch");
    let running = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: dispatch_result(&original, 702, 703, 5),
        })
        .expect("accepted dispatch");
    assert_eq!(running.job.state, ExecutionJobState::Running);

    register_instance(&mut storage, 9, 704, 7);
    let replacement = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(705, "boot-new-scope", 9, 24))
        .expect("replacement")
        .expect("replacement dispatch");
    let authority = storage
        .load_execution_scope_replacement(&replacement.job.job_id)
        .expect("load replacement authority")
        .expect("sealed replacement authority");
    assert_eq!(authority.job_id(), &replacement.job.job_id);
    assert_eq!(authority.scope(), &replacement.job.scope);
    assert_eq!(authority.previous_attempt(), 1);
    assert_eq!(authority.replacement_attempt(), 2);
    assert_eq!(
        authority.previous_worker_session_id(),
        Some(&WorkerSessionId(id("wsn", 703)))
    );
    assert!(authority.predecessor_slot().is_none());
    assert_eq!(authority.previous_lease_id(), &original.lease.lease_id);
    assert_eq!(authority.replacement_lease(), &replacement.lease);
    assert!(!authority.applied());

    drop(storage);
    let restarted = SqliteStorage::open(&root).expect("restart");
    assert_eq!(
        restarted
            .load_execution_scope_replacement(&replacement.job.job_id)
            .expect("restart replacement authority")
            .expect("durable replacement authority"),
        authority
    );
    drop(restarted);
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("tamper connection");
    connection
        .execute(
            "UPDATE execution_scope_replacements SET receipt_digest = ?1 WHERE job_id = ?2",
            [
                format!("sha256:{}", "f".repeat(64)),
                replacement.job.job_id.0.clone(),
            ],
        )
        .expect("tamper replacement seal");
    drop(connection);
    let corrupted = SqliteStorage::open(&root).expect("corrupt restart");
    assert_eq!(
        corrupted
            .load_execution_scope_replacement(&replacement.job.job_id)
            .expect_err("tampered replacement seal"),
        winwincode_storage::StorageError::adapter(
            "execution replacement authority digest is corrupt"
        )
    );
    drop(corrupted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn failed_retry_is_receipt_first_policy_gated_and_claims_one_higher_attempt() {
    let root = directory("failed-production-retry");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    register_instance(&mut storage, 8, 720, 2);
    let first = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(721, "boot-first-retry", 8, 4))
        .expect("claim")
        .expect("dispatch");
    let mut rejected = dispatch_result(&first, 722, 723, 5);
    rejected.status = DispatchResultStatus::RejectedCapacity;
    rejected.worker_session_id = None;
    let failed = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: rejected,
        })
        .expect("failed dispatch");
    assert_eq!(failed.job.state, ExecutionJobState::Failed);

    register_instance(&mut storage, 9, 724, 7);
    let request = RepositorySchedulerRetryRequest {
        scope: repository(),
        job_id: failed.job.job_id.clone(),
        request_id: RequestId(id("req", 725)),
        scheduler_generation: "boot-retry".into(),
        worker_id: WorkerId(id("wrk", 7)),
        worker_instance_id: WorkerInstanceId(id("wki", 9)),
        retryable_failure: true,
        failed_at_tick: 100,
        now_tick: 105,
        policy: SchedulerRetryPolicy {
            max_attempts: 3,
            initial_backoff_ticks: 5,
            max_backoff_ticks: 20,
        },
        issued_at: at(9),
        expires_at: at(29),
    };
    let ineligible = RepositorySchedulerRetryRequest {
        request_id: RequestId(id("req", 726)),
        now_tick: 104,
        issued_at: at(8),
        expires_at: at(28),
        ..request.clone()
    };
    assert!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .retry_failed(&ineligible)
            .expect("ineligible retry")
            .is_none()
    );
    assert_eq!(
        storage
            .execution_queue()
            .expect("queue")
            .load_job(&queue_scope(), &failed.job.job_id)
            .expect("failed job")
            .expect("failed job exists")
            .state,
        ExecutionJobState::Failed
    );
    let retry = storage
        .repository_scheduler()
        .expect("scheduler")
        .retry_failed(&request)
        .expect("production retry")
        .expect("eligible retry dispatch");
    assert_eq!(retry.job.state, ExecutionJobState::Leased);
    assert_eq!(retry.job.attempt, 2);
    assert_eq!(retry.lease.attempt, 2);
    assert_ne!(retry.lease.lease_id, first.lease.lease_id);

    drop(storage);
    let mut restarted = SqliteStorage::open(&root).expect("restart storage");
    let replay = restarted
        .repository_scheduler()
        .expect("scheduler")
        .retry_failed(&request)
        .expect("exact retry replay")
        .expect("replayed retry dispatch");
    assert!(replay.replayed);
    assert_eq!(replay.job, retry.job);
    assert_eq!(replay.lease, retry.lease);
    let mut changed = request;
    changed.now_tick += 1;
    assert_eq!(
        restarted
            .repository_scheduler()
            .expect("scheduler")
            .retry_failed(&changed)
            .expect_err("changed retry body")
            .kind(),
        StorageErrorKind::RequestConflict
    );

    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn concurrent_exact_failed_retry_commits_one_attempt_and_one_lease() {
    let root = directory("failed-concurrent-retry");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage);
    register_instance(&mut storage, 8, 740, 2);
    let first = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim(741, "boot-failed-concurrent", 8, 4))
        .expect("claim")
        .expect("dispatch");
    let mut rejected = dispatch_result(&first, 742, 743, 5);
    rejected.status = DispatchResultStatus::RejectedCapacity;
    rejected.worker_session_id = None;
    let failed = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: rejected,
        })
        .expect("failed dispatch");
    assert_eq!(failed.job.state, ExecutionJobState::Failed);
    register_instance(&mut storage, 9, 744, 7);
    drop(storage);

    let request = RepositorySchedulerRetryRequest {
        scope: repository(),
        job_id: failed.job.job_id.clone(),
        request_id: RequestId(id("req", 745)),
        scheduler_generation: "boot-concurrent-retry".into(),
        worker_id: WorkerId(id("wrk", 7)),
        worker_instance_id: WorkerInstanceId(id("wki", 9)),
        retryable_failure: true,
        failed_at_tick: 100,
        now_tick: 105,
        policy: SchedulerRetryPolicy {
            max_attempts: 3,
            initial_backoff_ticks: 5,
            max_backoff_ticks: 20,
        },
        issued_at: at(9),
        expires_at: at(29),
    };
    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            let request = request.clone();
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(&root).expect("concurrent storage");
                barrier.wait();
                storage
                    .repository_scheduler()
                    .expect("scheduler")
                    .retry_failed(&request)
                    .expect("concurrent retry")
                    .expect("concurrent retry dispatch")
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("retry thread"))
        .collect::<Vec<_>>();
    assert_eq!(receipts[0].job.attempt, 2);
    assert_eq!(receipts[1].job.attempt, 2);
    assert_eq!(receipts[0].job, receipts[1].job);
    assert_eq!(receipts[0].lease, receipts[1].lease);
    assert_eq!(receipts[0].message_id, receipts[1].message_id);
    assert_ne!(receipts[0].replayed, receipts[1].replayed);

    let mut reopened = SqliteStorage::open(&root).expect("reopen");
    let durable = reopened
        .execution_queue()
        .expect("queue")
        .load_job(&queue_scope(), &failed.job.job_id)
        .expect("load")
        .expect("job");
    assert_eq!(durable.attempt, 2);
    assert_eq!(durable.state, ExecutionJobState::Leased);
    drop(reopened);
    fs::remove_dir_all(root).expect("cleanup");
}
