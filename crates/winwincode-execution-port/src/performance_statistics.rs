// SPDX-License-Identifier: Apache-2.0

//! Deterministic fixed-point statistics for paired performance samples.
//!
//! This module deliberately knows nothing about rollout authority. The
//! performance-evaluation reducer first proves that every input is an exact,
//! authorized React/Delegated pair, then passes one metric at a time here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{RepositoryScope, Sha256Digest};

use crate::generated::ArtifactReference;
use crate::performance_evaluation::{
    EvaluationAssignmentSpecV1, EvaluationAssignmentV1, EvaluationAttemptPolicyV1,
    EvaluationObserverV1, EvaluationRetryPlanV1, EvaluationRouteV1, PerformanceEvaluationError,
    PerformanceEvaluationMetricV1, PerformancePairedSampleV1, summarize_authorized_pairs_v1,
};
use crate::runtime_trace_outbox::ObserverMode;

const BASIS_POINTS: i128 = 10_000;
/// Sentinel used when React measured zero and Delegated measured more than zero.
pub const UNBOUNDED_REGRESSION_BASIS_POINTS: i64 = 9_007_199_254_740_991;
const MAX_SAFE_INTEGER: i64 = UNBOUNDED_REGRESSION_BASIS_POINTS;
const MAX_PAIR_COUNT: usize = 4_096;
const MIN_BOOTSTRAP_RESAMPLES: u32 = 100;
const MAX_BOOTSTRAP_RESAMPLES: u32 = 100_000;
const MAX_BOOTSTRAP_DRAWS: u64 = 50_000_000;
const MAX_REGRESSION_BASIS_POINTS: i64 = 1_000_000;
const MAX_IDENTITY_TEXT: usize = 512;

const EVALUATION_METRICS: [PerformanceEvaluationMetricV1; 5] = [
    PerformanceEvaluationMetricV1::StrongModelCalls,
    PerformanceEvaluationMetricV1::TotalTokens,
    PerformanceEvaluationMetricV1::ModelWaitMillis,
    PerformanceEvaluationMetricV1::WallClockRuntimeMillis,
    PerformanceEvaluationMetricV1::SettledCostMicrounits,
];

/// One already-authorized control/candidate value for the same case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PairedMetricValue {
    pub react: i64,
    pub delegated: i64,
}

/// Reproducible statistics for one metric across exact pairs.
///
/// The paired mean is represented without floating point as
/// `paired_delta_total / sample_count`. Regression and confidence bounds use
/// basis points, where 10,000 is 100 percent. `MAX_SAFE_INTEGER` represents an
/// unbounded regression when the React total is zero and Delegated is not.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairedMetricStatistics {
    pub sample_count: u32,
    pub react_total: i64,
    pub delegated_total: i64,
    pub paired_delta_total: i64,
    pub paired_delta_p50: i64,
    pub paired_delta_p95: i64,
    pub observed_regression_basis_points: i64,
    pub bootstrap_lower_basis_points: i64,
    pub bootstrap_upper_basis_points: i64,
    pub bootstrap_seed: u64,
    pub bootstrap_resamples: u32,
    pub confidence_basis_points: u16,
}

/// Stable failure categories for deterministic statistics input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerformanceStatisticsError {
    InvalidPairCount,
    InvalidMetric,
    InvalidBootstrapConfiguration,
    MetricOverflow,
}

impl fmt::Display for PerformanceStatisticsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPairCount => {
                "paired statistics sample count is outside the supported range"
            }
            Self::InvalidMetric => "paired statistics contain an invalid metric",
            Self::InvalidBootstrapConfiguration => {
                "paired statistics bootstrap configuration is invalid"
            }
            Self::MetricOverflow => "paired statistics exceed the exact integer range",
        })
    }
}

impl std::error::Error for PerformanceStatisticsError {}

