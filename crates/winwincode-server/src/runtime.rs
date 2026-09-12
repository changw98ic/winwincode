// SPDX-License-Identifier: Apache-2.0

//! Supervised local execution runtime owned by the Server composition.
//!
//! The supervisor does not create a second Control Plane, `SQLite` connection,
//! queue, or event hub. Callers inject the already-composed [`ExecutionPort`] core
//! into [`LocalLauncher`], and this module only serializes launcher lifecycle,
//! periodic liveness work, and ordered shutdown.

use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

#[cfg(feature = "local-worker")]
use futures::FutureExt;
use sha2::{Digest, Sha256};
#[cfg(feature = "local-worker")]
use std::sync::atomic::{AtomicU8, Ordering};
#[cfg(feature = "local-worker")]
use tokio::sync::{Mutex, watch};
#[cfg(feature = "local-worker")]
use tokio::task::JoinHandle;
use winwincode_domain::{RepositoryScope, RepositoryScopeKind};

use winwincode_control_plane::{
    DurableExecutionPortDelegate, DurableExecutionPortIngress, DurableWorkerExecutionLifecycle,
    RepositoryExecutionScheduler,
};
use winwincode_domain::{ExecutionJobId, Instant, RequestId, UserId, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionPortMessage, ExecutionWorkspaceWriteMode, JobDispatchMessage,
};
use winwincode_execution_port::{
    execution_identity::canonical_dispatch_session_identity, transport::ExecutionPortCore,
};
#[cfg(feature = "local-worker")]
use winwincode_local::{
    LocalExecutionPortHandle, LocalLauncher, LocalLauncherConfig, SharedControlPlaneHandle,
};
use winwincode_storage::{
    ExecutionAdmissionBoundary, ExecutionAdmissionErrorCode, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionJobRecord, ExecutionJobState, ExecutionLeaseClaim,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationStart,
    ExecutionReservationState, RepositorySchedulerClaimRequest, RepositorySchedulerScope,
    WorkerOutboundAuthority, WorkerPoolId, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources,
};
#[cfg(feature = "local-worker")]
use winwincode_worker::{CodexCoreAdapter, WorkerConfig};

use crate::StandaloneApplicationClock;
use crate::application::{ApplicationState, StandaloneControlPlaneApplication};

#[cfg(feature = "local-worker")]
const HEALTHY: u8 = 0;
#[cfg(feature = "local-worker")]
const FAULTED: u8 = 1;
#[cfg(feature = "local-worker")]
const STOPPED: u8 = 2;
const DEFAULT_RUNTIME_USER_ID: &str = "usr_00000000000000000000000001";
const DEFAULT_RUNTIME_WORKER_POOL_ID: &str = "wpl_00000000000000000000000001";
const LOCAL_ADMISSION_LIMITS: ExecutionAdmissionLimits = ExecutionAdmissionLimits {
    max_concurrent: 1,
    max_queued: 10_000,
    token_budget: 1_000_000_000,
    cost_budget_microunits: 1_000_000_000,
    max_runtime_millis: 604_800_000,
};
const LOCAL_SLOT_RESOURCE_LIMITS: WorkerSlotResourceLimits = WorkerSlotResourceLimits {
    max_memory_bytes: 1_073_741_824,
    max_disk_bytes: 1_073_741_824,
    max_processes: 10_000,
};
const LOCAL_SLOT_RESOURCES: WorkerSlotResources = WorkerSlotResources {
    memory_bytes: 1,
    disk_bytes: 1,
    process_slots: 1,
};
const RUNTIME_INTERACTION_PAGE_SIZE: usize = 100;

/// Admission denials that describe expected queue pressure rather than a
/// broken runtime.  The driver must leave the Job queued and retry after the
/// current Worker slot or budget is released; turning these into a health
/// fault strands the entire local runtime behind one ordinary backpressure
/// event.
const fn deferred_admission_error(code: ExecutionAdmissionErrorCode) -> bool {
    matches!(
        code,
        ExecutionAdmissionErrorCode::RevisionConflict
            | ExecutionAdmissionErrorCode::QueueCapacityExhausted
            | ExecutionAdmissionErrorCode::ConcurrencyExhausted
            | ExecutionAdmissionErrorCode::TokenBudgetExhausted
            | ExecutionAdmissionErrorCode::CostBudgetExhausted
            | ExecutionAdmissionErrorCode::RepositoryWriteConflict
    )
}

/// Stable failure categories for the supervised local runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeSupervisorErrorKind {
    InvalidConfiguration,
    Launcher,
    Driver,
    Faulted,
    Shutdown,
}

/// Secret-free runtime supervisor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeSupervisorError {
    kind: RuntimeSupervisorErrorKind,
    message: &'static str,
}

impl RuntimeSupervisorError {
    const fn new(kind: RuntimeSupervisorErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(self) -> RuntimeSupervisorErrorKind {
        self.kind
    }

    pub(crate) const fn transport_unavailable() -> Self {
        Self::new(
            RuntimeSupervisorErrorKind::Launcher,
            "remote execution-port queue is unavailable",
        )
    }
}

impl fmt::Display for RuntimeSupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for RuntimeSupervisorError {}

/// Read-only health port installed on the public application.
pub trait RuntimeHealthPort: Send + Sync {
    /// Returns `true` while every supervised runtime task is healthy.
    #[must_use]
    fn is_healthy(&self) -> bool;
}

/// Health implementation used before a local runtime is attached.
///
/// The composition root replaces this value with [`RuntimeHealthHandle`] once
/// the launcher has started. Keeping the default explicit means an application
/// built for command/query-only tests remains available while production code
/// can opt into fail-closed supervised health.
#[derive(Clone, Copy, Debug, Default)]
pub struct HealthyRuntimeHealth;

impl RuntimeHealthPort for HealthyRuntimeHealth {
    fn is_healthy(&self) -> bool {
        true
    }
}

/// Stable error categories for the Server-owned execution-port core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerExecutionPortErrorKind {
    ApplicationUnavailable,
    StatePoisoned,
    Ingress,
    Publication,
}

/// Secret-free error returned when the Server's shared application state cannot
/// accept a Worker frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerExecutionPortError {
    kind: ServerExecutionPortErrorKind,
    message: &'static str,
}

impl ServerExecutionPortError {
    const fn new(kind: ServerExecutionPortErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(self) -> ServerExecutionPortErrorKind {
        self.kind
    }
}

impl fmt::Display for ServerExecutionPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ServerExecutionPortError {}

/// Synchronous scheduler hook executed by the supervised local driver.
///
/// The hook only performs short durable work and enqueues typed messages on
/// the supplied local port. It must not retain the port or start an async task;
/// the supervisor remains the sole owner of the Worker lifecycle.
#[cfg(feature = "local-worker")]
pub trait LocalRuntimeScheduler: Send + 'static {
    /// Claims/replays pending work and enqueues exact CP-to-Worker messages.
    ///
    /// # Errors
    ///
    /// Returns a stable driver failure when durable scheduling cannot make
    /// progress.
    fn drive(
        &mut self,
        now: Instant,
        execution_port: &LocalExecutionPortHandle,
    ) -> Result<(), RuntimeSupervisorError>;

    /// Acknowledges Control Plane-to-Worker interaction frames after the local
    /// launcher has accepted the queued batch. Implementations may retain
    /// claimed frames across a failed drive so restart replays the exact bytes.
    ///
    /// # Errors
    ///
    /// Returns a stable driver failure when an interaction acknowledgement
    /// cannot be committed.
    fn complete(&mut self, _now: Instant) -> Result<(), RuntimeSupervisorError> {
        Ok(())
    }
}

/// Minimal Control Plane-to-Worker queue used by both the same-process
/// launcher and the authenticated remote exchange.
pub trait RuntimeControlOutbound {
    /// Retains one canonical control message until its transport accepts it.
    ///
    /// # Errors
    ///
    /// Returns a bounded runtime error when the transport queue is unavailable.
    fn enqueue_control(&self, message: ExecutionPortMessage) -> Result<(), RuntimeSupervisorError>;
}

#[cfg(feature = "local-worker")]
impl RuntimeControlOutbound for LocalExecutionPortHandle {
    fn enqueue_control(&self, message: ExecutionPortMessage) -> Result<(), RuntimeSupervisorError> {
        LocalExecutionPortHandle::enqueue_control(self, message).map_err(|_| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Launcher,
                "local execution-port queue is unavailable",
            )
        })
    }
}

/// Durable repository scheduler used by the production local driver.
///
/// The scheduler owns no queue or worker state. It borrows the one
/// `ApplicationState` only for the duration of a synchronous drive, asks the
/// canonical repository scheduler for exact receipts, and puts the returned
/// `job.dispatch`/`job.cancel` messages on the launcher's typed CP-to-Worker
/// queue. It never creates a Job or a terminal outcome itself.
pub struct RepositoryRuntimeScheduler {
    state: Arc<StdMutex<Option<ApplicationState>>>,
    hub: Arc<crate::DurableEventHub>,
    scope: RepositorySchedulerScope,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    user_id: UserId,
    worker_pool_id: WorkerPoolId,
    scheduler_generation: String,
    lease_duration_millis: u64,
    sequence: u64,
    active_worker_authorities: Vec<WorkerOutboundAuthority>,
    pending_interaction_acknowledgements: Vec<(
        WorkerOutboundAuthority,
        winwincode_domain::ExecutionMessageId,
    )>,
}

