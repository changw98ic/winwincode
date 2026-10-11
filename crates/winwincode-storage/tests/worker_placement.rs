// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{
    ExecutionJobId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, WorkerId,
    WorkerInstanceId, WorkspaceId,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, WorkerAffinityFailure, WorkerCapacityEntry, WorkerHealth,
    WorkerPlacementCandidate, WorkerPlacementDecision, WorkerPlacementFailure,
    WorkerPlacementGlobalFailure, WorkerPlacementQuota, WorkerPlacementRequest, WorkerPlatform,
    WorkerRepositoryAccess, WorkerSessionAffinity, place_worker_batch,
};

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn request(job: u64) -> WorkerPlacementRequest {
    WorkerPlacementRequest {
        job_id: ExecutionJobId(id("job", job)),
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
        product_session_id: ProductSessionId(id("psn", 1)),
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        required_capabilities: vec!["git".into(), "shell".into()],
        network_zone: "build".into(),
        security_zone: "trusted".into(),
        affinity: None,
    }
}

fn access(request: &WorkerPlacementRequest) -> WorkerRepositoryAccess {
    WorkerRepositoryAccess {
        organization_id: request.organization_id.clone(),
        workspace_id: request.workspace_id.clone(),
        project_id: request.project_id.clone(),
        repository_id: request.repository_id.clone(),
    }
}

fn candidate(
    worker: u64,
    available_slots: u64,
    request: &WorkerPlacementRequest,
) -> WorkerPlacementCandidate {
    WorkerPlacementCandidate {
        worker: WorkerCapacityEntry {
            worker_id: WorkerId(id("wrk", worker)),
            worker_instance_id: WorkerInstanceId(id("wki", worker)),
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::X86_64UnknownLinuxGnu,
            capabilities: vec!["git".into(), "shell".into()],
            security_zone: "trusted".into(),
            health: WorkerHealth::Healthy,
            max_slots: 4,
            running_slots: 4 - available_slots,
            available_slots,
        },
        network_zones: vec!["build".into()],
        repository_access: vec![access(request)],
    }
}

fn quota(request: &WorkerPlacementRequest, max: u64, running: u64) -> WorkerPlacementQuota {
    WorkerPlacementQuota {
        organization_id: request.organization_id.clone(),
        workspace_id: request.workspace_id.clone(),
        project_id: request.project_id.clone(),
        repository_id: request.repository_id.clone(),
        product_session_id: request.product_session_id.clone(),
        max_running_slots: max,
        running_slots: running,
    }
}

