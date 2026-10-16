// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use winwincode_delivery::application::workrun_execution::SessionBindingAuthority;
use winwincode_delivery::domain::{Delivery, SessionBindingSourceKind};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, Instant, RequestId, SchemaVersion, SessionIdentity,
    WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ActionEnforcementReceiptMessage, ActionEnforcementRequestMessage,
    ActiveLeaseSummary as WireActiveLeaseSummary, ExecutionJob, ExecutionLeaseStamp,
    ExecutionPortError, ExecutionPortErrorCode, ExecutionPortMessage, ExecutionScope,
    JobDispatchMessage, JobDispatchMessageKind, JobDispatchResultMessage,
    JobDispatchResultMessageKind, JobDispatchResultMessageStatus, RuntimeAckMessage,
    RuntimeEventMessage, RuntimeReplayRequestMessage, RuntimeReplayRequestMessageKind,
    WorkerHeartbeatAckMessage, WorkerHeartbeatAckMessageKind, WorkerHeartbeatAckMessageStatus,
    WorkerHeartbeatMessage, WorkerRegisterMessage, WorkerRegistrationResultMessage,
    WorkerRegistrationResultMessageKind, WorkerRegistrationResultMessageLeaseRecovery,
    WorkerRegistrationResultMessageStatus,
};
use winwincode_execution_port::transport::{ExecutionPortCore, FrameDirection, TypedFrame};
use winwincode_storage::{
    ActiveLeaseSummary, AuthenticatedWorkerPlacement, DispatchResultError, DispatchResultErrorCode,
    DispatchResultStatus, EXECUTION_PROTOCOL_VERSION, ExecutionLeaseClaim, ExecutionLeaseRecord,
    ExecutionRegistry, LeaseRecovery, LeaseWriteStatus, ProductStateStorage, SqliteStorage,
    StorageError, WorkerAuthenticationIdentity, WorkerHeartbeatReceipt, WorkerHeartbeatRequest,
    WorkerPlatform, WorkerPoolId, WorkerRegistrationErrorCode, WorkerRegistrationReceipt,
    WorkerRegistrationRequest, WorkerRegistrationStatus, WorkerRegistryScope,
};

use crate::delivery_transaction::{delivery_stream_id, load_durable_execution_job};
use crate::runtime_event_transaction::runtime_ack_sequence_for_replay;
use crate::{
    ControlPlane, DurableWorkerExecutionLifecycle, RepositoryExecutionScheduler,
    RepositoryExecutionSchedulerError, WorkerExecutionLifecycleError,
};

pub const DEFAULT_HEARTBEAT_INTERVAL_MS: i64 = 5_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeReplayRequestCommand {
    pub job_id: ExecutionJobId,
    pub max_events: i64,
    pub message_id: ExecutionMessageId,
    pub request_id: RequestId,
    pub sent_at: Instant,
}

#[derive(Debug)]
pub enum ExecutionPortServiceError {
    Storage(StorageError),
    Protocol(&'static str),
    JobMismatch(&'static str),
    AuthorityRejected(&'static str),
    ClaimRejected(LeaseWriteStatus),
    WorkerLifecycle(WorkerExecutionLifecycleError),
    ActionPolicy(crate::ActionPolicyEnforcementError),
    UnsupportedMessage,
}

impl fmt::Display for ExecutionPortServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "ExecutionPort storage error: {error}"),
            Self::Protocol(field) => write!(formatter, "invalid ExecutionPort message: {field}"),
            Self::JobMismatch(field) => {
                write!(
                    formatter,
                    "ExecutionJob and lease claim do not match: {field}"
                )
            }
            Self::AuthorityRejected(field) => {
                write!(formatter, "ExecutionPort authority rejected: {field}")
            }
            Self::ClaimRejected(status) => {
                write!(formatter, "execution lease claim rejected: {status:?}")
            }
            Self::WorkerLifecycle(error) => write!(formatter, "{error}"),
            Self::ActionPolicy(error) => write!(formatter, "{error}"),
            Self::UnsupportedMessage => {
                formatter.write_str("ExecutionPort message is not handled by the Worker service")
            }
        }
    }
}

impl std::error::Error for ExecutionPortServiceError {}

impl From<StorageError> for ExecutionPortServiceError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

pub struct ExecutionPortService<'storage> {
    storage: &'storage mut SqliteStorage,
    server_time: Instant,
    heartbeat_interval_ms: i64,
}

impl<'storage> ExecutionPortService<'storage> {
    #[must_use = "use the configured ExecutionPort service"]
    pub fn new(storage: &'storage mut SqliteStorage, server_time: Instant) -> Self {
        Self {
            storage,
            server_time,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
        }
    }

    #[must_use = "use the configured ExecutionPort service"]
    pub fn with_heartbeat_interval(
        storage: &'storage mut SqliteStorage,
        server_time: Instant,
        heartbeat_interval_ms: i64,
    ) -> Result<Self, ExecutionPortServiceError> {
        if !(1_000..=300_000).contains(&heartbeat_interval_ms) {
            return Err(ExecutionPortServiceError::Protocol("heartbeatIntervalMs"));
        }
        Ok(Self {
            storage,
            server_time,
            heartbeat_interval_ms,
        })
    }

