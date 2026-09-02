// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::drop_non_drop)]

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

use winwincode_domain::{
    ExecutionMessageId, ExecutionSequence, Instant, OrganizationId, ProjectId, RepositoryId,
    RequestId, Sha256Digest, WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, LeaseWriteStatus, SqliteStorage, WorkerAuthenticationIdentity,
    WorkerHealth, WorkerHeartbeatRequest, WorkerOperationalState, WorkerPlatform,
    WorkerRegistrationErrorCode, WorkerRegistrationRequest, WorkerRegistrationStatus,
    WorkerRegistryScope,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-worker-registry-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn registration(
    worker: u64,
    instance: u64,
    request: u64,
    max_slots: u64,
) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::TransportPrincipal {
            issuer: "fixture-issuer".into(),
            subject: format!("worker-{worker}"),
            credential_fingerprint: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        capabilities: vec!["artifact_stream".into(), "shell".into()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "build-local".into(),
        max_slots,
        message_id: ExecutionMessageId(id("xmsg", request)),
        request_id: RequestId(id("req", request)),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn heartbeat(
    worker: u64,
    instance: u64,
    message: u64,
    running_slots: u64,
    max_slots: u64,
) -> WorkerHeartbeatRequest {
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: max_slots - running_slots,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots,
        running_slots,
        message_id: ExecutionMessageId(id("xmsg", message)),
        observed_at: instant(2),
        sent_at: instant(2),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn repository_scope(seed: u64) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

#[test]
fn local_and_explicit_registration_paths_do_not_mix_worker_scope() {
    let root = temporary_directory("scope-boundary");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry");

    let local_request = registration(90, 1, 90, 4);
    let local = registry
        .register_worker(&local_request)
        .expect("local registration");
    assert_eq!(
        local.worker.management_scope,
        WorkerRegistryScope::local_default()
    );

    let scoped_request = registration(91, 1, 91, 4);
    let scope = repository_scope(1);
    let scoped = registry
        .register_worker_for_scope(&scoped_request, &scope)
        .expect("scoped registration");
    assert_eq!(scoped.worker.management_scope, scope);

    let foreign_scope_request = registration(90, 1, 92, 4);
    let rejected = registry
        .register_worker_for_scope(&foreign_scope_request, &repository_scope(2))
        .expect("foreign scope registration decision");
    assert_eq!(rejected.status, WorkerRegistrationStatus::RejectedConflict);
    assert_eq!(
        rejected.error,
        Some(WorkerRegistrationErrorCode::ScopeMismatch)
    );
    assert_eq!(
        registry
            .load_worker(&local_request.worker_id)
            .expect("local Worker lookup")
            .expect("local Worker")
            .management_scope,
        WorkerRegistryScope::local_default()
    );

    drop(registry);
    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn scoped_worker_pages_are_isolated_stable_and_restart_safe() {
    let root = temporary_directory("scoped-page");
    let scope = repository_scope(3);
    let foreign_scope = repository_scope(4);
    let observed_at = instant(3);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry");
    for worker in [100, 102] {
        registry
            .register_worker_for_scope(&registration(worker, 1, worker, 4), &scope)
            .expect("scoped registration");
    }
    registry
        .register_worker_for_scope(&registration(101, 1, 101, 8), &foreign_scope)
        .expect("foreign registration");
    registry
        .record_heartbeat(&heartbeat(100, 1, 103, 1, 4))
        .expect("heartbeat");

    let first = registry
        .list_managed_workers(&scope, &[], None, 1, &observed_at)
        .expect("first page");
    assert_eq!(first.workers.len(), 1);
    assert_eq!(first.workers[0].worker_id, WorkerId(id("wrk", 100)));
    assert_eq!(first.workers[0].revision, 0);
    assert_eq!(first.workers[0].capacity, 4);
    assert_eq!(first.workers[0].last_heartbeat_at, Some(instant(2)));
    assert_eq!(
        first.workers[0].operational_state,
        WorkerOperationalState::Enabled
    );
    let cursor = first.next_cursor.expect("next cursor");
    assert_eq!(cursor.upper_bound_worker_id, WorkerId(id("wrk", 102)));

    registry
        .register_worker_for_scope(&registration(104, 1, 104, 4), &scope)
        .expect("late registration");
    drop(registry);
    drop(storage);

    let mut reopened = SqliteStorage::open(&root).expect("storage reopen");
    let registry = reopened.execution_registry().expect("registry reopen");
    let second = registry
        .list_managed_workers(&scope, &[], Some(&cursor), 1, &observed_at)
        .expect("second page");
    assert_eq!(
        second
            .workers
            .iter()
            .map(|worker| worker.worker_id.clone())
            .collect::<Vec<_>>(),
        vec![WorkerId(id("wrk", 102))]
    );
    assert_eq!(second.next_cursor, None);
    assert!(
        registry
            .load_managed_worker(&scope, &WorkerId(id("wrk", 101)), &observed_at)
            .expect("foreign Worker lookup")
            .is_none()
    );
    assert_eq!(
        registry
            .list_managed_workers(&foreign_scope, &[], None, 10, &observed_at)
            .expect("foreign page")
            .workers
            .len(),
        1
    );

    drop(registry);
    drop(reopened);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn registration_replays_profile_and_reports_version_capability_and_authentication_mismatch() {
    let root = temporary_directory("registration");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry");
    let request = registration(1, 1, 1, 4);

    let first = registry.register_worker(&request).expect("registration");
    let replay = registry
        .register_worker(&request)
        .expect("registration replay");
    assert_eq!(first.status, WorkerRegistrationStatus::Accepted);
    assert_eq!(first.error, None);
    assert_eq!(replay.status, WorkerRegistrationStatus::Duplicate);
    assert_eq!(replay.worker, first.worker);
    assert_eq!(first.worker.health, WorkerHealth::Registered);
    assert_eq!(first.worker.max_slots, 4);
    assert_eq!(first.worker.running_slots, 0);
    assert_eq!(first.worker.available_slots, 4);

    let mut unsupported = registration(2, 2, 2, 4);
    unsupported.protocol_version = "winwincode/v2".into();
    let unsupported = registry
        .register_worker(&unsupported)
        .expect("version decision");
    assert_eq!(
        unsupported.status,
        WorkerRegistrationStatus::RejectedConflict
    );
    assert_eq!(
        unsupported.error,
        Some(WorkerRegistrationErrorCode::ProtocolVersionUnsupported)
    );
    assert_eq!(
        registry
            .load_worker(&WorkerId(id("wrk", 2)))
            .expect("rejected Worker lookup"),
        None
    );

    let mut changed_capability = registration(1, 1, 3, 4);
    changed_capability.capabilities.push("git".into());
    let changed_capability = registry
        .register_worker(&changed_capability)
        .expect("capability decision");
    assert_eq!(
        changed_capability.error,
        Some(WorkerRegistrationErrorCode::CapabilityMismatch)
    );

    let mut changed_authentication = registration(1, 1, 4, 4);
    changed_authentication.authentication_identity = WorkerAuthenticationIdentity::LocalEmbedded {
        control_plane_principal: "another-principal".into(),
    };
    let changed_authentication = registry
        .register_worker(&changed_authentication)
        .expect("authentication decision");
    assert_eq!(
        changed_authentication.error,
        Some(WorkerRegistrationErrorCode::AuthenticationMismatch)
    );

    drop(registry);
    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn replaced_instance_cannot_heartbeat_and_timeout_changes_persisted_health() {
    let root = temporary_directory("health");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker(&registration(10, 1, 10, 4))
        .expect("first registration");
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(10, 1, 11, 1, 4))
            .expect("first heartbeat")
            .status,
        LeaseWriteStatus::Accepted
    );

    let fresh = registry
        .refresh_worker_capacity_snapshot(&instant(3), &instant(2))
        .expect("fresh capacity snapshot");
    assert_eq!(fresh.workers[0].health, WorkerHealth::Healthy);
    assert_eq!(
        fresh.workers[0].protocol_version,
        EXECUTION_PROTOCOL_VERSION
    );
    assert_eq!(
        fresh.workers[0].platform,
        WorkerPlatform::X86_64UnknownLinuxGnu
    );
    assert_eq!(
        fresh.workers[0].capabilities,
        vec!["artifact_stream".to_owned(), "shell".to_owned()]
    );
    assert_eq!(fresh.healthy_running_slots, 1);

    registry
        .register_worker(&registration(10, 2, 12, 4))
        .expect("replacement registration");
    let stale = registry
        .record_heartbeat(&heartbeat(10, 1, 13, 2, 4))
        .expect("stale heartbeat decision");
    assert_eq!(stale.status, LeaseWriteStatus::RejectedWorkerInstance);

    let timed_out = registry
        .refresh_worker_capacity_snapshot(&instant(10), &instant(3))
        .expect("timed-out capacity snapshot");
    assert_eq!(timed_out.workers[0].health, WorkerHealth::TimedOut);
    assert_eq!(timed_out.healthy_worker_count(), 0);
    assert_eq!(timed_out.healthy_max_slots, 0);
    assert_eq!(
        registry
            .load_worker(&WorkerId(id("wrk", 10)))
            .expect("Worker read")
            .expect("Worker")
            .health,
        WorkerHealth::TimedOut,
        "snapshot must persist the timeout transition"
    );

    drop(registry);
    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn concurrent_heartbeats_produce_one_exact_capacity_snapshot() {
    let root = temporary_directory("concurrent-capacity");
    {
        let mut storage = SqliteStorage::open(&root).expect("bootstrap storage");
        let mut registry = storage.execution_registry().expect("bootstrap registry");
        for worker in 20..24 {
            registry
                .register_worker(&registration(worker, worker, worker, 4))
                .expect("bootstrap registration");
        }
    }

    let handles: Vec<_> = (20..24)
        .enumerate()
        .map(|(running_slots, worker)| {
            let root = root.clone();
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(&root).expect("thread storage");
                let mut registry = storage.execution_registry().expect("thread registry");
                registry
                    .record_heartbeat(&heartbeat(
                        worker,
                        worker,
                        worker + 100,
                        u64::try_from(running_slots).expect("bounded running slots"),
                        4,
                    ))
                    .expect("concurrent heartbeat")
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(
            handle.join().expect("heartbeat thread").status,
            LeaseWriteStatus::Accepted
        );
    }

    let mut storage = SqliteStorage::open(&root).expect("snapshot storage");
    let mut registry = storage.execution_registry().expect("snapshot registry");
    let snapshot = registry
        .refresh_worker_capacity_snapshot(&instant(5), &instant(1))
        .expect("capacity snapshot");
    assert_eq!(snapshot.healthy_worker_count(), 4);
    assert_eq!(snapshot.healthy_max_slots, 16);
    assert_eq!(snapshot.healthy_running_slots, 6);
    assert_eq!(snapshot.healthy_available_slots, 10);
    assert_eq!(snapshot.workers.len(), 4);

    drop(registry);
    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
