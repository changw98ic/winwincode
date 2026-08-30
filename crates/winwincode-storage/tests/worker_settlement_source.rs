// SPDX-License-Identifier: Apache-2.0

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
    ExecutionAdmissionLimits, ExecutionAdmissionPolicy, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRelease, ExecutionReservationReleaseReason,
    ExecutionReservationRequest, ExecutionReservationSettlement, ExecutionReservationStart,
    SqliteStorage, WorkerPoolId,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-worker-settlement-source-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-02-16T08:00:{second:02}.000Z"))
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

fn configure(
    admission: &mut ExecutionAdmission<'_>,
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
) {
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 20,
        max_queued: 20,
        token_budget: 20_000,
        cost_budget_microunits: 200_000,
        max_runtime_millis: 60_000,
    };
    let mut boundaries = vec![
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
    boundaries.push(ExecutionAdmissionBoundary::Delivery {
        organization_id: scope.organization_id.clone(),
        delivery_id: scope.delivery_id.clone().expect("delivery"),
    });
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("configure policy");
    }
}

fn reservation(seed: u64, scope: &ExecutionQueueScope) -> ExecutionReservationRequest {
    ExecutionReservationRequest {
        scope: scope.clone(),
        user_id: UserId(id("usr", seed)),
        worker_pool_id: pool(1),
        job_id: ExecutionJobId(id("job", seed)),
        request_id: RequestId(id("req", 100 + seed)),
        repository_access: ExecutionRepositoryAccess::ReadOnly,
        reserved_tokens: 100,
        reserved_cost_microunits: 1_000,
        runtime_limit_millis: 30_000,
        submitted_at: at(1),
    }
}

fn start(request: &ExecutionReservationRequest) -> ExecutionReservationStart {
    ExecutionReservationStart {
        scope: request.scope.clone(),
        worker_pool_id: request.worker_pool_id.clone(),
        job_id: request.job_id.clone(),
        request_id: RequestId(id("req", 200 + id_seed(&request.job_id.0))),
        expected_revision: 1,
        started_at: at(2),
    }
}

fn settlement(request: &ExecutionReservationRequest) -> ExecutionReservationSettlement {
    ExecutionReservationSettlement {
        scope: request.scope.clone(),
        worker_pool_id: request.worker_pool_id.clone(),
        job_id: request.job_id.clone(),
        request_id: RequestId(id("req", 300 + id_seed(&request.job_id.0))),
        expected_revision: 2,
        actual_tokens: 40,
        actual_cost_microunits: 400,
        actual_runtime_millis: 4_000,
        completed_at: at(3),
    }
}

fn id_seed(value: &str) -> u64 {
    value
        .rsplit('_')
        .next()
        .expect("id suffix")
        .parse()
        .expect("numeric id suffix")
}

fn reserve_and_start(
    admission: &mut ExecutionAdmission<'_>,
    request: &ExecutionReservationRequest,
) {
    admission.reserve(request).expect("reserve");
    admission.start(&start(request)).expect("start");
}

