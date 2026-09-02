use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, ExecutionMessageId, Instant, ModelExchangeId,
    OrganizationId, ProductSessionId, ProjectId, PublicationId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactStorageOperationKind, EnterpriseUsageAttribution, EnterpriseUsageErrorKind,
    EnterpriseUsageFilter, EnterpriseUsageMeasure, EnterpriseUsageSource,
    EnterpriseUsageSourceKind, SettledEnterpriseUsage, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-usage-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn attribution(seed: u64) -> EnterpriseUsageAttribution {
    EnterpriseUsageAttribution {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
        delivery_id: Some(DeliveryId(id("dlv", seed))),
        product_session_id: Some(ProductSessionId(id("psn", seed))),
        user_id: UserId(id("usr", seed)),
    }
}

fn provider(seed: u64, attempt: u64) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Provider {
            provider_usage_id: format!("provider-usage-{seed}"),
            source_sequence: seed,
            source_digest: format!("sha256:{:064x}", seed + 1),
            model_exchange_id: ModelExchangeId(id("mdl", seed)),
            request_id: RequestId(id("req", seed)),
            attempt,
            route_authority_fingerprint: format!("sha256:{seed:064x}"),
        },
        attribution: attribution(seed),
        measure: EnterpriseUsageMeasure::Provider {
            input_tokens: 100,
            cached_input_tokens: 20,
            cache_write_input_tokens: 5,
            output_tokens: 30,
            reasoning_output_tokens: 10,
            total_tokens: 130,
            cost_micros: 900,
        },
        settled_at: Instant("2027-03-01T08:00:00.000Z".to_owned()),
    }
}

fn worker(seed: u64) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Worker {
            job_id: ExecutionJobId(id("job", seed)),
            settlement_request_id: RequestId(id("req", seed + 100)),
            worker_pool_id: id("wpl", seed),
        },
        attribution: attribution(seed),
        measure: EnterpriseUsageMeasure::Worker {
            runtime_millis: 8_000,
            tokens: 500,
            cost_microunits: 1_200,
        },
        settled_at: Instant("2027-03-01T08:01:00.000Z".to_owned()),
    }
}

fn storage(seed: u64) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Storage {
            operation_id: ExecutionMessageId(id("xmsg", seed)),
            source_sequence: seed,
            source_digest: Sha256Digest(format!("sha256:{:064x}", seed + 20)),
            artifact_id: ArtifactId(id("art", seed)),
            operation_kind: ArtifactStorageOperationKind::ArtifactFinalize,
            request_id: RequestId(id("req", seed + 200)),
        },
        attribution: attribution(seed),
        measure: EnterpriseUsageMeasure::Storage { bytes: 4_096 },
        settled_at: Instant("2027-03-01T08:02:00.000Z".to_owned()),
    }
}

fn publication(seed: u64) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Publication {
            publication_id: PublicationId(id("pub", seed)),
            operation_key: format!("github:publication-{seed}:pull-request"),
            request_sha256: format!("sha256:{:064x}", seed + 10),
        },
        attribution: attribution(seed),
        measure: EnterpriseUsageMeasure::Publication,
        settled_at: Instant("2027-03-01T08:03:00.000Z".to_owned()),
    }
}

