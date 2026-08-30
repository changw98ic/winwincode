use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, UserId, WorkspaceId,
};
use winwincode_storage::{
    ExecutionAdmission, ExecutionAdmissionBoundary, ExecutionAdmissionErrorCode,
    ExecutionAdmissionLimits, ExecutionAdmissionPolicy, ExecutionAdmissionUsage,
    ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRelease,
    ExecutionReservationReleaseReason, ExecutionReservationRequest, ExecutionReservationSettlement,
    ExecutionReservationStart, ExecutionReservationState, SqliteStorage, WorkerPoolId,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-execution-admission-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-02-15T08:00:{second:02}.000Z"))
}

fn scope(seed: u64) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
        delivery_id: Some(DeliveryId(id("dlv", seed))),
        product_session_id: ProductSessionId(id("psn", seed)),
    }
}

fn pool(seed: u64) -> WorkerPoolId {
    WorkerPoolId(id("wpl", seed))
}

fn boundaries(
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
) -> Vec<ExecutionAdmissionBoundary> {
    vec![
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
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: scope.delivery_id.clone().expect("delivery"),
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
    ]
}

fn configure(
    admission: &mut ExecutionAdmission<'_>,
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
    limits: ExecutionAdmissionLimits,
) {
    for boundary in boundaries(scope, worker_pool_id) {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("policy configure");
    }
}

fn reservation(
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
    job: u64,
    request: u64,
    access: ExecutionRepositoryAccess,
) -> ExecutionReservationRequest {
    ExecutionReservationRequest {
        scope: scope.clone(),
        user_id: UserId(id("usr", job)),
        worker_pool_id: worker_pool_id.clone(),
        job_id: ExecutionJobId(id("job", job)),
        request_id: RequestId(id("req", request)),
        repository_access: access,
        reserved_tokens: 100,
        reserved_cost_microunits: 1_000,
        runtime_limit_millis: 30_000,
        submitted_at: at(1),
    }
}

fn start(request: &ExecutionReservationRequest, request_id: u64) -> ExecutionReservationStart {
    ExecutionReservationStart {
        scope: request.scope.clone(),
        worker_pool_id: request.worker_pool_id.clone(),
        job_id: request.job_id.clone(),
        request_id: RequestId(id("req", request_id)),
        expected_revision: 1,
        started_at: at(2),
    }
}

fn generous_limits() -> ExecutionAdmissionLimits {
    ExecutionAdmissionLimits {
        max_concurrent: 10,
        max_queued: 20,
        token_budget: 10_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 60_000,
    }
}

