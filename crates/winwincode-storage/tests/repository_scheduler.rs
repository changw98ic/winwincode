#![allow(clippy::too_many_lines)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, Instant, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    DispatchResultRequest, DispatchResultStatus, EXECUTION_PROTOCOL_VERSION, ExecutionJobState,
    ExecutionJobSubmission, ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest,
    ExecutionQueueScope, RepositorySchedulerCancellationRequest, RepositorySchedulerClaimRequest,
    RepositorySchedulerDispatchResultRequest, RepositorySchedulerScope,
    RepositorySchedulerTerminalRequest, SqliteStorage, StorageErrorKind,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform,
    WorkerRegistrationRequest,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-repository-scheduler-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-09-28T10:00:{second:02}.000Z"))
}

fn repository() -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
    }
}

fn queue_scope(session: u64) -> ExecutionQueueScope {
    let repository = repository();
    ExecutionQueueScope {
        organization_id: repository.organization_id,
        workspace_id: repository.workspace_id,
        project_id: repository.project_id,
        repository_id: repository.repository_id,
        product_session_id: ProductSessionId(id("psn", session)),
        delivery_id: None,
    }
}

fn submit(storage: &mut SqliteStorage, session: u64, job: u64, second: u64) {
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope(session),
            job_id: ExecutionJobId(id("job", job)),
            request_id: RequestId(id("req", job)),
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            dispatch_payload: format!(r#"{{"jobId":"{}"}}"#, id("job", job)).into_bytes(),
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id: None,
            submitted_at: at(second),
        })
        .expect("submit");
}

fn register(storage: &mut SqliteStorage) -> (WorkerId, WorkerInstanceId) {
    let worker_id = WorkerId(id("wrk", 1));
    let worker_instance_id = WorkerInstanceId(id("wki", 1));
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
            max_slots: 4,
            message_id: ExecutionMessageId(id("xmsg", 1)),
            request_id: RequestId(id("req", 900)),
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
            available_slots: 4,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 4,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", 2)),
            observed_at: at(2),
            sent_at: at(2),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("heartbeat");
    (worker_id, worker_instance_id)
}

fn claim_request(
    request: u64,
    generation: &str,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
) -> RepositorySchedulerClaimRequest {
    RepositorySchedulerClaimRequest {
        scope: repository(),
        request_id: RequestId(id("req", request)),
        scheduler_generation: generation.into(),
        worker_id,
        worker_instance_id,
        issued_at: at(3),
        expires_at: at(50),
    }
}

fn cancellation_request(
    job: u64,
    request: u64,
    expected_revision: u64,
    second: u64,
) -> RepositorySchedulerCancellationRequest {
    RepositorySchedulerCancellationRequest {
        scope: repository(),
        job_id: ExecutionJobId(id("job", job)),
        request_id: RequestId(id("req", request)),
        expected_revision,
        requested_at: at(second),
    }
}

