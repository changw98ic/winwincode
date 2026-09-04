// SPDX-License-Identifier: Apache-2.0

//! Repository-scoped rollout authority for newly sealed writer jobs.
//!
//! The module owns one canonical policy/evidence/decision head. Every update
//! advances that head through the existing state receipt, outbox, and audit
//! transaction. Delivery job creation consumes only a sealed decision and
//! later guards that exact head revision while committing the immutable job.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::{AuditAction, AuditEventId, AuditState, AuditSubject};
use winwincode_domain::{ExecutionJobId, RepositoryScope, RequestId, Sha256Digest};
use winwincode_execution_port::{
    generated::{ExecutionJob, ExecutionWorkspaceWriteMode},
    performance_statistics::{
        PerformanceDecisionReasonV1, PerformanceEvaluationOutcomeV1, PerformanceEvaluationReportV1,
        PerformanceStatisticalPlanInputV1, PerformanceStatisticalPolicyV1,
        evaluate_authorized_pairs_v1,
    },
    runtime_trace_outbox::ExecutionMode,
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity,
    StateCommit, StateMutation, StateRevisionGuard, StorageError, StorageErrorKind, StoredState,
};

use crate::{execution_audit_event_with_state, repository_scope_key};

const STATE_SCHEMA: &str = "winwincode.rollout-gate.v1";
const RECEIPT_TOPIC: &str = "rollout.gate.changed";
const JOB_BINDING_TOPIC: &str = "rollout.gate.job-bound";
const ACTOR_KEY: &[u8] = b"winwincode.rollout-gate.actor.v1";
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
pub use winwincode_execution_port::performance_evaluation::PerformanceEvaluationMetricV1 as RolloutGateMetric;
pub use winwincode_execution_port::performance_statistics::PerformanceMetricThresholdV1 as RolloutGateThreshold;

/// Closed policy input retained by the Control Plane.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RolloutGatePolicyInput {
    plan: PerformanceStatisticalPlanInputV1,
}

impl RolloutGatePolicyInput {
    /// Creates a complete, deterministic policy.
    ///
    /// # Errors
    ///
    /// Rejects missing samples, missing/duplicate metrics, or excessive input.
    pub fn try_new(plan: PerformanceStatisticalPlanInputV1) -> Result<Self, RolloutGateError> {
        let policy = Self { plan };
        validate_policy_input(&policy)?;
        Ok(policy)
    }

    #[must_use]
    pub const fn minimum_complete_pair_count(&self) -> u32 {
        self.plan.minimum_complete_pair_count
    }

    #[must_use]
    pub fn thresholds(&self) -> &[RolloutGateThreshold] {
        &self.plan.thresholds
    }

    pub(crate) const fn plan(&self) -> &PerformanceStatisticalPlanInputV1 {
        &self.plan
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RolloutGatePolicyState {
    Active,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RolloutGatePolicy {
    repository_scope: RepositoryScope,
    revision: u64,
    state: RolloutGatePolicyState,
    input: RolloutGatePolicyInput,
    updated_at_millis: u64,
    digest: Sha256Digest,
}

/// Deterministic rollout decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutGateOutcome {
    Go,
    NoGo,
    InsufficientEvidence,
}

/// Bounded reason set for an exact rollout decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutGateReasonCode {
    PolicyMissing,
    PolicyRevoked,
    EvidenceMissing,
    EvidenceForPriorPolicy,
    ExpectedPairsMissing,
    MinimumPairsNotMet,
    IncompleteModelCalls,
    UnpricedModelCalls,
    DuplicateLedgerWrites,
    MetricThresholdExceeded,
    AllChecksPassed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RolloutGateEvidenceSnapshot {
    revision: u64,
    policy_revision: Option<u64>,
    captured_at_millis: u64,
    authorized_pair_refs: Vec<Sha256Digest>,
    report: PerformanceEvaluationReportV1,
    digest: Sha256Digest,
}

/// Current decision projection returned only through the internal Rust seam.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RolloutGateDecision {
    revision: u64,
    policy_revision: Option<u64>,
    evidence_revision: Option<u64>,
    outcome: RolloutGateOutcome,
    reason_codes: Vec<RolloutGateReasonCode>,
    decided_at_millis: u64,
    digest: Sha256Digest,
}

impl RolloutGateDecision {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn outcome(&self) -> RolloutGateOutcome {
        self.outcome
    }

    #[must_use]
    pub fn reason_codes(&self) -> &[RolloutGateReasonCode] {
        &self.reason_codes
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RolloutGateHead {
    schema: String,
    scope: RepositoryScope,
    revision: u64,
    policy: Option<RolloutGatePolicy>,
    evidence: Option<RolloutGateEvidenceSnapshot>,
    decision: RolloutGateDecision,
}

/// Idempotent mutation result backed by the exact durable receipt event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolloutGateMutationReceipt {
    head_revision: u64,
    decision: RolloutGateDecision,
    replayed: bool,
}

/// Exact active policy identity used to predeclare evaluation assignments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolloutGatePolicyReference {
    revision: u64,
    digest: Sha256Digest,
}

impl RolloutGatePolicyReference {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

impl RolloutGateMutationReceipt {
    #[must_use]
    pub const fn head_revision(&self) -> u64 {
        self.head_revision
    }

    #[must_use]
    pub const fn decision(&self) -> &RolloutGateDecision {
        &self.decision
    }

    #[must_use]
    pub const fn replayed(&self) -> bool {
        self.replayed
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RolloutGateReceiptEvent {
    schema: String,
    head: RolloutGateHead,
}

/// Stable rollout authority failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RolloutGateErrorKind {
    Invalid,
    RevisionConflict,
    Storage,
    Corrupt,
}

/// Secret-safe failure from the internal rollout authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolloutGateError {
    kind: RolloutGateErrorKind,
    message: &'static str,
}

impl RolloutGateError {
    const fn invalid(message: &'static str) -> Self {
        Self {
            kind: RolloutGateErrorKind::Invalid,
            message,
        }
    }

    const fn corrupt() -> Self {
        Self {
            kind: RolloutGateErrorKind::Corrupt,
            message: "rollout gate durable state is invalid",
        }
    }

    fn storage(error: &StorageError) -> Self {
        if error.kind() == StorageErrorKind::RevisionConflict {
            Self {
                kind: RolloutGateErrorKind::RevisionConflict,
                message: "rollout gate revision changed",
            }
        } else {
            Self {
                kind: RolloutGateErrorKind::Storage,
                message: "rollout gate storage is unavailable",
            }
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RolloutGateErrorKind {
        self.kind
    }
}

impl fmt::Display for RolloutGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for RolloutGateError {}

/// Atomic policy update command.
#[derive(Clone, Debug)]
pub struct PutRolloutGatePolicy {
    pub scope: RepositoryScope,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub policy: RolloutGatePolicyInput,
    pub occurred_at_millis: u64,
}

/// Atomic evidence and decision update command.
#[derive(Clone, Debug)]
pub struct RecordRolloutGateEvidence {
    pub scope: RepositoryScope,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub authorized_pair_refs: Vec<Sha256Digest>,
    pub occurred_at_millis: u64,
}

/// Atomic policy revocation command.
#[derive(Clone, Debug)]
pub struct RevokeRolloutGatePolicy {
    pub scope: RepositoryScope,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub occurred_at_millis: u64,
}

/// Deep persistence boundary for the single rollout head.
pub struct RolloutGateService<'storage> {
    storage: &'storage mut dyn ProductStateStorage,
}

impl<'storage> RolloutGateService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Test/internal policy commit. Production admission is owned by the
    /// same-database Artifact authority in `performance_evaluation_projection`.
    ///
    /// # Errors
    ///
    /// Rejects stale revisions, invalid policy, request conflicts, and corrupt storage.
    pub(crate) fn put_policy(
        &mut self,
        command: PutRolloutGatePolicy,
    ) -> Result<RolloutGateMutationReceipt, RolloutGateError> {
        validate_command_common(command.expected_revision, command.occurred_at_millis)?;
        validate_policy_input(&command.policy)?;
        let command_digest = digest_json(&PolicyCommandDigest {
            scope: &command.scope,
            expected_revision: command.expected_revision,
            policy: &command.policy,
            occurred_at_millis: command.occurred_at_millis,
        })?;
        let identity = receipt_identity(&command.scope, command.request_id)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&identity, &command_digest)
            .map_err(|error| RolloutGateError::storage(&error))?
        {
            return decode_receipt(&receipt, &command.scope, true);
        }
        let before = load_head(self.storage, &command.scope)?;
        require_revision(before.as_ref(), command.expected_revision)?;
        require_monotonic_time(before.as_ref(), command.occurred_at_millis)?;
        let next_revision = next_revision(command.expected_revision)?;
        let policy = active_policy(
            command.scope.clone(),
            next_revision,
            command.policy,
            command.occurred_at_millis,
        )?;
        let decision = decision(
            next_revision,
            Some(&policy),
            None,
            command.occurred_at_millis,
        )?;
        let head = RolloutGateHead {
            schema: STATE_SCHEMA.to_owned(),
            scope: command.scope.clone(),
            revision: next_revision,
            policy: Some(policy),
            evidence: None,
            decision,
        };
        commit_head(
            self.storage,
            &identity,
            &command_digest,
            before.as_ref(),
            &head,
            "rollout.gate.policy.updated",
        )
    }