    fn registry(&mut self) -> Result<ExecutionRegistry<'_>, StorageError> {
        self.storage.execution_registry()
    }

    pub fn handle_with_action_enforcement(
        &mut self,
        message: ExecutionPortMessage,
    ) -> Result<ExecutionPortMessage, ExecutionPortServiceError> {
        match message {
            ExecutionPortMessage::ActionEnforcementRequestMessage(request) => self
                .enforce_action(&request)
                .map(ExecutionPortMessage::ActionEnforcementReceiptMessage),
            other => self.handle(other),
        }
    }

    pub fn enforce_action(
        &mut self,
        request: &ActionEnforcementRequestMessage,
    ) -> Result<ActionEnforcementReceiptMessage, ExecutionPortServiceError> {
        crate::issue_action_enforcement_receipt(self.storage, &self.server_time, request)
            .map_err(ExecutionPortServiceError::ActionPolicy)
    }

    pub fn handle(
        &mut self,
        message: ExecutionPortMessage,
    ) -> Result<ExecutionPortMessage, ExecutionPortServiceError> {
        match message {
            ExecutionPortMessage::WorkerRegisterMessage(message) => self
                .register_worker(&message)
                .map(ExecutionPortMessage::WorkerRegistrationResultMessage),
            ExecutionPortMessage::WorkerHeartbeatMessage(message) => self
                .record_heartbeat(&message)
                .map(ExecutionPortMessage::WorkerHeartbeatAckMessage),
            ExecutionPortMessage::JobDispatchResultMessage(message) => self
                .accept_dispatch_result(message)
                .map(ExecutionPortMessage::JobDispatchResultMessage),
            _ => Err(ExecutionPortServiceError::UnsupportedMessage),
        }
    }

    pub fn register_worker(
        &mut self,
        message: &WorkerRegisterMessage,
    ) -> Result<WorkerRegistrationResultMessage, ExecutionPortServiceError> {
        let request = worker_registration_request(
            message,
            WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "embedded-control-plane".to_owned(),
            },
            "local".to_owned(),
        )?;
        let receipt = self.registry()?.register_worker(&request)?;
        Ok(registration_response(
            message,
            receipt,
            &self.server_time,
            self.heartbeat_interval_ms,
        ))
    }

    pub fn register_local_worker_for_scope(
        &mut self,
        message: &WorkerRegisterMessage,
        scope: &RepositoryScope,
    ) -> Result<WorkerRegistrationResultMessage, ExecutionPortServiceError> {
        let request = worker_registration_request(
            message,
            WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "embedded-control-plane".to_owned(),
            },
            "local".to_owned(),
        )?;
        let scope = WorkerRegistryScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        };
        let receipt = self
            .registry()?
            .register_worker_for_scope(&request, &scope)?;
        Ok(registration_response(
            message,
            receipt,
            &self.server_time,
            self.heartbeat_interval_ms,
        ))
    }

    pub(crate) fn register_authenticated_remote_worker(
        &mut self,
        message: &WorkerRegisterMessage,
        authentication_identity: WorkerAuthenticationIdentity,
        scope: &WorkerRegistryScope,
        worker_pool_id: &WorkerPoolId,
        security_zone: String,
    ) -> Result<WorkerRegistrationResultMessage, ExecutionPortServiceError> {
        let request = worker_registration_request(message, authentication_identity, security_zone)?;
        let receipt = self
            .registry()?
            .register_worker_for_scope(&request, scope)?;
        if matches!(
            receipt.status,
            WorkerRegistrationStatus::Accepted | WorkerRegistrationStatus::Duplicate
        ) {
            let placed_at = if receipt.status == WorkerRegistrationStatus::Duplicate {
                self.registry()?
                    .load_authenticated_worker_placement(
                        &receipt.worker.worker_id,
                        &receipt.worker.worker_instance_id,
                    )?
                    .map_or_else(|| self.server_time.clone(), |placement| placement.placed_at)
            } else {
                self.server_time.clone()
            };
            self.registry()?.record_authenticated_worker_placement(
                &AuthenticatedWorkerPlacement {
                    worker_id: receipt.worker.worker_id.clone(),
                    worker_instance_id: receipt.worker.worker_instance_id.clone(),
                    worker_pool_id: worker_pool_id.clone(),
                    management_scope: receipt.worker.management_scope.clone(),
                    authentication_identity: receipt.worker.authentication_identity.clone(),
                    registration_request_id: request.request_id.clone(),
                    placed_at,
                },
            )?;
        }
        Ok(registration_response(
            message,
            receipt,
            &self.server_time,
            self.heartbeat_interval_ms,
        ))
    }

    pub fn record_heartbeat(
        &mut self,
        message: &WorkerHeartbeatMessage,
    ) -> Result<WorkerHeartbeatAckMessage, ExecutionPortServiceError> {
        validate_worker_heartbeat(message)?;
        let active_leases = message
            .active_leases
            .iter()
            .map(active_lease_summary)
            .collect::<Result<Vec<_>, _>>()?;
        let running_jobs = u64::try_from(message.capacity.running_jobs)
            .map_err(|_| ExecutionPortServiceError::Protocol("capacity.runningJobs"))?;
        let available_slots = u64::try_from(message.capacity.available_slots)
            .map_err(|_| ExecutionPortServiceError::Protocol("capacity.availableSlots"))?;
        let max_slots = running_jobs
            .checked_add(available_slots)
            .ok_or(ExecutionPortServiceError::Protocol("capacity"))?;
        let request = WorkerHeartbeatRequest {
            active_leases,
            available_slots,
            heartbeat_sequence: message.heartbeat_sequence.clone(),
            max_slots,
            running_slots: running_jobs,
            message_id: message.message_id.clone(),
            observed_at: message.observed_at.clone(),
            sent_at: message.sent_at.clone(),
            worker_id: message.worker_id.clone(),
            worker_instance_id: message.worker_instance_id.clone(),
        };
        let receipt = self.registry()?.record_heartbeat(&request)?;
        Ok(heartbeat_response(
            message,
            &receipt,
            &self.server_time,
            self.heartbeat_interval_ms,
        ))
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "claim ownership is consumed after the durable identity join"
    )]
    pub fn claim_execution_job(
        &mut self,
        job: ExecutionJob,
        claim: ExecutionLeaseClaim,
    ) -> Result<JobDispatchMessage, ExecutionPortServiceError> {
        validate_job_claim(&job, &claim)?;
        let (_durable_event, durable_job) =
            load_durable_execution_job(self.storage, &claim.job_id)?;
        if durable_job.job_id != job.job_id {
            return Err(ExecutionPortServiceError::JobMismatch("jobId"));
        }
        if durable_job.attempt != job.attempt {
            return Err(ExecutionPortServiceError::JobMismatch("attempt"));
        }
        if durable_job.payload_digest != job.payload_digest {
            return Err(ExecutionPortServiceError::JobMismatch("payloadDigest"));
        }
        if durable_job.scope != job.scope {
            return Err(ExecutionPortServiceError::JobMismatch("scope"));
        }
        if durable_job != job {
            return Err(ExecutionPortServiceError::JobMismatch(
                "durableExecutionJob",
            ));
        }
        self.registry()?.load_worker(&claim.worker_id)?.ok_or(
            ExecutionPortServiceError::AuthorityRejected("Worker registration is missing"),
        )?;
        let authenticated_remote = self
            .registry()?
            .load_authenticated_worker_placement(&claim.worker_id, &claim.worker_instance_id)?
            .is_some();
        let receipt = if authenticated_remote {
            let data_directory = self
                .storage
                .database_path()
                .parent()
                .ok_or(ExecutionPortServiceError::Protocol("storage.databasePath"))?;
            DurableWorkerExecutionLifecycle::open(data_directory)
                .map_err(ExecutionPortServiceError::WorkerLifecycle)?
                .claim(&claim)
                .map_err(ExecutionPortServiceError::WorkerLifecycle)?
        } else {
            self.registry()?.claim_execution_job(&claim)?
        };
        let status = receipt.status;
        let Some(lease) = receipt.lease else {
            return Err(ExecutionPortServiceError::ClaimRejected(status));
        };
        if !matches!(
            status,
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
        ) {
            return Err(ExecutionPortServiceError::ClaimRejected(status));
        }
        Ok(JobDispatchMessage {
            job: durable_job,
            kind: JobDispatchMessageKind::JobDispatch,
            lease: lease_stamp(&lease),
            message_id: claim.message_id,
            replacement_authority: None,
            request_id: claim.request_id,
            schema_version: SchemaVersion::WinwincodeV1,
            snapshot_id: None,
            sent_at: lease.issued_at.clone(),
        })
    }

    pub fn build_runtime_replay_request(
        &mut self,
        command: RuntimeReplayRequestCommand,
    ) -> Result<TypedFrame, ExecutionPortServiceError> {
        validate_runtime_replay_command(&command)?;
        let (durable_job_event, job) = load_durable_execution_job(self.storage, &command.job_id)?;
        let authority = load_runtime_replay_authority(self.storage, &job)?;
        if command.sent_at.0 < authority.lease.issued_at.0 {
            return Err(ExecutionPortServiceError::AuthorityRejected(
                "replay command time precedes the original execution",
            ));
        }
        let after_sequence = runtime_ack_sequence_for_replay(
            self.storage,
            durable_job_event.receipt_identity().scope_key(),
            &job,
        )?;
        let request = RuntimeReplayRequestMessage {
            after_sequence,
            kind: RuntimeReplayRequestMessageKind::RuntimeReplayRequest,
            lease: lease_stamp(&authority.lease),
            max_events: command.max_events,
            message_id: command.message_id,
            request_id: command.request_id,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: command.sent_at,
            session_identity: authority.session_identity,
            worker_session_id: authority.worker_session_id,
        };
        TypedFrame::new(
            FrameDirection::ControlPlaneToWorker,
            ExecutionPortMessage::RuntimeReplayRequestMessage(request),
        )
        .map_err(|_| ExecutionPortServiceError::Protocol("runtime.replay_request"))
    }

    pub fn accept_dispatch_result(
        &mut self,
        mut message: JobDispatchResultMessage,
    ) -> Result<JobDispatchResultMessage, ExecutionPortServiceError> {
        validate_dispatch_result(&message)?;
        let receipt = RepositoryExecutionScheduler::new(self.storage)
            .record_dispatch_result_for_job(&message, &self.server_time)
            .map_err(repository_scheduler_error)?;
        message.status = wire_dispatch_result_status(receipt.dispatch.status);
        message.error = receipt.dispatch.error.map(dispatch_result_error);
        Ok(message)
    }
}

