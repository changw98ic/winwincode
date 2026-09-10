// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
};

use winwincode_control_plane::DurableWorkerExecutionLifecycle;
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId,
    WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_storage::{
    AuthenticatedWorkerPlacement, EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary,
    ExecutionAdmissionLimits, ExecutionAdmissionPolicy, ExecutionJobSubmission,
    ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseWriteStatus, SqliteStorage,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerRegistryScope,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-08-12T08:00:{second:02}.000Z"))
}

#[test]
#[allow(clippy::too_many_lines)]
fn authenticated_claim_replays_after_restart() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-worker-claim-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ));
    let scope = ExecutionQueueScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
        product_session_id: ProductSessionId(id("psn", 5)),
        delivery_id: None,
    };
    let job_id = ExecutionJobId(id("job", 6));
    let worker_id = WorkerId(id("wrk", 7));
    let worker_instance_id = WorkerInstanceId(id("wki", 8));
    let worker_pool_id = WorkerPoolId(id("wpl", 9));
    let payload_digest = Sha256Digest(format!("sha256:{}", "a".repeat(64)));
    let identity = WorkerAuthenticationIdentity::TransportPrincipal {
        issuer: "local-worker-identity".to_owned(),
        subject: "remote-worker-07".to_owned(),
        credential_fingerprint: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
    };
    let management_scope = WorkerRegistryScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    };
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: scope.clone(),
            job_id: job_id.clone(),
            request_id: RequestId(id("req", 10)),
            payload_digest: payload_digest.clone(),
            dispatch_payload: b"{}".to_vec(),
            attempt: 1,
            dependencies: Vec::new(),
            work_run_id: None,
            submitted_at: at(1),
        })
        .expect("submit job");
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 8,
        max_queued: 8,
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
            .expect("configure admission");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(id("usr", 11)),
            worker_pool_id: worker_pool_id.clone(),
            job_id: job_id.clone(),
            request_id: RequestId(id("req", 12)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: at(2),
        })
        .expect("reserve admission");
    admission
        .start(&ExecutionReservationStart {
            scope: scope.clone(),
            worker_pool_id: worker_pool_id.clone(),
            job_id: job_id.clone(),
            request_id: RequestId(id("req", 13)),
            expected_revision: 1,
            started_at: at(3),
        })
        .expect("start admission");
    let registration_request_id = RequestId(id("req", 14));
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker_for_scope(
            &WorkerRegistrationRequest {
                authentication_identity: identity.clone(),
                protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
                platform: WorkerPlatform::Aarch64AppleDarwin,
                capabilities: vec!["codex".to_owned()],
                capability_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
                security_zone: "local".to_owned(),
                max_slots: 2,
                message_id: ExecutionMessageId(id("xmsg", 15)),
                request_id: registration_request_id.clone(),
                sent_at: at(1),
                started_at: at(0),
                worker_id: worker_id.clone(),
                worker_instance_id: worker_instance_id.clone(),
            },
            &management_scope,
        )
        .expect("register worker");
    registry
        .record_authenticated_worker_placement(&AuthenticatedWorkerPlacement {
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
            worker_pool_id,
            management_scope,
            authentication_identity: identity,
            registration_request_id,
            placed_at: at(1),
        })
        .expect("record placement");
    registry
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 2,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 2,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", 16)),
            observed_at: at(2),
            sent_at: at(2),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("record heartbeat");
    drop(storage);

    let claim = ExecutionLeaseClaim {
        expires_at: at(50),
        fencing_token: FencingToken("1".to_owned()),
        issued_at: at(5),
        job_id,
        lease_id: LeaseId(id("lse", 17)),
        message_id: ExecutionMessageId(id("xmsg", 18)),
        payload_digest,
        request_id: RequestId(id("req", 19)),
        worker_id,
        worker_instance_id,
        attempt: 1,
    };
    let first = DurableWorkerExecutionLifecycle::open(&root)
        .expect("open lifecycle")
        .claim(&claim)
        .expect("claim");
    assert_eq!(first.status, LeaseWriteStatus::Accepted);
    let replay = DurableWorkerExecutionLifecycle::open(&root)
        .expect("restart lifecycle")
        .claim(&claim)
        .expect("replay claim");
    assert_eq!(replay.status, LeaseWriteStatus::Duplicate);
    fs::remove_dir_all(root).expect("remove fixture");
}
