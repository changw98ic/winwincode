// SPDX-License-Identifier: Apache-2.0

//! Production Worker enterprise-quota lifecycle over the canonical stores.
//!
//! Authenticated Worker placement is read from the Registry, tenant/user and
//! budget attribution are read from the execution admission receipt, and the
//! immutable scheduler Job is read from the queue. The adapter does not own a
//! second pool, slot counter, or execution outcome ledger.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use winwincode_domain::{ExecutionJobId, Instant, RequestId, WorkerSessionId};
use winwincode_execution_port::generated::{ExecutionOutcomeStatus, JobOutcomeMessage};
use winwincode_storage::{
    EnterpriseQuotaRelease, EnterpriseQuotaReleaseReason, EnterpriseQuotaReservationRecord,
    EnterpriseQuotaReservationState, EnterpriseQuotaTerminal, ExecutionAdmissionReceipt,
    ExecutionLeaseClaim, ExecutionLeaseReceipt, ExecutionReservationRecord,
    ExecutionReservationRelease, ExecutionReservationReleaseReason, ExecutionReservationSettlement,
    ExecutionReservationState, LeaseWriteStatus, SqliteStorage, StorageError, WorkerSlotState,
};

use crate::{
    DurableEnterpriseQuotaAdmission, DurableWorkerPolicyEnforcement,
    WorkerEnterpriseQuotaAuthority, WorkerEnterpriseQuotaAuthorityPort, WorkerEnterpriseQuotaClaim,
    WorkerEnterpriseQuotaError, WorkerEnterpriseQuotaErrorKind, WorkerEnterpriseQuotaSaga,
    WorkerEnterpriseUsageReconciler, WorkerOperationalClaimPort, WorkerPolicyErrorKind,
};

const WORKER_USAGE_PAGE_LIMIT: u64 = 200;

/// Stable production Worker lifecycle failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerExecutionLifecycleErrorKind {
    Authority,
    PolicyRejected,
    PolicyUnavailable,
    Quota,
    OperationalAdmission,
    Usage,
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

/// Trusted terminal Worker usage. Scope, user, pool, and reserved ceilings are
/// deliberately absent and are loaded from the durable admission receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerExecutionUsageSettlement {
    pub job_id: ExecutionJobId,
    pub worker_session_id: WorkerSessionId,
    pub request_id: RequestId,
    pub actual_tokens: u64,
    pub actual_cost_microunits: u64,
    pub actual_runtime_millis: u64,
    pub completed_at: Instant,
}

/// Failure/cancellation command for one already admitted Worker execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerExecutionRelease {
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub reason: ExecutionReservationReleaseReason,
    pub released_at: Instant,
}

/// Durable terminal result after operational and enterprise stores converge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerExecutionTerminalReceipt {
    pub operational: ExecutionAdmissionReceipt,
    pub enterprise: EnterpriseQuotaReservationRecord,
}

/// Local production composition. Each operation reopens the same canonical
/// database so a process restart follows exactly the same identity path.
pub struct DurableWorkerExecutionLifecycle {
    data_directory: PathBuf,
}

