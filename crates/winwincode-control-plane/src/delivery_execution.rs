// SPDX-License-Identifier: Apache-2.0

//! Atomic Delivery-to-ExecutionPort dispatch composition.
//!
//! This module maps one Delivery-owned pending effect into generated public
//! types. It neither schedules Codex work nor treats an uncommitted effect as
//! HTTP success. The transaction adapter must commit Delivery journal state,
//! request receipt, and the immutable job outbox intent together before the
//! dispatcher is called.

use std::{collections::HashSet, error::Error, fmt};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    DeliveryReworkAuthorizationScope, DeliveryReworkTargetScope, DeliveryStageExecutionScope,
    ExecutionJob, ExecutionLimits, ExecutionScope, ExecutionWorkspace, JobCancelAckMessage,
};
use winwincode_delivery::{
    application::{
        CoordinationError,
        stage::{
            ActiveLeaseIdentity, CancelAcknowledgement, CancelIntent, ExecutionIntent,
            StageAdvanceEffect, StageAdvanceResult, acknowledge_cancel,
        },
    },
    domain::{
        Delivery, DeliveryStage, StageRunActorType, StageRunStatus, rework::ReworkAuthorization,
    },
};
use winwincode_domain::{RequestId, Sha256Digest};

#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryExecutionConfig {
    pub payload_digest: Sha256Digest,
    pub workspace: ExecutionWorkspace,
    pub limits: ExecutionLimits,
}

/// Delivery mutation and generated job waiting for one outer transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingDeliveryExecution {
    request_id: RequestId,
    stage_transition: StageAdvanceResult,
    job: ExecutionJob,
}

impl PendingDeliveryExecution {
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub fn delivery(&self) -> &Delivery {
        &self.stage_transition.delivery
    }

    pub fn stage_transition(&self) -> &StageAdvanceResult {
        &self.stage_transition
    }