impl ExecutionPortCore for ExecutionPortService<'_> {
    type Error = ExecutionPortServiceError;
    type Output = ExecutionPortMessage;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        self.handle(message.clone())
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeEventRoute {
    scope: RepositoryScope,
    authority: SessionBindingAuthority,
}

impl RuntimeEventRoute {
    #[must_use]
    pub fn from_sealed_scheduler(
        scope: RepositoryScope,
        authority: SessionBindingAuthority,
    ) -> Self {
        Self { scope, authority }
    }

    #[must_use]
    pub const fn scope(&self) -> &RepositoryScope {
        &self.scope
    }

    #[must_use]
    pub const fn authority(&self) -> &SessionBindingAuthority {
        &self.authority
    }
}

pub trait RuntimeEventRouteResolver {
    type Error;

    fn resolve(
        &mut self,
        control_plane: &ControlPlane,
        message: &RuntimeEventMessage,
    ) -> Result<RuntimeEventRoute, Self::Error>;
}

impl<F, E> RuntimeEventRouteResolver for F
where
    F: FnMut(&ControlPlane, &RuntimeEventMessage) -> Result<RuntimeEventRoute, E>,
{
    type Error = E;

    fn resolve(
        &mut self,
        control_plane: &ControlPlane,
        message: &RuntimeEventMessage,
    ) -> Result<RuntimeEventRoute, Self::Error> {
        self(control_plane, message)
    }
}

