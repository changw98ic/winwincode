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
        reserved_tokens: Some(100),
        reserved_cost_microunits: Some(1_000),
        runtime_limit_millis: Some(30_000),
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
        token_budget: Some(10_000),
        cost_budget_microunits: Some(100_000),
        max_runtime_millis: Some(60_000),
    }
}

#[test]
fn late_predecessor_receipts_are_additive_idempotent_and_do_not_revive_cancelled_work() {
    for tokens_first in [true, false] {
        reconcile_late_predecessor(tokens_first);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the fixture checks both arrival orders across a restart and conflicting replays"
)]
fn reconcile_late_predecessor(tokens_first: bool) {
    use winwincode_domain::{
        FencingToken, LeaseId, ModelExchangeId, Sha256Digest, WorkerId, WorkerInstanceId,
    };
    use winwincode_execution_port::{
        accounting::{AttemptAccountingStatement, ProviderAccountingReceipt},
        action_enforcement::ActionEnforcementSigningKey,
        generated::{
            ExecutionLeaseStamp, ExecutionOutcomeUsage, ExecutionOutcomeUsageAccountingStatus,
        },
    };
    use winwincode_storage::ExecutionLeaseRecord;
    let root = temporary_directory("late-predecessor-provider-receipts");
    let scope = scope(190);
    let pool = pool(190);
    let request = reservation(&scope, &pool, 190, 190, ExecutionRepositoryAccess::ReadOnly);
    let mut storage = SqliteStorage::open(&root).unwrap();
    let mut admission = storage.execution_admission().unwrap();
    configure(&mut admission, &scope, &pool, generous_limits());
    admission.reserve(&request).unwrap();
    admission.start(&start(&request, 191)).unwrap();
    let terminal = ExecutionReservationRelease {
        scope: scope.clone(),
        worker_pool_id: pool.clone(),
        job_id: request.job_id.clone(),
        request_id: RequestId(id("req", 192)),
        expected_revision: 2,
        reason: ExecutionReservationReleaseReason::Cancelled,
        released_at: at(3),
    };
    let lease = ExecutionLeaseStamp {
        job_id: request.job_id.clone(),
        lease_id: LeaseId(id("lse", 190)),
        worker_id: WorkerId(id("wrk", 190)),
        worker_instance_id: WorkerInstanceId(id("wki", 190)),
        attempt: 1,
        fencing_token: FencingToken("1".into()),
        issued_at: at(1),
        expires_at: at(2),
    };
    let previous = ExecutionLeaseRecord {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        payload_digest: Sha256Digest(format!("sha256:{}", "e".repeat(64))),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        attempt: 1,
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        expires_at: lease.expires_at.clone(),
    };
    let mut current = lease.clone();
    current.attempt = 2;
    current.lease_id = LeaseId(id("lse", 191));
    current.fencing_token = FencingToken("2".into());
    storage.execution_registry().unwrap();
    let fixture = rusqlite::Connection::open(storage.database_path()).unwrap();
    fixture
        .execute(
            "INSERT INTO execution_attempt_accounting_pending VALUES(?1,1,?2,?3)",
            rusqlite::params![
                request.job_id.0,
                serde_json::to_string(&previous).unwrap(),
                at(2).0
            ],
        )
        .unwrap();
    fixture
        .execute(
            "INSERT INTO execution_leases VALUES(?1,?2,?3,?4,?5,2,?6,?7,?8)",
            rusqlite::params![
                current.job_id.0,
                current.lease_id.0,
                previous.payload_digest.0,
                current.worker_id.0,
                current.worker_instance_id.0,
                current.fencing_token.0,
                current.issued_at.0,
                current.expires_at.0
            ],
        )
        .unwrap();
    drop(fixture);
    let current_usage = ExecutionOutcomeUsage {
        runtime_millis: 1000,
        tokens: Some(20),
        known_tokens: 20,
        cost_microunits: Some(30),
        accounting_status: ExecutionOutcomeUsageAccountingStatus::Known,
    };
    let combined = storage
        .combine_execution_attempt_usage(&current, &current_usage)
        .unwrap();
    assert_eq!(combined.tokens, None);
    storage
        .execution_admission()
        .unwrap()
        .release_with_unknown_usage(&terminal, &combined)
        .unwrap();
    let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
    let mut statement = AttemptAccountingStatement {
        lease: lease.clone(),
        manifest: vec!["primary:mdl_00000000000000000000000190".into()],
        receipts: vec![ProviderAccountingReceipt {
            slot: "primary:mdl_00000000000000000000000190".into(),
            model_exchange_id: ModelExchangeId(id("mdl", 190)),
            provider_id: "provider".into(),
            provider_receipt_id: "provider-response-190".into(),
            source_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            tokens: tokens_first.then_some(11),
            cost_microunits: (!tokens_first).then_some(7),
        }],
        signature: Sha256Digest(String::new()),
    };
    statement.sign(&key).unwrap();
    statement.verify(&key).unwrap();
    storage
        .reconcile_provider_attempt(&statement.verified(&key).unwrap(), &at(4))
        .unwrap();
    let fixture = rusqlite::Connection::open(storage.database_path()).unwrap();
    let usage:String=fixture.query_row("SELECT usage_json FROM execution_admission_reconciliation_facts ORDER BY sequence DESC LIMIT 1",[],|row|row.get(0)).unwrap();
    let usage: ExecutionOutcomeUsage = serde_json::from_str(&usage).unwrap();
    assert_eq!(usage.tokens, tokens_first.then_some(31));
    assert_eq!(usage.cost_microunits, (!tokens_first).then_some(37));
    assert!(
        storage
            .execution_has_unknown_predecessor(&request.job_id)
            .unwrap()
    );
    drop(fixture);
    drop(storage);
    let mut storage = SqliteStorage::open(&root).unwrap();
    storage
        .reconcile_provider_attempt(&statement.verified(&key).unwrap(), &at(5))
        .unwrap();
    let token_only = statement.clone();
    statement.receipts[0].cost_microunits = Some(7);
    statement.receipts[0].tokens = Some(11);
    statement.receipts[0].source_digest = Sha256Digest(format!("sha256:{}", "c".repeat(64)));
    statement.sign(&key).unwrap();
    storage
        .reconcile_provider_attempt(&statement.verified(&key).unwrap(), &at(6))
        .unwrap();
    storage
        .reconcile_provider_attempt(&token_only.verified(&key).unwrap(), &at(7))
        .unwrap();
    assert!(
        !storage
            .execution_has_unknown_predecessor(&request.job_id)
            .unwrap()
    );
    let record = storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&request.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, ExecutionReservationState::Released);
    let fixture = rusqlite::Connection::open(storage.database_path()).unwrap();
    let final_usage: String = fixture
        .query_row(
            "SELECT settlement_usage_json FROM execution_admission_reconciliation WHERE job_id=?1",
            [&request.job_id.0],
            |row| row.get(0),
        )
        .unwrap();
    let final_usage: ExecutionOutcomeUsage = serde_json::from_str(&final_usage).unwrap();
    assert_eq!(
        (final_usage.tokens, final_usage.cost_microunits),
        (Some(31), Some(37))
    );
    assert_eq!(
        fixture
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM execution_attempt_accounting_pending",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        1,
        "original marker remains as audit evidence"
    );
    let mut foreign = statement.clone();
    foreign.lease.fencing_token = FencingToken("999".into());
    foreign.sign(&key).unwrap();
    assert!(
        storage
            .reconcile_provider_attempt(&foreign.verified(&key).unwrap(), &at(8))
            .is_err()
    );
    let mut changed = statement.clone();
    changed.receipts[0].tokens = Some(12);
    changed.sign(&key).unwrap();
    assert!(
        storage
            .reconcile_provider_attempt(&changed.verified(&key).unwrap(), &at(8))
            .is_err()
    );
    let mut reused = statement.clone();
    reused.lease = current;
    reused.manifest = vec!["primary:current".into()];
    reused.receipts[0].slot = "primary:current".into();
    reused.receipts[0].tokens = Some(20);
    reused.receipts[0].cost_microunits = Some(30);
    reused.sign(&key).unwrap();
    assert_eq!(
        storage
            .reconcile_provider_attempt(&reused.verified(&key).unwrap(), &at(8))
            .unwrap_err()
            .kind(),
        winwincode_storage::StorageErrorKind::InvalidInput,
        "one Provider receipt cannot charge another attempt"
    );
    drop(fixture);
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_queue_and_budget_reservation_has_exactly_one_winner() {
    let root = temporary_directory("atomic");
    let request_scope = scope(1);
    let worker_pool = pool(1);
    let limits = ExecutionAdmissionLimits {
        max_queued: 1,
        token_budget: Some(100),
        cost_budget_microunits: Some(1_000),
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
        actual_cost_microunits: Some(300),
        actual_runtime_millis: 20_000,
        completed_at: at(3),
    };
    for (tokens, cost) in [(101, 300), (40, 1_001)] {
        let mut excess = settlement.clone();
        excess.actual_tokens = tokens;
        excess.actual_cost_microunits = Some(cost);
        assert_eq!(
            admission
                .settle(&excess)
                .expect_err("finite reservation still enforced")
                .code(),
            ExecutionAdmissionErrorCode::InvalidInput
        );
    }
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
    assert_eq!(usage.committed_cost_microunits, Some(300));
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
        token_budget: Some(50),
        cost_budget_microunits: Some(500),
        max_runtime_millis: Some(1_000),
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
    token.reserved_tokens = Some(51);
    token.reserved_cost_microunits = Some(1);
    token.runtime_limit_millis = Some(1);
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
    cost.reserved_tokens = Some(1);
    cost.reserved_cost_microunits = Some(501);
    assert_eq!(
        admission.reserve(&cost).expect_err("cost exhausted").code(),
        ExecutionAdmissionErrorCode::CostBudgetExhausted
    );

    let mut runtime = cost;
    runtime.job_id = ExecutionJobId(id("job", 62));
    runtime.request_id = RequestId(id("req", 62));
    runtime.reserved_cost_microunits = Some(1);
    runtime.runtime_limit_millis = Some(1_001);
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

#[test]
fn unlimited_runtime_requires_unlimited_policy_and_survives_reopen_and_settlement() {
    let root = temporary_directory("unlimited-runtime");
    let request_scope = scope(90);
    let worker_pool = pool(90);
    let mut request = reservation(
        &request_scope,
        &worker_pool,
        90,
        90,
        ExecutionRepositoryAccess::ReadOnly,
    );
    request.runtime_limit_millis = None;
    let mut missing_limit = serde_json::to_value(&request).expect("serialize");
    missing_limit
        .as_object_mut()
        .expect("object")
        .remove("runtime_limit_millis");
    assert!(serde_json::from_value::<ExecutionReservationRequest>(missing_limit).is_err());
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let mut admission = storage.execution_admission().expect("admission");
    configure(
        &mut admission,
        &request_scope,
        &worker_pool,
        generous_limits(),
    );
    assert_eq!(
        admission
            .reserve(&request)
            .expect_err("bounded policy rejects unlimited job")
            .code(),
        ExecutionAdmissionErrorCode::RuntimeLimitExceeded
    );
    drop(storage);
    fs::remove_dir_all(&root).expect("release bounded database");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let mut admission = storage.execution_admission().expect("admission");
    configure(
        &mut admission,
        &request_scope,
        &worker_pool,
        ExecutionAdmissionLimits {
            max_runtime_millis: None,
            ..generous_limits()
        },
    );
    let mut invalid = request.clone();
    invalid.runtime_limit_millis = Some(0);
    assert!(admission.reserve(&invalid).is_err());
    assert_eq!(
        admission
            .reserve(&request)
            .expect("reserve")
            .reservation
            .runtime_limit_millis,
        None
    );
    admission.start(&start(&request, 91)).expect("start");
    drop(storage);
    let mut storage = SqliteStorage::open(&root).expect("reopen");
    let mut admission = storage.execution_admission().expect("reopen admission");
    let current = admission
        .load_reservation_by_job(&request.job_id)
        .expect("load")
        .expect("record");
    assert_eq!(current.runtime_limit_millis, None);
    let receipt = admission
        .settle(&ExecutionReservationSettlement {
            scope: request_scope,
            worker_pool_id: worker_pool,
            job_id: request.job_id,
            request_id: RequestId(id("req", 92)),
            expected_revision: current.revision,
            actual_tokens: 10,
            actual_cost_microunits: Some(10),
            actual_runtime_millis: 3_600_001,
            completed_at: Instant("2027-02-15T10:00:00.000Z".into()),
        })
        .expect("settle beyond one hour");
    assert_eq!(receipt.reservation.actual_runtime_millis, Some(3_600_001));
    drop(storage);
    fs::remove_dir_all(root).expect("release");
}

#[test]
fn unlimited_budget_cannot_bypass_finite_boundaries() {
    let root = temporary_directory("unlimited-budget");
    let request_scope = scope(91);
    let worker_pool = pool(91);
    let mut request = reservation(
        &request_scope,
        &worker_pool,
        91,
        91,
        ExecutionRepositoryAccess::ReadOnly,
    );
    for (tokens, cost, expected) in [
        (
            None,
            Some(1),
            ExecutionAdmissionErrorCode::TokenBudgetExhausted,
        ),
        (
            Some(1),
            None,
            ExecutionAdmissionErrorCode::CostBudgetExhausted,
        ),
    ] {
        let mut storage = SqliteStorage::open(&root).expect("storage");
        let mut admission = storage.execution_admission().expect("admission");
        configure(
            &mut admission,
            &request_scope,
            &worker_pool,
            generous_limits(),
        );
        request.reserved_tokens = tokens;
        request.reserved_cost_microunits = cost;
        assert_eq!(
            admission
                .reserve(&request)
                .expect_err("finite boundary rejects unlimited")
                .code(),
            expected
        );
        drop(storage);
        fs::remove_dir_all(&root).expect("cleanup bounded database");
    }
}

#[test]
fn unlimited_budget_preserves_actual_usage_and_concurrency() {
    let root = temporary_directory("unlimited-budget-usage");
    let request_scope = scope(91);
    let worker_pool = pool(91);
    let mut request = reservation(
        &request_scope,
        &worker_pool,
        91,
        91,
        ExecutionRepositoryAccess::ReadOnly,
    );
    request.reserved_tokens = None;
    request.reserved_cost_microunits = None;
    for field in ["reserved_tokens", "reserved_cost_microunits"] {
        let mut value = serde_json::to_value(&request).expect("serialize");
        value.as_object_mut().expect("object").remove(field);
        assert!(serde_json::from_value::<ExecutionReservationRequest>(value).is_err());
    }
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let mut admission = storage.execution_admission().expect("admission");
    configure(
        &mut admission,
        &request_scope,
        &worker_pool,
        ExecutionAdmissionLimits {
            token_budget: None,
            cost_budget_microunits: None,
            max_concurrent: 1,
            ..generous_limits()
        },
    );
    admission.reserve(&request).expect("unlimited reserve");
    admission
        .start(&start(&request, 92))
        .expect("unlimited start");
    let mut second = request.clone();
    second.job_id = ExecutionJobId(id("job", 93));
    second.request_id = RequestId(id("req", 93));
    admission.reserve(&second).expect("second queued");
    assert_eq!(
        admission
            .start(&start(&second, 94))
            .expect_err("concurrency still enforced")
            .code(),
        ExecutionAdmissionErrorCode::ConcurrencyExhausted
    );
    drop(storage);
    let mut storage = SqliteStorage::open(&root).expect("reopen before settlement");
    let mut admission = storage.execution_admission().expect("admission");
    let current = admission
        .load_reservation_by_job(&request.job_id)
        .expect("read")
        .expect("reservation");
    assert_eq!(
        (current.reserved_tokens, current.reserved_cost_microunits),
        (None, None)
    );
    let settlement = ExecutionReservationSettlement {
        scope: request_scope.clone(),
        worker_pool_id: worker_pool.clone(),
        job_id: request.job_id.clone(),
        request_id: RequestId(id("req", 95)),
        expected_revision: current.revision,
        actual_tokens: 1_000_000_001,
        actual_cost_microunits: Some(2_000_000_003),
        actual_runtime_millis: 10,
        completed_at: at(3),
    };
    let settled = admission
        .settle(&settlement)
        .expect("settle above previous task and boundary caps");
    assert_eq!(
        settled.reservation.actual_tokens,
        Some(settlement.actual_tokens)
    );
    assert_eq!(
        settled.reservation.actual_cost_microunits,
        settlement.actual_cost_microunits
    );
    drop(storage);
    let mut storage = SqliteStorage::open(&root).expect("reopen settled");
    let mut admission = storage.execution_admission().expect("admission");
    assert!(admission.settle(&settlement).expect("replay").replayed);
    for boundary in boundaries(&request_scope, &worker_pool) {
        let usage = admission
            .usage(&boundary)
            .expect("actual usage after restart");
        assert_eq!(usage.committed_tokens, settlement.actual_tokens);
        assert_eq!(
            usage.committed_cost_microunits,
            settlement.actual_cost_microunits
        );
        assert_eq!(usage.queued, 1);
        assert_eq!(usage.running, 0);
    }
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn unknown_charge_survives_settlement_replay_and_blocks_finite_cost_admission() {
    for limited in [true, false] {
        let root = temporary_directory("unknown-settled-cost");
        let request_scope = scope(98);
        let worker_pool = pool(98);
        let request = reservation(
            &request_scope,
            &worker_pool,
            98,
            98,
            ExecutionRepositoryAccess::ReadOnly,
        );
        let mut storage = SqliteStorage::open(&root).expect("storage");
        let mut admission = storage.execution_admission().expect("admission");
        configure(
            &mut admission,
            &request_scope,
            &worker_pool,
            ExecutionAdmissionLimits {
                cost_budget_microunits: if limited {
                    generous_limits().cost_budget_microunits
                } else {
                    None
                },
                ..generous_limits()
            },
        );
        admission.reserve(&request).expect("reserve");
        admission.start(&start(&request, 99)).expect("start");
        let settlement = ExecutionReservationSettlement {
            scope: request_scope.clone(),
            worker_pool_id: worker_pool.clone(),
            job_id: request.job_id.clone(),
            request_id: RequestId(id("req", 100)),
            expected_revision: 2,
            actual_tokens: 30,
            actual_cost_microunits: None,
            actual_runtime_millis: 10,
            completed_at: at(3),
        };
        let receipt = admission
            .settle(&settlement)
            .expect("settle unknown actual cost");
        assert_eq!(receipt.reservation.actual_cost_microunits, None);
        assert_eq!(receipt.reservation.actual_tokens, Some(30));
        drop(storage);
        let mut storage = SqliteStorage::open(&root).expect("reopen");
        let mut admission = storage.execution_admission().expect("admission");
        assert!(
            admission
                .settle(&settlement)
                .expect("exact replay")
                .replayed
        );
        assert_eq!(
            admission
                .load_settlement_source(&request.job_id)
                .expect("source")
                .expect("source exists")
                .fact
                .actual_cost_microunits,
            None
        );
        for boundary in boundaries(&request_scope, &worker_pool) {
            assert_eq!(
                admission
                    .usage(&boundary)
                    .expect("usage")
                    .committed_cost_microunits,
                None
            );
        }
        let second = reservation(
            &request_scope,
            &worker_pool,
            101,
            101,
            ExecutionRepositoryAccess::ReadOnly,
        );
        if limited {
            assert_eq!(
                admission
                    .reserve(&second)
                    .expect_err("unknown is not free")
                    .code(),
                ExecutionAdmissionErrorCode::CostBudgetExhausted
            );
        } else {
            admission
                .reserve(&second)
                .expect("unlimited cost policy remains usable");
        }
        drop(storage);
        fs::remove_dir_all(root).expect("cleanup");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn terminal_unknown_accounting_reconciles_tokens_and_cost_independently_across_restart() {
    use winwincode_execution_port::generated::{
        ExecutionOutcomeUsage, ExecutionOutcomeUsageAccountingStatus,
    };
    let root = temporary_directory("independent-reconciliation");
    let scope = scope(120);
    let worker_pool = pool(120);
    let request = reservation(
        &scope,
        &worker_pool,
        120,
        120,
        ExecutionRepositoryAccess::ReadOnly,
    );
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let mut admission = storage.execution_admission().expect("admission");
    configure(&mut admission, &scope, &worker_pool, generous_limits());
    admission.reserve(&request).expect("reserve");
    admission.start(&start(&request, 121)).expect("start");
    let terminal = ExecutionReservationRelease {
        scope: scope.clone(),
        worker_pool_id: worker_pool.clone(),
        job_id: request.job_id.clone(),
        request_id: RequestId(id("req", 122)),
        expected_revision: 2,
        reason: ExecutionReservationReleaseReason::Failed,
        released_at: at(3),
    };
    let unknown = ExecutionOutcomeUsage::unknown(1000, 10);
    admission
        .release_with_unknown_usage(&terminal, &unknown)
        .expect("terminal unknown");
    let next = reservation(
        &scope,
        &worker_pool,
        130,
        130,
        ExecutionRepositoryAccess::ReadOnly,
    );
    assert!(
        admission.reserve(&next).is_err(),
        "finite budgets block unknown usage"
    );
    let noop = RequestId(id("req", 123));
    assert!(
        !admission
            .settle_reconciliation(
                &request.job_id,
                &terminal.request_id,
                &noop,
                &unknown,
                &at(4)
            )
            .expect("noop accepted")
    );
    let tokens = ExecutionOutcomeUsage {
        tokens: Some(30),
        known_tokens: 30,
        accounting_status: ExecutionOutcomeUsageAccountingStatus::Known,
        cost_microunits: None,
        runtime_millis: 1000,
    };
    assert!(
        admission
            .settle_reconciliation(
                &request.job_id,
                &terminal.request_id,
                &noop,
                &tokens,
                &at(4)
            )
            .is_err(),
        "noop identity cannot be changed"
    );
    let token_id = RequestId(id("req", 124));
    assert!(
        admission
            .settle_reconciliation(
                &request.job_id,
                &terminal.request_id,
                &token_id,
                &tokens,
                &at(5)
            )
            .expect("tokens known")
    );
    assert!(admission.reserve(&next).is_err(), "cost remains unknown");
    drop(storage);
    let mut storage = SqliteStorage::open(&root).expect("reopen");
    let mut admission = storage.execution_admission().expect("admission");
    let cost = ExecutionOutcomeUsage {
        cost_microunits: Some(40),
        ..tokens.clone()
    };
    let cost_id = RequestId(id("req", 125));
    assert!(
        admission
            .settle_reconciliation(
                &request.job_id,
                &terminal.request_id,
                &cost_id,
                &cost,
                &at(6)
            )
            .expect("cost-only reconciliation")
    );
    assert!(
        !admission
            .settle_reconciliation(
                &request.job_id,
                &terminal.request_id,
                &token_id,
                &tokens,
                &at(5)
            )
            .expect("old settlement replay")
    );
    assert!(
        admission
            .release_with_unknown_usage(&terminal, &unknown)
            .expect("original terminal replay")
            .replayed
    );
    for boundary in boundaries(&scope, &worker_pool) {
        let usage = admission.usage(&boundary).expect("usage");
        assert_eq!(usage.accounting_pending, 0);
        assert_eq!(usage.committed_tokens, 30);
        assert_eq!(usage.committed_cost_microunits, Some(40));
    }
    admission.reserve(&next).expect("settled budgets reusable");
    let lower = ExecutionOutcomeUsage {
        known_tokens: 29,
        tokens: Some(29),
        ..cost.clone()
    };
    assert!(
        admission
            .settle_reconciliation(
                &request.job_id,
                &terminal.request_id,
                &RequestId(id("req", 126)),
                &lower,
                &at(7)
            )
            .is_err()
    );
    assert!(
        admission
            .settle_reconciliation(
                &request.job_id,
                &terminal.request_id,
                &RequestId(id("req", 127)),
                &cost,
                &Instant("invalid".into())
            )
            .is_err()
    );
    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}