    pub fn job(&self) -> &ExecutionJob {
        &self.job
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryExecutionPortError {
    message: String,
}

impl DeliveryExecutionPortError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for DeliveryExecutionPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DeliveryExecutionPortError {}

/// Adapter seam for the transaction completed by phase 2.3.1.
///
/// Implementations must atomically commit the Delivery journal publication,
/// request receipt, and `job.dispatch` outbox intent. A replay receipt means
/// the same request was already committed and must not append another intent.
pub trait DeliveryExecutionTransaction {
    /// Commits the pending Delivery and job intent as one authoritative change.
    ///
    /// # Errors
    ///
    /// Returns without dispatch when the outer transaction cannot commit.
    fn commit_delivery_and_job_intent(
        &mut self,
        pending: &PendingDeliveryExecution,
    ) -> Result<DeliveryExecutionCommitReceipt, DeliveryExecutionPortError>;

    /// Marks the exact durable outbox event published after dispatch succeeds.
    ///
    /// # Errors
    ///
    /// Leaves the event pending for startup/outbox replay when acknowledgement
    /// cannot be committed.
    fn mark_job_dispatched(
        &mut self,
        outbox_event_id: &str,
    ) -> Result<(), DeliveryExecutionPortError>;
}

/// `ExecutionPort` adapter called only after the outer transaction commits.
pub trait ExecutionJobDispatcher {
    /// Sends or offers one immutable generated `ExecutionJob`.
    ///
    /// # Errors
    ///
    /// Leaves the committed outbox intent pending for replay.
    fn dispatch(&mut self, job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryExecutionCommitReceipt {
    pub committed_revision: u64,
    pub outbox_event_id: String,
    pub job: ExecutionJob,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryExecutionDispatchReceipt {
    pub commit: DeliveryExecutionCommitReceipt,
    pub dispatched: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeliveryExecutionError {
    InvalidEffect(String),
    Coordination(CoordinationError),
    Commit(DeliveryExecutionPortError),
    CommittedPayloadInvalid {
        commit: Box<DeliveryExecutionCommitReceipt>,
        message: String,
    },
    DispatchAfterCommit {
        commit: Box<DeliveryExecutionCommitReceipt>,
        source: DeliveryExecutionPortError,
    },
    AcknowledgeAfterDispatch {
        commit: Box<DeliveryExecutionCommitReceipt>,
        source: DeliveryExecutionPortError,
    },
}

impl DeliveryExecutionError {
    pub fn committed_receipt(&self) -> Option<&DeliveryExecutionCommitReceipt> {
        match self {
            Self::CommittedPayloadInvalid { commit, .. }
            | Self::DispatchAfterCommit { commit, .. }
            | Self::AcknowledgeAfterDispatch { commit, .. } => Some(commit.as_ref()),
            Self::InvalidEffect(_) | Self::Coordination(_) | Self::Commit(_) => None,
        }
    }
}

impl fmt::Display for DeliveryExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEffect(message) => formatter.write_str(message),
            Self::Coordination(error) => write!(formatter, "Delivery coordination failed: {error}"),
            Self::Commit(error) => write!(formatter, "Delivery execution commit failed: {error}"),
            Self::CommittedPayloadInvalid { message, .. } => write!(
                formatter,
                "Delivery and job intent committed, but durable payload is invalid: {message}"
            ),
            Self::DispatchAfterCommit { source, .. } => write!(
                formatter,
                "Delivery and job intent committed, but dispatch remains pending: {source}"
            ),
            Self::AcknowledgeAfterDispatch { source, .. } => write!(
                formatter,
                "ExecutionJob dispatched, but its durable outbox acknowledgement remains pending: {source}"
            ),
        }
    }
}

impl Error for DeliveryExecutionError {}

/// Converts one Delivery-owned dispatch effect to the generated `ExecutionJob`.
///
/// The result remains pending until [`commit_and_dispatch`] obtains a durable
/// commit receipt.
///
/// # Errors
///
/// Rejects review/resume effects, malformed dispatch configuration, and an
/// attempt outside the generated transport integer range.
pub fn prepare_delivery_advance(
    request_id: RequestId,
    result: StageAdvanceResult,
    config: DeliveryExecutionConfig,
) -> Result<PendingDeliveryExecution, DeliveryExecutionError> {
    result.validate_projection().map_err(|error| {
        DeliveryExecutionError::InvalidEffect(format!("invalid sealed stage transition: {error}"))
    })?;
    let StageAdvanceEffect::Dispatch(intent) = &result.effect else {
        return Err(DeliveryExecutionError::InvalidEffect(
            "only a newly committed Codex stage creates an ExecutionJob intent".to_owned(),
        ));
    };
    validate_dispatch_intent(&result.delivery, intent)?;
    let attempt = i64::try_from(intent.attempt).map_err(|_| {
        DeliveryExecutionError::InvalidEffect(
            "Delivery stage attempt exceeds the ExecutionPort range".to_owned(),
        )
    })?;
    let (goal, payload_digest, rework_authorization) = execution_payload(intent, &config)?;
    let job = ExecutionJob {
        attempt,
        execution_profile: intent.role.clone(),
        goal,
        job_id: intent.execution_job_id.clone(),
        limits: config.limits,
        payload_digest,
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id: intent.delivery_id.clone(),
            delivery_task_id: intent.delivery_task_id.clone(),
            kind: "delivery-stage".to_owned(),
            product_session_id: intent.product_session_id.clone(),
            rework_authorization,
            stage_run_id: intent.stage_run_id.clone(),
        }),
        workspace: config.workspace,
    };
    validate_request_id(&request_id)?;
    validate_execution_job(&job)?;
    Ok(PendingDeliveryExecution {
        request_id,
        stage_transition: result,
        job,
    })
}

/// Commits state and outbox intent, then dispatches at most once for this call.
///
/// # Errors
///
/// Returns [`DeliveryExecutionError::Commit`] when nothing committed, or
/// [`DeliveryExecutionError::DispatchAfterCommit`] when durable replay must
/// finish publication.
pub fn commit_and_dispatch(
    pending: &PendingDeliveryExecution,
    transaction: &mut dyn DeliveryExecutionTransaction,
    dispatcher: &mut dyn ExecutionJobDispatcher,
) -> Result<DeliveryExecutionDispatchReceipt, DeliveryExecutionError> {
    let commit = transaction
        .commit_delivery_and_job_intent(pending)
        .map_err(DeliveryExecutionError::Commit)?;
    if let Err(message) = validate_commit_receipt(pending, &commit) {
        return Err(DeliveryExecutionError::CommittedPayloadInvalid {
            commit: Box::new(commit),
            message,
        });
    }
    if commit.replayed {
        return Ok(DeliveryExecutionDispatchReceipt {
            commit,
            dispatched: false,
        });
    }
    dispatcher.dispatch(&commit.job).map_err(|source| {
        DeliveryExecutionError::DispatchAfterCommit {
            commit: Box::new(commit.clone()),
            source,
        }
    })?;
    transaction
        .mark_job_dispatched(&commit.outbox_event_id)
        .map_err(|source| DeliveryExecutionError::AcknowledgeAfterDispatch {
            commit: Box::new(commit.clone()),
            source,
        })?;
    Ok(DeliveryExecutionDispatchReceipt {
        commit,
        dispatched: true,
    })
}

fn validate_commit_receipt(
    pending: &PendingDeliveryExecution,
    commit: &DeliveryExecutionCommitReceipt,
) -> Result<(), String> {
    validate_execution_job(&commit.job).map_err(|error| error.to_string())?;
    if commit.committed_revision != pending.delivery().revision() {
        return Err("durable receipt revision does not match the committed Delivery".to_owned());
    }
    if !bounded_length(commit.outbox_event_id.trim(), 1, 200) {
        return Err("durable receipt has an invalid outbox event identity".to_owned());
    }
    let receipt_delivery_id = match &commit.job.scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => &scope.delivery_id,
        ExecutionScope::ProductSessionExecutionScope(_) => {
            return Err("durable receipt job is not a Delivery stage job".to_owned());
        }
    };
    if receipt_delivery_id != pending.delivery().id() {
        return Err("durable receipt job belongs to another Delivery".to_owned());
    }
    if !commit.replayed && commit.job != pending.job {
        return Err("new durable receipt does not contain the exact pending job".to_owned());
    }
    Ok(())
}