#[derive(Debug)]
pub enum RuntimeEventPortError<ResolverError> {
    Resolution(ResolverError),
    ControlPlane(crate::RuntimeMessageError),
    UnsupportedMessage,
}

impl<ResolverError: fmt::Display> fmt::Display for RuntimeEventPortError<ResolverError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolution(error) => {
                write!(formatter, "runtime route resolution failed: {error}")
            }
            Self::ControlPlane(error) => {
                write!(formatter, "runtime Control Plane ingress failed: {error}")
            }
            Self::UnsupportedMessage => {
                formatter.write_str("ExecutionPort message is not a runtime event")
            }
        }
    }
}

impl<ResolverError: std::error::Error + 'static> std::error::Error
    for RuntimeEventPortError<ResolverError>
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Resolution(error) => Some(error),
            Self::ControlPlane(error) => Some(error),
            Self::UnsupportedMessage => None,
        }
    }
}

pub struct RuntimeEventPortRouter<'control_plane, Resolver> {
    control_plane: &'control_plane mut ControlPlane,
    resolver: Resolver,
    server_time: Instant,
}

impl<'control_plane, Resolver> RuntimeEventPortRouter<'control_plane, Resolver> {
    #[must_use]
    pub fn new(
        control_plane: &'control_plane mut ControlPlane,
        resolver: Resolver,
        server_time: Instant,
    ) -> Self {
        Self {
            control_plane,
            resolver,
            server_time,
        }
    }
}

impl<Resolver> RuntimeEventPortRouter<'_, Resolver>
where
    Resolver: RuntimeEventRouteResolver,
{
    pub fn accept_runtime_event(
        &mut self,
        message: &RuntimeEventMessage,
    ) -> Result<RuntimeAckMessage, RuntimeEventPortError<Resolver::Error>> {
        let route = self
            .resolver
            .resolve(&*self.control_plane, message)
            .map_err(RuntimeEventPortError::Resolution)?;
        self.control_plane
            .accept_runtime_event(route.scope(), message, route.authority(), &self.server_time)
            .map_err(RuntimeEventPortError::ControlPlane)
    }
}

impl<Resolver> ExecutionPortCore for RuntimeEventPortRouter<'_, Resolver>
where
    Resolver: RuntimeEventRouteResolver,
{
    type Error = RuntimeEventPortError<Resolver::Error>;
    type Output = ExecutionPortMessage;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        let ExecutionPortMessage::RuntimeEventMessage(runtime) = message else {
            return Err(RuntimeEventPortError::UnsupportedMessage);
        };
        self.accept_runtime_event(runtime)
            .map(ExecutionPortMessage::RuntimeAckMessage)
    }
}

pub(crate) struct DurableRuntimeReplayAuthority {
    pub(crate) lease: ExecutionLeaseRecord,
    pub(crate) session_identity: SessionIdentity,
    pub(crate) worker_session_id: WorkerSessionId,
}

fn validate_runtime_replay_command(
    command: &RuntimeReplayRequestCommand,
) -> Result<(), ExecutionPortServiceError> {
    if !(1..=10_000).contains(&command.max_events) {
        return Err(ExecutionPortServiceError::Protocol("maxEvents"));
    }
    if !canonical_execution_identifier(&command.message_id.0, "xmsg_") {
        return Err(ExecutionPortServiceError::Protocol("messageId"));
    }
    if !canonical_execution_identifier(&command.request_id.0, "req_") {
        return Err(ExecutionPortServiceError::Protocol("requestId"));
    }
    if command.sent_at.0.is_empty() {
        return Err(ExecutionPortServiceError::Protocol("sentAt"));
    }
    Ok(())
}

pub(crate) fn load_running_runtime_authority(
    storage: &mut SqliteStorage,
    job: &ExecutionJob,
    now: &Instant,
) -> Result<DurableRuntimeReplayAuthority, ExecutionPortServiceError> {
    let authority = load_runtime_authority(storage, job, true)?;
    if now.0 < authority.lease.issued_at.0 || now.0 >= authority.lease.expires_at.0 {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current execution lease is expired",
        ));
    }
    Ok(authority)
}