impl DurableWorkerExecutionLifecycle {
    /// Opens and verifies every canonical Worker/quota store.
    ///
    /// # Errors
    ///
    /// Returns a bounded storage failure when the local database is unavailable.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, WorkerExecutionLifecycleError> {
        let data_directory = data_directory.as_ref().to_path_buf();
        let mut storage = SqliteStorage::open(&data_directory).map_err(storage_error)?;
        storage.execution_queue().map_err(storage_error)?;
        storage.execution_admission().map_err(admission_error)?;
        storage.execution_registry().map_err(storage_error)?;
        storage.enterprise_quota_ledger().map_err(quota_error)?;
        storage.enterprise_usage_ledger().map_err(usage_error)?;
        drop(storage);
        Ok(Self { data_directory })
    }

    /// Reserves enterprise quota before the only authenticated Registry claim.
    ///
    /// Durable queue, operational admission, Worker registration, and pool
    /// placement facts are joined before quota is touched. Registry rejection
    /// releases both the enterprise reservation and operational admission.
    ///
    /// # Errors
    ///
    /// Fails closed for missing/changed authority, unavailable stores, quota
    /// failures, Registry rejection, or a failed compensating release.
    pub fn claim(
        &self,
        claim: &ExecutionLeaseClaim,
    ) -> Result<WorkerEnterpriseQuotaClaim<ExecutionLeaseReceipt>, WorkerExecutionLifecycleError>
    {
        let mut authority = DurableWorkerQuotaAuthoritySource::open(&self.data_directory)?;
        let policy_authority = authority
            .load(&claim.job_id, claim)
            .map_err(worker_quota_error)?
            .ok_or_else(authority_error)?;
        let mut policy =
            DurableWorkerPolicyEnforcement::open(&self.data_directory).map_err(|_| {
                WorkerExecutionLifecycleError::new(
                    WorkerExecutionLifecycleErrorKind::PolicyUnavailable,
                )
            })?;
        let policy_result = policy.enforce_placement(&policy_authority);
        policy.close().map_err(|_| {
            WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::PolicyUnavailable)
        })?;
        policy_result.map_err(|error| match error.kind() {
            WorkerPolicyErrorKind::Rejected => WorkerExecutionLifecycleError::new(
                WorkerExecutionLifecycleErrorKind::PolicyRejected,
            ),
            WorkerPolicyErrorKind::Unavailable => WorkerExecutionLifecycleError::new(
                WorkerExecutionLifecycleErrorKind::PolicyUnavailable,
            ),
        })?;
        let quota_storage = SqliteStorage::open(&self.data_directory).map_err(storage_error)?;
        let mut quota = DurableEnterpriseQuotaAdmission::new(quota_storage);
        let mut operational = DurableAuthenticatedRegistryClaim::open(&self.data_directory)?;
        let result = WorkerEnterpriseQuotaSaga::new(&mut quota).reserve_then_claim(
            &mut authority,
            &claim.job_id,
            claim,
            claim.issued_at.clone(),
            &mut operational,
        );
        let result = match result {
            Ok(WorkerEnterpriseQuotaClaim::Denied) => {
                self.release_operational_after_claim_failure(claim)?;
                Ok(WorkerEnterpriseQuotaClaim::Denied)
            }
            Err(error) if error.kind() == WorkerEnterpriseQuotaErrorKind::OperationalClaim => {
                self.release_operational_after_claim_failure(claim)?;
                Err(worker_quota_error(error))
            }
            Err(error) => Err(worker_quota_error(error)),
            Ok(value) => Ok(value),
        };
        quota.close().map_err(storage_error)?;
        result
    }

    /// Commits immutable actual Worker usage, projects it to enterprise Usage,
    /// and settles the matching enterprise reservation by its source seal.
    ///
    /// Exact retries after any crash window return the same operational and
    /// enterprise terminal records without adding another charge.
    ///
    /// # Errors
    ///
    /// Rejects stale lease/session/pool authority, usage above the reserved
    /// ceiling, changed terminal replay, or unavailable storage.
    pub fn settle_usage(
        &self,
        command: &WorkerExecutionUsageSettlement,
    ) -> Result<WorkerExecutionTerminalReceipt, WorkerExecutionLifecycleError> {
        let operational = self.commit_or_replay_usage(command)?;
        self.reconcile_worker_usage()?;
        let enterprise = self.load_enterprise_terminal(&command.job_id)?;
        if enterprise.state != EnterpriseQuotaReservationState::Settled {
            return Err(WorkerExecutionLifecycleError::new(
                WorkerExecutionLifecycleErrorKind::Quota,
            ));
        }
        Ok(WorkerExecutionTerminalReceipt {
            operational,
            enterprise,
        })
    }

    /// Releases operational and enterprise reservations after cancellation or
    /// failure. Exact retries after either write replay the same terminal facts.
    ///
    /// # Errors
    ///
    /// Rejects changed terminal reuse, already-settled work, stale authority,
    /// and unavailable storage.
    pub fn release(
        &self,
        command: &WorkerExecutionRelease,
    ) -> Result<WorkerExecutionTerminalReceipt, WorkerExecutionLifecycleError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(storage_error)?;
        let current = storage
            .execution_admission()
            .map_err(admission_error)?
            .load_reservation_by_job(&command.job_id)
            .map_err(admission_error)?
            .ok_or_else(authority_error)?;
        let expected_revision = match current.state {
            ExecutionReservationState::Queued | ExecutionReservationState::Running => {
                current.revision
            }
            ExecutionReservationState::Released => {
                current.revision.checked_sub(1).ok_or_else(|| {
                    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Storage)
                })?
            }
            ExecutionReservationState::Settled => return Err(authority_error()),
        };
        let operational = storage
            .execution_admission()
            .map_err(admission_error)?
            .release(&ExecutionReservationRelease {
                scope: current.scope.clone(),
                worker_pool_id: current.worker_pool_id.clone(),
                job_id: current.job_id.clone(),
                request_id: command.request_id.clone(),
                expected_revision,
                reason: command.reason,
                released_at: command.released_at.clone(),
            })
            .map_err(admission_error)?;
        let reservation_id = worker_quota_reservation_id(&command.job_id);
        let mut quota = storage.enterprise_quota_ledger().map_err(quota_error)?;
        let enterprise = quota
            .load_reservation(&reservation_id)
            .map_err(quota_error)?
            .ok_or_else(authority_error)?;
        let enterprise = match enterprise.state {
            EnterpriseQuotaReservationState::Active => {
                quota
                    .release(&EnterpriseQuotaRelease {
                        reservation_id,
                        request_id: command.request_id.clone(),
                        expected_revision: enterprise.revision,
                        reason: release_reason(command.reason),
                        released_at: command.released_at.clone(),
                    })
                    .map_err(quota_error)?
                    .record
            }
            EnterpriseQuotaReservationState::Released => {
                require_exact_enterprise_release(&enterprise, command)?;
                enterprise
            }
            EnterpriseQuotaReservationState::Settled => return Err(authority_error()),
        };
        drop(storage);
        Ok(WorkerExecutionTerminalReceipt {
            operational,
            enterprise,
        })
    }

    /// Releases an authenticated execution from the already validated,
    /// immutable Worker terminal outcome. Local embedded executions have no
    /// authenticated lease placement and are left unchanged.
    ///
    /// # Errors
    ///
    /// Rejects a successful outcome, a changed Worker/instance placement, a
    /// changed terminal replay, or unavailable durable state.
    pub fn release_terminal_outcome(
        &self,
        message: &JobOutcomeMessage,
    ) -> Result<Option<WorkerExecutionTerminalReceipt>, WorkerExecutionLifecycleError> {
        let reason = match message.outcome.status {
            ExecutionOutcomeStatus::Cancelled => ExecutionReservationReleaseReason::Cancelled,
            ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError => {
                ExecutionReservationReleaseReason::Failed
            }
            ExecutionOutcomeStatus::Succeeded => return Err(authority_error()),
        };
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(storage_error)?;
        let placement = storage
            .execution_registry()
            .map_err(storage_error)?
            .load_lease_placement(&message.lease.job_id)
            .map_err(storage_error)?;
        let Some(placement) = placement else {
            return Ok(None);
        };
        if placement.worker_id != message.lease.worker_id
            || placement.worker_instance_id != message.lease.worker_instance_id
        {
            return Err(authority_error());
        }
        drop(storage);
        self.release(&WorkerExecutionRelease {
            job_id: message.lease.job_id.clone(),
            request_id: stable_request_id("terminal-release", &message.message_id.0),
            reason,
            released_at: message.outcome.finished_at.clone(),
        })
        .map(Some)
    }

    /// Settles an authenticated execution from the immutable actual Usage
    /// carried by an already validated successful Worker terminal outcome.
    /// Local embedded executions have no authenticated lease placement and
    /// are left unchanged.
    ///
    /// # Errors
    ///
    /// Rejects a non-success outcome, invalid Usage integers, a changed
    /// Worker/instance placement, stale session authority, or unavailable
    /// durable state.
    pub fn settle_terminal_outcome(
        &self,
        message: &JobOutcomeMessage,
    ) -> Result<Option<WorkerExecutionTerminalReceipt>, WorkerExecutionLifecycleError> {
        if message.outcome.status != ExecutionOutcomeStatus::Succeeded {
            return Err(authority_error());
        }
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(storage_error)?;
        let placement = storage
            .execution_registry()
            .map_err(storage_error)?
            .load_lease_placement(&message.lease.job_id)
            .map_err(storage_error)?;
        let Some(placement) = placement else {
            return Ok(None);
        };
        if placement.worker_id != message.lease.worker_id
            || placement.worker_instance_id != message.lease.worker_instance_id
        {
            return Err(authority_error());
        }
        let usage = message.outcome.usage.as_ref().ok_or_else(authority_error)?;
        let actual_tokens = u64::try_from(usage.tokens).map_err(|_| authority_error())?;
        let actual_cost_microunits =
            u64::try_from(usage.cost_microunits).map_err(|_| authority_error())?;
        let actual_runtime_millis =
            u64::try_from(usage.runtime_millis).map_err(|_| authority_error())?;
        drop(storage);
        self.settle_usage(&WorkerExecutionUsageSettlement {
            job_id: message.lease.job_id.clone(),
            worker_session_id: message.worker_session_id.clone(),
            request_id: stable_request_id("terminal-settle", &message.message_id.0),
            actual_tokens,
            actual_cost_microunits,
            actual_runtime_millis,
            completed_at: message.outcome.finished_at.clone(),
        })
        .map(Some)
    }

    fn commit_or_replay_usage(
        &self,
        command: &WorkerExecutionUsageSettlement,
    ) -> Result<ExecutionAdmissionReceipt, WorkerExecutionLifecycleError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(storage_error)?;
        let current = require_terminal_authority(&mut storage, command)?;
        let receipt = match current.state {
            ExecutionReservationState::Running => storage
                .execution_admission()
                .map_err(admission_error)?
                .settle(&ExecutionReservationSettlement {
                    scope: current.scope.clone(),
                    worker_pool_id: current.worker_pool_id.clone(),
                    job_id: current.job_id.clone(),
                    request_id: command.request_id.clone(),
                    expected_revision: current.revision,
                    actual_tokens: command.actual_tokens,
                    actual_cost_microunits: command.actual_cost_microunits,
                    actual_runtime_millis: command.actual_runtime_millis,
                    completed_at: command.completed_at.clone(),
                })
                .map_err(admission_error)?,
            ExecutionReservationState::Settled => {
                require_exact_settlement_replay(&mut storage, &current, command)?;
                ExecutionAdmissionReceipt {
                    reservation: current,
                    replayed: true,
                }
            }
            ExecutionReservationState::Queued | ExecutionReservationState::Released => {
                return Err(authority_error());
            }
        };
        drop(storage);
        Ok(receipt)
    }

    fn reconcile_worker_usage(&self) -> Result<(), WorkerExecutionLifecycleError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(storage_error)?;
        let mut cursor = None;
        loop {
            let page = WorkerEnterpriseUsageReconciler::new(&mut storage)
                .reconcile_worker_page(cursor.as_ref(), WORKER_USAGE_PAGE_LIMIT)
                .map_err(|_| {
                    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Usage)
                })?;
            let Some(next) = page.next else {
                break;
            };
            cursor = Some(next);
        }
        drop(storage);
        Ok(())
    }

    fn load_enterprise_terminal(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<EnterpriseQuotaReservationRecord, WorkerExecutionLifecycleError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(storage_error)?;
        let record = storage
            .enterprise_quota_ledger()
            .map_err(quota_error)?
            .load_reservation(&worker_quota_reservation_id(job_id))
            .map_err(quota_error)?
            .ok_or_else(authority_error)?;
        drop(storage);
        Ok(record)
    }

    fn release_operational_after_claim_failure(
        &self,
        claim: &ExecutionLeaseClaim,
    ) -> Result<(), WorkerExecutionLifecycleError> {
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(storage_error)?;
        let current = storage
            .execution_admission()
            .map_err(admission_error)?
            .load_reservation_by_job(&claim.job_id)
            .map_err(admission_error)?
            .ok_or_else(authority_error)?;
        if matches!(
            current.state,
            ExecutionReservationState::Queued | ExecutionReservationState::Running
        ) {
            storage
                .execution_admission()
                .map_err(admission_error)?
                .release(&ExecutionReservationRelease {
                    scope: current.scope.clone(),
                    worker_pool_id: current.worker_pool_id.clone(),
                    job_id: current.job_id,
                    request_id: stable_request_id("operational-release", &claim.request_id.0),
                    expected_revision: current.revision,
                    reason: ExecutionReservationReleaseReason::Failed,
                    released_at: claim.issued_at.clone(),
                })
                .map_err(admission_error)?;
        }
        drop(storage);
        Ok(())
    }
}