fn validate_dispatch_intent(
    delivery: &Delivery,
    intent: &ExecutionIntent,
) -> Result<(), DeliveryExecutionError> {
    intent
        .validate_for_delivery(delivery)
        .map_err(DeliveryExecutionError::Coordination)?;
    match (intent.stage, intent.rework_authorization()) {
        (DeliveryStage::Reworking, Some(authorization)) => authorization
            .validate_started_dispatch(delivery, &intent.stage_run_id)
            .map_err(|error| {
                DeliveryExecutionError::InvalidEffect(format!(
                    "invalid rework dispatch authorization: {error}"
                ))
            })?,
        (DeliveryStage::Reworking, None) => {
            return Err(DeliveryExecutionError::InvalidEffect(
                "invalid rework dispatch authorization: missing sealed authorization".to_owned(),
            ));
        }
        (_, Some(_)) => {
            return Err(DeliveryExecutionError::InvalidEffect(
                "invalid dispatch intent: non-reworking stage carries rework authorization"
                    .to_owned(),
            ));
        }
        (_, None) => {}
    }
    let mut runs = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| run.id == intent.stage_run_id && run.status == StageRunStatus::Running);
    let run = runs.next().ok_or_else(|| {
        DeliveryExecutionError::InvalidEffect(
            "invalid dispatch intent: no matching running Delivery StageRun".to_owned(),
        )
    })?;
    if runs.next().is_some()
        || run.delivery_id != intent.delivery_id
        || run.delivery_task_id != intent.delivery_task_id
        || run.stage != intent.stage
        || run.actor_type != StageRunActorType::Codex
        || run.role != intent.role
        || run.attempt != intent.attempt
    {
        return Err(DeliveryExecutionError::InvalidEffect(
            "invalid dispatch intent: fields differ from the exact Delivery StageRun".to_owned(),
        ));
    }
    let mut bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == intent.stage_run_id);
    let binding = bindings.next().ok_or_else(|| {
        DeliveryExecutionError::InvalidEffect(
            "invalid dispatch intent: no exact Delivery SessionBinding".to_owned(),
        )
    })?;
    if bindings.next().is_some()
        || binding.delivery_id != intent.delivery_id
        || binding.delivery_task_id != intent.delivery_task_id
        || binding.product_session_id != intent.product_session_id
        || binding.execution_job_id != intent.execution_job_id
        || binding.worker_session_id.is_some()
        || binding.codex_thread_id.is_some()
    {
        return Err(DeliveryExecutionError::InvalidEffect(
            "invalid dispatch intent: fields differ from the exact pending SessionBinding"
                .to_owned(),
        ));
    }
    Ok(())
}

fn execution_payload(
    intent: &ExecutionIntent,
    config: &DeliveryExecutionConfig,
) -> Result<
    (
        String,
        Sha256Digest,
        Option<DeliveryReworkAuthorizationScope>,
    ),
    DeliveryExecutionError,