pub(crate) fn load_runtime_replay_authority(
    storage: &mut SqliteStorage,
    job: &ExecutionJob,
) -> Result<DurableRuntimeReplayAuthority, ExecutionPortServiceError> {
    load_runtime_authority(storage, job, false)
}

#[allow(
    clippy::too_many_lines,
    reason = "the durable identity join stays one fail-closed seam"
)]
fn load_runtime_authority(
    storage: &mut SqliteStorage,
    job: &ExecutionJob,
    require_running: bool,
) -> Result<DurableRuntimeReplayAuthority, ExecutionPortServiceError> {
    let ExecutionScope::WorkRunExecutionScope(job_scope) = &job.scope else {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "runtime replay requires a Delivery-stage ExecutionJob",
        ));
    };
    let attempt = u64::try_from(job.attempt)
        .map_err(|_| ExecutionPortServiceError::AuthorityRejected("job attempt"))?;
    if !(1..=1_000).contains(&attempt) {
        return Err(ExecutionPortServiceError::AuthorityRejected("job attempt"));
    }

    let job_record = storage.load_execution_job_record(&job.job_id)?.ok_or(
        ExecutionPortServiceError::AuthorityRejected("durable ExecutionJob record is missing"),
    )?;
    let delivery_id = job_record.scope.delivery_id.clone().ok_or(
        ExecutionPortServiceError::AuthorityRejected("WorkRun ExecutionJob has no Delivery scope"),
    )?;
    let stream_id = delivery_stream_id(&delivery_id);
    let state =
        storage
            .load_state(&stream_id)?
            .ok_or(ExecutionPortServiceError::AuthorityRejected(
                "current Delivery state is missing",
            ))?;
    if state.stream_id != stream_id {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current Delivery state stream",
        ));
    }
    let delivery = Delivery::decode_json(&state.payload).map_err(|_| {
        ExecutionPortServiceError::AuthorityRejected("current Delivery state is invalid")
    })?;
    if state.revision != delivery.revision() || delivery.id() != &delivery_id {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current Delivery state is stale or foreign",
        ));
    }

    let mut bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            binding.delivery_id == delivery_id
                && binding.work_contract_id == job_scope.work_contract_id
                && binding.work_contract_revision == job_scope.work_contract_revision
                && binding.work_item_id == job_scope.work_item_id
                && binding.work_item_revision == job_scope.work_item_revision
                && binding.work_run_id == job_scope.work_run_id
                && binding.product_session_id == job_scope.product_session_id
                && binding.execution_job_id == job.job_id
        });
    let Some(binding) = bindings.next() else {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current SessionBinding is missing",
        ));
    };
    if bindings.next().is_some() {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current SessionBinding is ambiguous",
        ));
    }

    let mut runs = delivery
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .filter(|run| run.id == job_scope.work_run_id);
    let Some(run) = runs.next() else {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current WorkRun is missing",
        ));
    };
    if runs.next().is_some()
        || run.work_contract_id != job_scope.work_contract_id
        || run.contract_revision != job_scope.work_contract_revision
        || run.work_item_id != job_scope.work_item_id
        || run.work_item_revision != job_scope.work_item_revision
        || run.attempt != i64::try_from(attempt).unwrap_or(-1)
    {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current WorkRun is stale or foreign",
        ));
    }

    let worker_session_id =
        binding
            .worker_session_id
            .clone()
            .ok_or(ExecutionPortServiceError::AuthorityRejected(
                "SessionBinding WorkerSession is pending",
            ))?;
    let codex_thread_id =
        binding
            .codex_thread_id
            .clone()
            .ok_or(ExecutionPortServiceError::AuthorityRejected(
                "SessionBinding CodexThread is pending",
            ))?;
    if require_running && !matches!(run.state, winwincode_domain::WorkRunState::Running) {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current WorkRun is stale or foreign",
        ));
    }
    let worker_id =
        binding
            .worker_id
            .clone()
            .ok_or(ExecutionPortServiceError::AuthorityRejected(
                "SessionBinding worker authority is pending",
            ))?;
    let worker_instance_id =
        binding
            .worker_instance_id
            .clone()
            .ok_or(ExecutionPortServiceError::AuthorityRejected(
                "SessionBinding worker instance authority is pending",
            ))?;
    let binding_lease_id =
        binding
            .lease_id
            .clone()
            .ok_or(ExecutionPortServiceError::AuthorityRejected(
                "SessionBinding lease authority is pending",
            ))?;
    let binding_fencing_token =
        binding
            .fencing_token
            .clone()
            .ok_or(ExecutionPortServiceError::AuthorityRejected(
                "SessionBinding fencing authority is pending",
            ))?;
    if binding.attempt != attempt
        || binding.source_provenance.kind() != SessionBindingSourceKind::ExecutionPort
        || !canonical_execution_identifier(binding.source_provenance.reference(), "xmsg_")
    {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "SessionBinding authority is stale, foreign, or unproven",
        ));
    }

    let lease =
        {
            let registry = ExecutionRegistry::new(storage)?;
            registry.load_lease(&job.job_id)?.ok_or(
                ExecutionPortServiceError::AuthorityRejected("current execution lease is missing"),
            )?
        };
    if lease.job_id != job.job_id
        || lease.payload_digest != job.payload_digest
        || lease.attempt != attempt
        || lease.worker_id != worker_id
        || lease.worker_instance_id != worker_instance_id
        || lease.lease_id != binding_lease_id
        || lease.fencing_token != binding_fencing_token
    {
        return Err(ExecutionPortServiceError::AuthorityRejected(
            "current execution lease is foreign or stale",
        ));
    }

    Ok(DurableRuntimeReplayAuthority {
        lease,
        session_identity: SessionIdentity {
            codex_thread_id,
            product_session_id: job_scope.product_session_id.clone(),
            work_run_id: Some(job_scope.work_run_id.clone()),
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    })
}

