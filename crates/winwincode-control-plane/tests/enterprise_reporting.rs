// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{
    EnterpriseReportCurrencyRule, EnterpriseReportDimension, EnterpriseReportErrorKind,
    EnterpriseReportFormat, EnterpriseReportGroup, EnterpriseReportQuery, EnterpriseReportTotals,
    EnterpriseReportingLimits, EnterpriseReportingService,
};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, ExecutionMessageId, Instant, ModelExchangeId,
    OrganizationId, ProductSessionId, ProjectId, PublicationId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactStorageOperationKind, EnterpriseUsageAttribution, EnterpriseUsageFilter,
    EnterpriseUsageMeasure, EnterpriseUsageSource, SettledEnterpriseUsage, SqliteStorage,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let seed = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-enterprise-reporting-{name}-{}-{seed}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
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

fn provider(seed: u64, settled_at: &str) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Provider {
            provider_usage_id: format!("provider-usage-{seed}"),
            source_sequence: seed,
            source_digest: format!("sha256:{:064x}", seed + 10),
            model_exchange_id: ModelExchangeId(id("mdl", seed)),
            request_id: RequestId(id("req", seed)),
            attempt: 1,
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
        settled_at: Instant(settled_at.to_owned()),
    }
}

fn worker(seed: u64, settled_at: &str) -> SettledEnterpriseUsage {
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
        settled_at: Instant(settled_at.to_owned()),
    }
}

fn storage(seed: u64, settled_at: &str, session: bool) -> SettledEnterpriseUsage {
    let mut scope = attribution(seed);
    if !session {
        scope.delivery_id = None;
        scope.product_session_id = None;
    }
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Storage {
            operation_id: ExecutionMessageId(id("xmsg", seed)),
            source_sequence: seed,
            source_digest: Sha256Digest(format!("sha256:{:064x}", seed + 20)),
            artifact_id: ArtifactId(id("art", seed)),
            operation_kind: ArtifactStorageOperationKind::ArtifactFinalize,
            request_id: RequestId(id("req", seed + 200)),
        },
        attribution: scope,
        measure: EnterpriseUsageMeasure::Storage { bytes: 4_096 },
        settled_at: Instant(settled_at.to_owned()),
    }
}

fn publication(seed: u64, settled_at: &str) -> SettledEnterpriseUsage {
    SettledEnterpriseUsage {
        source: EnterpriseUsageSource::Publication {
            publication_id: PublicationId(id("pub", seed)),
            operation_key: format!("github:publication-{seed}:pull-request"),
            request_sha256: format!("sha256:{:064x}", seed + 30),
        },
        attribution: attribution(seed),
        measure: EnterpriseUsageMeasure::Publication,
        settled_at: Instant(settled_at.to_owned()),
    }
}

fn seed_facts(root: &Path, facts: &[SettledEnterpriseUsage]) {
    let mut storage = SqliteStorage::open(root).expect("storage");
    let mut ledger = storage.enterprise_usage_ledger().expect("ledger");
    for fact in facts {
        assert!(!ledger.record(fact).expect("record").idempotent_replay);
    }
}

fn query(group_by: EnterpriseReportDimension) -> EnterpriseReportQuery {
    EnterpriseReportQuery {
        filter: EnterpriseUsageFilter::default(),
        from_inclusive: Instant("2027-03-01T00:00:00.000Z".to_owned()),
        to_exclusive: Instant("2027-03-03T00:00:00.000Z".to_owned()),
        group_by,
    }
}

fn limits() -> EnterpriseReportingLimits {
    EnterpriseReportingLimits::try_new(2, 100, 100, 1024 * 1024).expect("limits")
}

fn fixture_facts() -> Vec<SettledEnterpriseUsage> {
    vec![
        provider(1, "2027-03-01T08:00:00.000Z"),
        worker(1, "2027-03-01T08:01:00.000Z"),
        storage(2, "2027-03-02T09:00:00.000Z", false),
        publication(1, "2027-03-02T09:01:00.000Z"),
        provider(3, "2027-03-03T00:00:00.000Z"),
    ]
}

fn project(
    storage: &mut SqliteStorage,
    group_by: EnterpriseReportDimension,
) -> winwincode_control_plane::EnterpriseReportingProjection {
    EnterpriseReportingService::new(storage, limits())
        .project(&query(group_by))
        .expect("projection")
}