    /// Rebuilds a decision from exact durable authorized-pair references.
    ///
    /// # Errors
    ///
    /// Rejects stale revisions, unknown or foreign pair references, and corrupt storage.
    pub fn record_evidence(
        &mut self,
        command: RecordRolloutGateEvidence,
    ) -> Result<RolloutGateMutationReceipt, RolloutGateError> {
        validate_command_common(command.expected_revision, command.occurred_at_millis)?;
        let mut authorized_pair_refs = command.authorized_pair_refs;
        authorized_pair_refs.sort_by(|left, right| left.0.cmp(&right.0));
        let command_digest = digest_json(&EvidenceCommandDigest {
            scope: &command.scope,
            expected_revision: command.expected_revision,
            authorized_pair_refs: &authorized_pair_refs,
            occurred_at_millis: command.occurred_at_millis,
        })?;
        let identity = receipt_identity(&command.scope, command.request_id)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&identity, &command_digest)
            .map_err(|error| RolloutGateError::storage(&error))?
        {
            return decode_receipt(&receipt, &command.scope, true);
        }
        let before = load_head(self.storage, &command.scope)?;
        require_revision(before.as_ref(), command.expected_revision)?;
        require_monotonic_time(before.as_ref(), command.occurred_at_millis)?;
        let next_revision = next_revision(command.expected_revision)?;
        let policy = before
            .as_ref()
            .and_then(|head| head.policy.clone())
            .filter(|policy| policy.state == RolloutGatePolicyState::Active)
            .ok_or_else(|| {
                RolloutGateError::invalid("rollout evidence requires an active policy")
            })?;
        let pairs = crate::rollout_evaluation::load_authorized_pairs(
            self.storage,
            &command.scope,
            &authorized_pair_refs,
        )
        .map_err(|_| RolloutGateError::invalid("rollout evidence references are invalid"))?;
        let report = evaluate_authorized_pairs_v1(&statistical_policy(&policy)?, &pairs)
            .map_err(|_| RolloutGateError::invalid("rollout evidence pairs are invalid"))?;
        let evidence = evidence_snapshot(
            next_revision,
            Some(policy.revision),
            authorized_pair_refs,
            report,
            command.occurred_at_millis,
        )?;
        let gate_decision = decision(
            next_revision,
            Some(&policy),
            Some(&evidence),
            command.occurred_at_millis,
        )?;
        let head = RolloutGateHead {
            schema: STATE_SCHEMA.to_owned(),
            scope: command.scope.clone(),
            revision: next_revision,
            policy: Some(policy),
            evidence: Some(evidence),
            decision: gate_decision,
        };
        commit_head(
            self.storage,
            &identity,
            &command_digest,
            before.as_ref(),
            &head,
            "rollout.gate.evidence.recorded",
        )
    }

    /// Revokes the active policy and persists a No-Go decision.
    ///
    /// # Errors
    ///
    /// Rejects missing policy, stale revision, request conflicts, and corrupt storage.
    pub fn revoke_policy(
        &mut self,
        command: RevokeRolloutGatePolicy,
    ) -> Result<RolloutGateMutationReceipt, RolloutGateError> {
        validate_command_common(command.expected_revision, command.occurred_at_millis)?;
        let command_digest = digest_json(&RevokeCommandDigest {
            scope: &command.scope,
            expected_revision: command.expected_revision,
            occurred_at_millis: command.occurred_at_millis,
        })?;
        let identity = receipt_identity(&command.scope, command.request_id)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&identity, &command_digest)
            .map_err(|error| RolloutGateError::storage(&error))?
        {
            return decode_receipt(&receipt, &command.scope, true);
        }
        let before = load_head(self.storage, &command.scope)?;
        require_revision(before.as_ref(), command.expected_revision)?;
        require_monotonic_time(before.as_ref(), command.occurred_at_millis)?;
        let next_revision = next_revision(command.expected_revision)?;
        let mut policy = before
            .as_ref()
            .and_then(|head| head.policy.clone())
            .ok_or_else(|| RolloutGateError::invalid("rollout policy does not exist"))?;
        policy.revision = next_revision;
        policy.state = RolloutGatePolicyState::Revoked;
        policy.updated_at_millis = command.occurred_at_millis;
        policy.digest = policy_digest(&policy)?;
        let evidence = before.as_ref().and_then(|head| head.evidence.clone());
        let gate_decision = decision(
            next_revision,
            Some(&policy),
            evidence.as_ref(),
            command.occurred_at_millis,
        )?;
        let head = RolloutGateHead {
            schema: STATE_SCHEMA.to_owned(),
            scope: command.scope.clone(),
            revision: next_revision,
            policy: Some(policy),
            evidence,
            decision: gate_decision,
        };
        commit_head(
            self.storage,
            &identity,
            &command_digest,
            before.as_ref(),
            &head,
            "rollout.gate.policy.revoked",
        )
    }

    /// Loads the exact current decision.
    ///
    /// # Errors
    ///
    /// Returns a closed error for malformed durable state.
    pub fn current_decision(
        &self,
        scope: &RepositoryScope,
    ) -> Result<Option<RolloutGateDecision>, RolloutGateError> {
        Ok(load_head(self.storage, scope)?.map(|head| head.decision))
    }

    /// Loads the active policy identity used by a predeclared evaluation.
    ///
    /// # Errors
    ///
    /// Returns a closed error for malformed durable state.
    pub fn current_policy_reference(
        &self,
        scope: &RepositoryScope,
    ) -> Result<Option<RolloutGatePolicyReference>, RolloutGateError> {
        Ok(load_head(self.storage, scope)?.and_then(|head| {
            head.policy
                .filter(|policy| policy.state == RolloutGatePolicyState::Active)
                .map(|policy| RolloutGatePolicyReference {
                    revision: policy.revision,
                    digest: policy.digest,
                })
        }))
    }
}

pub(crate) fn active_statistical_policy(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
) -> Result<
    Option<(
        String,
        u64,
        RolloutGatePolicyReference,
        PerformanceStatisticalPolicyV1,
    )>,
    RolloutGateError,