fn canonical_execution_identifier(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H'
                            | b'J'..=b'K'
                            | b'M'..=b'N'
                            | b'P'..=b'T'
                            | b'V'..=b'Z'
                    )
            })
    })
}

fn validate_worker_register(
    message: &WorkerRegisterMessage,
) -> Result<(), ExecutionPortServiceError> {
    if message.kind
        != winwincode_execution_port::generated::WorkerRegisterMessageKind::WorkerRegister
    {
        return Err(ExecutionPortServiceError::Protocol("kind"));
    }
    if message.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(ExecutionPortServiceError::Protocol("schemaVersion"));
    }
    if !(1..=1_024).contains(&message.capabilities.max_concurrent_jobs) {
        return Err(ExecutionPortServiceError::Protocol(
            "capabilities.maxConcurrentJobs",
        ));
    }
    Ok(())
}

fn validate_worker_heartbeat(
    message: &WorkerHeartbeatMessage,
) -> Result<(), ExecutionPortServiceError> {
    if message.kind
        != winwincode_execution_port::generated::WorkerHeartbeatMessageKind::WorkerHeartbeat
    {
        return Err(ExecutionPortServiceError::Protocol("kind"));
    }
    if message.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(ExecutionPortServiceError::Protocol("schemaVersion"));
    }
    if message.heartbeat_sequence.0 <= 0 {
        return Err(ExecutionPortServiceError::Protocol("heartbeatSequence"));
    }
    if message.capacity.running_jobs < 0 || message.capacity.available_slots < 0 {
        return Err(ExecutionPortServiceError::Protocol("capacity"));
    }
    Ok(())
}

fn validate_dispatch_result(
    message: &JobDispatchResultMessage,
) -> Result<(), ExecutionPortServiceError> {
    if message.kind != JobDispatchResultMessageKind::JobDispatchResult {
        return Err(ExecutionPortServiceError::Protocol("kind"));
    }
    if message.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(ExecutionPortServiceError::Protocol("schemaVersion"));
    }
    if message.job_id != message.lease.job_id {
        return Err(ExecutionPortServiceError::Protocol("jobId"));
    }
    if message.lease.attempt <= 0 {
        return Err(ExecutionPortServiceError::Protocol("lease.attempt"));
    }
    Ok(())
}

fn validate_job_claim(
    job: &ExecutionJob,
    claim: &ExecutionLeaseClaim,
) -> Result<(), ExecutionPortServiceError> {
    if job.job_id != claim.job_id {
        return Err(ExecutionPortServiceError::JobMismatch("jobId"));
    }
    if job.payload_digest != claim.payload_digest {
        return Err(ExecutionPortServiceError::JobMismatch("payloadDigest"));
    }
    if job.attempt
        != i64::try_from(claim.attempt)
            .map_err(|_| ExecutionPortServiceError::JobMismatch("attempt"))?
    {
        return Err(ExecutionPortServiceError::JobMismatch("attempt"));
    }
    Ok(())
}

fn capability_name(
    feature: &winwincode_execution_port::generated::WorkerCapabilityFeature,
) -> Result<String, ExecutionPortServiceError> {
    serde_json::to_value(feature)
        .map_err(|_| ExecutionPortServiceError::Protocol("capabilities.features"))?
        .as_str()
        .map(str::to_owned)
        .ok_or(ExecutionPortServiceError::Protocol("capabilities.features"))
}

fn worker_platform(
    platform: &winwincode_execution_port::generated::WorkerCapabilitySetPlatform,
) -> WorkerPlatform {
    use winwincode_execution_port::generated::WorkerCapabilitySetPlatform as WirePlatform;

    match platform {
        WirePlatform::Aarch64AppleDarwin => WorkerPlatform::Aarch64AppleDarwin,
        WirePlatform::X8664AppleDarwin => WorkerPlatform::X86_64AppleDarwin,
        WirePlatform::Aarch64UnknownLinuxGnu => WorkerPlatform::Aarch64UnknownLinuxGnu,
        WirePlatform::X8664UnknownLinuxGnu => WorkerPlatform::X86_64UnknownLinuxGnu,
    }
}

fn active_lease_summary(
    summary: &WireActiveLeaseSummary,
) -> Result<ActiveLeaseSummary, ExecutionPortServiceError> {
    let attempt = u64::try_from(summary.attempt)
        .map_err(|_| ExecutionPortServiceError::Protocol("activeLeases.attempt"))?;
    if attempt == 0 || summary.last_event_sequence.0 < 0 {
        return Err(ExecutionPortServiceError::Protocol("activeLeases"));
    }
    Ok(ActiveLeaseSummary {
        job_id: summary.job_id.clone(),
        lease_id: summary.lease_id.clone(),
        attempt,
        fencing_token: summary.fencing_token.clone(),
    })
}

