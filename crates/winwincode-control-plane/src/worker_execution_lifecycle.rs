// SPDX-License-Identifier: Apache-2.0

//! Authenticated Worker claim lifecycle over the canonical local stores.

use std::{fmt, path::Path};

use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, RequestId, WorkerSessionId};
use winwincode_execution_port::generated::{ExecutionOutcomeStatus, JobOutcomeMessage};
use winwincode_storage::{
    ExecutionAdmissionReceipt, ExecutionJobRecord, ExecutionJobState, ExecutionLeaseClaim,
    ExecutionLeaseReceipt, ExecutionReservationRecord, ExecutionReservationRelease,
    ExecutionReservationReleaseReason, ExecutionReservationSettlement, ExecutionReservationState,
    LeaseWriteStatus, SqliteStorage, StorageError, WorkerRegistryScope, WorkerSlotState,
};

/// Stable production Worker lifecycle failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerExecutionLifecycleErrorKind {
    Authority,
    OperationalAdmission,
    Storage,
}

/// Secret-free production Worker lifecycle error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerExecutionLifecycleError {
    kind: WorkerExecutionLifecycleErrorKind,
}

impl WorkerExecutionLifecycleError {
    const fn new(kind: WorkerExecutionLifecycleErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(&self) -> WorkerExecutionLifecycleErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerExecutionLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Worker execution lifecycle operation failed")
    }
}

impl std::error::Error for WorkerExecutionLifecycleError {}

impl From<StorageError> for WorkerExecutionLifecycleError {
    fn from(_error: StorageError) -> Self {
        Self::new(WorkerExecutionLifecycleErrorKind::Storage)
    }
}

/// Local production composition for authenticated Worker claims.
pub struct DurableWorkerExecutionLifecycle {
    storage: SqliteStorage,
}