#[test]
fn records_closed_settlements_and_reconciles_every_business_dimension() {
    let directory = temporary_directory("dimensions");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let mut ledger = sqlite.enterprise_usage_ledger().expect("ledger");
    for fact in [provider(1, 1), worker(1), storage(1), publication(1)] {
        assert!(!ledger.record(&fact).expect("record").idempotent_replay);
    }

    let filter = EnterpriseUsageFilter {
        organization_id: Some(attribution(1).organization_id),
        workspace_id: Some(attribution(1).workspace_id),
        project_id: Some(attribution(1).project_id),
        repository_id: Some(attribution(1).repository_id),
        delivery_id: attribution(1).delivery_id,
        product_session_id: attribution(1).product_session_id,
        user_id: Some(attribution(1).user_id),
        source_kind: None,
    };
    let totals = ledger.reconcile(&filter).expect("totals");
    assert_eq!(totals.entries, 4);
    assert_eq!(totals.provider_total_tokens, 130);
    assert_eq!(totals.provider_cost_micros, 900);
    assert_eq!(totals.worker_runtime_millis, 8_000);
    assert_eq!(totals.worker_tokens, 500);
    assert_eq!(totals.worker_cost_microunits, 1_200);
    assert_eq!(totals.storage_bytes, 4_096);
    assert_eq!(totals.storage_operations, 1);
    assert_eq!(totals.publication_operations, 1);
    assert_eq!(
        ledger.scan(&filter, None, 10).expect("page").entries.len(),
        4
    );
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn source_receipt_replay_is_free_and_changed_reuse_is_rejected() {
    let directory = temporary_directory("dedup");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let mut ledger = sqlite.enterprise_usage_ledger().expect("ledger");
    let primary = provider(2, 1);
    let first = ledger.record(&primary).expect("first");
    let replay = ledger.record(&primary).expect("replay");
    assert!(!first.idempotent_replay);
    assert!(replay.idempotent_replay);
    assert_eq!(replay.entry, first.entry);

    let mut changed = primary.clone();
    changed.measure = EnterpriseUsageMeasure::Provider {
        input_tokens: 101,
        cached_input_tokens: 20,
        cache_write_input_tokens: 5,
        output_tokens: 30,
        reasoning_output_tokens: 10,
        total_tokens: 131,
        cost_micros: 901,
    };
    assert_eq!(
        ledger.record(&changed).expect_err("changed source").kind(),
        EnterpriseUsageErrorKind::SourceConflict
    );
    assert_eq!(
        ledger
            .reconcile(&EnterpriseUsageFilter::default())
            .expect("totals")
            .entries,
        1
    );
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn provider_token_subtotals_are_bounded_by_their_canonical_totals() {
    let directory = temporary_directory("provider-subtotals");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let mut ledger = sqlite.enterprise_usage_ledger().expect("ledger");
    let invalid_measures = [
        EnterpriseUsageMeasure::Provider {
            input_tokens: 100,
            cached_input_tokens: 101,
            cache_write_input_tokens: 5,
            output_tokens: 30,
            reasoning_output_tokens: 10,
            total_tokens: 130,
            cost_micros: 900,
        },
        EnterpriseUsageMeasure::Provider {
            input_tokens: 100,
            cached_input_tokens: 20,
            cache_write_input_tokens: 101,
            output_tokens: 30,
            reasoning_output_tokens: 10,
            total_tokens: 130,
            cost_micros: 900,
        },
        EnterpriseUsageMeasure::Provider {
            input_tokens: 100,
            cached_input_tokens: 20,
            cache_write_input_tokens: 5,
            output_tokens: 30,
            reasoning_output_tokens: 31,
            total_tokens: 130,
            cost_micros: 900,
        },
    ];
    for (offset, measure) in invalid_measures.into_iter().enumerate() {
        let mut fact = provider(50 + offset as u64, 1);
        fact.measure = measure;
        assert_eq!(
            ledger.record(&fact).expect_err("invalid subtotal").kind(),
            EnterpriseUsageErrorKind::InvalidInput
        );
    }
    assert_eq!(
        ledger
            .reconcile(&EnterpriseUsageFilter::default())
            .expect("empty ledger")
            .entries,
        0
    );
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn storage_usage_without_a_product_session_survives_restart_and_exact_filters() {
    let directory = temporary_directory("storage-without-session");
    let mut fact = storage(60);
    fact.attribution.product_session_id = None;
    let original = {
        let mut sqlite = SqliteStorage::open(&directory).expect("storage");
        sqlite
            .enterprise_usage_ledger()
            .expect("ledger")
            .record(&fact)
            .expect("storage fact")
            .entry
    };
    let mut sqlite = SqliteStorage::open(&directory).expect("restart storage");
    let replay = sqlite
        .enterprise_usage_ledger()
        .expect("restart ledger")
        .record(&fact)
        .expect("exact replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.entry, original);
    let matching = EnterpriseUsageFilter {
        source_kind: Some(EnterpriseUsageSourceKind::Storage),
        ..EnterpriseUsageFilter::default()
    };
    assert_eq!(
        sqlite
            .enterprise_usage_ledger()
            .expect("scan ledger")
            .scan(&matching, None, 10)
            .expect("storage page")
            .entries,
        vec![original]
    );
    let session_filter = EnterpriseUsageFilter {
        product_session_id: Some(ProductSessionId(id("psn", 60))),
        ..matching
    };
    assert!(
        sqlite
            .enterprise_usage_ledger()
            .expect("filtered ledger")
            .scan(&session_filter, None, 10)
            .expect("filtered page")
            .entries
            .is_empty()
    );
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn charged_retry_and_fallback_entries_are_distinct_but_each_replay_once() {
    let directory = temporary_directory("retry-fallback");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let mut ledger = sqlite.enterprise_usage_ledger().expect("ledger");
    let first_attempt = provider(3, 1);
    let mut fallback = provider(4, 2);
    fallback.attribution = first_attempt.attribution.clone();
    for fact in [&first_attempt, &fallback, &first_attempt, &fallback] {
        ledger.record(fact).expect("settled charge");
    }
    let filter = EnterpriseUsageFilter {
        source_kind: Some(EnterpriseUsageSourceKind::Provider),
        ..EnterpriseUsageFilter::default()
    };
    let totals = ledger.reconcile(&filter).expect("provider totals");
    assert_eq!(totals.entries, 2);
    assert_eq!(totals.provider_total_tokens, 260);
    assert_eq!(totals.provider_cost_micros, 1_800);
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn scan_cursor_holds_a_fixed_snapshot_across_new_settlements() {
    let directory = temporary_directory("snapshot");
    let mut sqlite = SqliteStorage::open(&directory).expect("storage");
    let mut ledger = sqlite.enterprise_usage_ledger().expect("ledger");
    for seed in 10..13 {
        ledger.record(&provider(seed, 1)).expect("initial fact");
    }
    let filter = EnterpriseUsageFilter::default();
    let first = ledger.scan(&filter, None, 2).expect("first page");
    assert_eq!(first.entries.len(), 2);
    let cursor = first.next.expect("next cursor");
    ledger
        .record(&provider(13, 1))
        .expect("concurrent later fact");
    let second = ledger.scan(&filter, Some(&cursor), 2).expect("second page");
    assert_eq!(second.entries.len(), 1);
    assert!(second.next.is_none());
    assert_eq!(second.snapshot_sequence, 3);
    assert_eq!(
        ledger
            .scan(&filter, None, 10)
            .expect("new snapshot")
            .entries
            .len(),
        4
    );
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn restart_replays_the_original_entry_bytes_and_sequence() {
    let directory = temporary_directory("restart");
    let fact = worker(20);
    let original = {
        let mut sqlite = SqliteStorage::open(&directory).expect("storage");
        sqlite
            .enterprise_usage_ledger()
            .expect("ledger")
            .record(&fact)
            .expect("record")
            .entry
    };
    let replay = {
        let mut sqlite = SqliteStorage::open(&directory).expect("restart storage");
        sqlite
            .enterprise_usage_ledger()
            .expect("restart ledger")
            .record(&fact)
            .expect("replay")
    };
    assert!(replay.idempotent_replay);
    assert_eq!(replay.entry, original);
    assert_eq!(
        serde_json::to_vec(&replay.entry).expect("replay bytes"),
        serde_json::to_vec(&original).expect("original bytes")
    );
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn concurrent_same_source_commits_one_entry() {
    let directory = temporary_directory("concurrent");
    drop(SqliteStorage::open(&directory).expect("initialize storage"));
    let barrier = Arc::new(Barrier::new(4));
    let fact = provider(30, 1);
    let handles = (0..4)
        .map(|_| {
            let directory = directory.clone();
            let barrier = Arc::clone(&barrier);
            let fact = fact.clone();
            thread::spawn(move || {
                let mut sqlite = SqliteStorage::open(directory).expect("thread storage");
                let mut ledger = sqlite.enterprise_usage_ledger().expect("thread ledger");
                barrier.wait();
                ledger.record(&fact).expect("concurrent record")
            })
        })
        .collect::<Vec<_>>();
    let receipts = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread join"))
        .collect::<Vec<_>>();
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| !receipt.idempotent_replay)
            .count(),
        1
    );
    assert!(
        receipts
            .windows(2)
            .all(|pair| pair[0].entry == pair[1].entry)
    );
    let mut sqlite = SqliteStorage::open(&directory).expect("verify storage");
    assert_eq!(
        sqlite
            .enterprise_usage_ledger()
            .expect("verify ledger")
            .reconcile(&EnterpriseUsageFilter::default())
            .expect("verify totals")
            .entries,
        1
    );
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}

#[test]
fn corrupt_scope_column_is_detected_instead_of_reattributed() {
    let directory = temporary_directory("corrupt");
    let source = storage(40);
    {
        let mut sqlite = SqliteStorage::open(&directory).expect("storage");
        sqlite
            .enterprise_usage_ledger()
            .expect("ledger")
            .record(&source)
            .expect("record");
    }
    let database = directory.join("control-plane.sqlite3");
    let connection = rusqlite::Connection::open(database).expect("tamper connection");
    connection
        .execute("DROP TRIGGER enterprise_usage_entries_no_update", [])
        .expect("simulate bypass of immutable trigger");
    connection
        .execute(
            "UPDATE enterprise_usage_entries SET user_id = ?1",
            [id("usr", 41)],
        )
        .expect("tamper row");
    drop(connection);
    let mut sqlite = SqliteStorage::open(&directory).expect("restart storage");
    let error = sqlite
        .enterprise_usage_ledger()
        .expect("ledger")
        .load_source(&source.source)
        .expect_err("corrupt row");
    assert_eq!(error.kind(), EnterpriseUsageErrorKind::CorruptState);
    drop(sqlite);
    fs::remove_dir_all(directory).expect("cleanup");
}
