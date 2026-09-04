// SPDX-License-Identifier: Apache-2.0

//! Closed identities for pre-Go performance evaluation.
//!
//! An assignment is frozen before a Job runs. An authorization is created only
//! after that exact Job has a terminal result and an exact Candidate Artifact.
//! A paired sample retains the raw performance rows instead of trusting caller-
//! supplied aggregates. Rollout policy consumes only complete, validated pairs.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ExecutionJobId, RepositoryScope, Sha256Digest};

use crate::generated::ArtifactReference;
use crate::performance_comparison::{
    PerformanceV0ArmSummary, PerformanceV0Comparison, PerformanceV0ModelCallEvidence,
    PerformanceV0ModelKind, PerformanceV0RunEvidence, summarize_performance_v0,
};
use crate::performance_statistics::PairedMetricValue;
use crate::runtime_trace_outbox::{ExecutionMode, ObserverMode};

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const MAX_ROUTES: usize = 16;
const MAX_ROUTE_ATTEMPTS: u32 = 16;
const MAX_PAIRS: usize = 4_096;
const MAX_IDENTITY_TEXT: usize = 512;

/// Fixed side of one predeclared paired evaluation case.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationArmV1 {
    React,
    Delegated,
}

/// Exact Provider/model route admitted before the cohort starts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationRouteV1 {
    pub provider_id: String,
    pub model_id: String,
    pub route_digest: Sha256Digest,
}

/// Observer mode and its predeclared route set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationObserverV1 {
    pub mode: ObserverMode,
    pub planned_routes: Vec<EvaluationRouteV1>,
}

/// One route step in a frozen Provider retry/failover plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationRetryStepV1 {
    pub route_index: u32,
    pub maximum_attempts: u32,
}

/// Exact retry policy authority shared by every logical call of one kind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationRetryPlanV1 {
    pub policy_revision: u64,
    pub plan_fingerprint: Sha256Digest,
    pub steps: Vec<EvaluationRetryStepV1>,
}

/// Retry bounds frozen before one logical evaluation sample starts.
///
/// V1 admits exactly one logical sample for the slot. Every Provider retry and
/// failover exchange remains inside that sample and the exact frozen plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationAttemptPolicyV1 {
    pub logical_sample_count: u32,
    pub primary: EvaluationRetryPlanV1,
    pub observer: Option<EvaluationRetryPlanV1>,
}

/// Exact per-source snapshots joined after one evaluation arm reaches its
/// terminal Candidate.
///
/// The cursor fields are interpreted only inside the source that produced
/// them. They are never compared with one another: the Control Plane terminal
/// stream, retry ledger, Worker ledger, and Artifact catalog have independent
/// revision domains.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationEvidenceCutoffV1 {
    pub cutoff_at_millis: u64,
    pub control_plane_terminal_cursor: u64,
    pub retry_ledger_cursor: u64,
    pub candidate_ack_cursor: u64,
    pub artifact_acknowledged_sequence: u64,
    pub worker_ledger_snapshot_digest: Sha256Digest,
    pub artifact_snapshot_digest: Sha256Digest,
}

/// Caller input frozen into an assignment before either arm executes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationAssignmentSpecV1 {
    pub repository_scope: RepositoryScope,
    pub source_release: ArtifactReference,
    pub cohort_manifest: ArtifactReference,
    pub cohort_id: Sha256Digest,
    pub case_id: Sha256Digest,
    pub pair_id: Sha256Digest,
    pub arm: EvaluationArmV1,
    pub base_revision: String,
    pub job_id: ExecutionJobId,
    pub run_id: Sha256Digest,
    pub primary_planned_routes: Vec<EvaluationRouteV1>,
    pub observer: EvaluationObserverV1,
    pub attempt_policy: EvaluationAttemptPolicyV1,
    pub policy_revision: u64,
    pub policy_digest: Sha256Digest,
    pub cutoff_at_millis: u64,
}

/// Immutable pre-run assignment used only by the evaluation lane.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationAssignmentV1 {
    spec: EvaluationAssignmentSpecV1,
    assignment_digest: Sha256Digest,
}

impl EvaluationAssignmentV1 {
    /// Freezes and digests an exact evaluation assignment.
    ///
    /// # Errors
    ///
    /// Rejects unbounded or non-canonical assignment facts.
    pub fn try_new(spec: EvaluationAssignmentSpecV1) -> Result<Self, PerformanceEvaluationError> {
        validate_assignment_spec(&spec)?;
        let assignment_digest = digest_json(&spec)?;
        Ok(Self {
            spec,
            assignment_digest,
        })
    }