> {
    let Some(head) = load_head(storage, scope)? else {
        return Ok(None);
    };
    let Some(policy) = head
        .policy
        .filter(|policy| policy.state == RolloutGatePolicyState::Active)
    else {
        return Ok(None);
    };
    let reference = RolloutGatePolicyReference {
        revision: policy.revision,
        digest: policy.digest.clone(),
    };
    Ok(Some((
        scope_stream_id(scope)?,
        head.revision,
        reference,
        statistical_policy(&policy)?,
    )))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PolicyCommandDigest<'facts> {
    scope: &'facts RepositoryScope,
    expected_revision: u64,
    policy: &'facts RolloutGatePolicyInput,
    occurred_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceCommandDigest<'facts> {
    scope: &'facts RepositoryScope,
    expected_revision: u64,
    authorized_pair_refs: &'facts [Sha256Digest],
    occurred_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RevokeCommandDigest<'facts> {
    scope: &'facts RepositoryScope,
    expected_revision: u64,
    occurred_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PolicyDigest<'facts> {
    repository_scope: &'facts RepositoryScope,
    revision: u64,
    state: RolloutGatePolicyState,
    input: &'facts RolloutGatePolicyInput,
    updated_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceDigest<'facts> {
    revision: u64,
    policy_revision: Option<u64>,
    captured_at_millis: u64,
    authorized_pair_refs: &'facts [Sha256Digest],
    report: &'facts PerformanceEvaluationReportV1,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecisionDigest<'facts> {
    revision: u64,
    policy_revision: Option<u64>,
    evidence_revision: Option<u64>,
    outcome: RolloutGateOutcome,
    reason_codes: &'facts [RolloutGateReasonCode],
    decided_at_millis: u64,
}

fn active_policy(
    repository_scope: RepositoryScope,
    revision: u64,
    input: RolloutGatePolicyInput,
    updated_at_millis: u64,
) -> Result<RolloutGatePolicy, RolloutGateError> {
    let mut policy = RolloutGatePolicy {
        repository_scope,
        revision,
        state: RolloutGatePolicyState::Active,
        input,
        updated_at_millis,
        digest: Sha256Digest(String::new()),
    };
    policy.digest = policy_digest(&policy)?;
    statistical_policy(&policy)?;
    Ok(policy)
}

fn evidence_snapshot(
    revision: u64,
    policy_revision: Option<u64>,
    authorized_pair_refs: Vec<Sha256Digest>,
    report: PerformanceEvaluationReportV1,
    captured_at_millis: u64,
) -> Result<RolloutGateEvidenceSnapshot, RolloutGateError> {
    let mut evidence = RolloutGateEvidenceSnapshot {
        revision,
        policy_revision,
        captured_at_millis,
        authorized_pair_refs,
        report,
        digest: Sha256Digest(String::new()),
    };
    evidence.digest = evidence_digest(&evidence)?;
    Ok(evidence)
}

fn decision(
    revision: u64,
    policy: Option<&RolloutGatePolicy>,
    evidence: Option<&RolloutGateEvidenceSnapshot>,
    decided_at_millis: u64,
) -> Result<RolloutGateDecision, RolloutGateError> {
    let (outcome, reason_codes) = evaluate(policy, evidence);
    let mut decision = RolloutGateDecision {
        revision,
        policy_revision: policy.map(|policy| policy.revision),
        evidence_revision: evidence.map(|evidence| evidence.revision),
        outcome,
        reason_codes,
        decided_at_millis,
        digest: Sha256Digest(String::new()),
    };
    decision.digest = decision_digest(&decision)?;
    Ok(decision)
}

fn evaluate(
    policy: Option<&RolloutGatePolicy>,
    evidence: Option<&RolloutGateEvidenceSnapshot>,
) -> (RolloutGateOutcome, Vec<RolloutGateReasonCode>) {
    let Some(policy) = policy else {
        return (
            RolloutGateOutcome::InsufficientEvidence,
            vec![RolloutGateReasonCode::PolicyMissing],
        );
    };
    if policy.state == RolloutGatePolicyState::Revoked {
        return (
            RolloutGateOutcome::NoGo,
            vec![RolloutGateReasonCode::PolicyRevoked],
        );
    }
    let Some(evidence) = evidence else {
        return (
            RolloutGateOutcome::InsufficientEvidence,
            vec![RolloutGateReasonCode::EvidenceMissing],
        );
    };
    if evidence.policy_revision != Some(policy.revision)
        || evidence.report.policy_revision != policy.revision
        || evidence.report.policy_digest != policy.digest
    {
        return (
            RolloutGateOutcome::InsufficientEvidence,
            vec![RolloutGateReasonCode::EvidenceForPriorPolicy],
        );
    }
    let outcome = match evidence.report.outcome {
        PerformanceEvaluationOutcomeV1::Go => RolloutGateOutcome::Go,
        PerformanceEvaluationOutcomeV1::NoGo => RolloutGateOutcome::NoGo,
        PerformanceEvaluationOutcomeV1::InsufficientEvidence => {
            RolloutGateOutcome::InsufficientEvidence
        }
    };
    let reason_codes = evidence
        .report
        .reason_codes
        .iter()
        .copied()
        .map(report_reason)
        .collect();
    (outcome, reason_codes)
}

const fn report_reason(reason: PerformanceDecisionReasonV1) -> RolloutGateReasonCode {
    match reason {
        PerformanceDecisionReasonV1::ExpectedPairsMissing => {
            RolloutGateReasonCode::ExpectedPairsMissing
        }
        PerformanceDecisionReasonV1::MinimumPairsNotMet => {
            RolloutGateReasonCode::MinimumPairsNotMet
        }
        PerformanceDecisionReasonV1::IncompleteModelCalls => {
            RolloutGateReasonCode::IncompleteModelCalls
        }
        PerformanceDecisionReasonV1::UnpricedModelCalls => {
            RolloutGateReasonCode::UnpricedModelCalls
        }
        PerformanceDecisionReasonV1::DuplicateLedgerWrites => {
            RolloutGateReasonCode::DuplicateLedgerWrites
        }
        PerformanceDecisionReasonV1::MetricThresholdExceeded => {
            RolloutGateReasonCode::MetricThresholdExceeded
        }
        PerformanceDecisionReasonV1::AllChecksPassed => RolloutGateReasonCode::AllChecksPassed,
    }
}

fn statistical_policy(
    policy: &RolloutGatePolicy,
) -> Result<PerformanceStatisticalPolicyV1, RolloutGateError> {
    statistical_policy_at(policy, policy.revision, policy.digest.clone())
}

fn statistical_policy_at(
    policy: &RolloutGatePolicy,
    revision: u64,
    digest: Sha256Digest,
) -> Result<PerformanceStatisticalPolicyV1, RolloutGateError> {
    PerformanceStatisticalPolicyV1::seal(
        policy.repository_scope.clone(),
        revision,
        digest,
        policy.input.plan.clone(),
    )
    .map_err(|_| RolloutGateError::invalid("rollout policy is invalid"))
}

fn validate_policy_input(policy: &RolloutGatePolicyInput) -> Result<(), RolloutGateError> {
    policy
        .plan
        .validate()
        .map_err(|_| RolloutGateError::invalid("rollout policy is invalid"))
}

fn validate_command_common(
    expected_revision: u64,
    occurred_at_millis: u64,
) -> Result<(), RolloutGateError> {
    if expected_revision > MAX_SAFE_INTEGER as u64
        || occurred_at_millis == 0
        || occurred_at_millis > MAX_SAFE_INTEGER as u64
    {
        return Err(RolloutGateError::invalid("rollout gate command is invalid"));
    }
    Ok(())
}

fn require_revision(
    head: Option<&RolloutGateHead>,
    expected_revision: u64,
) -> Result<(), RolloutGateError> {
    let actual = head.map_or(0, |head| head.revision);
    if actual == expected_revision {
        Ok(())
    } else {
        Err(RolloutGateError {
            kind: RolloutGateErrorKind::RevisionConflict,
            message: "rollout gate revision changed",
        })
    }
}

fn next_revision(revision: u64) -> Result<u64, RolloutGateError> {
    revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER as u64)
        .ok_or_else(|| RolloutGateError::invalid("rollout gate revision is exhausted"))
}

fn require_monotonic_time(
    head: Option<&RolloutGateHead>,
    occurred_at_millis: u64,
) -> Result<(), RolloutGateError> {
    if head.is_some_and(|head| occurred_at_millis < head.decision.decided_at_millis) {
        Err(RolloutGateError::invalid(
            "rollout gate command time precedes its current decision",
        ))
    } else {
        Ok(())
    }
}

fn policy_digest(policy: &RolloutGatePolicy) -> Result<Sha256Digest, RolloutGateError> {
    digest_json(&PolicyDigest {
        repository_scope: &policy.repository_scope,
        revision: policy.revision,
        state: policy.state,
        input: &policy.input,
        updated_at_millis: policy.updated_at_millis,
    })
}

fn evidence_digest(
    evidence: &RolloutGateEvidenceSnapshot,
) -> Result<Sha256Digest, RolloutGateError> {
    digest_json(&EvidenceDigest {
        revision: evidence.revision,
        policy_revision: evidence.policy_revision,
        captured_at_millis: evidence.captured_at_millis,
        authorized_pair_refs: &evidence.authorized_pair_refs,
        report: &evidence.report,
    })
}

fn decision_digest(decision: &RolloutGateDecision) -> Result<Sha256Digest, RolloutGateError> {
    digest_json(&DecisionDigest {
        revision: decision.revision,
        policy_revision: decision.policy_revision,
        evidence_revision: decision.evidence_revision,
        outcome: decision.outcome,
        reason_codes: &decision.reason_codes,
        decided_at_millis: decision.decided_at_millis,
    })
}

fn digest_json(value: &impl Serialize) -> Result<Sha256Digest, RolloutGateError> {
    let payload = serde_json::to_vec(value).map_err(|_| RolloutGateError::corrupt())?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(payload)
    )))
}

fn scope_stream_id(scope: &RepositoryScope) -> Result<String, RolloutGateError> {
    let key = repository_scope_key(scope).map_err(|error| RolloutGateError::storage(&error))?;
    Ok(format!(
        "rollout-gate:{}",
        hex_digest(key.as_bytes()).trim_start_matches("sha256:")
    ))
}

fn receipt_identity(
    scope: &RepositoryScope,
    request_id: RequestId,
) -> Result<ReceiptIdentity, RolloutGateError> {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(ACTOR_KEY.to_vec())
            .map_err(|error| RolloutGateError::storage(&error))?,
        repository_scope_key(scope).map_err(|error| RolloutGateError::storage(&error))?,
        request_id,
    )
    .map_err(|error| RolloutGateError::storage(&error))
}

fn load_head(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
) -> Result<Option<RolloutGateHead>, RolloutGateError> {
    let stream_id = scope_stream_id(scope)?;
    let head = storage
        .load_state(&stream_id)
        .map_err(|error| RolloutGateError::storage(&error))?
        .map(|stored| decode_head(&stored, scope))
        .transpose()?;
    if let Some(head) = &head {
        validate_evidence_references(storage, head)?;
    }
    Ok(head)
}

fn validate_evidence_references(
    storage: &dyn ProductStateStorage,
    head: &RolloutGateHead,
) -> Result<(), RolloutGateError> {
    let Some(evidence) = &head.evidence else {
        return Ok(());
    };
    let pairs = crate::rollout_evaluation::load_authorized_pairs(
        storage,
        &head.scope,
        &evidence.authorized_pair_refs,
    )
    .map_err(|_| RolloutGateError::corrupt())?;
    let policy = head.policy.as_ref().ok_or_else(RolloutGateError::corrupt)?;
    let report = evaluate_authorized_pairs_v1(
        &statistical_policy_at(
            policy,
            evidence.report.policy_revision,
            evidence.report.policy_digest.clone(),
        )?,
        &pairs,
    )
    .map_err(|_| RolloutGateError::corrupt())?;
    if report != evidence.report {
        return Err(RolloutGateError::corrupt());
    }
    Ok(())
}