/// Derives the fixed bootstrap seed from frozen authority digests.
///
/// Callers cannot choose a favorable seed after observing samples: policy,
/// cohort manifest, and source release identities determine it completely.
#[must_use]
pub fn derive_bootstrap_seed(
    policy_digest: &Sha256Digest,
    cohort_manifest_digest: &Sha256Digest,
    source_release_digest: &Sha256Digest,
) -> u64 {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.performance-bootstrap-seed.v1");
    for value in [policy_digest, cohort_manifest_digest, source_release_digest] {
        digest.update(
            u64::try_from(value.0.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(value.0.as_bytes());
    }
    digest_seed(digest)
}

/// Derives an independent deterministic stream for one typed metric.
#[must_use]
pub fn derive_metric_seed(cohort_seed: u64, metric_discriminant: u8) -> u64 {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.performance-bootstrap-metric.v1");
    digest.update(cohort_seed.to_be_bytes());
    digest.update([metric_discriminant]);
    digest_seed(digest)
}

fn digest_seed(digest: Sha256) -> u64 {
    let [
        first,
        second,
        third,
        fourth,
        fifth,
        sixth,
        seventh,
        eighth,
        ..,
    ] = <[u8; 32]>::from(digest.finalize());
    u64::from_be_bytes([first, second, third, fourth, fifth, sixth, seventh, eighth])
}

/// Calculates checked paired deltas, quantiles, and a percentile bootstrap
/// interval using one fixed pseudo-random stream.
///
/// The interval is one-sided for decision purposes: the upper bound is the
/// configured confidence quantile and the lower bound is its complementary
/// quantile. Rollout policy compares only the upper bound with its threshold.
///
/// # Errors
///
/// Rejects fewer than two or too many pairs, invalid/non-exact metrics,
/// unbounded bootstrap settings, and integer overflow.
pub fn calculate_paired_metric_statistics(
    values: &[PairedMetricValue],
    bootstrap_seed: u64,
    bootstrap_resamples: u32,
    confidence_basis_points: u16,
) -> Result<PairedMetricStatistics, PerformanceStatisticsError> {
    validate_inputs(values, bootstrap_resamples, confidence_basis_points)?;
    let (react_total, delegated_total, mut deltas) = metric_totals(values)?;
    deltas.sort_unstable();
    let observed_regression_basis_points = regression_basis_points(delegated_total, react_total)?;
    let mut bootstrap = bootstrap_regressions(values, bootstrap_seed, bootstrap_resamples)?;
    bootstrap.sort_unstable();
    let lower_quantile = 10_000_u16
        .checked_sub(confidence_basis_points)
        .ok_or(PerformanceStatisticsError::InvalidBootstrapConfiguration)?;
    let lower_index = quantile_index(bootstrap.len(), lower_quantile);
    let upper_index = quantile_index(bootstrap.len(), confidence_basis_points);
    Ok(PairedMetricStatistics {
        sample_count: u32::try_from(values.len())
            .map_err(|_| PerformanceStatisticsError::MetricOverflow)?,
        react_total,
        delegated_total,
        paired_delta_total: checked_metric(i128::from(delegated_total) - i128::from(react_total))?,
        paired_delta_p50: deltas[quantile_index(deltas.len(), 5_000)],
        paired_delta_p95: deltas[quantile_index(deltas.len(), 9_500)],
        observed_regression_basis_points,
        bootstrap_lower_basis_points: bootstrap[lower_index],
        bootstrap_upper_basis_points: bootstrap[upper_index],
        bootstrap_seed,
        bootstrap_resamples,
        confidence_basis_points,
    })
}

fn validate_inputs(
    values: &[PairedMetricValue],
    bootstrap_resamples: u32,
    confidence_basis_points: u16,
) -> Result<(), PerformanceStatisticsError> {
    if !(2..=MAX_PAIR_COUNT).contains(&values.len()) {
        return Err(PerformanceStatisticsError::InvalidPairCount);
    }
    if !(MIN_BOOTSTRAP_RESAMPLES..=MAX_BOOTSTRAP_RESAMPLES).contains(&bootstrap_resamples)
        || !(5_001..10_000).contains(&confidence_basis_points)
        || u64::from(bootstrap_resamples)
            .checked_mul(
                u64::try_from(values.len())
                    .map_err(|_| PerformanceStatisticsError::InvalidBootstrapConfiguration)?,
            )
            .is_none_or(|draws| draws > MAX_BOOTSTRAP_DRAWS)
    {
        return Err(PerformanceStatisticsError::InvalidBootstrapConfiguration);
    }
    if values.iter().any(|value| {
        !(0..=MAX_SAFE_INTEGER).contains(&value.react)
            || !(0..=MAX_SAFE_INTEGER).contains(&value.delegated)
    }) {
        return Err(PerformanceStatisticsError::InvalidMetric);
    }
    Ok(())
}

fn metric_totals(
    values: &[PairedMetricValue],
) -> Result<(i64, i64, Vec<i64>), PerformanceStatisticsError> {
    let mut react_total = 0_i128;
    let mut delegated_total = 0_i128;
    let mut deltas = Vec::with_capacity(values.len());
    for value in values {
        react_total = react_total
            .checked_add(i128::from(value.react))
            .ok_or(PerformanceStatisticsError::MetricOverflow)?;
        delegated_total = delegated_total
            .checked_add(i128::from(value.delegated))
            .ok_or(PerformanceStatisticsError::MetricOverflow)?;
        deltas.push(checked_metric(
            i128::from(value.delegated) - i128::from(value.react),
        )?);
    }
    Ok((
        checked_non_negative_metric(react_total)?,
        checked_non_negative_metric(delegated_total)?,
        deltas,
    ))
}

fn bootstrap_regressions(
    values: &[PairedMetricValue],
    seed: u64,
    resamples: u32,
) -> Result<Vec<i64>, PerformanceStatisticsError> {
    let mut generator = SplitMix64(seed);
    let mut results = Vec::with_capacity(
        usize::try_from(resamples).map_err(|_| PerformanceStatisticsError::MetricOverflow)?,
    );
    let pair_count =
        u64::try_from(values.len()).map_err(|_| PerformanceStatisticsError::MetricOverflow)?;
    for _ in 0..resamples {
        let mut react_total = 0_i128;
        let mut delegated_total = 0_i128;
        for _ in values {
            let index = usize::try_from(generator.next() % pair_count)
                .map_err(|_| PerformanceStatisticsError::MetricOverflow)?;
            react_total += i128::from(values[index].react);
            delegated_total += i128::from(values[index].delegated);
        }
        results.push(regression_basis_points_i128(delegated_total, react_total)?);
    }
    Ok(results)
}

fn regression_basis_points(delegated: i64, react: i64) -> Result<i64, PerformanceStatisticsError> {
    regression_basis_points_i128(i128::from(delegated), i128::from(react))
}

fn regression_basis_points_i128(
    delegated: i128,
    react: i128,
) -> Result<i64, PerformanceStatisticsError> {
    if react == 0 {
        return Ok(if delegated == 0 { 0 } else { MAX_SAFE_INTEGER });
    }
    let numerator = delegated
        .checked_sub(react)
        .and_then(|delta| delta.checked_mul(BASIS_POINTS))
        .ok_or(PerformanceStatisticsError::MetricOverflow)?;
    let rounded_up = if numerator >= 0 {
        numerator
            .checked_add(react - 1)
            .ok_or(PerformanceStatisticsError::MetricOverflow)?
            / react
    } else {
        numerator / react
    };
    checked_metric(rounded_up)
}

fn checked_non_negative_metric(value: i128) -> Result<i64, PerformanceStatisticsError> {
    if !(0..=i128::from(MAX_SAFE_INTEGER)).contains(&value) {
        return Err(PerformanceStatisticsError::MetricOverflow);
    }
    i64::try_from(value).map_err(|_| PerformanceStatisticsError::MetricOverflow)
}

fn checked_metric(value: i128) -> Result<i64, PerformanceStatisticsError> {
    if !(-i128::from(MAX_SAFE_INTEGER)..=i128::from(MAX_SAFE_INTEGER)).contains(&value) {
        return Err(PerformanceStatisticsError::MetricOverflow);
    }
    i64::try_from(value).map_err(|_| PerformanceStatisticsError::MetricOverflow)
}

fn quantile_index(length: usize, quantile_basis_points: u16) -> usize {
    let numerator = length * usize::from(quantile_basis_points);
    numerator.div_ceil(10_000).saturating_sub(1).min(length - 1)
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

/// The only estimator version accepted by the V1 rollout decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceEstimatorV1 {
    PairedPercentileBootstrapV1,
}

/// One predeclared metric limit from the frozen rollout policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceMetricThresholdV1 {
    metric: PerformanceEvaluationMetricV1,
    maximum_regression_basis_points: i64,
}

impl PerformanceMetricThresholdV1 {
    /// Creates one bounded typed threshold.
    ///
    /// # Errors
    ///
    /// Rejects a negative or excessive regression allowance.
    pub fn try_new(
        metric: PerformanceEvaluationMetricV1,
        maximum_regression_basis_points: i64,
    ) -> Result<Self, PerformanceDecisionError> {
        if !(0..=MAX_REGRESSION_BASIS_POINTS).contains(&maximum_regression_basis_points) {
            return Err(PerformanceDecisionError::InvalidPolicy);
        }
        Ok(Self {
            metric,
            maximum_regression_basis_points,
        })
    }

    #[must_use]
    pub const fn metric(&self) -> PerformanceEvaluationMetricV1 {
        self.metric
    }

    #[must_use]
    pub const fn maximum_regression_basis_points(&self) -> i64 {
        self.maximum_regression_basis_points
    }
}

/// One exact case identity predeclared in the cohort manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedPerformancePairV1 {
    pub pair_id: Sha256Digest,
    pub case_id: Sha256Digest,
    pub base_revision: String,
}

