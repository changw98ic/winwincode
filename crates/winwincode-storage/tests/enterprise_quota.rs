use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, ExecutionJobId, Instant, ModelExchangeId, OrganizationId, ProductSessionId,
    ProjectId, PublicationId, RepositoryId, RequestId, UserId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactStorageOperationKind, EnterpriseQuotaAmounts, EnterpriseQuotaBoundary,
    EnterpriseQuotaDecision, EnterpriseQuotaDimension, EnterpriseQuotaErrorKind,
    EnterpriseQuotaLimits, EnterpriseQuotaPolicy, EnterpriseQuotaRelease,
    EnterpriseQuotaReleaseReason, EnterpriseQuotaReservationRequest,
    EnterpriseQuotaReservationState, EnterpriseQuotaSettlement, EnterpriseQuotaSourceSeal,
    EnterpriseUsageAttribution, EnterpriseUsageMeasure, EnterpriseUsageSource,
    SettledEnterpriseUsage, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-quota-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn attribution(seed: u64) -> EnterpriseUsageAttribution {
    EnterpriseUsageAttribution {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
        delivery_id: None,
        product_session_id: Some(ProductSessionId(id("psn", 5))),
        user_id: UserId(id("usr", seed)),
    }
}

fn organization_boundary() -> EnterpriseQuotaBoundary {
    EnterpriseQuotaBoundary::Organization {
        organization_id: OrganizationId(id("org", 1)),
    }
}

fn policy(revision: u64, limits: EnterpriseQuotaLimits) -> EnterpriseQuotaPolicy {
    EnterpriseQuotaPolicy {
        boundary: organization_boundary(),
        revision,
        limits,
    }
}

fn reservation(seed: u64, amounts: EnterpriseQuotaAmounts) -> EnterpriseQuotaReservationRequest {
    EnterpriseQuotaReservationRequest {
        reservation_id: RequestId(id("req", seed)),
        attribution: attribution(6),
        source_seal: source_seal(seed, amounts),
        reserved: amounts,
        requested_at: Instant("2027-04-01T08:00:00.000Z".to_owned()),
    }
}

fn source_seal(seed: u64, amounts: EnterpriseQuotaAmounts) -> EnterpriseQuotaSourceSeal {
    if amounts.worker_runtime_millis > 0 || amounts.worker_cost_microunits > 0 {
        EnterpriseQuotaSourceSeal::Worker {
            job_id: ExecutionJobId(id("job", seed)),
            worker_pool_id: id("wpl", seed),
        }
    } else if amounts.storage_bytes > 0 {
        EnterpriseQuotaSourceSeal::Storage {
            artifact_id: ArtifactId(id("art", seed)),
            operation_kind: ArtifactStorageOperationKind::ArtifactFinalize,
            request_id: RequestId(id("req", seed + 100)),
            expected_bytes: amounts.storage_bytes,
        }
    } else if amounts.tokens > 0 || amounts.provider_cost_micros > 0 {
        let usage = provider_usage(seed);
        let EnterpriseUsageSource::Provider {
            model_exchange_id,
            request_id,
            attempt,
            route_authority_fingerprint,
            ..
        } = usage.source
        else {
            unreachable!("provider fixture")
        };
        EnterpriseQuotaSourceSeal::Provider {
            model_exchange_id,
            request_id,
            attempt,
            route_authority_fingerprint,
        }
    } else {
        EnterpriseQuotaSourceSeal::Publication {
            publication_id: PublicationId(id("pub", seed)),
            operation_key: format!("publish-{seed}"),
            request_sha256: format!("sha256:{seed:064x}"),
        }
    }
}

fn provider_usage(seed: u64) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Provider {
            provider_usage_id: format!("provider-usage-{seed}"),
            source_sequence: seed,
            source_digest: format!("sha256:{seed:064x}"),
            model_exchange_id: ModelExchangeId(id("mdl", seed)),
            request_id: RequestId(id("req", seed + 100)),
            attempt: 1,
            route_authority_fingerprint: format!("sha256:{:064x}", seed + 1),
        },
        attribution: attribution(6),
        measure: EnterpriseUsageMeasure::Provider {
            input_tokens: 100,
            cached_input_tokens: 20,
            cache_write_input_tokens: 5,
            output_tokens: 30,
            reasoning_output_tokens: 10,
            total_tokens: 130,
            cost_micros: 900,
        },
        settled_at: Instant("2027-04-01T08:01:00.000Z".to_owned()),
    }
}