struct DurableWorkerQuotaAuthoritySource {
    storage: SqliteStorage,
}

impl DurableWorkerQuotaAuthoritySource {
    fn open(data_directory: &Path) -> Result<Self, WorkerExecutionLifecycleError> {
        SqliteStorage::open(data_directory)
            .map(|storage| Self { storage })
            .map_err(storage_error)
    }
}

impl WorkerEnterpriseQuotaAuthorityPort for DurableWorkerQuotaAuthoritySource {
    fn load(
        &mut self,
        job_id: &ExecutionJobId,
        claim: &ExecutionLeaseClaim,
    ) -> Result<Option<WorkerEnterpriseQuotaAuthority>, WorkerEnterpriseQuotaError> {
        let admission = self
            .storage
            .execution_admission()
            .map_err(|_| worker_authority_unavailable())?
            .load_reservation_by_job(job_id)
            .map_err(|_| worker_authority_unavailable())?;
        let Some(admission) = admission else {
            return Ok(None);
        };
        let job = self
            .storage
            .execution_queue()
            .map_err(|_| worker_authority_unavailable())?
            .load_job(&admission.scope, job_id)
            .map_err(|_| worker_authority_unavailable())?;
        let Some(job) = job else {
            return Ok(None);
        };
        let registry = self
            .storage
            .execution_registry()
            .map_err(|_| worker_authority_unavailable())?;
        let placement = registry
            .load_authenticated_worker_placement(&claim.worker_id, &claim.worker_instance_id)
            .map_err(|_| worker_authority_unavailable())?;
        let Some(placement) = placement else {
            return Ok(None);
        };
        let worker = registry
            .load_worker(&claim.worker_id)
            .map_err(|_| worker_authority_unavailable())?;
        let Some(worker) = worker else {
            return Ok(None);
        };
        if worker.worker_instance_id != placement.worker_instance_id
            || worker.management_scope != placement.management_scope
            || worker.authentication_identity != placement.authentication_identity
        {
            return Err(WorkerEnterpriseQuotaError::new(
                WorkerEnterpriseQuotaErrorKind::AuthorityMismatch,
            ));
        }
        WorkerEnterpriseQuotaAuthority::from_durable_records(
            job,
            admission,
            placement,
            claim.clone(),
        )
        .map(Some)
    }
}

