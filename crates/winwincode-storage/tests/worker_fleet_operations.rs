// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    AuthenticatedWorkerPlacement, DispatchResultRequest, DispatchResultStatus,
    EXECUTION_PROTOCOL_VERSION, ExecutionLeaseClaim, LeaseWriteStatus, ProductStateStorage,
    SqliteStorage, StorageErrorKind, WorkerAuthenticationIdentity, WorkerFleetAction,
    WorkerFleetFailureCommand, WorkerFleetMemberHealth, WorkerFleetMemberObservation,
    WorkerFleetObservation, WorkerFleetReleaseVersion, WorkerFleetRolloutCommand,
    WorkerFleetRolloutPhase, WorkerFleetRolloutPolicy, WorkerHeartbeatRequest, WorkerPlatform,
    WorkerPoolId, WorkerRegistrationRequest, WorkerRegistryScope,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-worker-fleet-operations-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-03-01T08:00:{second:02}.000Z"))
}

fn scope(seed: u64) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn pool(seed: u64) -> WorkerPoolId {
    WorkerPoolId(id("wpl", seed))
}

fn authentication(worker: u64) -> WorkerAuthenticationIdentity {
    WorkerAuthenticationIdentity::TransportPrincipal {
        issuer: "fixture-issuer".to_owned(),
        subject: format!("worker-{worker}"),
        credential_fingerprint: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
    }
}

