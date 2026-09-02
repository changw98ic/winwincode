// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, WorkerId,
    WorkerInstanceId, WorkspaceId,
};
use winwincode_storage::{
    AuthenticatedWorkerPlacement, EXECUTION_PROTOCOL_VERSION, EnterpriseWorkerLeaseClaim,
    EnterpriseWorkerPlacementCandidate, EnterpriseWorkerPlacementDecision,
    EnterpriseWorkerPlacementRequest, EnterpriseWorkerPoolProfile, EnterpriseWorkerSecurityTier,
    LeaseWriteStatus, SqliteStorage, WorkerAuthenticationIdentity, WorkerCapacityEntry,
    WorkerHealth, WorkerHeartbeatRequest, WorkerPlacementFailure, WorkerPlacementQuota,
    WorkerPlacementRequest, WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest,
    WorkerRegistryScope, WorkerRepositoryAccess, WorkerSessionAffinity,
    claim_enterprise_worker_selection, place_enterprise_worker_batch,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-placement-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-02-01T08:00:{second:02}.000Z"))
}

fn scope(seed: u64) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn access(owner_scope: &WorkerRegistryScope) -> WorkerRepositoryAccess {
    let WorkerRegistryScope::Repository {
        organization_id,
        workspace_id,
        project_id,
        repository_id,
    } = owner_scope
    else {
        panic!("repository fixture scope")
    };
    WorkerRepositoryAccess {
        organization_id: organization_id.clone(),
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        repository_id: repository_id.clone(),
    }
}

fn worker(worker: u64, available_slots: u64) -> WorkerCapacityEntry {
    WorkerCapacityEntry {
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", worker)),
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        capabilities: vec!["artifact_stream".to_owned(), "shell".to_owned()],
        security_zone: "trusted-build".to_owned(),
        health: WorkerHealth::Healthy,
        max_slots: 4,
        running_slots: 4 - available_slots,
        available_slots,
    }
}

fn authenticated_placement(
    worker: &WorkerCapacityEntry,
    pool: u64,
    owner_scope: &WorkerRegistryScope,
) -> AuthenticatedWorkerPlacement {
    AuthenticatedWorkerPlacement {
        worker_id: worker.worker_id.clone(),
        worker_instance_id: worker.worker_instance_id.clone(),
        worker_pool_id: WorkerPoolId(id("wpl", pool)),
        management_scope: owner_scope.clone(),
        authentication_identity: WorkerAuthenticationIdentity::TransportPrincipal {
            issuer: "enterprise-fixture".to_owned(),
            subject: format!("worker-{pool}"),
            credential_fingerprint: Sha256Digest(format!("sha256:{pool:064x}")),
        },
        registration_request_id: RequestId(id("req", 10_000 + pool)),
        placed_at: instant(1),
    }
}

fn profile(
    pool: u64,
    owner_scope: &WorkerRegistryScope,
    region: &str,
    tier: EnterpriseWorkerSecurityTier,
) -> EnterpriseWorkerPoolProfile {
    EnterpriseWorkerPoolProfile {
        scope: owner_scope.clone(),
        worker_pool_id: WorkerPoolId(id("wpl", pool)),
        region: region.to_owned(),
        security_tier: tier,
        network_zones: vec!["build-private".to_owned()],
        plugins: vec!["docker".to_owned(), "git".to_owned()],
        repository_capabilities: vec!["repository-read".to_owned()],
        repository_access: vec![access(owner_scope)],
    }
}

fn candidate(
    worker_seed: u64,
    available_slots: u64,
    pool: u64,
    owner_scope: &WorkerRegistryScope,
) -> EnterpriseWorkerPlacementCandidate {
    let worker = worker(worker_seed, available_slots);
    EnterpriseWorkerPlacementCandidate::from_authority(
        worker.clone(),
        authenticated_placement(&worker, pool, owner_scope),
        profile(
            pool,
            owner_scope,
            "us-east-1",
            EnterpriseWorkerSecurityTier::Restricted,
        ),
    )
    .expect("enterprise candidate")
}

fn placement_request(job: u64, owner_scope: &WorkerRegistryScope) -> WorkerPlacementRequest {
    let target = access(owner_scope);
    WorkerPlacementRequest {
        job_id: ExecutionJobId(id("job", job)),
        organization_id: target.organization_id,
        workspace_id: target.workspace_id,
        project_id: target.project_id,
        repository_id: target.repository_id,
        product_session_id: ProductSessionId(id("psn", 1)),
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        required_capabilities: vec!["artifact_stream".to_owned(), "shell".to_owned()],
        network_zone: "build-private".to_owned(),
        security_zone: "trusted-build".to_owned(),
        affinity: None,
    }
}

