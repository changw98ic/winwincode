// SPDX-License-Identifier: Apache-2.0

//! Canonical execution identities shared by the scheduler, Worker, and Codex adapter.

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, FencingToken, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkerSessionId,
};

use crate::generated::JobDispatchMessage;

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
