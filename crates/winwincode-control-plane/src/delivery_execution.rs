// SPDX-License-Identifier: Apache-2.0

//! Atomic Delivery-to-ExecutionPort dispatch composition.
//!
//! This module maps one Delivery-owned pending effect into generated public
//! types. It neither schedules Codex work nor treats an uncommitted effect as
//! HTTP success. The transaction adapter must commit Delivery journal state,
//! request receipt, and the immutable job outbox intent together before the
//! dispatcher is called.

use std::{error::Error, fmt};

#[cfg(test)]
use sha2::{Digest, Sha256};
use winwincode_delivery::{
    application::{
        CoordinationError,
        workrun_execution::{
            ActiveLeaseIdentity, CancelAcknowledgement, CancelIntent, ExecutionIntent,
            WorkRunStartResult, acknowledge_cancel,
        },
    },
    domain::Delivery,
};
use winwincode_domain::{RequestId, SchemaVersion, Sha256Digest};
use winwincode_execution_port::generated::{
    DeliveryReworkAuthorizationScope, ExecutionJob, ExecutionLimits, ExecutionScope,
    ExecutionWorkspace, ExecutionWorkspaceWriteMode, JobCancelAckMessage, JobCancelAckMessageKind,
    JobCancelAckMessageStatus, WorkRunExecutionScope, WorkRunExecutionScopeKind, WorkRunInput,
};

/// Builds a WorkRun-shaped dispatch intent before a Worker lease exists.
///
/// # Errors
/// Rejects stale identities, invalid role/workspace bindings or malformed request/job fields.
pub fn prepare_workrun_start(
    request_id: &RequestId,
    aggregate: &winwincode_delivery::application::workrun::WorkRunAggregate,
    spec: &winwincode_delivery::domain::DeliverySpec,
    intent: &ExecutionIntent,
    config: DeliveryExecutionConfig,
) -> Result<ExecutionJob, DeliveryExecutionError> {
    let item = aggregate
        .items
        .iter()
        .find(|item| item.id == intent.work_item_id)
        .ok_or_else(|| {
            DeliveryExecutionError::InvalidEffect(
                "stage execution intent references a missing WorkItem".to_owned(),
            )
        })?;
    if item.revision != intent.work_item_revision
        || aggregate.contract.id != intent.work_contract_id
        || aggregate.contract.revision != intent.work_contract_revision
    {
        return Err(DeliveryExecutionError::InvalidEffect(
            "stage execution intent contract or WorkItem revision is stale".to_owned(),
        ));
    }
    if aggregate
        .runs
        .iter()
        .any(|run| run.id == intent.work_run_id || run.execution_job_id == intent.execution_job_id)
    {
        return Err(DeliveryExecutionError::InvalidEffect(
            "new dispatch reuses an accepted WorkRun or ExecutionJob identity".to_owned(),
        ));
    }
    let attempt = i64::try_from(intent.attempt).map_err(|_| {
        DeliveryExecutionError::InvalidEffect(
            "stage execution attempt exceeds wire range".to_owned(),
        )
    })?;
    let rework_authorization =
        intent
            .rework_authorization()
            .map(|authorization| DeliveryReworkAuthorizationScope {
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
                    .map(
                        |target| winwincode_execution_port::generated::DeliveryReworkTargetScope {
                            work_item_id: target.work_item_id().clone(),

                            evidence_ref_ids: target.evidence_ref_ids().to_vec(),
                            file_path: target.file_path().to_owned(),

                            source_hunk_sha256: target.hunk_sha256().to_owned(),
                        },
                    )
                    .collect(),
            });
    let scope = WorkRunExecutionScope {
        attempt,
        kind: WorkRunExecutionScopeKind::WorkRun,
        product_session_id: intent.product_session_id.clone(),
        rework_authorization,
        work_contract_id: intent.work_contract_id.clone(),
        work_contract_revision: intent.work_contract_revision.clone(),
        work_item_id: intent.work_item_id.clone(),
        work_item_revision: intent.work_item_revision.clone(),
        work_run_id: intent.work_run_id.clone(),
    };
    let job = ExecutionJob {
        attempt,
        execution_profile: intent.role.clone(),
        goal: intent.goal.clone(),
        job_id: intent.execution_job_id.clone(),
        limits: config.limits,
        payload_digest: config.payload_digest,
        scope: ExecutionScope::WorkRunExecutionScope(scope),
        work_input: Some(WorkRunInput {
            delivery_spec_id: spec.id.0.clone(),
            delivery_spec_revision: winwincode_domain::Revision(
                i64::try_from(spec.revision).map_err(|_| {
                    DeliveryExecutionError::InvalidEffect(
                        "DeliverySpec revision exceeds wire range".into(),
                    )
                })?,
            ),
            candidate_ref: config.candidate_ref,
            schema_version: SchemaVersion::WinwincodeV1,
            work_contract: aggregate.contract.clone(),
            work_item: item.clone(),
        }),
        workspace: config.workspace,
    };
    validate_request_id(request_id)?;
    validate_workrun_execution_job(&job)?;
    Ok(job)
}