fn registration(worker: u64, instance: u64, request: u64) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: authentication(worker),
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        capabilities: vec!["artifact_stream".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        security_zone: "build-local".to_owned(),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", request)),
        request_id: RequestId(id("req", request)),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn placement(
    worker: u64,
    instance: u64,
    request: u64,
    worker_scope: &WorkerRegistryScope,
    worker_pool: &WorkerPoolId,
) -> AuthenticatedWorkerPlacement {
    AuthenticatedWorkerPlacement {
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
        worker_pool_id: worker_pool.clone(),
        management_scope: worker_scope.clone(),
        authentication_identity: authentication(worker),
        registration_request_id: RequestId(id("req", request)),
        placed_at: instant(1),
    }
}

fn register_remote_worker(
    storage: &mut SqliteStorage,
    worker: u64,
    instance: u64,
    request: u64,
    worker_scope: &WorkerRegistryScope,
    worker_pool: &WorkerPoolId,
) {
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker_for_scope(&registration(worker, instance, request), worker_scope)
        .expect("registration");
    registry
        .record_authenticated_worker_placement(&placement(
            worker,
            instance,
            request,
            worker_scope,
            worker_pool,
        ))
        .expect("placement");
}

fn member(
    worker: u64,
    instance: u64,
    version: u64,
    health: WorkerFleetMemberHealth,
) -> WorkerFleetMemberObservation {
    WorkerFleetMemberObservation {
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
        release_version: WorkerFleetReleaseVersion(version),
        health,
        active_leases: 0,
    }
}

fn observation(
    second: u64,
    capacity: u64,
    members: Vec<WorkerFleetMemberObservation>,
) -> WorkerFleetObservation {
    WorkerFleetObservation::seal(instant(second), capacity, members).expect("observation")
}

fn policy(stable: u64, target: u64, desired_capacity: u64) -> WorkerFleetRolloutPolicy {
    WorkerFleetRolloutPolicy {
        stable_version: WorkerFleetReleaseVersion(stable),
        target_version: WorkerFleetReleaseVersion(target),
        minimum_version: WorkerFleetReleaseVersion(stable),
        canary_size: 1,
        max_unavailable: 1,
        desired_capacity,
    }
}

fn rollout_command(
    request: u64,
    revision: u64,
    rollout_policy: WorkerFleetRolloutPolicy,
    observed: WorkerFleetObservation,
) -> WorkerFleetRolloutCommand {
    WorkerFleetRolloutCommand {
        request_id: RequestId(id("req", request)),
        scope: scope(1),
        worker_pool_id: pool(1),
        expected_revision: revision,
        policy: rollout_policy,
        observation: observed,
    }
}

fn replacement_worker(action: &WorkerFleetAction) -> Option<&WorkerId> {
    match action {
        WorkerFleetAction::DrainAndReplace { worker_id, .. } => Some(worker_id),
        WorkerFleetAction::SetPoolCapacity { .. } => None,
    }
}

#[test]
fn canary_rollout_scale_replay_restart_and_batches_are_deterministic() {
    let root = temporary_directory("rollout");
    let first_command = rollout_command(
        1,
        0,
        policy(1, 2, 4),
        observation(
            1,
            3,
            vec![
                member(1, 1, 1, WorkerFleetMemberHealth::Ready),
                member(2, 2, 1, WorkerFleetMemberHealth::Ready),
                member(3, 3, 1, WorkerFleetMemberHealth::Ready),
            ],
        ),
    );
    let first = {
        let mut storage = SqliteStorage::open(&root).expect("storage");
        storage
            .worker_fleet_operations()
            .expect("operations")
            .reconcile_rollout(&first_command)
            .expect("first rollout")
    };
    assert_eq!(first.record.phase, WorkerFleetRolloutPhase::Canary);
    assert_eq!(first.actions.len(), 2);
    assert_eq!(
        replacement_worker(&first.actions[1]),
        Some(&WorkerId(id("wrk", 1)))
    );

    let mut storage = SqliteStorage::open(&root).expect("restart");
    let replay = storage
        .worker_fleet_operations()
        .expect("operations")
        .reconcile_rollout(&first_command)
        .expect("exact replay");
    assert!(replay.replayed);
    assert_eq!(replay.actions, first.actions);
    assert_eq!(replay.record.revision, 1);

    let pending = storage
        .worker_fleet_operations()
        .expect("operations")
        .reconcile_rollout(&rollout_command(
            2,
            1,
            policy(1, 2, 4),
            first_command.observation.clone(),
        ))
        .expect("pending rollout");
    assert!(pending.actions.is_empty());

    let canary_ready = storage
        .worker_fleet_operations()
        .expect("operations")
        .reconcile_rollout(&rollout_command(
            3,
            2,
            policy(1, 2, 4),
            observation(
                3,
                4,
                vec![
                    member(1, 11, 2, WorkerFleetMemberHealth::Ready),
                    member(2, 2, 1, WorkerFleetMemberHealth::Ready),
                    member(3, 3, 1, WorkerFleetMemberHealth::Ready),
                ],
            ),
        ))
        .expect("canary ready");
    assert_eq!(canary_ready.record.phase, WorkerFleetRolloutPhase::Rolling);
    assert_eq!(canary_ready.actions.len(), 1);
    assert_eq!(
        replacement_worker(&canary_ready.actions[0]),
        Some(&WorkerId(id("wrk", 2)))
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove");
}

#[test]
fn failed_canary_rolls_back_with_the_same_idempotent_action_contract() {
    let root = temporary_directory("rollback");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let first = storage
        .worker_fleet_operations()
        .expect("operations")
        .reconcile_rollout(&rollout_command(
            10,
            0,
            policy(1, 2, 2),
            observation(
                1,
                2,
                vec![
                    member(1, 1, 1, WorkerFleetMemberHealth::Ready),
                    member(2, 2, 1, WorkerFleetMemberHealth::Ready),
                ],
            ),
        ))
        .expect("canary start");
    assert_eq!(
        replacement_worker(&first.actions[0]),
        Some(&WorkerId(id("wrk", 1)))
    );

    let failed = storage
        .worker_fleet_operations()
        .expect("operations")
        .reconcile_rollout(&rollout_command(
            11,
            1,
            policy(1, 2, 2),
            observation(
                2,
                2,
                vec![
                    member(1, 11, 2, WorkerFleetMemberHealth::Degraded),
                    member(2, 2, 1, WorkerFleetMemberHealth::Ready),
                ],
            ),
        ))
        .expect("canary failure");
    assert_eq!(failed.record.phase, WorkerFleetRolloutPhase::RollingBack);
    let WorkerFleetAction::DrainAndReplace { target_version, .. } = &failed.actions[0] else {
        panic!("rollback must use the canonical replacement action");
    };
    assert_eq!(*target_version, WorkerFleetReleaseVersion(1));

    let restored = storage
        .worker_fleet_operations()
        .expect("operations")
        .reconcile_rollout(&rollout_command(
            12,
            2,
            policy(1, 2, 2),
            observation(
                3,
                2,
                vec![
                    member(1, 12, 1, WorkerFleetMemberHealth::Ready),
                    member(2, 2, 1, WorkerFleetMemberHealth::Ready),
                ],
            ),
        ))
        .expect("rollback complete");
    assert_eq!(restored.record.phase, WorkerFleetRolloutPhase::Stable);
    assert_eq!(
        restored.record.policy.target_version,
        WorkerFleetReleaseVersion(1)
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove");
}

#[test]
fn stale_revision_changed_request_and_noncanonical_observation_fail_closed() {
    let root = temporary_directory("conflicts");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let command = rollout_command(
        20,
        0,
        policy(1, 2, 1),
        observation(1, 1, vec![member(1, 1, 1, WorkerFleetMemberHealth::Ready)]),
    );
    storage
        .worker_fleet_operations()
        .expect("operations")
        .reconcile_rollout(&command)
        .expect("first");
    let mut changed = command.clone();
    changed.observation.current_capacity = 0;
    assert_eq!(
        storage
            .worker_fleet_operations()
            .expect("operations")
            .reconcile_rollout(&changed)
            .expect_err("changed request")
            .kind(),
        StorageErrorKind::InvalidInput
    );
    changed.observation = observation(1, 0, vec![member(1, 1, 1, WorkerFleetMemberHealth::Ready)]);
    assert_eq!(
        storage
            .worker_fleet_operations()
            .expect("operations")
            .reconcile_rollout(&changed)
            .expect_err("changed canonical request")
            .kind(),
        StorageErrorKind::RequestConflict
    );
    let stale = rollout_command(
        21,
        0,
        policy(1, 2, 1),
        observation(2, 1, vec![member(1, 1, 1, WorkerFleetMemberHealth::Ready)]),
    );
    assert_eq!(
        storage
            .worker_fleet_operations()
            .expect("operations")
            .reconcile_rollout(&stale)
            .expect_err("stale revision")
            .kind(),
        StorageErrorKind::RevisionConflict
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove");
}

#[test]
fn minimum_version_and_concurrent_exact_request_produce_one_rollout_revision() {
    let root = temporary_directory("minimum-concurrent");
    let command = rollout_command(
        25,
        0,
        policy(2, 2, 1),
        observation(1, 1, vec![member(1, 1, 1, WorkerFleetMemberHealth::Ready)]),
    );
    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let root = root.clone();
            let command = command.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let mut storage = SqliteStorage::open(&root).expect("concurrent storage");
                storage
                    .worker_fleet_operations()
                    .expect("operations")
                    .reconcile_rollout(&command)
                    .expect("concurrent reconcile")
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let mut receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("concurrent thread"))
        .collect::<Vec<_>>();
    receipts.sort_by_key(|receipt| receipt.replayed);
    assert!(!receipts[0].replayed);
    assert!(receipts[1].replayed);
    assert_eq!(receipts[0].record.revision, 1);
    assert_eq!(receipts[0].actions, receipts[1].actions);
    let WorkerFleetAction::DrainAndReplace { target_version, .. } = &receipts[0].actions[0] else {
        panic!("minimum version must replace the stale Worker");
    };
    assert_eq!(*target_version, WorkerFleetReleaseVersion(2));
    let mut storage = SqliteStorage::open(&root).expect("restart");
    assert_eq!(
        storage
            .worker_fleet_operations()
            .expect("operations")
            .load_rollout(&scope(1), &pool(1))
            .expect("load rollout")
            .expect("rollout")
            .revision,
        1
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove");
}

#[test]
fn locked_database_open_fails_within_the_configured_busy_timeout() {
    let root = temporary_directory("bounded-locked-open");
    let storage = SqliteStorage::open(&root).expect("initialize locked storage");
    let database_path = storage.database_path().to_owned();
    Box::new(storage)
        .close()
        .expect("close initialized storage");

    let lock = rusqlite::Connection::open(database_path).expect("open lock connection");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("hold database write lock");
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let (sender, receiver) = mpsc::channel();
    let started_at = std::time::Instant::now();
    let handle = thread::spawn(move || {
        worker_barrier.wait();
        sender
            .send(
                SqliteStorage::open(&root)
                    .map(|_| ())
                    .map_err(|error| error.kind()),
            )
            .expect("report bounded open result");
        root
    });

    barrier.wait();
    let result = receiver
        .recv_timeout(Duration::from_secs(7))
        .expect("locked open must finish within its five-second busy timeout");
    assert_eq!(
        result.expect_err("write lock must reject schema setup"),
        StorageErrorKind::Adapter
    );
    assert!(started_at.elapsed() < Duration::from_secs(7));
    let root = handle.join().expect("bounded open thread");
    lock.execute_batch("ROLLBACK")
        .expect("release database lock");
    fs::remove_dir_all(root).expect("remove");
}

fn claim(
    worker: u64,
    instance: u64,
    request: u64,
    attempt: u64,
    fencing_token: u64,
    issued_second: u64,
) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: instant(9),
        fencing_token: FencingToken(fencing_token.to_string()),
        issued_at: instant(issued_second),
        job_id: ExecutionJobId(id("job", 1)),
        lease_id: LeaseId(id("lse", request)),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
        request_id: RequestId(id("req", request)),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
        attempt,
    }
}

fn dispatch_result(lease: &ExecutionLeaseClaim, request: u64) -> DispatchResultRequest {
    DispatchResultRequest {
        checked_at: instant(4),
        expires_at: lease.expires_at.clone(),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: lease.payload_digest.clone(),
        request_id: RequestId(id("req", request)),
        sent_at: instant(4),
        status: DispatchResultStatus::Accepted,
        attempt: lease.attempt,
        error: None,
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: Some(WorkerSessionId(id("wsn", request))),
    }
}

fn make_worker_healthy(storage: &mut SqliteStorage, worker: u64, instance: u64, request: u64) {
    storage
        .execution_registry()
        .expect("registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 4,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 4,
            message_id: ExecutionMessageId(id("xmsg", request)),
            observed_at: instant(2),
            running_slots: 0,
            sent_at: instant(2),
            worker_id: WorkerId(id("wrk", worker)),
            worker_instance_id: WorkerInstanceId(id("wki", instance)),
        })
        .expect("heartbeat");
}

#[test]
fn disconnected_fence_rejects_late_result_and_allows_one_higher_replacement() {
    let root = temporary_directory("failure-fence");
    let worker_scope = scope(1);
    let worker_pool = pool(1);
    let old_claim = claim(1, 1, 30, 1, 10, 2);
    let first = {
        let mut storage = SqliteStorage::open(&root).expect("storage");
        register_remote_worker(&mut storage, 1, 1, 31, &worker_scope, &worker_pool);
        register_remote_worker(&mut storage, 2, 2, 32, &worker_scope, &worker_pool);
        make_worker_healthy(&mut storage, 1, 1, 33);
        make_worker_healthy(&mut storage, 2, 2, 34);
        assert_eq!(
            storage
                .execution_registry()
                .expect("registry")
                .claim_execution_job_with_authenticated_placement(&old_claim)
                .expect("claim")
                .status,
            LeaseWriteStatus::Accepted
        );
        storage
            .execution_registry()
            .expect("registry")
            .mark_worker_disconnected(&WorkerId(id("wrk", 1)), &WorkerInstanceId(id("wki", 1)))
            .expect("disconnect");
        storage
            .worker_fleet_operations()
            .expect("operations")
            .fence_disconnected_worker(&WorkerFleetFailureCommand {
                request_id: RequestId(id("req", 35)),
                scope: worker_scope.clone(),
                worker_pool_id: worker_pool.clone(),
                worker_id: WorkerId(id("wrk", 1)),
                worker_instance_id: WorkerInstanceId(id("wki", 1)),
                detected_at: instant(3),
            })
            .expect("fence")
    };
    assert_eq!(first.fenced_leases.len(), 1);
    assert_eq!(first.fenced_leases[0].next_attempt, 2);
    assert_eq!(
        first.fenced_leases[0].next_fencing_token,
        FencingToken("11".to_owned())
    );

    let mut storage = SqliteStorage::open(&root).expect("restart");
    let late = storage
        .execution_registry()
        .expect("registry")
        .record_dispatch_result(&dispatch_result(&old_claim, 36))
        .expect("late result");
    assert_eq!(late.status, DispatchResultStatus::RejectedExpiredLease);
    let replacement = claim(2, 2, 37, 2, 11, 3);
    assert_eq!(
        storage
            .execution_registry()
            .expect("registry")
            .claim_execution_job_with_authenticated_placement(&replacement)
            .expect("replacement")
            .status,
        LeaseWriteStatus::Accepted
    );
    let stale_after_replacement = storage
        .execution_registry()
        .expect("registry")
        .record_dispatch_result(&dispatch_result(&old_claim, 38))
        .expect("stale result");
    assert_eq!(
        stale_after_replacement.status,
        DispatchResultStatus::RejectedStaleFencingToken
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove");
}

#[test]
fn failure_fence_exact_replay_survives_restart_and_changed_reuse_is_rejected() {
    let root = temporary_directory("failure-replay");
    let worker_scope = scope(1);
    let worker_pool = pool(1);
    let command = WorkerFleetFailureCommand {
        request_id: RequestId(id("req", 50)),
        scope: worker_scope.clone(),
        worker_pool_id: worker_pool.clone(),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
        detected_at: instant(3),
    };
    {
        let mut storage = SqliteStorage::open(&root).expect("storage");
        register_remote_worker(&mut storage, 1, 1, 51, &worker_scope, &worker_pool);
        make_worker_healthy(&mut storage, 1, 1, 52);
        storage
            .execution_registry()
            .expect("registry")
            .mark_worker_disconnected(&command.worker_id, &command.worker_instance_id)
            .expect("disconnect");
        storage
            .worker_fleet_operations()
            .expect("operations")
            .fence_disconnected_worker(&command)
            .expect("first fence");
    }
    let mut storage = SqliteStorage::open(&root).expect("restart");
    let replay = storage
        .worker_fleet_operations()
        .expect("operations")
        .fence_disconnected_worker(&command)
        .expect("replay");
    assert!(replay.replayed);
    let mut changed = command.clone();
    changed.detected_at = instant(4);
    assert_eq!(
        storage
            .worker_fleet_operations()
            .expect("operations")
            .fence_disconnected_worker(&changed)
            .expect_err("changed reuse")
            .kind(),
        StorageErrorKind::RequestConflict
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove");
}