fn request(job: u64, owner_scope: &WorkerRegistryScope) -> EnterpriseWorkerPlacementRequest {
    EnterpriseWorkerPlacementRequest {
        placement: placement_request(job, owner_scope),
        allowed_worker_pools: vec![WorkerPoolId(id("wpl", 1))],
        allowed_regions: vec!["us-east-1".to_owned()],
        minimum_security_tier: EnterpriseWorkerSecurityTier::Restricted,
        required_plugins: vec!["docker".to_owned()],
        required_repository_capabilities: vec!["repository-read".to_owned()],
    }
}

fn quota(request: &EnterpriseWorkerPlacementRequest, limit: u64) -> WorkerPlacementQuota {
    WorkerPlacementQuota {
        organization_id: request.placement.organization_id.clone(),
        workspace_id: request.placement.workspace_id.clone(),
        project_id: request.placement.project_id.clone(),
        repository_id: request.placement.repository_id.clone(),
        product_session_id: request.placement.product_session_id.clone(),
        max_running_slots: limit,
        running_slots: 0,
    }
}

fn invalid_enterprise_candidates(
    owner_scope: &WorkerRegistryScope,
) -> Vec<EnterpriseWorkerPlacementCandidate> {
    let wrong_pool = candidate(1, 1, 2, owner_scope);
    let wrong_region = enterprise_candidate(
        2,
        owner_scope,
        profile(
            1,
            owner_scope,
            "eu-west-1",
            EnterpriseWorkerSecurityTier::Restricted,
        ),
    );
    let low_tier = enterprise_candidate(
        3,
        owner_scope,
        profile(
            1,
            owner_scope,
            "us-east-1",
            EnterpriseWorkerSecurityTier::Standard,
        ),
    );
    let mut missing_plugin_profile = profile(
        1,
        owner_scope,
        "us-east-1",
        EnterpriseWorkerSecurityTier::Restricted,
    );
    missing_plugin_profile.plugins = vec!["git".to_owned()];
    let missing_plugin = enterprise_candidate(4, owner_scope, missing_plugin_profile);
    let mut missing_repository_profile = profile(
        1,
        owner_scope,
        "us-east-1",
        EnterpriseWorkerSecurityTier::Restricted,
    );
    missing_repository_profile.repository_capabilities = vec!["repository-status".to_owned()];
    let missing_repository = enterprise_candidate(5, owner_scope, missing_repository_profile);
    vec![
        missing_repository,
        missing_plugin,
        low_tier,
        wrong_region,
        wrong_pool,
    ]
}

fn enterprise_candidate(
    worker_seed: u64,
    owner_scope: &WorkerRegistryScope,
    profile: EnterpriseWorkerPoolProfile,
) -> EnterpriseWorkerPlacementCandidate {
    let worker = worker(worker_seed, 1);
    EnterpriseWorkerPlacementCandidate::from_authority(
        worker.clone(),
        authenticated_placement(&worker, 1, owner_scope),
        profile,
    )
    .expect("enterprise candidate")
}

#[test]
fn every_enterprise_constraint_queues_with_a_stable_explanation() {
    let owner_scope = scope(1);
    let request = request(1, &owner_scope);

    let decisions = place_enterprise_worker_batch(
        std::slice::from_ref(&request),
        &invalid_enterprise_candidates(&owner_scope),
        &[quota(&request, 10)],
    )
    .expect("placement decision");
    let EnterpriseWorkerPlacementDecision::Queued(rejection) = &decisions[0] else {
        panic!("all candidates must remain queued")
    };
    assert_eq!(rejection.workers.len(), 5);
    assert_eq!(
        rejection.workers[0].failures,
        vec![WorkerPlacementFailure::WorkerPoolNotAllowed(WorkerPoolId(
            id("wpl", 2)
        ))]
    );
    assert_eq!(
        rejection.workers[1].failures,
        vec![WorkerPlacementFailure::RegionNotAllowed(
            "eu-west-1".to_owned()
        )]
    );
    assert_eq!(
        rejection.workers[2].failures,
        vec![WorkerPlacementFailure::SecurityTierInsufficient {
            required: "restricted".to_owned(),
            actual: "standard".to_owned(),
        }]
    );
    assert_eq!(
        rejection.workers[3].failures,
        vec![WorkerPlacementFailure::MissingPlugin("docker".to_owned())]
    );
    assert_eq!(
        rejection.workers[4].failures,
        vec![WorkerPlacementFailure::MissingRepositoryCapability(
            "repository-read".to_owned()
        )]
    );
}

