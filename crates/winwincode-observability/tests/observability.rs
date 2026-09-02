// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use winwincode_observability::{
    AlertCondition, AlertRule, AlertRuleId, AlertSeverity, AlertStatus, CapacityResource,
    Component, DiagnosticCode, FactDigest, LogSeverity, MetricSeriesKey, ObservabilityConfig,
    ObservabilityErrorKind, Observation, ObservationId, ObservationSignal, ObservationSource,
    ObservationSourceKind, Operation, Outcome, SourceFactId, SqliteObservability, TraceContext,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-observability-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn database(&self) -> PathBuf {
        self.0.join("telemetry.sqlite")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn digest(material: &str) -> FactDigest {
    FactDigest::try_new(format!("sha256:{:x}", Sha256::digest(material.as_bytes())))
        .expect("fact digest")
}

fn observation_id(seed: u64) -> ObservationId {
    ObservationId::try_new(format!("obs_{seed:026}")).expect("observation id")
}

fn source(seed: u64, kind: ObservationSourceKind) -> ObservationSource {
    ObservationSource {
        kind,
        fact_id: SourceFactId::try_new(format!("settled/source/{seed}")).expect("source fact id"),
        fact_digest: digest(&format!("source-fact-{seed}")),
    }
}

fn observation(
    seed: u64,
    source_kind: ObservationSourceKind,
    component: Component,
    operation: Operation,
    occurred_at_unix_millis: u64,
    signal: ObservationSignal,
) -> Observation {
    let correlation = digest(&format!("correlation-{}", seed % 3));
    Observation {
        observation_id: observation_id(seed),
        source: source(seed, source_kind),
        trace: TraceContext::derive(&correlation, component, operation, seed, None)
            .expect("trace context"),
        component,
        operation,
        occurred_at_unix_millis,
        signal,
    }
}

fn rules() -> Vec<AlertRule> {
    vec![
        AlertRule {
            rule_id: AlertRuleId::try_new("http-latency").expect("rule id"),
            severity: AlertSeverity::Warning,
            condition: AlertCondition::LatencyAtLeast {
                component: Component::Http,
                operation: Operation::HttpRequest,
                threshold_millis: 500,
            },
        },
        AlertRule {
            rule_id: AlertRuleId::try_new("provider-error").expect("rule id"),
            severity: AlertSeverity::Critical,
            condition: AlertCondition::OutcomeEquals {
                component: Component::Provider,
                operation: Operation::ProviderOpen,
                outcome: Outcome::ServerError,
            },
        },
        AlertRule {
            rule_id: AlertRuleId::try_new("queue-capacity").expect("rule id"),
            severity: AlertSeverity::Warning,
            condition: AlertCondition::CapacityRatioAtLeast {
                component: Component::Queue,
                resource: CapacityResource::QueueDepth,
                numerator: 4,
                denominator: 5,
            },
        },
        AlertRule {
            rule_id: AlertRuleId::try_new("worker-recovery").expect("rule id"),
            severity: AlertSeverity::Critical,
            condition: AlertCondition::RecoveryFailed {
                component: Component::Worker,
                operation: Operation::WorkerRecovery,
            },
        },
    ]
}

fn config() -> ObservabilityConfig {
    ObservabilityConfig {
        bucket_width_millis: 60_000,
        max_receipts: 1_000,
        max_trace_rows: 100,
        max_metric_rows: 100,
        max_query_rows: 100,
        max_query_buckets: 60,
        alert_rules: rules(),
    }
}

fn all_component_observations() -> Vec<Observation> {
    vec![
        observation(
            1,
            ObservationSourceKind::AuditLedger,
            Component::Http,
            Operation::HttpRequest,
            60_001,
            ObservationSignal::OperationCompleted {
                outcome: Outcome::Succeeded,
                latency_millis: 25,
            },
        ),
        observation(
            2,
            ObservationSourceKind::InternalOperation,
            Component::WebSocket,
            Operation::WebSocketConnect,
            60_002,
            ObservationSignal::CapacityObserved {
                resource: CapacityResource::WebSocketConnections,
                used: 4,
                limit: 100,
            },
        ),
        observation(
            3,
            ObservationSourceKind::InternalOperation,
            Component::Scheduler,
            Operation::SchedulerTick,
            60_003,
            ObservationSignal::StructuredLog {
                severity: LogSeverity::Warning,
                code: DiagnosticCode::SchedulerStalled,
            },
        ),
        observation(
            4,
            ObservationSourceKind::RuntimeEvent,
            Component::Worker,
            Operation::WorkerRecovery,
            60_004,
            ObservationSignal::RecoveryObserved {
                outcome: Outcome::Recovered,
                latency_millis: 80,
                recovered_items: 2,
            },
        ),
        observation(
            5,
            ObservationSourceKind::UsageLedger,
            Component::Provider,
            Operation::ProviderSettlement,
            60_005,
            ObservationSignal::OperationCompleted {
                outcome: Outcome::Succeeded,
                latency_millis: 120,
            },
        ),
        observation(
            6,
            ObservationSourceKind::InternalOperation,
            Component::Storage,
            Operation::StorageWrite,
            60_006,
            ObservationSignal::CapacityObserved {
                resource: CapacityResource::StorageBusyWriters,
                used: 1,
                limit: 8,
            },
        ),
        observation(
            7,
            ObservationSourceKind::InternalOperation,
            Component::Queue,
            Operation::QueueEnqueue,
            60_007,
            ObservationSignal::CapacityObserved {
                resource: CapacityResource::QueueDepth,
                used: 2,
                limit: 100,
            },
        ),
    ]
}

#[test]
fn structured_signals_cover_every_runtime_boundary_without_unbounded_labels_or_secrets() {
    let directory = TestDirectory::new("structured");
    let mut service =
        SqliteObservability::open(directory.database(), config()).expect("open observability");
    for fact in all_component_observations() {
        let receipt = service.record(&fact).expect("record structured fact");
        assert!(!receipt.duplicate);
    }
    for seed in 20..30 {
        service
            .record(&observation(
                seed,
                ObservationSourceKind::UsageLedger,
                Component::Provider,
                Operation::ProviderSettlement,
                60_000 + seed,
                ObservationSignal::OperationCompleted {
                    outcome: Outcome::Succeeded,
                    latency_millis: seed,
                },
            ))
            .expect("record correlated Provider fact");
    }

    let metrics = service
        .metric_page(60_000, 120_000, None, 100)
        .expect("bounded metric page");
    assert_eq!(metrics.rows.len(), 7);
    let provider = metrics
        .rows
        .iter()
        .find(|row| {
            row.key
                == MetricSeriesKey::Operation {
                    component: Component::Provider,
                    operation: Operation::ProviderSettlement,
                    outcome: Outcome::Succeeded,
                }
        })
        .expect("Provider metric series");
    assert_eq!(provider.observations, 11);
    assert_eq!(provider.latency_total_millis, 120 + (20..30).sum::<u64>());
    assert_eq!(provider.latency_max_millis, 120);

    let serialized = serde_json::to_string(&metrics.rows).expect("serialize metrics");
    for forbidden in [
        "prompt",
        "credential",
        "authorization",
        "requestBody",
        "message",
    ] {
        assert!(!serialized.contains(forbidden));
    }
    assert_eq!(
        SourceFactId::try_new("authorization:Bearer-private")
            .expect_err("secret-shaped source")
            .kind(),
        ObservabilityErrorKind::InvalidInput
    );
}

fn firing_observations() -> Vec<Observation> {
    vec![
        observation(
            100,
            ObservationSourceKind::AuditLedger,
            Component::Http,
            Operation::HttpRequest,
            120_100,
            ObservationSignal::OperationCompleted {
                outcome: Outcome::Succeeded,
                latency_millis: 700,
            },
        ),
        observation(
            101,
            ObservationSourceKind::UsageLedger,
            Component::Provider,
            Operation::ProviderOpen,
            120_101,
            ObservationSignal::OperationCompleted {
                outcome: Outcome::ServerError,
                latency_millis: 10,
            },
        ),
        observation(
            102,
            ObservationSourceKind::InternalOperation,
            Component::Queue,
            Operation::QueueEnqueue,
            120_102,
            ObservationSignal::CapacityObserved {
                resource: CapacityResource::QueueDepth,
                used: 80,
                limit: 100,
            },
        ),
        observation(
            103,
            ObservationSourceKind::RuntimeEvent,
            Component::Worker,
            Operation::WorkerRecovery,
            120_103,
            ObservationSignal::RecoveryObserved {
                outcome: Outcome::Failed,
                latency_millis: 300,
                recovered_items: 0,
            },
        ),
    ]
}

fn resolving_observations() -> Vec<Observation> {
    vec![
        observation(
            110,
            ObservationSourceKind::AuditLedger,
            Component::Http,
            Operation::HttpRequest,
            120_110,
            ObservationSignal::OperationCompleted {
                outcome: Outcome::Succeeded,
                latency_millis: 20,
            },
        ),
        observation(
            111,
            ObservationSourceKind::UsageLedger,
            Component::Provider,
            Operation::ProviderOpen,
            120_111,
            ObservationSignal::OperationCompleted {
                outcome: Outcome::Succeeded,
                latency_millis: 10,
            },
        ),
        observation(
            112,
            ObservationSourceKind::InternalOperation,
            Component::Queue,
            Operation::QueueEnqueue,
            120_112,
            ObservationSignal::CapacityObserved {
                resource: CapacityResource::QueueDepth,
                used: 1,
                limit: 100,
            },
        ),
        observation(
            113,
            ObservationSourceKind::RuntimeEvent,
            Component::Worker,
            Operation::WorkerRecovery,
            120_113,
            ObservationSignal::RecoveryObserved {
                outcome: Outcome::Recovered,
                latency_millis: 20,
                recovered_items: 4,
            },
        ),
    ]
}

fn record_initial_alerts(
    path: &Path,
    firing: &[Observation],
) -> Vec<winwincode_observability::AlertTransition> {
    let mut service = SqliteObservability::open(path, config()).expect("open observability");
    let mut transitions = Vec::new();
    for fact in firing {
        transitions.extend(service.record(fact).expect("fire alert").alert_transitions);
    }
    assert_eq!(transitions.len(), 4);
    let duplicate = service.record(&firing[0]).expect("exact replay");
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.alert_transitions, vec![transitions[0].clone()]);

    let repeated = observation(
        104,
        ObservationSourceKind::AuditLedger,
        Component::Http,
        Operation::HttpRequest,
        120_104,
        ObservationSignal::OperationCompleted {
            outcome: Outcome::Succeeded,
            latency_millis: 900,
        },
    );
    assert!(
        service
            .record(&repeated)
            .expect("same active alert")
            .alert_transitions
            .is_empty()
    );
    transitions
}