    /// Revalidates a deserialized assignment and its digest.
    ///
    /// # Errors
    ///
    /// Rejects malformed facts or a digest mismatch.
    pub fn validate(&self) -> Result<(), PerformanceEvaluationError> {
        validate_assignment_spec(&self.spec)?;
        if self.assignment_digest != digest_json(&self.spec)? {
            return Err(PerformanceEvaluationError::DigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn spec(&self) -> &EvaluationAssignmentSpecV1 {
        &self.spec
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.assignment_digest
    }
}

/// Closed settlement class for one actual Provider exchange.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationAttemptOutcomeV1 {
    FailedNoCharge,
    FailedCharged,
    Succeeded,
}

/// Exact normalized usage attached to one charged or successful exchange.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationSettledUsageV1 {
    pub provider_usage_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub cost_microunits: u64,
}

/// One actual Provider exchange selected during one logical model call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationRouteAttemptV1 {
    pub ordinal: u32,
    pub step_index: u32,
    pub attempt_on_step: u32,
    pub route: EvaluationRouteV1,
    pub provider_exchange_digest: Sha256Digest,
    pub outcome: EvaluationAttemptOutcomeV1,
    pub settled_usage: Option<EvaluationSettledUsageV1>,
}

/// Retry and Provider authority for one logical Primary or Observer call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationModelCallAuthorityV1 {
    pub model_call_digest: Sha256Digest,
    pub retry_state_revision: u64,
    pub retry_plan: EvaluationRetryPlanV1,
    pub attempts: Vec<EvaluationRouteAttemptV1>,
}

/// Terminal facts joined to an assignment after its Candidate is acknowledged.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationAuthorizationFactsV1 {
    pub candidate_artifact: ArtifactReference,
    pub evidence_cutoff: EvaluationEvidenceCutoffV1,
    pub candidate_artifact_ack_revision: u64,
    pub dispatch_accepted_at_millis: u64,
    pub worker_terminal_finished_at_millis: u64,
    pub terminal_accepted_at_millis: u64,
    pub terminal_revision: u64,
    pub authorization_revision: u64,
    pub primary_model_calls: Vec<EvaluationModelCallAuthorityV1>,
    pub observer_model_calls: Vec<EvaluationModelCallAuthorityV1>,
}

/// Post-terminal authorization proving one arm may enter paired statistics.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvaluationAuthorizationV1 {
    assignment: EvaluationAssignmentV1,
    facts: EvaluationAuthorizationFactsV1,
    authorization_digest: Sha256Digest,
}

impl EvaluationAuthorizationV1 {
    /// Seals exact terminal, route-attempt, and Candidate facts.
    ///
    /// # Errors
    ///
    /// Rejects facts outside the assignment, cutoff, or planned routes.
    pub fn try_new(
        assignment: EvaluationAssignmentV1,
        facts: EvaluationAuthorizationFactsV1,
    ) -> Result<Self, PerformanceEvaluationError> {
        assignment.validate()?;
        validate_authorization_facts(&assignment, &facts)?;
        let authorization_digest = digest_json(&AuthorizationDigestFacts {
            assignment_digest: assignment.digest(),
            facts: &facts,
        })?;
        Ok(Self {
            assignment,
            facts,
            authorization_digest,
        })
    }

