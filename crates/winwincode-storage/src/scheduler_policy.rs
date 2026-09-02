// SPDX-License-Identifier: Apache-2.0

//! Deterministic scheduling policy layered over durable execution queue records.
//!
//! The policy never reads a Codex plan and never writes storage. Callers use the
//! selected job identity and revision to perform the durable `queued -> leased`
//! transition through [`crate::ExecutionQueue`].

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use winwincode_domain::{ExecutionJobId, OrganizationId, ProductSessionId, ProjectId, StageRunId};

use crate::{ExecutionJobRecord, ExecutionJobState, ExecutionQueueScope};

/// Closed, explicit scheduler priority. A larger rank wins unless starvation
/// protection has made an older job urgent.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SchedulerPriority {
    Low,
    Normal,
    High,
    Critical,
}

/// Relative fair-share weights for the three scheduler-owned scope levels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerWeights {
    pub organization: u32,
    pub project: u32,
    pub product_session: u32,
}

impl SchedulerWeights {
    /// Equal weight at every scheduler-owned scope level.
    pub const EQUAL: Self = Self {
        organization: 1,
        project: 1,
        product_session: 1,
    };
}

/// Queue record plus policy metadata that is deliberately absent from the
/// persistence contract.
#[derive(Clone, Copy, Debug)]
pub struct SchedulerCandidate<'record> {
    pub record: &'record ExecutionJobRecord,
    /// Delivery jobs reserve one `StageRun`; `ProductSession` Chat jobs use `None`.
    pub stage_run_id: Option<&'record StageRunId>,
    pub priority: SchedulerPriority,
    /// Monotonic scheduler tick when the job entered this queue tier.
    pub enqueued_at_tick: u64,
    /// Earliest monotonic scheduler tick at which a retry may run.
    pub eligible_at_tick: u64,
    pub weights: SchedulerWeights,
}

/// Durable transition input selected by the policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulerDispatch {
    pub scope: ExecutionQueueScope,
    pub job_id: ExecutionJobId,
    pub stage_run_id: Option<StageRunId>,
    pub attempt: u64,
    pub expected_revision: u64,
}

/// Parent whose cancellation must be expanded into concrete execution jobs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerCancellationTarget {
    Organization(OrganizationId),
    Project {
        organization_id: OrganizationId,
        project_id: ProjectId,
    },
    ProductSession {
        organization_id: OrganizationId,
        project_id: ProjectId,
        product_session_id: ProductSessionId,
    },
    ExecutionJob(ExecutionJobId),
}

/// Deterministically ordered jobs to receive durable cancellation requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulerCancellationPlan {
    pub job_ids: Vec<ExecutionJobId>,
}

/// Finite retry and capped exponential-backoff policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerRetryPolicy {
    pub max_attempts: u64,
    pub initial_backoff_ticks: u64,
    pub max_backoff_ticks: u64,
}

/// Scheduler decision after one failed dispatch attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerRetryDecision {
    Retry {
        next_attempt: u64,
        eligible_at_tick: u64,
    },
    Exhausted,
    PermanentFailure,
}

/// Invalid scheduler input or an inconsistent active-dispatch snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulerPolicyError {
    message: String,
}

impl SchedulerPolicyError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SchedulerPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SchedulerPolicyError {}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum FairnessKey {
    Organization(String),
    Project(String, String),
    ProductSession(String, String, String),
}

/// Stateful deterministic scheduler. Dispatch counters retain fair-share
/// history while active stage-run reservations prevent concurrent duplicates.
#[derive(Debug)]
pub struct SchedulerPolicy {
    starvation_threshold_ticks: u64,
    dispatch_counts: HashMap<FairnessKey, u64>,
    active_stage_runs: HashMap<String, ExecutionJobId>,
}

impl SchedulerPolicy {
    /// Creates a scheduler with a fixed starvation threshold in monotonic
    /// caller-owned ticks.
    #[must_use]
    pub fn new(starvation_threshold_ticks: u64) -> Self {
        Self {
            starvation_threshold_ticks,
            dispatch_counts: HashMap::new(),
            active_stage_runs: HashMap::new(),
        }
    }