#[test]
fn settlement_freezes_authenticated_user_and_replays_one_source_after_restart() {
    let root = temporary_directory("restart");
    let request_scope = scope(1);
    let request = reservation(1, &request_scope);
    let settle = settlement(&request);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(&mut admission, &request_scope, &request.worker_pool_id);
    reserve_and_start(&mut admission, &request);
    let receipt = admission.settle(&settle).expect("settle");
    assert_eq!(receipt.reservation.user_id, request.user_id);
    let source = admission
        .load_settlement_source(&request.job_id)
        .expect("source load")
        .expect("settlement source");
    assert_eq!(source.fact.scope, request.scope);
    assert_eq!(source.fact.user_id, request.user_id);
    assert_eq!(source.fact.settlement_request_id, settle.request_id);
    assert_eq!(source.fact.actual_tokens, settle.actual_tokens);
    assert_eq!(source.fact.completed_at, settle.completed_at);
    drop(storage);

    let mut storage = SqliteStorage::open(&root).expect("storage reopen");
    let mut admission = storage.execution_admission().expect("admission reopen");
    let replay = admission.settle(&settle).expect("settlement replay");
    assert!(replay.replayed);
    assert_eq!(
        admission
            .load_settlement_source(&request.job_id)
            .expect("source reload"),
        Some(source.clone())
    );
    let page = admission
        .scan_settlement_sources(None, 200)
        .expect("source scan");
    assert_eq!(page.entries, vec![source]);

    let mut changed_reservation = request.clone();
    changed_reservation.user_id = UserId(id("usr", 2));
    assert_eq!(
        admission
            .reserve(&changed_reservation)
            .expect_err("authenticated user cannot change on replay")
            .code(),
        ExecutionAdmissionErrorCode::RequestConflict
    );
    let mut changed_settlement = settle;
    changed_settlement.actual_tokens += 1;
    assert_eq!(
        admission
            .settle(&changed_settlement)
            .expect_err("settlement receipt cannot change")
            .code(),
        ExecutionAdmissionErrorCode::RequestConflict
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn cancelled_and_failed_releases_produce_no_settlement_sources() {
    let root = temporary_directory("release");
    let request_scope = scope(2);
    let cancelled = reservation(10, &request_scope);
    let failed = reservation(11, &request_scope);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(&mut admission, &request_scope, &cancelled.worker_pool_id);
    admission.reserve(&cancelled).expect("cancelled reserve");
    admission
        .release(&ExecutionReservationRelease {
            scope: cancelled.scope.clone(),
            worker_pool_id: cancelled.worker_pool_id.clone(),
            job_id: cancelled.job_id.clone(),
            request_id: RequestId(id("req", 410)),
            expected_revision: 1,
            reason: ExecutionReservationReleaseReason::Cancelled,
            released_at: at(2),
        })
        .expect("cancel release");
    reserve_and_start(&mut admission, &failed);
    admission
        .release(&ExecutionReservationRelease {
            scope: failed.scope.clone(),
            worker_pool_id: failed.worker_pool_id.clone(),
            job_id: failed.job_id.clone(),
            request_id: RequestId(id("req", 411)),
            expected_revision: 2,
            reason: ExecutionReservationReleaseReason::Failed,
            released_at: at(3),
        })
        .expect("failed release");
    assert!(
        admission
            .scan_settlement_sources(None, 200)
            .expect("source scan")
            .entries
            .is_empty()
    );
    assert_eq!(
        admission
            .load_settlement_source(&cancelled.job_id)
            .expect("cancelled source load"),
        None
    );
    assert_eq!(
        admission
            .load_settlement_source(&failed.job_id)
            .expect("failed source load"),
        None
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn settlement_state_and_source_roll_back_together_when_receipt_commit_fails() {
    let root = temporary_directory("atomic-rollback");
    let request_scope = scope(5);
    let request = reservation(15, &request_scope);
    let settle = settlement(&request);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(&mut admission, &request_scope, &request.worker_pool_id);
    reserve_and_start(&mut admission, &request);
    drop(storage);

    let fault =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("fault connection");
    fault
        .execute_batch(&format!(
            "CREATE TRIGGER fail_settlement_receipt
             BEFORE INSERT ON execution_admission_receipts
             WHEN NEW.request_id = '{}'
             BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END;",
            settle.request_id.0
        ))
        .expect("install fault");
    let mut storage = SqliteStorage::open(&root).expect("storage reopen");
    let mut admission = storage.execution_admission().expect("admission reopen");
    assert_eq!(
        admission
            .settle(&settle)
            .expect_err("receipt failure rolls settlement back")
            .code(),
        ExecutionAdmissionErrorCode::Adapter
    );
    let reservation = admission
        .load_reservation(&request.scope, &request.worker_pool_id, &request.job_id)
        .expect("reservation load")
        .expect("reservation");
    assert_eq!(
        reservation.state,
        winwincode_storage::ExecutionReservationState::Running
    );
    assert_eq!(
        admission
            .load_settlement_source(&request.job_id)
            .expect("source load"),
        None
    );
    drop(storage);
    fault
        .execute_batch("DROP TRIGGER fail_settlement_receipt;")
        .expect("remove fault");
    drop(fault);

    let mut storage = SqliteStorage::open(&root).expect("final storage reopen");
    let mut admission = storage
        .execution_admission()
        .expect("final admission reopen");
    admission.settle(&settle).expect("settlement retry");
    assert!(
        admission
            .load_settlement_source(&request.job_id)
            .expect("source reload")
            .is_some()
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn concurrent_identical_settlement_commits_exactly_one_source() {
    let root = temporary_directory("concurrent");
    let request_scope = scope(3);
    let request = reservation(20, &request_scope);
    let settle = settlement(&request);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(&mut admission, &request_scope, &request.worker_pool_id);
    reserve_and_start(&mut admission, &request);
    drop(storage);

    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let root = root.clone();
            let settle = settle.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(root).expect("concurrent storage open");
                let mut admission = storage.execution_admission().expect("admission open");
                barrier.wait();
                admission.settle(&settle).expect("concurrent settle")
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("settlement thread"))
        .collect::<Vec<_>>();
    assert_eq!(
        receipts.iter().filter(|receipt| receipt.replayed).count(),
        1
    );
    let mut storage = SqliteStorage::open(&root).expect("storage reopen");
    assert_eq!(
        storage
            .execution_admission()
            .expect("admission reopen")
            .scan_settlement_sources(None, 200)
            .expect("source scan")
            .entries
            .len(),
        1
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn settlement_source_cursor_keeps_a_fixed_snapshot() {
    let root = temporary_directory("snapshot");
    let request_scope = scope(4);
    let first = reservation(30, &request_scope);
    let second = reservation(31, &request_scope);
    let third = reservation(32, &request_scope);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut admission = storage.execution_admission().expect("admission open");
    configure(&mut admission, &request_scope, &first.worker_pool_id);
    for request in [&first, &second] {
        reserve_and_start(&mut admission, request);
        admission
            .settle(&settlement(request))
            .expect("initial settlement");
    }
    let first_page = admission
        .scan_settlement_sources(None, 1)
        .expect("first source page");
    assert_eq!(first_page.entries.len(), 1);
    let cursor = first_page.next.expect("continued snapshot");

    reserve_and_start(&mut admission, &third);
    admission
        .settle(&settlement(&third))
        .expect("later settlement");
    let second_page = admission
        .scan_settlement_sources(Some(&cursor), 1)
        .expect("continued source page");
    assert_eq!(second_page.entries.len(), 1);
    assert_eq!(second_page.entries[0].fact.job_id, second.job_id);
    assert!(second_page.next.is_none());
    assert_eq!(
        admission
            .scan_settlement_sources(None, 200)
            .expect("new source snapshot")
            .entries
            .len(),
        3
    );

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