/// Sample manifest and statistical rules frozen before authority is issued.
///
/// Repository and policy revision/digest are supplied only by the durable gate
/// when this plan is sealed. They cannot be caller-authored inside the plan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceStatisticalPlanInputV1 {
    pub source_release: ArtifactReference,
    pub cohort_manifest: ArtifactReference,
    pub cohort_id: Sha256Digest,
    pub cutoff_at_millis: u64,
    pub primary_planned_routes: Vec<EvaluationRouteV1>,
    pub observer: EvaluationObserverV1,
    pub attempt_policy: EvaluationAttemptPolicyV1,
    pub expected_pairs: Vec<ExpectedPerformancePairV1>,
    pub minimum_complete_pair_count: u32,
    pub estimator: PerformanceEstimatorV1,
    pub bootstrap_resamples: u32,
    pub confidence_basis_points: u16,
    pub thresholds: Vec<PerformanceMetricThresholdV1>,
}

impl PerformanceStatisticalPlanInputV1 {
    /// Validates the sample manifest and statistical bounds before a gate
    /// binds repository or policy authority.
    ///
    /// # Errors
    ///
    /// Rejects malformed source/cohort identities, fewer than two planned
    /// pairs, duplicate cases, incomplete thresholds, or excessive work.
    pub fn validate(&self) -> Result<(), PerformanceDecisionError> {
        validate_plan(self)
    }
}

/// Frozen sample plan and statistical decision rules.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceStatisticalPolicyV1 {
    repository_scope: RepositoryScope,
    policy_revision: u64,
    policy_digest: Sha256Digest,
    plan: PerformanceStatisticalPlanInputV1,
}

impl PerformanceStatisticalPolicyV1 {
    /// Seals a deterministic paired sample plan under durable gate authority.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, fewer than two planned pairs, duplicate
    /// cases, incomplete thresholds, and excessive bootstrap work.
    pub fn seal(
        repository_scope: RepositoryScope,
        policy_revision: u64,
        policy_digest: Sha256Digest,
        plan: PerformanceStatisticalPlanInputV1,
    ) -> Result<Self, PerformanceDecisionError> {
        plan.validate()?;
        let policy = Self {
            repository_scope,
            policy_revision,
            policy_digest,
            plan,
        };
        policy.validate()?;
        Ok(policy)
    }

    /// Revalidates a deserialized statistical policy.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, sample plans, thresholds, or bounds.
    pub fn validate(&self) -> Result<(), PerformanceDecisionError> {
        validate_policy(self)
    }

    /// Verifies that a pre-run assignment is inside this exact frozen plan.
    ///
    /// # Errors
    ///
    /// Rejects malformed assignments, unknown pairs/cases, different source
    /// or cohort facts, different routes, or policy/cutoff mismatches.
    pub fn authorizes_assignment(
        &self,
        assignment: &EvaluationAssignmentV1,
    ) -> Result<(), PerformanceDecisionError> {
        self.validate()?;
        assignment.validate()?;
        let spec = assignment.spec();
        let expected = self
            .plan
            .expected_pairs
            .iter()
            .find(|expected| expected.pair_id == spec.pair_id)
            .ok_or(PerformanceDecisionError::UnauthorizedPair)?;
        if assignment_matches_policy(spec, self, expected) {
            Ok(())
        } else {
            Err(PerformanceDecisionError::UnauthorizedPair)
        }
    }

    #[must_use]
    pub const fn repository_scope(&self) -> &RepositoryScope {
        &self.repository_scope
    }

    #[must_use]
    pub const fn source_release(&self) -> &ArtifactReference {
        &self.plan.source_release
    }

    #[must_use]
    pub const fn cohort_manifest(&self) -> &ArtifactReference {
        &self.plan.cohort_manifest
    }

    #[must_use]
    pub const fn cohort_id(&self) -> &Sha256Digest {
        &self.plan.cohort_id
    }

    #[must_use]
    pub const fn plan(&self) -> &PerformanceStatisticalPlanInputV1 {
        &self.plan
    }

    #[must_use]
    pub fn expected_pairs(&self) -> &[ExpectedPerformancePairV1] {
        &self.plan.expected_pairs
    }

    #[must_use]
    pub const fn policy_revision(&self) -> u64 {
        self.policy_revision
    }

    #[must_use]
    pub const fn policy_digest(&self) -> &Sha256Digest {
        &self.policy_digest
    }

    #[must_use]
    pub const fn minimum_complete_pair_count(&self) -> u32 {
        self.plan.minimum_complete_pair_count
    }

    #[must_use]
    pub fn thresholds(&self) -> &[PerformanceMetricThresholdV1] {
        &self.plan.thresholds
    }
}

/// Decision result produced only from the frozen policy and raw authorized pairs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceEvaluationOutcomeV1 {
    Go,
    NoGo,
    InsufficientEvidence,
}

/// Bounded reason codes retained with the reproducible report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceDecisionReasonV1 {
    ExpectedPairsMissing,
    MinimumPairsNotMet,
    IncompleteModelCalls,
    UnpricedModelCalls,
    DuplicateLedgerWrites,
    MetricThresholdExceeded,
    AllChecksPassed,
}

/// Why one authorized pair was excluded from statistics.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformancePairExclusionReasonV1 {
    IncompleteModelCalls,
    UnpricedModelCalls,
    DuplicateLedgerWrites,
}

/// Deterministic count for one excluded-pair reason.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformancePairExclusionCountV1 {
    pub reason: PerformancePairExclusionReasonV1,
    pub pair_count: u32,
}

/// Threshold result for one required metric.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceMetricDecisionV1 {
    pub metric: PerformanceEvaluationMetricV1,
    pub maximum_regression_basis_points: i64,
    pub statistics: PairedMetricStatistics,
    pub passed: bool,
}

/// Canonical audit report for one frozen cohort evaluation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PerformanceEvaluationReportV1 {
    pub outcome: PerformanceEvaluationOutcomeV1,
    pub reason_codes: Vec<PerformanceDecisionReasonV1>,
    pub policy_revision: u64,
    pub policy_digest: Sha256Digest,
    pub source_release_digest: Sha256Digest,
    pub cohort_manifest_digest: Sha256Digest,
    pub cohort_id: Sha256Digest,
    pub cutoff_at_millis: u64,
    pub expected_pair_count: u32,
    pub received_pair_count: u32,
    pub complete_pair_count: u32,
    pub missing_pair_count: u32,
    pub excluded_pair_count: u32,
    pub exact_replay_count: u32,
    pub excluded_reasons: Vec<PerformancePairExclusionCountV1>,
    pub authorized_sample_digests: Vec<Sha256Digest>,
    pub evaluated_sample_digests: Vec<Sha256Digest>,
    pub estimator: PerformanceEstimatorV1,
    pub bootstrap_seed: u64,
    pub bootstrap_resamples: u32,
    pub confidence_basis_points: u16,
    pub metrics: Vec<PerformanceMetricDecisionV1>,
    report_digest: Sha256Digest,
}