fn worker_usage(seed: u64, cost_microunits: u64) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Worker {
            job_id: ExecutionJobId(id("job", seed)),
            settlement_request_id: RequestId(id("req", seed + 200)),
            worker_pool_id: id("wpl", seed),
        },
        attribution: attribution(6),
        measure: EnterpriseUsageMeasure::Worker {
            runtime_millis: 50,
            tokens: 20,
            cost_microunits,
        },
        settled_at: Instant("2027-04-01T08:01:00.000Z".to_owned()),
    }
}

fn allowance() -> EnterpriseQuotaAmounts {
    EnterpriseQuotaAmounts {
        tokens: 130,
        provider_cost_micros: 900,
        worker_cost_microunits: 0,
        worker_runtime_millis: 0,
        storage_bytes: 0,
        operations: 1,
    }
}

fn allowed(
    decision: EnterpriseQuotaDecision,
) -> winwincode_storage::EnterpriseQuotaReservationReceipt {
    match decision {
        EnterpriseQuotaDecision::Allowed(receipt) => *receipt,
        EnterpriseQuotaDecision::TerminalReplay(receipt) => {
            panic!("unexpected terminal replay: {receipt:?}")
        }
        EnterpriseQuotaDecision::Denied(denial) => panic!("unexpected denial: {denial:?}"),
    }
}

