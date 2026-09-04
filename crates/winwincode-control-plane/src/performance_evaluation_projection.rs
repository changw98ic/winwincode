// SPDX-License-Identifier: Apache-2.0

//! Closed projection of CP and Worker authority into one evaluation arm.
//!
//! The caller supplies only links read from the Worker's private durable
//! ledger. Dispatch, terminal acceptance, Provider attempts, and route facts
//! are reloaded from the Control Plane database before authorization is sealed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use sha2::{Digest as _, Sha256};
use winwincode_domain::{ModelExchangeId, RepositoryScope, RequestId, Sha256Digest};
use winwincode_execution_port::generated::ArtifactReference;
use winwincode_execution_port::performance_comparison::{
    PerformanceV0ModelCallEvidence, PerformanceV0ModelKind,
};
use winwincode_execution_port::performance_evaluation::{
    EvaluationAssignmentV1, EvaluationAttemptOutcomeV1, EvaluationAuthorizationFactsV1,
    EvaluationAuthorizationV1, EvaluationEvidenceCutoffV1, EvaluationModelCallAuthorityV1,
    EvaluationRetryPlanV1, EvaluationRouteAttemptV1, EvaluationRouteV1,
    EvaluationSettledUsageV1,
    PerformanceArmMeasurementV1, PerformancePairedSampleV1,
};
use winwincode_execution_port::runtime_trace_outbox::ObserverMode;
use winwincode_storage::{
    ArtifactError, ArtifactErrorKind, ArtifactObject, ArtifactStore, LocalArtifactObjectStore,
    ProductStateStorage, SqliteStorage, StorageErrorKind,
};

use crate::model_retry_usage::{
    EvaluationModelRequestProjection, EvaluationRouteAttemptStatusProjection,
    ModelRetryUsageErrorKind, load_evaluation_route_attempts,
};
use crate::session_binding_transaction::instant_millis;
use crate::terminal_outcome_transaction::load_evaluation_terminal_authority;

const AUTHORIZATION_REVISION_V1: u64 = 1;
const SOURCE_RELEASE_KIND: &str = "performance_source_release";
const SOURCE_RELEASE_MEDIA_TYPE: &str =
    "application/vnd.winwincode.performance-source-release+json";
const COHORT_MANIFEST_KIND: &str = "performance_cohort_manifest";
const COHORT_MANIFEST_MEDIA_TYPE: &str =
    "application/vnd.winwincode.performance-cohort-manifest+json";
const CANDIDATE_KIND: &str = "candidate";
const CANDIDATE_MEDIA_TYPE: &str = "application/vnd.winwincode.git-candidate+json";

/// Worker-owned durable link for one hashed Primary Model measurement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPerformanceModelCallAuthorityV1 {
    pub model_call_digest: Sha256Digest,
    pub request_id: RequestId,
    pub initial_model_exchange_id: ModelExchangeId,
}

/// Worker-owned Candidate ACK and model links for one exact arm.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerPerformanceArmAuthorityV1 {
    pub measurement: PerformanceArmMeasurementV1,
    pub candidate_artifact: ArtifactReference,
    pub candidate_artifact_ack_revision: u64,
    pub primary_model_calls: Vec<WorkerPerformanceModelCallAuthorityV1>,
    pub worker_ledger_snapshot_digest: Sha256Digest,
}

/// Exact React and Delegated assignments loaded from one durable pair slot.
///
/// This value has no public constructor. Production callers name only the
/// repository, policy revision, and pair id; the Control Plane reloads both
/// assignments from its one-shot slot authority.
#[derive(Clone, Debug)]
pub struct ConsumedPerformanceEvaluationPair {
    react: EvaluationAssignmentV1,
    delegated: EvaluationAssignmentV1,
}

impl ConsumedPerformanceEvaluationPair {
    #[must_use]
    pub const fn react(&self) -> &EvaluationAssignmentV1 {
        &self.react
    }

    #[must_use]
    pub const fn delegated(&self) -> &EvaluationAssignmentV1 {
        &self.delegated
    }
}

/// Opaque proof that policy artifacts were read and re-hashed from the exact
/// repository-owned Artifact store before policy admission.
#[derive(Clone, Debug)]
pub struct ValidatedPerformancePolicyArtifacts {
    scope: RepositoryScope,
    source_release: ArtifactReference,
    cohort_manifest: ArtifactReference,
    cutoff_at_millis: u64,
}

impl ValidatedPerformancePolicyArtifacts {
    pub(crate) fn matches(
        &self,
        scope: &RepositoryScope,
        source_release: &ArtifactReference,
        cohort_manifest: &ArtifactReference,
        cutoff_at_millis: u64,
    ) -> bool {
        self.scope == *scope
            && self.source_release == *source_release
            && self.cohort_manifest == *cohort_manifest
            && self.cutoff_at_millis == cutoff_at_millis
    }
}