impl PerformanceEvaluationReportV1 {
    /// Revalidates the canonical report digest.
    ///
    /// # Errors
    ///
    /// Rejects a report whose retained facts were changed after evaluation.
    pub fn validate(&self) -> Result<(), PerformanceDecisionError> {
        if self.report_digest != report_digest(self)? {
            return Err(PerformanceDecisionError::DigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.report_digest
    }
}

/// Stable failure categories for the closed statistical reducer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerformanceDecisionError {
    InvalidPolicy,
    UnauthorizedPair,
    ConflictingReplay,
    Evaluation(PerformanceEvaluationError),
    Statistics(PerformanceStatisticsError),
    DigestMismatch,
    MetricOverflow,
}

impl fmt::Display for PerformanceDecisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPolicy => "performance statistical policy is invalid",
            Self::UnauthorizedPair => "performance pair is outside the frozen sample plan",
            Self::ConflictingReplay => "performance pair replay conflicts with retained evidence",
            Self::Evaluation(_) => "performance pair authority is invalid",
            Self::Statistics(_) => "performance paired statistics are invalid",
            Self::DigestMismatch => "performance report digest does not match its facts",
            Self::MetricOverflow => "performance report count exceeds the supported range",
        })
    }
}

impl std::error::Error for PerformanceDecisionError {}

impl From<PerformanceEvaluationError> for PerformanceDecisionError {
    fn from(error: PerformanceEvaluationError) -> Self {
        Self::Evaluation(error)
    }
}

impl From<PerformanceStatisticsError> for PerformanceDecisionError {
    fn from(error: PerformanceStatisticsError) -> Self {
        Self::Statistics(error)
    }
}

/// Evaluates one frozen manifest from exact authorized raw pairs.
///
/// Exact retries do not increase sample size. Missing, incomplete, unpriced,
/// or duplicate-ledger pairs produce `InsufficientEvidence`. A sufficient
/// cohort produces `Go` only when every one-sided bootstrap upper bound is at
/// or below its predeclared threshold.
///
/// # Errors
///
/// Rejects malformed policy, foreign pairs, conflicting replays, invalid raw
/// evidence, or arithmetic overflow.
pub fn evaluate_authorized_pairs_v1(
    policy: &PerformanceStatisticalPolicyV1,
    pairs: &[PerformancePairedSampleV1],
) -> Result<PerformanceEvaluationReportV1, PerformanceDecisionError> {
    policy.validate()?;
    let (unique_pairs, exact_replay_count) = collect_unique_pairs(policy, pairs)?;
    if unique_pairs.len() >= 2 {
        summarize_authorized_pairs_v1(
            &unique_pairs.values().copied().cloned().collect::<Vec<_>>(),
        )?;
    }
    let classified = classify_pairs(&unique_pairs);
    let expected_pair_count = count(policy.plan.expected_pairs.len())?;
    let received_pair_count = count(unique_pairs.len())?;
    let complete_pair_count = count(classified.complete.len())?;
    let excluded_pair_count = count(classified.excluded_pair_count)?;
    let missing_pair_count = expected_pair_count
        .checked_sub(received_pair_count)
        .ok_or(PerformanceDecisionError::MetricOverflow)?;
    let bootstrap_seed = derive_bootstrap_seed(
        &policy.policy_digest,
        &policy.plan.cohort_manifest.digest,
        &policy.plan.source_release.digest,
    );
    let metrics = if classified.complete.len() >= 2 {
        metric_decisions(policy, &classified.complete, bootstrap_seed)?
    } else {
        Vec::new()
    };
    let sufficient = missing_pair_count == 0
        && excluded_pair_count == 0
        && complete_pair_count >= policy.plan.minimum_complete_pair_count;
    let threshold_exceeded = metrics.iter().any(|metric| !metric.passed);
    let outcome = if !sufficient {
        PerformanceEvaluationOutcomeV1::InsufficientEvidence
    } else if threshold_exceeded {
        PerformanceEvaluationOutcomeV1::NoGo
    } else {
        PerformanceEvaluationOutcomeV1::Go
    };
    let reason_codes = decision_reasons(
        outcome,
        missing_pair_count,
        complete_pair_count,
        policy.plan.minimum_complete_pair_count,
        &classified.reason_counts,
        threshold_exceeded,
    );
    let mut report = PerformanceEvaluationReportV1 {
        outcome,
        reason_codes,
        policy_revision: policy.policy_revision,
        policy_digest: policy.policy_digest.clone(),
        source_release_digest: policy.plan.source_release.digest.clone(),
        cohort_manifest_digest: policy.plan.cohort_manifest.digest.clone(),
        cohort_id: policy.plan.cohort_id.clone(),
        cutoff_at_millis: policy.plan.cutoff_at_millis,
        expected_pair_count,
        received_pair_count,
        complete_pair_count,
        missing_pair_count,
        excluded_pair_count,
        exact_replay_count,
        excluded_reasons: exclusion_counts(&classified.reason_counts)?,
        authorized_sample_digests: unique_pairs
            .values()
            .map(|pair| pair.digest().clone())
            .collect(),
        evaluated_sample_digests: classified
            .complete
            .iter()
            .map(|pair| pair.digest().clone())
            .collect(),
        estimator: policy.plan.estimator,
        bootstrap_seed,
        bootstrap_resamples: policy.plan.bootstrap_resamples,
        confidence_basis_points: policy.plan.confidence_basis_points,
        metrics,
        report_digest: Sha256Digest(String::new()),
    };
    report.report_digest = report_digest(&report)?;
    Ok(report)
}

struct ClassifiedPairs<'pairs> {
    complete: Vec<&'pairs PerformancePairedSampleV1>,
    excluded_pair_count: usize,
    reason_counts: BTreeMap<PerformancePairExclusionReasonV1, usize>,
}

fn validate_policy(
    policy: &PerformanceStatisticalPolicyV1,
) -> Result<(), PerformanceDecisionError> {
    if !valid_scope(&policy.repository_scope)
        || !valid_digest(&policy.policy_digest)
        || policy.policy_revision == 0
        || policy.policy_revision > MAX_SAFE_INTEGER as u64
    {
        return Err(PerformanceDecisionError::InvalidPolicy);
    }
    policy.plan.validate()
}

fn validate_plan(plan: &PerformanceStatisticalPlanInputV1) -> Result<(), PerformanceDecisionError> {
    if !valid_artifact(&plan.source_release)
        || !valid_artifact(&plan.cohort_manifest)
        || !valid_digest(&plan.cohort_id)
        || plan.cutoff_at_millis == 0
        || plan.cutoff_at_millis > MAX_SAFE_INTEGER as u64
        || !(2..=MAX_PAIR_COUNT).contains(&plan.expected_pairs.len())
        || plan.minimum_complete_pair_count < 2
        || usize::try_from(plan.minimum_complete_pair_count)
            .ok()
            .is_none_or(|minimum| minimum > plan.expected_pairs.len())
        || !(MIN_BOOTSTRAP_RESAMPLES..=MAX_BOOTSTRAP_RESAMPLES).contains(&plan.bootstrap_resamples)
        || !(5_001..10_000).contains(&plan.confidence_basis_points)
        || u64::from(plan.bootstrap_resamples)
            .checked_mul(u64::try_from(plan.expected_pairs.len()).unwrap_or(u64::MAX))
            .is_none_or(|draws| draws > MAX_BOOTSTRAP_DRAWS)
    {
        return Err(PerformanceDecisionError::InvalidPolicy);
    }
    validate_route_policy(&plan.primary_planned_routes, &plan.observer)?;
    validate_attempt_policy(
        &plan.attempt_policy,
        &plan.primary_planned_routes,
        &plan.observer,
    )?;
    validate_expected_pairs(&plan.expected_pairs)?;
    validate_thresholds(&plan.thresholds)
}