#[cfg(test)]
fn deterministic_workrun_id(
    request_id: &RequestId,
    contract_id: &str,
    work_item_id: &str,
    product_session_id: &str,
) -> String {
    let digest = Sha256::digest(
        [
            b"winwincode.workrun.v1\0".as_slice(),
            request_id.0.as_bytes(),
            b"\0",
            contract_id.as_bytes(),
            b"\0",
            work_item_id.as_bytes(),
            b"\0",
            product_session_id.as_bytes(),
        ]
        .concat(),
    );
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let value: String = digest
        .iter()
        .take(26)
        .map(|byte| char::from(alphabet[usize::from(byte & 31)]))
        .collect();
    format!("wrn_{value}")
}

/// Validates the canonical `WorkRun` execution shape before queue publication.
/// `WorkRun` identity and its embedded contract/item are the sole dispatch
/// authority for the new path.
pub(crate) fn validate_workrun_execution_job(
    job: &ExecutionJob,
) -> Result<(), DeliveryExecutionError> {
    validate_job_fields(job)?;
    let ExecutionScope::WorkRunExecutionScope(scope) = &job.scope else {
        return Err(DeliveryExecutionError::InvalidEffect(
            "execution job must use WorkRun scope".to_owned(),
        ));
    };
    for (field, value, prefix) in [
        ("workRunId", scope.work_run_id.0.as_str(), "wrn"),
        ("workItemId", scope.work_item_id.0.as_str(), "wit"),
        ("workContractId", scope.work_contract_id.0.as_str(), "wct"),
        (
            "productSessionId",
            scope.product_session_id.0.as_str(),
            "psn",
        ),
    ] {
        if !canonical_identifier(value, prefix) {
            return Err(invalid_execution_value(field));
        }
    }
    if scope.work_contract_revision.0 < 1 || scope.work_item_revision.0 < 1 {
        return Err(invalid_execution_value("WorkRun revision"));
    }
    if scope.kind != WorkRunExecutionScopeKind::WorkRun
        || scope.attempt != job.attempt
        || job.attempt < 1
    {
        return Err(DeliveryExecutionError::InvalidEffect(
            "WorkRun scope attempt does not match the job".to_owned(),
        ));
    }
    let input = job.work_input.as_ref().ok_or_else(|| {
        DeliveryExecutionError::InvalidEffect(
            "WorkRun execution job requires canonical workInput".to_owned(),
        )
    })?;
    if input.delivery_spec_id.trim().is_empty()
        || input.delivery_spec_id.len() > 256
        || input.delivery_spec_revision.0 <= 0
        || job.goal != input.work_item.goal
        || input.work_item.id != scope.work_item_id
        || input.work_item.revision != scope.work_item_revision
        || input.work_item.work_contract_id != scope.work_contract_id
        || input.work_item.work_contract_revision != scope.work_contract_revision
        || input.work_contract.id != scope.work_contract_id
        || input.work_contract.revision != scope.work_contract_revision
    {
        return Err(DeliveryExecutionError::InvalidEffect(
            "WorkRun input identity or revision differs from scope".to_owned(),
        ));
    }
    if !workrun_role_write_mode_valid(job) {
        return Err(DeliveryExecutionError::InvalidEffect(
            "WorkRun execution profile and workspace write mode are incompatible".to_owned(),
        ));
    }
    if scope.rework_authorization.is_some() != (job.execution_profile == "remediator") {
        return Err(DeliveryExecutionError::InvalidEffect(
            "rework authorization requires remediator profile".to_owned(),
        ));
    }
    if let Some(authorization) = &scope.rework_authorization
        && (input.candidate_ref.as_deref() != Some(authorization.candidate_ref.as_str())
            || job.workspace.checkout_revision != authorization.source_candidate_commit_id
            || !authorization.requires_full_reverification
            || authorization.targets.is_empty()
            || !lowercase_sha256(&authorization.diff_sha256)
            || authorization.targets.iter().any(|target| {
                !lowercase_sha256(&target.source_hunk_sha256)
                    || target.file_path.is_empty()
                    || target.evidence_ref_ids.is_empty()
            })
            || !sha256_digest(&authorization.authorization_digest.0)
            || !authorization
                .candidate_ref
                .strip_prefix("git-candidate:sha256:")
                .is_some_and(lowercase_sha256))
    {
        return Err(DeliveryExecutionError::InvalidEffect(
            "WorkRun rework authorization digest or candidate is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn workrun_role_write_mode_valid(job: &ExecutionJob) -> bool {
    match job.execution_profile.as_str() {
        "executor" | "remediator" => matches!(
            job.workspace.write_mode,
            ExecutionWorkspaceWriteMode::Candidate | ExecutionWorkspaceWriteMode::ReadOnly
        ),
        "reviewer" | "verifier" | "adversarial-verifier" => {
            job.workspace.write_mode == ExecutionWorkspaceWriteMode::ReadOnly
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryExecutionConfig {
    pub payload_digest: Sha256Digest,
    /// Exact frozen source candidate for verification and authorized rework Jobs.
    pub candidate_ref: Option<String>,
    pub workspace: ExecutionWorkspace,
    pub limits: ExecutionLimits,
}

/// Delivery mutation and generated job waiting for one outer transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingDeliveryExecution {
    request_id: RequestId,
    work_run_transition: WorkRunStartResult,
    job: ExecutionJob,
}

impl PendingDeliveryExecution {
    pub fn from_workrun(
        request_id: RequestId,
        work_run_transition: WorkRunStartResult,
        job: ExecutionJob,
    ) -> Self {
        Self {
            request_id,
            work_run_transition,
            job,
        }
    }

    #[must_use]
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    #[must_use]
    pub fn delivery(&self) -> &Delivery {
        &self.work_run_transition.delivery
    }

    #[must_use]
    pub fn work_run_transition(&self) -> &WorkRunStartResult {
        &self.work_run_transition
    }

    #[must_use]
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
pub trait ExecutionJobDispatcher: Send {
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
    ProjectionPublicationAfterDispatch {
        commit: Box<DeliveryExecutionCommitReceipt>,
        source: DeliveryExecutionPortError,
    },
}

impl DeliveryExecutionError {
    #[must_use]
    pub fn committed_receipt(&self) -> Option<&DeliveryExecutionCommitReceipt> {
        match self {
            Self::CommittedPayloadInvalid { commit, .. }
            | Self::DispatchAfterCommit { commit, .. }
            | Self::AcknowledgeAfterDispatch { commit, .. }
            | Self::ProjectionPublicationAfterDispatch { commit, .. } => Some(commit.as_ref()),
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
            Self::ProjectionPublicationAfterDispatch { source, .. } => write!(
                formatter,
                "ExecutionJob dispatched, but the Delivery change event remains pending: {source}"
            ),
        }
    }
}

impl Error for DeliveryExecutionError {}

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
    validate_workrun_execution_job(&commit.job).map_err(|error| error.to_string())?;
    if commit.committed_revision != pending.delivery().revision() {
        return Err("durable receipt revision does not match the committed Delivery".to_owned());
    }
    if !bounded_length(commit.outbox_event_id.trim(), 1, 200) {
        return Err("durable receipt has an invalid outbox event identity".to_owned());
    }
    if !commit.replayed && commit.job != pending.job {
        return Err("new durable receipt does not contain the exact pending job".to_owned());
    }
    Ok(())
}

/// Acknowledges the exact leased cancellation and returns its Delivery transition.
///
/// # Errors
/// Rejects forged, stale or mismatched acknowledgements and lease identities.
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
        && &acknowledgement.lease.job_id == lease.execution_job_id()
        && lease_attempt == lease.attempt()
        && &acknowledgement.lease.lease_id == lease.lease_id()
        && &acknowledgement.lease.fencing_token == lease.fencing_token()
        && &acknowledgement.lease.worker_id == lease.worker_id()
        && &acknowledgement.lease.worker_instance_id == lease.worker_instance_id()
        && &acknowledgement.worker_session_id == lease.worker_session_id();
    if !exact {
        return Err(DeliveryExecutionError::InvalidEffect(
            "job.cancel_ack does not match the exact request and active lease".to_owned(),
        ));
    }
    acknowledge_cancel(
        delivery,
        intent,
        &CancelAcknowledgement {
            work_run_id: intent.work_run_id.clone(),
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

fn validate_job_fields(job: &ExecutionJob) -> Result<(), DeliveryExecutionError> {
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
            workrun_role_write_mode_valid(job),
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
    Ok(())
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64 && lowercase_hex(value)
}

fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn validate_cancel_ack(
    acknowledgement: &JobCancelAckMessage,
) -> Result<(), DeliveryExecutionError> {
    let lease = &acknowledgement.lease;
    let error_valid = acknowledgement
        .error
        .as_ref()
        .is_none_or(|error| bounded_length(&error.message, 1, 500));
    let valid = acknowledgement.kind == JobCancelAckMessageKind::JobCancelAck
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
            &acknowledgement.status,
            JobCancelAckMessageStatus::Accepted
                | JobCancelAckMessageStatus::AlreadyCancelling
                | JobCancelAckMessageStatus::AlreadyTerminal
                | JobCancelAckMessageStatus::RejectedExpiredLease
                | JobCancelAckMessageStatus::RejectedStaleFencingToken
                | JobCancelAckMessageStatus::RejectedWorkerInstance
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
    use winwincode_domain::{
        Criterion, CriterionId, WorkContract, WorkContractId, WorkItem, WorkItemId, WorkItemState,
        WorkRunId,
    };
    use winwincode_execution_port::generated::{
        ExecutionScope, WorkRunExecutionScope, WorkRunExecutionScopeKind, WorkRunInput,
    };

    fn workrun_job(role: &str) -> ExecutionJob {
        let contract_id = WorkContractId("wct_00000000000000000000000001".into());
        let item_id = WorkItemId("wit_00000000000000000000000001".into());
        let criterion_id = CriterionId("crt_00000000000000000000000001".into());
        let contract = WorkContract {
            constraints: vec!["boundary".into()],
            created_at: winwincode_domain::Instant("2026-08-28T00:00:00.000Z".into()),
            criteria: vec![Criterion {
                id: criterion_id.clone(),
                description: "verified".into(),
                required: true,
                required_evidence_class: "machine".into(),
                verification_method: None,
            }],
            id: contract_id.clone(),
            objective: "fixture".into(),
            protected_scope: vec!["src".into()],
            required_human_authority: "none".into(),
            revision: winwincode_domain::Revision(2),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: vec!["src".into()],
        };
        let item = WorkItem {
            criterion_ids: vec![criterion_id],
            depends_on: vec![],
            goal: "fixture".into(),
            id: item_id.clone(),
            revision: winwincode_domain::Revision(1),
            schema_version: SchemaVersion::WinwincodeV1,
            state: WorkItemState::Ready,
            title: "fixture".into(),
            work_contract_id: contract_id.clone(),
            work_contract_revision: winwincode_domain::Revision(2),
        };
        ExecutionJob {
            attempt: 1,
            execution_profile: role.into(),
            goal: "fixture".into(),
            job_id: winwincode_domain::ExecutionJobId("job_00000000000000000000000001".into()),
            limits: ExecutionLimits {
                deadline_at: winwincode_domain::Instant("2026-08-28T00:00:00.000Z".into()),
                max_artifact_bytes: 1_048_576,
                max_runtime_seconds: 300,
            },
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            scope: ExecutionScope::WorkRunExecutionScope(WorkRunExecutionScope {
                attempt: 1,
                kind: WorkRunExecutionScopeKind::WorkRun,
                product_session_id: winwincode_domain::ProductSessionId(
                    "psn_00000000000000000000000001".into(),
                ),
                rework_authorization: None,
                work_contract_id: contract_id.clone(),
                work_contract_revision: winwincode_domain::Revision(2),
                work_item_id: item_id,
                work_item_revision: winwincode_domain::Revision(1),
                work_run_id: WorkRunId("wrn_00000000000000000000000001".into()),
            }),
            work_input: Some(WorkRunInput {
                delivery_spec_id: "spec-fixture".into(),
                delivery_spec_revision: winwincode_domain::Revision(2),
                candidate_ref: None,
                schema_version: SchemaVersion::WinwincodeV1,
                work_contract: contract,
                work_item: item,
            }),
            workspace: ExecutionWorkspace {
                checkout_revision: "main".into(),
                repository_id: winwincode_domain::RepositoryId(
                    "rep_00000000000000000000000001".into(),
                ),
                write_mode: if role == "executor" {
                    ExecutionWorkspaceWriteMode::Candidate
                } else {
                    ExecutionWorkspaceWriteMode::ReadOnly
                },
            },
        }
    }

    #[test]
    fn workrun_positive() {
        validate_workrun_execution_job(&workrun_job("executor")).unwrap();
    }
    #[test]
    fn workrun_rejects_legacy_ids_and_invalid_limits_before_queue_publication() {
        let mut job = workrun_job("executor");
        if let ExecutionScope::WorkRunExecutionScope(scope) = &mut job.scope {
            scope.work_run_id.0 = "run_00000000000000000000000001".into();
        }
        assert!(validate_workrun_execution_job(&job).is_err());
        let mut job = workrun_job("executor");
        job.limits.max_runtime_seconds = 0;
        assert!(validate_workrun_execution_job(&job).is_err());
        let mut job = workrun_job("executor");
        job.payload_digest.0 = "unsealed".into();
        assert!(validate_workrun_execution_job(&job).is_err());
        let mut job = workrun_job("executor");
        job.goal = "a different task".into();
        assert!(validate_workrun_execution_job(&job).is_err());
        for profile in ["requirements", "solution"] {
            assert!(validate_workrun_execution_job(&workrun_job(profile)).is_err());
        }
    }

    #[test]
    fn workrun_missing_input_rejected() {
        let mut j = workrun_job("executor");
        j.work_input = None;
        assert!(validate_workrun_execution_job(&j).is_err());
    }
    #[test]
    fn workrun_wrong_identity_rejected() {
        let mut j = workrun_job("executor");
        if let ExecutionScope::WorkRunExecutionScope(s) = &mut j.scope {
            s.work_item_id.0.replace_range(4.., "x");
        }
        assert!(validate_workrun_execution_job(&j).is_err());
    }
    #[test]
    fn workrun_wrong_revision_rejected() {
        let mut j = workrun_job("executor");
        if let ExecutionScope::WorkRunExecutionScope(s) = &mut j.scope {
            s.work_item_revision = winwincode_domain::Revision(9);
        }
        assert!(validate_workrun_execution_job(&j).is_err());
    }
    #[test]
    fn workrun_wrong_attempt_rejected() {
        let mut j = workrun_job("executor");
        j.attempt = 2;
        assert!(validate_workrun_execution_job(&j).is_err());
    }
    #[test]
    fn workrun_readonly_role_cannot_write() {
        let mut j = workrun_job("verifier");
        j.workspace.write_mode = ExecutionWorkspaceWriteMode::Candidate;
        assert!(validate_workrun_execution_job(&j).is_err());
    }

    #[test]
    fn workrun_identity_is_scoped_beyond_request_id() {
        let request = RequestId("req_00000000000000000000000001".into());
        let first = deterministic_workrun_id(&request, "wct_first", "wit_first", "psn_first");
        let second = deterministic_workrun_id(&request, "wct_second", "wit_first", "psn_first");
        let third = deterministic_workrun_id(&request, "wct_first", "wit_first", "psn_second");
        assert_ne!(first, second);
        assert_ne!(first, third);
        assert_eq!(
            first,
            deterministic_workrun_id(&request, "wct_first", "wit_first", "psn_first")
        );
    }
}
