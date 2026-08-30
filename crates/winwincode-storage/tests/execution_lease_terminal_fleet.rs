// SPDX-License-Identifier: Apache-2.0

//! Terminal lease authority joined to Worker heartbeat and Fleet projections.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkspaceId,
};
use winwincode_storage::{
    ActiveLeaseSummary, AuthenticatedWorkerPlacement, EXECUTION_PROTOCOL_VERSION,
    ExecutionLeaseClaim, ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest,
    LeaseRecovery, LeaseWriteStatus, SqliteStorage, StorageErrorKind, WorkerAuthenticationIdentity,
    WorkerFleetInventoryState, WorkerFleetSnapshotRequest, WorkerHeartbeatRequest, WorkerPlatform,
    WorkerPoolId, WorkerRegistrationRequest, WorkerRegistryScope,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-terminal-fleet-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        Self(root)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct Fixture {
    scope: WorkerRegistryScope,
    worker_pool_id: WorkerPoolId,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    lease: ExecutionLeaseClaim,
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn scope(seed: u64) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn authentication(seed: u64) -> WorkerAuthenticationIdentity {
    WorkerAuthenticationIdentity::TransportPrincipal {
        issuer: "terminal-fleet-fixture".to_owned(),
        subject: format!("remote-worker-{seed}"),
        credential_fingerprint: Sha256Digest(format!("sha256:{seed:064x}")),
    }
}

fn registration(seed: u64, instance: u64, request: u64) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: authentication(seed),
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        capabilities: vec!["model".to_owned(), "repository_read".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "terminal-fleet-zone".to_owned(),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", request)),
        request_id: RequestId(id("req", request)),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn heartbeat(
    fixture: &Fixture,
    sequence: i64,
    request: u64,
    observed_second: u64,
    running_slots: u64,
) -> WorkerHeartbeatRequest {
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 4 - running_slots,
        heartbeat_sequence: ExecutionSequence(sequence),
        max_slots: 4,
        running_slots,
        message_id: ExecutionMessageId(id("xmsg", request)),
        observed_at: instant(observed_second),
        sent_at: instant(observed_second),
        worker_id: fixture.worker_id.clone(),
        worker_instance_id: fixture.worker_instance_id.clone(),
    }
}

fn lease(seed: u64, worker: &WorkerRegistrationRequest) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: instant(40),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: instant(3),
        job_id: ExecutionJobId(id("job", seed)),
        lease_id: LeaseId(id("lse", seed)),
        message_id: ExecutionMessageId(id("xmsg", seed + 1_000)),
        payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        request_id: RequestId(id("req", seed + 1_000)),
        worker_id: worker.worker_id.clone(),
        worker_instance_id: worker.worker_instance_id.clone(),
        attempt: 1,
    }
}

fn terminal(
    lease: &ExecutionLeaseClaim,
    request: u64,
    outcome: ExecutionLeaseTerminalOutcome,
) -> ExecutionLeaseTerminalRequest {
    ExecutionLeaseTerminalRequest {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        attempt: lease.attempt,
        fencing_token: lease.fencing_token.clone(),
        outcome,
        terminal_at: instant(5),
        request_id: RequestId(id("req", request)),
    }
}

fn setup(root: &TestDirectory, seed: u64) -> Fixture {
    let owner_scope = scope(seed);
    let worker_pool_id = WorkerPoolId(id("wpl", seed));
    let worker = registration(seed, 1, seed + 10);
    let lease = lease(seed, &worker);
    let fixture = Fixture {
        scope: owner_scope.clone(),
        worker_pool_id: worker_pool_id.clone(),
        worker_id: worker.worker_id.clone(),
        worker_instance_id: worker.worker_instance_id.clone(),
        lease: lease.clone(),
    };
    let mut storage = SqliteStorage::open(&root.0).expect("open terminal Fleet storage");
    let mut registry = storage.execution_registry().expect("execution registry");
    registry
        .register_worker_for_scope(&worker, &owner_scope)
        .expect("register scoped Worker");
    registry
        .record_authenticated_worker_placement(&AuthenticatedWorkerPlacement {
            worker_id: worker.worker_id.clone(),
            worker_instance_id: worker.worker_instance_id.clone(),
            worker_pool_id,
            management_scope: owner_scope,
            authentication_identity: worker.authentication_identity.clone(),
            registration_request_id: worker.request_id.clone(),
            placed_at: instant(1),
        })
        .expect("record authenticated Worker placement");
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(&fixture, 1, seed + 20, 2, 1))
            .expect("record running heartbeat")
            .status,
        LeaseWriteStatus::Accepted
    );
    assert_eq!(
        registry
            .claim_execution_job_with_authenticated_placement(&lease)
            .expect("claim authenticated execution lease")
            .status,
        LeaseWriteStatus::Accepted
    );
    fixture
}