fn validate_attempt_policy(
    policy: &EvaluationAttemptPolicyV1,
    primary_routes: &[EvaluationRouteV1],
    observer: &EvaluationObserverV1,
) -> Result<(), PerformanceDecisionError> {
    if policy.logical_sample_count != 1 {
        return Err(PerformanceDecisionError::InvalidPolicy);
    }
    validate_retry_plan(&policy.primary, primary_routes)?;
    match (&policy.observer, observer.mode) {
        (None, ObserverMode::Off) => Ok(()),
        (Some(plan), ObserverMode::Shadow | ObserverMode::AmbiguousOnly | ObserverMode::Always) => {
            validate_retry_plan(plan, &observer.planned_routes)
        }
        (None, _) | (Some(_), ObserverMode::Off) => Err(PerformanceDecisionError::InvalidPolicy),
    }
}

fn validate_retry_plan(
    plan: &EvaluationRetryPlanV1,
    routes: &[EvaluationRouteV1],
) -> Result<(), PerformanceDecisionError> {
    let mut route_indices = BTreeSet::new();
    let mut total_attempts = 0_u32;
    if plan.policy_revision == 0
        || plan.policy_revision > MAX_SAFE_INTEGER as u64
        || !valid_digest(&plan.plan_fingerprint)
        || plan.steps.is_empty()
        || plan.steps.len() > 16
    {
        return Err(PerformanceDecisionError::InvalidPolicy);
    }
    for step in &plan.steps {
        let route_index = usize::try_from(step.route_index)
            .map_err(|_| PerformanceDecisionError::InvalidPolicy)?;
        if routes.get(route_index).is_none()
            || !route_indices.insert(step.route_index)
            || !(1..=16).contains(&step.maximum_attempts)
        {
            return Err(PerformanceDecisionError::InvalidPolicy);
        }
        total_attempts = total_attempts
            .checked_add(step.maximum_attempts)
            .ok_or(PerformanceDecisionError::InvalidPolicy)?;
    }
    if total_attempts > 16 {
        return Err(PerformanceDecisionError::InvalidPolicy);
    }
    Ok(())
}

fn validate_expected_pairs(
    pairs: &[ExpectedPerformancePairV1],
) -> Result<(), PerformanceDecisionError> {
    let mut pair_ids = BTreeSet::new();
    let mut case_ids = BTreeSet::new();
    for pair in pairs {
        if !valid_digest(&pair.pair_id)
            || !valid_digest(&pair.case_id)
            || pair.base_revision.is_empty()
            || pair.base_revision.len() > MAX_IDENTITY_TEXT
            || !pair_ids.insert(pair.pair_id.0.as_str())
            || !case_ids.insert(pair.case_id.0.as_str())
        {
            return Err(PerformanceDecisionError::InvalidPolicy);
        }
    }
    if pairs
        .windows(2)
        .any(|pair| pair[0].pair_id.0 > pair[1].pair_id.0)
    {
        return Err(PerformanceDecisionError::InvalidPolicy);
    }
    Ok(())
}

fn validate_route_policy(
    primary: &[EvaluationRouteV1],
    observer: &EvaluationObserverV1,
) -> Result<(), PerformanceDecisionError> {
    if primary.is_empty()
        || primary.len() > 16
        || observer.planned_routes.len() > 16
        || (observer.mode == ObserverMode::Off && !observer.planned_routes.is_empty())
        || (observer.mode != ObserverMode::Off && observer.planned_routes.is_empty())
    {
        return Err(PerformanceDecisionError::InvalidPolicy);
    }
    validate_route_group(primary)?;
    validate_route_group(&observer.planned_routes)
}

fn validate_route_group(routes: &[EvaluationRouteV1]) -> Result<(), PerformanceDecisionError> {
    let mut route_digests = BTreeSet::new();
    for route in routes {
        if route.provider_id.is_empty()
            || route.provider_id.len() > MAX_IDENTITY_TEXT
            || route.model_id.is_empty()
            || route.model_id.len() > MAX_IDENTITY_TEXT
            || !valid_digest(&route.route_digest)
            || !route_digests.insert(route.route_digest.0.as_str())
        {
            return Err(PerformanceDecisionError::InvalidPolicy);
        }
    }
    Ok(())
}

fn validate_thresholds(
    thresholds: &[PerformanceMetricThresholdV1],
) -> Result<(), PerformanceDecisionError> {
    if thresholds.len() != EVALUATION_METRICS.len()
        || thresholds
            .iter()
            .zip(EVALUATION_METRICS)
            .any(|(threshold, metric)| {
                threshold.metric != metric
                    || !(0..=MAX_REGRESSION_BASIS_POINTS)
                        .contains(&threshold.maximum_regression_basis_points)
            })
    {
        return Err(PerformanceDecisionError::InvalidPolicy);
    }
    Ok(())
}

fn collect_unique_pairs<'pairs>(
    policy: &PerformanceStatisticalPolicyV1,
    pairs: &'pairs [PerformancePairedSampleV1],
) -> Result<(BTreeMap<String, &'pairs PerformancePairedSampleV1>, u32), PerformanceDecisionError> {
    let expected = policy
        .plan
        .expected_pairs
        .iter()
        .map(|pair| (pair.pair_id.0.as_str(), pair))
        .collect::<BTreeMap<_, _>>();
    let mut unique = BTreeMap::<String, &'pairs PerformancePairedSampleV1>::new();
    let mut replay_count = 0_u32;
    for pair in pairs {
        pair.validate()?;
        let spec = pair.react_authorization().assignment().spec();
        let expected_pair = expected
            .get(spec.pair_id.0.as_str())
            .ok_or(PerformanceDecisionError::UnauthorizedPair)?;
        if !pair_matches_policy(pair, policy, expected_pair) {
            return Err(PerformanceDecisionError::UnauthorizedPair);
        }
        if let Some(retained) = unique.get(&spec.pair_id.0) {
            if retained.digest() != pair.digest() {
                return Err(PerformanceDecisionError::ConflictingReplay);
            }
            replay_count = replay_count
                .checked_add(1)
                .ok_or(PerformanceDecisionError::MetricOverflow)?;
        } else {
            unique.insert(spec.pair_id.0.clone(), pair);
        }
    }
    Ok((unique, replay_count))
}

