// SPDX-License-Identifier: Apache-2.0

//! Stable Fleet inventory snapshots over real Worker Registry facts.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkspaceId,
};
use winwincode_storage::{
    AuthenticatedWorkerPlacement, EXECUTION_PROTOCOL_VERSION, ExecutionLeaseClaim,
    LeaseWriteStatus, SqliteStorage, StorageErrorKind, WorkerAuthenticationIdentity,
    WorkerFleetInventoryState, WorkerFleetSnapshotRequest, WorkerHeartbeatRequest, WorkerPlatform,
    WorkerPoolId, WorkerRegistrationRequest, WorkerRegistryScope,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-worker-fleet-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
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

fn authentication(worker: u64) -> WorkerAuthenticationIdentity {
    WorkerAuthenticationIdentity::TransportPrincipal {
        issuer: "enterprise-fixture".to_owned(),
        subject: format!("remote-worker-{worker}"),
        credential_fingerprint: Sha256Digest(format!("sha256:{worker:064x}")),
    }
}

struct WorkerFixture {
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
}

fn register_worker(
    storage: &mut SqliteStorage,
    worker: u64,
    pool: u64,
    owner_scope: &WorkerRegistryScope,
    max_slots: u64,
    running_slots: u64,
    heartbeat_second: u64,
) -> WorkerFixture {
    let worker_id = WorkerId(id("wrk", worker));
    let worker_instance_id = WorkerInstanceId(id("wki", worker));
    let worker_pool_id = WorkerPoolId(id("wpl", pool));
    let registration_request_id = RequestId(id("req", worker));
    let authentication_identity = authentication(worker);
    let registration = WorkerRegistrationRequest {
        authentication_identity: authentication_identity.clone(),
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: if worker.is_multiple_of(2) {
            WorkerPlatform::Aarch64AppleDarwin
        } else {
            WorkerPlatform::X86_64UnknownLinuxGnu
        },
        capabilities: vec!["artifact_stream".to_owned(), "repository_read".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: format!("region-{}", pool % 3),
        max_slots,
        message_id: ExecutionMessageId(id("xmsg", worker)),
        request_id: registration_request_id.clone(),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: worker_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
    };
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker_for_scope(&registration, owner_scope)
        .expect("registration");
    registry
        .record_authenticated_worker_placement(&AuthenticatedWorkerPlacement {
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
            management_scope: owner_scope.clone(),
            authentication_identity,
            registration_request_id: registration_request_id.clone(),
            placed_at: instant(1),
        })
        .expect("authenticated placement");
    registry
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: max_slots - running_slots,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots,
            running_slots,
            message_id: ExecutionMessageId(id("xmsg", worker + 100_000)),
            observed_at: instant(heartbeat_second),
            sent_at: instant(heartbeat_second),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("heartbeat");
    WorkerFixture {
        worker_id,
        worker_instance_id,
    }
}

fn claim(storage: &mut SqliteStorage, worker: &WorkerFixture, seed: u64) {
    let receipt = storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job_with_authenticated_placement(&ExecutionLeaseClaim {
            expires_at: instant(40),
            fencing_token: FencingToken(seed.to_string()),
            issued_at: instant(3),
            job_id: ExecutionJobId(id("job", seed)),
            lease_id: LeaseId(id("lse", seed)),
            message_id: ExecutionMessageId(id("xmsg", seed + 200_000)),
            payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            request_id: RequestId(id("req", seed + 200_000)),
            worker_id: worker.worker_id.clone(),
            worker_instance_id: worker.worker_instance_id.clone(),
            attempt: 1,
        })
        .expect("lease claim");
    assert_eq!(receipt.status, LeaseWriteStatus::Accepted);
}

fn request(
    owner_scope: &WorkerRegistryScope,
    observed_second: u64,
    limit: usize,
) -> WorkerFleetSnapshotRequest {
    WorkerFleetSnapshotRequest {
        scope: owner_scope.clone(),
        states: Vec::new(),
        observed_at: instant(observed_second),
        stale_after_ms: 5_000,
        limit,
        cursor: None,
    }
}

#[test]
fn capacity_health_and_labels_reconcile_from_registry_facts() {
    let root = temporary_directory("capacity");
    let owner_scope = scope(1);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let first = register_worker(&mut storage, 1, 1, &owner_scope, 4, 1, 2);
    register_worker(&mut storage, 2, 1, &owner_scope, 8, 2, 2);
    claim(&mut storage, &first, 1);

    let page = storage
        .worker_fleet_inventory()
        .expect("Fleet inventory")
        .page(&request(&owner_scope, 4, 10))
        .expect("Fleet page");
    assert_eq!(page.items.len(), 1);
    let pool = &page.items[0];
    assert_eq!(pool.worker_pool_id, WorkerPoolId(id("wpl", 1)));
    assert_eq!(pool.state, WorkerFleetInventoryState::Healthy);
    assert_eq!(pool.registered_workers, 2);
    assert_eq!(pool.usable_workers, 2);
    assert_eq!(pool.active_leases, 1);
    assert_eq!(pool.max_capacity, 12);
    assert_eq!(pool.running_capacity, 3);
    assert_eq!(pool.reported_available_capacity, 9);
    assert_eq!(pool.available_capacity, 9);
    assert_eq!(
        pool.max_capacity,
        pool.running_capacity + pool.reported_available_capacity
    );
    assert!(
        pool.labels
            .contains(&"platform:aarch64-apple-darwin".to_owned())
    );
    assert!(
        pool.labels
            .contains(&"platform:x86_64-unknown-linux-gnu".to_owned())
    );
    assert!(pool.labels.contains(&"protocol:winwincode/v1".to_owned()));
    assert!(
        pool.labels
            .contains(&"capability:repository_read".to_owned())
    );
    assert!(pool.labels.contains(&"network-zone:region-1".to_owned()));

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn stale_heartbeat_is_offline_and_contributes_no_available_capacity() {
    let root = temporary_directory("stale");
    let owner_scope = scope(2);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    register_worker(&mut storage, 10, 10, &owner_scope, 8, 0, 2);

    let page = storage
        .worker_fleet_inventory()
        .expect("Fleet inventory")
        .page(&request(&owner_scope, 20, 10))
        .expect("Fleet page");
    let pool = &page.items[0];
    assert_eq!(pool.state, WorkerFleetInventoryState::Offline);
    assert_eq!(pool.registered_workers, 1);
    assert_eq!(pool.usable_workers, 0);
    assert_eq!(pool.stale_workers, 1);
    assert_eq!(pool.reported_available_capacity, 8);
    assert_eq!(pool.available_capacity, 0);

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn snapshot_cursor_is_scope_bound_and_restart_stable() {
    let root = temporary_directory("scope-restart");
    let owner_scope = scope(3);
    let foreign_scope = scope(4);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    for pool in 1..=3 {
        register_worker(&mut storage, pool, pool, &owner_scope, 4, 0, 2);
    }
    register_worker(&mut storage, 100, 100, &foreign_scope, 4, 0, 2);
    let first = storage
        .worker_fleet_inventory()
        .expect("Fleet inventory")
        .page(&request(&owner_scope, 4, 2))
        .expect("first page");
    assert_eq!(first.items.len(), 2);
    let cursor = first.next_cursor.expect("next cursor");
    drop(storage);

    let mut reopened = SqliteStorage::open(&root).expect("storage reopen");
    let mut continuation = request(&owner_scope, 30, 2);
    continuation.cursor = Some(cursor.clone());
    let second = reopened
        .worker_fleet_inventory()
        .expect("Fleet inventory reopen")
        .page(&continuation)
        .expect("second page");
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.snapshot_revision, first.snapshot_revision);
    assert_eq!(second.items[0].state, WorkerFleetInventoryState::Healthy);
    assert_eq!(second.next_cursor, None);

    let mut foreign = request(&foreign_scope, 4, 2);
    foreign.cursor = Some(cursor);
    let error = reopened
        .worker_fleet_inventory()
        .expect("foreign Fleet inventory")
        .page(&foreign)
        .expect_err("foreign cursor rejected");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);

    drop(reopened);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn large_inventory_pages_are_bounded_and_late_workers_do_not_enter_snapshot() {
    let root = temporary_directory("large-page");
    let owner_scope = scope(5);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    for pool in 1..=205 {
        register_worker(&mut storage, pool, pool, &owner_scope, 4, 0, 2);
    }
    let first = storage
        .worker_fleet_inventory()
        .expect("Fleet inventory")
        .page(&request(&owner_scope, 4, 100))
        .expect("first page");
    assert_eq!(first.items.len(), 100);
    let mut cursor = first.next_cursor.expect("first cursor");

    register_worker(&mut storage, 999, 999, &owner_scope, 4, 0, 2);
    let mut seen = first.items.len();
    loop {
        let mut continuation = request(&owner_scope, 30, 100);
        continuation.cursor = Some(cursor);
        let page = storage
            .worker_fleet_inventory()
            .expect("Fleet inventory continuation")
            .page(&continuation)
            .expect("continued page");
        assert!(page.items.len() <= 100);
        assert_eq!(page.snapshot_revision, first.snapshot_revision);
        assert!(page.items.iter().all(|pool| {
            pool.worker_pool_id != WorkerPoolId(id("wpl", 999))
                && pool.state == WorkerFleetInventoryState::Healthy
        }));
        seen += page.items.len();
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = next;
    }
    assert_eq!(seen, 205);

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