fn worker_registration_request(
    message: &WorkerRegisterMessage,
    authentication_identity: WorkerAuthenticationIdentity,
    security_zone: String,
) -> Result<WorkerRegistrationRequest, ExecutionPortServiceError> {
    validate_worker_register(message)?;
    if message.usage_accounting_version != 2 {
        return Err(ExecutionPortServiceError::Protocol(
            "usageAccountingVersion",
        ));
    }
    let max_slots = u64::try_from(message.capabilities.max_concurrent_jobs)
        .map_err(|_| ExecutionPortServiceError::Protocol("capabilities.maxConcurrentJobs"))?;
    Ok(WorkerRegistrationRequest {
        authentication_identity,
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: worker_platform(&message.capabilities.platform),
        capabilities: message
            .capabilities
            .features
            .iter()
            .map(capability_name)
            .collect::<Result<Vec<_>, _>>()?,
        capability_digest: message.capabilities.capability_digest.clone(),
        security_zone,
        max_slots,
        message_id: message.message_id.clone(),
        request_id: message.request_id.clone(),
        sent_at: message.sent_at.clone(),
        started_at: message.started_at.clone(),
        worker_id: message.worker_id.clone(),
        worker_instance_id: message.worker_instance_id.clone(),
    })
}

fn registration_response(
    message: &WorkerRegisterMessage,
    receipt: WorkerRegistrationReceipt,
    server_time: &Instant,
    heartbeat_interval_ms: i64,
) -> WorkerRegistrationResultMessage {
    let (status, error) = match receipt.status {
        WorkerRegistrationStatus::Accepted => {
            (WorkerRegistrationResultMessageStatus::Accepted, None)
        }
        WorkerRegistrationStatus::Duplicate => {
            (WorkerRegistrationResultMessageStatus::Duplicate, None)
        }
        WorkerRegistrationStatus::RejectedConflict => (
            WorkerRegistrationResultMessageStatus::Rejected,
            Some(registration_error(
                receipt
                    .error
                    .unwrap_or(WorkerRegistrationErrorCode::MessageConflict),
            )),
        ),
    };
    WorkerRegistrationResultMessage {
        error,
        heartbeat_interval_ms,
        kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
        lease_recovery: lease_recovery_name(receipt.lease_recovery),
        message_id: message.message_id.clone(),
        request_id: message.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: server_time.clone(),
        server_time: server_time.clone(),
        status,
        worker_id: receipt.worker.worker_id,
        worker_instance_id: receipt.worker.worker_instance_id,
    }
}

fn registration_error(code: WorkerRegistrationErrorCode) -> ExecutionPortError {
    let (code, message) = match code {
        WorkerRegistrationErrorCode::ProtocolVersionUnsupported => (
            ExecutionPortErrorCode::ProtocolVersionUnsupported,
            "Worker protocol version is unsupported",
        ),
        WorkerRegistrationErrorCode::CapabilityMismatch => (
            ExecutionPortErrorCode::CapabilityMismatch,
            "Worker capability profile conflicts with its registration",
        ),
        WorkerRegistrationErrorCode::AuthenticationMismatch => (
            ExecutionPortErrorCode::MessageConflict,
            "Worker authentication identity conflicts with its registration",
        ),
        WorkerRegistrationErrorCode::SecurityZoneMismatch => (
            ExecutionPortErrorCode::MessageConflict,
            "Worker security zone conflicts with its registration",
        ),
        WorkerRegistrationErrorCode::ScopeMismatch => (
            ExecutionPortErrorCode::MessageConflict,
            "Worker scope conflicts with its registration",
        ),
        WorkerRegistrationErrorCode::MessageConflict => (
            ExecutionPortErrorCode::MessageConflict,
            "registration request conflicts with its durable receipt",
        ),
    };
    ExecutionPortError {
        code,
        message: message.to_owned(),
        retryable: false,
    }
}

fn heartbeat_response(
    message: &WorkerHeartbeatMessage,
    receipt: &WorkerHeartbeatReceipt,
    server_time: &Instant,
    interval_ms: i64,
) -> WorkerHeartbeatAckMessage {
    let (status, error) = lease_status_response(receipt.status);
    WorkerHeartbeatAckMessage {
        error,
        heartbeat_sequence: message.heartbeat_sequence.clone(),
        kind: WorkerHeartbeatAckMessageKind::WorkerHeartbeatAck,
        message_id: message.message_id.clone(),
        next_heartbeat_within_ms: interval_ms,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: server_time.clone(),
        server_time: server_time.clone(),
        status,
        worker_id: message.worker_id.clone(),
        worker_instance_id: message.worker_instance_id.clone(),
    }
}

pub(crate) fn lease_stamp(record: &ExecutionLeaseRecord) -> ExecutionLeaseStamp {
    ExecutionLeaseStamp {
        attempt: i64::try_from(record.attempt).unwrap_or(i64::MAX),
        expires_at: record.expires_at.clone(),
        fencing_token: record.fencing_token.clone(),
        issued_at: record.issued_at.clone(),
        job_id: record.job_id.clone(),
        lease_id: record.lease_id.clone(),
        worker_id: record.worker_id.clone(),
        worker_instance_id: record.worker_instance_id.clone(),
    }
}