impl RepositoryRuntimeScheduler {
    /// Creates a scheduler over one already-composed application authority.
    ///
    /// # Errors
    ///
    /// Rejects a non-repository scope, empty process identities, or a zero
    /// lease duration before the driver starts.
    pub fn from_application(
        application: &StandaloneControlPlaneApplication,
        scope: RepositoryScope,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        scheduler_generation: impl Into<String>,
        lease_duration: Duration,
    ) -> Result<Self, RuntimeSupervisorError> {
        let scheduler_generation = scheduler_generation.into();
        if scope.kind != RepositoryScopeKind::Repository
            || scope.organization_id.0.is_empty()
            || scope.workspace_id.0.is_empty()
            || scope.project_id.0.is_empty()
            || scope.repository_id.0.is_empty()
            || worker_id.0.is_empty()
            || worker_instance_id.0.is_empty()
            || scheduler_generation.is_empty()
            || lease_duration.is_zero()
        {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::InvalidConfiguration,
                "repository runtime scheduler configuration is invalid",
            ));
        }
        let lease_duration_millis = u64::try_from(lease_duration.as_millis()).map_err(|_| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::InvalidConfiguration,
                "repository runtime scheduler lease duration is invalid",
            )
        })?;
        if lease_duration_millis == 0 {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::InvalidConfiguration,
                "repository runtime scheduler lease duration is invalid",
            ));
        }
        Ok(Self {
            state: application.shared_runtime_state(),
            hub: application.runtime_hub(),
            scope: RepositorySchedulerScope {
                organization_id: scope.organization_id,
                workspace_id: scope.workspace_id,
                project_id: scope.project_id,
                repository_id: scope.repository_id,
            },
            worker_id,
            worker_instance_id,
            user_id: UserId(DEFAULT_RUNTIME_USER_ID.to_owned()),
            worker_pool_id: WorkerPoolId(DEFAULT_RUNTIME_WORKER_POOL_ID.to_owned()),
            scheduler_generation,
            lease_duration_millis,
            sequence: 0,
            active_worker_authorities: Vec::new(),
            pending_interaction_acknowledgements: Vec::new(),
        })
    }

    /// Adopts the durable active Owner as the local admission identity once
    /// one exists.
    ///
    /// The composition root resolves the Owner once at process startup, so a
    /// Server initialized after startup (browser bootstrap) would otherwise
    /// keep the stable default subject for its whole lifetime and reserve
    /// admission under an identity that can never pass the active-account
    /// check. Adoption re-reads the account ledger on every drive until the
    /// default is replaced; a lookup failure leaves the identity untouched so
    /// admission simply defers.
    fn refresh_admission_identity(&mut self, storage: &mut winwincode_storage::SqliteStorage) {
        if self.user_id.0 != DEFAULT_RUNTIME_USER_ID {
            return;
        }
        let active_owner = storage
            .user_account_ledger()
            .and_then(|ledger| ledger.active_owner())
            .ok()
            .flatten();
        if let Some(owner) = active_owner {
            self.user_id = owner.user_id;
        }
    }

    /// Installs the authenticated local user and the one configured Worker
    /// pool used by operational execution admission.  These identities are
    /// carried into the durable reservation; Worker frames cannot replace
    /// them. The user identity is the initializing Owner's durable userId
    /// once the Server is initialized; an uninitialized Server keeps the
    /// stable default until initialization assigns the Owner.
    ///
    /// # Errors
    ///
    /// Returns an error when either identity is outside the canonical format.
    pub fn with_admission_identity(
        mut self,
        user_id: Option<UserId>,
        worker_pool_id: WorkerPoolId,
    ) -> Result<Self, RuntimeSupervisorError> {
        if let Some(user_id) = &user_id
            && (!user_id.0.starts_with("usr_") || user_id.0.len() <= 4)
        {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::InvalidConfiguration,
                "repository runtime scheduler admission identity is invalid",
            ));
        }
        if !worker_pool_id.0.starts_with("wpl_") || worker_pool_id.0.len() <= 4 {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::InvalidConfiguration,
                "repository runtime scheduler admission identity is invalid",
            ));
        }
        if let Some(user_id) = user_id {
            self.user_id = user_id;
        }
        self.worker_pool_id = worker_pool_id;
        Ok(self)
    }

    /// Returns the scheduler's immutable repository scope.
    #[must_use]
    pub const fn scope(&self) -> &RepositorySchedulerScope {
        &self.scope
    }

    fn next_request_id(&mut self, purpose: &[u8]) -> RequestId {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        let mut digest = Sha256::new();
        digest.update(b"winwincode.server-runtime-request.v1\0");
        digest.update(purpose);
        digest.update((self.scheduler_generation.len() as u64).to_be_bytes());
        digest.update(self.scheduler_generation.as_bytes());
        digest.update(sequence.to_be_bytes());
        RequestId(format!("req_{}", crockford_26(&digest.finalize())))
    }

    fn job_request_id(&self, purpose: &[u8], job_id: &ExecutionJobId) -> RequestId {
        let mut digest = Sha256::new();
        digest.update(b"winwincode.server-runtime-job-request.v1\0");
        digest.update(purpose);
        digest.update([0]);
        digest.update(self.scheduler_generation.as_bytes());
        digest.update([0]);
        digest.update(self.worker_id.0.as_bytes());
        digest.update([0]);
        digest.update(self.worker_instance_id.0.as_bytes());
        digest.update([0]);
        digest.update(job_id.0.as_bytes());
        RequestId(format!("req_{}", crockford_26(&digest.finalize())))
    }

    fn ensure_admission(
        &self,
        storage: &mut winwincode_storage::SqliteStorage,
        record: &ExecutionJobRecord,
        now: &Instant,
    ) -> Result<bool, RuntimeSupervisorError> {
        let (job, runtime_seconds) = validate_admission_job(record)?;
        let runtime_limit_millis = runtime_seconds.checked_mul(1_000).ok_or_else(|| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler execution deadline overflowed",
            )
        })?;
        // Never reserve under an identity that is not an active durable
        // account: an uninitialized Server still carries the stable default
        // subject, and a reservation created under it could never pass the
        // active-account check below. Leave the Job queued (and the
        // supervisor healthy) until the Owner exists and
        // `refresh_admission_identity` adopts it.
        if !admission_user_is_active(storage, &self.user_id)? {
            return Ok(false);
        }
        let mut admission = storage.execution_admission().map_err(|error| {
            debug_scheduler_error("open execution admission", &error);
            scheduler_failure()
        })?;
        let reservation = admission
            .load_reservation_by_job(&record.job_id)
            .map_err(|error| {
                debug_scheduler_error("load execution admission reservation", &error);
                scheduler_failure()
            })?;
        let reservation = if let Some(reservation) = reservation {
            reservation
        } else {
            let policy_limits = ExecutionAdmissionLimits {
                max_runtime_millis: runtime_limit_millis
                    .max(LOCAL_ADMISSION_LIMITS.max_runtime_millis),
                ..LOCAL_ADMISSION_LIMITS
            };
            self.configure_admission_policies(&mut admission, &record.scope, policy_limits)?;
            let Some(reservation) =
                self.reserve_admission(&mut admission, record, &job, runtime_limit_millis)?
            else {
                return Ok(false);
            };
            reservation
        };
        if reservation.scope != record.scope || reservation.worker_pool_id != self.worker_pool_id {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler admission identity differs",
            ));
        }
        // Admission admits reservations from any active user account instead
        // of one fixed process subject: every browser session now carries its
        // own durable userId, so the reservation subject may differ per user.
        // The stable default subject is the embedded local runtime's own
        // pre-initialization identity rather than an account: it can never
        // resolve through the ledger, so reservations it created (a Server
        // that queued work before its first Owner existed) stay admissible
        // for the local pool instead of faulting the supervisor on every
        // restart.
        let reservation_user = reservation.user_id.clone();
        let reservation_state = reservation.state;
        let reservation_revision = reservation.revision;
        let reservation_updated_at = reservation.updated_at.clone();
        let reservation_scope = reservation.scope.clone();
        let reservation_pool = reservation.worker_pool_id.clone();
        // The first admission borrow ends here; the reopen below shadows it.
        if reservation_user.0 != DEFAULT_RUNTIME_USER_ID
            && !admission_user_is_active(storage, &reservation_user)?
        {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler admission identity differs",
            ));
        }
        let mut admission = storage.execution_admission().map_err(|error| {
            debug_scheduler_error("reopen execution admission", &error);
            scheduler_failure()
        })?;
        match reservation_state {
            ExecutionReservationState::Queued => {
                // A command may commit a new queued Job after this driver's
                // tick captured its clock value.  Admission intentionally
                // rejects an operation timestamp that precedes the durable
                // reservation; leave the Job queued and retry on the next
                // tick instead of faulting the supervisor or claiming a
                // dispatch without a running reservation.
                if now.0 < reservation_updated_at.0 {
                    return Ok(false);
                }
                match admission.start(&ExecutionReservationStart {
                    scope: reservation_scope,
                    worker_pool_id: reservation_pool,
                    job_id: reservation.job_id,
                    request_id: self.job_request_id(b"admission-start", &record.job_id),
                    expected_revision: reservation_revision,
                    started_at: now.clone(),
                }) {
                    Ok(_) => {}
                    Err(error) if deferred_admission_error(error.code()) => {
                        debug_scheduler_error("defer execution admission start", &error);
                        return Ok(false);
                    }
                    Err(error) => {
                        debug_scheduler_error("start execution admission", &error);
                        return Err(scheduler_failure());
                    }
                }
            }
            ExecutionReservationState::Running => {}
            ExecutionReservationState::Released | ExecutionReservationState::Settled => {
                return Err(RuntimeSupervisorError::new(
                    RuntimeSupervisorErrorKind::Driver,
                    "repository runtime scheduler found terminal admission for queued work",
                ));
            }
        }
        Ok(true)
    }

    fn reserve_admission(
        &self,
        admission: &mut winwincode_storage::ExecutionAdmission<'_>,
        record: &ExecutionJobRecord,
        job: &ExecutionJob,
        runtime_limit_millis: u64,
    ) -> Result<Option<winwincode_storage::ExecutionReservationRecord>, RuntimeSupervisorError>
    {
        let repository_access = match job.workspace.write_mode {
            ExecutionWorkspaceWriteMode::ReadOnly => ExecutionRepositoryAccess::ReadOnly,
            ExecutionWorkspaceWriteMode::Candidate => ExecutionRepositoryAccess::IsolatedWrite {
                worktree_key: format!("job-{}", record.job_id.0),
            },
        };
        let request = ExecutionReservationRequest {
            scope: record.scope.clone(),
            user_id: self.user_id.clone(),
            worker_pool_id: self.worker_pool_id.clone(),
            job_id: record.job_id.clone(),
            request_id: self.job_request_id(b"admission-reserve", &record.job_id),
            repository_access,
            reserved_tokens: 1_000_000,
            reserved_cost_microunits: 1_000_000,
            runtime_limit_millis,
            submitted_at: record.submitted_at.clone(),
        };
        match admission.reserve(&request) {
            Ok(receipt) => Ok(Some(receipt.reservation)),
            Err(error) if deferred_admission_error(error.code()) => {
                debug_scheduler_error("defer execution admission reservation", &error);
                Ok(None)
            }
            Err(error) => {
                debug_scheduler_error("reserve execution admission", &error);
                Err(scheduler_failure())
            }
        }
    }

    fn configure_admission_policies(
        &self,
        admission: &mut winwincode_storage::ExecutionAdmission<'_>,
        scope: &winwincode_storage::ExecutionQueueScope,
        policy_limits: ExecutionAdmissionLimits,
    ) -> Result<(), RuntimeSupervisorError> {
        for boundary in admission_boundaries(scope, &self.worker_pool_id) {
            admission
                .configure_policy(&ExecutionAdmissionPolicy {
                    boundary,
                    limits: policy_limits,
                })
                .map_err(|error| {
                    debug_scheduler_error("configure execution admission policy", &error);
                    scheduler_failure()
                })?;
        }
        Ok(())
    }

    fn ensure_worker_slot(
        &self,
        storage: &mut winwincode_storage::SqliteStorage,
        dispatch: &JobDispatchMessage,
        now: &Instant,
    ) -> Result<WorkerOutboundAuthority, RuntimeSupervisorError> {
        if dispatch.lease.worker_id != self.worker_id
            || dispatch.lease.worker_instance_id != self.worker_instance_id
        {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler received a foreign Worker dispatch",
            ));
        }
        let (worker_session_id, codex_thread_id) = canonical_dispatch_session_identity(
            &self.worker_id,
            &self.worker_instance_id,
            dispatch,
        )
        .map_err(|_| scheduler_failure())?;
        let attempt = u64::try_from(dispatch.lease.attempt).map_err(|_| scheduler_failure())?;
        let authority = WorkerSlotAuthority {
            worker_id: self.worker_id.clone(),
            worker_instance_id: self.worker_instance_id.clone(),
            worker_session_id: worker_session_id.clone(),
            codex_thread_id,
            job_id: dispatch.job.job_id.clone(),
            lease_id: dispatch.lease.lease_id.clone(),
            attempt,
            fencing_token: dispatch.lease.fencing_token.clone(),
        };
        let mut slots = storage
            .worker_session_slots()
            .map_err(|_| scheduler_failure())?;
        slots
            .configure_resources(
                &self.worker_id,
                &self.worker_instance_id,
                LOCAL_SLOT_RESOURCE_LIMITS,
            )
            .map_err(|_| scheduler_failure())?;
        if let Some(existing) = slots
            .load(&worker_session_id)
            .map_err(|_| scheduler_failure())?
        {
            if existing.authority == authority
                && existing.state == winwincode_storage::WorkerSlotState::Running
            {
                return Ok(WorkerOutboundAuthority {
                    slot: authority,
                    lease_issued_at: dispatch.lease.issued_at.clone(),
                    lease_expires_at: dispatch.lease.expires_at.clone(),
                });
            }
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler found a conflicting Worker slot",
            ));
        }
        slots
            .open(&WorkerSlotOpenRequest {
                authority: authority.clone(),
                resources: LOCAL_SLOT_RESOURCES,
                request_id: self.job_request_id(b"worker-slot-open", &dispatch.job.job_id),
                opened_at: now.clone(),
            })
            .map_err(|error| {
                debug_scheduler_error("open Worker slot", &error);
                scheduler_failure()
            })?;
        Ok(WorkerOutboundAuthority {
            slot: authority,
            lease_issued_at: dispatch.lease.issued_at.clone(),
            lease_expires_at: dispatch.lease.expires_at.clone(),
        })
    }

    /// Configures and reads the durable slot table before claiming work.
    ///
    /// Repository scheduling persists a lease before the Worker can accept a
    /// dispatch.  Without this preflight a full local Worker would leave a
    /// durable job in `Leased`, then `ensure_worker_slot` would turn ordinary
    /// capacity pressure into a supervisor fault.  The slot table is the
    /// authoritative source here because it is updated in the same durable
    /// lifecycle as the dispatch and terminal outcome.
    fn ensure_worker_capacity(
        &self,
        storage: &mut winwincode_storage::SqliteStorage,
    ) -> Result<bool, RuntimeSupervisorError> {
        let mut slots = storage.worker_session_slots().map_err(|error| {
            debug_scheduler_error("open Worker slots for capacity preflight", &error);
            scheduler_failure()
        })?;
        slots
            .configure_resources(
                &self.worker_id,
                &self.worker_instance_id,
                LOCAL_SLOT_RESOURCE_LIMITS,
            )
            .map_err(|error| {
                debug_scheduler_error("configure Worker slot capacity", &error);
                scheduler_failure()
            })?;
        let capacity = slots
            .capacity(&self.worker_id, &self.worker_instance_id)
            .map_err(|error| {
                debug_scheduler_error("read Worker slot capacity", &error);
                scheduler_failure()
            })?;
        let memory_available = capacity
            .limits
            .max_memory_bytes
            .saturating_sub(capacity.reserved.memory_bytes);
        let disk_available = capacity
            .limits
            .max_disk_bytes
            .saturating_sub(capacity.reserved.disk_bytes);
        let process_slots_available = capacity
            .limits
            .max_processes
            .saturating_sub(capacity.reserved.process_slots);
        Ok(capacity.available_slots > 0
            && memory_available >= LOCAL_SLOT_RESOURCES.memory_bytes
            && disk_available >= LOCAL_SLOT_RESOURCES.disk_bytes
            && process_slots_available >= LOCAL_SLOT_RESOURCES.process_slots)
    }

    fn remember_worker_authority(&mut self, authority: WorkerOutboundAuthority) {
        if let Some(existing) = self
            .active_worker_authorities
            .iter_mut()
            .find(|existing| existing.slot.job_id == authority.slot.job_id)
        {
            *existing = authority;
        } else {
            self.active_worker_authorities.push(authority);
        }
    }

    fn prune_worker_authorities(
        &mut self,
        storage: &mut winwincode_storage::SqliteStorage,
    ) -> Result<(), RuntimeSupervisorError> {
        let active = storage
            .repository_scheduler()
            .map_err(|_| scheduler_failure())?
            .list_jobs(
                &self.scope,
                &[
                    ExecutionJobState::Leased,
                    ExecutionJobState::Running,
                    ExecutionJobState::Cancelling,
                ],
            )
            .map_err(|_| scheduler_failure())?;
        self.active_worker_authorities.retain(|authority| {
            active
                .iter()
                .any(|record| record.job_id == authority.slot.job_id)
        });
        Ok(())
    }

    fn dispatch_interactions(
        &mut self,
        state: &mut ApplicationState,
        now: &Instant,
        execution_port: &dyn RuntimeControlOutbound,
    ) -> Result<(), RuntimeSupervisorError> {
        if !self.pending_interaction_acknowledgements.is_empty() {
            return Ok(());
        }
        for authority in self.active_worker_authorities.clone() {
            let mut cursor = None;
            loop {
                let page = state
                    .worker_outbound
                    .claim_page(
                        &authority,
                        now,
                        cursor.as_ref(),
                        RUNTIME_INTERACTION_PAGE_SIZE,
                    )
                    .map_err(|_| scheduler_failure())?;
                for claim in page.claims {
                    execution_port.enqueue_control(claim.typed_frame().message().clone())?;
                    self.pending_interaction_acknowledgements
                        .push((authority.clone(), claim.message_id().clone()));
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
        }
        Ok(())
    }

    #[cfg(feature = "local-worker")]
    fn acknowledge_interactions(
        &mut self,
        state: &mut ApplicationState,
        now: &Instant,
    ) -> Result<(), RuntimeSupervisorError> {
        let pending = std::mem::take(&mut self.pending_interaction_acknowledgements);
        for (index, (authority, message_id)) in pending.iter().enumerate() {
            if state
                .worker_outbound
                .acknowledge(authority, message_id, now)
                .is_err()
            {
                self.pending_interaction_acknowledgements
                    .extend(pending[index..].iter().cloned());
                return Err(scheduler_failure());
            }
        }
        Ok(())
    }

    fn ensure_queued_admission(
        &self,
        state: &mut ApplicationState,
        now: &Instant,
    ) -> Result<bool, RuntimeSupervisorError> {
        let queued = state
            .storage
            .repository_scheduler()
            .map_err(|error| {
                debug_scheduler_error("open repository scheduler", &error);
                scheduler_failure()
            })?
            .list_jobs(&self.scope, &[ExecutionJobState::Queued])
            .map_err(|error| {
                debug_scheduler_error("list queued jobs", &error);
                scheduler_failure()
            })?;
        // Local/fleet Workers skip device jobs. A Device Worker selects only
        // the job whose sealed launch facts name this exact process.
        let mut matching_head = None;
        for record in &queued {
            let device_facts = {
                let ledger = state
                    .storage
                    .device_execution_binding_ledger()
                    .map_err(|error| {
                        debug_scheduler_error("open device execution binding ledger", &error);
                        scheduler_failure()
                    })?;
                ledger.facts(record.job_id.0.as_str()).map_err(|error| {
                    debug_scheduler_error("read device execution facts", &error);
                    scheduler_failure()
                })?
            };
            let matches = match device_facts {
                None => {
                    self.worker_pool_id.0
                        != winwincode_control_plane::STRONGFLOW_DEVICE_WORKER_POOL_ID
                }
                Some(facts) => {
                    facts.worker_id == self.worker_id.0
                        && facts.worker_instance_id == self.worker_instance_id.0
                }
            };
            if matches {
                matching_head = Some(record);
                break;
            }
        }
        let Some(record) = matching_head else {
            return Ok(true);
        };
        let admission_ready = self
            .ensure_admission(&mut state.storage, record, now)
            .inspect_err(|error| debug_scheduler_error("ensure admission", error))?;
        if !admission_ready {
            self.hub
                .publish_pending(&mut state.storage)
                .map_err(|error| {
                    debug_scheduler_error("publish pending events", &error);
                    scheduler_failure()
                })?;
        }
        Ok(admission_ready)
    }

    fn dispatch_scheduler_messages(
        &mut self,
        state: &mut ApplicationState,
        now: &Instant,
        execution_port: &dyn RuntimeControlOutbound,
        admission_ready: bool,
        authenticated_remote: bool,
    ) -> Result<(), RuntimeSupervisorError> {
        let expires_at = add_millis(now, self.lease_duration_millis).ok_or_else(|| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler could not construct a lease expiry",
            )
        })?;
        let request_id = self.next_request_id(b"claim");
        let cancellations = {
            let mut scheduler = RepositoryExecutionScheduler::new(&mut state.storage);
            scheduler
                .pending_cancellations(&self.scope)
                .map_err(|error| {
                    debug_scheduler_error("list pending cancellations", &error);
                    scheduler_failure()
                })?
        };
        for cancellation in cancellations {
            execution_port.enqueue_control(ExecutionPortMessage::JobCancelMessage(cancellation))?;
        }
        // A queued Job may be waiting for an admission timestamp or for
        // ordinary concurrency/budget capacity.  Cancellation and interaction
        // traffic for already-running work must still flow while that queue
        // head waits; otherwise a cancellation can never release the very
        // slot needed to make admission progress.
        if !admission_ready {
            return Ok(());
        }
        // `claim_next` writes the Registry lease before the local Worker slot
        // is opened.  Do not create that lease while the current Worker is at
        // its durable slot/resource ceiling.  A normal restart has a fresh
        // Worker instance, so old-instance slots are not counted and the
        // scheduler can perform its replacement transaction.
        if !self.ensure_worker_capacity(&mut state.storage)? {
            return Ok(());
        }
        let mut scheduler = RepositoryExecutionScheduler::new(&mut state.storage);
        let dispatch = scheduler
            .claim_next(&RepositorySchedulerClaimRequest {
                scope: self.scope.clone(),
                request_id,
                scheduler_generation: self.scheduler_generation.clone(),
                worker_id: self.worker_id.clone(),
                worker_instance_id: self.worker_instance_id.clone(),
                issued_at: now.clone(),
                expires_at,
            })
            .map_err(|error| {
                debug_scheduler_error("claim next job", &error);
                scheduler_failure()
            })?;
        if let Some(dispatch) = dispatch {
            if authenticated_remote {
                let data_directory = state
                    .storage
                    .database_path()
                    .parent()
                    .ok_or_else(scheduler_failure)?;
                let claim = ExecutionLeaseClaim {
                    expires_at: dispatch.lease.expires_at.clone(),
                    fencing_token: dispatch.lease.fencing_token.clone(),
                    issued_at: dispatch.lease.issued_at.clone(),
                    job_id: dispatch.lease.job_id.clone(),
                    lease_id: dispatch.lease.lease_id.clone(),
                    message_id: dispatch.message_id.clone(),
                    payload_digest: dispatch.job.payload_digest.clone(),
                    request_id: dispatch.request_id.clone(),
                    worker_id: dispatch.lease.worker_id.clone(),
                    worker_instance_id: dispatch.lease.worker_instance_id.clone(),
                    attempt: u64::try_from(dispatch.lease.attempt)
                        .map_err(|_| scheduler_failure())?,
                };
                DurableWorkerExecutionLifecycle::open(data_directory)
                    .and_then(|mut lifecycle| lifecycle.claim(&claim))
                    .map_err(|error| {
                        debug_scheduler_error("claim remote Worker execution", &error);
                        scheduler_failure()
                    })?;
            }
            let authority = self
                .ensure_worker_slot(&mut state.storage, &dispatch, now)
                .inspect_err(|error| debug_scheduler_error("ensure Worker slot", error))?;
            execution_port.enqueue_control(ExecutionPortMessage::JobDispatchMessage(dispatch))?;
            self.remember_worker_authority(authority);
        }
        Ok(())
    }
}