> {
    match (intent.stage, intent.rework_authorization()) {
        (DeliveryStage::Reworking, Some(authorization)) => {
            validate_rework_dispatch_scope(intent, config, authorization)?;
            let authorization_scope = execution_rework_scope(authorization);
            let payload_digest = authorized_payload_digest(
                &intent.goal,
                &config.payload_digest,
                &authorization_scope,
            )?;
            Ok((
                intent.goal.clone(),
                payload_digest,
                Some(authorization_scope),
            ))
        }
        (DeliveryStage::Reworking, None) => Err(DeliveryExecutionError::InvalidEffect(
            "remediator ExecutionJob is missing its sealed rework authorization".to_owned(),
        )),
        (_, Some(_)) => Err(DeliveryExecutionError::InvalidEffect(
            "non-reworking ExecutionJob cannot carry a rework authorization".to_owned(),
        )),
        (_, None) => Ok((intent.goal.clone(), config.payload_digest.clone(), None)),
    }
}

fn execution_rework_scope(authorization: &ReworkAuthorization) -> DeliveryReworkAuthorizationScope {
    DeliveryReworkAuthorizationScope {
        authorization_digest: authorization.authorization_digest().clone(),
        candidate_ref: authorization.candidate_ref().to_owned(),
        diff_sha256: authorization.diff_sha256().to_owned(),
        requires_full_reverification: authorization.requires_full_reverification(),
        source_candidate_commit_id: authorization
            .previous_candidate()
            .candidate_commit_id()
            .to_owned(),
        source_candidate_tree_id: authorization
            .previous_candidate()
            .candidate_tree_id()
            .to_owned(),
        targets: authorization
            .targets()
            .iter()
            .map(|target| DeliveryReworkTargetScope {
                delivery_task_id: target.delivery_task_id().clone(),
                diagram_id: target.diagram_id().to_owned(),
                evidence_ref_ids: target
                    .evidence_ref_ids()
                    .iter()
                    .map(|id| id.0.clone())
                    .collect(),
                file_path: target.file_path().to_owned(),
                node_id: target.node_id().to_owned(),
                source_hunk_sha256: target.hunk_sha256().to_owned(),
            })
            .collect(),
    }
}

fn authorized_payload_digest(
    base_goal: &str,
    base_payload_digest: &Sha256Digest,
    authorization: &DeliveryReworkAuthorizationScope,
) -> Result<Sha256Digest, DeliveryExecutionError> {
    let encoded =
        serde_json::to_vec(&(base_goal, base_payload_digest, authorization)).map_err(|error| {
            DeliveryExecutionError::InvalidEffect(format!(
                "rework authorization cannot be encoded: {error}"
            ))
        })?;
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode/rework-execution-payload/v1\0");
    hasher.update(encoded);
    Ok(Sha256Digest(format!("sha256:{:x}", hasher.finalize())))
}

fn validate_rework_dispatch_scope(
    intent: &ExecutionIntent,
    config: &DeliveryExecutionConfig,
    authorization: &ReworkAuthorization,
) -> Result<(), DeliveryExecutionError> {
    let exact = intent.role == authorization.writer_role()
        && intent.delivery_task_id.as_ref() == Some(authorization.delivery_task_id())
        && intent.attempt == authorization.next_attempt()
        && config.workspace.checkout_revision
            == authorization.previous_candidate().candidate_commit_id();
    if exact {
        Ok(())
    } else {
        Err(DeliveryExecutionError::InvalidEffect(
            "remediator ExecutionJob does not match its authorized task, attempt, or candidate checkout"
                .to_owned(),
        ))
    }
}

