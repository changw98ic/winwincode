// SPDX-License-Identifier: Apache-2.0

use sha2::{Digest, Sha256};
use winwincode_observability_core::{
    AlertCondition, AlertRule, AlertRuleId, AlertSeverity, AlertState, AlertStatus,
    AlertTransition, CapacityResource, Component, FactDigest, MetricRow, Observation,
    ObservationId, ObservationSignal, ObservationSource, ObservationSourceKind, Operation, Outcome,
    SourceFactId, TraceContext, evaluate_alert_rule,
};

fn digest(material: &str) -> FactDigest {
    FactDigest::try_new(format!("sha256:{:x}", Sha256::digest(material.as_bytes())))
        .expect("fact digest")
}

fn observation(
    seed: u64,
    component: Component,
    operation: Operation,
    occurred_at_unix_millis: u64,
    signal: ObservationSignal,
) -> Observation {
    Observation {
        observation_id: ObservationId::try_new(format!("obs_{seed:026}")).expect("observation id"),
        source: ObservationSource {
            kind: ObservationSourceKind::InternalOperation,
            fact_id: SourceFactId::try_new(format!("settled/source/{seed}"))
                .expect("source fact id"),
            fact_digest: digest(&format!("source-fact-{seed}")),
        },
        trace: TraceContext::derive(
            &digest(&format!("correlation-{}", seed % 3)),
            component,
            operation,
            seed,
            None,
        )
        .expect("trace context"),
        component,
        operation,
        occurred_at_unix_millis,
        signal,
    }
}

fn canonical_rules() -> [AlertRule; 4] {
    [
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

fn canonical_alert_observations() -> [Observation; 4] {
    [
        observation(
            100,
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

#[test]
fn closed_dimensions_reject_cross_component_and_secret_shaped_values() {
    let invalid = observation(
        1,
        Component::Http,
        Operation::StorageWrite,
        60_001,
        ObservationSignal::OperationCompleted {
            outcome: Outcome::Succeeded,
            latency_millis: 5,
        },
    );
    assert!(invalid.validate().is_err());
    assert!(SourceFactId::try_new("authorization:Bearer-private").is_err());
}

#[test]
fn metric_aggregation_uses_only_closed_series_and_checked_accumulators() {
    let first = observation(
        10,
        Component::Provider,
        Operation::ProviderSettlement,
        60_010,
        ObservationSignal::OperationCompleted {
            outcome: Outcome::Succeeded,
            latency_millis: 20,
        },
    );
    let second = observation(
        11,
        Component::Provider,
        Operation::ProviderSettlement,
        60_011,
        ObservationSignal::OperationCompleted {
            outcome: Outcome::Succeeded,
            latency_millis: 35,
        },
    );
    let mut row = MetricRow::try_from_observation(60_000, &first).expect("metric row");
    row.apply_observation(&second)
        .expect("aggregate second observation");
    assert_eq!(row.observations, 2);
    assert_eq!(row.latency_total_millis, 55);
    assert_eq!(row.latency_max_millis, 35);

    let different_series = observation(
        12,
        Component::Queue,
        Operation::QueueEnqueue,
        60_012,
        ObservationSignal::CapacityObserved {
            resource: CapacityResource::QueueDepth,
            used: 1,
            limit: 10,
        },
    );
    assert!(row.apply_observation(&different_series).is_err());
}

#[test]
fn alert_state_machine_reproduces_the_canonical_firing_transitions() {
    let rules = canonical_rules();
    let observations = canonical_alert_observations();
    let transitions = rules
        .iter()
        .zip(&observations)
        .enumerate()
        .map(|(index, (rule, observed))| {
            let state = evaluate_alert_rule(rule, observed, None)
                .expect("evaluate alert")
                .expect("firing state");
            assert_eq!(
                state,
                AlertState {
                    status: AlertStatus::Firing,
                    generation: 1
                }
            );
            AlertTransition::try_from_state(
                u64::try_from(index).expect("sequence") + 1,
                rule,
                state,
                observed,
            )
            .expect("transition")
        })
        .collect::<Vec<_>>();
    let actual = format!(
        "{}\n",
        serde_json::to_string_pretty(&transitions).expect("serialize fixture")
    );
    assert_eq!(actual, include_str!("fixtures/alert-transitions.v1.json"));

    let stable = evaluate_alert_rule(
        &rules[0],
        &observations[0],
        Some(AlertState {
            status: AlertStatus::Firing,
            generation: 1,
        }),
    )
    .expect("evaluate stable state");
    assert_eq!(stable, None);
}
