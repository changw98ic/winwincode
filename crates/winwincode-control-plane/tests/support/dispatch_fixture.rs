// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{
    ExecutionMessageId, ExecutionSequence, Instant, RequestId, Sha256Digest, UserId, WorkerId,
    WorkerInstanceId,
};
use winwincode_execution_port::generated::ExecutionJob;
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseRecovery, SqliteStorage,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest,
};

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

pub fn attempt_time(attempt: u64, second: u64) -> Instant {
    Instant(format!(
        "2027-01-15T08:{:02}:{second:02}.000Z",
        attempt.saturating_sub(1)
    ))
}

pub fn register_attempt_worker(
    storage: &mut SqliteStorage,
    seed: u64,
    attempt: u64,
    clock: u64,
) -> (WorkerId, WorkerInstanceId) {
    let worker_id = WorkerId(canonical_id("wrk", seed + 100));
    let worker_instance_id = WorkerInstanceId(canonical_id("wki", seed + 100 + attempt));
    let receipt = storage
        .execution_registry()
        .expect("scheduler Registry")
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "replacement-fixture".into(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".into()],
            capability_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            security_zone: "local".into(),
            max_slots: 1,
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 200 + attempt * 10)),
            request_id: RequestId(canonical_id("req", seed + 200 + attempt * 10)),
            sent_at: attempt_time(clock, 1),
            started_at: attempt_time(clock, 0),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("register attempt Worker");
    assert_eq!(
        receipt.lease_recovery,
        if attempt == 1 {
            LeaseRecovery::NoActiveLeases
        } else {
            LeaseRecovery::ReacquireRequired
        }
    );
    storage
        .execution_registry()
        .expect("scheduler Registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 1,
            running_slots: 0,
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 201 + attempt * 10)),
            observed_at: attempt_time(clock, 2),
            sent_at: attempt_time(clock, 2),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("attempt heartbeat");
    (worker_id, worker_instance_id)
}

pub fn prepare_delivery_admission(
    storage: &mut SqliteStorage,
    seed: u64,
    scope: ExecutionQueueScope,
    job: &ExecutionJob,
    identity_seed: u64,
    clock: u64,
) {
    let worker_pool_id = WorkerPoolId(canonical_id("wpl", seed));
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
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: scope.delivery_id.clone().expect("Delivery queue scope"),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        },
    ];
    let mut admission = storage.execution_admission().expect("execution admission");
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(canonical_id("usr", seed)),
            worker_pool_id: worker_pool_id.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", identity_seed + 230)),
            repository_access: ExecutionRepositoryAccess::IsolatedWrite {
                worktree_key: job.job_id.0.clone(),
            },
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: attempt_time(clock, 1),
        })
        .expect("admission reserve");
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id,
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", identity_seed + 231)),
            expected_revision: 1,
            started_at: attempt_time(clock, 2),
        })
        .expect("admission start");
}