impl RepositoryRuntimeScheduler {
    /// Drives the canonical repository scheduler into a separated Worker
    /// transport without changing its durable queue, lease, or slot owners.
    ///
    /// # Errors
    ///
    /// Returns the same durable scheduling failures as the local driver.
    pub fn drive_remote(
        &mut self,
        now: &Instant,
        outbound: &dyn RuntimeControlOutbound,
    ) -> Result<(), RuntimeSupervisorError> {
        self.drive_outbound(now, outbound, true)
    }

    /// Drives one authenticated remote process using its transport-bound
    /// worker and pool identities.
    ///
    /// # Errors
    ///
    /// Returns the same durable scheduling failures as the local driver.
    pub fn drive_remote_for(
        &mut self,
        now: &Instant,
        outbound: &dyn RuntimeControlOutbound,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        worker_pool_id: WorkerPoolId,
    ) -> Result<(), RuntimeSupervisorError> {
        let previous = (
            std::mem::replace(&mut self.worker_id, worker_id),
            std::mem::replace(&mut self.worker_instance_id, worker_instance_id),
            std::mem::replace(&mut self.worker_pool_id, worker_pool_id),
        );
        let result = self.drive_outbound(now, outbound, true);
        self.worker_id = previous.0;
        self.worker_instance_id = previous.1;
        self.worker_pool_id = previous.2;
        result
    }