    /// Selects one ready queue record. Priority is explicit; jobs waiting past
    /// the starvation threshold outrank fresh jobs; remaining ties use
    /// hierarchical weighted fairness across organization, project, and
    /// product session.
    ///
    /// The returned revision must be used for the durable lease transition.
    /// Until [`Self::release_stage_run`] is called or a terminal snapshot is
    /// observed, the policy will not select a second job for that stage run.
    ///
    /// # Errors
    ///
    /// Rejects duplicate records, zero/inconsistent weights, empty stage-run
    /// identities, and snapshots containing two active jobs for one stage run.
    pub fn select(
        &mut self,
        now_tick: u64,
        candidates: &[SchedulerCandidate<'_>],
    ) -> Result<Option<SchedulerDispatch>, SchedulerPolicyError> {
        let weights = validate_candidates(candidates)?;
        self.synchronize_active_stage_runs(candidates)?;

        let records = candidates
            .iter()
            .map(|candidate| (&candidate.record.job_id.0, candidate.record))
            .collect::<HashMap<_, _>>();
        let mut ready = candidates
            .iter()
            .filter(|candidate| {
                ready_for_dispatch(candidate, now_tick, &records, &self.active_stage_runs)
            })
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Ok(None);
        }

        let mut starved = ready
            .iter()
            .copied()
            .filter(|candidate| {
                now_tick.saturating_sub(candidate.enqueued_at_tick)
                    >= self.starvation_threshold_ticks
            })
            .collect::<Vec<_>>();
        let selected = if starved.is_empty() {
            let Some(highest_priority) = ready.iter().map(|candidate| candidate.priority).max()
            else {
                return Ok(None);
            };
            ready.retain(|candidate| candidate.priority == highest_priority);
            self.select_fair(&ready, &weights).ok_or_else(|| {
                SchedulerPolicyError::invalid("ready scheduler candidates could not be ranked")
            })?
        } else {
            starved.sort_unstable_by(|left, right| starvation_order(left, right, now_tick));
            starved.first().copied().ok_or_else(|| {
                SchedulerPolicyError::invalid("starved scheduler candidates could not be ranked")
            })?
        };

        self.record_dispatch(selected);
        Ok(Some(SchedulerDispatch {
            scope: selected.record.scope.clone(),
            job_id: selected.record.job_id.clone(),
            stage_run_id: selected.stage_run_id.cloned(),
            attempt: selected.record.attempt,
            expected_revision: selected.record.revision,
        }))
    }

    /// Releases an in-memory stage-run reservation only when both identities
    /// match the reservation created by [`Self::select`].
    #[must_use]
    pub fn release_stage_run(
        &mut self,
        stage_run_id: &StageRunId,
        job_id: &ExecutionJobId,
    ) -> bool {
        if self
            .active_stage_runs
            .get(&stage_run_id.0)
            .is_some_and(|active| active == job_id)
        {
            self.active_stage_runs.remove(&stage_run_id.0);
            true
        } else {
            false
        }
    }

    fn select_fair<'candidate, 'record>(
        &self,
        candidates: &'candidate [&'candidate SchedulerCandidate<'record>],
        weights: &HashMap<FairnessKey, u32>,
    ) -> Option<&'candidate SchedulerCandidate<'record>> {
        let organization = least_served_key(
            candidates
                .iter()
                .map(|candidate| organization_key(&candidate.record.scope)),
            &self.dispatch_counts,
            weights,
        )?;
        let organization_candidates = candidates
            .iter()
            .copied()
            .filter(|candidate| organization_key(&candidate.record.scope) == organization)
            .collect::<Vec<_>>();
        let project = least_served_key(
            organization_candidates
                .iter()
                .map(|candidate| project_key(&candidate.record.scope)),
            &self.dispatch_counts,
            weights,
        )?;
        let project_candidates = organization_candidates
            .into_iter()
            .filter(|candidate| project_key(&candidate.record.scope) == project)
            .collect::<Vec<_>>();
        let product_session = least_served_key(
            project_candidates
                .iter()
                .map(|candidate| product_session_key(&candidate.record.scope)),
            &self.dispatch_counts,
            weights,
        )?;

        project_candidates
            .into_iter()
            .filter(|candidate| product_session_key(&candidate.record.scope) == product_session)
            .min_by(|left, right| stable_candidate_order(left, right))
    }

    fn record_dispatch(&mut self, candidate: &SchedulerCandidate<'_>) {
        for key in [
            organization_key(&candidate.record.scope),
            project_key(&candidate.record.scope),
            product_session_key(&candidate.record.scope),
        ] {
            *self.dispatch_counts.entry(key).or_default() += 1;
        }
        if let Some(stage_run_id) = candidate.stage_run_id {
            self.active_stage_runs
                .insert(stage_run_id.0.clone(), candidate.record.job_id.clone());
        }
    }