struct DurableAuthenticatedRegistryClaim {
    storage: SqliteStorage,
}

impl DurableAuthenticatedRegistryClaim {
    fn open(data_directory: &Path) -> Result<Self, WorkerExecutionLifecycleError> {
        SqliteStorage::open(data_directory)
            .map(|storage| Self { storage })
            .map_err(storage_error)
    }
}

impl WorkerOperationalClaimPort for DurableAuthenticatedRegistryClaim {
    type Receipt = ExecutionLeaseReceipt;

    fn claim(
        &mut self,
        authority: &WorkerEnterpriseQuotaAuthority,
    ) -> Result<Self::Receipt, WorkerEnterpriseQuotaError> {
        let receipt = self
            .storage
            .execution_registry()
            .map_err(|_| worker_operational_error())?
            .claim_execution_job_with_authenticated_placement(authority.claim())
            .map_err(|_| worker_operational_error())?;
        if !matches!(
            receipt.status,
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
        ) || receipt.lease.is_none()
        {
            return Err(worker_operational_error());
        }
        Ok(receipt)
    }
}

fn require_terminal_authority(
    storage: &mut SqliteStorage,
    command: &WorkerExecutionUsageSettlement,
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
        || !matches!(
            slot.state,
            WorkerSlotState::Running | WorkerSlotState::Completed
        )
        || command.completed_at.0 < lease.issued_at.0
    {
        return Err(authority_error());
    }
    Ok(current)
}