    /// Commits only interaction receipts explicitly confirmed by the remote
    /// Worker. Unconfirmed messages remain claimed for exact replay.
    ///
    /// # Errors
    ///
    /// Returns a durable queue failure without dropping remaining receipts.
    pub fn acknowledge_remote(
        &mut self,
        now: &Instant,
        confirmed: &[winwincode_domain::ExecutionMessageId],
    ) -> Result<(), RuntimeSupervisorError> {
        if confirmed.is_empty() {
            return Ok(());
        }
        let state_handle = Arc::clone(&self.state);
        let mut guard = state_handle.lock().map_err(|_| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler state is unavailable",
            )
        })?;
        let state = guard.as_mut().ok_or_else(|| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Shutdown,
                "repository runtime scheduler application has stopped",
            )
        })?;
        let mut remaining = Vec::new();
        for (authority, message_id) in
            std::mem::take(&mut self.pending_interaction_acknowledgements)
        {
            if confirmed.contains(&message_id) {
                if state
                    .worker_outbound
                    .acknowledge(&authority, &message_id, now)
                    .is_err()
                {
                    remaining.push((authority, message_id));
                }
            } else {
                remaining.push((authority, message_id));
            }
        }
        self.pending_interaction_acknowledgements = remaining;
        Ok(())
    }

    fn drive_outbound(
        &mut self,
        now: &Instant,
        execution_port: &dyn RuntimeControlOutbound,
        authenticated_remote: bool,
    ) -> Result<(), RuntimeSupervisorError> {
        let state_handle = Arc::clone(&self.state);
        let mut guard = state_handle.lock().map_err(|_| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler state is unavailable",
            )
        })?;
        let state = guard.as_mut().ok_or_else(|| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Shutdown,
                "repository runtime scheduler application has stopped",
            )
        })?;
        self.refresh_admission_identity(&mut state.storage);
        self.prune_worker_authorities(&mut state.storage)?;
        let admission_ready = self.ensure_queued_admission(state, now)?;
        self.dispatch_scheduler_messages(
            state,
            now,
            execution_port,
            admission_ready,
            authenticated_remote,
        )?;
        self.dispatch_interactions(state, now, execution_port)?;
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| {
                debug_scheduler_error("publish pending events", &error);
                scheduler_failure()
            })?;
        Ok(())
    }
}

