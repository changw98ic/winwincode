// SPDX-License-Identifier: Apache-2.0

//! Deterministic React-versus-DelegatedBatch performance evidence reduction.
//!
//! The reducer consumes secret-safe, read-only projections of the existing
//! performance and model-call ledgers. It produces measurements only; rollout
//! policy and Go/No-Go decisions remain outside this module.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use winwincode_domain::Sha256Digest;

use crate::runtime_trace_outbox::{ExecutionMode, ObserverMode};

const MAX_SAFE_METRIC: i64 = 9_007_199_254_740_991;

/// One terminal projection from a single comparison run.
///
/// `run_id` must be a SHA-256 digest of the private run key. Exact repeats are
/// counted as replayed writes and do not contribute to measured totals twice.
/// Only cross-mode metrics with authoritative durable facts are retained;
/// workspace Patch and Validation counters are excluded from V0 evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceV0RunEvidence {
    pub run_id: Sha256Digest,
    pub execution_mode: ExecutionMode,
    pub observer_mode: ObserverMode,
    pub primary_model_call_count: i64,
    pub primary_model_input_tokens: i64,
    pub primary_model_cached_tokens: i64,
    pub primary_model_output_tokens: i64,
    pub primary_model_wait_ms: i64,
    pub observer_call_count: i64,
    pub observer_wait_ms: i64,
    pub total_runtime_ms: i64,
}

/// Model operation category retained by the existing performance ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceV0ModelKind {
    Primary,
    Observer,
}

/// One primary/strong-model or Observer operation projected from the durable
/// performance ledger.
///
/// Both identities are digests so the comparison artifact contains no raw run
/// or Provider request identifiers. `actual_cost_microunits` is the settled
/// charge retained on the unique performance operation, when the Provider
/// supplied one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceV0ModelCallEvidence {
    pub run_id: Sha256Digest,
    pub model_call_id: Sha256Digest,
    pub model_kind: PerformanceV0ModelKind,
    pub completed: bool,
    pub input_tokens: i64,
    pub cached_tokens: i64,
    pub output_tokens: i64,
    pub elapsed_millis: i64,
    pub actual_cost_microunits: Option<i64>,
}

/// Aggregate for one side of the V0 comparison.
///
/// A strong-model call is the existing `primary_model` ledger operation.
/// Observer calls remain separate, while Token and settled-cost totals include
/// both primary and Observer operations. Token totals preserve the current
/// ledger convention: input, cached, and output counters are added exactly as
/// retained. Workspace Patch and Validation counters are intentionally absent
/// until their delegated execution path has an authoritative durable
/// settlement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceV0ArmSummary {
    pub sample_count: i64,
    pub strong_model_call_count: i64,
    pub observer_model_call_count: i64,
    pub completed_strong_model_call_count: i64,
    pub incomplete_strong_model_call_count: i64,
    pub completed_observer_model_call_count: i64,
    pub incomplete_observer_model_call_count: i64,
    pub total_tokens: i64,
    pub total_strong_model_wait_ms: i64,
    pub total_observer_model_wait_ms: i64,
    pub total_runtime_ms: i64,
    pub settled_cost_microunits: i64,
    pub unpriced_completed_call_count: i64,
    pub duplicate_run_write_count: i64,
    pub duplicate_model_call_write_count: i64,
    pub duplicate_settled_charge_write_count: i64,
    pub duplicate_settled_charge_microunits: i64,
}

/// React and Structured/DelegatedBatch V0 evidence kept side by side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceV0Comparison {
    pub react: PerformanceV0ArmSummary,
    pub structured: PerformanceV0ArmSummary,
}

/// Invalid or internally inconsistent comparison evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformanceV0ComparisonError {
    InvalidIdentity,
    InvalidMetric,
    MetricOverflow,
    UnknownRun,
    ConflictingRunReplay,
    ConflictingModelCallReplay,
    InvalidIncompleteModelCall,
    ModelCallReportMismatch,
}

impl fmt::Display for PerformanceV0ComparisonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentity => "performance evidence identity is invalid",
            Self::InvalidMetric => "performance evidence metric is invalid",
            Self::MetricOverflow => "performance evidence metric overflowed",
            Self::UnknownRun => "model-call evidence references an unknown run",
            Self::ConflictingRunReplay => "run replay conflicts with retained evidence",
            Self::ConflictingModelCallReplay => {
                "model-call replay conflicts with retained evidence"
            }
            Self::InvalidIncompleteModelCall => {
                "incomplete model-call evidence contains terminal usage"
            }
            Self::ModelCallReportMismatch => {
                "model-call evidence does not reconcile with its terminal report"
            }
        })
    }
}

impl std::error::Error for PerformanceV0ComparisonError {}

#[derive(Clone, Copy)]
enum ComparisonArm {
    React,
    Structured,
}