/// Read-only production authority over the canonical CP database.
pub struct DurablePerformanceEvaluationAuthority {
    storage: SqliteStorage,
    artifacts: ArtifactStore,
}

impl DurablePerformanceEvaluationAuthority {
    /// Opens the canonical CP database used by dispatch, terminal, and model
    /// retry authority.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the database cannot be opened.
    pub fn open(
        data_directory: impl AsRef<Path>,
    ) -> Result<Self, PerformanceEvaluationProjectionError> {
        let data_directory = data_directory.as_ref();
        let objects = LocalArtifactObjectStore::open(data_directory.join("artifacts"))
            .map_err(|_| PerformanceEvaluationProjectionError::Unavailable)?;
        Ok(Self {
            storage: SqliteStorage::open(data_directory)
                .map_err(|_| PerformanceEvaluationProjectionError::Unavailable)?,
            artifacts: ArtifactStore::open(
                data_directory.join("artifact-catalog"),
                Box::new(objects),
            )
            .map_err(|_| PerformanceEvaluationProjectionError::Unavailable)?,
        })
    }

    /// Rebuilds one authorization from durable sources.
    ///
    /// The V1 authorization revision is the append-once pair record revision,
    /// fixed at one. Observer-enabled runs remain ineligible until their exact
    /// Worker-to-Provider attempt links are durable.
    ///
    /// # Errors
    ///
    /// Rejects foreign/incomplete Worker links, missing dispatch or terminal
    /// authority, a different Candidate, non-terminal retries, and any facts
    /// outside the frozen assignment.
    pub fn project_authorization(
        &mut self,
        assignment: EvaluationAssignmentV1,
        worker: &WorkerPerformanceArmAuthorityV1,
    ) -> Result<EvaluationAuthorizationV1, PerformanceEvaluationProjectionError> {
        assignment
            .validate()
            .map_err(|_| PerformanceEvaluationProjectionError::InvalidAuthority)?;
        self.validate_assignment_artifacts(&assignment)?;
        crate::rollout_evaluation::validate_consumed_evaluation_slot(&self.storage, &assignment)
            .map_err(|_| PerformanceEvaluationProjectionError::InvalidAuthority)?;
        if assignment.spec().observer.mode != ObserverMode::Off {
            return Err(PerformanceEvaluationProjectionError::ObserverAuthorityUnavailable);
        }
        validate_worker_authority(worker)?;
        let dispatch = self
            .storage
            .execution_registry()
            .and_then(|registry| registry.load_dispatch_authority(&assignment.spec().job_id))
            .map_err(|error| map_storage(&error))?
            .ok_or(PerformanceEvaluationProjectionError::IncompleteAuthority)?;
        if dispatch.lease().job_id != assignment.spec().job_id {
            return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
        }
        let dispatch_accepted_at_millis =
            instant_millis(dispatch.accepted_at()).map_err(|error| map_storage(&error))?;
        let terminal = load_evaluation_terminal_authority(&self.storage, &assignment.spec().job_id)
            .map_err(|error| map_storage(&error))?
            .ok_or(PerformanceEvaluationProjectionError::IncompleteAuthority)?;
        let candidate_matches = terminal
            .artifacts
            .iter()
            .filter(|artifact| *artifact == &worker.candidate_artifact)
            .count();
        if candidate_matches != 1 {
            return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
        }
        let candidate_artifact = self.validate_candidate_artifact(
            &assignment.spec().repository_scope,
            &worker.candidate_artifact,
            assignment.spec().cutoff_at_millis,
        )?;
        let primary_model_calls = project_primary_model_calls(
            &self.storage,
            &assignment.spec().attempt_policy.primary,
            &assignment.spec().primary_planned_routes,
            worker,
        )?;
        let retry_ledger_cursor = primary_model_calls
            .iter()
            .map(|call| call.retry_state_revision)
            .max()
            .ok_or(PerformanceEvaluationProjectionError::IncompleteAuthority)?;
        let cutoff_at_millis = assignment.spec().cutoff_at_millis;
        EvaluationAuthorizationV1::try_new(
            assignment,
            EvaluationAuthorizationFactsV1 {
                candidate_artifact: worker.candidate_artifact.clone(),
                evidence_cutoff: EvaluationEvidenceCutoffV1 {
                    cutoff_at_millis,
                    control_plane_terminal_cursor: terminal.terminal_revision,
                    retry_ledger_cursor,
                    candidate_ack_cursor: worker.candidate_artifact_ack_revision,
                    artifact_acknowledged_sequence: candidate_artifact.acknowledged_sequence,
                    worker_ledger_snapshot_digest: worker.worker_ledger_snapshot_digest.clone(),
                    artifact_snapshot_digest: candidate_artifact.snapshot_digest,
                },
                candidate_artifact_ack_revision: worker.candidate_artifact_ack_revision,
                dispatch_accepted_at_millis,
                worker_terminal_finished_at_millis: terminal.worker_finished_at_millis,
                terminal_accepted_at_millis: terminal.accepted_at_millis,
                terminal_revision: terminal.terminal_revision,
                authorization_revision: AUTHORIZATION_REVISION_V1,
                primary_model_calls,
                observer_model_calls: Vec::new(),
            },
        )
        .map_err(|_| PerformanceEvaluationProjectionError::InvalidAuthority)
    }