#[test]
fn settled_cost_capacity_and_trends_allocate_and_reconcile_without_currency_inference() {
    let root = temporary_directory("allocation");
    seed_facts(&root, &fixture_facts());
    let mut storage = SqliteStorage::open(&root).expect("storage");

    let by_source = project(&mut storage, EnterpriseReportDimension::SourceKind);
    assert_eq!(by_source.snapshot_sequence, 5);
    assert_eq!(by_source.scanned_entries, 5);
    assert_eq!(by_source.matched_entries, 4);
    assert_eq!(
        by_source.currency_rule,
        EnterpriseReportCurrencyRule::SourceNativeSeparatedNoConversion
    );
    assert_eq!(
        by_source.totals,
        EnterpriseReportTotals {
            entries: 4,
            provider_total_tokens: 130,
            provider_cost_micros: 900,
            worker_runtime_millis: 8_000,
            worker_tokens: 500,
            worker_cost_microunits: 1_200,
            storage_bytes: 4_096,
            storage_operations: 1,
            publication_operations: 1,
        }
    );
    assert_eq!(by_source.rows.len(), 4);

    for dimension in [
        EnterpriseReportDimension::Organization,
        EnterpriseReportDimension::Workspace,
        EnterpriseReportDimension::Project,
        EnterpriseReportDimension::Repository,
        EnterpriseReportDimension::Delivery,
        EnterpriseReportDimension::ProductSession,
        EnterpriseReportDimension::User,
        EnterpriseReportDimension::UtcDay,
    ] {
        let projection = project(&mut storage, dimension);
        assert_eq!(projection.totals, by_source.totals);
        assert_eq!(
            projection
                .rows
                .iter()
                .map(|row| row.totals.entries)
                .sum::<u64>(),
            projection.matched_entries
        );
    }
    let optional = project(&mut storage, EnterpriseReportDimension::ProductSession);
    assert!(optional.rows.iter().any(|row| {
        row.group == EnterpriseReportGroup::ProductSession(None)
            && row.totals.storage_operations == 1
    }));
    let trend = project(&mut storage, EnterpriseReportDimension::UtcDay);
    assert_eq!(trend.rows.len(), 2);

    let service = EnterpriseReportingService::new(&mut storage, limits());
    let json = service
        .export_projection(&by_source, EnterpriseReportFormat::Json)
        .expect("JSON");
    let csv = service
        .export_projection(&by_source, EnterpriseReportFormat::Csv)
        .expect("CSV");
    assert_eq!(
        json,
        service
            .export_projection(&by_source, EnterpriseReportFormat::Json)
            .expect("stable JSON")
    );
    assert!(
        String::from_utf8_lossy(&json.bytes)
            .contains("\"currencyRule\":\"source_native_separated_no_conversion\"")
    );
    assert!(!String::from_utf8_lossy(&json.bytes).contains("totalCost"));
    assert!(String::from_utf8_lossy(&csv.bytes).contains("providerCostMicros"));
    assert!(String::from_utf8_lossy(&csv.bytes).contains("workerCostMicrounits"));
    assert!(csv.bytes.windows(2).any(|window| window == b"\r\n"));

    drop(storage);
    let mut restarted = SqliteStorage::open(&root).expect("restart");
    let rebuilt = project(&mut restarted, EnterpriseReportDimension::SourceKind);
    assert_eq!(rebuilt, by_source);
    assert_eq!(
        EnterpriseReportingService::new(&mut restarted, limits())
            .export_projection(&rebuilt, EnterpriseReportFormat::Json)
            .expect("restart JSON"),
        json
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn fixed_snapshot_pages_survive_restart_exclude_late_rows_and_bind_the_query() {
    let root = temporary_directory("snapshot");
    seed_facts(
        &root,
        &[
            provider(11, "2027-03-01T01:00:00.000Z"),
            worker(12, "2027-03-01T02:00:00.000Z"),
            storage(13, "2027-03-01T03:00:00.000Z", true),
        ],
    );
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let expected = project(&mut storage, EnterpriseReportDimension::User);
    let first = EnterpriseReportingService::new(&mut storage, limits())
        .page(&query(EnterpriseReportDimension::User), None, 1)
        .expect("first page");
    assert_eq!(first.snapshot_sequence, 3);
    assert_eq!(first.entries.len(), 1);
    let mut reconciled = first.totals;
    let cursor = first.next.expect("cursor");

    storage
        .enterprise_usage_ledger()
        .expect("ledger")
        .record(&publication(14, "2027-03-01T04:00:00.000Z"))
        .expect("late fact");
    drop(storage);

    let (second_json, second_entries) = {
        let mut restarted = SqliteStorage::open(&root).expect("restart");
        let mut service = EnterpriseReportingService::new(&mut restarted, limits());
        let second = service
            .page(&query(EnterpriseReportDimension::User), Some(&cursor), 1)
            .expect("second page");
        assert_eq!(second.snapshot_sequence, 3);
        assert_eq!(second.entries.len(), 1);
        let second_json = service
            .export_page(&second, EnterpriseReportFormat::Json)
            .expect("second JSON");
        let second_cursor = second.next.as_ref().expect("second cursor");
        let third = service
            .page(
                &query(EnterpriseReportDimension::User),
                Some(second_cursor),
                1,
            )
            .expect("third page");
        assert_eq!(third.snapshot_sequence, 3);
        assert_eq!(third.entries.len(), 1);
        assert!(third.next.is_none());
        reconciled
            .checked_merge(second.totals)
            .expect("second totals");
        reconciled
            .checked_merge(third.totals)
            .expect("third totals");

        let mut changed = query(EnterpriseReportDimension::User);
        changed.from_inclusive = Instant("2027-03-01T00:00:01.000Z".to_owned());
        assert_eq!(
            service
                .page(&changed, Some(&cursor), 1)
                .expect_err("changed query")
                .kind(),
            EnterpriseReportErrorKind::InvalidInput
        );
        (second_json, second.entries)
    };
    assert_eq!(reconciled, expected.totals);

    {
        let mut restarted_again = SqliteStorage::open(&root).expect("second restart");
        let mut service = EnterpriseReportingService::new(&mut restarted_again, limits());
        let replay = service
            .page(&query(EnterpriseReportDimension::User), Some(&cursor), 1)
            .expect("page replay");
        assert_eq!(replay.entries, second_entries);
        assert_eq!(
            service
                .export_page(&replay, EnterpriseReportFormat::Json)
                .expect("replay JSON"),
            second_json
        );
    }
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn time_group_and_export_bounds_fail_closed_before_unbounded_reports() {
    let root = temporary_directory("bounds");
    seed_facts(
        &root,
        &[
            provider(21, "2027-03-01T01:00:00.000Z"),
            worker(22, "2027-03-01T02:00:00.000Z"),
            storage(23, "2027-03-01T03:00:00.000Z", true),
        ],
    );
    let mut storage = SqliteStorage::open(&root).expect("storage");

    let scan_limits = EnterpriseReportingLimits::try_new(1, 2, 10, 1_000).expect("limits");
    assert_eq!(
        EnterpriseReportingService::new(&mut storage, scan_limits)
            .project(&query(EnterpriseReportDimension::User))
            .expect_err("scan bound")
            .kind(),
        EnterpriseReportErrorKind::LimitExceeded
    );
    let group_limits = EnterpriseReportingLimits::try_new(2, 10, 1, 1_000).expect("limits");
    assert_eq!(
        EnterpriseReportingService::new(&mut storage, group_limits)
            .project(&query(EnterpriseReportDimension::SourceKind))
            .expect_err("group bound")
            .kind(),
        EnterpriseReportErrorKind::LimitExceeded
    );

    let projection = project(&mut storage, EnterpriseReportDimension::User);
    let export_limits = EnterpriseReportingLimits::try_new(2, 10, 10, 32).expect("limits");
    for format in [EnterpriseReportFormat::Json, EnterpriseReportFormat::Csv] {
        assert_eq!(
            EnterpriseReportingService::new(&mut storage, export_limits)
                .export_projection(&projection, format)
                .expect_err("export bound")
                .kind(),
            EnterpriseReportErrorKind::LimitExceeded
        );
    }

    for (from, to) in [
        ("2027-03-01T00:00:00.000Z", "2027-03-01T00:00:00.000Z"),
        ("2027-02-29T00:00:00.000Z", "2027-03-01T00:00:00.000Z"),
        ("2027-03-01T24:00:00.000Z", "2027-03-02T00:00:00.000Z"),
    ] {
        let invalid = EnterpriseReportQuery {
            filter: EnterpriseUsageFilter::default(),
            from_inclusive: Instant(from.to_owned()),
            to_exclusive: Instant(to.to_owned()),
            group_by: EnterpriseReportDimension::UtcDay,
        };
        assert_eq!(
            EnterpriseReportingService::new(&mut storage, limits())
                .project(&invalid)
                .expect_err("invalid interval")
                .kind(),
            EnterpriseReportErrorKind::InvalidInput
        );
    }

    let invalid_scope = EnterpriseReportQuery {
        filter: EnterpriseUsageFilter {
            organization_id: Some(OrganizationId("wrong_organization".to_owned())),
            ..EnterpriseUsageFilter::default()
        },
        ..query(EnterpriseReportDimension::Organization)
    };
    assert_eq!(
        EnterpriseReportingService::new(&mut storage, limits())
            .project(&invalid_scope)
            .expect_err("invalid scope filter")
            .kind(),
        EnterpriseReportErrorKind::InvalidInput
    );

    storage
        .enterprise_usage_ledger()
        .expect("ledger")
        .record(&provider(24, "2027-02-29T01:00:00.000Z"))
        .expect("shape-valid legacy instant");
    let corrupt_window = EnterpriseReportQuery {
        filter: EnterpriseUsageFilter::default(),
        from_inclusive: Instant("2027-02-01T00:00:00.000Z".to_owned()),
        to_exclusive: Instant("2027-03-02T00:00:00.000Z".to_owned()),
        group_by: EnterpriseReportDimension::UtcDay,
    };
    assert_eq!(
        EnterpriseReportingService::new(&mut storage, limits())
            .project(&corrupt_window)
            .expect_err("invalid durable Gregorian date")
            .kind(),
        EnterpriseReportErrorKind::Ledger
    );

    drop(storage);
    fs::remove_dir_all(root).expect("cleanup");
}