#[test]
fn concurrent_queue_and_budget_reservation_has_exactly_one_winner() {
    let root = temporary_directory("atomic");
    let request_scope = scope(1);
    let worker_pool = pool(1);
    let limits = ExecutionAdmissionLimits {
        max_queued: 1,
        token_budget: 100,
        cost_budget_microunits: 1_000,
        ..generous_limits()
    };
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    configure(
        &mut storage.execution_admission().expect("admission open"),
        &request_scope,
        &worker_pool,
        limits,
    );
    drop(storage);

    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for (job, request_id) in [(1, 1), (2, 2)] {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        let request = reservation(
            &request_scope,
            &worker_pool,
            job,
            request_id,
            ExecutionRepositoryAccess::ReadOnly,
        );
        handles.push(thread::spawn(move || {
            let mut storage = SqliteStorage::open(root).expect("concurrent storage open");
            let mut admission = storage.execution_admission().expect("admission open");
            barrier.wait();
            admission
                .reserve(&request)
                .map(|receipt| receipt.reservation.job_id)
                .map_err(|error| error.code())
        }));
    }
    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("reservation thread"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| {
                matches!(
                    result,
                    Err(ExecutionAdmissionErrorCode::QueueCapacityExhausted)
                )
            })
            .count(),
        1
    );

    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn concurrency_is_enforced_across_every_scope_and_isolated_between_organizations() {
    let root = temporary_directory("concurrency");
    let first_scope = scope(2);
    let second_scope = scope(3);
    let first_pool = pool(2);
    let second_pool = pool(3);
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 1,
        ..generous_limits()
    };
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(&mut admission, &first_scope, &first_pool, limits);
    configure(&mut admission, &second_scope, &second_pool, limits);
    let first = reservation(
        &first_scope,
        &first_pool,
        20,
        20,
        ExecutionRepositoryAccess::ReadOnly,
    );
    let blocked = reservation(
        &first_scope,
        &first_pool,
        21,
        21,
        ExecutionRepositoryAccess::ReadOnly,
    );
    let isolated = reservation(
        &second_scope,
        &second_pool,
        30,
        30,
        ExecutionRepositoryAccess::ReadOnly,
    );
    for request in [&first, &blocked, &isolated] {
        admission.reserve(request).expect("reservation");
    }
    admission.start(&start(&first, 40)).expect("first start");
    let error = admission
        .start(&start(&blocked, 41))
        .expect_err("scope concurrency exhausted");
    assert_eq!(
        error.code(),
        ExecutionAdmissionErrorCode::ConcurrencyExhausted
    );
    assert_eq!(
        error.boundary(),
        Some(&ExecutionAdmissionBoundary::Organization {
            organization_id: first_scope.organization_id.clone(),
        })
    );
    admission
        .start(&start(&isolated, 42))
        .expect("other organization starts");

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn each_scope_level_and_worker_pool_has_an_independent_concurrency_limit() {
    let root = temporary_directory("boundary-concurrency");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");

    for target_index in 0_usize..6 {
        let seed = 100 + u64::try_from(target_index).expect("small boundary index");
        let request_scope = scope(seed);
        let worker_pool = pool(seed);
        let configured = boundaries(&request_scope, &worker_pool);
        for (index, boundary) in configured.iter().cloned().enumerate() {
            let limits = ExecutionAdmissionLimits {
                max_concurrent: u64::from(index != target_index) * 10,
                ..generous_limits()
            };
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("boundary policy configure");
        }
        let request = reservation(
            &request_scope,
            &worker_pool,
            seed,
            seed,
            ExecutionRepositoryAccess::ReadOnly,
        );
        admission.reserve(&request).expect("boundary reserve");
        let error = admission
            .start(&start(
                &request,
                200 + u64::try_from(target_index).expect("small boundary index"),
            ))
            .expect_err("boundary concurrency exhausted");
        assert_eq!(
            error.code(),
            ExecutionAdmissionErrorCode::ConcurrencyExhausted
        );
        assert_eq!(error.boundary(), Some(&configured[target_index]));
    }

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn cancellation_and_failure_release_all_reserved_resources_across_restart() {
    let root = temporary_directory("release-restart");
    let request_scope = scope(4);
    let worker_pool = pool(4);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(
        &mut admission,
        &request_scope,
        &worker_pool,
        generous_limits(),
    );
    let cancelled = reservation(
        &request_scope,
        &worker_pool,
        40,
        40,
        ExecutionRepositoryAccess::ReadOnly,
    );
    admission.reserve(&cancelled).expect("cancelled reserve");
    let cancellation = ExecutionReservationRelease {
        scope: request_scope.clone(),
        worker_pool_id: worker_pool.clone(),
        job_id: cancelled.job_id.clone(),
        request_id: RequestId(id("req", 41)),
        expected_revision: 1,
        reason: ExecutionReservationReleaseReason::Cancelled,
        released_at: at(2),
    };
    admission
        .release(&cancellation)
        .expect("cancellation release");

    let failed = reservation(
        &request_scope,
        &worker_pool,
        41,
        42,
        ExecutionRepositoryAccess::ReadOnly,
    );
    admission.reserve(&failed).expect("failed reserve");
    admission.start(&start(&failed, 43)).expect("failed start");
    admission
        .release(&ExecutionReservationRelease {
            scope: request_scope.clone(),
            worker_pool_id: worker_pool.clone(),
            job_id: failed.job_id.clone(),
            request_id: RequestId(id("req", 44)),
            expected_revision: 2,
            reason: ExecutionReservationReleaseReason::Failed,
            released_at: at(3),
        })
        .expect("failure release");
    let organization = boundaries(&request_scope, &worker_pool).remove(0);
    assert_eq!(
        admission.usage(&organization).expect("released usage"),
        ExecutionAdmissionUsage::default()
    );
    drop(storage);

    let mut restarted = SqliteStorage::open(&root).expect("storage restart");
    let mut admission = restarted.execution_admission().expect("admission restart");
    let replay = admission.release(&cancellation).expect("release replay");
    assert!(replay.replayed);
    assert_eq!(
        replay.reservation.state,
        ExecutionReservationState::Released
    );
    assert_eq!(
        admission.usage(&organization).expect("restart usage"),
        ExecutionAdmissionUsage::default()
    );

    drop(restarted);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn completion_settles_actual_usage_and_releases_unused_budget() {
    let root = temporary_directory("settlement");
    let request_scope = scope(5);
    let worker_pool = pool(5);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(
        &mut admission,
        &request_scope,
        &worker_pool,
        generous_limits(),
    );
    let request = reservation(
        &request_scope,
        &worker_pool,
        50,
        50,
        ExecutionRepositoryAccess::ReadOnly,
    );
    admission.reserve(&request).expect("reserve");
    admission.start(&start(&request, 51)).expect("start");
    let settlement = ExecutionReservationSettlement {
        scope: request_scope.clone(),
        worker_pool_id: worker_pool.clone(),
        job_id: request.job_id.clone(),
        request_id: RequestId(id("req", 52)),
        expected_revision: 2,
        actual_tokens: 40,
        actual_cost_microunits: 300,
        actual_runtime_millis: 20_000,
        completed_at: at(3),
    };
    let settled = admission.settle(&settlement).expect("settle");
    assert_eq!(
        settled.reservation.state,
        ExecutionReservationState::Settled
    );
    let usage = admission
        .usage(&boundaries(&request_scope, &worker_pool).remove(0))
        .expect("settled usage");
    assert_eq!(usage.queued, 0);
    assert_eq!(usage.running, 0);
    assert_eq!(usage.reserved_tokens, 0);
    assert_eq!(usage.reserved_cost_microunits, 0);
    assert_eq!(usage.committed_tokens, 40);
    assert_eq!(usage.committed_cost_microunits, 300);
    assert!(
        admission
            .settle(&settlement)
            .expect("settlement replay")
            .replayed
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn exhausted_budget_and_runtime_have_stable_rejection_codes() {
    let root = temporary_directory("rejection-codes");
    let request_scope = scope(6);
    let worker_pool = pool(6);
    let limits = ExecutionAdmissionLimits {
        token_budget: 50,
        cost_budget_microunits: 500,
        max_runtime_millis: 1_000,
        ..generous_limits()
    };
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(&mut admission, &request_scope, &worker_pool, limits);

    let mut token = reservation(
        &request_scope,
        &worker_pool,
        60,
        60,
        ExecutionRepositoryAccess::ReadOnly,
    );
    token.reserved_tokens = 51;
    token.reserved_cost_microunits = 1;
    token.runtime_limit_millis = 1;
    assert_eq!(
        admission
            .reserve(&token)
            .expect_err("token exhausted")
            .code(),
        ExecutionAdmissionErrorCode::TokenBudgetExhausted
    );

    let mut cost = token.clone();
    cost.job_id = ExecutionJobId(id("job", 61));
    cost.request_id = RequestId(id("req", 61));
    cost.reserved_tokens = 1;
    cost.reserved_cost_microunits = 501;
    assert_eq!(
        admission.reserve(&cost).expect_err("cost exhausted").code(),
        ExecutionAdmissionErrorCode::CostBudgetExhausted
    );

    let mut runtime = cost;
    runtime.job_id = ExecutionJobId(id("job", 62));
    runtime.request_id = RequestId(id("req", 62));
    runtime.reserved_cost_microunits = 1;
    runtime.runtime_limit_millis = 1_001;
    assert_eq!(
        admission
            .reserve(&runtime)
            .expect_err("runtime exceeded")
            .code(),
        ExecutionAdmissionErrorCode::RuntimeLimitExceeded
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn repository_writes_serialize_unless_worktrees_are_distinct() {
    let root = temporary_directory("repository-write");
    let request_scope = scope(7);
    let worker_pool = pool(7);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(
        &mut admission,
        &request_scope,
        &worker_pool,
        generous_limits(),
    );
    let shared = reservation(
        &request_scope,
        &worker_pool,
        70,
        70,
        ExecutionRepositoryAccess::SharedWrite,
    );
    let worktree_one = reservation(
        &request_scope,
        &worker_pool,
        71,
        71,
        ExecutionRepositoryAccess::IsolatedWrite {
            worktree_key: "worktree-one".into(),
        },
    );
    let worktree_two = reservation(
        &request_scope,
        &worker_pool,
        72,
        72,
        ExecutionRepositoryAccess::IsolatedWrite {
            worktree_key: "worktree-two".into(),
        },
    );
    let duplicate_worktree = reservation(
        &request_scope,
        &worker_pool,
        73,
        73,
        ExecutionRepositoryAccess::IsolatedWrite {
            worktree_key: "worktree-one".into(),
        },
    );
    for request in [&shared, &worktree_one, &worktree_two, &duplicate_worktree] {
        admission.reserve(request).expect("write reservation");
    }
    admission.start(&start(&shared, 80)).expect("shared start");
    assert_eq!(
        admission
            .start(&start(&worktree_one, 81))
            .expect_err("shared write blocks isolated")
            .code(),
        ExecutionAdmissionErrorCode::RepositoryWriteConflict
    );
    admission
        .release(&ExecutionReservationRelease {
            scope: request_scope.clone(),
            worker_pool_id: worker_pool.clone(),
            job_id: shared.job_id.clone(),
            request_id: RequestId(id("req", 82)),
            expected_revision: 2,
            reason: ExecutionReservationReleaseReason::Cancelled,
            released_at: at(3),
        })
        .expect("shared release");
    admission
        .start(&start(&worktree_one, 83))
        .expect("first worktree start");
    admission
        .start(&start(&worktree_two, 84))
        .expect("distinct worktree start");
    assert_eq!(
        admission
            .start(&start(&duplicate_worktree, 85))
            .expect_err("same worktree conflict")
            .code(),
        ExecutionAdmissionErrorCode::RepositoryWriteConflict
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