#[test]
fn deterministic_ranking_affinity_and_capacity_are_shared_with_generic_placement() {
    let owner_scope = scope(2);
    let first = request(1, &owner_scope);
    let second = request(2, &owner_scope);
    let workers = [
        candidate(1, 1, 1, &owner_scope),
        candidate(2, 2, 1, &owner_scope),
    ];
    let forward = place_enterprise_worker_batch(
        &[second.clone(), first.clone()],
        &workers,
        &[quota(&first, 10)],
    )
    .expect("forward placement");
    let reverse = place_enterprise_worker_batch(
        &[first.clone(), second.clone()],
        &[workers[1].clone(), workers[0].clone()],
        &[quota(&first, 10)],
    )
    .expect("reverse placement");
    assert_eq!(forward, reverse);
    let EnterpriseWorkerPlacementDecision::Placed(first_selection) = &forward[0] else {
        panic!("first Job selected")
    };
    assert_eq!(
        first_selection.placement().worker_id,
        WorkerId(id("wrk", 2))
    );
    let EnterpriseWorkerPlacementDecision::Placed(second_selection) = &forward[1] else {
        panic!("second Job selected")
    };
    assert_eq!(
        second_selection.placement().worker_id,
        WorkerId(id("wrk", 1))
    );

    let mut sticky = request(3, &owner_scope);
    sticky.placement.affinity = Some(WorkerSessionAffinity {
        organization_id: sticky.placement.organization_id.clone(),
        workspace_id: sticky.placement.workspace_id.clone(),
        project_id: sticky.placement.project_id.clone(),
        repository_id: sticky.placement.repository_id.clone(),
        product_session_id: sticky.placement.product_session_id.clone(),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    });
    let decisions = place_enterprise_worker_batch(
        std::slice::from_ref(&sticky),
        &workers,
        &[quota(&sticky, 10)],
    )
    .expect("affinity placement");
    let EnterpriseWorkerPlacementDecision::Placed(selection) = &decisions[0] else {
        panic!("affinity selected")
    };
    assert_eq!(selection.placement().worker_id, WorkerId(id("wrk", 1)));
    assert!(selection.placement().reused_affinity);
    assert_eq!(selection.placement().affinity_failure, None);
}

#[test]
fn capacity_exhaustion_and_cross_tenant_access_never_place() {
    let owner_scope = scope(3);
    let first = request(1, &owner_scope);
    let second = request(2, &owner_scope);
    let only_worker = candidate(1, 1, 1, &owner_scope);
    let decisions = place_enterprise_worker_batch(
        &[second.clone(), first.clone()],
        std::slice::from_ref(&only_worker),
        &[quota(&first, 10)],
    )
    .expect("capacity placement");
    assert!(matches!(
        decisions[0],
        EnterpriseWorkerPlacementDecision::Placed(_)
    ));
    let EnterpriseWorkerPlacementDecision::Queued(capacity) = &decisions[1] else {
        panic!("second Job queued")
    };
    assert_eq!(
        capacity.workers[0].failures,
        vec![WorkerPlacementFailure::NoAvailableCapacity]
    );

    let foreign_scope = scope(4);
    let mut foreign_request = request(3, &foreign_scope);
    foreign_request.placement.repository_id = access(&owner_scope).repository_id;
    let decisions = place_enterprise_worker_batch(
        std::slice::from_ref(&foreign_request),
        &[only_worker],
        &[quota(&foreign_request, 10)],
    )
    .expect("foreign placement");
    let EnterpriseWorkerPlacementDecision::Queued(foreign) = &decisions[0] else {
        panic!("foreign tenant queued")
    };
    assert!(foreign.workers[0].failures.contains(
        &WorkerPlacementFailure::WorkspaceTenantMismatch {
            organization_id: foreign_request.placement.organization_id,
            workspace_id: foreign_request.placement.workspace_id,
        }
    ));

    let worker = worker(9, 1);
    let mut escaped_profile = profile(
        1,
        &owner_scope,
        "us-east-1",
        EnterpriseWorkerSecurityTier::Restricted,
    );
    escaped_profile.repository_access = vec![access(&foreign_scope)];
    assert!(
        EnterpriseWorkerPlacementCandidate::from_authority(
            worker.clone(),
            authenticated_placement(&worker, 1, &owner_scope),
            escaped_profile,
        )
        .is_err()
    );
}