/// Accepts a generated `job.cancel_ack` without treating it as terminal.
///
/// The acknowledgement must match the exact cancellation request and current
/// lease. The returned Delivery remains unchanged; only a separately verified
/// terminal `job.outcome` may settle the `StageRun`.
///
/// # Errors
///
/// Rejects malformed generated values, another request or lease, and a stale
/// Delivery binding.
pub fn acknowledge_job_cancel(
    delivery: &Delivery,
    intent: &CancelIntent,
    lease: &ActiveLeaseIdentity,
    expected_request_id: &RequestId,
    acknowledgement: &JobCancelAckMessage,
) -> Result<Delivery, DeliveryExecutionError> {
    validate_cancel_ack(acknowledgement)?;
    let lease_attempt = u64::try_from(acknowledgement.lease.attempt).map_err(|_| {
        DeliveryExecutionError::InvalidEffect(
            "job.cancel_ack attempt is outside the Delivery range".to_owned(),
        )
    })?;
    let exact = acknowledgement.request_id == *expected_request_id
        && acknowledgement.lease.job_id == lease.execution_job_id
        && lease_attempt == lease.attempt
        && acknowledgement.lease.lease_id == lease.lease_id
        && acknowledgement.lease.fencing_token == lease.fencing_token
        && acknowledgement.lease.worker_id == lease.worker_id
        && acknowledgement.lease.worker_instance_id == lease.worker_instance_id
        && acknowledgement.worker_session_id == lease.worker_session_id;
    if !exact {
        return Err(DeliveryExecutionError::InvalidEffect(
            "job.cancel_ack does not match the exact request and active lease".to_owned(),
        ));
    }
    acknowledge_cancel(
        delivery,
        intent,
        &CancelAcknowledgement {
            stage_run_id: intent.stage_run_id.clone(),
            execution_job_id: acknowledgement.lease.job_id.clone(),
            attempt: lease_attempt,
            worker_session_id: acknowledgement.worker_session_id.clone(),
        },
    )
    .map_err(DeliveryExecutionError::Coordination)
}

fn validate_request_id(request_id: &RequestId) -> Result<(), DeliveryExecutionError> {
    if canonical_identifier(&request_id.0, "req") {
        Ok(())
    } else {
        Err(invalid_execution_value("requestId"))
    }
}

