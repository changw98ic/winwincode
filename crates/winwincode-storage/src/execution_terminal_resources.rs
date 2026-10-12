// SPDX-License-Identifier: Apache-2.0

//! Original execution resources frozen by the common terminal acceptance.

use rusqlite::Connection;
use winwincode_domain::ExecutionJobId;

use crate::{
    ExecutionQueueScope, StorageError, WorkerPoolId, WorkerSlotAuthority, execution_admission,
    execution_registry, repository_scheduler, worker_session_slots,
};

/// Read-only cleanup attribution from the canonical reservation and Worker slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTerminalResources {
    pub scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub authority: WorkerSlotAuthority,
}

pub(crate) fn load(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionTerminalResources>, StorageError> {
    let Some(reservation) =
        execution_admission::load_reservation_by_job_in_transaction(connection, job_id)
            .map_err(|error| StorageError::adapter(error.to_string()))?
    else {
        return Ok(None);
    };
    let dispatch = execution_registry::load_dispatch_authority_in_transaction(connection, job_id)?
        .ok_or_else(|| {
            StorageError::invalid_input("terminal resources have no accepted dispatch")
        })?;
    let job = repository_scheduler::load_execution_job_by_id(connection, job_id)?
        .ok_or_else(|| StorageError::invalid_input("terminal resources have no queue Job"))?;
    let slot =
        worker_session_slots::load_slot_in_transaction(connection, dispatch.worker_session_id())
            .map_err(|error| StorageError::adapter(error.to_string()))?
            .ok_or_else(|| StorageError::invalid_input("terminal resources have no Worker slot"))?;
    let lease = dispatch.lease();
    let authority = &slot.authority;
    if reservation.scope != job.scope
        || reservation.job_id != job.job_id
        || job.job_id != lease.job_id
        || job.attempt != lease.attempt
        || job.payload_digest != lease.payload_digest
        || authority.job_id != lease.job_id
        || authority.worker_id != lease.worker_id
        || authority.worker_instance_id != lease.worker_instance_id
        || authority.worker_session_id != *dispatch.worker_session_id()
        || authority.lease_id != lease.lease_id
        || authority.attempt != lease.attempt
        || authority.fencing_token != lease.fencing_token
    {
        return Err(StorageError::invalid_input(
            "terminal resources differ from their queue and accepted dispatch",
        ));
    }
    Ok(Some(ExecutionTerminalResources {
        scope: reservation.scope,
        worker_pool_id: reservation.worker_pool_id,
        authority: slot.authority,
    }))
}