fn require_exact_settlement_replay(
    storage: &mut SqliteStorage,
    current: &ExecutionReservationRecord,
    command: &WorkerExecutionUsageSettlement,
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

fn require_exact_enterprise_release(
    enterprise: &EnterpriseQuotaReservationRecord,
    command: &WorkerExecutionRelease,
) -> Result<(), WorkerExecutionLifecycleError> {
    let Some(EnterpriseQuotaTerminal::Released {
        request_id,
        reason,
        released_at,
    }) = enterprise.terminal.as_ref()
    else {
        return Err(authority_error());
    };
    if request_id != &command.request_id
        || *reason != release_reason(command.reason)
        || released_at != &command.released_at
    {
        return Err(authority_error());
    }
    Ok(())
}

fn release_reason(reason: ExecutionReservationReleaseReason) -> EnterpriseQuotaReleaseReason {
    match reason {
        ExecutionReservationReleaseReason::Cancelled => EnterpriseQuotaReleaseReason::Cancelled,
        ExecutionReservationReleaseReason::Failed => EnterpriseQuotaReleaseReason::Failed,
    }
}

fn worker_quota_reservation_id(job_id: &ExecutionJobId) -> RequestId {
    stable_request_id("reserve", &job_id.0)
}

fn stable_request_id(action: &str, identity: &str) -> RequestId {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(
        [
            b"winwincode.worker-enterprise-quota.v1\0".as_slice(),
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

fn worker_authority_unavailable() -> WorkerEnterpriseQuotaError {
    WorkerEnterpriseQuotaError::new(WorkerEnterpriseQuotaErrorKind::AuthorityUnavailable)
}

fn worker_operational_error() -> WorkerEnterpriseQuotaError {
    WorkerEnterpriseQuotaError::new(WorkerEnterpriseQuotaErrorKind::OperationalClaim)
}

fn authority_error() -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Authority)
}

fn worker_quota_error(_error: WorkerEnterpriseQuotaError) -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Quota)
}

fn admission_error<T>(_error: T) -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::OperationalAdmission)
}

fn quota_error<T>(_error: T) -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Quota)
}

fn usage_error<T>(_error: T) -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Usage)
}

fn storage_error(_error: StorageError) -> WorkerExecutionLifecycleError {
    WorkerExecutionLifecycleError::new(WorkerExecutionLifecycleErrorKind::Storage)
}