/// Reports whether the reservation subject resolves to one active durable
/// user account.
fn admission_user_is_active(
    storage: &mut winwincode_storage::SqliteStorage,
    user_id: &UserId,
) -> Result<bool, RuntimeSupervisorError> {
    let state = storage
        .user_account_ledger()
        .and_then(|ledger| ledger.find(user_id))
        .map(|account| account.map(|account| account.state))
        .map_err(|error| {
            debug_scheduler_error("load admission user account", &error);
            scheduler_failure()
        })?;
    Ok(state == Some(winwincode_domain::UserAccountState::Active))
}

fn validate_admission_job(
    record: &ExecutionJobRecord,
) -> Result<(ExecutionJob, u64), RuntimeSupervisorError> {
    let job: ExecutionJob = serde_json::from_slice(&record.dispatch_payload).map_err(|_| {
        RuntimeSupervisorError::new(
            RuntimeSupervisorErrorKind::Driver,
            "repository runtime scheduler found an invalid ExecutionJob payload",
        )
    })?;
    let attempt = u64::try_from(job.attempt).map_err(|_| scheduler_failure())?;
    if job.job_id != record.job_id
        || job.payload_digest != record.payload_digest
        || attempt != record.attempt
    {
        return Err(RuntimeSupervisorError::new(
            RuntimeSupervisorErrorKind::Driver,
            "repository runtime scheduler found mismatched ExecutionJob authority",
        ));
    }
    let runtime_seconds = u64::try_from(job.limits.max_runtime_seconds)
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler found an invalid execution deadline",
            )
        })?;
    Ok((job, runtime_seconds))
}

fn admission_boundaries(
    scope: &winwincode_storage::ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
) -> Vec<ExecutionAdmissionBoundary> {
    let mut boundaries = vec![
        ExecutionAdmissionBoundary::Organization {
            organization_id: scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        },
    ];
    if let Some(delivery_id) = &scope.delivery_id {
        boundaries.push(ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        });
    }
    boundaries
}

#[cfg(feature = "local-worker")]
impl LocalRuntimeScheduler for RepositoryRuntimeScheduler {
    fn drive(
        &mut self,
        now: Instant,
        execution_port: &LocalExecutionPortHandle,
    ) -> Result<(), RuntimeSupervisorError> {
        self.drive_outbound(&now, execution_port, false)
    }

    fn complete(&mut self, now: Instant) -> Result<(), RuntimeSupervisorError> {
        let state = self.state.clone();
        let mut guard = state.lock().map_err(|_| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Driver,
                "repository runtime scheduler state is unavailable",
            )
        })?;
        let state = guard.as_mut().ok_or_else(|| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Shutdown,
                "repository runtime scheduler application has stopped",
            )
        })?;
        self.acknowledge_interactions(state, &now)
    }
}

/// The one Worker-to-Control Plane core used by a local Server launcher.
///
/// It borrows no application state across an await: each synchronous typed
/// frame obtains the shared `ApplicationState` lock, runs the canonical durable
/// ingress, publishes pending public events through the same hub, and releases
/// the lock before returning. A second Control Plane, storage, hub, or queue is
/// never constructed here.
pub struct ServerExecutionPortCore<Delegate> {
    state: Arc<StdMutex<Option<ApplicationState>>>,
    hub: Arc<crate::DurableEventHub>,
    repository_scope: RepositoryScope,
    clock: Arc<dyn StandaloneApplicationClock>,
    delegate: Delegate,
}

impl<Delegate> ServerExecutionPortCore<Delegate> {
    /// Creates a core over the exact state and hub owned by one application.
    #[must_use]
    pub fn from_application(
        application: &StandaloneControlPlaneApplication,
        repository_scope: RepositoryScope,
        delegate: Delegate,
    ) -> Self {
        Self {
            state: application.shared_runtime_state(),
            hub: application.runtime_hub(),
            repository_scope,
            clock: application.runtime_clock(),
            delegate,
        }
    }

    #[must_use]
    pub const fn repository_scope(&self) -> &RepositoryScope {
        &self.repository_scope
    }

    #[must_use]
    pub fn delegate(&self) -> &Delegate {
        &self.delegate
    }

    pub fn delegate_mut(&mut self) -> &mut Delegate {
        &mut self.delegate
    }
}

impl<Delegate> ExecutionPortCore for ServerExecutionPortCore<Delegate>
where
    Delegate: DurableExecutionPortDelegate + Send,
{
    type Output = Vec<ExecutionPortMessage>;
    type Error = ServerExecutionPortError;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        let mut guard = self.state.lock().map_err(|_| {
            ServerExecutionPortError::new(
                ServerExecutionPortErrorKind::StatePoisoned,
                "Server application state is unavailable",
            )
        })?;
        let state = guard.as_mut().ok_or_else(|| {
            ServerExecutionPortError::new(
                ServerExecutionPortErrorKind::ApplicationUnavailable,
                "Server application has stopped",
            )
        })?;
        let server_time = self.clock.now_instant();
        let mut ingress = DurableExecutionPortIngress::with_delegate(
            &mut state.control_plane,
            &mut state.storage,
            &self.repository_scope,
            server_time,
            &mut self.delegate,
        )
        .map_err(|_error| {
            ServerExecutionPortError::new(
                ServerExecutionPortErrorKind::Ingress,
                "Server Worker ingress configuration is invalid",
            )
        })?;
        let responses = ingress.handle(message).map_err(|error| {
            if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                eprintln!("Server Worker ingress category: {error}");
            }
            ServerExecutionPortError::new(
                ServerExecutionPortErrorKind::Ingress,
                "Server Worker ingress rejected a typed frame",
            )
        })?;
        drop(ingress);
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|_error| {
                ServerExecutionPortError::new(
                    ServerExecutionPortErrorKind::Publication,
                    "Server public event publication is unavailable",
                )
            })?;
        Ok(responses)
    }
}

#[derive(Clone)]
#[cfg(feature = "local-worker")]
struct RuntimeHealthState {
    status: Arc<AtomicU8>,
}

#[cfg(feature = "local-worker")]
impl RuntimeHealthState {
    fn mark_faulted(&self) {
        let _ = self
            .status
            .compare_exchange(HEALTHY, FAULTED, Ordering::AcqRel, Ordering::Acquire);
    }

    fn mark_stopped(&self) {
        self.status.store(STOPPED, Ordering::Release);
    }

    fn is_healthy(&self) -> bool {
        self.status.load(Ordering::Acquire) == HEALTHY
    }
}

/// Cloneable health handle for an application and its HTTP health endpoint.
#[derive(Clone)]
#[cfg(feature = "local-worker")]
pub struct RuntimeHealthHandle {
    state: RuntimeHealthState,
}

#[cfg(feature = "local-worker")]
impl RuntimeHealthHandle {
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.state.is_healthy()
    }
}

#[cfg(feature = "local-worker")]
impl RuntimeHealthPort for RuntimeHealthHandle {
    fn is_healthy(&self) -> bool {
        self.state.is_healthy()
    }
}

/// One running local Worker and its supervised liveness driver.
#[cfg(feature = "local-worker")]
pub struct LocalRuntimeSupervisor<Core, Codex>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send + 'static,
    Codex: CodexCoreAdapter + Send + 'static,
{
    launcher: Arc<Mutex<Option<LocalLauncher<Core, Codex>>>>,
    health: RuntimeHealthHandle,
    stop: watch::Sender<bool>,
    driver: Option<JoinHandle<()>>,
    clock: Arc<dyn StandaloneApplicationClock>,
    tick: Duration,
}