fn pair_matches_policy(
    pair: &PerformancePairedSampleV1,
    policy: &PerformanceStatisticalPolicyV1,
    expected: &ExpectedPerformancePairV1,
) -> bool {
    assignment_matches_policy(
        pair.react_authorization().assignment().spec(),
        policy,
        expected,
    )
}

fn assignment_matches_policy(
    spec: &EvaluationAssignmentSpecV1,
    policy: &PerformanceStatisticalPolicyV1,
    expected: &ExpectedPerformancePairV1,
) -> bool {
    spec.repository_scope == policy.repository_scope
        && spec.source_release == policy.plan.source_release
        && spec.cohort_manifest == policy.plan.cohort_manifest
        && spec.cohort_id == policy.plan.cohort_id
        && spec.pair_id == expected.pair_id
        && spec.case_id == expected.case_id
        && spec.base_revision == expected.base_revision
        && spec.policy_revision == policy.policy_revision
        && spec.policy_digest == policy.policy_digest
        && spec.cutoff_at_millis == policy.plan.cutoff_at_millis
        && spec.primary_planned_routes == policy.plan.primary_planned_routes
        && spec.observer == policy.plan.observer
        && spec.attempt_policy == policy.plan.attempt_policy
}

fn classify_pairs<'pairs>(
    pairs: &BTreeMap<String, &'pairs PerformancePairedSampleV1>,
) -> ClassifiedPairs<'pairs> {
    let mut complete = Vec::new();
    let mut excluded_pair_count = 0;
    let mut reason_counts = BTreeMap::new();
    for pair in pairs.values() {
        let mut reasons = BTreeSet::new();
        for summary in [
            pair.react_measurement().summary(),
            pair.delegated_measurement().summary(),
        ] {
            if summary.incomplete_strong_model_call_count > 0
                || summary.incomplete_observer_model_call_count > 0
            {
                reasons.insert(PerformancePairExclusionReasonV1::IncompleteModelCalls);
            }
            if summary.unpriced_completed_call_count > 0 {
                reasons.insert(PerformancePairExclusionReasonV1::UnpricedModelCalls);
            }
            if summary.duplicate_run_write_count > 0
                || summary.duplicate_model_call_write_count > 0
                || summary.duplicate_settled_charge_write_count > 0
            {
                reasons.insert(PerformancePairExclusionReasonV1::DuplicateLedgerWrites);
            }
        }
        if reasons.is_empty() {
            complete.push(*pair);
        } else {
            excluded_pair_count += 1;
            for reason in reasons {
                *reason_counts.entry(reason).or_insert(0) += 1;
            }
        }
    }
    ClassifiedPairs {
        complete,
        excluded_pair_count,
        reason_counts,
    }
}

fn metric_decisions(
    policy: &PerformanceStatisticalPolicyV1,
    pairs: &[&PerformancePairedSampleV1],
    cohort_seed: u64,
) -> Result<Vec<PerformanceMetricDecisionV1>, PerformanceDecisionError> {
    EVALUATION_METRICS
        .into_iter()
        .zip(&policy.plan.thresholds)
        .map(|(metric, threshold)| {
            let values = pairs
                .iter()
                .map(|pair| pair.metric(metric))
                .collect::<Result<Vec<_>, _>>()?;
            let statistics = calculate_paired_metric_statistics(
                &values,
                derive_metric_seed(cohort_seed, metric.discriminant()),
                policy.plan.bootstrap_resamples,
                policy.plan.confidence_basis_points,
            )?;
            let passed = statistics.bootstrap_upper_basis_points
                <= threshold.maximum_regression_basis_points;
            Ok(PerformanceMetricDecisionV1 {
                metric,
                maximum_regression_basis_points: threshold.maximum_regression_basis_points,
                statistics,
                passed,
            })
        })
        .collect()
}

fn decision_reasons(
    outcome: PerformanceEvaluationOutcomeV1,
    missing_pairs: u32,
    complete_pairs: u32,
    minimum_pairs: u32,
    exclusions: &BTreeMap<PerformancePairExclusionReasonV1, usize>,
    threshold_exceeded: bool,
) -> Vec<PerformanceDecisionReasonV1> {
    if outcome == PerformanceEvaluationOutcomeV1::Go {
        return vec![PerformanceDecisionReasonV1::AllChecksPassed];
    }
    let mut reasons = BTreeSet::new();
    if missing_pairs > 0 {
        reasons.insert(PerformanceDecisionReasonV1::ExpectedPairsMissing);
    }
    if complete_pairs < minimum_pairs {
        reasons.insert(PerformanceDecisionReasonV1::MinimumPairsNotMet);
    }
    if exclusions.contains_key(&PerformancePairExclusionReasonV1::IncompleteModelCalls) {
        reasons.insert(PerformanceDecisionReasonV1::IncompleteModelCalls);
    }
    if exclusions.contains_key(&PerformancePairExclusionReasonV1::UnpricedModelCalls) {
        reasons.insert(PerformanceDecisionReasonV1::UnpricedModelCalls);
    }
    if exclusions.contains_key(&PerformancePairExclusionReasonV1::DuplicateLedgerWrites) {
        reasons.insert(PerformanceDecisionReasonV1::DuplicateLedgerWrites);
    }
    if threshold_exceeded {
        reasons.insert(PerformanceDecisionReasonV1::MetricThresholdExceeded);
    }
    reasons.into_iter().collect()
}

fn exclusion_counts(
    reasons: &BTreeMap<PerformancePairExclusionReasonV1, usize>,
) -> Result<Vec<PerformancePairExclusionCountV1>, PerformanceDecisionError> {
    reasons
        .iter()
        .map(|(reason, count_value)| {
            Ok(PerformancePairExclusionCountV1 {
                reason: *reason,
                pair_count: count(*count_value)?,
            })
        })
        .collect()
}

