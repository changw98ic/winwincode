// SPDX-License-Identifier: Apache-2.0

//! Shared claim/renewal proof for immutable pending Worker frames.

use winwincode_delivery::application::workrun_execution::SessionBindingAuthority;
use winwincode_domain::{Instant, Sha256Digest};
use winwincode_execution_port::generated::ExecutionLeaseStamp;
use winwincode_storage::{ExecutionLeaseRecord, ProductStateStorage, StorageError};

/// Checks a retained message timestamp without treating renewal as a new stream.
/// The caller separately checks current execution authority and trusted Server time.
pub(crate) fn retained_message_within_lease(
    storage: &mut dyn ProductStateStorage,
    lease: &ExecutionLeaseStamp,
    sent_at: &Instant,
) -> Result<bool, StorageError> {
    if sent_at.0 < lease.issued_at.0 {
        return Ok(false);
    }
    if sent_at.0 < lease.expires_at.0 {
        return Ok(true);
    }
    let job = match crate::delivery_transaction::load_durable_execution_job(storage, &lease.job_id)
    {
        Ok((_, job)) => job,
        Err(error) if error.kind() == winwincode_storage::StorageErrorKind::InvalidInput => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let Ok(attempt) = u64::try_from(lease.attempt) else {
        return Ok(false);
    };
    let period = ExecutionLeaseRecord {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        payload_digest: job.payload_digest,
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        attempt,
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        expires_at: lease.expires_at.clone(),
    };
    Ok(storage
        .load_accepted_execution_lease_for_period(&period)?
        .is_some_and(|current| sent_at.0 < current.expires_at.0))
}

pub(crate) fn accepted_lease_window_matches(
    storage: &mut dyn ProductStateStorage,
    lease: &ExecutionLeaseStamp,
    payload_digest: &Sha256Digest,
    authority: &SessionBindingAuthority,
) -> Result<bool, StorageError> {
    if authority.issued_at() != &lease.issued_at {
        return Ok(false);
    }
    if authority.expires_at() == &lease.expires_at {
        return Ok(true);
    }
    let Ok(attempt) = u64::try_from(lease.attempt) else {
        return Ok(false);
    };
    let period = ExecutionLeaseRecord {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        payload_digest: payload_digest.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        attempt,
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        expires_at: lease.expires_at.clone(),
    };
    let mut current = period.clone();
    current.expires_at.clone_from(authority.expires_at());
    Ok(storage
        .load_accepted_execution_lease_for_period(&period)?
        .as_ref()
        == Some(&current))
}