fn assert_changed_durable_config_is_rejected(path: &Path) {
    let mut changed_rules = config();
    changed_rules.alert_rules[0].severity = AlertSeverity::Critical;
    assert_eq!(
        SqliteObservability::open(path, changed_rules)
            .err()
            .expect("changed durable rules")
            .kind(),
        ObservabilityErrorKind::RuleSetChanged
    );
    let mut changed_bucket = config();
    changed_bucket.bucket_width_millis = 30_000;
    assert_eq!(
        SqliteObservability::open(path, changed_bucket)
            .err()
            .expect("changed durable bucket width")
            .kind(),
        ObservabilityErrorKind::ConfigurationChanged
    );
}

#[test]
fn trace_and_alert_identity_survive_restart_while_replay_and_active_alerts_deduplicate() {
    let directory = TestDirectory::new("alerts");
    let path = directory.database();
    let firing = firing_observations();
    let expected_trace = TraceContext::derive(
        &digest("correlation-1"),
        Component::Http,
        Operation::HttpRequest,
        100,
        None,
    )
    .expect("derived trace");
    assert_eq!(firing[0].trace, expected_trace);
    let first_transitions = record_initial_alerts(&path, &firing);
    let fixture = format!(
        "{}\n",
        serde_json::to_string_pretty(&first_transitions).expect("serialize alert fixture")
    );
    assert_eq!(fixture, include_str!("fixtures/alert-transitions.v1.json"));

    let mut service = SqliteObservability::open(&path, config()).expect("restart observability");
    let trace_page = service
        .trace_page(&expected_trace.trace_id, 0, 10)
        .expect("restart trace query");
    assert_eq!(trace_page.rows[0].observation.trace, expected_trace);
    let mut resolutions = Vec::new();
    for fact in resolving_observations() {
        resolutions.extend(
            service
                .record(&fact)
                .expect("resolve alert")
                .alert_transitions,
        );
    }
    assert_eq!(resolutions.len(), 4);
    for resolution in &resolutions {
        assert_eq!(resolution.status, AlertStatus::Resolved);
        let firing = first_transitions
            .iter()
            .find(|transition| transition.rule_id == resolution.rule_id)
            .expect("matching firing alert");
        assert_eq!(resolution.alert_id, firing.alert_id);
        assert_eq!(resolution.generation, firing.generation);
    }

    let refire = observation(
        120,
        ObservationSourceKind::AuditLedger,
        Component::Http,
        Operation::HttpRequest,
        120_120,
        ObservationSignal::OperationCompleted {
            outcome: Outcome::Succeeded,
            latency_millis: 800,
        },
    );
    let refired = service.record(&refire).expect("refire alert");
    assert_eq!(refired.alert_transitions.len(), 1);
    assert_eq!(refired.alert_transitions[0].generation, 2);
    assert_ne!(
        refired.alert_transitions[0].alert_id,
        first_transitions[0].alert_id
    );
    let alert_page = service.alert_page(0, 100).expect("bounded alert page");
    assert_eq!(alert_page.transitions.len(), 9);
    assert_eq!(alert_page.next_after_sequence, None);

    let changed = Observation {
        signal: ObservationSignal::OperationCompleted {
            outcome: Outcome::Succeeded,
            latency_millis: 801,
        },
        ..refire.clone()
    };
    assert_eq!(
        service
            .record(&changed)
            .expect_err("changed observation replay")
            .kind(),
        ObservabilityErrorKind::Conflict
    );
    let aliased_source = Observation {
        observation_id: observation_id(121),
        ..refire
    };
    assert_eq!(
        service
            .record(&aliased_source)
            .expect_err("source fact cannot be counted twice")
            .kind(),
        ObservabilityErrorKind::Conflict
    );
    drop(service);

    assert_changed_durable_config_is_rejected(&path);
}