    fn synchronize_active_stage_runs(
        &mut self,
        candidates: &[SchedulerCandidate<'_>],
    ) -> Result<(), SchedulerPolicyError> {
        let present_jobs = candidates
            .iter()
            .map(|candidate| candidate.record.job_id.0.as_str())
            .collect::<HashSet<_>>();
        self.active_stage_runs
            .retain(|_, job_id| present_jobs.contains(job_id.0.as_str()));

        for candidate in candidates {
            if matches!(
                candidate.record.state,
                ExecutionJobState::Leased
                    | ExecutionJobState::Running
                    | ExecutionJobState::Cancelling
            ) {
                let Some(stage_run_id) = candidate.stage_run_id else {
                    continue;
                };
                if let Some(active_job) = self.active_stage_runs.get(&stage_run_id.0) {
                    if active_job != &candidate.record.job_id {
                        return Err(SchedulerPolicyError::invalid(
                            "one stage run has multiple active execution jobs",
                        ));
                    }
                } else {
                    self.active_stage_runs
                        .insert(stage_run_id.0.clone(), candidate.record.job_id.clone());
                }
            } else if let Some(stage_run_id) = candidate.stage_run_id
                && matches!(
                    candidate.record.state,
                    ExecutionJobState::Completed | ExecutionJobState::Failed
                )
                && self
                    .active_stage_runs
                    .get(&stage_run_id.0)
                    .is_some_and(|job_id| job_id == &candidate.record.job_id)
            {
                self.active_stage_runs.remove(&stage_run_id.0);
            }
        }
        Ok(())
    }
}

/// Expands an organization, project, product-session, or execution-job parent
/// cancellation into all cancellable jobs and dependency descendants.
#[must_use]
pub fn plan_scheduler_cancellation(
    target: &SchedulerCancellationTarget,
    records: &[ExecutionJobRecord],
) -> SchedulerCancellationPlan {
    let mut selected = records
        .iter()
        .filter(|record| target_matches(target, record))
        .map(|record| record.job_id.0.clone())
        .collect::<HashSet<_>>();
    let mut pending = selected.iter().cloned().collect::<VecDeque<_>>();

    while let Some(parent) = pending.pop_front() {
        for record in records {
            if record
                .dependencies
                .iter()
                .any(|dependency| dependency.0 == parent)
                && selected.insert(record.job_id.0.clone())
            {
                pending.push_back(record.job_id.0.clone());
            }
        }
    }

    let mut job_ids = records
        .iter()
        .filter(|record| selected.contains(&record.job_id.0) && cancellable(record))
        .map(|record| record.job_id.clone())
        .collect::<Vec<_>>();
    job_ids.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    job_ids.dedup();
    SchedulerCancellationPlan { job_ids }
}

/// Produces a finite retry decision without mutating the failed record.
/// Callers create the next durable attempt only for [`SchedulerRetryDecision::Retry`].
///
/// # Errors
///
/// Rejects an invalid retry policy or a job outside an active/failed state.
pub fn scheduler_retry_decision(
    record: &ExecutionJobRecord,
    retryable_failure: bool,
    failed_at_tick: u64,
    policy: SchedulerRetryPolicy,
) -> Result<SchedulerRetryDecision, SchedulerPolicyError> {
    if policy.max_attempts == 0
        || policy.max_attempts > 1_000
        || policy.initial_backoff_ticks == 0
        || policy.max_backoff_ticks < policy.initial_backoff_ticks
    {
        return Err(SchedulerPolicyError::invalid(
            "scheduler retry policy is invalid",
        ));
    }
    if !matches!(
        record.state,
        ExecutionJobState::Leased | ExecutionJobState::Running | ExecutionJobState::Failed
    ) {
        return Err(SchedulerPolicyError::invalid(
            "only an active or failed execution job can be retried",
        ));
    }
    if !retryable_failure {
        return Ok(SchedulerRetryDecision::PermanentFailure);
    }
    if record.attempt >= policy.max_attempts {
        return Ok(SchedulerRetryDecision::Exhausted);
    }

    let exponent = u32::try_from(record.attempt.saturating_sub(1).min(63)).unwrap_or(63);
    let backoff = policy
        .initial_backoff_ticks
        .saturating_mul(1_u64 << exponent)
        .min(policy.max_backoff_ticks);
    Ok(SchedulerRetryDecision::Retry {
        next_attempt: record.attempt + 1,
        eligible_at_tick: failed_at_tick.saturating_add(backoff),
    })
}

fn validate_candidates(
    candidates: &[SchedulerCandidate<'_>],
) -> Result<HashMap<FairnessKey, u32>, SchedulerPolicyError> {
    let mut jobs = HashSet::new();
    let mut weights = HashMap::new();
    for candidate in candidates {
        if !jobs.insert(candidate.record.job_id.0.as_str()) {
            return Err(SchedulerPolicyError::invalid(
                "scheduler candidate job identities must be unique",
            ));
        }
        if candidate
            .stage_run_id
            .is_some_and(|stage_run_id| stage_run_id.0.is_empty())
        {
            return Err(SchedulerPolicyError::invalid(
                "scheduler candidate stage-run identity is empty",
            ));
        }
        if candidate.record.stage_run_id.as_ref() != candidate.stage_run_id {
            return Err(SchedulerPolicyError::invalid(
                "scheduler candidate reservation differs from its durable job",
            ));
        }
        for (key, weight) in [
            (
                organization_key(&candidate.record.scope),
                candidate.weights.organization,
            ),
            (
                project_key(&candidate.record.scope),
                candidate.weights.project,
            ),
            (
                product_session_key(&candidate.record.scope),
                candidate.weights.product_session,
            ),
        ] {
            if weight == 0 {
                return Err(SchedulerPolicyError::invalid(
                    "scheduler fair-share weight must be positive",
                ));
            }
            if weights
                .insert(key, weight)
                .is_some_and(|existing| existing != weight)
            {
                return Err(SchedulerPolicyError::invalid(
                    "one scheduler scope has inconsistent fair-share weights",
                ));
            }
        }
    }
    Ok(weights)
}

fn ready_for_dispatch(
    candidate: &SchedulerCandidate<'_>,
    now_tick: u64,
    records: &HashMap<&String, &ExecutionJobRecord>,
    active_stage_runs: &HashMap<String, ExecutionJobId>,
) -> bool {
    candidate.record.state == ExecutionJobState::Queued
        && candidate.record.cancellation.is_none()
        && candidate.eligible_at_tick <= now_tick
        && candidate
            .stage_run_id
            .is_none_or(|stage_run_id| !active_stage_runs.contains_key(&stage_run_id.0))
        && candidate.record.dependencies.iter().all(|dependency| {
            records.get(&dependency.0).is_some_and(|record| {
                record.state == ExecutionJobState::Completed && record.cancellation.is_none()
            })
        })
}

fn starvation_order(
    left: &SchedulerCandidate<'_>,
    right: &SchedulerCandidate<'_>,
    now_tick: u64,
) -> Ordering {
    now_tick
        .saturating_sub(right.enqueued_at_tick)
        .cmp(&now_tick.saturating_sub(left.enqueued_at_tick))
        .then_with(|| right.priority.cmp(&left.priority))
        .then_with(|| stable_candidate_order(left, right))
}

fn stable_candidate_order(
    left: &SchedulerCandidate<'_>,
    right: &SchedulerCandidate<'_>,
) -> Ordering {
    left.enqueued_at_tick
        .cmp(&right.enqueued_at_tick)
        .then_with(|| left.record.submitted_at.0.cmp(&right.record.submitted_at.0))
        .then_with(|| left.record.job_id.0.cmp(&right.record.job_id.0))
}

fn least_served_key(
    keys: impl Iterator<Item = FairnessKey>,
    dispatch_counts: &HashMap<FairnessKey, u64>,
    weights: &HashMap<FairnessKey, u32>,
) -> Option<FairnessKey> {
    let unique = keys.collect::<HashSet<_>>();
    unique.into_iter().min_by(|left, right| {
        let left_count = u128::from(*dispatch_counts.get(left).unwrap_or(&0));
        let right_count = u128::from(*dispatch_counts.get(right).unwrap_or(&0));
        let left_weight = u128::from(weights.get(left).copied().unwrap_or(1));
        let right_weight = u128::from(weights.get(right).copied().unwrap_or(1));
        (left_count * right_weight)
            .cmp(&(right_count * left_weight))
            .then_with(|| left.cmp(right))
    })
}

fn organization_key(scope: &ExecutionQueueScope) -> FairnessKey {
    FairnessKey::Organization(scope.organization_id.0.clone())
}

fn project_key(scope: &ExecutionQueueScope) -> FairnessKey {
    FairnessKey::Project(scope.organization_id.0.clone(), scope.project_id.0.clone())
}

fn product_session_key(scope: &ExecutionQueueScope) -> FairnessKey {
    FairnessKey::ProductSession(
        scope.organization_id.0.clone(),
        scope.project_id.0.clone(),
        scope.product_session_id.0.clone(),
    )
}

fn target_matches(target: &SchedulerCancellationTarget, record: &ExecutionJobRecord) -> bool {
    match target {
        SchedulerCancellationTarget::Organization(organization_id) => {
            &record.scope.organization_id == organization_id
        }
        SchedulerCancellationTarget::Project {
            organization_id,
            project_id,
        } => {
            &record.scope.organization_id == organization_id
                && &record.scope.project_id == project_id
        }
        SchedulerCancellationTarget::ProductSession {
            organization_id,
            project_id,
            product_session_id,
        } => {
            &record.scope.organization_id == organization_id
                && &record.scope.project_id == project_id
                && &record.scope.product_session_id == product_session_id
        }
        SchedulerCancellationTarget::ExecutionJob(job_id) => &record.job_id == job_id,
    }
}

fn cancellable(record: &ExecutionJobRecord) -> bool {
    record.cancellation.is_none()
        && matches!(
            record.state,
            ExecutionJobState::Queued | ExecutionJobState::Leased | ExecutionJobState::Running
        )
}
