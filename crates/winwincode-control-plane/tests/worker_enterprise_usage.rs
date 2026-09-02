// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{WorkerEnterpriseUsageErrorKind, WorkerEnterpriseUsageReconciler};
use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, UserId, WorkspaceId,
};
use winwincode_storage::{
    EnterpriseQuotaAmounts, EnterpriseQuotaDecision, EnterpriseQuotaReservationRequest,
    EnterpriseQuotaReservationState, EnterpriseQuotaSourceSeal, EnterpriseUsageAttribution,
    EnterpriseUsageFilter, EnterpriseUsageMeasure, EnterpriseUsageSource,
    EnterpriseUsageSourceKind, ExecutionAdmission, ExecutionAdmissionBoundary,
    ExecutionAdmissionLimits, ExecutionAdmissionPolicy, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationSettlement,
    ExecutionReservationStart, SqliteStorage, WorkerPoolId,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let seed = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-worker-enterprise-usage-{name}-{}-{seed}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-02-17T08:00:{second:02}.000Z"))
}

fn scope() -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
        delivery_id: Some(DeliveryId(id("dlv", 5))),
        product_session_id: ProductSessionId(id("psn", 6)),
    }
}

fn pool() -> WorkerPoolId {
    WorkerPoolId(id("wpl", 7))
}

fn configure(admission: &mut ExecutionAdmission<'_>) {
    let scope = scope();
    let worker_pool_id = pool();
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 10,
        max_queued: 10,
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
            organization_id: scope.organization_id,
            worker_pool_id,
        },
    ];
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("configure policy");
    }
}

fn seed_settlement(storage: &mut SqliteStorage, job_seed: u64) -> ExecutionReservationSettlement {
    let request = ExecutionReservationRequest {
        scope: scope(),
        user_id: UserId(id("usr", 8)),
        worker_pool_id: pool(),
        job_id: ExecutionJobId(id("job", job_seed)),
        request_id: RequestId(id("req", 100 + job_seed)),
        repository_access: ExecutionRepositoryAccess::ReadOnly,
        reserved_tokens: 100,
        reserved_cost_microunits: 1_000,
        runtime_limit_millis: 30_000,
        submitted_at: at(1),
    };
    let mut admission = storage.execution_admission().expect("admission");
    configure(&mut admission);
    admission.reserve(&request).expect("reserve");
    admission
        .start(&ExecutionReservationStart {
            scope: request.scope.clone(),
            worker_pool_id: request.worker_pool_id.clone(),
            job_id: request.job_id.clone(),
            request_id: RequestId(id("req", 200 + job_seed)),
            expected_revision: 1,
            started_at: at(2),
        })
        .expect("start");
    let settlement = ExecutionReservationSettlement {
        scope: request.scope,
        worker_pool_id: request.worker_pool_id,
        job_id: request.job_id,
        request_id: RequestId(id("req", 300 + job_seed)),
        expected_revision: 2,
        actual_tokens: 41,
        actual_cost_microunits: 401,
        actual_runtime_millis: 4_001,
        completed_at: at(3),
    };
    admission.settle(&settlement).expect("settle");
    settlement
}