fn fleet_request(fixture: &Fixture, observed_second: u64) -> WorkerFleetSnapshotRequest {
    WorkerFleetSnapshotRequest {
        scope: fixture.scope.clone(),
        states: Vec::new(),
        observed_at: instant(observed_second),
        stale_after_ms: 5_000,
        limit: 10,
        cursor: None,
    }
}

fn assert_fleet(
    storage: &mut SqliteStorage,
    fixture: &Fixture,
    observed_second: u64,
    active_leases: u64,
    available_capacity: u64,
) {
    let page = storage
        .worker_fleet_inventory()
        .expect("Fleet inventory")
        .page(&fleet_request(fixture, observed_second))
        .expect("Fleet page");
    assert_eq!(page.items.len(), 1);
    let pool = &page.items[0];
    assert_eq!(pool.worker_pool_id, fixture.worker_pool_id);
    assert_eq!(pool.state, WorkerFleetInventoryState::Healthy);
    assert_eq!(pool.registered_workers, 1);
    assert_eq!(pool.usable_workers, 1);
    assert_eq!(pool.active_leases, active_leases);
    assert_eq!(pool.max_capacity, 4);
    assert_eq!(pool.available_capacity, available_capacity);
}

fn run_terminal_restart_case(outcome: ExecutionLeaseTerminalOutcome, seed: u64) {
    let root = TestDirectory::new("restart");
    let fixture = setup(&root, seed);
    let terminal = terminal(&fixture.lease, seed + 30, outcome);
    {
        let mut storage = SqliteStorage::open(&root.0).expect("open active Fleet storage");
        assert_fleet(&mut storage, &fixture, 4, 1, 3);
        let mut registry = storage.execution_registry().expect("execution registry");
        assert!(
            registry
                .finish_execution_lease(&terminal)
                .expect("finish execution lease")
        );
        assert!(
            !registry
                .finish_execution_lease(&terminal)
                .expect("exact terminal replay")
        );
        let mut changed = terminal.clone();
        changed.outcome = match outcome {
            ExecutionLeaseTerminalOutcome::Completed => ExecutionLeaseTerminalOutcome::Failed,
            ExecutionLeaseTerminalOutcome::Cancelled | ExecutionLeaseTerminalOutcome::Failed => {
                ExecutionLeaseTerminalOutcome::Completed
            }
        };
        assert_eq!(
            registry
                .finish_execution_lease(&changed)
                .expect_err("changed terminal replay is rejected")
                .kind(),
            StorageErrorKind::InvalidInput
        );
        assert_eq!(
            registry
                .record_heartbeat(&heartbeat(&fixture, 2, seed + 40, 6, 0))
                .expect("record released-capacity heartbeat")
                .status,
            LeaseWriteStatus::Accepted
        );
    }

    let mut restarted = SqliteStorage::open(&root.0).expect("restart terminal Fleet storage");
    assert!(
        !restarted
            .execution_registry()
            .expect("restart execution registry")
            .finish_execution_lease(&terminal)
            .expect("restart exact terminal replay")
    );
    assert_fleet(&mut restarted, &fixture, 7, 0, 4);
    let registry = restarted
        .execution_registry()
        .expect("restart execution registry");
    let worker = registry
        .load_worker(&fixture.worker_id)
        .expect("load restarted Worker")
        .expect("restarted Worker exists");
    assert_eq!(worker.running_slots, 0);
    assert_eq!(worker.available_slots, 4);
    assert_eq!(
        registry
            .load_lease(&fixture.lease.job_id)
            .expect("load retained fencing lease")
            .expect("fencing lease remains")
            .fencing_token,
        fixture.lease.fencing_token
    );
}

#[test]
fn completed_and_cancelled_terminals_replay_exactly_and_restore_fleet_capacity() {
    run_terminal_restart_case(ExecutionLeaseTerminalOutcome::Completed, 100);
    run_terminal_restart_case(ExecutionLeaseTerminalOutcome::Cancelled, 200);
}