#[test]
fn registry_claim_replays_after_restart_and_rejects_a_stale_fence() {
    let root = temporary_directory("restart-fence");
    let owner_scope = scope(5);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    register_worker(&mut storage, &owner_scope);
    let selection = select_registered_worker(&mut storage, &owner_scope);
    let claim = lease_claim(1, 1, "10", 3, 10);
    let accepted = claim_enterprise_worker_selection(&mut storage, &selection, &claim)
        .expect("accepted claim");
    assert_eq!(accepted.status, LeaseWriteStatus::Accepted);
    drop(storage);

    let mut reopened = SqliteStorage::open(&root).expect("storage reopen");
    let replay =
        claim_enterprise_worker_selection(&mut reopened, &selection, &claim).expect("claim replay");
    assert_eq!(replay.status, LeaseWriteStatus::Duplicate);
    assert!(replay.replayed);
    let placement = reopened
        .execution_registry()
        .expect("registry")
        .load_lease_placement(&claim.job_id)
        .expect("placement read")
        .expect("lease placement");
    assert_eq!(placement.worker_pool_id, WorkerPoolId(id("wpl", 1)));

    let stale = lease_claim(1, 2, "9", 11, 20);
    let rejection = claim_enterprise_worker_selection(&mut reopened, &selection, &stale)
        .expect("stale fence decision");
    assert_eq!(
        rejection.status,
        LeaseWriteStatus::RejectedStaleFencingToken
    );
    drop(reopened);
    fs::remove_dir_all(root).expect("directory release");
}

fn register_worker(storage: &mut SqliteStorage, owner_scope: &WorkerRegistryScope) {
    let worker = worker(1, 1);
    let placement = authenticated_placement(&worker, 1, owner_scope);
    let registration = WorkerRegistrationRequest {
        authentication_identity: placement.authentication_identity.clone(),
        protocol_version: worker.protocol_version.clone(),
        platform: worker.platform,
        capabilities: worker.capabilities.clone(),
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: worker.security_zone.clone(),
        max_slots: worker.max_slots,
        message_id: ExecutionMessageId(id("xmsg", 1)),
        request_id: placement.registration_request_id.clone(),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: worker.worker_id.clone(),
        worker_instance_id: worker.worker_instance_id.clone(),
    };
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker_for_scope(&registration, owner_scope)
        .expect("registration");
    registry
        .record_authenticated_worker_placement(&placement)
        .expect("placement");
    registry
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 4,
            running_slots: 3,
            message_id: ExecutionMessageId(id("xmsg", 2)),
            observed_at: instant(2),
            sent_at: instant(2),
            worker_id: worker.worker_id,
            worker_instance_id: worker.worker_instance_id,
        })
        .expect("heartbeat");
}

fn select_registered_worker(
    storage: &mut SqliteStorage,
    owner_scope: &WorkerRegistryScope,
) -> winwincode_storage::EnterpriseWorkerPlacementSelection {
    let snapshot = storage
        .execution_registry()
        .expect("registry")
        .refresh_worker_capacity_snapshot(&instant(3), &instant(0))
        .expect("capacity snapshot");
    let worker = snapshot.workers[0].clone();
    let placement = storage
        .execution_registry()
        .expect("registry")
        .load_authenticated_worker_placement(&worker.worker_id, &worker.worker_instance_id)
        .expect("placement read")
        .expect("authenticated placement");
    let candidate = EnterpriseWorkerPlacementCandidate::from_authority(
        worker,
        placement,
        profile(
            1,
            owner_scope,
            "us-east-1",
            EnterpriseWorkerSecurityTier::Restricted,
        ),
    )
    .expect("candidate");
    let request = request(1, owner_scope);
    let decisions = place_enterprise_worker_batch(
        std::slice::from_ref(&request),
        &[candidate],
        &[quota(&request, 1)],
    )
    .expect("placement");
    let EnterpriseWorkerPlacementDecision::Placed(selection) =
        decisions.into_iter().next().expect("placement decision")
    else {
        panic!("registered Worker selected")
    };
    selection
}

fn lease_claim(
    job: u64,
    request: u64,
    fence: &str,
    issued_second: u64,
    expires_second: u64,
) -> EnterpriseWorkerLeaseClaim {
    EnterpriseWorkerLeaseClaim {
        expires_at: instant(expires_second),
        fencing_token: FencingToken(fence.to_owned()),
        issued_at: instant(issued_second),
        job_id: ExecutionJobId(id("job", job)),
        lease_id: LeaseId(id("lse", request)),
        message_id: ExecutionMessageId(id("xmsg", 100 + request)),
        payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        request_id: RequestId(id("req", 100 + request)),
        attempt: request,
    }
}