#[test]
fn repository_claim_is_fair_receipt_first_and_restart_reoffers_exact_dispatch() {
    let root = directory("fair-restart");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage, 10, 10, 1);
    submit(&mut storage, 10, 11, 2);
    submit(&mut storage, 20, 20, 3);
    let (worker_id, worker_instance_id) = register(&mut storage);

    let first_request = claim_request(100, "boot-A", worker_id.clone(), worker_instance_id.clone());
    let first = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&first_request)
        .expect("first claim")
        .expect("dispatch");
    assert_eq!(first.job.job_id, ExecutionJobId(id("job", 10)));
    assert_eq!(first.job.state, ExecutionJobState::Leased);
    assert!(!first.replayed);
    assert!(!first.recovered);

    let replay = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&first_request)
        .expect("claim replay")
        .expect("dispatch replay");
    assert_eq!(replay.job, first.job);
    assert_eq!(replay.lease, first.lease);
    assert_eq!(replay.message_id, first.message_id);
    assert!(replay.replayed);

    let second = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim_request(
            101,
            "boot-A",
            worker_id.clone(),
            worker_instance_id.clone(),
        ))
        .expect("second claim")
        .expect("second dispatch");
    assert_eq!(second.job.job_id, ExecutionJobId(id("job", 20)));
    let third = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim_request(
            102,
            "boot-A",
            worker_id.clone(),
            worker_instance_id.clone(),
        ))
        .expect("third claim")
        .expect("third dispatch");
    assert_eq!(third.job.job_id, ExecutionJobId(id("job", 11)));

    drop(storage);
    let mut restarted = SqliteStorage::open(&root).expect("restart");
    let recovered = restarted
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim_request(103, "boot-B", worker_id, worker_instance_id))
        .expect("recovery claim")
        .expect("recovery dispatch");
    assert_eq!(recovered.job.job_id, first.job.job_id);
    assert_eq!(recovered.lease, first.lease);
    assert_eq!(recovered.message_id, first.message_id);
    assert!(recovered.recovered);

    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn rejected_registry_claim_rolls_back_queue_and_scheduler_receipts() {
    let root = directory("rollback");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage, 10, 30, 1);
    let request = claim_request(
        200,
        "boot-A",
        WorkerId(id("wrk", 9)),
        WorkerInstanceId(id("wki", 9)),
    );

    assert!(
        storage
            .repository_scheduler()
            .expect("scheduler")
            .claim_next(&request)
            .is_err()
    );
    let job = storage
        .execution_queue()
        .expect("queue")
        .load_job(&queue_scope(10), &ExecutionJobId(id("job", 30)))
        .expect("load")
        .expect("job");
    assert_eq!(job.state, ExecutionJobState::Queued);
    assert!(
        storage
            .execution_registry()
            .expect("registry")
            .load_lease(&job.job_id)
            .expect("lease")
            .is_none()
    );

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn queued_cancellation_terminalizes_without_worker_and_replays_after_restart() {
    let root = directory("queued-cancel");
    let request = cancellation_request(40, 300, 1, 4);
    let first = {
        let mut storage = SqliteStorage::open(&root).expect("storage");
        submit(&mut storage, 10, 40, 1);
        let first = storage
            .repository_scheduler()
            .expect("scheduler")
            .request_cancellation(&request)
            .expect("cancel queued");
        assert_eq!(first.job.state, ExecutionJobState::Failed);
        assert!(first.lease.is_none());
        assert!(first.worker_session_id.is_none());
        assert!(first.message_id.is_none());
        assert!(!first.replayed);
        first
    };

    let mut restarted = SqliteStorage::open(&root).expect("restart");
    let replay = restarted
        .repository_scheduler()
        .expect("scheduler")
        .request_cancellation(&request)
        .expect("cancel replay");
    assert_eq!(replay.job, first.job);
    assert!(replay.replayed);
    let mut changed = request;
    changed.requested_at = at(5);
    assert_eq!(
        restarted
            .repository_scheduler()
            .expect("scheduler")
            .request_cancellation(&changed)
            .expect_err("changed replay")
            .kind(),
        StorageErrorKind::RequestConflict
    );

    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn accepted_dispatch_cancel_and_terminal_join_queue_and_registry_exactly() {
    let root = directory("active-cancel-terminal");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    submit(&mut storage, 10, 50, 1);
    let (worker_id, worker_instance_id) = register(&mut storage);
    let claim = storage
        .repository_scheduler()
        .expect("scheduler")
        .claim_next(&claim_request(
            400,
            "boot-A",
            worker_id.clone(),
            worker_instance_id.clone(),
        ))
        .expect("claim")
        .expect("dispatch");
    let worker_session_id = WorkerSessionId(id("wsn", 50));
    let dispatch_request_id = RequestId(id("req", 401));
    let dispatch = DispatchResultRequest {
        checked_at: at(4),
        expires_at: claim.lease.expires_at.clone(),
        fencing_token: claim.lease.fencing_token.clone(),
        issued_at: claim.lease.issued_at.clone(),
        job_id: claim.job.job_id.clone(),
        lease_id: claim.lease.lease_id.clone(),
        message_id: claim.message_id.clone(),
        payload_digest: claim.job.payload_digest.clone(),
        request_id: dispatch_request_id,
        sent_at: at(4),
        status: DispatchResultStatus::Accepted,
        attempt: claim.job.attempt,
        error: None,
        worker_id,
        worker_instance_id,
        worker_session_id: Some(worker_session_id.clone()),
    };
    let running = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch: dispatch.clone(),
        })
        .expect("dispatch result")
        .job;
    let dispatch_replay = storage
        .repository_scheduler()
        .expect("scheduler")
        .record_dispatch_result(&RepositorySchedulerDispatchResultRequest {
            scope: repository(),
            dispatch,
        })
        .expect("dispatch replay");
    assert_eq!(dispatch_replay.job, running);
    assert!(dispatch_replay.accepted);
    assert!(dispatch_replay.dispatch.replayed);
    let cancel = storage
        .repository_scheduler()
        .expect("scheduler")
        .request_cancellation(&cancellation_request(50, 403, running.revision, 5))
        .expect("cancel running");
    assert_eq!(cancel.job.state, ExecutionJobState::Cancelling);
    assert_eq!(cancel.lease.as_ref(), Some(&claim.lease));
    assert_eq!(cancel.worker_session_id.as_ref(), Some(&worker_session_id));
    assert!(cancel.message_id.is_some());

    let terminal = RepositorySchedulerTerminalRequest {
        scope: repository(),
        terminal: ExecutionLeaseTerminalRequest {
            job_id: claim.job.job_id.clone(),
            lease_id: claim.lease.lease_id.clone(),
            worker_id: claim.lease.worker_id.clone(),
            worker_instance_id: claim.lease.worker_instance_id.clone(),
            attempt: claim.lease.attempt,
            fencing_token: claim.lease.fencing_token.clone(),
            outcome: ExecutionLeaseTerminalOutcome::Cancelled,
            terminal_at: at(6),
            request_id: RequestId(id("req", 404)),
        },
    };
    let settled = storage
        .repository_scheduler()
        .expect("scheduler")
        .settle_terminal(&terminal)
        .expect("terminal");
    assert_eq!(settled.job.state, ExecutionJobState::Failed);
    assert!(settled.lease_inserted);

    drop(storage);
    let mut restarted = SqliteStorage::open(&root).expect("restart");
    let replay = restarted
        .repository_scheduler()
        .expect("scheduler")
        .settle_terminal(&terminal)
        .expect("terminal replay");
    assert_eq!(replay.job, settled.job);
    assert!(!replay.lease_inserted);
    assert!(replay.replayed);

    let mut changed = terminal;
    changed.terminal.outcome = ExecutionLeaseTerminalOutcome::Failed;
    assert_eq!(
        restarted
            .repository_scheduler()
            .expect("scheduler")
            .settle_terminal(&changed)
            .expect_err("changed terminal")
            .kind(),
        StorageErrorKind::InvalidInput
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}