#[test]
fn alert_transition_failure_rolls_back_receipt_metric_trace_and_state_before_restart() {
    let directory = TestDirectory::new("alert-rollback");
    let path = directory.database();
    drop(SqliteObservability::open(&path, config()).expect("initialize observability"));
    let connection = Connection::open(&path).expect("open fault injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_alert_transition
             BEFORE INSERT ON alert_transitions
             BEGIN
                 SELECT RAISE(ABORT, 'fixture fault');
             END;",
        )
        .expect("install alert transition fault");
    drop(connection);

    let firing = firing_observations().remove(0);
    let mut service = SqliteObservability::open(&path, config()).expect("open faulted service");
    assert_eq!(
        service
            .record(&firing)
            .expect_err("alert transition fault")
            .kind(),
        ObservabilityErrorKind::Storage
    );
    drop(service);
    let connection = Connection::open(&path).expect("inspect rolled back store");
    for table in [
        "observation_receipts",
        "observation_log",
        "metric_series",
        "alert_states",
        "alert_transitions",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count rolled back table");
        assert_eq!(count, 0, "{table}");
    }
    connection
        .execute_batch("DROP TRIGGER fail_alert_transition")
        .expect("remove fault");
    drop(connection);

    let mut restarted = SqliteObservability::open(&path, config()).expect("restart after fault");
    let receipt = restarted.record(&firing).expect("retry after restart");
    assert_eq!(receipt.accepted_sequence, 1);
    assert_eq!(receipt.alert_transitions.len(), 1);
}