#[derive(Clone, Default)]
struct ModelCallTotals {
    count: i64,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    elapsed_millis: i64,
}

/// Reduces exact ledger projections into a comparison without double-counting
/// replayed report, model-call, or settled-charge writes.
///
/// Delegated patch is the Structured arm. Delegated patch shadow retains its
/// cohort label in the projection but executes the React role-session strategy,
/// so it enters the React arm. Every unique run must include the complete
/// primary-model and Observer call projections so the operation ledger can be
/// reconciled with its terminal usage report.
///
/// # Errors
///
/// Returns an error for malformed values, unknown identities, conflicting
/// replays, overflow, or a report that disagrees with its model-call rows.
pub fn summarize_performance_v0(
    runs: &[PerformanceV0RunEvidence],
    model_calls: &[PerformanceV0ModelCallEvidence],
) -> Result<PerformanceV0Comparison, PerformanceV0ComparisonError> {
    let mut comparison = PerformanceV0Comparison::default();
    let mut unique_runs = BTreeMap::<String, PerformanceV0RunEvidence>::new();

    for run in runs {
        validate_digest(&run.run_id)?;
        validate_run(run)?;
        if let Some(existing) = unique_runs.get(&run.run_id.0) {
            if existing != run {
                return Err(PerformanceV0ComparisonError::ConflictingRunReplay);
            }
            add_metric(
                &mut summary_mut(&mut comparison, arm(run)).duplicate_run_write_count,
                1,
            )?;
            continue;
        }

        let summary = summary_mut(&mut comparison, arm(run));
        add_metric(&mut summary.sample_count, 1)?;
        add_metric(
            &mut summary.strong_model_call_count,
            run.primary_model_call_count,
        )?;
        add_metric(
            &mut summary.observer_model_call_count,
            run.observer_call_count,
        )?;
        add_metric(
            &mut summary.total_strong_model_wait_ms,
            run.primary_model_wait_ms,
        )?;
        add_metric(
            &mut summary.total_observer_model_wait_ms,
            run.observer_wait_ms,
        )?;
        add_metric(&mut summary.total_runtime_ms, run.total_runtime_ms)?;
        unique_runs.insert(run.run_id.0.clone(), run.clone());
    }

    let mut unique_calls =
        BTreeMap::<(String, PerformanceV0ModelKind, String), PerformanceV0ModelCallEvidence>::new();
    let mut calls_by_run = BTreeMap::<(String, PerformanceV0ModelKind), ModelCallTotals>::new();

    for call in model_calls {
        validate_model_call(call)?;
        let run = unique_runs
            .get(&call.run_id.0)
            .ok_or(PerformanceV0ComparisonError::UnknownRun)?;
        let call_key = (
            call.run_id.0.clone(),
            call.model_kind,
            call.model_call_id.0.clone(),
        );
        if let Some(existing) = unique_calls.get(&call_key) {
            if existing != call {
                return Err(PerformanceV0ComparisonError::ConflictingModelCallReplay);
            }
            let summary = summary_mut(&mut comparison, arm(run));
            add_metric(&mut summary.duplicate_model_call_write_count, 1)?;
            if let Some(cost) = call.actual_cost_microunits {
                add_metric(&mut summary.duplicate_settled_charge_write_count, 1)?;
                add_metric(&mut summary.duplicate_settled_charge_microunits, cost)?;
            }
            continue;
        }

        let summary = summary_mut(&mut comparison, arm(run));
        if call.completed {
            match call.model_kind {
                PerformanceV0ModelKind::Primary => {
                    add_metric(&mut summary.completed_strong_model_call_count, 1)?;
                }
                PerformanceV0ModelKind::Observer => {
                    add_metric(&mut summary.completed_observer_model_call_count, 1)?;
                }
            }
            if let Some(cost) = call.actual_cost_microunits {
                add_metric(&mut summary.settled_cost_microunits, cost)?;
            } else {
                add_metric(&mut summary.unpriced_completed_call_count, 1)?;
            }
        } else {
            match call.model_kind {
                PerformanceV0ModelKind::Primary => {
                    add_metric(&mut summary.incomplete_strong_model_call_count, 1)?;
                }
                PerformanceV0ModelKind::Observer => {
                    add_metric(&mut summary.incomplete_observer_model_call_count, 1)?;
                }
            }
        }
        let call_tokens = model_call_total_tokens(call)?;
        add_metric(&mut summary.total_tokens, call_tokens)?;

        let totals = calls_by_run
            .entry((call.run_id.0.clone(), call.model_kind))
            .or_default();
        add_metric(&mut totals.count, 1)?;
        add_metric(&mut totals.input_tokens, call.input_tokens)?;
        add_metric(&mut totals.cached_tokens, call.cached_tokens)?;
        add_metric(&mut totals.output_tokens, call.output_tokens)?;
        add_metric(&mut totals.elapsed_millis, call.elapsed_millis)?;
        unique_calls.insert(call_key, call.clone());
    }

    reconcile_model_calls(&unique_runs, &calls_by_run)?;

    Ok(comparison)
}