    /// Loads both exact consumed assignments from the canonical pair slot.
    ///
    /// # Errors
    ///
    /// Rejects an unknown, unconsumed, foreign, or conflicting slot.
    pub fn load_consumed_pair(
        &self,
        scope: &RepositoryScope,
        policy_revision: u64,
        pair_id: &Sha256Digest,
    ) -> Result<ConsumedPerformanceEvaluationPair, PerformanceEvaluationProjectionError> {
        let assignments = crate::rollout_evaluation::load_consumed_pair_assignments(
            &self.storage,
            scope,
            policy_revision,
            pair_id,
        )
        .map_err(|error| map_rollout_projection(&error))?;
        Ok(ConsumedPerformanceEvaluationPair {
            react: assignments.react,
            delegated: assignments.delegated,
        })
    }

    /// Validates policy Artifacts and commits the policy through the same
    /// Control Plane data-directory authority.
    ///
    /// # Errors
    ///
    /// Rejects a foreign, incomplete, late, or digest-mismatched Artifact and
    /// any gate revision or request conflict before returning a receipt.
    pub fn put_policy(
        &mut self,
        command: crate::rollout_gate::PutRolloutGatePolicy,
    ) -> Result<crate::rollout_gate::RolloutGateMutationReceipt, PerformanceEvaluationProjectionError>
    {
        let plan = command.policy.plan();
        self.validate_policy_artifacts(
            &command.scope,
            &plan.source_release,
            &plan.cohort_manifest,
            plan.cutoff_at_millis,
        )?;
        crate::rollout_gate::RolloutGateService::new(&mut self.storage)
            .put_policy(command)
            .map_err(|error| map_gate_projection(&error))
    }

    /// Reads and re-hashes the exact source-release and cohort-manifest
    /// Artifacts before a statistical policy is admitted.
    ///
    /// # Errors
    ///
    /// Rejects missing, incomplete, late, foreign, wrongly typed, or
    /// digest-mismatched Artifacts.
    pub fn validate_policy_artifacts(
        &self,
        scope: &RepositoryScope,
        source_release: &ArtifactReference,
        cohort_manifest: &ArtifactReference,
        cutoff_at_millis: u64,
    ) -> Result<ValidatedPerformancePolicyArtifacts, PerformanceEvaluationProjectionError> {
        let scope_key = crate::repository_scope_key(scope).map_err(|error| map_storage(&error))?;
        validate_policy_artifact(
            &self.artifacts,
            &scope_key,
            source_release,
            SOURCE_RELEASE_KIND,
            SOURCE_RELEASE_MEDIA_TYPE,
            cutoff_at_millis,
        )?;
        validate_policy_artifact(
            &self.artifacts,
            &scope_key,
            cohort_manifest,
            COHORT_MANIFEST_KIND,
            COHORT_MANIFEST_MEDIA_TYPE,
            cutoff_at_millis,
        )?;
        Ok(ValidatedPerformancePolicyArtifacts {
            scope: scope.clone(),
            source_release: source_release.clone(),
            cohort_manifest: cohort_manifest.clone(),
            cutoff_at_millis,
        })
    }