fn decode_head(
    stored: &StoredState,
    scope: &RepositoryScope,
) -> Result<RolloutGateHead, RolloutGateError> {
    let head: RolloutGateHead =
        serde_json::from_slice(&stored.payload).map_err(|_| RolloutGateError::corrupt())?;
    if stored.stream_id != scope_stream_id(scope)?
        || stored.revision != head.revision
        || head.schema != STATE_SCHEMA
        || &head.scope != scope
        || head.revision == 0
        || serde_json::to_vec(&head).map_err(|_| RolloutGateError::corrupt())? != stored.payload
    {
        return Err(RolloutGateError::corrupt());
    }
    validate_head(&head)?;
    Ok(head)
}

fn validate_head(head: &RolloutGateHead) -> Result<(), RolloutGateError> {
    if head.decision.revision != head.revision
        || head.decision.digest != decision_digest(&head.decision)?
    {
        return Err(RolloutGateError::corrupt());
    }
    if let Some(policy) = &head.policy {
        validate_policy_input(&policy.input).map_err(|_| RolloutGateError::corrupt())?;
        if policy.revision == 0
            || policy.revision > head.revision
            || policy.repository_scope != head.scope
            || policy.digest != policy_digest(policy)?
            || statistical_policy(policy).is_err()
            || head.decision.policy_revision != Some(policy.revision)
        {
            return Err(RolloutGateError::corrupt());
        }
    } else if head.decision.policy_revision.is_some() {
        return Err(RolloutGateError::corrupt());
    }
    if let Some(evidence) = &head.evidence {
        evidence
            .report
            .validate()
            .map_err(|_| RolloutGateError::corrupt())?;
        if evidence.authorized_pair_refs.len() < 2
            || evidence
                .authorized_pair_refs
                .windows(2)
                .any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(RolloutGateError::corrupt());
        }
        if evidence.revision == 0
            || evidence.revision > head.revision
            || evidence.policy_revision != Some(evidence.report.policy_revision)
            || evidence.digest != evidence_digest(evidence)?
            || head.decision.evidence_revision != Some(evidence.revision)
        {
            return Err(RolloutGateError::corrupt());
        }
    } else if head.decision.evidence_revision.is_some() {
        return Err(RolloutGateError::corrupt());
    }
    let expected = evaluate(head.policy.as_ref(), head.evidence.as_ref());
    if head.decision.outcome != expected.0 || head.decision.reason_codes != expected.1 {
        return Err(RolloutGateError::corrupt());
    }
    let current_fact_time = head
        .policy
        .as_ref()
        .filter(|policy| policy.revision == head.revision)
        .map(|policy| policy.updated_at_millis)
        .or_else(|| {
            head.evidence
                .as_ref()
                .filter(|evidence| evidence.revision == head.revision)
                .map(|evidence| evidence.captured_at_millis)
        });
    if current_fact_time != Some(head.decision.decided_at_millis) {
        return Err(RolloutGateError::corrupt());
    }
    Ok(())
}

fn commit_head(
    storage: &mut dyn ProductStateStorage,
    identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
    before: Option<&RolloutGateHead>,
    head: &RolloutGateHead,
    action_name: &'static str,
) -> Result<RolloutGateMutationReceipt, RolloutGateError> {
    let state = serde_json::to_vec(head).map_err(|_| RolloutGateError::corrupt())?;
    let event = RolloutGateReceiptEvent {
        schema: STATE_SCHEMA.to_owned(),
        head: head.clone(),
    };
    let stream_id = scope_stream_id(&head.scope)?;
    let event_id = format!("{stream_id}:{}", head.revision);
    let before_digest = before.map(digest_json).transpose()?;
    let after_digest = digest_json(head)?;
    let audit_state = AuditState::changed(before_digest, after_digest)
        .map_err(|_| RolloutGateError::corrupt())?;
    let pending_audit = execution_audit_event_with_state(
        AuditEventId::from_digest(command_digest).map_err(|_| RolloutGateError::corrupt())?,
        head.decision.decided_at_millis,
        identity.request_id().clone(),
        &head.scope,
        AuditAction::policy(action_name).map_err(|_| RolloutGateError::corrupt())?,
        audit_state,
        AuditSubject::new(),
        outcome_name(head.decision.outcome),
    )
    .map_err(|error| RolloutGateError::storage(&error))?;
    let commit = StateCommit::new(
        identity.clone(),
        command_digest.clone(),
        stream_id,
        head.revision - 1,
        state,
        vec![NewOutboxEvent::internal(
            event_id,
            RECEIPT_TOPIC,
            serde_json::to_vec(&event).map_err(|_| RolloutGateError::corrupt())?,
        )],
    )
    .with_pending_audit_event(pending_audit);
    let receipt = storage
        .commit(&commit)
        .map_err(|error| RolloutGateError::storage(&error))?;
    decode_receipt(&receipt, &head.scope, receipt.idempotent_replay)
}

fn decode_receipt(
    receipt: &CommitReceipt,
    scope: &RepositoryScope,
    replayed: bool,
) -> Result<RolloutGateMutationReceipt, RolloutGateError> {
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == RECEIPT_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(RolloutGateError::corrupt());
    };
    let durable_payload = &event.payload;
    let event: RolloutGateReceiptEvent =
        serde_json::from_slice(durable_payload).map_err(|_| RolloutGateError::corrupt())?;
    if event.schema != STATE_SCHEMA
        || &event.head.scope != scope
        || event.head.revision != receipt.revision
        || serde_json::to_vec(&event).map_err(|_| RolloutGateError::corrupt())? != *durable_payload
    {
        return Err(RolloutGateError::corrupt());
    }
    validate_head(&event.head)?;
    Ok(RolloutGateMutationReceipt {
        head_revision: event.head.revision,
        decision: event.head.decision,
        replayed,
    })
}

const fn outcome_name(outcome: RolloutGateOutcome) -> &'static str {
    match outcome {
        RolloutGateOutcome::Go => "go",
        RolloutGateOutcome::NoGo => "no_go",
        RolloutGateOutcome::InsufficientEvidence => "insufficient_evidence",
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RolloutGateJobSeal {
    gate_stream_id: String,
    gate_revision: u64,
    decision_revision: Option<u64>,
    decision_digest: Option<Sha256Digest>,
    decision_outcome: Option<RolloutGateOutcome>,
    capability_enabled: bool,
    write_mode: ExecutionWorkspaceWriteMode,
    evaluation: Option<crate::rollout_evaluation::EvaluationJobSeal>,
}

impl RolloutGateJobSeal {
    pub(crate) fn write_mode(&self) -> ExecutionWorkspaceWriteMode {
        self.write_mode.clone()
    }
}

pub(crate) struct WriterJobSealInput<'facts> {
    pub configured_mode: ExecutionMode,
    pub role: &'facts str,
    pub evaluation_assignment: Option<&'facts Sha256Digest>,
    pub job_id: &'facts ExecutionJobId,
    pub base_revision: &'facts str,
    pub now_millis: u64,
}