#[test]
fn policy_revisions_are_exact_and_reservation_replay_survives_restart() {
    let directory = temporary_directory("policy-restart");
    let limits = EnterpriseQuotaLimits {
        max_concurrent: Some(2),
        tokens: Some(500),
        ..EnterpriseQuotaLimits::default()
    };
    let request = reservation(1, allowance());
    let original = {
        let mut sqlite = SqliteStorage::open(&directory).expect("storage");
        let mut ledger = sqlite.enterprise_quota_ledger().expect("quota");
        let first_policy = ledger.put_policy(&policy(1, limits)).expect("policy");
        assert!(!first_policy.idempotent_replay);
        assert!(
            ledger
                .put_policy(&policy(1, limits))
                .expect("policy replay")
                .idempotent_replay
        );
        let changed = policy(
            1,
            EnterpriseQuotaLimits {
                tokens: Some(499),
                ..limits
            },
        );
        assert_eq!(
            ledger
                .put_policy(&changed)
                .expect_err("changed revision")
                .kind(),
            EnterpriseQuotaErrorKind::PolicyConflict
        );
        let original = allowed(ledger.reserve(&request).expect("reserve")).record;
        let mut changed_request = request.clone();
        changed_request.reserved.tokens += 1;
        assert_eq!(
            ledger
                .reserve(&changed_request)
                .expect_err("changed reservation reuse")
                .kind(),
            EnterpriseQuotaErrorKind::ReservationConflict
        );
        original
    };

    let mut sqlite = SqliteStorage::open(&directory).expect("restart storage");
    let mut ledger = sqlite.enterprise_quota_ledger().expect("restart quota");
    let replay = allowed(ledger.reserve(&request).expect("reserve replay"));
    assert!(replay.idempotent_replay);
    assert_eq!(replay.record, original);
    let loaded = ledger
        .load_reservation(&request.reservation_id)
        .expect("load")
        .expect("reservation");
    assert_eq!(loaded, original);
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn concurrent_reservations_never_exceed_the_boundary_limit() {
    let directory = temporary_directory("concurrent");
    {
        let mut sqlite = SqliteStorage::open(&directory).expect("storage");
        sqlite
            .enterprise_quota_ledger()
            .expect("quota")
            .put_policy(&policy(
                1,
                EnterpriseQuotaLimits {
                    max_concurrent: Some(1),
                    ..EnterpriseQuotaLimits::default()
                },
            ))
            .expect("policy");
    }
    let barrier = Arc::new(Barrier::new(2));
    let handles = (10..12)
        .map(|seed| {
            let directory = directory.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut sqlite = SqliteStorage::open(directory).expect("thread storage");
                let mut ledger = sqlite.enterprise_quota_ledger().expect("thread quota");
                barrier.wait();
                ledger
                    .reserve(&reservation(seed, allowance()))
                    .expect("concurrent decision")
            })
        })
        .collect::<Vec<_>>();
    let decisions = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread join"))
        .collect::<Vec<_>>();
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| matches!(decision, EnterpriseQuotaDecision::Allowed(_)))
            .count(),
        1
    );
    assert!(decisions.iter().any(|decision| matches!(
        decision,
        EnterpriseQuotaDecision::Denied(denial)
            if denial.dimension == EnterpriseQuotaDimension::Concurrent
    )));
    drop(decisions);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn every_enterprise_quantity_is_enforced_independently() {
    let directory = temporary_directory("dimensions");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let mut ledger = sqlite.enterprise_quota_ledger().expect("quota");
    let cases = [
        (
            EnterpriseQuotaLimits {
                tokens: Some(0),
                ..EnterpriseQuotaLimits::default()
            },
            EnterpriseQuotaAmounts {
                tokens: 1,
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            EnterpriseQuotaDimension::Tokens,
        ),
        (
            EnterpriseQuotaLimits {
                provider_cost_micros: Some(0),
                ..EnterpriseQuotaLimits::default()
            },
            EnterpriseQuotaAmounts {
                provider_cost_micros: 1,
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            EnterpriseQuotaDimension::ProviderCost,
        ),
        (
            EnterpriseQuotaLimits {
                worker_cost_microunits: Some(0),
                ..EnterpriseQuotaLimits::default()
            },
            EnterpriseQuotaAmounts {
                worker_cost_microunits: 1,
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            EnterpriseQuotaDimension::WorkerCost,
        ),
        (
            EnterpriseQuotaLimits {
                worker_runtime_millis: Some(0),
                ..EnterpriseQuotaLimits::default()
            },
            EnterpriseQuotaAmounts {
                worker_runtime_millis: 1,
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            EnterpriseQuotaDimension::WorkerRuntime,
        ),
        (
            EnterpriseQuotaLimits {
                storage_bytes: Some(0),
                ..EnterpriseQuotaLimits::default()
            },
            EnterpriseQuotaAmounts {
                storage_bytes: 1,
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            EnterpriseQuotaDimension::Storage,
        ),
        (
            EnterpriseQuotaLimits {
                operations: Some(0),
                ..EnterpriseQuotaLimits::default()
            },
            EnterpriseQuotaAmounts {
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
            EnterpriseQuotaDimension::Operations,
        ),
    ];
    for (index, (limits, amounts, expected)) in cases.into_iter().enumerate() {
        let revision = u64::try_from(index + 1).expect("revision");
        ledger
            .put_policy(&policy(revision, limits))
            .expect("policy revision");
        let decision = ledger
            .reserve(&reservation(100 + revision, amounts))
            .expect("quota decision");
        assert!(matches!(
            decision,
            EnterpriseQuotaDecision::Denied(ref denial) if denial.dimension == expected
        ));
    }
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn cancellation_and_downstream_failure_release_capacity_once() {
    let directory = temporary_directory("release");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let mut ledger = sqlite.enterprise_quota_ledger().expect("quota");
    ledger
        .put_policy(&policy(
            1,
            EnterpriseQuotaLimits {
                max_concurrent: Some(1),
                ..EnterpriseQuotaLimits::default()
            },
        ))
        .expect("policy");
    let first = reservation(20, allowance());
    allowed(ledger.reserve(&first).expect("first reserve"));
    let release = EnterpriseQuotaRelease {
        reservation_id: first.reservation_id,
        request_id: RequestId(id("req", 21)),
        expected_revision: 1,
        reason: EnterpriseQuotaReleaseReason::OperationalAdmissionDenied,
        released_at: Instant("2027-04-01T08:00:01.000Z".to_owned()),
    };
    let first_release = ledger.release(&release).expect("release");
    assert!(!first_release.idempotent_replay);
    assert_eq!(
        first_release.record.state,
        EnterpriseQuotaReservationState::Released
    );
    assert!(
        ledger
            .release(&release)
            .expect("release replay")
            .idempotent_replay
    );
    allowed(
        ledger
            .reserve(&reservation(22, allowance()))
            .expect("capacity released"),
    );
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn settlement_accepts_only_the_exact_durable_usage_source() {
    let directory = temporary_directory("settlement");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let request = reservation(30, allowance());
    {
        let mut quota = sqlite.enterprise_quota_ledger().expect("quota");
        allowed(quota.reserve(&request).expect("reserve"));
    }
    let fact = provider_usage(30);
    sqlite
        .enterprise_usage_ledger()
        .expect("Usage")
        .record(&fact)
        .expect("settled Usage");
    let settlement = EnterpriseQuotaSettlement {
        reservation_id: request.reservation_id.clone(),
        request_id: RequestId(id("req", 31)),
        expected_revision: 1,
        usage_source: fact.source,
    };
    let first = sqlite
        .enterprise_quota_ledger()
        .expect("quota")
        .settle(&settlement)
        .expect("settle");
    assert_eq!(first.record.state, EnterpriseQuotaReservationState::Settled);
    assert!(!first.idempotent_replay);
    drop(sqlite);

    let mut sqlite = SqliteStorage::open(&directory).expect("restart storage");
    let replay = sqlite
        .enterprise_quota_ledger()
        .expect("restart quota")
        .settle(&settlement)
        .expect("settlement replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.record, first.record);
    assert!(matches!(
        sqlite
            .enterprise_quota_ledger()
            .expect("quota")
            .reserve(&request)
            .expect("terminal reserve replay"),
        EnterpriseQuotaDecision::TerminalReplay(_)
    ));
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn missing_or_foreign_usage_cannot_terminalize_a_reservation() {
    let directory = temporary_directory("foreign-settlement");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let request = reservation(35, allowance());
    sqlite
        .enterprise_quota_ledger()
        .expect("quota")
        .reserve(&request)
        .map(allowed)
        .expect("reserve");
    let mut foreign = provider_usage(35);
    foreign.attribution.user_id = UserId(id("usr", 7));
    let missing = EnterpriseQuotaSettlement {
        reservation_id: request.reservation_id.clone(),
        request_id: RequestId(id("req", 36)),
        expected_revision: 1,
        usage_source: foreign.source.clone(),
    };
    assert_eq!(
        sqlite
            .enterprise_quota_ledger()
            .expect("quota")
            .settle(&missing)
            .expect_err("missing Usage")
            .kind(),
        EnterpriseQuotaErrorKind::AuthorityMismatch
    );
    sqlite
        .enterprise_usage_ledger()
        .expect("Usage")
        .record(&foreign)
        .expect("foreign Usage");
    assert_eq!(
        sqlite
            .enterprise_quota_ledger()
            .expect("quota")
            .settle(&missing)
            .expect_err("foreign Usage")
            .kind(),
        EnterpriseQuotaErrorKind::AuthorityMismatch
    );
    let current = sqlite
        .enterprise_quota_ledger()
        .expect("quota")
        .load_reservation(&request.reservation_id)
        .expect("load")
        .expect("reservation");
    assert_eq!(current.state, EnterpriseQuotaReservationState::Active);
    assert_eq!(current.revision, 1);
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn terminal_replay_revalidates_the_immutable_usage_authority() {
    let directory = temporary_directory("corrupt-usage-replay");
    let request = reservation(38, allowance());
    let fact = provider_usage(38);
    let settlement = EnterpriseQuotaSettlement {
        reservation_id: request.reservation_id.clone(),
        request_id: RequestId(id("req", 39)),
        expected_revision: 1,
        usage_source: fact.source.clone(),
    };
    {
        let mut sqlite = SqliteStorage::open(&directory).expect("storage");
        sqlite
            .enterprise_quota_ledger()
            .expect("quota")
            .reserve(&request)
            .map(allowed)
            .expect("reserve");
        sqlite
            .enterprise_usage_ledger()
            .expect("Usage")
            .record(&fact)
            .expect("Usage fact");
        sqlite
            .enterprise_quota_ledger()
            .expect("quota")
            .settle(&settlement)
            .expect("settle");
    }
    let connection = rusqlite::Connection::open(directory.join("control-plane.sqlite3"))
        .expect("tamper connection");
    connection
        .execute("DROP TRIGGER enterprise_usage_entries_no_update", [])
        .expect("simulate bypass of immutable trigger");
    connection
        .execute(
            "UPDATE enterprise_usage_entries SET source_digest = ?1",
            [format!("sha256:{:064x}", 999)],
        )
        .expect("tamper Usage authority");
    drop(connection);
    let mut sqlite = SqliteStorage::open(&directory).expect("restart storage");
    let error = sqlite
        .enterprise_quota_ledger()
        .expect("quota")
        .settle(&settlement)
        .expect_err("corrupt Usage replay");
    assert_eq!(error.kind(), EnterpriseQuotaErrorKind::CorruptState);
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn settled_usage_and_every_applicable_policy_are_intersected() {
    let directory = temporary_directory("settled-intersection");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let fact = provider_usage(40);
    sqlite
        .enterprise_usage_ledger()
        .expect("Usage")
        .record(&fact)
        .expect("settled Usage");
    let mut quota = sqlite.enterprise_quota_ledger().expect("quota");
    quota
        .put_policy(&policy(
            1,
            EnterpriseQuotaLimits {
                tokens: Some(200),
                ..EnterpriseQuotaLimits::default()
            },
        ))
        .expect("organization policy");
    quota
        .put_policy(&EnterpriseQuotaPolicy {
            boundary: EnterpriseQuotaBoundary::User {
                organization_id: attribution(6).organization_id,
                user_id: attribution(6).user_id,
            },
            revision: 1,
            limits: EnterpriseQuotaLimits {
                operations: Some(1),
                ..EnterpriseQuotaLimits::default()
            },
        })
        .expect("user policy");
    let decision = quota
        .reserve(&reservation(
            41,
            EnterpriseQuotaAmounts {
                tokens: 70,
                operations: 1,
                ..EnterpriseQuotaAmounts::default()
            },
        ))
        .expect("decision");
    assert!(matches!(
        decision,
        EnterpriseQuotaDecision::Denied(ref denial)
            if denial.dimension == EnterpriseQuotaDimension::Operations
    ));
    assert!(
        quota
            .load_reservation(&RequestId(id("req", 41)))
            .expect("load denied")
            .is_none()
    );
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn noncanonical_durable_reservation_bytes_fail_closed_after_restart() {
    let directory = temporary_directory("corrupt-record");
    let request = reservation(50, allowance());
    {
        let mut sqlite = SqliteStorage::open(&directory).expect("storage");
        sqlite
            .enterprise_quota_ledger()
            .expect("quota")
            .reserve(&request)
            .map(allowed)
            .expect("reserve");
    }
    let connection = rusqlite::Connection::open(directory.join("control-plane.sqlite3"))
        .expect("tamper connection");
    connection
        .execute(
            "UPDATE enterprise_quota_reservations
             SET record_json = record_json || ' '",
            [],
        )
        .expect("tamper record");
    drop(connection);
    let mut sqlite = SqliteStorage::open(&directory).expect("restart storage");
    let error = sqlite
        .enterprise_quota_ledger()
        .expect("quota")
        .load_reservation(&request.reservation_id)
        .expect_err("noncanonical record");
    assert_eq!(error.kind(), EnterpriseQuotaErrorKind::CorruptState);
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn one_source_seal_admits_exactly_one_concurrent_reservation() {
    let directory = temporary_directory("source-seal-race");
    let first = reservation(60, allowance());
    let mut second = first.clone();
    second.reservation_id = RequestId(id("req", 61));
    let storages = [
        SqliteStorage::open(&directory).expect("first thread storage"),
        SqliteStorage::open(&directory).expect("second thread storage"),
    ];
    let barrier = Arc::new(Barrier::new(2));
    let handles = [first, second]
        .into_iter()
        .zip(storages)
        .map(|(request, mut storage)| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                storage
                    .enterprise_quota_ledger()
                    .expect("quota")
                    .reserve(&request)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread join"))
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(EnterpriseQuotaDecision::Allowed(_))))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(error) if error.kind() == EnterpriseQuotaErrorKind::ReservationConflict))
            .count(),
        1
    );
    drop(results);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn same_family_usage_cannot_settle_another_sealed_operation() {
    let directory = temporary_directory("cross-source");
    let mut storage = SqliteStorage::open(&directory).expect("storage");
    let second = reservation(71, allowance());
    storage
        .enterprise_quota_ledger()
        .expect("quota")
        .reserve(&second)
        .map(allowed)
        .expect("reserve");
    let first_usage = provider_usage(70);
    storage
        .enterprise_usage_ledger()
        .expect("Usage")
        .record(&first_usage)
        .expect("Usage fact");
    let error = storage
        .enterprise_quota_ledger()
        .expect("quota")
        .settle(&EnterpriseQuotaSettlement {
            reservation_id: second.reservation_id.clone(),
            request_id: RequestId(id("req", 72)),
            expected_revision: 1,
            usage_source: first_usage.source,
        })
        .expect_err("cross operation settlement");
    assert_eq!(error.kind(), EnterpriseQuotaErrorKind::AuthorityMismatch);
    assert_eq!(
        storage
            .enterprise_quota_ledger()
            .expect("quota")
            .load_reservation(&second.reservation_id)
            .expect("load")
            .expect("reservation")
            .state,
        EnterpriseQuotaReservationState::Active
    );
    drop(storage);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn terminal_request_identity_cannot_finish_two_reservations() {
    let directory = temporary_directory("terminal-request");
    let mut storage = SqliteStorage::open(&directory).expect("storage");
    let first = reservation(80, allowance());
    let second = reservation(81, allowance());
    {
        let mut quota = storage.enterprise_quota_ledger().expect("quota");
        allowed(quota.reserve(&first).expect("first"));
        allowed(quota.reserve(&second).expect("second"));
    }
    let terminal_request_id = RequestId(id("req", 82));
    storage
        .enterprise_quota_ledger()
        .expect("quota")
        .release(&EnterpriseQuotaRelease {
            reservation_id: first.reservation_id,
            request_id: terminal_request_id.clone(),
            expected_revision: 1,
            reason: EnterpriseQuotaReleaseReason::Cancelled,
            released_at: Instant("2027-04-01T08:00:01.000Z".to_owned()),
        })
        .expect("first release");
    let error = storage
        .enterprise_quota_ledger()
        .expect("quota")
        .release(&EnterpriseQuotaRelease {
            reservation_id: second.reservation_id.clone(),
            request_id: terminal_request_id,
            expected_revision: 1,
            reason: EnterpriseQuotaReleaseReason::Cancelled,
            released_at: Instant("2027-04-01T08:00:01.000Z".to_owned()),
        })
        .expect_err("request identity reuse");
    assert_eq!(error.kind(), EnterpriseQuotaErrorKind::ReservationConflict);
    assert_eq!(
        storage
            .enterprise_quota_ledger()
            .expect("quota")
            .load_reservation(&second.reservation_id)
            .expect("load")
            .expect("reservation")
            .state,
        EnterpriseQuotaReservationState::Active
    );
    drop(storage);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn release_and_settlement_race_has_one_durable_terminal() {
    let directory = temporary_directory("terminal-race");
    let request = reservation(85, allowance());
    let usage = provider_usage(85);
    {
        let mut storage = SqliteStorage::open(&directory).expect("storage");
        storage
            .enterprise_quota_ledger()
            .expect("quota")
            .reserve(&request)
            .map(allowed)
            .expect("reserve");
        storage
            .enterprise_usage_ledger()
            .expect("Usage")
            .record(&usage)
            .expect("Usage fact");
    }
    let release = EnterpriseQuotaRelease {
        reservation_id: request.reservation_id.clone(),
        request_id: RequestId(id("req", 86)),
        expected_revision: 1,
        reason: EnterpriseQuotaReleaseReason::Cancelled,
        released_at: Instant("2027-04-01T08:00:01.000Z".to_owned()),
    };
    let settlement = EnterpriseQuotaSettlement {
        reservation_id: request.reservation_id.clone(),
        request_id: RequestId(id("req", 87)),
        expected_revision: 1,
        usage_source: usage.source,
    };
    let barrier = Arc::new(Barrier::new(2));
    let release_handle = {
        let directory = directory.clone();
        let barrier = Arc::clone(&barrier);
        let command = release.clone();
        thread::spawn(move || {
            let mut storage = SqliteStorage::open(directory).expect("release storage");
            barrier.wait();
            storage
                .enterprise_quota_ledger()
                .expect("quota")
                .release(&command)
        })
    };
    let settlement_handle = {
        let directory = directory.clone();
        let barrier = Arc::clone(&barrier);
        let command = settlement.clone();
        thread::spawn(move || {
            let mut storage = SqliteStorage::open(directory).expect("settlement storage");
            barrier.wait();
            storage
                .enterprise_quota_ledger()
                .expect("quota")
                .settle(&command)
        })
    };
    let release_result = release_handle.join().expect("release thread");
    let settlement_result = settlement_handle.join().expect("settlement thread");
    assert_ne!(release_result.is_ok(), settlement_result.is_ok());
    let conflict = if release_result.is_err() {
        release_result.expect_err("release conflict")
    } else {
        settlement_result.expect_err("settlement conflict")
    };
    assert_eq!(
        conflict.kind(),
        EnterpriseQuotaErrorKind::ReservationConflict
    );
    let mut storage = SqliteStorage::open(&directory).expect("restart storage");
    let record = storage
        .enterprise_quota_ledger()
        .expect("quota")
        .load_reservation(&request.reservation_id)
        .expect("load")
        .expect("reservation");
    assert_eq!(record.revision, 2);
    assert_ne!(record.state, EnterpriseQuotaReservationState::Active);
    drop(storage);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn usage_rows_are_append_only_and_provider_worker_costs_remain_separate() {
    let directory = temporary_directory("immutable-cost-units");
    let mut storage = SqliteStorage::open(&directory).expect("storage");
    let provider = provider_usage(90);
    let worker = worker_usage(91, 700);
    {
        let mut usage = storage.enterprise_usage_ledger().expect("Usage");
        usage.record(&provider).expect("provider Usage");
        usage.record(&worker).expect("worker Usage");
    }
    let connection = rusqlite::Connection::open(directory.join("control-plane.sqlite3"))
        .expect("direct connection");
    assert!(
        connection
            .execute("UPDATE enterprise_usage_entries SET storage_bytes = 0", [])
            .is_err()
    );
    assert!(
        connection
            .execute("DELETE FROM enterprise_usage_entries", [])
            .is_err()
    );
    drop(connection);
    let mut quota = storage.enterprise_quota_ledger().expect("quota");
    quota
        .put_policy(&policy(
            1,
            EnterpriseQuotaLimits {
                provider_cost_micros: Some(1_000),
                worker_cost_microunits: Some(800),
                ..EnterpriseQuotaLimits::default()
            },
        ))
        .expect("policy");
    allowed(
        quota
            .reserve(&reservation(
                92,
                EnterpriseQuotaAmounts {
                    provider_cost_micros: 100,
                    operations: 1,
                    ..EnterpriseQuotaAmounts::default()
                },
            ))
            .expect("provider cost remains independent"),
    );
    allowed(
        quota
            .reserve(&reservation(
                93,
                EnterpriseQuotaAmounts {
                    worker_cost_microunits: 100,
                    operations: 1,
                    ..EnterpriseQuotaAmounts::default()
                },
            ))
            .expect("worker cost remains independent"),
    );
    drop(storage);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn terminal_source_non_key_drift_is_rejected_after_restart() {
    let directory = temporary_directory("terminal-source-drift");
    let request = reservation(100, allowance());
    let usage = provider_usage(100);
    let mut settlement = EnterpriseQuotaSettlement {
        reservation_id: request.reservation_id.clone(),
        request_id: RequestId(id("req", 101)),
        expected_revision: 1,
        usage_source: usage.source.clone(),
    };
    {
        let mut storage = SqliteStorage::open(&directory).expect("storage");
        storage
            .enterprise_quota_ledger()
            .expect("quota")
            .reserve(&request)
            .map(allowed)
            .expect("reserve");
        storage
            .enterprise_usage_ledger()
            .expect("Usage")
            .record(&usage)
            .expect("Usage fact");
        storage
            .enterprise_quota_ledger()
            .expect("quota")
            .settle(&settlement)
            .expect("settle");
    }
    let connection = rusqlite::Connection::open(directory.join("control-plane.sqlite3"))
        .expect("tamper connection");
    let record_json: String = connection
        .query_row(
            "SELECT record_json FROM enterprise_quota_reservations
             WHERE reservation_id = ?1",
            [&request.reservation_id.0],
            |row| row.get(0),
        )
        .expect("record JSON");
    let mut record: winwincode_storage::EnterpriseQuotaReservationRecord =
        serde_json::from_str(&record_json).expect("record");
    let Some(winwincode_storage::EnterpriseQuotaTerminal::Settled {
        usage_source: EnterpriseUsageSource::Provider {
            source_sequence, ..
        },
        ..
    }) = &mut record.terminal
    else {
        panic!("settled Provider terminal")
    };
    *source_sequence += 1;
    let Some(winwincode_storage::EnterpriseQuotaTerminal::Settled { usage_source, .. }) =
        &record.terminal
    else {
        unreachable!("settled terminal")
    };
    settlement.usage_source = usage_source.clone();
    let changed_record_json = serde_json::to_string(&record).expect("changed record");
    let record_digest = format!(
        "sha256:{:x}",
        Sha256::digest(changed_record_json.as_bytes())
    );
    let command_digest = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&settlement).expect("settlement JSON"))
    );
    connection
        .execute(
            "UPDATE enterprise_quota_reservations SET record_json = ?2
             WHERE reservation_id = ?1",
            rusqlite::params![request.reservation_id.0, changed_record_json],
        )
        .expect("tamper terminal");
    connection
        .execute(
            "UPDATE enterprise_quota_terminal_receipts
             SET record_digest = ?2, command_digest = ?3 WHERE request_id = ?1",
            rusqlite::params![settlement.request_id.0, record_digest, command_digest],
        )
        .expect("tamper receipt seals");
    drop(connection);
    let mut storage = SqliteStorage::open(&directory).expect("restart storage");
    let error = storage
        .enterprise_quota_ledger()
        .expect("quota")
        .settle(&settlement)
        .expect_err("drifted terminal source");
    assert_eq!(error.kind(), EnterpriseQuotaErrorKind::CorruptState);
    drop(storage);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn immutable_usage_recovery_settles_the_matching_active_reservation_once() {
    let directory = temporary_directory("source-recovery-settle");
    let request = reservation(110, allowance());
    let usage = provider_usage(110);
    let terminal = {
        let mut storage = SqliteStorage::open(&directory).expect("storage");
        storage
            .enterprise_quota_ledger()
            .expect("quota")
            .reserve(&request)
            .map(allowed)
            .expect("reserve");
        storage
            .enterprise_usage_ledger()
            .expect("Usage")
            .record(&usage)
            .expect("Usage fact");
        let applied = storage
            .enterprise_quota_ledger()
            .expect("quota")
            .settle_usage_source(&usage.source)
            .expect("recover settlement")
            .expect("matching reservation");
        assert!(!applied.idempotent_replay);
        assert_eq!(
            applied.record.state,
            EnterpriseQuotaReservationState::Settled
        );
        let replay = storage
            .enterprise_quota_ledger()
            .expect("quota")
            .settle_usage_source(&usage.source)
            .expect("same-process replay")
            .expect("matching reservation replay");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.record, applied.record);
        applied.record.terminal
    };
    let mut storage = SqliteStorage::open(&directory).expect("restart storage");
    let restarted = storage
        .enterprise_quota_ledger()
        .expect("quota")
        .settle_usage_source(&usage.source)
        .expect("restart replay")
        .expect("matching reservation after restart");
    assert!(restarted.idempotent_replay);
    assert_eq!(restarted.record.terminal, terminal);
    drop(storage);
    fs::remove_dir_all(directory).expect("cleanup");
}
