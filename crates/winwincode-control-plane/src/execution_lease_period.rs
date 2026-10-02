// SPDX-License-Identifier: Apache-2.0

//! Shared claim/renewal proof for immutable pending Worker frames.

use winwincode_delivery::application::workrun_execution::SessionBindingAuthority;
use winwincode_domain::Sha256Digest;
use winwincode_execution_port::generated::ExecutionLeaseStamp;
use winwincode_storage::{ExecutionLeaseRecord, ProductStateStorage, StorageError};

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