    /// Rebuilds and joins both arms of one exact predeclared pair.
    ///
    /// The returned value is opaque outside the Control Plane. It can only be
    /// handed to the rollout evaluation service, which rechecks the active
    /// policy and one-shot assignment consumption before persistence.
    ///
    /// # Errors
    ///
    /// Rejects either incomplete authority chain and any mismatch between the
    /// two assignments, terminal facts, or raw Worker measurements.
    pub fn project_pair(
        &mut self,
        react_assignment: EvaluationAssignmentV1,
        react_worker: &WorkerPerformanceArmAuthorityV1,
        delegated_assignment: EvaluationAssignmentV1,
        delegated_worker: &WorkerPerformanceArmAuthorityV1,
    ) -> Result<
        crate::rollout_evaluation::ProjectedEvaluationPair,
        PerformanceEvaluationProjectionError,
    > {
        let react_authorization = self.project_authorization(react_assignment, react_worker)?;
        let delegated_authorization =
            self.project_authorization(delegated_assignment, delegated_worker)?;
        let pair = PerformancePairedSampleV1::try_new(
            react_authorization,
            react_worker.measurement.clone(),
            delegated_authorization,
            delegated_worker.measurement.clone(),
        )
        .map_err(|_| PerformanceEvaluationProjectionError::InvalidAuthority)?;
        crate::rollout_evaluation::ProjectedEvaluationPair::try_from_authority(pair)
            .map_err(|_| PerformanceEvaluationProjectionError::InvalidAuthority)
    }

    /// Deterministically closes the owned database connection.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when close/checkpoint fails.
    pub fn close(self) -> Result<(), PerformanceEvaluationProjectionError> {
        let storage = Box::new(self.storage)
            .close()
            .map_err(|_| PerformanceEvaluationProjectionError::Unavailable);
        let artifacts = self
            .artifacts
            .close()
            .map_err(|_| PerformanceEvaluationProjectionError::Unavailable);
        storage.and(artifacts)
    }

    fn validate_assignment_artifacts(
        &self,
        assignment: &EvaluationAssignmentV1,
    ) -> Result<(), PerformanceEvaluationProjectionError> {
        let validated = self.validate_policy_artifacts(
            &assignment.spec().repository_scope,
            &assignment.spec().source_release,
            &assignment.spec().cohort_manifest,
            assignment.spec().cutoff_at_millis,
        )?;
        if !validated.matches(
            &assignment.spec().repository_scope,
            &assignment.spec().source_release,
            &assignment.spec().cohort_manifest,
            assignment.spec().cutoff_at_millis,
        ) {
            return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
        }
        Ok(())
    }

    fn validate_candidate_artifact(
        &self,
        scope: &RepositoryScope,
        candidate: &ArtifactReference,
        cutoff_at_millis: u64,
    ) -> Result<ValidatedCandidateArtifact, PerformanceEvaluationProjectionError> {
        let scope_key = crate::repository_scope_key(scope).map_err(|error| map_storage(&error))?;
        validate_candidate_artifact_from_store(
            &self.artifacts,
            &scope_key,
            candidate,
            cutoff_at_millis,
        )
    }
}

#[derive(Clone, Debug)]
struct ValidatedCandidateArtifact {
    acknowledged_sequence: u64,
    snapshot_digest: Sha256Digest,
}

fn validate_candidate_artifact_from_store(
    artifacts: &ArtifactStore,
    scope: &winwincode_storage::ReceiptScopeKey,
    candidate: &ArtifactReference,
    cutoff_at_millis: u64,
) -> Result<ValidatedCandidateArtifact, PerformanceEvaluationProjectionError> {
    let object = artifacts
        .read_reference_exact(scope, &candidate.artifact_id, &candidate.digest)
        .map_err(|error| map_artifact(&error))?;
    if object.metadata().kind() != CANDIDATE_KIND
        || object.metadata().media_type() != CANDIDATE_MEDIA_TYPE
        || object.metadata().created_at_millis() > cutoff_at_millis
        || object.metadata().acknowledged_sequence() == 0
    {
        return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
    }
    Ok(ValidatedCandidateArtifact {
        acknowledged_sequence: object.metadata().acknowledged_sequence(),
        snapshot_digest: candidate_artifact_snapshot_digest(&object),
    })
}

impl fmt::Debug for DurablePerformanceEvaluationAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurablePerformanceEvaluationAuthority")
            .finish_non_exhaustive()
    }
}

/// Stable projection failure categories with no private IDs or paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerformanceEvaluationProjectionError {
    Unavailable,
    IncompleteAuthority,
    InvalidAuthority,
    ObserverAuthorityUnavailable,
    RevisionConflict,
}

impl fmt::Display for PerformanceEvaluationProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "performance evaluation authority is unavailable",
            Self::IncompleteAuthority => "performance evaluation authority is incomplete",
            Self::InvalidAuthority => "performance evaluation authority is invalid",
            Self::ObserverAuthorityUnavailable => {
                "Observer performance authority is not retained end to end"
            }
            Self::RevisionConflict => "performance evaluation policy revision changed",
        })
    }
}

impl std::error::Error for PerformanceEvaluationProjectionError {}

