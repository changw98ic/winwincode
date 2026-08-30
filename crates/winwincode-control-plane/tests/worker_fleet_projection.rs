// SPDX-License-Identifier: Apache-2.0

//! Generated Fleet query coverage over authoritative Registry facts.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_api::generated::{
    Actor, EnterpriseFleetListParameters, EnterpriseFleetListQuery, EnterpriseFleetListQueryQuery,
    PageRequest, RepositoryScope, RepositoryScopeKind, Scope, SystemActor, SystemActorKind,
};
use winwincode_control_plane::{
    WorkerFleetProjectionService, WorkerFleetProjectionServiceErrorKind,
};
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    OpaqueCursor, OrganizationId, ProjectId, RepositoryId, RequestId, Revision, SchemaVersion,
    Sha256Digest, SystemActorId, WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_storage::{
    AuthenticatedWorkerPlacement, EXECUTION_PROTOCOL_VERSION, ExecutionLeaseClaim, SqliteStorage,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerRegistryScope,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-control-plane-fleet-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn actor() -> Actor {
    Actor::SystemActor(SystemActor {
        id: SystemActorId(id("sys", 1)),
        kind: SystemActorKind::System,
    })
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn registry_scope(scope: &RepositoryScope) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

struct WorkerFixture {
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
}

fn register_worker(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    worker: u64,
    pool: u64,
    max_slots: u64,
    running_slots: u64,
    heartbeat_second: u64,
) -> WorkerFixture {
    let worker_id = WorkerId(id("wrk", worker));
    let worker_instance_id = WorkerInstanceId(id("wki", worker));
    let authentication_identity = WorkerAuthenticationIdentity::TransportPrincipal {
        issuer: "fleet-fixture".to_owned(),
        subject: format!("worker-{worker}"),
        credential_fingerprint: Sha256Digest(format!("sha256:{worker:064x}")),
    };
    let registration_request_id = RequestId(id("req", worker));
    let registration = WorkerRegistrationRequest {
        authentication_identity: authentication_identity.clone(),
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        capabilities: vec!["artifact_stream".to_owned(), "repository_read".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: format!("zone-{}", pool % 3),
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
        .register_worker_for_scope(&registration, &registry_scope(scope))
        .expect("registration");
    registry
        .record_authenticated_worker_placement(&AuthenticatedWorkerPlacement {
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
            worker_pool_id: WorkerPoolId(id("wpl", pool)),
            management_scope: registry_scope(scope),
            authentication_identity,
            registration_request_id,
            placed_at: instant(1),
        })
        .expect("placement");
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
    storage
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
}

fn query(
    scope: &RepositoryScope,
    request: u64,
    limit: i64,
    states: &[&str],
    cursor: Option<OpaqueCursor>,
) -> EnterpriseFleetListQuery {
    EnterpriseFleetListQuery {
        actor: actor(),
        page: PageRequest { cursor, limit },
        parameters: EnterpriseFleetListParameters {
            states: states.iter().map(|state| (*state).to_owned()).collect(),
        },
        query: EnterpriseFleetListQueryQuery::EnterpriseFleetList,
        request_id: RequestId(id("req", request)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope.clone()),
    }
}

#[test]
fn generated_projection_reconciles_capacity_health_leases_and_labels() {
    let root = temporary_directory("projection");
    let scope = repository_scope(1);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let first = register_worker(&mut storage, &scope, 1, 1, 4, 1, 2);
    register_worker(&mut storage, &scope, 2, 1, 8, 2, 2);
    claim(&mut storage, &first, 1);

    let response = WorkerFleetProjectionService::with_stale_after_ms(&mut storage, 5_000)
        .list(&query(&scope, 1, 10, &[], None), &instant(4))
        .expect("Fleet response");
    assert_eq!(response.result.snapshot_revision, Revision(1));
    assert_eq!(response.result.items.len(), 1);
    assert!(!response.page.has_more);
    let pool = &response.result.items[0];
    assert_eq!(pool.id.0, id("wpl", 1));
    assert_eq!(pool.state, "healthy");
    assert_eq!(pool.registered_workers, 2);
    assert_eq!(pool.active_leases, 1);
    assert_eq!(pool.available_capacity, 9);
    assert!(pool.labels.contains(&"protocol:winwincode/v1".to_owned()));
    assert!(
        pool.labels
            .contains(&"capability:repository_read".to_owned())
    );
    assert_eq!(
        serde_json::from_value::<winwincode_api::generated::QueryResultResponse>(
            serde_json::to_value(&response).expect("response JSON"),
        )
        .expect("generated response validation"),
        winwincode_api::generated::QueryResultResponse::EnterpriseFleetListResultResponse(response)
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn cursor_is_scope_and_filter_bound_and_continues_the_fixed_snapshot_after_restart() {
    let root = temporary_directory("cursor");
    let owner = repository_scope(2);
    let foreign = repository_scope(3);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    for pool in 1..=3 {
        register_worker(&mut storage, &owner, pool, pool, 4, 0, 2);
    }
    register_worker(&mut storage, &foreign, 100, 100, 4, 0, 2);
    let first = WorkerFleetProjectionService::with_stale_after_ms(&mut storage, 5_000)
        .list(&query(&owner, 10, 2, &[], None), &instant(4))
        .expect("first page");
    assert_eq!(first.result.items.len(), 2);
    let cursor = first.page.next_cursor.clone().expect("next cursor");
    let snapshot_revision = first.result.snapshot_revision;
    let foreign_first = WorkerFleetProjectionService::with_stale_after_ms(&mut storage, 5_000)
        .list(&query(&foreign, 14, 2, &[], None), &instant(4))
        .expect("foreign first page");
    assert_eq!(snapshot_revision, Revision(1));
    assert_eq!(foreign_first.result.snapshot_revision, Revision(1));
    assert_eq!(foreign_first.result.items.len(), 1);
    assert_eq!(foreign_first.result.items[0].id.0, id("wpl", 100));
    drop(storage);

    let mut reopened = SqliteStorage::open(&root).expect("storage reopen");
    register_worker(&mut reopened, &owner, 999, 999, 4, 0, 20);
    let continued = WorkerFleetProjectionService::with_stale_after_ms(&mut reopened, 5_000)
        .list(
            &query(&owner, 11, 2, &[], Some(cursor.clone())),
            &instant(30),
        )
        .expect("continued page");
    assert_eq!(continued.result.snapshot_revision, snapshot_revision);
    assert_eq!(continued.result.items.len(), 1);
    assert_eq!(continued.result.items[0].state, "healthy");
    assert_eq!(continued.result.items[0].id.0, id("wpl", 3));
    assert!(!continued.page.has_more);

    for invalid_query in [
        query(&foreign, 12, 2, &[], Some(cursor.clone())),
        query(&owner, 13, 2, &["healthy"], Some(cursor)),
    ] {
        let error = WorkerFleetProjectionService::with_stale_after_ms(&mut reopened, 5_000)
            .list(&invalid_query, &instant(30))
            .expect_err("foreign cursor rejected");
        assert_eq!(
            error.kind(),
            WorkerFleetProjectionServiceErrorKind::InvalidRequest
        );
    }

    drop(reopened);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn stale_heartbeat_is_only_returned_by_the_offline_filter_with_zero_capacity() {
    let root = temporary_directory("stale-filter");
    let scope = repository_scope(4);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    register_worker(&mut storage, &scope, 10, 10, 8, 0, 2);

    let offline = WorkerFleetProjectionService::with_stale_after_ms(&mut storage, 5_000)
        .list(&query(&scope, 20, 10, &["offline"], None), &instant(20))
        .expect("offline page");
    assert_eq!(offline.result.items.len(), 1);
    assert_eq!(offline.result.items[0].state, "offline");
    assert_eq!(offline.result.items[0].available_capacity, 0);

    let healthy = WorkerFleetProjectionService::with_stale_after_ms(&mut storage, 5_000)
        .list(&query(&scope, 21, 10, &["healthy"], None), &instant(20))
        .expect("healthy page");
    assert!(healthy.result.items.is_empty());
    let duplicate = query(&scope, 22, 10, &["offline", "offline"], None);
    let error = WorkerFleetProjectionService::new(&mut storage)
        .list(&duplicate, &instant(20))
        .expect_err("duplicate filter rejected");
    assert_eq!(
        error.kind(),
        WorkerFleetProjectionServiceErrorKind::InvalidRequest
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn evicted_snapshot_is_reported_as_an_expired_cursor() {
    let root = temporary_directory("cursor-expired");
    let scope = repository_scope(5);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    register_worker(&mut storage, &scope, 1, 1, 4, 0, 2);
    register_worker(&mut storage, &scope, 2, 2, 4, 0, 2);
    let first = WorkerFleetProjectionService::with_stale_after_ms(&mut storage, 5_000)
        .list(&query(&scope, 30, 1, &[], None), &instant(4))
        .expect("first page");
    let cursor = first.page.next_cursor.expect("retained cursor");

    for request in 31..=63 {
        WorkerFleetProjectionService::with_stale_after_ms(&mut storage, 5_000)
            .list(&query(&scope, request, 2, &[], None), &instant(4))
            .expect("replacement snapshot");
    }
    let error = WorkerFleetProjectionService::with_stale_after_ms(&mut storage, 5_000)
        .list(&query(&scope, 64, 1, &[], Some(cursor)), &instant(4))
        .expect_err("expired cursor rejected");
    assert_eq!(
        error.kind(),
        WorkerFleetProjectionServiceErrorKind::CursorExpired
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