#[cfg(feature = "local-worker")]
impl<Core, Codex> LocalRuntimeSupervisor<Core, Codex>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send + 'static,
    Codex: CodexCoreAdapter + Send + 'static,
{
    /// Starts the local launcher and then its supervised heartbeat/poll driver.
    ///
    /// The injected `Core` must already contain the Server's canonical
    /// Control Plane, `SQLite`, interaction outbox, repository scope, and event
    /// hub. No runtime-owned replacement is created here.
    ///
    /// # Errors
    ///
    /// Returns an error when launcher startup or runtime configuration fails.
    pub async fn start(
        config: LocalLauncherConfig,
        worker_config: WorkerConfig,
        control_plane_endpoint: Core,
        codex: Codex,
        clock: Arc<dyn StandaloneApplicationClock>,
        tick: Duration,
    ) -> Result<Self, RuntimeSupervisorError> {
        Self::start_with_scheduler(
            config,
            worker_config,
            control_plane_endpoint,
            codex,
            clock,
            tick,
            None,
        )
        .await
    }

    /// Starts the launcher with one supervised scheduler hook.
    ///
    /// # Errors
    ///
    /// Returns an error when the polling interval is invalid or launcher startup fails.
    pub async fn start_with_scheduler(
        config: LocalLauncherConfig,
        worker_config: WorkerConfig,
        control_plane_endpoint: Core,
        codex: Codex,
        clock: Arc<dyn StandaloneApplicationClock>,
        tick: Duration,
        scheduler: Option<Box<dyn LocalRuntimeScheduler>>,
    ) -> Result<Self, RuntimeSupervisorError> {
        if tick.is_zero() {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::InvalidConfiguration,
                "runtime supervisor tick must be positive",
            ));
        }
        let worker_now = clock.now_instant();
        let launcher = LocalLauncher::start(
            config,
            worker_config,
            control_plane_endpoint,
            codex,
            worker_now,
        )
        .await
        .map_err(|_| {
            RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Launcher,
                "local runtime launcher failed to start",
            )
        })?;
        let health = RuntimeHealthHandle {
            state: RuntimeHealthState {
                status: Arc::new(AtomicU8::new(HEALTHY)),
            },
        };
        let (stop, stop_rx) = watch::channel(false);
        let launcher = Arc::new(Mutex::new(Some(launcher)));
        let driver_health = health.state.clone();
        let driver_launcher = Arc::clone(&launcher);
        let driver_clock = Arc::clone(&clock);
        let driver = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(run_driver(
                driver_launcher,
                driver_health.clone(),
                stop_rx,
                driver_clock,
                tick,
                scheduler,
            ))
            .catch_unwind()
            .await;
            if result.is_err() {
                driver_health.mark_faulted();
            }
        });
        Ok(Self {
            launcher,
            health,
            stop,
            driver: Some(driver),
            clock,
            tick,
        })
    }

    /// Returns the read-only health handle used by `StandaloneControlPlaneApplication`.
    #[must_use]
    pub fn health_handle(&self) -> RuntimeHealthHandle {
        self.health.clone()
    }

    /// Returns the supervisor polling interval.
    #[must_use]
    pub const fn tick(&self) -> Duration {
        self.tick
    }

    /// Runs one CP-to-Worker message through the sole local launcher.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is faulted or the launcher rejects the frame.
    pub async fn accept_control(
        &self,
        message: ExecutionPortMessage,
        now: Instant,
    ) -> Result<(), RuntimeSupervisorError> {
        self.ensure_healthy()?;
        let mut launcher = self.launcher.lock().await;
        let launcher = launcher.as_mut().ok_or_else(stopped_error)?;
        launcher
            .accept_control(message, now)
            .await
            .map_err(|_| self.mark_faulted(RuntimeSupervisorErrorKind::Launcher))
    }

    /// Runs one explicit launcher drive cycle.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is faulted or the launcher drive fails.
    pub async fn drive(&self, now: Instant) -> Result<usize, RuntimeSupervisorError> {
        self.ensure_healthy()?;
        let mut launcher = self.launcher.lock().await;
        let launcher = launcher.as_mut().ok_or_else(stopped_error)?;
        launcher
            .drive(now)
            .await
            .map_err(|_| self.mark_faulted(RuntimeSupervisorErrorKind::Launcher))
    }

    /// Returns the same CP-to-Worker queue handle used by the launcher.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is faulted or stopped.
    pub async fn execution_port(&self) -> Result<LocalExecutionPortHandle, RuntimeSupervisorError> {
        self.ensure_healthy()?;
        let launcher = self.launcher.lock().await;
        launcher
            .as_ref()
            .map(LocalLauncher::execution_port)
            .ok_or_else(stopped_error)
    }

    /// Returns the same shared Control Plane endpoint used by the launcher.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is faulted or stopped.
    pub async fn control_plane(
        &self,
    ) -> Result<SharedControlPlaneHandle<Core>, RuntimeSupervisorError> {
        self.ensure_healthy()?;
        let launcher = self.launcher.lock().await;
        launcher
            .as_ref()
            .map(LocalLauncher::control_plane)
            .ok_or_else(stopped_error)
    }

    /// Reports the current Worker lifecycle state through a short lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is faulted or stopped.
    pub async fn worker_lifecycle(
        &self,
    ) -> Result<winwincode_worker::WorkerLifecycleState, RuntimeSupervisorError> {
        self.ensure_healthy()?;
        let launcher = self.launcher.lock().await;
        launcher
            .as_ref()
            .map(LocalLauncher::worker_lifecycle)
            .ok_or_else(stopped_error)
    }

    /// Stops the driver, drains the Worker/ACK queue, and releases the CP lease.
    ///
    /// # Errors
    ///
    /// Returns an error when the driver or local launcher does not stop cleanly.
    pub async fn shutdown(mut self) -> Result<(), RuntimeSupervisorError> {
        let _ = self.stop.send(true);
        let driver_error = if let Some(driver) = self.driver.take() {
            driver.await.err().map(|_| {
                RuntimeSupervisorError::new(
                    RuntimeSupervisorErrorKind::Driver,
                    "runtime supervisor driver did not stop cleanly",
                )
            })
        } else {
            None
        };

        // Take the launcher before awaiting its shutdown so the async mutex is
        // never held across Worker/Core cleanup. Cleanup still runs when the
        // driver join failed, preserving the single owned resource boundary.
        let launcher = self.launcher.lock().await.take();
        let now = self.clock.now_instant();
        let now_millis = self.clock.now_millis();
        let launcher_error = match launcher {
            Some(mut launcher) => launcher.shutdown(now, now_millis).await.err().map(|_| {
                RuntimeSupervisorError::new(
                    RuntimeSupervisorErrorKind::Shutdown,
                    "local runtime launcher did not shut down cleanly",
                )
            }),
            None => None,
        };

        if let Some(error) = driver_error.or(launcher_error) {
            self.health.state.mark_faulted();
            return Err(error);
        }
        if !self.health.is_healthy() {
            return Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Faulted,
                "local runtime supervisor was faulted before shutdown",
            ));
        }
        self.health.state.mark_stopped();
        Ok(())
    }

    fn ensure_healthy(&self) -> Result<(), RuntimeSupervisorError> {
        if self.health.is_healthy() {
            Ok(())
        } else {
            Err(RuntimeSupervisorError::new(
                RuntimeSupervisorErrorKind::Faulted,
                "local runtime supervisor is faulted",
            ))
        }
    }

    fn mark_faulted(&self, kind: RuntimeSupervisorErrorKind) -> RuntimeSupervisorError {
        self.health.state.mark_faulted();
        RuntimeSupervisorError::new(kind, "local runtime operation failed")
    }
}