fn validate_execution_job(job: &ExecutionJob) -> Result<(), DeliveryExecutionError> {
    let (scope_identity_valid, rework_scope_error) = match &job.scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => (
            scope.kind == "delivery-stage"
                && canonical_identifier(&scope.product_session_id.0, "psn")
                && canonical_identifier(&scope.delivery_id.0, "dlv")
                && scope
                    .delivery_task_id
                    .as_ref()
                    .is_none_or(|id| canonical_identifier(&id.0, "dtk"))
                && canonical_identifier(&scope.stage_run_id.0, "run"),
            delivery_rework_scope_error(job, scope),
        ),
        ExecutionScope::ProductSessionExecutionScope(_) => (false, Some("executionJob.scope.kind")),
    };
    for (field, valid) in [
        (
            "executionJob.jobId",
            canonical_identifier(&job.job_id.0, "job"),
        ),
        ("executionJob.attempt", (1..=1_000).contains(&job.attempt)),
        (
            "executionJob.payloadDigest",
            sha256_digest(&job.payload_digest.0),
        ),
        ("executionJob.scope.identity", scope_identity_valid),
        (
            "executionJob.workspace.repositoryId",
            canonical_identifier(&job.workspace.repository_id.0, "rep"),
        ),
        (
            "executionJob.workspace.checkoutRevision",
            bounded_length(&job.workspace.checkout_revision, 1, 200),
        ),
        (
            "executionJob.workspace.writeMode",
            job.workspace.write_mode == "candidate",
        ),
        (
            "executionJob.executionProfile",
            bounded_length(&job.execution_profile, 1, 100),
        ),
        ("executionJob.goal", bounded_length(&job.goal, 1, 20_000)),
        (
            "executionJob.limits.deadlineAt",
            instant(&job.limits.deadline_at.0),
        ),
        (
            "executionJob.limits.maxRuntimeSeconds",
            (1..=604_800).contains(&job.limits.max_runtime_seconds),
        ),
        (
            "executionJob.limits.maxArtifactBytes",
            (0..=1_099_511_627_776).contains(&job.limits.max_artifact_bytes),
        ),
    ] {
        if !valid {
            return Err(invalid_execution_value(field));
        }
    }
    if let Some(field) = rework_scope_error {
        return Err(invalid_execution_value(field));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "every structured authorization field stays visibly fail-closed at the Worker boundary"
)]
fn delivery_rework_scope_error(
    job: &ExecutionJob,
    scope: &DeliveryStageExecutionScope,
) -> Option<&'static str> {
    let Some(authorization) = scope.rework_authorization.as_ref() else {
        return (job.execution_profile == "remediator")
            .then_some("executionJob.scope.reworkAuthorization");
    };
    let Some(task_id) = scope.delivery_task_id.as_ref() else {
        return Some("executionJob.scope.deliveryTaskId");
    };
    for (field, valid) in [
        (
            "executionJob.executionProfile",
            job.execution_profile == "remediator",
        ),
        (
            "executionJob.workspace.checkoutRevision",
            job.workspace.checkout_revision == authorization.source_candidate_commit_id,
        ),
        (
            "executionJob.scope.reworkAuthorization.requiresFullReverification",
            authorization.requires_full_reverification,
        ),
        (
            "executionJob.scope.reworkAuthorization.authorizationDigest",
            sha256_digest(&authorization.authorization_digest.0),
        ),
        (
            "executionJob.scope.reworkAuthorization.candidateRef",
            authorization
                .candidate_ref
                .strip_prefix("git-candidate:sha256:")
                .is_some_and(lowercase_sha256),
        ),
        (
            "executionJob.scope.reworkAuthorization.diffSha256",
            lowercase_sha256(&authorization.diff_sha256),
        ),
        (
            "executionJob.scope.reworkAuthorization.sourceCandidateCommitId",
            git_object_id(&authorization.source_candidate_commit_id),
        ),
        (
            "executionJob.scope.reworkAuthorization.sourceCandidateTreeId",
            git_object_id(&authorization.source_candidate_tree_id),
        ),
        (
            "executionJob.scope.reworkAuthorization.targets",
            !authorization.targets.is_empty() && authorization.targets.len() <= 1_000,
        ),
    ] {
        if !valid {
            return Some(field);
        }
    }
    let mut targets = HashSet::with_capacity(authorization.targets.len());
    for target in &authorization.targets {
        let key = (
            target.delivery_task_id.0.as_str(),
            target.diagram_id.as_str(),
            target.node_id.as_str(),
            target.file_path.as_str(),
            target.source_hunk_sha256.as_str(),
        );
        let mut evidence = HashSet::with_capacity(target.evidence_ref_ids.len());
        for (field, valid) in [
            (
                "executionJob.scope.reworkAuthorization.targets.deliveryTaskId",
                target.delivery_task_id == *task_id
                    && canonical_identifier(&target.delivery_task_id.0, "dtk"),
            ),
            (
                "executionJob.scope.reworkAuthorization.targets.diagramId",
                portable_execution_identifier(&target.diagram_id),
            ),
            (
                "executionJob.scope.reworkAuthorization.targets.nodeId",
                portable_execution_identifier(&target.node_id),
            ),
            (
                "executionJob.scope.reworkAuthorization.targets.filePath",
                portable_path(&target.file_path),
            ),
            (
                "executionJob.scope.reworkAuthorization.targets.sourceHunkSha256",
                lowercase_sha256(&target.source_hunk_sha256),
            ),
            (
                "executionJob.scope.reworkAuthorization.targets.evidenceRefIds",
                !target.evidence_ref_ids.is_empty()
                    && target.evidence_ref_ids.len() <= 1_000
                    && target
                        .evidence_ref_ids
                        .iter()
                        .all(|id| derived_evidence_id(id) && evidence.insert(id.as_str())),
            ),
            (
                "executionJob.scope.reworkAuthorization.targets",
                targets.insert(key),
            ),
        ] {
            if !valid {
                return Some(field);
            }
        }
    }
    None
}

fn git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && lowercase_hex(value)
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64 && lowercase_hex(value)
}

fn derived_evidence_id(value: &str) -> bool {
    value
        .strip_prefix("evidence:sha256:")
        .is_some_and(lowercase_sha256)
}

fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn portable_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4_096
        && !value.starts_with('/')
        && !value.contains('\\')
        && !value.bytes().any(|byte| byte <= 31 || byte == 127)
        && !value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        && !value
            .get(1..2)
            .is_some_and(|second| second == ":" && value.as_bytes()[0].is_ascii_alphabetic())
}

fn portable_execution_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 200
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