impl DurableWorkerExecutionLifecycle {
    /// Opens and verifies the canonical queue, admission, and Registry stores.
    ///
    /// # Errors
    ///
    /// Returns a bounded storage failure when the local database is unavailable.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, WorkerExecutionLifecycleError> {
        let mut storage = SqliteStorage::open(data_directory).map_err(storage_error)?;
        storage.execution_queue().map_err(storage_error)?;
        storage.execution_admission().map_err(admission_error)?;
        storage.execution_registry().map_err(storage_error)?;
        Ok(Self { storage })
    }

    /// Claims a durable Job after joining its queue, admission, Worker, and
    /// transport-authenticated placement authority.
    ///
    /// # Errors
    ///
    /// Fails closed for missing or changed authority, unavailable stores, or
    /// Registry rejection. Registry rejection also releases local admission.
    pub fn claim(
        &mut self,
        claim: &ExecutionLeaseClaim,
    ) -> Result<ExecutionLeaseReceipt, WorkerExecutionLifecycleError> {
        let admission = self
            .storage
            .execution_admission()
            .map_err(admission_error)?
            .load_reservation_by_job(&claim.job_id)
            .map_err(admission_error)?
            .ok_or_else(authority_error)?;
        let job = self
            .storage
            .execution_queue()
            .map_err(storage_error)?
            .load_job(&admission.scope, &claim.job_id)
            .map_err(storage_error)?
            .ok_or_else(authority_error)?;
        let registry = self.storage.execution_registry().map_err(storage_error)?;
        let placement = registry
            .load_authenticated_worker_placement(&claim.worker_id, &claim.worker_instance_id)
            .map_err(storage_error)?
            .ok_or_else(authority_error)?;
        let worker = registry
            .load_worker(&claim.worker_id)
            .map_err(storage_error)?
            .ok_or_else(authority_error)?;

        if job.job_id != admission.job_id
            || job.job_id != claim.job_id
            || job.payload_digest != claim.payload_digest
            || job.attempt != claim.attempt
            || placement.worker_id != claim.worker_id
            || placement.worker_instance_id != claim.worker_instance_id
            || admission.worker_pool_id != placement.worker_pool_id
            || worker.worker_instance_id != placement.worker_instance_id
            || worker.management_scope != placement.management_scope
            || worker.authentication_identity != placement.authentication_identity
            || !placement_scope_contains(&placement.management_scope, &job)
            || matches!(
                job.state,
                ExecutionJobState::Completed | ExecutionJobState::Failed
            )
            || !matches!(
                admission.state,
                ExecutionReservationState::Queued | ExecutionReservationState::Running
            )
        {
            return Err(authority_error());
        }

        let receipt = self
            .storage
            .execution_registry()
            .map_err(storage_error)?
            .claim_execution_job_with_authenticated_placement(claim)
            .map_err(storage_error)?;
        if matches!(
            receipt.status,
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
        ) && receipt.lease.is_some()
        {
            return Ok(receipt);
        }

        self.release_admission_after_claim_failure(&admission, claim)?;
        Err(WorkerExecutionLifecycleError::new(
            WorkerExecutionLifecycleErrorKind::OperationalAdmission,
        ))
    }

    /// Settles recorded usage for an authenticated terminal Worker outcome.
    /// Embedded executions have no authenticated placement and return `None`.
    ///
    /// # Errors
    ///
    /// Missing totals retain unknown accounting. Rejects inconsistent Usage,
    /// changed placement, or stale session authority.
    #[allow(clippy::too_many_lines)]
    pub fn settle_terminal_outcome(
        &mut self,
        message: &JobOutcomeMessage,
    ) -> Result<Option<ExecutionAdmissionReceipt>, WorkerExecutionLifecycleError> {
        if !self.matches_authenticated_placement(message)? {
            return Ok(None);
        }
        let trusted = self
            .storage
            .execution_admission()
            .map_err(admission_error)?
            .load_reservation_by_job(&message.lease.job_id)
            .map_err(admission_error)?
            .ok_or_else(authority_error)?;
        let mut usage = if let Some(usage) = &message.outcome.usage {
            usage.clone()
        } else {
            winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(
                trusted_execution_runtime_millis(&trusted, &message.outcome.finished_at)?,
                0,
            )
        };
        // Verify terminal session authority before retaining any usage fact.
        require_terminal_authority(
            &mut self.storage,
            &WorkerUsageSettlement {
                outcome_status: message.outcome.status.clone(),
                job_id: message.lease.job_id.clone(),
                worker_session_id: message.worker_session_id.clone(),
                request_id: stable_request_id("terminal-authority", &message.message_id.0),
                actual_tokens: u64::try_from(usage.known_tokens).map_err(|_| authority_error())?,
                actual_cost_microunits: usage
                    .cost_microunits
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| authority_error())?,
                actual_runtime_millis: u64::try_from(usage.runtime_millis)
                    .map_err(|_| authority_error())?,
                completed_at: message.outcome.finished_at.clone(),
            },
        )?;
        usage = self
            .storage
            .combine_execution_attempt_usage(&message.lease, &usage)
            .map_err(|_| authority_error())?;
        if self
            .storage
            .execution_has_unknown_predecessor(&message.lease.job_id)
            .map_err(|_| authority_error())?
        {
            usage.runtime_millis =
                trusted_execution_runtime_millis(&trusted, &message.outcome.finished_at)?;
        }
        if !winwincode_execution_port::usage::valid_usage(&usage) {
            return Err(authority_error());
        }
        if trusted.state == ExecutionReservationState::Settled {
            // Signed physical accounting can later refine the financial view.
            // Cleanup still replays the immutable original settlement receipt.
            return self.replay_settled_terminal(message).map(Some);
        }
        if trusted.state == ExecutionReservationState::Released
            && trusted.release_reason == Some(ExecutionReservationReleaseReason::Cancelled)
            && crate::execution_outcome_commit::is_cancellation_cleanup(&self.storage, message)
                .map_err(storage_error)?
        {
            return retain_cancelled_terminal_accounting(
                &mut self.storage,
                message,
                &trusted,
                &usage,
            )
            .map(Some);
        }
        let unknown_request_id = stable_request_id("terminal-unknown", &message.message_id.0);
        let retained_unknown = if trusted.state == ExecutionReservationState::Released {
            self.storage
                .execution_admission()
                .map_err(admission_error)?
                .load_retained_terminal_usage(&trusted.job_id, &unknown_request_id)
                .map_err(admission_error)?
        } else {
            None
        };
        if usage.tokens.is_none() || retained_unknown.is_some() {
            let original_usage = retained_unknown.unwrap_or_else(|| usage.clone());
            let current = require_terminal_authority(
                &mut self.storage,
                &WorkerUsageSettlement {
                    outcome_status: message.outcome.status.clone(),
                    job_id: message.lease.job_id.clone(),
                    worker_session_id: message.worker_session_id.clone(),
                    request_id: unknown_request_id.clone(),
                    actual_tokens: u64::try_from(original_usage.known_tokens)
                        .map_err(|_| authority_error())?,
                    actual_cost_microunits: original_usage
                        .cost_microunits
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| authority_error())?,
                    actual_runtime_millis: u64::try_from(original_usage.runtime_millis)
                        .map_err(|_| authority_error())?,
                    completed_at: message.outcome.finished_at.clone(),
                },
            )?;
            let expected_revision = if current.state == ExecutionReservationState::Released {
                current
                    .revision
                    .checked_sub(1)
                    .ok_or_else(authority_error)?
            } else {
                current.revision
            };
            let receipt = self
                .storage
                .execution_admission()
                .map_err(admission_error)?
                .release_with_unknown_usage(
                    &ExecutionReservationRelease {
                        scope: current.scope,
                        worker_pool_id: current.worker_pool_id,
                        job_id: current.job_id,
                        request_id: unknown_request_id.clone(),
                        expected_revision,
                        reason: if message.outcome.status == ExecutionOutcomeStatus::Cancelled {
                            ExecutionReservationReleaseReason::Cancelled
                        } else {
                            ExecutionReservationReleaseReason::Failed
                        },
                        released_at: message.outcome.finished_at.clone(),
                    },
                    &original_usage,
                )
                .map_err(admission_error)?;
            settle_complete_terminal_reconciliation(
                &mut self.storage,
                message,
                &trusted.job_id,
                &unknown_request_id,
                "terminal-unknown-reconciliation",
                &original_usage,
                &usage,
            )?;
            return Ok(Some(receipt));
        }
        self.settle_usage(&WorkerUsageSettlement {
            outcome_status: message.outcome.status.clone(),
            job_id: message.lease.job_id.clone(),
            worker_session_id: message.worker_session_id.clone(),
            request_id: stable_request_id("terminal-settle", &message.message_id.0),
            actual_tokens: u64::try_from(usage.tokens.ok_or_else(authority_error)?)
                .map_err(|_| authority_error())?,
            actual_cost_microunits: usage
                .cost_microunits
                .map(u64::try_from)
                .transpose()
                .map_err(|_| authority_error())?,
            actual_runtime_millis: u64::try_from(usage.runtime_millis)
                .map_err(|_| authority_error())?,
            completed_at: message.outcome.finished_at.clone(),
        })
        .map(Some)
    }

    /// Settles known usage or releases admission for an authenticated failed
    /// or cancelled Worker outcome. Embedded executions return `None`.
    ///
    /// # Errors
    ///
    /// Rejects successful outcomes, changed placement, changed replay, or
    /// unavailable durable state.
    pub fn release_terminal_outcome(
        &mut self,
        message: &JobOutcomeMessage,
    ) -> Result<Option<ExecutionAdmissionReceipt>, WorkerExecutionLifecycleError> {
        let reason = match message.outcome.status {
            ExecutionOutcomeStatus::Cancelled => ExecutionReservationReleaseReason::Cancelled,
            ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError => {
                ExecutionReservationReleaseReason::Failed
            }
            ExecutionOutcomeStatus::Succeeded => return Err(authority_error()),
        };
        let _ = reason;
        self.settle_terminal_outcome(message)
    }

    fn matches_authenticated_placement(
        &mut self,
        message: &JobOutcomeMessage,
    ) -> Result<bool, WorkerExecutionLifecycleError> {
        let placement = self
            .storage
            .execution_registry()
            .map_err(storage_error)?
            .load_lease_placement(&message.lease.job_id)
            .map_err(storage_error)?;
        let Some(placement) = placement else {
            return Ok(false);
        };
        if placement.worker_id != message.lease.worker_id
            || placement.worker_instance_id != message.lease.worker_instance_id
        {
            return Err(authority_error());
        }
        Ok(true)
    }

    fn settle_usage(
        &mut self,
        command: &WorkerUsageSettlement,
    ) -> Result<ExecutionAdmissionReceipt, WorkerExecutionLifecycleError> {
        let current = require_terminal_authority(&mut self.storage, command)?;
        match current.state {
            ExecutionReservationState::Running => self
                .storage
                .execution_admission()
                .map_err(admission_error)?
                .settle(&ExecutionReservationSettlement {
                    scope: current.scope,
                    worker_pool_id: current.worker_pool_id,
                    job_id: current.job_id,
                    request_id: command.request_id.clone(),
                    expected_revision: current.revision,
                    actual_tokens: command.actual_tokens,
                    actual_cost_microunits: command.actual_cost_microunits,
                    actual_runtime_millis: command.actual_runtime_millis,
                    completed_at: command.completed_at.clone(),
                })
                .map_err(admission_error),
            ExecutionReservationState::Settled => {
                require_exact_settlement_replay(&mut self.storage, &current, command)?;
                Ok(ExecutionAdmissionReceipt {
                    reservation: current,
                    replayed: true,
                })
            }
            ExecutionReservationState::Queued | ExecutionReservationState::Released => {
                Err(authority_error())
            }
        }
    }

    fn replay_settled_terminal(
        &mut self,
        message: &JobOutcomeMessage,
    ) -> Result<ExecutionAdmissionReceipt, WorkerExecutionLifecycleError> {
        let source = self
            .storage
            .execution_admission()
            .map_err(admission_error)?
            .load_settlement_source(&message.lease.job_id)
            .map_err(admission_error)?
            .ok_or_else(authority_error)?;
        self.settle_usage(&WorkerUsageSettlement {
            outcome_status: message.outcome.status.clone(),
            job_id: message.lease.job_id.clone(),
            worker_session_id: message.worker_session_id.clone(),
            request_id: stable_request_id("terminal-settle", &message.message_id.0),
            actual_tokens: source.fact.actual_tokens,
            actual_cost_microunits: source.fact.actual_cost_microunits,
            actual_runtime_millis: source.fact.actual_runtime_millis,
            completed_at: message.outcome.finished_at.clone(),
        })
    }

    fn release_admission_after_claim_failure(
        &mut self,
        admission: &ExecutionReservationRecord,
        claim: &ExecutionLeaseClaim,
    ) -> Result<(), WorkerExecutionLifecycleError> {
        self.storage
            .execution_admission()
            .map_err(admission_error)?
            .release(&ExecutionReservationRelease {
                scope: admission.scope.clone(),
                worker_pool_id: admission.worker_pool_id.clone(),
                job_id: admission.job_id.clone(),
                request_id: stable_request_id("operational-release", &claim.request_id.0),
                expected_revision: admission.revision,
                reason: ExecutionReservationReleaseReason::Failed,
                released_at: claim.issued_at.clone(),
            })
            .map_err(admission_error)?;
        Ok(())
    }
}

