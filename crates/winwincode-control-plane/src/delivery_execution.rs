// SPDX-License-Identifier: Apache-2.0

//! Atomic Delivery-to-ExecutionPort dispatch composition.
//!
//! This module maps one Delivery-owned pending effect into generated public
//! types. It neither schedules Codex work nor treats an uncommitted effect as
//! HTTP success. The transaction adapter must commit Delivery journal state,
//! request receipt, and the immutable job outbox intent together before the
//! dispatcher is called.

use std::{error::Error, fmt};

use winwincode_api::generated::{
    DeliveryStageExecutionScope, ExecutionJob, ExecutionLimits, ExecutionScope,
    ExecutionWorkspace, JobCancelAckMessage,
};
use winwincode_delivery::{
    application::{
        stage::{
            acknowledge_cancel, ActiveLeaseIdentity, CancelAcknowledgement, CancelIntent,
            StageAdvanceEffect, StageAdvanceResult,
        },
        CoordinationError,
    },
    domain::Delivery,
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
    delivery: Delivery,
    job: ExecutionJob,
}

impl PendingDeliveryExecution {
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub fn delivery(&self) -> &Delivery {
        &self.delivery
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
}

/// ExecutionPort adapter called only after the outer transaction commits.
pub trait ExecutionJobDispatcher {
    /// Sends or offers one immutable generated ExecutionJob.
    ///
    /// # Errors
    ///
    /// Leaves the committed outbox intent pending for replay.
    fn dispatch(&mut self, job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryExecutionCommitReceipt {
    pub committed_revision: u64,
    pub replayed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryExecutionDispatchReceipt {
    pub commit: DeliveryExecutionCommitReceipt,
    pub dispatched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryExecutionError {
    InvalidEffect(String),
    Coordination(CoordinationError),
    Commit(DeliveryExecutionPortError),
    DispatchAfterCommit {
        commit: DeliveryExecutionCommitReceipt,
        source: DeliveryExecutionPortError,
    },
}

impl DeliveryExecutionError {
    pub const fn committed_receipt(&self) -> Option<&DeliveryExecutionCommitReceipt> {
        match self {
            Self::DispatchAfterCommit { commit, .. } => Some(commit),
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
            Self::DispatchAfterCommit { source, .. } => write!(
                formatter,
                "Delivery and job intent committed, but dispatch remains pending: {source}"
            ),
        }
    }
}

impl Error for DeliveryExecutionError {}

/// Converts one Delivery-owned dispatch effect to the generated ExecutionJob.
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
    let StageAdvanceEffect::Dispatch(intent) = result.effect else {
        return Err(DeliveryExecutionError::InvalidEffect(
            "only a newly committed Codex stage creates an ExecutionJob intent".to_owned(),
        ));
    };
    let attempt = i64::try_from(intent.attempt).map_err(|_| {
        DeliveryExecutionError::InvalidEffect(
            "Delivery stage attempt exceeds the ExecutionPort range".to_owned(),
        )
    })?;
    let job = ExecutionJob {
        attempt,
        execution_profile: intent.role,
        goal: intent.goal,
        job_id: intent.execution_job_id,
        limits: config.limits,
        payload_digest: config.payload_digest,
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id: intent.delivery_id,
            delivery_task_id: intent.delivery_task_id,
            kind: "delivery-stage".to_owned(),
            product_session_id: intent.product_session_id,
            stage_run_id: intent.stage_run_id,
        }),
        workspace: config.workspace,
    };
    validate_request_id(&request_id)?;
    validate_execution_job(&job)?;
    Ok(PendingDeliveryExecution {
        request_id,
        delivery: result.delivery,
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
    if commit.replayed {
        return Ok(DeliveryExecutionDispatchReceipt {
            commit,
            dispatched: false,
        });
    }
    dispatcher
        .dispatch(pending.job())
        .map_err(|source| DeliveryExecutionError::DispatchAfterCommit { commit, source })?;
    Ok(DeliveryExecutionDispatchReceipt {
        commit,
        dispatched: true,
    })
}

/// Accepts a generated `job.cancel_ack` without treating it as terminal.
///
/// The acknowledgement must match the exact cancellation request and current
/// lease. The returned Delivery remains unchanged; only a separately verified
/// terminal `job.outcome` may settle the StageRun.
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
        CancelAcknowledgement {
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
    let scope_valid = match &job.scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => {
            scope.kind == "delivery-stage"
                && canonical_identifier(&scope.product_session_id.0, "psn")
                && canonical_identifier(&scope.delivery_id.0, "dlv")
                && scope
                    .delivery_task_id
                    .as_ref()
                    .is_none_or(|id| canonical_identifier(&id.0, "dtk"))
                && canonical_identifier(&scope.stage_run_id.0, "run")
        }
        ExecutionScope::ProductSessionExecutionScope(_) => false,
    };
    let valid = canonical_identifier(&job.job_id.0, "job")
        && (1..=1_000).contains(&job.attempt)
        && sha256_digest(&job.payload_digest.0)
        && scope_valid
        && canonical_identifier(&job.workspace.repository_id.0, "rep")
        && bounded_length(&job.workspace.checkout_revision, 1, 200)
        && job.workspace.write_mode == "candidate"
        && bounded_length(&job.execution_profile, 1, 100)
        && bounded_length(&job.goal, 1, 20_000)
        && instant(&job.limits.deadline_at.0)
        && (1..=604_800).contains(&job.limits.max_runtime_seconds)
        && (0..=1_099_511_627_776).contains(&job.limits.max_artifact_bytes);
    if valid {
        Ok(())
    } else {
        return Err(DeliveryExecutionError::InvalidEffect(
            "generated ExecutionJob is invalid under the canonical ExecutionPort schema"
                .to_owned(),
        ));
    }
}

fn validate_cancel_ack(
    acknowledgement: &JobCancelAckMessage,
) -> Result<(), DeliveryExecutionError> {
    let lease = &acknowledgement.lease;
    let error_valid = acknowledgement.error.as_ref().is_none_or(|error| {
        bounded_length(&error.message, 1, 500)
    });
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
    value
        .strip_prefix("sha256:")
        .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')))
}

fn fencing_token(value: &str) -> bool {
    (1..=20).contains(&value.len())
        && value.as_bytes().first().is_some_and(|byte| matches!(byte, b'1'..=b'9'))
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
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) && !byte.is_ascii_digit())
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
