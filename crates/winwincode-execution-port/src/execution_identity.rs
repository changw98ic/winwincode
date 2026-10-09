// SPDX-License-Identifier: Apache-2.0

//! Canonical execution identities shared by the scheduler, Worker, and Codex adapter.

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, FencingToken, Instant, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkerSessionId,
};

use crate::generated::{ExecutionLeaseStamp, JobDispatchMessage, LeaseRenewMessage};

/// Secret-free failure to encode one canonical execution identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionIdentityError;

impl fmt::Display for ExecutionIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("canonical execution identity is invalid")
    }
}

impl std::error::Error for ExecutionIdentityError {}

/// Derives the canonical Codex thread for one execution run.
///
/// # Errors
///
/// Returns an opaque error when the identity facts cannot be encoded.
pub fn canonical_codex_thread_id(
    job_id: &ExecutionJobId,
    attempt: i64,
    fencing_token: &FencingToken,
    payload_digest: &Sha256Digest,
) -> Result<CodexThreadId, ExecutionIdentityError> {
    let digest = Sha256::digest(canonical_run_bytes(
        job_id,
        attempt,
        fencing_token,
        payload_digest,
    )?);
    Ok(CodexThreadId(format!(
        "cdx_{}",
        &format!("{digest:x}")[..26].to_ascii_uppercase()
    )))
}

/// Derives the canonical digest for one execution run.
///
/// # Errors
///
/// Returns an opaque error when the identity facts cannot be encoded.
pub fn canonical_execution_run_digest(
    job_id: &ExecutionJobId,
    attempt: i64,
    fencing_token: &FencingToken,
    payload_digest: &Sha256Digest,
) -> Result<Sha256Digest, ExecutionIdentityError> {
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(canonical_run_bytes(
            job_id,
            attempt,
            fencing_token,
            payload_digest,
        )?)
    )))
}

/// Derives the Worker session and Codex thread identities for one sealed dispatch.
///
/// # Errors
///
/// Returns an opaque error when the identity facts cannot be encoded.
pub fn canonical_dispatch_session_identity(
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
    dispatch: &JobDispatchMessage,
) -> Result<(WorkerSessionId, CodexThreadId), ExecutionIdentityError> {
    let codex_thread_id = canonical_codex_thread_id(
        &dispatch.job.job_id,
        dispatch.job.attempt,
        &dispatch.lease.fencing_token,
        &dispatch.job.payload_digest,
    )?;
    let canonical = serde_json::to_vec(&(
        worker_id,
        worker_instance_id,
        &dispatch.lease,
        &dispatch.job.job_id,
        dispatch.job.attempt,
        &dispatch.lease.fencing_token,
        &dispatch.job.payload_digest,
    ))
    .map_err(|_| ExecutionIdentityError)?;
    let digest = format!("{:x}", Sha256::digest(canonical));
    let worker_session_id = WorkerSessionId(format!("wsn_{}", &digest[..26].to_ascii_uppercase()));
    Ok((worker_session_id, codex_thread_id))
}

fn canonical_run_bytes(
    job_id: &ExecutionJobId,
    attempt: i64,
    fencing_token: &FencingToken,
    payload_digest: &Sha256Digest,
) -> Result<Vec<u8>, ExecutionIdentityError> {
    serde_json::to_vec(&(job_id, attempt, fencing_token, payload_digest))
        .map_err(|_| ExecutionIdentityError)
}

/// Checks canonical UTC timestamps used by execution authority comparisons.
#[must_use]
pub fn canonical_instant(instant: &Instant) -> bool {
    let value = instant.0.as_bytes();
    value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
        && number(value, 5, 7).is_some_and(|month| (1..=12).contains(&month))
        && number(value, 8, 10).is_some_and(|day| (1..=31).contains(&day))
        && number(value, 11, 13).is_some_and(|hour| hour <= 23)
        && number(value, 14, 16).is_some_and(|minute| minute <= 59)
        && number(value, 17, 19).is_some_and(|second| second <= 59)
}

fn number(value: &[u8], start: usize, end: usize) -> Option<u8> {
    value
        .get(start..end)?
        .iter()
        .try_fold(0_u8, |number, byte| {
            number.checked_mul(10)?.checked_add(byte - b'0')
        })
}

/// Compares a retained stamp with independently authenticated current authority.
/// Only the expiry may advance; this comparison does not authenticate a renewal.
#[must_use]
pub fn retained_lease_matches_current(
    retained: &ExecutionLeaseStamp,
    current: &ExecutionLeaseStamp,
) -> bool {
    let mut expected = retained.clone();
    expected.expires_at = current.expires_at.clone();
    [
        &retained.issued_at,
        &retained.expires_at,
        &current.expires_at,
    ]
    .into_iter()
    .all(canonical_instant)
        && retained.issued_at.0 < retained.expires_at.0
        && retained.expires_at.0 <= current.expires_at.0
        && expected == *current
}

/// Validates a live same-attempt lease extension or its unchanged replay.
#[must_use]
pub fn valid_lease_renewal(
    current: &ExecutionLeaseStamp,
    renewal: &LeaseRenewMessage,
    now: &Instant,
) -> bool {
    lease_renewal_rejection(current, renewal, now).is_none()
}

/// Identifies the first rejected renewal predicate without exposing identity values.
/// The boolean validator and diagnostics share this single set of checks.
#[must_use]
pub fn lease_renewal_rejection(
    current: &ExecutionLeaseStamp,
    renewal: &LeaseRenewMessage,
    now: &Instant,
) -> Option<&'static str> {
    if ![
        &current.issued_at,
        &current.expires_at,
        &renewal.prior_expires_at,
        &renewal.lease.expires_at,
        &renewal.sent_at,
        now,
    ]
    .into_iter()
    .all(canonical_instant)
    {
        return Some("noncanonical_time");
    }
    let mut expected = current.clone();
    expected.expires_at = renewal.lease.expires_at.clone();
    if expected != renewal.lease {
        return Some("authority_mismatch");
    }
    if current.issued_at.0 > renewal.sent_at.0 {
        return Some("sent_before_issued");
    }
    if renewal.sent_at.0 > now.0 {
        return Some("sent_in_future");
    }
    if renewal.sent_at.0 >= renewal.prior_expires_at.0 {
        return Some("sent_after_prior_expiry");
    }
    if renewal.prior_expires_at.0 >= renewal.lease.expires_at.0 {
        return Some("nonextending_expiry");
    }
    if now.0 >= current.expires_at.0 {
        return Some("consumed_after_expiry");
    }
    if current.expires_at != renewal.prior_expires_at && *current != renewal.lease {
        return Some("prior_expiry_mismatch");
    }
    None
}