pub(crate) fn retain_cancelled_terminal_accounting(
    storage: &mut SqliteStorage,
    message: &JobOutcomeMessage,
    reservation: &ExecutionReservationRecord,
    usage: &winwincode_execution_port::generated::ExecutionOutcomeUsage,
) -> Result<ExecutionAdmissionReceipt, WorkerExecutionLifecycleError> {
    let terminal_request_id =
        stable_request_id("cancelled-terminal-accounting", &message.message_id.0);
    let retained_usage = storage
        .execution_admission()
        .map_err(admission_error)?
        .load_retained_terminal_usage(&reservation.job_id, &terminal_request_id)
        .map_err(admission_error)?
        .unwrap_or_else(|| usage.clone());
    let receipt = storage
        .execution_admission()
        .map_err(admission_error)?
        .retain_released_terminal_usage(
            &ExecutionReservationRelease {
                scope: reservation.scope.clone(),
                worker_pool_id: reservation.worker_pool_id.clone(),
                job_id: reservation.job_id.clone(),
                request_id: terminal_request_id.clone(),
                expected_revision: reservation.revision,
                reason: ExecutionReservationReleaseReason::Cancelled,
                released_at: message.outcome.finished_at.clone(),
            },
            &retained_usage,
        )
        .map_err(admission_error)?;
    settle_complete_terminal_reconciliation(
        storage,
        message,
        &reservation.job_id,
        &terminal_request_id,
        "cancelled-terminal-reconciliation",
        &retained_usage,
        usage,
    )?;
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
fn settle_complete_terminal_reconciliation(
    storage: &mut SqliteStorage,
    message: &JobOutcomeMessage,
    job_id: &ExecutionJobId,
    terminal_request_id: &RequestId,
    settlement_namespace: &str,
    retained_usage: &winwincode_execution_port::generated::ExecutionOutcomeUsage,
    current_usage: &winwincode_execution_port::generated::ExecutionOutcomeUsage,
) -> Result<(), WorkerExecutionLifecycleError> {
    if current_usage.tokens.is_some() && current_usage.cost_microunits.is_some() {
        let mut reconciled_usage = current_usage.clone();
        reconciled_usage.runtime_millis = retained_usage.runtime_millis;
        storage
            .execution_admission()
            .map_err(admission_error)?
            .settle_reconciliation(
                job_id,
                terminal_request_id,
                &stable_request_id(settlement_namespace, &message.message_id.0),
                &reconciled_usage,
                &message.outcome.finished_at,
            )
            .map_err(admission_error)?;
    }
    Ok(())
}

pub(crate) fn trusted_execution_runtime_millis(
    reservation: &ExecutionReservationRecord,
    finished_at: &Instant,
) -> Result<i64, WorkerExecutionLifecycleError> {
    let started = reservation
        .started_at
        .as_ref()
        .ok_or_else(authority_error)?;
    let start = crate::session_binding_transaction::instant_millis(started)
        .map_err(|_| authority_error())?;
    let finish = crate::session_binding_transaction::instant_millis(finished_at)
        .map_err(|_| authority_error())?;
    i64::try_from(finish.checked_sub(start).ok_or_else(authority_error)?)
        .map_err(|_| authority_error())
}

struct WorkerUsageSettlement {
    outcome_status: ExecutionOutcomeStatus,
    job_id: ExecutionJobId,
    worker_session_id: WorkerSessionId,
    request_id: RequestId,
    actual_tokens: u64,
    actual_cost_microunits: Option<u64>,
    actual_runtime_millis: u64,
    completed_at: Instant,
}

fn require_terminal_authority(
    storage: &mut SqliteStorage,
    command: &WorkerUsageSettlement,
) -> Result<ExecutionReservationRecord, WorkerExecutionLifecycleError> {
    let current = storage
        .execution_admission()
        .map_err(admission_error)?
        .load_reservation_by_job(&command.job_id)
        .map_err(admission_error)?
        .ok_or_else(authority_error)?;
    let registry = storage.execution_registry().map_err(storage_error)?;
    let lease = registry
        .load_lease(&command.job_id)
        .map_err(storage_error)?
        .ok_or_else(authority_error)?;
    let placement = registry
        .load_lease_placement(&command.job_id)
        .map_err(storage_error)?
        .ok_or_else(authority_error)?;
    if placement.worker_pool_id != current.worker_pool_id {
        return Err(authority_error());
    }
    let slot = storage
        .worker_session_slots()
        .map_err(|_| authority_error())?
        .load(&command.worker_session_id)
        .map_err(|_| authority_error())?
        .ok_or_else(authority_error)?;
    if slot.authority.job_id != command.job_id
        || slot.authority.lease_id != lease.lease_id
        || slot.authority.worker_id != lease.worker_id
        || slot.authority.worker_instance_id != lease.worker_instance_id
        || slot.authority.attempt != lease.attempt
        || slot.authority.fencing_token != lease.fencing_token
        || !match slot.state {
            WorkerSlotState::Running => true,
            WorkerSlotState::Cancelling | WorkerSlotState::Cancelled => {
                command.outcome_status == ExecutionOutcomeStatus::Cancelled
            }
            WorkerSlotState::Completed => {
                command.outcome_status == ExecutionOutcomeStatus::Succeeded
            }
            WorkerSlotState::Failed => matches!(
                command.outcome_status,
                ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError
            ),
            WorkerSlotState::RecoveryFailed => false,
        }
        || command.completed_at.0 < lease.issued_at.0
    {
        return Err(authority_error());
    }
    Ok(current)
}

fn require_exact_settlement_replay(
    storage: &mut SqliteStorage,
    current: &ExecutionReservationRecord,
    command: &WorkerUsageSettlement,
) -> Result<(), WorkerExecutionLifecycleError> {
    let source = storage
        .execution_admission()
        .map_err(admission_error)?
        .load_settlement_source(&command.job_id)
        .map_err(admission_error)?
        .ok_or_else(authority_error)?;
    if source.fact.settlement_request_id != command.request_id
        || source.fact.scope != current.scope
        || source.fact.worker_pool_id != current.worker_pool_id
        || source.fact.user_id != current.user_id
        || source.fact.actual_tokens != command.actual_tokens
        || source.fact.actual_cost_microunits != command.actual_cost_microunits
        || source.fact.actual_runtime_millis != command.actual_runtime_millis
        || source.fact.completed_at != command.completed_at
    {
        return Err(authority_error());
    }
    Ok(())
}

fn placement_scope_contains(placement: &WorkerRegistryScope, job: &ExecutionJobRecord) -> bool {
    match placement {
        WorkerRegistryScope::Organization { organization_id } => {
            organization_id == &job.scope.organization_id
        }
        WorkerRegistryScope::Workspace {
            organization_id,
            workspace_id,
        } => {
            organization_id == &job.scope.organization_id && workspace_id == &job.scope.workspace_id
        }
        WorkerRegistryScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            organization_id == &job.scope.organization_id
                && workspace_id == &job.scope.workspace_id
                && project_id == &job.scope.project_id
        }
        WorkerRegistryScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            organization_id == &job.scope.organization_id
                && workspace_id == &job.scope.workspace_id
                && project_id == &job.scope.project_id
                && repository_id == &job.scope.repository_id
        }
    }
}

fn stable_request_id(action: &str, identity: &str) -> RequestId {
    let digest = Sha256::digest(
        [
            b"winwincode.worker-lifecycle.v1\0".as_slice(),
            action.as_bytes(),
            b"\0".as_slice(),
            identity.as_bytes(),
        ]
        .concat(),
    );
    let mut value = u128::from_be_bytes(digest[..16].try_into().expect("digest prefix fits"));
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut suffix = [b'0'; 26];
    for byte in suffix.iter_mut().rev() {
        *byte = alphabet[usize::try_from(value & 31).expect("base32 digit fits usize")];
        value >>= 5;
    }
    RequestId(format!(
        "req_{}",
        std::str::from_utf8(&suffix).expect("Crockford alphabet is UTF-8")
    ))
}

fn authority_error() -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Authority)
}

fn admission_error<T>(_error: T) -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::OperationalAdmission)
}

fn storage_error(_error: StorageError) -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Storage)
}