fn affinity(request: &WorkerPlacementRequest, worker: u64, instance: u64) -> WorkerSessionAffinity {
    WorkerSessionAffinity {
        organization_id: request.organization_id.clone(),
        workspace_id: request.workspace_id.clone(),
        project_id: request.project_id.clone(),
        repository_id: request.repository_id.clone(),
        product_session_id: request.product_session_id.clone(),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

#[test]
fn every_hard_constraint_excludes_the_worker_and_is_reported() {
    let request = request(1);
    let mut protocol = candidate(1, 1, &request);
    protocol.worker.protocol_version = "winwincode/v2".into();
    let mut platform = candidate(2, 1, &request);
    platform.worker.platform = WorkerPlatform::Aarch64AppleDarwin;
    let mut capability = candidate(3, 1, &request);
    capability.worker.capabilities = vec!["shell".into()];
    let mut health = candidate(4, 1, &request);
    health.worker.health = WorkerHealth::TimedOut;
    let mut security = candidate(5, 1, &request);
    security.worker.security_zone = "untrusted".into();
    let mut network = candidate(6, 1, &request);
    network.network_zones = vec!["public".into()];
    let mut repository = candidate(7, 1, &request);
    repository.repository_access.clear();
    let capacity = candidate(8, 0, &request);

    let decisions = place_worker_batch(
        std::slice::from_ref(&request),
        &[
            protocol, platform, capability, health, security, network, repository, capacity,
        ],
        &[quota(&request, 10, 0)],
    )
    .expect("placement decision");
    let WorkerPlacementDecision::Rejected(rejection) = &decisions[0] else {
        panic!("all hard constraints must reject")
    };
    assert!(rejection.global_failures.is_empty());
    assert_eq!(rejection.workers.len(), 8);
    assert_eq!(
        rejection.workers[0].failures,
        vec![WorkerPlacementFailure::ProtocolVersionMismatch {
            required: EXECUTION_PROTOCOL_VERSION.into(),
            actual: "winwincode/v2".into(),
        }]
    );
    assert_eq!(
        rejection.workers[1].failures,
        vec![WorkerPlacementFailure::PlatformMismatch {
            required: WorkerPlatform::X86_64UnknownLinuxGnu,
            actual: WorkerPlatform::Aarch64AppleDarwin,
        }]
    );
    assert_eq!(
        rejection.workers[2].failures,
        vec![WorkerPlacementFailure::MissingCapability("git".into())]
    );
    assert_eq!(
        rejection.workers[3].failures,
        vec![WorkerPlacementFailure::WorkerNotHealthy(
            WorkerHealth::TimedOut
        )]
    );
    assert_eq!(
        rejection.workers[4].failures,
        vec![WorkerPlacementFailure::SecurityZoneMismatch {
            required: "trusted".into(),
            actual: "untrusted".into(),
        }]
    );
    assert_eq!(
        rejection.workers[5].failures,
        vec![WorkerPlacementFailure::NetworkZoneUnavailable(
            "build".into()
        )]
    );
    assert_eq!(
        rejection.workers[6].failures,
        vec![WorkerPlacementFailure::RepositoryUnreachable(
            request.repository_id.clone()
        )]
    );
    assert_eq!(
        rejection.workers[7].failures,
        vec![WorkerPlacementFailure::NoAvailableCapacity]
    );
}

#[test]
fn one_visible_worker_slot_selects_only_the_canonical_first_job() {
    let first = request(1);
    let second = request(2);
    let worker = candidate(1, 1, &first);
    let scope_quota = quota(&first, 10, 0);
    let forward = place_worker_batch(
        &[first.clone(), second.clone()],
        std::slice::from_ref(&worker),
        std::slice::from_ref(&scope_quota),
    )
    .expect("forward placement");
    let reverse =
        place_worker_batch(&[second, first], &[worker], &[scope_quota]).expect("reverse placement");
    assert_eq!(forward, reverse, "caller order must not change placement");

    let WorkerPlacementDecision::Selected(selected) = &forward[0] else {
        panic!("canonical first job must receive the slot")
    };
    assert_eq!(selected.job_id, ExecutionJobId(id("job", 1)));
    assert_eq!(selected.worker_available_slots_after, 0);
    let WorkerPlacementDecision::Rejected(rejected) = &forward[1] else {
        panic!("second job must observe consumed batch capacity")
    };
    assert_eq!(rejected.job_id, ExecutionJobId(id("job", 2)));
    assert_eq!(
        rejected.workers[0].failures,
        vec![WorkerPlacementFailure::NoAvailableCapacity]
    );
}

#[test]
fn scope_quota_is_consumed_once_even_when_multiple_workers_have_capacity() {
    let first = request(10);
    let second = request(11);
    let decisions = place_worker_batch(
        &[second, first.clone()],
        &[candidate(1, 1, &first), candidate(2, 1, &first)],
        &[quota(&first, 1, 0)],
    )
    .expect("quota placement");
    assert!(matches!(decisions[0], WorkerPlacementDecision::Selected(_)));
    let WorkerPlacementDecision::Rejected(rejection) = &decisions[1] else {
        panic!("scope quota must reject the second job")
    };
    assert_eq!(
        rejection.global_failures,
        vec![WorkerPlacementGlobalFailure::ScopeQuotaExhausted {
            max_running_slots: 1,
            running_slots: 1,
        }]
    );
}

#[test]
fn valid_affinity_wins_and_replaced_instance_reroutes_deterministically() {
    let mut sticky = request(20);
    sticky.affinity = Some(affinity(&sticky, 1, 1));
    let workers = [candidate(1, 1, &sticky), candidate(2, 4, &sticky)];
    let decisions = place_worker_batch(
        std::slice::from_ref(&sticky),
        &workers,
        &[quota(&sticky, 10, 0)],
    )
    .expect("sticky placement");
    let WorkerPlacementDecision::Selected(selected) = &decisions[0] else {
        panic!("valid affinity must select")
    };
    assert_eq!(selected.worker_id, WorkerId(id("wrk", 1)));
    assert!(selected.reused_affinity);
    assert_eq!(selected.affinity_failure, None);

    let mut replaced = sticky;
    replaced.job_id = ExecutionJobId(id("job", 21));
    replaced.affinity = Some(affinity(&replaced, 1, 9));
    let decisions = place_worker_batch(
        std::slice::from_ref(&replaced),
        &workers,
        &[quota(&replaced, 10, 0)],
    )
    .expect("replacement placement");
    let WorkerPlacementDecision::Selected(selected) = &decisions[0] else {
        panic!("replacement must reroute")
    };
    assert_eq!(selected.worker_id, WorkerId(id("wrk", 2)));
    assert!(!selected.reused_affinity);
    assert_eq!(
        selected.affinity_failure,
        Some(WorkerAffinityFailure::WorkerInstanceReplaced {
            current_worker_instance_id: WorkerInstanceId(id("wki", 1)),
        })
    );

    let mut saturated = replaced;
    saturated.job_id = ExecutionJobId(id("job", 22));
    saturated.affinity = Some(affinity(&saturated, 1, 1));
    let saturated_workers = [candidate(1, 0, &saturated), candidate(2, 4, &saturated)];
    let decisions = place_worker_batch(
        std::slice::from_ref(&saturated),
        &saturated_workers,
        &[quota(&saturated, 10, 0)],
    )
    .expect("saturated affinity placement");
    let WorkerPlacementDecision::Selected(selected) = &decisions[0] else {
        panic!("saturated affinity must reroute")
    };
    assert_eq!(selected.worker_id, WorkerId(id("wrk", 2)));
    assert_eq!(
        selected.affinity_failure,
        Some(WorkerAffinityFailure::WorkerIneligible {
            failures: vec![WorkerPlacementFailure::NoAvailableCapacity],
        })
    );
}

#[test]
fn cross_tenant_workspace_affinity_is_not_reused() {
    let mut request = request(30);
    let mut foreign_affinity = affinity(&request, 1, 1);
    foreign_affinity.organization_id = OrganizationId(id("org", 9));
    foreign_affinity.workspace_id = WorkspaceId(id("wsp", 9));
    request.affinity = Some(foreign_affinity);

    let mut foreign_worker = candidate(1, 4, &request);
    foreign_worker.repository_access[0].organization_id = OrganizationId(id("org", 9));
    foreign_worker.repository_access[0].workspace_id = WorkspaceId(id("wsp", 9));
    let exact_worker = candidate(2, 1, &request);
    let decisions = place_worker_batch(
        std::slice::from_ref(&request),
        &[foreign_worker.clone(), exact_worker],
        &[quota(&request, 10, 0)],
    )
    .expect("cross-tenant reroute");
    let WorkerPlacementDecision::Selected(selected) = &decisions[0] else {
        panic!("exact tenant Worker must be selected")
    };
    assert_eq!(selected.worker_id, WorkerId(id("wrk", 2)));
    assert!(!selected.reused_affinity);
    assert_eq!(
        selected.affinity_failure,
        Some(WorkerAffinityFailure::ScopeMismatch)
    );

    let rejection = place_worker_batch(
        std::slice::from_ref(&request),
        &[foreign_worker],
        &[quota(&request, 10, 0)],
    )
    .expect("cross-tenant rejection");
    let WorkerPlacementDecision::Rejected(rejection) = &rejection[0] else {
        panic!("foreign workspace must not be reused")
    };
    assert_eq!(
        rejection.workers[0].failures,
        vec![WorkerPlacementFailure::WorkspaceTenantMismatch {
            organization_id: request.organization_id,
            workspace_id: request.workspace_id,
        }]
    );
}