#[test]
fn worker_source_reconciles_with_complete_frozen_attribution_after_restart() {
    let root = temporary_directory("restart");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let settlement = seed_settlement(&mut storage, 10);

    let first = WorkerEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_worker_page(None, 10)
        .expect("first reconciliation");
    assert_eq!((first.source_entries, first.inserted_entries), (1, 1));
    assert_eq!(first.replayed_entries, 0);
    let replay = WorkerEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_worker_page(None, 10)
        .expect("same-process replay");
    assert_eq!((replay.inserted_entries, replay.replayed_entries), (0, 1));
    drop(storage);

    let mut storage = SqliteStorage::open(&root).expect("restart storage");
    let restarted = WorkerEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_worker_page(None, 10)
        .expect("restart replay");
    assert_eq!(
        (restarted.inserted_entries, restarted.replayed_entries),
        (0, 1)
    );
    let expected_scope = scope();
    let filter = EnterpriseUsageFilter {
        organization_id: Some(expected_scope.organization_id.clone()),
        workspace_id: Some(expected_scope.workspace_id.clone()),
        project_id: Some(expected_scope.project_id.clone()),
        repository_id: Some(expected_scope.repository_id.clone()),
        delivery_id: expected_scope.delivery_id.clone(),
        product_session_id: Some(expected_scope.product_session_id.clone()),
        user_id: Some(UserId(id("usr", 8))),
        source_kind: Some(EnterpriseUsageSourceKind::Worker),
    };
    let page = storage
        .enterprise_usage_ledger()
        .expect("ledger")
        .scan(&filter, None, 10)
        .expect("ledger scan");
    assert_eq!(page.entries.len(), 1);
    let fact = &page.entries[0].fact;
    assert_eq!(fact.settled_at, settlement.completed_at);
    assert_eq!(
        fact.attribution.product_session_id,
        filter.product_session_id
    );
    assert_eq!(
        fact.source,
        EnterpriseUsageSource::Worker {
            job_id: settlement.job_id,
            settlement_request_id: settlement.request_id,
            worker_pool_id: pool().0,
        }
    );
    assert_eq!(
        fact.measure,
        EnterpriseUsageMeasure::Worker {
            runtime_millis: 4_001,
            tokens: 41,
            cost_microunits: 401,
        }
    );

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn worker_projection_recovers_a_post_settlement_ledger_failure_exactly_once() {
    let root = temporary_directory("recovery");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    seed_settlement(&mut storage, 20);
    storage
        .enterprise_usage_ledger()
        .expect("prepare enterprise ledger");
    let database = root.join("control-plane.sqlite3");
    let fault = rusqlite::Connection::open(database).expect("fault connection");
    fault
        .execute_batch(
            "CREATE TRIGGER fail_worker_enterprise_projection
             BEFORE INSERT ON enterprise_usage_entries
             BEGIN SELECT RAISE(ABORT, 'injected projection failure'); END;",
        )
        .expect("install fault");
    let error = WorkerEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_worker_page(None, 10)
        .expect_err("ledger insert fails");
    assert_eq!(error.kind(), WorkerEnterpriseUsageErrorKind::Ledger);
    assert_eq!(
        storage
            .enterprise_usage_ledger()
            .expect("ledger after fault")
            .reconcile(&EnterpriseUsageFilter::default())
            .expect("zero totals")
            .entries,
        0
    );
    fault
        .execute_batch("DROP TRIGGER fail_worker_enterprise_projection;")
        .expect("remove fault");
    drop(fault);
    drop(storage);

    let mut storage = SqliteStorage::open(&root).expect("restart storage");
    let applied = WorkerEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_worker_page(None, 10)
        .expect("rebuild projection");
    assert_eq!((applied.inserted_entries, applied.replayed_entries), (1, 0));
    let replay = WorkerEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_worker_page(None, 10)
        .expect("exact replay");
    assert_eq!((replay.inserted_entries, replay.replayed_entries), (0, 1));

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn worker_projection_settles_the_matching_quota_after_a_crash_window_once() {
    let root = temporary_directory("quota-recovery");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let settlement = seed_settlement(&mut storage, 30);
    let scope = scope();
    let reservation_id = RequestId(id("req", 430));
    let decision = storage
        .enterprise_quota_ledger()
        .expect("quota ledger")
        .reserve(&EnterpriseQuotaReservationRequest {
            reservation_id: reservation_id.clone(),
            attribution: EnterpriseUsageAttribution {
                organization_id: scope.organization_id,
                workspace_id: scope.workspace_id,
                project_id: scope.project_id,
                repository_id: scope.repository_id,
                delivery_id: scope.delivery_id,
                product_session_id: Some(scope.product_session_id),
                user_id: UserId(id("usr", 8)),
            },
            source_seal: EnterpriseQuotaSourceSeal::Worker {
                job_id: settlement.job_id.clone(),
                worker_pool_id: settlement.worker_pool_id.0.clone(),
            },
            reserved: EnterpriseQuotaAmounts {
                tokens: 100,
                provider_cost_micros: 0,
                worker_cost_microunits: 1_000,
                worker_runtime_millis: 30_000,
                storage_bytes: 0,
                operations: 1,
            },
            requested_at: at(1),
        })
        .expect("reserve quota");
    assert!(matches!(decision, EnterpriseQuotaDecision::Allowed(_)));

    WorkerEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_worker_page(None, 10)
        .expect("project and settle quota");
    let settled = storage
        .enterprise_quota_ledger()
        .expect("quota ledger")
        .load_reservation(&reservation_id)
        .expect("load settled reservation")
        .expect("settled reservation");
    assert_eq!(settled.state, EnterpriseQuotaReservationState::Settled);
    let terminal = settled.terminal.clone();
    drop(storage);

    let mut storage = SqliteStorage::open(&root).expect("restart storage");
    WorkerEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_worker_page(None, 10)
        .expect("exact recovery replay");
    let replayed = storage
        .enterprise_quota_ledger()
        .expect("quota ledger")
        .load_reservation(&reservation_id)
        .expect("load replayed reservation")
        .expect("replayed reservation");
    assert_eq!(replayed.revision, settled.revision);
    assert_eq!(replayed.terminal, terminal);

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}