#[test]
fn terminal_authority_rejects_old_instance_stale_fence_and_terminal_heartbeat() {
    let root = TestDirectory::new("authority");
    let fixture = setup(&root, 300);
    let exact = terminal(
        &fixture.lease,
        330,
        ExecutionLeaseTerminalOutcome::Completed,
    );
    let mut storage = SqliteStorage::open(&root.0).expect("open authority storage");
    let mut registry = storage.execution_registry().expect("execution registry");

    let mut old_instance = exact.clone();
    old_instance.worker_instance_id = WorkerInstanceId(id("wki", 999));
    old_instance.request_id = RequestId(id("req", 331));
    assert_eq!(
        registry
            .finish_execution_lease(&old_instance)
            .expect_err("foreign instance terminal is rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );
    let mut stale_fence = exact.clone();
    stale_fence.fencing_token = FencingToken("6".to_owned());
    stale_fence.request_id = RequestId(id("req", 332));
    assert_eq!(
        registry
            .finish_execution_lease(&stale_fence)
            .expect_err("stale fence terminal is rejected")
            .kind(),
        StorageErrorKind::InvalidInput
    );
    assert!(
        registry
            .finish_execution_lease(&exact)
            .expect("exact terminal succeeds")
    );

    let mut terminal_heartbeat = heartbeat(&fixture, 2, 333, 6, 1);
    terminal_heartbeat.active_leases = vec![ActiveLeaseSummary {
        job_id: fixture.lease.job_id.clone(),
        lease_id: fixture.lease.lease_id.clone(),
        attempt: fixture.lease.attempt,
        fencing_token: fixture.lease.fencing_token.clone(),
    }];
    assert_eq!(
        registry
            .record_heartbeat(&terminal_heartbeat)
            .expect("terminal lease heartbeat")
            .status,
        LeaseWriteStatus::RejectedConflict
    );
    assert_eq!(
        registry
            .load_worker(&fixture.worker_id)
            .expect("load unchanged Worker")
            .expect("Worker exists")
            .heartbeat_sequence,
        1
    );

    let replacement = registration(300, 2, 334);
    let receipt = registry
        .register_worker_for_scope(&replacement, &fixture.scope)
        .expect("replace Worker after terminal lease");
    assert_eq!(receipt.lease_recovery, LeaseRecovery::NoActiveLeases);
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(&fixture, 2, 335, 7, 0))
            .expect("old instance heartbeat")
            .status,
        LeaseWriteStatus::RejectedWorkerInstance
    );
}

#[test]
fn fleet_uses_its_observation_cut_and_rejects_regressing_heartbeat_time() {
    let root = TestDirectory::new("trusted-time");
    let owner_scope = scope(400);
    let worker_pool_id = WorkerPoolId(id("wpl", 400));
    let worker = registration(400, 1, 410);
    let fixture = Fixture {
        scope: owner_scope.clone(),
        worker_pool_id: worker_pool_id.clone(),
        worker_id: worker.worker_id.clone(),
        worker_instance_id: worker.worker_instance_id.clone(),
        lease: lease(400, &worker),
    };
    let mut storage = SqliteStorage::open(&root.0).expect("open trusted-time storage");
    {
        let mut registry = storage.execution_registry().expect("execution registry");
        registry
            .register_worker_for_scope(&worker, &owner_scope)
            .expect("register future-heartbeat Worker");
        registry
            .record_authenticated_worker_placement(&AuthenticatedWorkerPlacement {
                worker_id: worker.worker_id.clone(),
                worker_instance_id: worker.worker_instance_id.clone(),
                worker_pool_id,
                management_scope: owner_scope,
                authentication_identity: worker.authentication_identity,
                registration_request_id: worker.request_id,
                placed_at: instant(1),
            })
            .expect("record authenticated placement");
        assert_eq!(
            registry
                .record_heartbeat(&heartbeat(&fixture, 1, 420, 20, 0))
                .expect("record future heartbeat")
                .status,
            LeaseWriteStatus::Accepted
        );
    }

    let early = storage
        .worker_fleet_inventory()
        .expect("early Fleet inventory")
        .page(&fleet_request(&fixture, 4))
        .expect("early Fleet page");
    assert_eq!(early.items[0].state, WorkerFleetInventoryState::Offline);
    assert_eq!(early.items[0].usable_workers, 0);
    assert_eq!(early.items[0].available_capacity, 0);

    let current = storage
        .worker_fleet_inventory()
        .expect("current Fleet inventory")
        .page(&fleet_request(&fixture, 20))
        .expect("current Fleet page");
    assert_eq!(current.items[0].state, WorkerFleetInventoryState::Healthy);
    assert_eq!(current.items[0].usable_workers, 1);
    assert_eq!(current.items[0].available_capacity, 4);

    let mut registry = storage.execution_registry().expect("execution registry");
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(&fixture, 2, 421, 19, 0))
            .expect("regressing observed time heartbeat")
            .status,
        LeaseWriteStatus::RejectedConflict
    );
    let worker = registry
        .load_worker(&fixture.worker_id)
        .expect("load trusted heartbeat Worker")
        .expect("trusted heartbeat Worker exists");
    assert_eq!(worker.heartbeat_sequence, 1);
    assert_eq!(worker.last_heartbeat_at, Some(instant(20)));
}