fn reconcile_model_calls(
    runs: &BTreeMap<String, PerformanceV0RunEvidence>,
    calls_by_run: &BTreeMap<(String, PerformanceV0ModelKind), ModelCallTotals>,
) -> Result<(), PerformanceV0ComparisonError> {
    for (run_id, run) in runs {
        let primary = calls_by_run
            .get(&(run_id.clone(), PerformanceV0ModelKind::Primary))
            .cloned()
            .unwrap_or_default();
        let observer = calls_by_run
            .get(&(run_id.clone(), PerformanceV0ModelKind::Observer))
            .cloned()
            .unwrap_or_default();
        if primary.count != run.primary_model_call_count
            || primary.input_tokens != run.primary_model_input_tokens
            || primary.cached_tokens != run.primary_model_cached_tokens
            || primary.output_tokens != run.primary_model_output_tokens
            || primary.elapsed_millis != run.primary_model_wait_ms
            || observer.count != run.observer_call_count
            || observer.elapsed_millis != run.observer_wait_ms
        {
            return Err(PerformanceV0ComparisonError::ModelCallReportMismatch);
        }
    }
    Ok(())
}

const fn arm(run: &PerformanceV0RunEvidence) -> ComparisonArm {
    match run.execution_mode {
        ExecutionMode::React | ExecutionMode::DelegatedPatchShadow => ComparisonArm::React,
        ExecutionMode::DelegatedPatch => ComparisonArm::Structured,
    }
}

const fn summary_mut(
    comparison: &mut PerformanceV0Comparison,
    arm: ComparisonArm,
) -> &mut PerformanceV0ArmSummary {
    match arm {
        ComparisonArm::React => &mut comparison.react,
        ComparisonArm::Structured => &mut comparison.structured,
    }
}

fn validate_run(run: &PerformanceV0RunEvidence) -> Result<(), PerformanceV0ComparisonError> {
    for value in [
        run.primary_model_call_count,
        run.primary_model_input_tokens,
        run.primary_model_cached_tokens,
        run.primary_model_output_tokens,
        run.primary_model_wait_ms,
        run.observer_call_count,
        run.observer_wait_ms,
        run.total_runtime_ms,
    ] {
        validate_metric(value)?;
    }
    run_total_primary_tokens(run).map(|_| ())
}

fn validate_model_call(
    call: &PerformanceV0ModelCallEvidence,
) -> Result<(), PerformanceV0ComparisonError> {
    validate_digest(&call.run_id)?;
    validate_digest(&call.model_call_id)?;
    for value in [
        call.input_tokens,
        call.cached_tokens,
        call.output_tokens,
        call.elapsed_millis,
    ] {
        validate_metric(value)?;
    }
    if let Some(cost) = call.actual_cost_microunits {
        validate_metric(cost)?;
    }
    if !call.completed
        && (call.input_tokens != 0
            || call.cached_tokens != 0
            || call.output_tokens != 0
            || call.elapsed_millis != 0
            || call.actual_cost_microunits.is_some())
    {
        return Err(PerformanceV0ComparisonError::InvalidIncompleteModelCall);
    }
    Ok(())
}

fn run_total_primary_tokens(
    run: &PerformanceV0RunEvidence,
) -> Result<i64, PerformanceV0ComparisonError> {
    let mut total = 0;
    add_metric(&mut total, run.primary_model_input_tokens)?;
    add_metric(&mut total, run.primary_model_cached_tokens)?;
    add_metric(&mut total, run.primary_model_output_tokens)?;
    Ok(total)
}

fn model_call_total_tokens(
    call: &PerformanceV0ModelCallEvidence,
) -> Result<i64, PerformanceV0ComparisonError> {
    let mut total = 0;
    add_metric(&mut total, call.input_tokens)?;
    add_metric(&mut total, call.cached_tokens)?;
    add_metric(&mut total, call.output_tokens)?;
    Ok(total)
}

fn validate_digest(value: &Sha256Digest) -> Result<(), PerformanceV0ComparisonError> {
    if value.0.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        Ok(())
    } else {
        Err(PerformanceV0ComparisonError::InvalidIdentity)
    }
}

fn validate_metric(value: i64) -> Result<(), PerformanceV0ComparisonError> {
    if (0..=MAX_SAFE_METRIC).contains(&value) {
        Ok(())
    } else {
        Err(PerformanceV0ComparisonError::InvalidMetric)
    }
}

fn add_metric(target: &mut i64, value: i64) -> Result<(), PerformanceV0ComparisonError> {
    validate_metric(value)?;
    *target = target
        .checked_add(value)
        .filter(|sum| *sum <= MAX_SAFE_METRIC)
        .ok_or(PerformanceV0ComparisonError::MetricOverflow)?;
    Ok(())
}