fn record_provider_streams(service: &mut SqliteObservability, first_seed: u64, count: u64) {
    for offset in 0..count {
        let seed = first_seed + offset;
        service
            .record(&observation(
                seed,
                ObservationSourceKind::UsageLedger,
                Component::Provider,
                Operation::ProviderStream,
                seed * 60_000,
                ObservationSignal::OperationCompleted {
                    outcome: Outcome::Succeeded,
                    latency_millis: 10,
                },
            ))
            .expect("record Provider stream");
    }
}

fn assert_bounded_queries(service: &SqliteObservability) {
    let latest_trace = observation(
        203,
        ObservationSourceKind::UsageLedger,
        Component::Provider,
        Operation::ProviderStream,
        203 * 60_000,
        ObservationSignal::OperationCompleted {
            outcome: Outcome::Succeeded,
            latency_millis: 10,
        },
    )
    .trace
    .trace_id;
    assert!(
        service
            .trace_page(&latest_trace, 0, 2)
            .expect("bounded trace page")
            .rows
            .iter()
            .all(|row| row.sequence >= 2)
    );
    assert_eq!(
        service
            .trace_page(&latest_trace, 0, 3)
            .expect_err("query row bound")
            .kind(),
        ObservabilityErrorKind::LimitExceeded
    );
    assert_eq!(
        service
            .metric_page(0, 180_000, None, 2)
            .expect_err("metric bucket window bound")
            .kind(),
        ObservabilityErrorKind::InvalidInput
    );
    let invalid_cursor = winwincode_observability::MetricCursor {
        bucket_start_unix_millis: 60_000,
        key: MetricSeriesKey::Capacity {
            component: Component::Http,
            resource: CapacityResource::QueueDepth,
        },
    };
    assert_eq!(
        service
            .metric_page(60_000, 120_000, Some(&invalid_cursor), 2)
            .expect_err("cursor dimensions must be closed")
            .kind(),
        ObservabilityErrorKind::InvalidInput
    );
}