fn validate_worker_authority(
    worker: &WorkerPerformanceArmAuthorityV1,
) -> Result<(), PerformanceEvaluationProjectionError> {
    worker
        .measurement
        .validate()
        .map_err(|_| PerformanceEvaluationProjectionError::InvalidAuthority)?;
    let mut calls = BTreeSet::new();
    let mut requests = BTreeSet::new();
    let mut exchanges = BTreeSet::new();
    if worker.primary_model_calls.is_empty() {
        return Err(PerformanceEvaluationProjectionError::IncompleteAuthority);
    }
    let measurement_calls = worker
        .measurement
        .model_calls()
        .iter()
        .filter(|call| call.model_kind == PerformanceV0ModelKind::Primary)
        .map(|call| call.model_call_id.0.as_str())
        .collect::<BTreeSet<_>>();
    if worker.candidate_artifact.artifact_id.0.is_empty()
        || !canonical_digest(&worker.candidate_artifact.digest)
        || !canonical_digest(&worker.worker_ledger_snapshot_digest)
        || worker.candidate_artifact_ack_revision == 0
        || worker.primary_model_calls.len() > 4_096
        || worker.primary_model_calls.iter().any(|call| {
            !canonical_digest(&call.model_call_digest)
                || !calls.insert(call.model_call_digest.0.as_str())
                || !requests.insert(call.request_id.0.as_str())
                || !exchanges.insert(call.initial_model_exchange_id.0.as_str())
        })
        || measurement_calls != calls
        || worker
            .measurement
            .model_calls()
            .iter()
            .any(|call| call.model_kind == PerformanceV0ModelKind::Observer)
    {
        return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
    }
    Ok(())
}

fn project_primary_model_calls(
    storage: &SqliteStorage,
    expected_plan: &EvaluationRetryPlanV1,
    planned_routes: &[EvaluationRouteV1],
    worker: &WorkerPerformanceArmAuthorityV1,
) -> Result<Vec<EvaluationModelCallAuthorityV1>, PerformanceEvaluationProjectionError> {
    let measurement_calls = worker
        .measurement
        .model_calls()
        .iter()
        .map(|call| (call.model_call_id.0.as_str(), call))
        .collect::<BTreeMap<_, _>>();
    let mut projected = Vec::new();
    let mut arm_totals = ReconciledUsage::default();
    for call in &worker.primary_model_calls {
        let model_call = measurement_calls
            .get(call.model_call_digest.0.as_str())
            .ok_or(PerformanceEvaluationProjectionError::InvalidAuthority)?;
        let request = load_evaluation_route_attempts(storage, &call.request_id)
            .map_err(|error| map_retry_projection(&error))?;
        if request
            .attempts
            .first()
            .map(|attempt| &attempt.model_exchange_id)
            != Some(&call.initial_model_exchange_id)
        {
            return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
        }
        validate_retry_plan_projection(&request, expected_plan, planned_routes)?;
        let mut call_totals = ReconciledUsage::default();
        let attempts = request
            .attempts
            .into_iter()
            .map(|attempt| {
                if let Some(usage) = &attempt.settled_usage {
                    call_totals.add(usage)?;
                }
                let outcome = match (attempt.status, attempt.settled_usage.is_some()) {
                    (EvaluationRouteAttemptStatusProjection::Failed, false) => {
                        EvaluationAttemptOutcomeV1::FailedNoCharge
                    }
                    (EvaluationRouteAttemptStatusProjection::Failed, true) => {
                        EvaluationAttemptOutcomeV1::FailedCharged
                    }
                    (EvaluationRouteAttemptStatusProjection::Succeeded, true) => {
                        EvaluationAttemptOutcomeV1::Succeeded
                    }
                    (EvaluationRouteAttemptStatusProjection::Succeeded, false) => {
                        return Err(PerformanceEvaluationProjectionError::IncompleteAuthority);
                    }
                };
                let settled_usage = attempt.settled_usage.map(|usage| EvaluationSettledUsageV1 {
                    provider_usage_id: usage.provider_usage_id,
                    provider_id: usage.provider_id,
                    model_id: usage.model_id,
                    input_tokens: usage.input_tokens,
                    cached_input_tokens: usage.cached_input_tokens,
                    cache_write_input_tokens: usage.cache_write_input_tokens,
                    output_tokens: usage.output_tokens,
                    reasoning_output_tokens: usage.reasoning_output_tokens,
                    total_tokens: usage.total_tokens,
                    cost_microunits: usage.cost_micros,
                });
                Ok(EvaluationRouteAttemptV1 {
                    ordinal: attempt.ordinal,
                    step_index: attempt.route_index,
                    attempt_on_step: attempt.attempt_on_step,
                    route: EvaluationRouteV1 {
                        provider_id: attempt.provider_id,
                        model_id: attempt.model_id,
                        route_digest: attempt.route_digest,
                    },
                    provider_exchange_digest: provider_exchange_digest(&attempt.model_exchange_id),
                    outcome,
                    settled_usage,
                })
            })
            .collect::<Result<Vec<_>, PerformanceEvaluationProjectionError>>()?;
        call_totals.matches_model_call(model_call)?;
        arm_totals.add_totals(call_totals)?;
        projected.push(EvaluationModelCallAuthorityV1 {
            model_call_digest: call.model_call_digest.clone(),
            retry_state_revision: request.state_revision,
            retry_plan: expected_plan.clone(),
            attempts,
        });
    }
    arm_totals.matches_arm(worker.measurement.summary())?;
    Ok(projected)
}