fn validate_cancel_ack(
    acknowledgement: &JobCancelAckMessage,
) -> Result<(), DeliveryExecutionError> {
    let lease = &acknowledgement.lease;
    let error_valid = acknowledgement
        .error
        .as_ref()
        .is_none_or(|error| bounded_length(&error.message, 1, 500));
    let valid = acknowledgement.kind == "job.cancel_ack"
        && canonical_identifier(&acknowledgement.message_id.0, "xmsg")
        && instant(&acknowledgement.sent_at.0)
        && canonical_identifier(&acknowledgement.request_id.0, "req")
        && canonical_identifier(&lease.lease_id.0, "lse")
        && canonical_identifier(&lease.job_id.0, "job")
        && canonical_identifier(&lease.worker_id.0, "wrk")
        && canonical_identifier(&lease.worker_instance_id.0, "wki")
        && (1..=1_000).contains(&lease.attempt)
        && fencing_token(&lease.fencing_token.0)
        && instant(&lease.issued_at.0)
        && instant(&lease.expires_at.0)
        && canonical_identifier(&acknowledgement.worker_session_id.0, "wsn")
        && matches!(
            acknowledgement.status.as_str(),
            "accepted"
                | "already_cancelling"
                | "already_terminal"
                | "rejected_expired_lease"
                | "rejected_stale_fencing_token"
                | "rejected_worker_instance"
        )
        && error_valid;
    if valid {
        Ok(())
    } else {
        Err(invalid_execution_value("job.cancel_ack"))
    }
}

fn invalid_execution_value(field: &str) -> DeliveryExecutionError {
    DeliveryExecutionError::InvalidEffect(format!(
        "generated ExecutionPort {field} is invalid under the canonical schema"
    ))
}

fn bounded_length(value: &str, minimum: usize, maximum: usize) -> bool {
    let length = value.chars().count();
    (minimum..=maximum).contains(&length)
}

fn canonical_identifier(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|tail| tail.strip_prefix('_'))
        .is_some_and(|ulid| {
            ulid.len() == 26
                && ulid.bytes().all(|byte| {
                    byte.is_ascii_digit()
                        || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
                })
        })
}

fn sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn fencing_token(value: &str) -> bool {
    (1..=20).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| matches!(byte, b'1'..=b'9'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn instant(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) && !byte.is_ascii_digit()
        })
    {
        return false;
    }
    let Some(year) = decimal(&bytes[0..4]) else {
        return false;
    };
    let Some(month) = decimal(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = decimal(&bytes[8..10]) else {
        return false;
    };
    let Some(hour) = decimal(&bytes[11..13]) else {
        return false;
    };
    let Some(minute) = decimal(&bytes[14..16]) else {
        return false;
    };
    let Some(second) = decimal(&bytes[17..19]) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day) && hour <= 23 && minute <= 59 && second <= 59
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then_some(value * 10 + u32::from(byte - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_domain::DeliveryTaskId;

    fn authorization_scope(file_path: &str) -> DeliveryReworkAuthorizationScope {
        DeliveryReworkAuthorizationScope {
            authorization_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            candidate_ref: format!("git-candidate:sha256:{}", "d".repeat(64)),
            diff_sha256: "b".repeat(64),
            requires_full_reverification: true,
            source_candidate_commit_id: "1".repeat(40),
            source_candidate_tree_id: "2".repeat(40),
            targets: vec![DeliveryReworkTargetScope {
                delivery_task_id: DeliveryTaskId("dtk_01J00000000000000000000000".into()),
                diagram_id: "diagram-main".into(),
                evidence_ref_ids: vec![format!("evidence:sha256:{}", "f".repeat(64))],
                file_path: file_path.into(),
                node_id: "node-api".into(),
                source_hunk_sha256: "e".repeat(64),
            }],
        }
    }

    #[test]
    fn rework_authorization_is_structured_and_part_of_the_payload_digest() {
        let base_digest = Sha256Digest(format!("sha256:{}", "a".repeat(64)));
        let first = authorization_scope("src/invitation.rs");
        let first_digest = authorized_payload_digest("repair invitation", &base_digest, &first)
            .expect("authorized payload");
        let replayed_digest = authorized_payload_digest("repair invitation", &base_digest, &first)
            .expect("deterministic replay payload");
        assert_eq!(first_digest, replayed_digest);
        let encoded = serde_json::to_value(&first).expect("structured authorization");
        assert_eq!(encoded["targets"][0]["filePath"], "src/invitation.rs");
        assert_eq!(encoded["targets"][0]["nodeId"], "node-api");
        assert_eq!(
            encoded["targets"][0]["evidenceRefIds"][0],
            format!("evidence:sha256:{}", "f".repeat(64))
        );

        let second = authorization_scope("src/foreign.rs");
        let second_digest = authorized_payload_digest("repair invitation", &base_digest, &second)
            .expect("changed authorization payload");
        assert_ne!(first_digest, second_digest);
        let changed_goal =
            authorized_payload_digest("repair another invitation", &base_digest, &first)
                .expect("changed base goal");
        assert_ne!(first_digest, changed_goal);
    }
}