fn report_digest(
    report: &PerformanceEvaluationReportV1,
) -> Result<Sha256Digest, PerformanceDecisionError> {
    let mut unsigned = report.clone();
    unsigned.report_digest = Sha256Digest(String::new());
    let bytes =
        serde_json::to_vec(&unsigned).map_err(|_| PerformanceDecisionError::DigestMismatch)?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn count(value: usize) -> Result<u32, PerformanceDecisionError> {
    u32::try_from(value).map_err(|_| PerformanceDecisionError::MetricOverflow)
}

fn valid_scope(scope: &RepositoryScope) -> bool {
    !scope.organization_id.0.is_empty()
        && !scope.workspace_id.0.is_empty()
        && !scope.project_id.0.is_empty()
        && !scope.repository_id.0.is_empty()
}

fn valid_artifact(artifact: &ArtifactReference) -> bool {
    !artifact.artifact_id.0.is_empty() && valid_digest(&artifact.digest)
}

fn valid_digest(digest: &Sha256Digest) -> bool {
    digest.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        ArtifactId, ExecutionJobId, OrganizationId, ProjectId, RepositoryId, RepositoryScopeKind,
        WorkspaceId,
    };

    use crate::performance_comparison::{
        PerformanceV0ModelCallEvidence, PerformanceV0ModelKind, PerformanceV0RunEvidence,
    };
    use crate::performance_evaluation::{
        EvaluationArmV1, EvaluationAssignmentSpecV1, EvaluationAssignmentV1,
        EvaluationAttemptOutcomeV1, EvaluationAuthorizationFactsV1, EvaluationAuthorizationV1,
        EvaluationEvidenceCutoffV1, EvaluationModelCallAuthorityV1, EvaluationObserverV1,
        EvaluationRetryStepV1, EvaluationRouteAttemptV1, EvaluationSettledUsageV1,
        PerformanceArmMeasurementV1,
    };
    use crate::runtime_trace_outbox::ExecutionMode;

    use super::*;

    #[test]
    fn paired_statistics_are_deterministic_and_fail_closed_at_zero_baseline() {
        let values = [
            PairedMetricValue {
                react: 100,
                delegated: 90,
            },
            PairedMetricValue {
                react: 200,
                delegated: 240,
            },
            PairedMetricValue {
                react: 300,
                delegated: 270,
            },
        ];
        let first = calculate_paired_metric_statistics(&values, 7, 1_000, 9_500)
            .expect("calculate paired statistics");
        let replay = calculate_paired_metric_statistics(&values, 7, 1_000, 9_500)
            .expect("replay paired statistics");
        assert_eq!(first, replay);
        assert_eq!(first.sample_count, 3);
        assert_eq!(first.react_total, 600);
        assert_eq!(first.delegated_total, 600);
        assert_eq!(first.paired_delta_total, 0);
        assert_eq!(first.paired_delta_p50, -10);
        assert_eq!(first.paired_delta_p95, 40);
        assert_eq!(first.observed_regression_basis_points, 0);
        assert!(first.bootstrap_lower_basis_points <= first.bootstrap_upper_basis_points);

        let zero_baseline = calculate_paired_metric_statistics(
            &[
                PairedMetricValue {
                    react: 0,
                    delegated: 0,
                },
                PairedMetricValue {
                    react: 0,
                    delegated: 1,
                },
            ],
            9,
            100,
            9_500,
        )
        .expect("calculate zero-baseline statistics");
        assert_eq!(
            zero_baseline.observed_regression_basis_points,
            MAX_SAFE_INTEGER
        );
        assert_eq!(zero_baseline.bootstrap_upper_basis_points, MAX_SAFE_INTEGER);
    }

    #[test]
    fn authorized_pair_decision_covers_go_no_go_and_insufficient() {
        let policy = policy();
        let passing = [paired_sample(1, 100, 90), paired_sample(2, 100, 90)];
        policy
            .authorizes_assignment(passing[0].react_authorization().assignment())
            .expect("assignment is inside the frozen plan");
        let foreign = assignment(3, EvaluationArmV1::React, 0, digest(900));
        assert_eq!(
            policy.authorizes_assignment(&foreign),
            Err(PerformanceDecisionError::UnauthorizedPair)
        );
        let report = evaluate_authorized_pairs_v1(
            &policy,
            &[passing[0].clone(), passing[0].clone(), passing[1].clone()],
        )
        .expect("evaluate sufficient paired cohort");
        assert_eq!(report.outcome, PerformanceEvaluationOutcomeV1::Go);
        assert_eq!(report.expected_pair_count, 2);
        assert_eq!(report.complete_pair_count, 2);
        assert_eq!(report.exact_replay_count, 1);
        assert_eq!(report.metrics.len(), EVALUATION_METRICS.len());
        assert!(report.metrics.iter().all(|metric| metric.passed));
        report.validate().expect("validate decision report");

        let assignment = passing[0].react_authorization().assignment().clone();
        let mut drifted_cutoff = passing[0].react_authorization().facts().clone();
        drifted_cutoff
            .evidence_cutoff
            .control_plane_terminal_cursor += 1;
        assert_eq!(
            EvaluationAuthorizationV1::try_new(assignment, drifted_cutoff),
            Err(PerformanceEvaluationError::InvalidAuthorization),
            "an unrelated source cursor cannot be substituted for terminal authority"
        );

        let insufficient = evaluate_authorized_pairs_v1(&policy, &passing[..1])
            .expect("evaluate incomplete manifest");
        assert_eq!(
            insufficient.outcome,
            PerformanceEvaluationOutcomeV1::InsufficientEvidence
        );
        assert_eq!(insufficient.missing_pair_count, 1);

        let regressed = [paired_sample(1, 100, 120), paired_sample(2, 100, 120)];
        let no_go =
            evaluate_authorized_pairs_v1(&policy, &regressed).expect("evaluate regressed cohort");
        assert_eq!(no_go.outcome, PerformanceEvaluationOutcomeV1::NoGo);
        let runtime = no_go
            .metrics
            .iter()
            .find(|metric| metric.metric == PerformanceEvaluationMetricV1::WallClockRuntimeMillis)
            .expect("wall-clock metric decision");
        assert!(!runtime.passed);
        assert_eq!(runtime.statistics.observed_regression_basis_points, 2_000);
    }

    fn policy() -> PerformanceStatisticalPolicyV1 {
        PerformanceStatisticalPolicyV1::seal(
            scope(),
            1,
            digest(4),
            PerformanceStatisticalPlanInputV1 {
                source_release: artifact(1),
                cohort_manifest: artifact(2),
                cohort_id: digest(3),
                cutoff_at_millis: 1_000_000,
                primary_planned_routes: vec![route()],
                observer: EvaluationObserverV1 {
                    mode: ObserverMode::Off,
                    planned_routes: Vec::new(),
                },
                attempt_policy: attempt_policy(),
                expected_pairs: [1_u64, 2]
                    .into_iter()
                    .map(|index| ExpectedPerformancePairV1 {
                        pair_id: digest(100 + index),
                        case_id: digest(200 + index),
                        base_revision: format!("base-{index}"),
                    })
                    .collect(),
                minimum_complete_pair_count: 2,
                estimator: PerformanceEstimatorV1::PairedPercentileBootstrapV1,
                bootstrap_resamples: 1_000,
                confidence_basis_points: 9_500,
                thresholds: EVALUATION_METRICS
                    .into_iter()
                    .map(|metric| {
                        PerformanceMetricThresholdV1::try_new(metric, 0)
                            .expect("valid zero-regression threshold")
                    })
                    .collect(),
            },
        )
        .expect("valid statistical policy")
    }

    fn paired_sample(
        index: u64,
        react_runtime_millis: u64,
        delegated_runtime_millis: u64,
    ) -> PerformancePairedSampleV1 {
        let (react_authorization, react_measurement) = arm_sample(
            index,
            EvaluationArmV1::React,
            ExecutionMode::React,
            ArmMetrics {
                tokens: 30,
                wait_millis: 10,
                cost_microunits: 20,
                wall_clock_runtime_millis: react_runtime_millis,
            },
        );
        let (delegated_authorization, delegated_measurement) = arm_sample(
            index,
            EvaluationArmV1::Delegated,
            ExecutionMode::DelegatedPatch,
            ArmMetrics {
                tokens: 15,
                wait_millis: 5,
                cost_microunits: 10,
                wall_clock_runtime_millis: delegated_runtime_millis,
            },
        );
        PerformancePairedSampleV1::try_new(
            react_authorization,
            react_measurement,
            delegated_authorization,
            delegated_measurement,
        )
        .expect("valid paired sample")
    }

    #[derive(Clone, Copy)]
    struct ArmMetrics {
        tokens: i64,
        wait_millis: i64,
        cost_microunits: i64,
        wall_clock_runtime_millis: u64,
    }

    fn arm_sample(
        index: u64,
        arm: EvaluationArmV1,
        execution_mode: ExecutionMode,
        metrics: ArmMetrics,
    ) -> (EvaluationAuthorizationV1, PerformanceArmMeasurementV1) {
        let arm_offset = match arm {
            EvaluationArmV1::React => 0,
            EvaluationArmV1::Delegated => 10,
        };
        let run_id = digest(300 + index * 20 + arm_offset);
        let model_call_id = digest(400 + index * 20 + arm_offset);
        let assignment = assignment(index, arm, arm_offset, run_id.clone());
        let retry_plan = assignment.spec().attempt_policy.primary.clone();
        let dispatched = 10_000 + index * 1_000;
        let settled_tokens = u64::try_from(metrics.tokens).expect("non-negative fixture tokens");
        let settled_cost =
            u64::try_from(metrics.cost_microunits).expect("non-negative fixture cost");
        let authorization = EvaluationAuthorizationV1::try_new(
            assignment,
            EvaluationAuthorizationFactsV1 {
                candidate_artifact: artifact(500 + index * 20 + arm_offset),
                evidence_cutoff: EvaluationEvidenceCutoffV1 {
                    cutoff_at_millis: 1_000_000,
                    control_plane_terminal_cursor: 1,
                    retry_ledger_cursor: 1,
                    candidate_ack_cursor: 1,
                    artifact_acknowledged_sequence: 1,
                    worker_ledger_snapshot_digest: digest(700 + index * 20 + arm_offset),
                    artifact_snapshot_digest: digest(800 + index * 20 + arm_offset),
                },
                candidate_artifact_ack_revision: 1,
                dispatch_accepted_at_millis: dispatched,
                worker_terminal_finished_at_millis: dispatched + metrics.wall_clock_runtime_millis
                    - 1,
                terminal_accepted_at_millis: dispatched + metrics.wall_clock_runtime_millis,
                terminal_revision: 1,
                authorization_revision: 1,
                primary_model_calls: vec![EvaluationModelCallAuthorityV1 {
                    model_call_digest: model_call_id.clone(),
                    retry_state_revision: 1,
                    retry_plan,
                    attempts: vec![EvaluationRouteAttemptV1 {
                        ordinal: 1,
                        step_index: 0,
                        attempt_on_step: 1,
                        route: route(),
                        provider_exchange_digest: digest(600 + index * 20 + arm_offset),
                        outcome: EvaluationAttemptOutcomeV1::Succeeded,
                        settled_usage: Some(EvaluationSettledUsageV1 {
                            provider_usage_id: format!("usage-{index}-{arm_offset}"),
                            provider_id: "provider-fixture".to_owned(),
                            model_id: "model-fixture".to_owned(),
                            input_tokens: settled_tokens,
                            cached_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 0,
                            reasoning_output_tokens: 0,
                            total_tokens: settled_tokens,
                            cost_microunits: settled_cost,
                        }),
                    }],
                }],
                observer_model_calls: Vec::new(),
            },
        )
        .expect("valid arm authorization");
        let run = PerformanceV0RunEvidence {
            run_id: run_id.clone(),
            execution_mode,
            observer_mode: ObserverMode::Off,
            primary_model_call_count: 1,
            primary_model_input_tokens: metrics.tokens,
            primary_model_cached_tokens: 0,
            primary_model_output_tokens: 0,
            primary_model_wait_ms: metrics.wait_millis,
            observer_call_count: 0,
            observer_wait_ms: 0,
            total_runtime_ms: metrics.wait_millis,
        };
        let call = PerformanceV0ModelCallEvidence {
            run_id,
            model_call_id,
            model_kind: PerformanceV0ModelKind::Primary,
            completed: true,
            input_tokens: metrics.tokens,
            cached_tokens: 0,
            output_tokens: 0,
            elapsed_millis: metrics.wait_millis,
            actual_cost_microunits: Some(metrics.cost_microunits),
        };
        let measurement =
            PerformanceArmMeasurementV1::from_v0(run, vec![call]).expect("valid arm measurement");
        (authorization, measurement)
    }

    fn assignment(
        index: u64,
        arm: EvaluationArmV1,
        arm_offset: u64,
        run_id: Sha256Digest,
    ) -> EvaluationAssignmentV1 {
        EvaluationAssignmentV1::try_new(EvaluationAssignmentSpecV1 {
            repository_scope: scope(),
            source_release: artifact(1),
            cohort_manifest: artifact(2),
            cohort_id: digest(3),
            case_id: digest(200 + index),
            pair_id: digest(100 + index),
            arm,
            base_revision: format!("base-{index}"),
            job_id: ExecutionJobId(format!("job_{index}_{arm_offset}")),
            run_id,
            primary_planned_routes: vec![route()],
            observer: EvaluationObserverV1 {
                mode: ObserverMode::Off,
                planned_routes: Vec::new(),
            },
            attempt_policy: attempt_policy(),
            policy_revision: 1,
            policy_digest: digest(4),
            cutoff_at_millis: 1_000_000,
        })
        .expect("valid arm assignment")
    }

    fn scope() -> RepositoryScope {
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId("org_1".to_owned()),
            workspace_id: WorkspaceId("wsp_1".to_owned()),
            project_id: ProjectId("prj_1".to_owned()),
            repository_id: RepositoryId("rep_1".to_owned()),
        }
    }

    fn route() -> EvaluationRouteV1 {
        EvaluationRouteV1 {
            provider_id: "provider-fixture".to_owned(),
            model_id: "model-fixture".to_owned(),
            route_digest: digest(5),
        }
    }

    fn attempt_policy() -> EvaluationAttemptPolicyV1 {
        EvaluationAttemptPolicyV1 {
            logical_sample_count: 1,
            primary: EvaluationRetryPlanV1 {
                policy_revision: 1,
                plan_fingerprint: digest(6),
                steps: vec![EvaluationRetryStepV1 {
                    route_index: 0,
                    maximum_attempts: 16,
                }],
            },
            observer: None,
        }
    }

    fn artifact(index: u64) -> ArtifactReference {
        ArtifactReference {
            artifact_id: ArtifactId(format!("art_{index}")),
            digest: digest(700 + index),
        }
    }

    fn digest(value: u64) -> Sha256Digest {
        Sha256Digest(format!("sha256:{value:064x}"))
    }
}