#[cfg(feature = "local-worker")]
async fn run_driver<Core, Codex>(
    launcher: Arc<Mutex<Option<LocalLauncher<Core, Codex>>>>,
    health: RuntimeHealthState,
    mut stop: watch::Receiver<bool>,
    clock: Arc<dyn StandaloneApplicationClock>,
    tick: Duration,
    mut scheduler: Option<Box<dyn LocalRuntimeScheduler>>,
) where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send + 'static,
    Codex: CodexCoreAdapter + Send + 'static,
{
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
            }
            () = tokio::time::sleep(tick) => {
                let mut guard = launcher.lock().await;
                let Some(launcher) = guard.as_mut() else {
                    return;
                };
                if launcher.reset_trace().is_err() {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("runtime driver reset trace failed");
                    }
                    health.mark_faulted();
                    return;
                }
                let heartbeat_now = clock.now_instant();
                // Publish the Worker capacity observation before processing
                // any durable commands from the preceding tick.  Heartbeat
                // is replay-safe, so it is the first durable operation on
                // every tick and keeps liveness visible while the launcher
                // drains its control queue.
                if let Err(error) = launcher.heartbeat(heartbeat_now.clone()).await {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("runtime driver heartbeat failed: {error}");
                    }
                    health.mark_faulted();
                    return;
                }
                if let Err(error) = launcher.drive(heartbeat_now.clone()).await {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("runtime driver control drive failed: {error}");
                    }
                    health.mark_faulted();
                    return;
                }
                // Poll the Codex adapter before making the next durable
                // scheduling decision.  `poll_codex` removes terminal jobs,
                // closes their Worker slots, and synchronously commits the
                // terminal outcome through the same Core.  This snapshot is
                // therefore the point at which a just-finished job becomes
                // eligible for the next claim.
                let active_jobs_before_poll = launcher.active_job_count();
                if let Err(error) = launcher.poll_codex(clock.now_instant()).await {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("runtime driver Codex poll failed: {error}");
                    }
                    health.mark_faulted();
                    return;
                }
                // Keep the regular one-heartbeat-per-tick cadence when no
                // capacity changed, but immediately publish a fresh Worker
                // capacity observation after terminal polling.  The scheduler
                // below must not wait another tick to see the slot released.
                if launcher.active_job_count() != active_jobs_before_poll
                    && let Err(error) = launcher.heartbeat(clock.now_instant()).await
                {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("runtime driver post-poll heartbeat failed: {error}");
                    }
                    health.mark_faulted();
                    return;
                }
                let scheduler_now = clock.now_instant();
                if let Some(scheduler) = scheduler.as_mut()
                    && let Err(error) = scheduler.drive(scheduler_now.clone(), &launcher.execution_port())
                {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("runtime driver scheduler failed: {error}");
                    }
                    health.mark_faulted();
                    return;
                }
                // Scheduler dispatches are queued on the same local port.  A
                // second drive applies the exact durable command before the
                // next tick's Codex poll, preserving command ordering.
                if let Err(error) = launcher.drive(scheduler_now.clone()).await {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("runtime driver post-scheduler control drive failed: {error}");
                    }
                    health.mark_faulted();
                    return;
                }
                if let Some(scheduler) = scheduler.as_mut()
                    && let Err(error) = scheduler.complete(scheduler_now.clone())
                {
                    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                        eprintln!("runtime driver interaction acknowledgement failed: {error}");
                    }
                    health.mark_faulted();
                    return;
                }
            }
        }
    }
}

#[cfg(feature = "local-worker")]
fn stopped_error() -> RuntimeSupervisorError {
    RuntimeSupervisorError::new(
        RuntimeSupervisorErrorKind::Shutdown,
        "local runtime supervisor is stopped",
    )
}

fn scheduler_failure() -> RuntimeSupervisorError {
    RuntimeSupervisorError::new(
        RuntimeSupervisorErrorKind::Driver,
        "repository runtime scheduler could not advance durable work",
    )
}

fn debug_scheduler_error(step: &str, error: &impl fmt::Debug) {
    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
        eprintln!("runtime scheduler {step} error: {error:?}");
    }
}

pub(crate) fn crockford_26(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut value = [0_u8; 26];
    for (index, slot) in value.iter_mut().enumerate() {
        let source = u16::from(bytes[index % bytes.len()]);
        let shift = (index % 5) * 2;
        *slot = ALPHABET[usize::from((source >> shift) & 0x1f)];
    }
    String::from_utf8(value.to_vec()).expect("Crockford alphabet is valid UTF-8")
}

fn add_millis(now: &Instant, delta: u64) -> Option<Instant> {
    let bytes = now.0.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return None;
    }
    let year = decimal(bytes, 0, 4)?;
    let month = decimal(bytes, 5, 2)?;
    let day = decimal(bytes, 8, 2)?;
    let hour = decimal(bytes, 11, 2)?;
    let minute = decimal(bytes, 14, 2)?;
    let second = decimal(bytes, 17, 2)?;
    let millis = decimal(bytes, 20, 3)?;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = yoe * 365 + yoe / 4 - yoe / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let base = u64::try_from(days)
        .ok()?
        .checked_mul(86_400_000)?
        .checked_add(u64::try_from(hour.checked_mul(3_600_000)?).ok()?)?
        .checked_add(u64::try_from(minute.checked_mul(60_000)?).ok()?)?
        .checked_add(u64::try_from(second.checked_mul(1_000)?).ok()?)?
        .checked_add(u64::try_from(millis).ok()?)?;
    millis_to_instant(base.checked_add(delta)?)
}

