// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::Sha256Digest;
use winwincode_execution_port::performance_comparison::{
    PerformanceV0ComparisonError, PerformanceV0ModelCallEvidence, PerformanceV0ModelKind,
    PerformanceV0RunEvidence, summarize_performance_v0,
};
use winwincode_execution_port::runtime_trace_outbox::{ExecutionMode, ObserverMode};

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn run_evidence(
    run_id: char,
    execution_mode: ExecutionMode,
    primary: (i64, i64, i64, i64, i64, i64),
) -> PerformanceV0RunEvidence {
    let (calls, input_tokens, cached_tokens, output_tokens, wait_ms, runtime_ms) = primary;
    PerformanceV0RunEvidence {
        run_id: digest(run_id),
        execution_mode,
        observer_mode: ObserverMode::Off,
        primary_model_call_count: calls,
        primary_model_input_tokens: input_tokens,
        primary_model_cached_tokens: cached_tokens,
        primary_model_output_tokens: output_tokens,
        primary_model_wait_ms: wait_ms,
        observer_call_count: 0,
        observer_wait_ms: 0,
        total_runtime_ms: runtime_ms,
    }
}

fn model_call(
    run_id: Sha256Digest,
    model_call_id: Sha256Digest,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    elapsed_millis: i64,
    actual_cost_microunits: Option<i64>,
) -> PerformanceV0ModelCallEvidence {
    PerformanceV0ModelCallEvidence {
        run_id,
        model_call_id,
        model_kind: PerformanceV0ModelKind::Primary,
        completed: true,
        input_tokens,
        cached_tokens,
        output_tokens,
        elapsed_millis,
        actual_cost_microunits,
    }
}

fn observer_call(
    run_id: Sha256Digest,
    model_call_id: Sha256Digest,
    input_tokens: i64,
    output_tokens: i64,
    elapsed_millis: i64,
    actual_cost_microunits: Option<i64>,
) -> PerformanceV0ModelCallEvidence {
    let mut call = model_call(
        run_id,
        model_call_id,
        input_tokens,
        0,
        output_tokens,
        elapsed_millis,
        actual_cost_microunits,
    );
    call.model_kind = PerformanceV0ModelKind::Observer;
    call
}

#[test]
fn react_and_delegated_batch_evidence_is_deduplicated_before_comparison() {
    let react_run = run_evidence('a', ExecutionMode::React, (2, 150, 20, 40, 600, 1_000));
    let mut structured_run = run_evidence(
        'b',
        ExecutionMode::DelegatedPatch,
        (1, 60, 10, 20, 350, 700),
    );
    structured_run.observer_mode = ObserverMode::Always;
    structured_run.observer_call_count = 1;
    structured_run.observer_wait_ms = 120;
    let react_call_one = model_call(
        react_run.run_id.clone(),
        digest('1'),
        100,
        20,
        30,
        400,
        Some(40),
    );
    let calls = vec![
        react_call_one.clone(),
        model_call(react_run.run_id.clone(), digest('2'), 50, 0, 10, 200, None),
        react_call_one,
        model_call(
            structured_run.run_id.clone(),
            digest('3'),
            60,
            10,
            20,
            350,
            Some(25),
        ),
        observer_call(
            structured_run.run_id.clone(),
            digest('5'),
            9,
            3,
            120,
            Some(7),
        ),
    ];

    let comparison =
        summarize_performance_v0(&[react_run.clone(), react_run, structured_run], &calls)
            .expect("summarize exact React and DelegatedBatch evidence");

    assert_eq!(comparison.react.sample_count, 1);
    assert_eq!(comparison.react.strong_model_call_count, 2);
    assert_eq!(comparison.react.completed_strong_model_call_count, 2);
    assert_eq!(comparison.react.total_tokens, 210);
    assert_eq!(comparison.react.total_strong_model_wait_ms, 600);
    assert_eq!(comparison.react.total_runtime_ms, 1_000);
    assert_eq!(comparison.react.settled_cost_microunits, 40);
    assert_eq!(comparison.react.unpriced_completed_call_count, 1);
    assert_eq!(comparison.react.duplicate_run_write_count, 1);
    assert_eq!(comparison.react.duplicate_model_call_write_count, 1);
    assert_eq!(comparison.react.duplicate_settled_charge_write_count, 1);
    assert_eq!(comparison.react.duplicate_settled_charge_microunits, 40);

    assert_eq!(comparison.structured.sample_count, 1);
    assert_eq!(comparison.structured.strong_model_call_count, 1);
    assert_eq!(comparison.structured.observer_model_call_count, 1);
    assert_eq!(comparison.structured.completed_strong_model_call_count, 1);
    assert_eq!(comparison.structured.completed_observer_model_call_count, 1);
    assert_eq!(comparison.structured.total_tokens, 102);
    assert_eq!(comparison.structured.total_strong_model_wait_ms, 350);
    assert_eq!(comparison.structured.total_observer_model_wait_ms, 120);
    assert_eq!(comparison.structured.total_runtime_ms, 700);
    assert_eq!(comparison.structured.settled_cost_microunits, 32);
    assert_eq!(comparison.structured.duplicate_model_call_write_count, 0);
}

#[test]
fn conflicting_settled_charge_replay_is_rejected_as_one_evidence_failure() {
    let run = run_evidence(
        'c',
        ExecutionMode::DelegatedPatchShadow,
        (1, 20, 0, 10, 50, 80),
    );
    let retained = model_call(run.run_id.clone(), digest('4'), 20, 0, 10, 50, Some(12));
    let mut conflicting = retained.clone();
    conflicting.actual_cost_microunits = Some(24);

    assert_eq!(
        summarize_performance_v0(&[run], &[retained, conflicting]),
        Err(PerformanceV0ComparisonError::ConflictingModelCallReplay)
    );
}