pub(crate) fn seal_writer_job(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    input: &WriterJobSealInput<'_>,
) -> Result<Option<RolloutGateJobSeal>, RolloutGateError> {
    if !matches!(input.role, "executor" | "remediator") {
        return Ok(None);
    }
    let head = load_head(storage, scope)?;
    let capability_enabled = input.configured_mode == ExecutionMode::DelegatedPatch;
    let evaluation = crate::rollout_evaluation::seal_evaluation_job(
        storage,
        scope,
        input.configured_mode,
        input.evaluation_assignment,
        input.job_id,
        input.base_revision,
        input.now_millis,
    )
    .map_err(|_| RolloutGateError::invalid("evaluation assignment is unavailable"))?;
    let production_go = capability_enabled
        && head
            .as_ref()
            .is_some_and(|head| head.decision.outcome == RolloutGateOutcome::Go);
    let evaluation_write_mode = evaluation
        .as_ref()
        .map(crate::rollout_evaluation::EvaluationJobSeal::write_mode);
    let delegated =
        production_go || evaluation_write_mode == Some(ExecutionWorkspaceWriteMode::ReadOnly);
    Ok(Some(RolloutGateJobSeal {
        gate_stream_id: scope_stream_id(scope)?,
        gate_revision: head.as_ref().map_or(0, |head| head.revision),
        decision_revision: head.as_ref().map(|head| head.decision.revision),
        decision_digest: head.as_ref().map(|head| head.decision.digest.clone()),
        decision_outcome: head.as_ref().map(|head| head.decision.outcome),
        capability_enabled,
        write_mode: if delegated {
            ExecutionWorkspaceWriteMode::ReadOnly
        } else {
            ExecutionWorkspaceWriteMode::Candidate
        },
        evaluation,
    }))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RolloutGateJobBinding {
    schema: String,
    job_id: ExecutionJobId,
    payload_digest: Sha256Digest,
    execution_profile: String,
    write_mode: ExecutionWorkspaceWriteMode,
    gate_revision: u64,
    decision_revision: Option<u64>,
    decision_digest: Option<Sha256Digest>,
    decision_outcome: Option<RolloutGateOutcome>,
    capability_enabled: bool,
    evaluation_assignment_digest: Option<Sha256Digest>,
    evaluation_write_mode: Option<ExecutionWorkspaceWriteMode>,
}

pub(crate) fn bind_writer_job(
    mut commit: StateCommit,
    job: &ExecutionJob,
    seal: Option<&RolloutGateJobSeal>,
) -> Result<StateCommit, RolloutGateError> {
    let Some(seal) = seal else {
        return Ok(commit);
    };
    if !matches!(job.execution_profile.as_str(), "executor" | "remediator")
        || job.workspace.write_mode != seal.write_mode
    {
        return Err(RolloutGateError::corrupt());
    }
    let binding = job_binding(job, seal);
    let payload = serde_json::to_vec(&binding).map_err(|_| RolloutGateError::corrupt())?;
    let event_id = job_binding_event_id(&job.job_id);
    commit.events.push(NewOutboxEvent::internal(
        event_id.clone(),
        JOB_BINDING_TOPIC,
        payload.clone(),
    ));
    commit = crate::rollout_evaluation::bind_evaluation_job(commit, job, seal.evaluation.as_ref())
        .map_err(|_| RolloutGateError::corrupt())?;
    Ok(commit
        .with_state_guard(
            StateRevisionGuard::new(seal.gate_stream_id.clone(), seal.gate_revision)
                .map_err(|error| RolloutGateError::storage(&error))?,
        )
        .with_state_mutation(
            StateMutation::new(event_id, 0, payload)
                .map_err(|error| RolloutGateError::storage(&error))?,
        ))
}

pub(crate) fn validate_writer_job_binding(
    receipt: &CommitReceipt,
    job: &ExecutionJob,
    seal: Option<&RolloutGateJobSeal>,
) -> Result<(), RolloutGateError> {
    let events = receipt
        .events
        .iter()
        .filter(|event| event.topic == JOB_BINDING_TOPIC)
        .collect::<Vec<_>>();
    if events.is_empty() && seal.is_none() {
        return Ok(());
    }
    let [event] = events.as_slice() else {
        return Err(RolloutGateError::corrupt());
    };
    let binding: RolloutGateJobBinding =
        serde_json::from_slice(&event.payload).map_err(|_| RolloutGateError::corrupt())?;
    if event.event_id != job_binding_event_id(&job.job_id)
        || !binding_matches_job(&binding, job)
        || !binding_has_consistent_gate_facts(&binding)
        || serde_json::to_vec(&binding).map_err(|_| RolloutGateError::corrupt())? != event.payload
    {
        return Err(RolloutGateError::corrupt());
    }
    if receipt.idempotent_replay {
        return Ok(());
    }
    let seal = seal.ok_or_else(RolloutGateError::corrupt)?;
    if binding != job_binding(job, seal) {
        return Err(RolloutGateError::corrupt());
    }
    Ok(())
}

fn job_binding(job: &ExecutionJob, seal: &RolloutGateJobSeal) -> RolloutGateJobBinding {
    RolloutGateJobBinding {
        schema: STATE_SCHEMA.to_owned(),
        job_id: job.job_id.clone(),
        payload_digest: job.payload_digest.clone(),
        execution_profile: job.execution_profile.clone(),
        write_mode: job.workspace.write_mode.clone(),
        gate_revision: seal.gate_revision,
        decision_revision: seal.decision_revision,
        decision_digest: seal.decision_digest.clone(),
        decision_outcome: seal.decision_outcome,
        capability_enabled: seal.capability_enabled,
        evaluation_assignment_digest: seal
            .evaluation
            .as_ref()
            .map(|evaluation| evaluation.assignment().digest().clone()),
        evaluation_write_mode: seal
            .evaluation
            .as_ref()
            .map(crate::rollout_evaluation::EvaluationJobSeal::write_mode),
    }
}

fn binding_matches_job(binding: &RolloutGateJobBinding, job: &ExecutionJob) -> bool {
    binding.schema == STATE_SCHEMA
        && binding.job_id == job.job_id
        && binding.payload_digest == job.payload_digest
        && binding.execution_profile == job.execution_profile
        && binding.write_mode == job.workspace.write_mode
}

fn binding_has_consistent_gate_facts(binding: &RolloutGateJobBinding) -> bool {
    let decision_present = binding.decision_revision.is_some()
        && binding.decision_digest.is_some()
        && binding.decision_outcome.is_some();
    let decision_absent = binding.decision_revision.is_none()
        && binding.decision_digest.is_none()
        && binding.decision_outcome.is_none();
    let decision_shape_valid = if binding.gate_revision == 0 {
        decision_absent
    } else {
        decision_present && binding.decision_revision == Some(binding.gate_revision)
    };
    let delegated = (binding.capability_enabled
        && binding.decision_outcome == Some(RolloutGateOutcome::Go))
        || binding.evaluation_write_mode == Some(ExecutionWorkspaceWriteMode::ReadOnly);
    let evaluation_shape_valid =
        binding.evaluation_assignment_digest.is_some() == binding.evaluation_write_mode.is_some();
    decision_shape_valid
        && evaluation_shape_valid
        && binding.write_mode
            == if delegated {
                ExecutionWorkspaceWriteMode::ReadOnly
            } else {
                ExecutionWorkspaceWriteMode::Candidate
            }
}

fn job_binding_event_id(job_id: &ExecutionJobId) -> String {
    format!("rollout-gate-job-binding:{}", job_id.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use winwincode_domain::{
        ArtifactId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
        RepositoryScopeKind, WorkspaceId,
    };
    use winwincode_execution_port::{
        generated::{
            ArtifactReference, ExecutionLimits, ExecutionScope, ExecutionWorkspace,
            ProductSessionExecutionScope, ProductSessionExecutionScopeKind,
        },
        performance_comparison::{
            PerformanceV0ModelCallEvidence, PerformanceV0ModelKind, PerformanceV0RunEvidence,
        },
        performance_evaluation::{
            EvaluationArmV1, EvaluationAssignmentSpecV1, EvaluationAssignmentV1,
            EvaluationAttemptOutcomeV1, EvaluationAttemptPolicyV1, EvaluationAuthorizationFactsV1,
            EvaluationAuthorizationV1, EvaluationEvidenceCutoffV1,
            EvaluationModelCallAuthorityV1, EvaluationObserverV1, EvaluationRetryPlanV1,
            EvaluationRetryStepV1, EvaluationRouteAttemptV1, EvaluationRouteV1,
            EvaluationSettledUsageV1, PerformanceArmMeasurementV1, PerformancePairedSampleV1,
        },
        performance_statistics::{
            ExpectedPerformancePairV1, PerformanceEstimatorV1, PerformanceMetricThresholdV1,
            PerformanceStatisticalPlanInputV1,
        },
        runtime_trace_outbox::ObserverMode,
    };
    use winwincode_storage::{
        NewOutboxEvent, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
        StateCommit,
    };

    use crate::rollout_evaluation::{
        CreateEvaluationAssignment, ProjectedEvaluationPair, RecordProjectedEvaluationPair,
        RolloutEvaluationErrorKind, RolloutEvaluationService,
    };

    const CUTOFF_MILLIS: u64 = 1_000_000;

    #[test]
    fn evaluation_allowlist_admits_only_the_exact_predeclared_arm() {
        let root = unique_root("evaluation-allowlist");
        let scope = scope(1);
        let mut storage = SqliteStorage::open(&root).expect("open rollout storage");
        put_gate_policy(&mut storage, &scope, 1, 0, 1);
        let policy = policy_reference(&mut storage, &scope);

        let delegated = assignment(&scope, &policy, 1, EvaluationArmV1::Delegated);
        let storage = assert_unique_assignment_slot(storage, &root, &scope, &delegated);
        let ordinary = seal_writer_job(
            &storage,
            &scope,
            &WriterJobSealInput {
                configured_mode: ExecutionMode::DelegatedPatch,
                role: "executor",
                evaluation_assignment: None,
                job_id: &ExecutionJobId(canonical_id("job", 999)),
                base_revision: "base-1",
                now_millis: 2,
            },
        )
        .expect("seal ordinary writer")
        .expect("writer seal");
        assert_eq!(
            ordinary.write_mode(),
            ExecutionWorkspaceWriteMode::Candidate
        );

        let delegated_seal = seal_writer_job(
            &storage,
            &scope,
            &WriterJobSealInput {
                configured_mode: ExecutionMode::DelegatedPatch,
                role: "executor",
                evaluation_assignment: Some(delegated.digest()),
                job_id: &delegated.spec().job_id,
                base_revision: &delegated.spec().base_revision,
                now_millis: 3,
            },
        )
        .expect("seal delegated evaluation writer")
        .expect("writer seal");
        assert_eq!(
            delegated_seal.write_mode(),
            ExecutionWorkspaceWriteMode::ReadOnly
        );
        let mut storage =
            commit_and_replay_assigned_job(storage, &root, &scope, &delegated, &delegated_seal);

        let react = assignment(&scope, &policy, 2, EvaluationArmV1::React);
        create_assignment(&mut storage, &scope, 4, &react, 1);
        let react_seal = seal_writer_job(
            &storage,
            &scope,
            &WriterJobSealInput {
                configured_mode: ExecutionMode::React,
                role: "executor",
                evaluation_assignment: Some(react.digest()),
                job_id: &react.spec().job_id,
                base_revision: &react.spec().base_revision,
                now_millis: 4,
            },
        )
        .expect("seal React evaluation writer")
        .expect("writer seal");
        assert_eq!(
            react_seal.write_mode(),
            ExecutionWorkspaceWriteMode::Candidate
        );

        assert!(
            seal_writer_job(
                &storage,
                &scope,
                &WriterJobSealInput {
                    configured_mode: ExecutionMode::React,
                    role: "executor",
                    evaluation_assignment: Some(delegated.digest()),
                    job_id: &delegated.spec().job_id,
                    base_revision: &delegated.spec().base_revision,
                    now_millis: 5,
                },
            )
            .is_err()
        );
        drop(storage);
        std::fs::remove_dir_all(root).expect("remove rollout fixture");
    }

    #[test]
    fn policy_assignment_and_raw_reference_boundaries_fail_closed() {
        let mut plan = statistical_plan();
        plan.minimum_complete_pair_count = 1;
        assert!(RolloutGatePolicyInput::try_new(plan).is_err());

        let root = unique_root("fail-closed-table");
        let scope = scope(2);
        let mut storage = SqliteStorage::open(&root).expect("open rollout storage");
        put_gate_policy(&mut storage, &scope, 10, 0, 10);
        let policy = policy_reference(&mut storage, &scope);
        let valid = assignment(&scope, &policy, 1, EvaluationArmV1::Delegated);
        let mut invalid_specs = Vec::new();
        let mut wrong_pair = valid.spec().clone();
        wrong_pair.pair_id = digest(9_001);
        invalid_specs.push(wrong_pair);
        let mut wrong_release = valid.spec().clone();
        wrong_release.source_release = artifact(9_002);
        invalid_specs.push(wrong_release);
        let mut wrong_route = valid.spec().clone();
        wrong_route.primary_planned_routes = vec![EvaluationRouteV1 {
            provider_id: "other-provider".to_owned(),
            model_id: "other-model".to_owned(),
            route_digest: digest(9_003),
        }];
        invalid_specs.push(wrong_route);
        let mut wrong_base = valid.spec().clone();
        wrong_base.base_revision = "other-base".to_owned();
        invalid_specs.push(wrong_base);
        for (offset, spec) in invalid_specs.into_iter().enumerate() {
            let invalid = EvaluationAssignmentV1::try_new(spec).expect("well-formed outsider");
            let error = RolloutEvaluationService::new(&mut storage)
                .create_assignment(CreateEvaluationAssignment {
                    scope: scope.clone(),
                    request_id: request_id(20 + offset as u64),
                    expected_gate_revision: 1,
                    assignment: invalid,
                    occurred_at_millis: 20 + offset as u64,
                })
                .expect_err("reject assignment outside frozen plan");
            assert_eq!(error.kind(), RolloutEvaluationErrorKind::Invalid);
        }

        for (offset, references) in [
            Vec::new(),
            vec![digest(9_100)],
            vec![digest(9_101), digest(9_101)],
            vec![digest(9_102), digest(9_103)],
        ]
        .into_iter()
        .enumerate()
        {
            let error = RolloutGateService::new(&mut storage)
                .record_evidence(RecordRolloutGateEvidence {
                    scope: scope.clone(),
                    request_id: request_id(30 + offset as u64),
                    expected_revision: 1,
                    authorized_pair_refs: references,
                    occurred_at_millis: 30 + offset as u64,
                })
                .expect_err("reject caller-authored or unknown evidence");
            assert_eq!(error.kind(), RolloutGateErrorKind::Invalid);
        }
        assert_eq!(
            RolloutGateService::new(&mut storage)
                .current_decision(&scope)
                .expect("load closed gate")
                .expect("gate decision")
                .outcome(),
            RolloutGateOutcome::InsufficientEvidence
        );
        let revoked = RolloutGateService::new(&mut storage)
            .revoke_policy(RevokeRolloutGatePolicy {
                scope: scope.clone(),
                request_id: request_id(40),
                expected_revision: 1,
                occurred_at_millis: 40,
            })
            .expect("revoke evaluation policy");
        assert_eq!(revoked.decision().outcome(), RolloutGateOutcome::NoGo);
        drop(storage);
        std::fs::remove_dir_all(root).expect("remove rollout fixture");
    }

    #[test]
    fn authorized_raw_pairs_and_sealed_job_replay_exactly_after_restart() {
        let root = unique_root("authorized-restart");
        let scope = scope(3);
        let mut storage = SqliteStorage::open(&root).expect("open rollout storage");
        put_gate_policy(&mut storage, &scope, 50, 0, 50);
        let policy = policy_reference(&mut storage, &scope);
        let pair_refs = record_two_authorized_pairs(&mut storage, &scope, &policy);
        let evidence_command = RecordRolloutGateEvidence {
            scope: scope.clone(),
            request_id: request_id(300),
            expected_revision: 1,
            authorized_pair_refs: pair_refs,
            occurred_at_millis: 100,
        };
        let go = RolloutGateService::new(&mut storage)
            .record_evidence(evidence_command.clone())
            .expect("evaluate frozen raw pairs");
        assert_eq!(go.decision().outcome(), RolloutGateOutcome::Go);

        let production_job_id = ExecutionJobId(canonical_id("job", 500));
        let production_seal = seal_writer_job(
            &storage,
            &scope,
            &WriterJobSealInput {
                configured_mode: ExecutionMode::DelegatedPatch,
                role: "executor",
                evaluation_assignment: None,
                job_id: &production_job_id,
                base_revision: "release-base",
                now_millis: 101,
            },
        )
        .expect("seal production writer")
        .expect("writer seal");
        assert_eq!(
            production_seal.write_mode(),
            ExecutionWorkspaceWriteMode::ReadOnly
        );
        let production_job = writer_job(
            &scope,
            production_job_id,
            "release-base",
            production_seal.write_mode(),
            500,
        );
        let commit = bound_job_commit(&production_job, &production_seal, 500);
        let first = storage
            .commit(&commit)
            .expect("commit sealed production Job");
        assert!(!first.idempotent_replay);
        drop(storage);

        assert_restart_replay(
            &root,
            &scope,
            evidence_command,
            &go,
            &commit,
            &first,
            &production_job,
        );
        std::fs::remove_dir_all(root).expect("remove rollout fixture");
    }

    fn assert_restart_replay(
        root: &PathBuf,
        scope: &RepositoryScope,
        evidence_command: RecordRolloutGateEvidence,
        go: &RolloutGateMutationReceipt,
        commit: &StateCommit,
        first: &CommitReceipt,
        production_job: &ExecutionJob,
    ) {
        let mut reopened = SqliteStorage::open(root).expect("reopen rollout storage");
        let evidence_replay = RolloutGateService::new(&mut reopened)
            .record_evidence(evidence_command)
            .expect("replay exact evidence command");
        assert!(evidence_replay.replayed());
        assert_eq!(evidence_replay.decision(), go.decision());
        RolloutGateService::new(&mut reopened)
            .revoke_policy(RevokeRolloutGatePolicy {
                scope: scope.clone(),
                request_id: request_id(301),
                expected_revision: 2,
                occurred_at_millis: 102,
            })
            .expect("revoke rollout after sealing Job");
        let current = seal_writer_job(
            &reopened,
            scope,
            &WriterJobSealInput {
                configured_mode: ExecutionMode::DelegatedPatch,
                role: "executor",
                evaluation_assignment: None,
                job_id: &production_job.job_id,
                base_revision: &production_job.workspace.checkout_revision,
                now_millis: 103,
            },
        )
        .expect("seal current writer")
        .expect("writer seal");
        assert_eq!(current.write_mode(), ExecutionWorkspaceWriteMode::Candidate);
        let replay = reopened.commit(commit).expect("replay exact sealed Job");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.events, first.events);
        validate_writer_job_binding(&replay, production_job, Some(&current))
            .expect("replay retains original sealed authority");
        let binding = reopened
            .load_state(&job_binding_event_id(&production_job.job_id))
            .expect("load writer binding")
            .expect("writer binding state");
        let binding: RolloutGateJobBinding =
            serde_json::from_slice(&binding.payload).expect("decode writer binding");
        assert_eq!(binding.write_mode, ExecutionWorkspaceWriteMode::ReadOnly);
        assert_eq!(binding.decision_outcome, Some(RolloutGateOutcome::Go));
    }

    fn put_gate_policy(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
        request: u64,
        expected_revision: u64,
        occurred_at_millis: u64,
    ) -> RolloutGateMutationReceipt {
        RolloutGateService::new(storage)
            .put_policy(PutRolloutGatePolicy {
                scope: scope.clone(),
                request_id: request_id(request),
                expected_revision,
                policy: RolloutGatePolicyInput::try_new(statistical_plan())
                    .expect("valid rollout plan"),
                occurred_at_millis,
            })
            .expect("put rollout gate policy")
    }

    fn policy_reference(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
    ) -> RolloutGatePolicyReference {
        RolloutGateService::new(storage)
            .current_policy_reference(scope)
            .expect("load policy reference")
            .expect("active policy")
    }

    fn statistical_plan() -> PerformanceStatisticalPlanInputV1 {
        PerformanceStatisticalPlanInputV1 {
            source_release: artifact(1),
            cohort_manifest: artifact(2),
            cohort_id: digest(3),
            cutoff_at_millis: CUTOFF_MILLIS,
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
            bootstrap_resamples: 100,
            confidence_basis_points: 9_500,
            thresholds: [
                RolloutGateMetric::StrongModelCalls,
                RolloutGateMetric::TotalTokens,
                RolloutGateMetric::ModelWaitMillis,
                RolloutGateMetric::WallClockRuntimeMillis,
                RolloutGateMetric::SettledCostMicrounits,
            ]
            .into_iter()
            .map(|metric| {
                PerformanceMetricThresholdV1::try_new(metric, 0).expect("valid threshold")
            })
            .collect(),
        }
    }

    fn assert_unique_assignment_slot(
        mut storage: SqliteStorage,
        root: &PathBuf,
        scope: &RepositoryScope,
        assignment: &EvaluationAssignmentV1,
    ) -> SqliteStorage {
        let create = CreateEvaluationAssignment {
            scope: scope.clone(),
            request_id: request_id(2),
            expected_gate_revision: 1,
            assignment: assignment.clone(),
            occurred_at_millis: 1,
        };
        let first = RolloutEvaluationService::new(&mut storage)
            .create_assignment(create.clone())
            .expect("create delegated slot");
        assert!(!first.replayed());
        drop(storage);
        let mut storage = SqliteStorage::open(root).expect("reopen rollout storage");
        let replay = RolloutEvaluationService::new(&mut storage)
            .create_assignment(create)
            .expect("replay delegated slot after restart");
        assert!(replay.replayed());
        assert_equivalent_slot_replay(&mut storage, scope, assignment);
        assert_competing_slot_rejected(&mut storage, scope, assignment);
        storage
    }

    fn assert_equivalent_slot_replay(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
        assignment: &EvaluationAssignmentV1,
    ) {
        let replay = RolloutEvaluationService::new(storage)
            .create_assignment(CreateEvaluationAssignment {
                scope: scope.clone(),
                request_id: request_id(20),
                expected_gate_revision: 1,
                assignment: assignment.clone(),
                occurred_at_millis: 1,
            })
            .expect("an equivalent command resolves to the existing slot");
        assert!(replay.replayed());
    }

    fn assert_competing_slot_rejected(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
        assignment: &EvaluationAssignmentV1,
    ) {
        let mut spec = assignment.spec().clone();
        spec.job_id = ExecutionJobId(canonical_id("job", 9_001));
        spec.run_id = digest(9_002);
        let competing = EvaluationAssignmentV1::try_new(spec).expect("build competing sample");
        let error = RolloutEvaluationService::new(storage)
            .create_assignment(CreateEvaluationAssignment {
                scope: scope.clone(),
                request_id: request_id(3),
                expected_gate_revision: 1,
                assignment: competing.clone(),
                occurred_at_millis: 2,
            })
            .expect_err("one policy/pair/arm slot admits only one Job and run");
        assert_eq!(error.kind(), RolloutEvaluationErrorKind::RevisionConflict);
        let stream = format!(
            "rollout-evaluation-assignment:{}",
            competing.digest().0.trim_start_matches("sha256:")
        );
        assert!(
            storage
                .load_state(&stream)
                .expect("load assignment")
                .is_none()
        );
        assert!(
            storage
                .load_state(&job_binding_event_id(&competing.spec().job_id))
                .expect("load competing Job binding")
                .is_none()
        );
    }

    fn commit_and_replay_assigned_job(
        mut storage: SqliteStorage,
        root: &PathBuf,
        scope: &RepositoryScope,
        assignment: &EvaluationAssignmentV1,
        seal: &RolloutGateJobSeal,
    ) -> SqliteStorage {
        let job = writer_job(
            scope,
            assignment.spec().job_id.clone(),
            &assignment.spec().base_revision,
            seal.write_mode(),
            3,
        );
        let commit = bound_job_commit(&job, seal, 3);
        let first = storage
            .commit(&commit)
            .expect("atomically consume delegated slot with Job");
        assert!(!first.idempotent_replay);
        drop(storage);
        let mut storage = SqliteStorage::open(root).expect("reopen consumed slot");
        let replay = storage
            .commit(&commit)
            .expect("replay exact Job and slot consumption");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.events, first.events);
        storage
    }

    fn record_two_authorized_pairs(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
        policy: &RolloutGatePolicyReference,
    ) -> Vec<Sha256Digest> {
        [1_u64, 2]
            .into_iter()
            .map(|index| record_authorized_pair_fixture(storage, scope, policy, index))
            .collect()
    }

    fn record_authorized_pair_fixture(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
        policy: &RolloutGatePolicyReference,
        index: u64,
    ) -> Sha256Digest {
        let react = assignment(scope, policy, index, EvaluationArmV1::React);
        let delegated = assignment(scope, policy, index, EvaluationArmV1::Delegated);
        create_assignment(storage, scope, 60 + index * 10, &react, 51 + index);
        create_assignment(storage, scope, 61 + index * 10, &delegated, 52 + index);
        commit_assignment_job(storage, scope, &react, 70 + index * 10);
        commit_assignment_job(storage, scope, &delegated, 71 + index * 10);
        let pair = paired_sample(&react, &delegated, 100 - index * 5, 80 - index * 5);
        let command = RecordProjectedEvaluationPair {
            scope: scope.clone(),
            expected_gate_revision: 1,
            projected_pair: ProjectedEvaluationPair::try_from_authority(pair)
                .expect("project pair from authority"),
            occurred_at_millis: 90 + index,
        };
        let reference = RolloutEvaluationService::new(storage)
            .record_projected_pair(command.clone())
            .expect("record authorized raw pair");
        assert_eq!(
            RolloutEvaluationService::new(storage)
                .record_projected_pair(command)
                .expect("replay exact authorized pair"),
            reference
        );
        if index == 1 {
            assert_changed_pair_rejected(storage, scope, &react, &delegated);
        }
        reference
    }

    fn assert_changed_pair_rejected(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
        react: &EvaluationAssignmentV1,
        delegated: &EvaluationAssignmentV1,
    ) {
        let changed = paired_sample(react, delegated, 94, 74);
        let changed_digest = changed.digest().clone();
        let error = RolloutEvaluationService::new(storage)
            .record_projected_pair(RecordProjectedEvaluationPair {
                scope: scope.clone(),
                expected_gate_revision: 1,
                projected_pair: ProjectedEvaluationPair::try_from_authority(changed)
                    .expect("project changed pair"),
                occurred_at_millis: 95,
            })
            .expect_err("changed evidence cannot replace a paired slot");
        assert_eq!(error.kind(), RolloutEvaluationErrorKind::RevisionConflict);
        let stream = format!(
            "rollout-evaluation-pair:{}",
            changed_digest.0.trim_start_matches("sha256:")
        );
        assert!(storage.load_state(&stream).expect("load pair").is_none());
    }

    fn create_assignment(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
        request: u64,
        assignment: &EvaluationAssignmentV1,
        occurred_at_millis: u64,
    ) {
        if let Err(error) =
            RolloutEvaluationService::new(storage).create_assignment(CreateEvaluationAssignment {
                scope: scope.clone(),
                request_id: request_id(request),
                expected_gate_revision: 1,
                assignment: assignment.clone(),
                occurred_at_millis,
            })
        {
            panic!("create evaluation assignment request {request}: {error:?}");
        }
    }

    fn commit_assignment_job(
        storage: &mut SqliteStorage,
        scope: &RepositoryScope,
        assignment: &EvaluationAssignmentV1,
        seed: u64,
    ) {
        let mode = match assignment.spec().arm {
            EvaluationArmV1::React => ExecutionMode::React,
            EvaluationArmV1::Delegated => ExecutionMode::DelegatedPatch,
        };
        let seal = seal_writer_job(
            storage,
            scope,
            &WriterJobSealInput {
                configured_mode: mode,
                role: "executor",
                evaluation_assignment: Some(assignment.digest()),
                job_id: &assignment.spec().job_id,
                base_revision: &assignment.spec().base_revision,
                now_millis: seed,
            },
        )
        .expect("seal evaluation writer")
        .expect("writer seal");
        let job = writer_job(
            scope,
            assignment.spec().job_id.clone(),
            &assignment.spec().base_revision,
            seal.write_mode(),
            seed,
        );
        storage
            .commit(&bound_job_commit(&job, &seal, seed))
            .expect("commit assigned evaluation Job");
    }

    fn bound_job_commit(job: &ExecutionJob, seal: &RolloutGateJobSeal, seed: u64) -> StateCommit {
        bind_writer_job(
            StateCommit::new(
                ReceiptIdentity::new(
                    ReceiptActorKey::from_encoded(format!("rollout-job-actor-{seed}").into_bytes())
                        .expect("actor key"),
                    ReceiptScopeKey::from_encoded(format!("rollout-job-scope-{seed}").into_bytes())
                        .expect("scope key"),
                    request_id(1_000 + seed),
                )
                .expect("receipt identity"),
                digest(2_000 + seed),
                format!("writer-job-fixture-{seed}"),
                0,
                format!("sealed-writer-job-{seed}").into_bytes(),
                vec![NewOutboxEvent::internal(
                    format!("writer-job-dispatch-{seed}"),
                    crate::delivery_transaction::EXECUTION_JOB_TOPIC,
                    serde_json::to_vec(job).expect("encode writer Job"),
                )],
            ),
            job,
            Some(seal),
        )
        .expect("bind writer authority")
    }

    fn paired_sample(
        react: &EvaluationAssignmentV1,
        delegated: &EvaluationAssignmentV1,
        react_runtime: u64,
        delegated_runtime: u64,
    ) -> PerformancePairedSampleV1 {
        let (react_authorization, react_measurement) = arm_sample(react, react_runtime, 100);
        let (delegated_authorization, delegated_measurement) =
            arm_sample(delegated, delegated_runtime, 80);
        PerformancePairedSampleV1::try_new(
            react_authorization,
            react_measurement,
            delegated_authorization,
            delegated_measurement,
        )
        .expect("build authorized pair")
    }

    fn arm_sample(
        assignment: &EvaluationAssignmentV1,
        wall_clock_runtime: u64,
        metric: i64,
    ) -> (EvaluationAuthorizationV1, PerformanceArmMeasurementV1) {
        let spec = assignment.spec();
        let model_call_id = digest(3_000 + numeric_suffix(&spec.run_id));
        let dispatched = 1_000 + numeric_suffix(&spec.run_id);
        let execution_mode = match spec.arm {
            EvaluationArmV1::React => ExecutionMode::React,
            EvaluationArmV1::Delegated => ExecutionMode::DelegatedPatch,
        };
        let authorization = EvaluationAuthorizationV1::try_new(
            assignment.clone(),
            EvaluationAuthorizationFactsV1 {
                candidate_artifact: artifact(4_000 + numeric_suffix(&spec.run_id)),
                evidence_cutoff: EvaluationEvidenceCutoffV1 {
                    cutoff_at_millis: CUTOFF_MILLIS,
                    control_plane_terminal_cursor: 1,
                    retry_ledger_cursor: 1,
                    candidate_ack_cursor: 1,
                    artifact_acknowledged_sequence: 1,
                    worker_ledger_snapshot_digest: digest(
                        6_000 + numeric_suffix(&spec.run_id),
                    ),
                    artifact_snapshot_digest: digest(7_000 + numeric_suffix(&spec.run_id)),
                },
                candidate_artifact_ack_revision: 1,
                dispatch_accepted_at_millis: dispatched,
                worker_terminal_finished_at_millis: dispatched + wall_clock_runtime - 1,
                terminal_accepted_at_millis: dispatched + wall_clock_runtime,
                terminal_revision: 1,
                authorization_revision: 1,
                primary_model_calls: vec![EvaluationModelCallAuthorityV1 {
                    model_call_digest: model_call_id.clone(),
                    retry_state_revision: 1,
                    retry_plan: spec.attempt_policy.primary.clone(),
                    attempts: vec![EvaluationRouteAttemptV1 {
                        ordinal: 1,
                        step_index: 0,
                        attempt_on_step: 1,
                        route: route(),
                        provider_exchange_digest: digest(5_000 + numeric_suffix(&spec.run_id)),
                        outcome: EvaluationAttemptOutcomeV1::Succeeded,
                        settled_usage: Some(EvaluationSettledUsageV1 {
                            provider_usage_id: format!("usage-{}", numeric_suffix(&spec.run_id)),
                            provider_id: "provider-fixture".to_owned(),
                            model_id: "model-fixture".to_owned(),
                            input_tokens: u64::try_from(metric).expect("positive input"),
                            cached_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 0,
                            reasoning_output_tokens: 0,
                            total_tokens: u64::try_from(metric).expect("positive total"),
                            cost_microunits: u64::try_from(metric).expect("positive cost"),
                        }),
                    }],
                }],
                observer_model_calls: Vec::new(),
            },
        )
        .expect("seal arm authorization");
        let run = PerformanceV0RunEvidence {
            run_id: spec.run_id.clone(),
            execution_mode,
            observer_mode: ObserverMode::Off,
            primary_model_call_count: 1,
            primary_model_input_tokens: metric,
            primary_model_cached_tokens: 0,
            primary_model_output_tokens: 0,
            primary_model_wait_ms: metric,
            observer_call_count: 0,
            observer_wait_ms: 0,
            total_runtime_ms: metric,
        };
        let call = PerformanceV0ModelCallEvidence {
            run_id: spec.run_id.clone(),
            model_call_id,
            model_kind: PerformanceV0ModelKind::Primary,
            completed: true,
            input_tokens: metric,
            cached_tokens: 0,
            output_tokens: 0,
            elapsed_millis: metric,
            actual_cost_microunits: Some(metric),
        };
        let measurement =
            PerformanceArmMeasurementV1::from_v0(run, vec![call]).expect("measure arm");
        (authorization, measurement)
    }

    fn assignment(
        scope: &RepositoryScope,
        policy: &RolloutGatePolicyReference,
        index: u64,
        arm: EvaluationArmV1,
    ) -> EvaluationAssignmentV1 {
        let arm_offset = match arm {
            EvaluationArmV1::React => 0,
            EvaluationArmV1::Delegated => 10,
        };
        EvaluationAssignmentV1::try_new(EvaluationAssignmentSpecV1 {
            repository_scope: scope.clone(),
            source_release: artifact(1),
            cohort_manifest: artifact(2),
            cohort_id: digest(3),
            case_id: digest(200 + index),
            pair_id: digest(100 + index),
            arm,
            base_revision: format!("base-{index}"),
            job_id: ExecutionJobId(canonical_id("job", index * 20 + arm_offset)),
            run_id: digest(300 + index * 20 + arm_offset),
            primary_planned_routes: vec![route()],
            observer: EvaluationObserverV1 {
                mode: ObserverMode::Off,
                planned_routes: Vec::new(),
            },
            attempt_policy: attempt_policy(),
            policy_revision: policy.revision(),
            policy_digest: policy.digest().clone(),
            cutoff_at_millis: CUTOFF_MILLIS,
        })
        .expect("build evaluation assignment")
    }

    fn writer_job(
        scope: &RepositoryScope,
        job_id: ExecutionJobId,
        base_revision: &str,
        write_mode: ExecutionWorkspaceWriteMode,
        seed: u64,
    ) -> ExecutionJob {
        ExecutionJob {
            attempt: 1,
            execution_profile: "executor".to_owned(),
            goal: "apply one sealed change batch".to_owned(),
            job_id,
            limits: ExecutionLimits {
                deadline_at: Instant("2030-01-01T00:05:00.000Z".to_owned()),
                max_artifact_bytes: 1_024,
                max_runtime_seconds: 60,
            },
            payload_digest: digest(6_000 + seed),
            scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
                kind: ProductSessionExecutionScopeKind::ProductSession,
                product_session_id: ProductSessionId(canonical_id("psn", seed)),
            }),
            stage_input: None,
            workspace: ExecutionWorkspace {
                checkout_revision: base_revision.to_owned(),
                repository_id: scope.repository_id.clone(),
                write_mode,
            },
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
                plan_fingerprint: digest(6_000),
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
            artifact_id: ArtifactId(canonical_id("art", index)),
            digest: digest(7_000 + index),
        }
    }

    fn scope(seed: u64) -> RepositoryScope {
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId(canonical_id("org", seed)),
            workspace_id: WorkspaceId(canonical_id("wsp", seed)),
            project_id: ProjectId(canonical_id("prj", seed)),
            repository_id: RepositoryId(canonical_id("rep", seed)),
        }
    }

    fn request_id(seed: u64) -> RequestId {
        RequestId(canonical_id("req", seed))
    }

    fn digest(value: u64) -> Sha256Digest {
        Sha256Digest(format!("sha256:{value:064x}"))
    }

    fn numeric_suffix(digest: &Sha256Digest) -> u64 {
        u64::from_str_radix(&digest.0[digest.0.len() - 8..], 16).expect("digest suffix")
    }

    fn canonical_id(prefix: &str, seed: u64) -> String {
        format!("{prefix}_{seed:026}")
    }

    fn unique_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "winwincode-rollout-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }
}