    /// Revalidates a deserialized authorization and every nested fact.
    ///
    /// # Errors
    ///
    /// Rejects malformed terminal facts or a digest mismatch.
    pub fn validate(&self) -> Result<(), PerformanceEvaluationError> {
        self.assignment.validate()?;
        validate_authorization_facts(&self.assignment, &self.facts)?;
        let expected = digest_json(&AuthorizationDigestFacts {
            assignment_digest: self.assignment.digest(),
            facts: &self.facts,
        })?;
        if self.authorization_digest != expected {
            return Err(PerformanceEvaluationError::DigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn assignment(&self) -> &EvaluationAssignmentV1 {
        &self.assignment
    }

    #[must_use]
    pub const fn facts(&self) -> &EvaluationAuthorizationFactsV1 {
        &self.facts
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.authorization_digest
    }

    #[must_use]
    pub const fn wall_clock_runtime_millis(&self) -> u64 {
        self.facts.terminal_accepted_at_millis - self.facts.dispatch_accepted_at_millis
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizationDigestFacts<'facts> {
    assignment_digest: &'facts Sha256Digest,
    facts: &'facts EvaluationAuthorizationFactsV1,
}

/// Raw, reconciled V0 ledger rows for one authorized arm.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceArmMeasurementV1 {
    run: PerformanceV0RunEvidence,
    model_calls: Vec<PerformanceV0ModelCallEvidence>,
    summary: PerformanceV0ArmSummary,
    measurement_digest: Sha256Digest,
}

impl PerformanceArmMeasurementV1 {
    /// Reconciles raw run and model-call rows into one arm measurement.
    ///
    /// # Errors
    ///
    /// Rejects a non-terminal, shadow, duplicated, or inconsistent run.
    pub fn from_v0(
        run: PerformanceV0RunEvidence,
        model_calls: Vec<PerformanceV0ModelCallEvidence>,
    ) -> Result<Self, PerformanceEvaluationError> {
        let summary = one_run_summary(&run, &model_calls)?;
        let measurement_digest = digest_json(&MeasurementDigestFacts {
            run: &run,
            model_calls: &model_calls,
            summary: &summary,
        })?;
        Ok(Self {
            run,
            model_calls,
            summary,
            measurement_digest,
        })
    }

    /// Revalidates raw rows and the retained summary/digest.
    ///
    /// # Errors
    ///
    /// Rejects any mismatch between raw ledger rows and the frozen summary.
    pub fn validate(&self) -> Result<(), PerformanceEvaluationError> {
        let summary = one_run_summary(&self.run, &self.model_calls)?;
        let digest = digest_json(&MeasurementDigestFacts {
            run: &self.run,
            model_calls: &self.model_calls,
            summary: &summary,
        })?;
        if self.summary != summary || self.measurement_digest != digest {
            return Err(PerformanceEvaluationError::DigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn run(&self) -> &PerformanceV0RunEvidence {
        &self.run
    }

    #[must_use]
    pub fn model_calls(&self) -> &[PerformanceV0ModelCallEvidence] {
        &self.model_calls
    }

    #[must_use]
    pub const fn summary(&self) -> &PerformanceV0ArmSummary {
        &self.summary
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.measurement_digest
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MeasurementDigestFacts<'facts> {
    run: &'facts PerformanceV0RunEvidence,
    model_calls: &'facts [PerformanceV0ModelCallEvidence],
    summary: &'facts PerformanceV0ArmSummary,
}

/// Five policy metrics exposed from one exact React/Delegated pair.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceEvaluationMetricV1 {
    StrongModelCalls,
    TotalTokens,
    ModelWaitMillis,
    WallClockRuntimeMillis,
    SettledCostMicrounits,
}

impl PerformanceEvaluationMetricV1 {
    #[must_use]
    pub const fn discriminant(self) -> u8 {
        match self {
            Self::StrongModelCalls => 1,
            Self::TotalTokens => 2,
            Self::ModelWaitMillis => 3,
            Self::WallClockRuntimeMillis => 4,
            Self::SettledCostMicrounits => 5,
        }
    }
}

/// One complete, immutable React/Delegated case pair.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformancePairedSampleV1 {
    react_authorization: EvaluationAuthorizationV1,
    react_measurement: PerformanceArmMeasurementV1,
    delegated_authorization: EvaluationAuthorizationV1,
    delegated_measurement: PerformanceArmMeasurementV1,
    pair_digest: Sha256Digest,
}

impl PerformancePairedSampleV1 {
    /// Joins two authorized arms from the same predeclared case.
    ///
    /// # Errors
    ///
    /// Rejects missing/mismatched identities, routes, raw rows, or cutoffs.
    pub fn try_new(
        react_authorization: EvaluationAuthorizationV1,
        react_measurement: PerformanceArmMeasurementV1,
        delegated_authorization: EvaluationAuthorizationV1,
        delegated_measurement: PerformanceArmMeasurementV1,
    ) -> Result<Self, PerformanceEvaluationError> {
        validate_arm(
            &react_authorization,
            &react_measurement,
            EvaluationArmV1::React,
        )?;
        validate_arm(
            &delegated_authorization,
            &delegated_measurement,
            EvaluationArmV1::Delegated,
        )?;
        validate_pair_identity(&react_authorization, &delegated_authorization)?;
        let pair_digest = pair_digest(
            &react_authorization,
            &react_measurement,
            &delegated_authorization,
            &delegated_measurement,
        )?;
        Ok(Self {
            react_authorization,
            react_measurement,
            delegated_authorization,
            delegated_measurement,
            pair_digest,
        })
    }

    /// Revalidates a deserialized pair from raw facts.
    ///
    /// # Errors
    ///
    /// Rejects any identity, evidence, or digest mismatch.
    pub fn validate(&self) -> Result<(), PerformanceEvaluationError> {
        validate_arm(
            &self.react_authorization,
            &self.react_measurement,
            EvaluationArmV1::React,
        )?;
        validate_arm(
            &self.delegated_authorization,
            &self.delegated_measurement,
            EvaluationArmV1::Delegated,
        )?;
        validate_pair_identity(&self.react_authorization, &self.delegated_authorization)?;
        let expected = pair_digest(
            &self.react_authorization,
            &self.react_measurement,
            &self.delegated_authorization,
            &self.delegated_measurement,
        )?;
        if self.pair_digest != expected {
            return Err(PerformanceEvaluationError::DigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.pair_digest
    }

    #[must_use]
    pub const fn react_authorization(&self) -> &EvaluationAuthorizationV1 {
        &self.react_authorization
    }

    #[must_use]
    pub const fn delegated_authorization(&self) -> &EvaluationAuthorizationV1 {
        &self.delegated_authorization
    }

    #[must_use]
    pub const fn react_measurement(&self) -> &PerformanceArmMeasurementV1 {
        &self.react_measurement
    }

    #[must_use]
    pub const fn delegated_measurement(&self) -> &PerformanceArmMeasurementV1 {
        &self.delegated_measurement
    }

    /// Returns the exact values used by paired statistics for one metric.
    ///
    /// # Errors
    ///
    /// Returns an overflow error when the two model-wait counters do not fit.
    pub fn metric(
        &self,
        metric: PerformanceEvaluationMetricV1,
    ) -> Result<PairedMetricValue, PerformanceEvaluationError> {
        Ok(PairedMetricValue {
            react: metric_value(
                metric,
                &self.react_authorization,
                self.react_measurement.summary(),
            )?,
            delegated: metric_value(
                metric,
                &self.delegated_authorization,
                self.delegated_measurement.summary(),
            )?,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairDigestFacts<'facts> {
    react_authorization: &'facts Sha256Digest,
    react_measurement: &'facts Sha256Digest,
    delegated_authorization: &'facts Sha256Digest,
    delegated_measurement: &'facts Sha256Digest,
}

/// Stable validation failure for evaluation authority and raw evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerformanceEvaluationError {
    InvalidIdentity,
    InvalidAssignment,
    InvalidAuthorization,
    InvalidRouteAttempt,
    InvalidMeasurement,
    MismatchedPair,
    DuplicatePair,
    InsufficientPairs,
    DigestMismatch,
    MetricOverflow,
}

impl fmt::Display for PerformanceEvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentity => "evaluation identity is invalid",
            Self::InvalidAssignment => "evaluation assignment is invalid",
            Self::InvalidAuthorization => "evaluation authorization is invalid",
            Self::InvalidRouteAttempt => "evaluation route attempt is invalid",
            Self::InvalidMeasurement => "evaluation measurement is invalid",
            Self::MismatchedPair => "evaluation pair identities differ",
            Self::DuplicatePair => "evaluation pair is duplicated",
            Self::InsufficientPairs => "evaluation requires at least two complete pairs",
            Self::DigestMismatch => "evaluation digest does not match its facts",
            Self::MetricOverflow => "evaluation metric exceeds the exact integer range",
        })
    }
}

impl std::error::Error for PerformanceEvaluationError {}

/// Rebuilds the V0 comparison only from complete authorized raw pairs.
///
/// # Errors
///
/// Rejects fewer than two pairs, duplicate identities, mixed cohorts, or any
/// malformed nested authorization/measurement. No aggregate input is accepted.
pub fn summarize_authorized_pairs_v1(
    pairs: &[PerformancePairedSampleV1],
) -> Result<PerformanceV0Comparison, PerformanceEvaluationError> {
    if !(2..=MAX_PAIRS).contains(&pairs.len()) {
        return Err(PerformanceEvaluationError::InsufficientPairs);
    }
    let first = &pairs[0];
    first.validate()?;
    let cohort = cohort_key(first);
    let mut pair_ids = BTreeSet::new();
    let mut case_ids = BTreeSet::new();
    let mut job_ids = BTreeSet::new();
    let mut run_ids = BTreeSet::new();
    let mut runs = Vec::with_capacity(pairs.len() * 2);
    let mut calls = Vec::new();
    for pair in pairs {
        pair.validate()?;
        if cohort_key(pair) != cohort {
            return Err(PerformanceEvaluationError::MismatchedPair);
        }
        let react = pair.react_authorization.assignment.spec();
        let delegated = pair.delegated_authorization.assignment.spec();
        if !pair_ids.insert(react.pair_id.0.clone()) || !case_ids.insert(react.case_id.0.clone()) {
            return Err(PerformanceEvaluationError::DuplicatePair);
        }
        for spec in [react, delegated] {
            if !job_ids.insert(spec.job_id.0.clone()) || !run_ids.insert(spec.run_id.0.clone()) {
                return Err(PerformanceEvaluationError::DuplicatePair);
            }
        }
        runs.push(pair.react_measurement.run.clone());
        runs.push(pair.delegated_measurement.run.clone());
        calls.extend(pair.react_measurement.model_calls.clone());
        calls.extend(pair.delegated_measurement.model_calls.clone());
    }
    let comparison = summarize_performance_v0(&runs, &calls)
        .map_err(|_| PerformanceEvaluationError::InvalidMeasurement)?;
    let expected =
        i64::try_from(pairs.len()).map_err(|_| PerformanceEvaluationError::MetricOverflow)?;
    if comparison.react.sample_count != expected || comparison.structured.sample_count != expected {
        return Err(PerformanceEvaluationError::InvalidMeasurement);
    }
    Ok(comparison)
}

fn validate_assignment_spec(
    spec: &EvaluationAssignmentSpecV1,
) -> Result<(), PerformanceEvaluationError> {
    validate_artifact(&spec.source_release)?;
    validate_artifact(&spec.cohort_manifest)?;
    for digest in [
        &spec.cohort_id,
        &spec.case_id,
        &spec.pair_id,
        &spec.run_id,
        &spec.policy_digest,
    ] {
        validate_digest(digest)?;
    }
    if spec.repository_scope.organization_id.0.is_empty()
        || spec.repository_scope.workspace_id.0.is_empty()
        || spec.repository_scope.project_id.0.is_empty()
        || spec.repository_scope.repository_id.0.is_empty()
        || spec.base_revision.is_empty()
        || spec.base_revision.len() > MAX_IDENTITY_TEXT
        || spec.job_id.0.is_empty()
        || spec.policy_revision == 0
        || spec.policy_revision > MAX_SAFE_INTEGER as u64
        || spec.cutoff_at_millis == 0
        || spec.cutoff_at_millis > MAX_SAFE_INTEGER as u64
    {
        return Err(PerformanceEvaluationError::InvalidAssignment);
    }
    validate_routes(&spec.primary_planned_routes, true)?;
    let observer_routes_required = spec.observer.mode != ObserverMode::Off;
    validate_routes(&spec.observer.planned_routes, observer_routes_required)?;
    if spec.attempt_policy.logical_sample_count != 1
        || (!observer_routes_required && !spec.observer.planned_routes.is_empty())
    {
        return Err(PerformanceEvaluationError::InvalidAssignment);
    }
    validate_retry_plan(&spec.attempt_policy.primary, &spec.primary_planned_routes)?;
    match (&spec.attempt_policy.observer, observer_routes_required) {
        (Some(plan), true) => validate_retry_plan(plan, &spec.observer.planned_routes)?,
        (None, false) => {}
        (Some(_), false) | (None, true) => {
            return Err(PerformanceEvaluationError::InvalidAssignment);
        }
    }
    Ok(())
}

fn validate_retry_plan(
    plan: &EvaluationRetryPlanV1,
    planned_routes: &[EvaluationRouteV1],
) -> Result<(), PerformanceEvaluationError> {
    validate_digest(&plan.plan_fingerprint)?;
    if plan.policy_revision == 0
        || plan.policy_revision > MAX_SAFE_INTEGER as u64
        || plan.steps.is_empty()
        || plan.steps.len() > MAX_ROUTES
    {
        return Err(PerformanceEvaluationError::InvalidAssignment);
    }
    let mut route_indices = BTreeSet::new();
    let mut total_attempts = 0_u32;
    for step in &plan.steps {
        let route_index = usize::try_from(step.route_index)
            .map_err(|_| PerformanceEvaluationError::InvalidAssignment)?;
        if planned_routes.get(route_index).is_none()
            || !route_indices.insert(step.route_index)
            || !(1..=MAX_ROUTE_ATTEMPTS).contains(&step.maximum_attempts)
        {
            return Err(PerformanceEvaluationError::InvalidAssignment);
        }
        total_attempts = total_attempts
            .checked_add(step.maximum_attempts)
            .ok_or(PerformanceEvaluationError::InvalidAssignment)?;
    }
    if total_attempts > MAX_ROUTE_ATTEMPTS {
        return Err(PerformanceEvaluationError::InvalidAssignment);
    }
    Ok(())
}

fn validate_routes(
    routes: &[EvaluationRouteV1],
    required: bool,
) -> Result<(), PerformanceEvaluationError> {
    if (required && routes.is_empty()) || routes.len() > MAX_ROUTES {
        return Err(PerformanceEvaluationError::InvalidAssignment);
    }
    let mut digests = BTreeSet::new();
    for route in routes {
        if route.provider_id.is_empty()
            || route.provider_id.len() > MAX_IDENTITY_TEXT
            || route.model_id.is_empty()
            || route.model_id.len() > MAX_IDENTITY_TEXT
            || !digests.insert(route.route_digest.0.clone())
        {
            return Err(PerformanceEvaluationError::InvalidAssignment);
        }
        validate_digest(&route.route_digest)?;
    }
    Ok(())
}

fn validate_authorization_facts(
    assignment: &EvaluationAssignmentV1,
    facts: &EvaluationAuthorizationFactsV1,
) -> Result<(), PerformanceEvaluationError> {
    validate_artifact(&facts.candidate_artifact)?;
    let spec = assignment.spec();
    validate_evidence_cutoff(&facts.evidence_cutoff, spec.cutoff_at_millis)?;
    if facts.dispatch_accepted_at_millis == 0
        || facts.worker_terminal_finished_at_millis < facts.dispatch_accepted_at_millis
        || facts.terminal_accepted_at_millis < facts.worker_terminal_finished_at_millis
        || facts.terminal_accepted_at_millis > spec.cutoff_at_millis
        || facts.candidate_artifact_ack_revision
            != facts.evidence_cutoff.candidate_ack_cursor
        || facts.terminal_revision != facts.evidence_cutoff.control_plane_terminal_cursor
        || facts.candidate_artifact_ack_revision == 0
        || facts.terminal_revision == 0
        || facts.authorization_revision == 0
        || facts.candidate_artifact_ack_revision > MAX_SAFE_INTEGER as u64
        || facts.terminal_revision > MAX_SAFE_INTEGER as u64
        || facts.authorization_revision > MAX_SAFE_INTEGER as u64
    {
        return Err(PerformanceEvaluationError::InvalidAuthorization);
    }
    let mut exchanges = BTreeSet::new();
    let mut usage_ids = BTreeSet::new();
    validate_model_call_authority(
        &facts.primary_model_calls,
        &spec.attempt_policy.primary,
        &spec.primary_planned_routes,
        true,
        facts.evidence_cutoff.retry_ledger_cursor,
        &mut exchanges,
        &mut usage_ids,
    )?;
    match &spec.attempt_policy.observer {
        Some(plan) => validate_model_call_authority(
            &facts.observer_model_calls,
            plan,
            &spec.observer.planned_routes,
            true,
            facts.evidence_cutoff.retry_ledger_cursor,
            &mut exchanges,
            &mut usage_ids,
        )?,
        None if facts.observer_model_calls.is_empty() => {}
        None => return Err(PerformanceEvaluationError::InvalidAuthorization),
    }
    let retry_ledger_cursor = facts
        .primary_model_calls
        .iter()
        .chain(&facts.observer_model_calls)
        .map(|call| call.retry_state_revision)
        .max()
        .ok_or(PerformanceEvaluationError::InvalidAuthorization)?;
    if retry_ledger_cursor != facts.evidence_cutoff.retry_ledger_cursor {
        return Err(PerformanceEvaluationError::InvalidAuthorization);
    }
    Ok(())
}

fn validate_evidence_cutoff(
    cutoff: &EvaluationEvidenceCutoffV1,
    assignment_cutoff_at_millis: u64,
) -> Result<(), PerformanceEvaluationError> {
    for digest in [
        &cutoff.worker_ledger_snapshot_digest,
        &cutoff.artifact_snapshot_digest,
    ] {
        validate_digest(digest)?;
    }
    if cutoff.cutoff_at_millis != assignment_cutoff_at_millis
        || cutoff.control_plane_terminal_cursor == 0
        || cutoff.control_plane_terminal_cursor > MAX_SAFE_INTEGER as u64
        || cutoff.retry_ledger_cursor == 0
        || cutoff.retry_ledger_cursor > MAX_SAFE_INTEGER as u64
        || cutoff.candidate_ack_cursor == 0
        || cutoff.candidate_ack_cursor > MAX_SAFE_INTEGER as u64
        || cutoff.artifact_acknowledged_sequence == 0
        || cutoff.artifact_acknowledged_sequence > MAX_SAFE_INTEGER as u64
    {
        return Err(PerformanceEvaluationError::InvalidAuthorization);
    }
    Ok(())
}

fn validate_model_call_authority(
    calls: &[EvaluationModelCallAuthorityV1],
    expected_plan: &EvaluationRetryPlanV1,
    planned: &[EvaluationRouteV1],
    required: bool,
    retry_ledger_cursor: u64,
    exchanges: &mut BTreeSet<String>,
    usage_ids: &mut BTreeSet<String>,
) -> Result<(), PerformanceEvaluationError> {
    if (required && calls.is_empty()) || calls.len() > MAX_PAIRS {
        return Err(PerformanceEvaluationError::InvalidRouteAttempt);
    }
    let mut call_digests = BTreeSet::new();
    for call in calls {
        validate_digest(&call.model_call_digest)?;
        if !call_digests.insert(call.model_call_digest.0.clone())
            || call.retry_state_revision == 0
            || call.retry_state_revision > retry_ledger_cursor
            || &call.retry_plan != expected_plan
            || call.attempts.is_empty()
        {
            return Err(PerformanceEvaluationError::InvalidRouteAttempt);
        }
        let mut expected_position = (0_u32, 1_u32);
        for (index, attempt) in call.attempts.iter().enumerate() {
            let step_index = usize::try_from(attempt.step_index)
                .map_err(|_| PerformanceEvaluationError::InvalidRouteAttempt)?;
            let step = expected_plan
                .steps
                .get(step_index)
                .ok_or(PerformanceEvaluationError::InvalidRouteAttempt)?;
            let route_index = usize::try_from(step.route_index)
                .map_err(|_| PerformanceEvaluationError::InvalidRouteAttempt)?;
            let is_last = index + 1 == call.attempts.len();
            if attempt.ordinal != u32::try_from(index + 1).unwrap_or(u32::MAX)
                || (attempt.step_index, attempt.attempt_on_step) != expected_position
                || planned.get(route_index) != Some(&attempt.route)
                || !exchanges.insert(attempt.provider_exchange_digest.0.clone())
                || (is_last && attempt.outcome != EvaluationAttemptOutcomeV1::Succeeded)
                || (!is_last && attempt.outcome == EvaluationAttemptOutcomeV1::Succeeded)
            {
                return Err(PerformanceEvaluationError::InvalidRouteAttempt);
            }
            validate_digest(&attempt.provider_exchange_digest)?;
            validate_settled_usage(attempt, usage_ids)?;
            expected_position = if attempt.attempt_on_step < step.maximum_attempts {
                (attempt.step_index, attempt.attempt_on_step + 1)
            } else {
                (attempt.step_index + 1, 1)
            };
        }
    }
    Ok(())
}

fn validate_settled_usage(
    attempt: &EvaluationRouteAttemptV1,
    usage_ids: &mut BTreeSet<String>,
) -> Result<(), PerformanceEvaluationError> {
    let usage_required = matches!(
        attempt.outcome,
        EvaluationAttemptOutcomeV1::FailedCharged | EvaluationAttemptOutcomeV1::Succeeded
    );
    if usage_required != attempt.settled_usage.is_some() {
        return Err(PerformanceEvaluationError::InvalidRouteAttempt);
    }
    let invalid_usage = attempt.settled_usage.as_ref().is_some_and(|usage| {
        usage.provider_usage_id.is_empty()
            || usage.provider_usage_id.len() > MAX_IDENTITY_TEXT
            || usage.provider_id != attempt.route.provider_id
            || usage.model_id != attempt.route.model_id
            || !usage_ids.insert(usage.provider_usage_id.clone())
            || usage.cached_input_tokens > usage.input_tokens
            || usage.cache_write_input_tokens > usage.input_tokens
            || usage.reasoning_output_tokens > usage.output_tokens
            || usage.input_tokens.checked_add(usage.output_tokens) != Some(usage.total_tokens)
            || usage.total_tokens > MAX_SAFE_INTEGER as u64
            || usage.cost_microunits > MAX_SAFE_INTEGER as u64
    });
    if invalid_usage {
        return Err(PerformanceEvaluationError::InvalidRouteAttempt);
    }
    Ok(())
}

fn one_run_summary(
    run: &PerformanceV0RunEvidence,
    model_calls: &[PerformanceV0ModelCallEvidence],
) -> Result<PerformanceV0ArmSummary, PerformanceEvaluationError> {
    if run.execution_mode == ExecutionMode::DelegatedPatchShadow
        || model_calls.iter().any(|call| call.run_id != run.run_id)
    {
        return Err(PerformanceEvaluationError::InvalidMeasurement);
    }
    let comparison = summarize_performance_v0(std::slice::from_ref(run), model_calls)
        .map_err(|_| PerformanceEvaluationError::InvalidMeasurement)?;
    match run.execution_mode {
        ExecutionMode::React
            if comparison.react.sample_count == 1 && comparison.structured.sample_count == 0 =>
        {
            Ok(comparison.react)
        }
        ExecutionMode::DelegatedPatch
            if comparison.react.sample_count == 0 && comparison.structured.sample_count == 1 =>
        {
            Ok(comparison.structured)
        }
        ExecutionMode::React
        | ExecutionMode::DelegatedPatch
        | ExecutionMode::DelegatedPatchShadow => {
            Err(PerformanceEvaluationError::InvalidMeasurement)
        }
    }
}

fn validate_arm(
    authorization: &EvaluationAuthorizationV1,
    measurement: &PerformanceArmMeasurementV1,
    expected_arm: EvaluationArmV1,
) -> Result<(), PerformanceEvaluationError> {
    authorization.validate()?;
    measurement.validate()?;
    let spec = authorization.assignment.spec();
    let expected_mode = match expected_arm {
        EvaluationArmV1::React => ExecutionMode::React,
        EvaluationArmV1::Delegated => ExecutionMode::DelegatedPatch,
    };
    if spec.arm != expected_arm
        || measurement.run.execution_mode != expected_mode
        || measurement.run.observer_mode != spec.observer.mode
        || measurement.run.run_id != spec.run_id
    {
        return Err(PerformanceEvaluationError::MismatchedPair);
    }
    validate_call_attempts(
        measurement,
        &authorization.facts.primary_model_calls,
        PerformanceV0ModelKind::Primary,
    )?;
    validate_call_attempts(
        measurement,
        &authorization.facts.observer_model_calls,
        PerformanceV0ModelKind::Observer,
    )?;
    Ok(())
}

fn validate_call_attempts(
    measurement: &PerformanceArmMeasurementV1,
    calls: &[EvaluationModelCallAuthorityV1],
    kind: PerformanceV0ModelKind,
) -> Result<(), PerformanceEvaluationError> {
    let attempted = calls
        .iter()
        .map(|call| call.model_call_digest.0.as_str())
        .collect::<BTreeSet<_>>();
    let measured = measurement
        .model_calls
        .iter()
        .filter(|call| call.model_kind == kind)
        .map(|call| call.model_call_id.0.as_str())
        .collect::<BTreeSet<_>>();
    if attempted != measured {
        return Err(PerformanceEvaluationError::InvalidRouteAttempt);
    }
    Ok(())
}

fn validate_pair_identity(
    react: &EvaluationAuthorizationV1,
    delegated: &EvaluationAuthorizationV1,
) -> Result<(), PerformanceEvaluationError> {
    let left = react.assignment.spec();
    let right = delegated.assignment.spec();
    if left.repository_scope != right.repository_scope
        || left.source_release != right.source_release
        || left.cohort_manifest != right.cohort_manifest
        || left.cohort_id != right.cohort_id
        || left.case_id != right.case_id
        || left.pair_id != right.pair_id
        || left.base_revision != right.base_revision
        || left.primary_planned_routes != right.primary_planned_routes
        || left.observer != right.observer
        || left.attempt_policy != right.attempt_policy
        || left.policy_revision != right.policy_revision
        || left.policy_digest != right.policy_digest
        || left.cutoff_at_millis != right.cutoff_at_millis
        || left.job_id == right.job_id
        || left.run_id == right.run_id
    {
        return Err(PerformanceEvaluationError::MismatchedPair);
    }
    Ok(())
}

fn pair_digest(
    react_authorization: &EvaluationAuthorizationV1,
    react_measurement: &PerformanceArmMeasurementV1,
    delegated_authorization: &EvaluationAuthorizationV1,
    delegated_measurement: &PerformanceArmMeasurementV1,
) -> Result<Sha256Digest, PerformanceEvaluationError> {
    digest_json(&PairDigestFacts {
        react_authorization: react_authorization.digest(),
        react_measurement: react_measurement.digest(),
        delegated_authorization: delegated_authorization.digest(),
        delegated_measurement: delegated_measurement.digest(),
    })
}

fn cohort_key(pair: &PerformancePairedSampleV1) -> Vec<u8> {
    let spec = pair.react_authorization.assignment.spec();
    serde_json::to_vec(&(
        &spec.repository_scope,
        &spec.source_release,
        &spec.cohort_manifest,
        &spec.cohort_id,
        spec.policy_revision,
        &spec.policy_digest,
        spec.cutoff_at_millis,
    ))
    .unwrap_or_default()
}

fn metric_value(
    metric: PerformanceEvaluationMetricV1,
    authorization: &EvaluationAuthorizationV1,
    summary: &PerformanceV0ArmSummary,
) -> Result<i64, PerformanceEvaluationError> {
    match metric {
        PerformanceEvaluationMetricV1::StrongModelCalls => Ok(summary.strong_model_call_count),
        PerformanceEvaluationMetricV1::TotalTokens => Ok(summary.total_tokens),
        PerformanceEvaluationMetricV1::ModelWaitMillis => summary
            .total_strong_model_wait_ms
            .checked_add(summary.total_observer_model_wait_ms)
            .ok_or(PerformanceEvaluationError::MetricOverflow),
        PerformanceEvaluationMetricV1::WallClockRuntimeMillis => {
            i64::try_from(authorization.wall_clock_runtime_millis())
                .map_err(|_| PerformanceEvaluationError::MetricOverflow)
        }
        PerformanceEvaluationMetricV1::SettledCostMicrounits => Ok(summary.settled_cost_microunits),
    }
}

fn validate_artifact(artifact: &ArtifactReference) -> Result<(), PerformanceEvaluationError> {
    if artifact.artifact_id.0.is_empty() {
        return Err(PerformanceEvaluationError::InvalidIdentity);
    }
    validate_digest(&artifact.digest)
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), PerformanceEvaluationError> {
    let Some(hex) = digest.0.strip_prefix("sha256:") else {
        return Err(PerformanceEvaluationError::InvalidIdentity);
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PerformanceEvaluationError::InvalidIdentity);
    }
    Ok(())
}

fn digest_json(value: &impl Serialize) -> Result<Sha256Digest, PerformanceEvaluationError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| PerformanceEvaluationError::InvalidIdentity)?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}