fn validate_retry_plan_projection(
    request: &EvaluationModelRequestProjection,
    expected: &EvaluationRetryPlanV1,
    planned_routes: &[EvaluationRouteV1],
) -> Result<(), PerformanceEvaluationProjectionError> {
    if request.policy_revision != expected.policy_revision
        || request.plan_digest != expected.plan_fingerprint
        || request.steps.len() != expected.steps.len()
    {
        return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
    }
    for (actual, step) in request.steps.iter().zip(&expected.steps) {
        let route_index = usize::try_from(step.route_index)
            .map_err(|_| PerformanceEvaluationProjectionError::InvalidAuthority)?;
        let expected_route = planned_routes
            .get(route_index)
            .ok_or(PerformanceEvaluationProjectionError::InvalidAuthority)?;
        if actual.provider_id != expected_route.provider_id
            || actual.model_id != expected_route.model_id
            || actual.route_digest != expected_route.route_digest
            || actual.maximum_attempts != u64::from(step.maximum_attempts)
        {
            return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct ReconciledUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    cost_microunits: u64,
    model_call_count: u64,
}

impl ReconciledUsage {
    fn add(
        &mut self,
        usage: &crate::model_retry_usage::SettledModelUsage,
    ) -> Result<(), PerformanceEvaluationProjectionError> {
        self.input_tokens = checked_add(self.input_tokens, usage.input_tokens)?;
        self.cached_input_tokens =
            checked_add(self.cached_input_tokens, usage.cached_input_tokens)?;
        self.output_tokens = checked_add(self.output_tokens, usage.output_tokens)?;
        self.cost_microunits = checked_add(self.cost_microunits, usage.cost_micros)?;
        Ok(())
    }

    fn add_totals(&mut self, totals: Self) -> Result<(), PerformanceEvaluationProjectionError> {
        self.input_tokens = checked_add(self.input_tokens, totals.input_tokens)?;
        self.cached_input_tokens =
            checked_add(self.cached_input_tokens, totals.cached_input_tokens)?;
        self.output_tokens = checked_add(self.output_tokens, totals.output_tokens)?;
        self.cost_microunits = checked_add(self.cost_microunits, totals.cost_microunits)?;
        self.model_call_count = checked_add(self.model_call_count, 1)?;
        Ok(())
    }

    fn matches_model_call(
        self,
        call: &PerformanceV0ModelCallEvidence,
    ) -> Result<(), PerformanceEvaluationProjectionError> {
        let Some(actual_cost) = call.actual_cost_microunits else {
            return Err(PerformanceEvaluationProjectionError::IncompleteAuthority);
        };
        if !call.completed {
            return Err(PerformanceEvaluationProjectionError::IncompleteAuthority);
        }
        if call.model_kind != PerformanceV0ModelKind::Primary
            || metric(call.input_tokens)? != self.input_tokens
            || metric(call.cached_tokens)? != self.cached_input_tokens
            || metric(call.output_tokens)? != self.output_tokens
            || metric(actual_cost)? != self.cost_microunits
        {
            return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
        }
        Ok(())
    }

    fn matches_arm(
        self,
        summary: &winwincode_execution_port::performance_comparison::PerformanceV0ArmSummary,
    ) -> Result<(), PerformanceEvaluationProjectionError> {
        let total_tokens = checked_add(
            checked_add(self.input_tokens, self.cached_input_tokens)?,
            self.output_tokens,
        )?;
        if summary.incomplete_strong_model_call_count != 0
            || summary.incomplete_observer_model_call_count != 0
            || summary.unpriced_completed_call_count != 0
        {
            return Err(PerformanceEvaluationProjectionError::IncompleteAuthority);
        }
        if metric(summary.strong_model_call_count)? != self.model_call_count
            || summary.completed_strong_model_call_count != summary.strong_model_call_count
            || summary.observer_model_call_count != 0
            || summary.completed_observer_model_call_count != 0
            || metric(summary.total_tokens)? != total_tokens
            || metric(summary.settled_cost_microunits)? != self.cost_microunits
        {
            return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
        }
        Ok(())
    }
}

fn checked_add(left: u64, right: u64) -> Result<u64, PerformanceEvaluationProjectionError> {
    left.checked_add(right)
        .ok_or(PerformanceEvaluationProjectionError::InvalidAuthority)
}

fn metric(value: i64) -> Result<u64, PerformanceEvaluationProjectionError> {
    u64::try_from(value).map_err(|_| PerformanceEvaluationProjectionError::InvalidAuthority)
}

fn map_retry_projection(
    error: &crate::model_retry_usage::ModelRetryUsageError,
) -> PerformanceEvaluationProjectionError {
    match error.kind() {
        ModelRetryUsageErrorKind::InvalidState => {
            PerformanceEvaluationProjectionError::IncompleteAuthority
        }
        ModelRetryUsageErrorKind::Storage => PerformanceEvaluationProjectionError::Unavailable,
        ModelRetryUsageErrorKind::InvalidRequest
        | ModelRetryUsageErrorKind::IdentityMismatch
        | ModelRetryUsageErrorKind::AttemptConflict
        | ModelRetryUsageErrorKind::TerminalConflict
        | ModelRetryUsageErrorKind::RequestConflict
        | ModelRetryUsageErrorKind::UsageConflict
        | ModelRetryUsageErrorKind::CorruptState => {
            PerformanceEvaluationProjectionError::InvalidAuthority
        }
    }
}

fn map_rollout_projection(
    error: &crate::rollout_evaluation::RolloutEvaluationError,
) -> PerformanceEvaluationProjectionError {
    match error.kind() {
        crate::rollout_evaluation::RolloutEvaluationErrorKind::Storage => {
            PerformanceEvaluationProjectionError::Unavailable
        }
        crate::rollout_evaluation::RolloutEvaluationErrorKind::Invalid => {
            PerformanceEvaluationProjectionError::IncompleteAuthority
        }
        crate::rollout_evaluation::RolloutEvaluationErrorKind::RevisionConflict
        | crate::rollout_evaluation::RolloutEvaluationErrorKind::Corrupt => {
            PerformanceEvaluationProjectionError::InvalidAuthority
        }
    }
}

fn map_gate_projection(
    error: &crate::rollout_gate::RolloutGateError,
) -> PerformanceEvaluationProjectionError {
    match error.kind() {
        crate::rollout_gate::RolloutGateErrorKind::Storage => {
            PerformanceEvaluationProjectionError::Unavailable
        }
        crate::rollout_gate::RolloutGateErrorKind::RevisionConflict => {
            PerformanceEvaluationProjectionError::RevisionConflict
        }
        crate::rollout_gate::RolloutGateErrorKind::Invalid
        | crate::rollout_gate::RolloutGateErrorKind::Corrupt => {
            PerformanceEvaluationProjectionError::InvalidAuthority
        }
    }
}

fn provider_exchange_digest(model_exchange_id: &ModelExchangeId) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.performance-provider-exchange.v1");
    digest.update(
        u64::try_from(model_exchange_id.0.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(model_exchange_id.0.as_bytes());
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn candidate_artifact_snapshot_digest(object: &ArtifactObject) -> Sha256Digest {
    let metadata = object.metadata();
    let mut digest = Sha256::new();
    digest.update(b"winwincode.performance-candidate-artifact-snapshot.v1");
    for bytes in [
        metadata.artifact_id().0.as_bytes(),
        metadata.digest().0.as_bytes(),
        metadata.kind().as_bytes(),
        metadata.media_type().as_bytes(),
    ] {
        digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(bytes);
    }
    digest.update(metadata.size_bytes().to_be_bytes());
    digest.update(metadata.created_at_millis().to_be_bytes());
    digest.update(metadata.acknowledged_sequence().to_be_bytes());
    digest.update(Sha256::digest(object.bytes()));
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn canonical_digest(digest: &Sha256Digest) -> bool {
    digest.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn map_storage(error: &winwincode_storage::StorageError) -> PerformanceEvaluationProjectionError {
    match error.kind() {
        StorageErrorKind::Adapter | StorageErrorKind::Closed => {
            PerformanceEvaluationProjectionError::Unavailable
        }
        StorageErrorKind::InvalidInput
        | StorageErrorKind::RequestConflict
        | StorageErrorKind::RevisionConflict
        | StorageErrorKind::RequestReplayMissing
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::EventCursorExpired => {
            PerformanceEvaluationProjectionError::InvalidAuthority
        }
    }
}

fn map_artifact(error: &ArtifactError) -> PerformanceEvaluationProjectionError {
    match error.kind() {
        ArtifactErrorKind::Adapter | ArtifactErrorKind::Closed => {
            PerformanceEvaluationProjectionError::Unavailable
        }
        ArtifactErrorKind::NotFound | ArtifactErrorKind::Incomplete => {
            PerformanceEvaluationProjectionError::IncompleteAuthority
        }
        ArtifactErrorKind::InvalidInput
        | ArtifactErrorKind::Conflict
        | ArtifactErrorKind::SequenceGap
        | ArtifactErrorKind::PermissionDenied
        | ArtifactErrorKind::Retained
        | ArtifactErrorKind::DigestMismatch
        | ArtifactErrorKind::Corrupt => PerformanceEvaluationProjectionError::InvalidAuthority,
    }
}

fn validate_policy_artifact(
    artifacts: &ArtifactStore,
    scope: &winwincode_storage::ReceiptScopeKey,
    reference: &ArtifactReference,
    expected_kind: &str,
    expected_media_type: &str,
    cutoff_at_millis: u64,
) -> Result<(), PerformanceEvaluationProjectionError> {
    let object = artifacts
        .read_reference_exact(scope, &reference.artifact_id, &reference.digest)
        .map_err(|error| map_artifact(&error))?;
    if object.metadata().kind() != expected_kind
        || object.metadata().media_type() != expected_media_type
        || object.metadata().created_at_millis() > cutoff_at_millis
    {
        return Err(PerformanceEvaluationProjectionError::InvalidAuthority);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use winwincode_execution_port::performance_comparison::{
        PerformanceV0ArmSummary, PerformanceV0ModelCallEvidence, PerformanceV0ModelKind,
    };

    use super::*;

    #[test]
    fn settled_provider_attempts_reconcile_each_call_and_arm_totals() {
        let mut totals = ReconciledUsage::default();
        for (usage_id, input, cached, output, cost) in
            [("charged-failure", 7, 2, 3, 11), ("success", 5, 1, 4, 13)]
        {
            totals
                .add(&crate::model_retry_usage::SettledModelUsage {
                    provider_usage_id: usage_id.to_owned(),
                    attempt: 1,
                    provider_id: "provider".to_owned(),
                    model_id: "model".to_owned(),
                    input_tokens: input,
                    cached_input_tokens: cached,
                    cache_write_input_tokens: 0,
                    output_tokens: output,
                    reasoning_output_tokens: 0,
                    total_tokens: input + output,
                    cost_micros: cost,
                })
                .expect("sum Provider settlement");
        }
        let model_call = PerformanceV0ModelCallEvidence {
            run_id: digest(1),
            model_call_id: digest(2),
            model_kind: PerformanceV0ModelKind::Primary,
            completed: true,
            input_tokens: 12,
            cached_tokens: 3,
            output_tokens: 7,
            elapsed_millis: 1,
            actual_cost_microunits: Some(24),
        };
        totals
            .matches_model_call(&model_call)
            .expect("reconcile exact logical call");

        let mut arm = ReconciledUsage::default();
        arm.add_totals(totals).expect("sum arm settlement");
        arm.matches_arm(&PerformanceV0ArmSummary {
            sample_count: 1,
            strong_model_call_count: 1,
            observer_model_call_count: 0,
            completed_strong_model_call_count: 1,
            incomplete_strong_model_call_count: 0,
            completed_observer_model_call_count: 0,
            incomplete_observer_model_call_count: 0,
            total_tokens: 22,
            total_strong_model_wait_ms: 1,
            total_observer_model_wait_ms: 0,
            total_runtime_ms: 1,
            settled_cost_microunits: 24,
            unpriced_completed_call_count: 0,
            duplicate_run_write_count: 0,
            duplicate_model_call_write_count: 0,
            duplicate_settled_charge_write_count: 0,
            duplicate_settled_charge_microunits: 0,
        })
        .expect("reconcile exact arm totals");

        let mut drifted = model_call;
        drifted.actual_cost_microunits = Some(23);
        assert_eq!(
            totals.matches_model_call(&drifted),
            Err(PerformanceEvaluationProjectionError::InvalidAuthority)
        );
        drifted.actual_cost_microunits = None;
        assert_eq!(
            totals.matches_model_call(&drifted),
            Err(PerformanceEvaluationProjectionError::IncompleteAuthority)
        );
    }

    fn digest(value: u64) -> Sha256Digest {
        Sha256Digest(format!("sha256:{value:064x}"))
    }
}