fn decimal(bytes: &[u8], start: usize, width: usize) -> Option<i64> {
    bytes
        .get(start..start + width)?
        .iter()
        .try_fold(0_i64, |value, byte| {
            let digit = i64::from(byte.checked_sub(b'0')?);
            if digit > 9 {
                return None;
            }
            value.checked_mul(10)?.checked_add(digit)
        })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn millis_to_instant(value: u64) -> Option<Instant> {
    let value = value.min(253_402_300_799_999);
    let seconds = value / 1_000;
    let millis = value % 1_000;
    let days = i64::try_from(seconds / 86_400).ok()?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Some(Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_prime = (5 * doy + 2) / 153;
    let day = doy - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DurableEventHubConfig;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    use winwincode_domain::{
        OrganizationId, ProductSessionId, ProjectId, RepositoryId, Revision, Sha256Digest,
        UserAccount, UserAccountRole, UserAccountState, WorkspaceId,
    };
    use winwincode_execution_port::generated::{
        ExecutionJob, ExecutionLimits, ExecutionScope, ExecutionWorkspace,
        ExecutionWorkspaceWriteMode, ProductSessionExecutionScope,
        ProductSessionExecutionScopeKind,
    };
    use winwincode_storage::{
        ExecutionJobState, ExecutionQueueScope, ExecutionReservationState, SqliteStorage,
    };

    const OWNER_USER_ID: &str = "usr_A0A0A0A0A0A0A0A0A0A0A0A0A0";
    const FOREIGN_USER_ID: &str = "usr_ZZZZZZZZZZZZZZZZZZZZZZZZZZ";

    fn unique_directory(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        std::env::temp_dir().join(format!(
            "winwincode-runtime-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    fn fixed_instant(text: &str) -> Instant {
        Instant(text.to_owned())
    }

    fn fixed_id(prefix: &str) -> String {
        format!("{prefix}{}", "A".repeat(26))
    }

    fn test_scope() -> ExecutionQueueScope {
        ExecutionQueueScope {
            organization_id: OrganizationId(fixed_id("org_")),
            workspace_id: WorkspaceId(fixed_id("wsp_")),
            project_id: ProjectId(fixed_id("prj_")),
            repository_id: RepositoryId(fixed_id("rep_")),
            product_session_id: ProductSessionId(fixed_id("psn_")),
            delivery_id: None,
        }
    }

    fn scheduler_with_user(
        user_id: UserId,
        directory: &std::path::Path,
    ) -> RepositoryRuntimeScheduler {
        RepositoryRuntimeScheduler {
            state: Arc::new(StdMutex::new(None)),
            hub: Arc::new(
                crate::DurableEventHub::open(
                    directory.join("event-hub"),
                    DurableEventHubConfig::default(),
                )
                .expect("event hub opens"),
            ),
            scope: RepositorySchedulerScope {
                organization_id: OrganizationId(fixed_id("org_")),
                workspace_id: WorkspaceId(fixed_id("wsp_")),
                project_id: ProjectId(fixed_id("prj_")),
                repository_id: RepositoryId(fixed_id("rep_")),
            },
            worker_id: WorkerId(fixed_id("wrk_")),
            worker_instance_id: WorkerInstanceId(fixed_id("wki_")),
            user_id,
            worker_pool_id: WorkerPoolId(DEFAULT_RUNTIME_WORKER_POOL_ID.to_owned()),
            scheduler_generation: fixed_id("gen_"),
            lease_duration_millis: 30_000,
            sequence: 0,
            active_worker_authorities: Vec::new(),
            pending_interaction_acknowledgements: Vec::new(),
        }
    }

    fn queued_job_record(scope: &ExecutionQueueScope) -> (ExecutionJobRecord, ExecutionJob) {
        let job = ExecutionJob {
            attempt: 0,
            execution_profile: "codex-chat".to_owned(),
            goal: "admission regression fixture".to_owned(),
            job_id: ExecutionJobId(fixed_id("job_")),
            limits: ExecutionLimits {
                deadline_at: fixed_instant("2026-09-07T00:00:00.000Z"),
                max_artifact_bytes: 1_024,
                max_runtime_seconds: 3_600,
            },
            payload_digest: Sha256Digest(format!("sha256:{}", "A".repeat(64))),
            scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
                kind: ProductSessionExecutionScopeKind::ProductSession,
                product_session_id: ProductSessionId(fixed_id("psn_")),
            }),
            work_input: None,
            workspace: ExecutionWorkspace {
                checkout_revision: "04e7640e".to_owned(),
                repository_id: RepositoryId(fixed_id("rep_")),
                write_mode: ExecutionWorkspaceWriteMode::ReadOnly,
            },
        };
        let dispatch_payload = serde_json::to_vec(&job).expect("canonical job serialization");
        let record = ExecutionJobRecord {
            scope: scope.clone(),
            job_id: job.job_id.clone(),
            submission_request_id: RequestId(format!("req_{}", "A".repeat(26))),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload,
            state: ExecutionJobState::Queued,
            attempt: 0,
            revision: 1,
            dependencies: Vec::new(),
            work_run_id: None,
            submitted_at: fixed_instant("2026-09-06T00:00:00.000Z"),
            updated_at: fixed_instant("2026-09-06T00:00:00.000Z"),
            cancellation: None,
        };
        (record, job)
    }

    fn poison_reservation_with_user(
        storage: &mut SqliteStorage,
        scope: &ExecutionQueueScope,
        job_id: &ExecutionJobId,
        user_id: &UserId,
    ) {
        let mut admission = storage.execution_admission().expect("admission opens");
        for boundary in admission_boundaries(
            scope,
            &WorkerPoolId(DEFAULT_RUNTIME_WORKER_POOL_ID.to_owned()),
        ) {
            admission
                .configure_policy(&ExecutionAdmissionPolicy {
                    boundary,
                    limits: LOCAL_ADMISSION_LIMITS,
                })
                .expect("admission policy configures");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: scope.clone(),
                user_id: user_id.clone(),
                worker_pool_id: WorkerPoolId(DEFAULT_RUNTIME_WORKER_POOL_ID.to_owned()),
                job_id: job_id.clone(),
                request_id: RequestId(format!("req_{}", "B".repeat(26))),
                repository_access: ExecutionRepositoryAccess::ReadOnly,
                reserved_tokens: 1_000,
                reserved_cost_microunits: 1_000,
                runtime_limit_millis: 3_600_000,
                submitted_at: fixed_instant("2026-09-06T00:00:00.000Z"),
            })
            .expect("stale reservation is created");
    }

    fn create_active_owner(storage: &mut SqliteStorage) -> UserId {
        let user_id = UserId(OWNER_USER_ID.to_owned());
        let account = UserAccount {
            user_id: user_id.clone(),
            username: "nb4-owner".to_owned(),
            normalized_username: "nb4-owner".to_owned(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$fixture$fixture".to_owned(),
            role: UserAccountRole::Owner,
            state: UserAccountState::Active,
            created_at: fixed_instant("2026-09-06T00:00:00.000Z"),
            updated_at: fixed_instant("2026-09-06T00:00:00.000Z"),
            revision: Revision(1),
        };
        storage
            .user_account_ledger()
            .and_then(|mut ledger| ledger.create(&account))
            .expect("owner account is created");
        user_id
    }

    #[test]
    fn uninitialized_server_defers_admission_without_reserving() {
        let directory = unique_directory("defer");
        std::fs::create_dir_all(&directory).expect("temp directory");
        let mut storage = SqliteStorage::open(&directory).expect("storage opens");
        let scope = test_scope();
        let (record, _) = queued_job_record(&scope);
        let mut scheduler =
            scheduler_with_user(UserId(DEFAULT_RUNTIME_USER_ID.to_owned()), &directory);
        scheduler.refresh_admission_identity(&mut storage);

        let ready = scheduler
            .ensure_admission(
                &mut storage,
                &record,
                &fixed_instant("2026-09-06T12:00:00.000Z"),
            )
            .expect("ensure_admission defers without faulting");

        assert!(!ready, "an uninitialized Server defers the queued job");
        let admission = storage.execution_admission().expect("admission reopens");
        assert!(
            admission
                .load_reservation_by_job(&record.job_id)
                .expect("reservation lookup succeeds")
                .is_none(),
            "no reservation may be created under the stable default subject"
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn restart_with_stale_default_subject_reservation_becomes_admissible() {
        let directory = unique_directory("adopt");
        std::fs::create_dir_all(&directory).expect("temp directory");
        let mut storage = SqliteStorage::open(&directory).expect("storage opens");
        let scope = test_scope();
        let (record, _) = queued_job_record(&scope);
        let default_user = UserId(DEFAULT_RUNTIME_USER_ID.to_owned());
        poison_reservation_with_user(&mut storage, &scope, &record.job_id, &default_user);
        // The restarted Server resolves a durable Owner at startup (the
        // browser bootstrap ran before the previous shutdown).
        create_active_owner(&mut storage);

        let mut scheduler =
            scheduler_with_user(UserId(DEFAULT_RUNTIME_USER_ID.to_owned()), &directory);
        scheduler.refresh_admission_identity(&mut storage);
        assert_eq!(
            scheduler.user_id.0, OWNER_USER_ID,
            "the durable active Owner is adopted as the admission identity"
        );

        let ready = scheduler
            .ensure_admission(
                &mut storage,
                &record,
                &fixed_instant("2026-09-06T12:00:00.000Z"),
            )
            .expect("the stale local reservation is admitted without faulting");
        assert!(ready, "the queued job becomes schedulable again");

        let admission = storage.execution_admission().expect("admission reopens");
        let reservation = admission
            .load_reservation_by_job(&record.job_id)
            .expect("reservation lookup succeeds")
            .expect("the reservation still exists");
        assert_eq!(reservation.state, ExecutionReservationState::Running);
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn fresh_reservations_carry_the_adopted_owner_identity() {
        let directory = unique_directory("fresh");
        std::fs::create_dir_all(&directory).expect("temp directory");
        let mut storage = SqliteStorage::open(&directory).expect("storage opens");
        let owner = create_active_owner(&mut storage);
        let scope = test_scope();
        let (record, _) = queued_job_record(&scope);
        let mut scheduler =
            scheduler_with_user(UserId(DEFAULT_RUNTIME_USER_ID.to_owned()), &directory);
        scheduler.refresh_admission_identity(&mut storage);
        assert_eq!(scheduler.user_id, owner);

        let ready = scheduler
            .ensure_admission(
                &mut storage,
                &record,
                &fixed_instant("2026-09-06T12:00:00.000Z"),
            )
            .expect("ensure_admission succeeds");
        assert!(ready);

        let admission = storage.execution_admission().expect("admission reopens");
        let reservation = admission
            .load_reservation_by_job(&record.job_id)
            .expect("reservation lookup succeeds")
            .expect("the reservation was created");
        assert_eq!(reservation.user_id, owner);
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn existing_device_reservation_keeps_its_worker_pool_policy() {
        let directory = unique_directory("device-reservation");
        std::fs::create_dir_all(&directory).expect("temp directory");
        let mut storage = SqliteStorage::open(&directory).expect("storage opens");
        let owner = create_active_owner(&mut storage);
        let scope = test_scope();
        let (record, _) = queued_job_record(&scope);
        let worker_pool_id =
            WorkerPoolId(winwincode_control_plane::STRONGFLOW_DEVICE_WORKER_POOL_ID.to_owned());
        {
            let mut admission = storage.execution_admission().expect("admission opens");
            for boundary in admission_boundaries(&scope, &worker_pool_id) {
                let limits = if matches!(boundary, ExecutionAdmissionBoundary::WorkerPool { .. }) {
                    ExecutionAdmissionLimits {
                        max_concurrent: 4,
                        ..LOCAL_ADMISSION_LIMITS
                    }
                } else {
                    LOCAL_ADMISSION_LIMITS
                };
                admission
                    .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                    .expect("device admission policy configures");
            }
            admission
                .reserve(&ExecutionReservationRequest {
                    scope: scope.clone(),
                    user_id: owner.clone(),
                    worker_pool_id: worker_pool_id.clone(),
                    job_id: record.job_id.clone(),
                    request_id: RequestId(format!("req_{}", "D".repeat(26))),
                    repository_access: ExecutionRepositoryAccess::ReadOnly,
                    reserved_tokens: 1_000,
                    reserved_cost_microunits: 1_000,
                    runtime_limit_millis: 3_600_000,
                    submitted_at: record.submitted_at.clone(),
                })
                .expect("device reservation is created");
        }

        let mut scheduler = scheduler_with_user(owner, &directory);
        scheduler.worker_pool_id = worker_pool_id;
        assert!(
            scheduler
                .ensure_admission(
                    &mut storage,
                    &record,
                    &fixed_instant("2026-09-06T12:00:00.000Z"),
                )
                .expect("existing device reservation is admitted")
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn inactive_foreign_reservation_subject_still_fails_closed() {
        let directory = unique_directory("foreign");
        std::fs::create_dir_all(&directory).expect("temp directory");
        let mut storage = SqliteStorage::open(&directory).expect("storage opens");
        let owner = create_active_owner(&mut storage);
        let scope = test_scope();
        let (record, _) = queued_job_record(&scope);
        poison_reservation_with_user(
            &mut storage,
            &scope,
            &record.job_id,
            &UserId(FOREIGN_USER_ID.to_owned()),
        );

        let scheduler = scheduler_with_user(owner, &directory);
        let error = scheduler
            .ensure_admission(
                &mut storage,
                &record,
                &fixed_instant("2026-09-06T12:00:00.000Z"),
            )
            .expect_err("a foreign inactive reservation subject still fails closed");
        assert!(matches!(error.kind(), RuntimeSupervisorErrorKind::Driver));
        std::fs::remove_dir_all(&directory).ok();
    }
}