fn lease_recovery_name(recovery: LeaseRecovery) -> WorkerRegistrationResultMessageLeaseRecovery {
    match recovery {
        LeaseRecovery::NoActiveLeases => {
            WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases
        }
        LeaseRecovery::ReacquireRequired => {
            WorkerRegistrationResultMessageLeaseRecovery::ReacquireRequired
        }
    }
}

fn lease_status_response(
    status: LeaseWriteStatus,
) -> (WorkerHeartbeatAckMessageStatus, Option<ExecutionPortError>) {
    match status {
        LeaseWriteStatus::Accepted => (WorkerHeartbeatAckMessageStatus::Accepted, None),
        LeaseWriteStatus::Duplicate => (WorkerHeartbeatAckMessageStatus::Duplicate, None),
        LeaseWriteStatus::Gap => (
            WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance,
            Some(ExecutionPortError {
                code: ExecutionPortErrorCode::SequenceGap,
                message: "heartbeat sequence has a gap".to_owned(),
                retryable: true,
            }),
        ),
        LeaseWriteStatus::RejectedConflict => (
            WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance,
            Some(ExecutionPortError {
                code: ExecutionPortErrorCode::MessageConflict,
                message: "heartbeat conflicts with its durable receipt".to_owned(),
                retryable: false,
            }),
        ),
        LeaseWriteStatus::RejectedExpiredLease => (
            WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance,
            Some(ExecutionPortError {
                code: ExecutionPortErrorCode::LeaseExpired,
                message: "heartbeat lease has expired".to_owned(),
                retryable: false,
            }),
        ),
        LeaseWriteStatus::RejectedStaleFencingToken => (
            WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance,
            Some(ExecutionPortError {
                code: ExecutionPortErrorCode::StaleFencingToken,
                message: "heartbeat uses a stale fencing token".to_owned(),
                retryable: false,
            }),
        ),
        LeaseWriteStatus::RejectedWorkerInstance => (
            WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance,
            Some(ExecutionPortError {
                code: ExecutionPortErrorCode::WorkerInstanceChanged,
                message: "heartbeat comes from a replaced Worker instance".to_owned(),
                retryable: false,
            }),
        ),
    }
}

fn repository_scheduler_error(
    error: RepositoryExecutionSchedulerError,
) -> ExecutionPortServiceError {
    match error {
        RepositoryExecutionSchedulerError::Storage(error) => {
            ExecutionPortServiceError::Storage(error)
        }
        RepositoryExecutionSchedulerError::StaleDeliveryRevision => {
            ExecutionPortServiceError::Protocol("Delivery revision is stale")
        }
        RepositoryExecutionSchedulerError::InvalidExecutionJob(field) => {
            ExecutionPortServiceError::Protocol(field)
        }
        RepositoryExecutionSchedulerError::MissingCancellationAuthority(field) => {
            ExecutionPortServiceError::AuthorityRejected(field)
        }
    }
}

fn wire_dispatch_result_status(status: DispatchResultStatus) -> JobDispatchResultMessageStatus {
    match status {
        DispatchResultStatus::Accepted => JobDispatchResultMessageStatus::Accepted,
        DispatchResultStatus::Duplicate => JobDispatchResultMessageStatus::Duplicate,
        DispatchResultStatus::Conflict => JobDispatchResultMessageStatus::Conflict,
        DispatchResultStatus::RejectedCapacity => JobDispatchResultMessageStatus::RejectedCapacity,
        DispatchResultStatus::RejectedCapability => {
            JobDispatchResultMessageStatus::RejectedCapability
        }
        DispatchResultStatus::RejectedExpiredLease => {
            JobDispatchResultMessageStatus::RejectedExpiredLease
        }
        DispatchResultStatus::RejectedStaleFencingToken => {
            JobDispatchResultMessageStatus::RejectedStaleFencingToken
        }
        DispatchResultStatus::RejectedWorkerInstance => {
            JobDispatchResultMessageStatus::RejectedWorkerInstance
        }
    }
}

fn dispatch_result_error(error: DispatchResultError) -> ExecutionPortError {
    let (code, message) = match error.code {
        DispatchResultErrorCode::MessageConflict => (
            ExecutionPortErrorCode::MessageConflict,
            "dispatch result conflicts with its durable receipt",
        ),
        DispatchResultErrorCode::JobDispatchConflict => (
            ExecutionPortErrorCode::JobDispatchConflict,
            "dispatch result does not match durable lease authority",
        ),
        DispatchResultErrorCode::LeaseExpired => (
            ExecutionPortErrorCode::LeaseExpired,
            "dispatch result lease has expired",
        ),
        DispatchResultErrorCode::StaleFencingToken => (
            ExecutionPortErrorCode::StaleFencingToken,
            "dispatch result uses a stale fencing token",
        ),
        DispatchResultErrorCode::WorkerNotRegistered => (
            ExecutionPortErrorCode::WorkerNotRegistered,
            "Worker is not registered in the durable registry",
        ),
        DispatchResultErrorCode::WorkerInstanceChanged => (
            ExecutionPortErrorCode::WorkerInstanceChanged,
            "dispatch result comes from a replaced Worker instance",
        ),
    };
    ExecutionPortError {
        code,
        message: message.to_owned(),
        retryable: error.retryable,
    }
}