fn record_during_wal_read(
    path: &Path,
    config: &ObservabilityConfig,
) -> (SqliteObservability, Observation) {
    let reader = Connection::open(path).expect("open independent reader");
    reader.execute_batch("BEGIN").expect("begin long read");
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM observation_log", [], |row| row.get(0))
        .expect("hold WAL read snapshot");
    let mut concurrent =
        SqliteObservability::open(path, config.clone()).expect("open concurrent writer");
    let heartbeat = observation(
        204,
        ObservationSourceKind::RuntimeEvent,
        Component::Worker,
        Operation::WorkerHeartbeat,
        204 * 60_000,
        ObservationSignal::OperationCompleted {
            outcome: Outcome::Succeeded,
            latency_millis: 1,
        },
    );
    concurrent
        .record(&heartbeat)
        .expect("heartbeat write while read snapshot is open");
    concurrent
        .record(&observation(
            205,
            ObservationSourceKind::UsageLedger,
            Component::Provider,
            Operation::ProviderStream,
            205 * 60_000,
            ObservationSignal::OperationCompleted {
                outcome: Outcome::Succeeded,
                latency_millis: 2,
            },
        ))
        .expect("model stream write while read snapshot is open");
    reader
        .execute_batch("ROLLBACK")
        .expect("release read snapshot");
    (concurrent, heartbeat)
}

fn fill_receipt_bound(service: &mut SqliteObservability) {
    for (seed, source_kind, component, operation, latency) in [
        (
            206,
            ObservationSourceKind::InternalOperation,
            Component::Queue,
            Operation::QueueDequeue,
            3,
        ),
        (
            207,
            ObservationSourceKind::AuditLedger,
            Component::Http,
            Operation::HttpRequest,
            4,
        ),
    ] {
        service
            .record(&observation(
                seed,
                source_kind,
                component,
                operation,
                seed * 60_000,
                ObservationSignal::OperationCompleted {
                    outcome: Outcome::Succeeded,
                    latency_millis: latency,
                },
            ))
            .expect("fill receipt capacity");
    }
    assert_eq!(
        service
            .record(&observation(
                208,
                ObservationSourceKind::InternalOperation,
                Component::Storage,
                Operation::StorageRead,
                208 * 60_000,
                ObservationSignal::OperationCompleted {
                    outcome: Outcome::Succeeded,
                    latency_millis: 5,
                },
            ))
            .expect_err("receipt bound must fail closed")
            .kind(),
        ObservabilityErrorKind::LimitExceeded
    );
}

#[test]
fn bounded_retention_queries_and_wal_keep_heartbeat_and_model_writes_available() {
    let directory = TestDirectory::new("bounded");
    let path = directory.database();
    let mut bounded = config();
    bounded.max_receipts = 8;
    bounded.max_trace_rows = 3;
    bounded.max_metric_rows = 3;
    bounded.max_query_rows = 2;
    bounded.max_query_buckets = 2;
    bounded.alert_rules.clear();
    let mut service =
        SqliteObservability::open(&path, bounded.clone()).expect("open bounded observability");
    record_provider_streams(&mut service, 200, 4);
    assert_bounded_queries(&service);
    let (mut concurrent, heartbeat) = record_during_wal_read(&path, &bounded);
    fill_receipt_bound(&mut concurrent);
    drop(concurrent);

    let mut restarted =
        SqliteObservability::open(&path, bounded).expect("restart bounded observability");
    let replay = restarted
        .record(&heartbeat)
        .expect("retained receipt replay");
    assert!(replay.duplicate);
    assert_eq!(replay.accepted_sequence, 5);
    assert_eq!(
        restarted
            .record(&Observation {
                signal: ObservationSignal::OperationCompleted {
                    outcome: Outcome::Failed,
                    latency_millis: 1,
                },
                ..heartbeat
            })
            .expect_err("changed replay after restart")
            .kind(),
        ObservabilityErrorKind::Conflict
    );
    assert_secure_file(&path);
}

#[cfg(unix)]
fn assert_secure_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(
        fs::metadata(path)
            .expect("database metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(not(unix))]
fn assert_secure_file(_path: &Path) {}
