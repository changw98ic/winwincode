// SPDX-License-Identifier: Apache-2.0

//! Production Worker ingress over the canonical local Control Plane storage.
//!
//! Local and remote transports use this same core. The generated Worker frame
//! is never accepted as lease authority: every job-scoped message first loads
//! the Registry's opaque accepted-dispatch record from the same `SQLite`
//! database owned by the running Control Plane.

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_api::generated::RepositoryScope;
use winwincode_delivery::application::stage::{
    DeliveryTerminalOutcomeFacts, TerminalArtifactReference, TerminalOutcomeStatus,
    WorkerTerminalOutcomeReport, seal_dispatch_terminal_outcome, seal_session_binding_authority,
};
use winwincode_domain::{ExecutionMessageId, Instant, SchemaVersion};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionLeaseStamp, ExecutionOutcomeStatus, ExecutionPortError,
    ExecutionPortErrorCode, ExecutionPortMessage, ExecutionScope, JobOutcomeAckMessage,
    JobOutcomeAckMessageKind, JobOutcomeAckMessageStatus, JobOutcomeMessage, SessionBindingMessage,
};
use winwincode_execution_port::transport::ExecutionPortCore;
use winwincode_storage::{
    ExecutionDispatchAuthority, ExecutionQueueScope, SqliteStorage, StorageError, StorageErrorKind,
};

use crate::delivery_transaction::load_durable_execution_job;
use crate::session_binding_transaction::instant_millis;
use crate::{
    ActionPolicyEnforcementError, ControlPlane, DeliverySessionBindingCommitError,
    DeliveryTerminalOutcomeCommitError, ExecutionPortService, ExecutionPortServiceError,
    RuntimeMessageError,
};

const OUTCOME_ACK_NAMESPACE: &[u8] = b"winwincode.execution-port.outcome-ack.v1";
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Failure before the Worker ingress can return a canonical generated result.
#[derive(Debug)]
pub enum DurableExecutionPortError {
    /// The Control Plane and Registry connections do not name one local DB.
    Configuration,
    /// Registry-backed registration, heartbeat, or dispatch-result handling failed.
    Service(ExecutionPortServiceError),
    /// Durable authority resolution or Registry settlement failed.
    Storage(StorageError),
    /// The canonical Delivery binding transaction failed.
    SessionBinding(DeliverySessionBindingCommitError),
    /// The canonical runtime ledger transaction failed.
    Runtime(RuntimeMessageError),
    /// The canonical Delivery terminal transaction failed.
    Terminal(DeliveryTerminalOutcomeCommitError),
    /// The canonical Control Plane action-policy receipt could not be issued.
    ActionPolicy(ActionPolicyEnforcementError),
    /// Another production seam owns this generated message kind.
    UnsupportedMessage,
}

impl fmt::Display for DurableExecutionPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration => formatter
                .write_str("Control Plane and Execution Registry do not share one local database"),
            Self::Service(error) => write!(formatter, "{error}"),
            Self::Storage(error) => write!(formatter, "ExecutionPort authority failed: {error}"),
            Self::SessionBinding(error) => write!(formatter, "{error}"),
            Self::Runtime(error) => write!(formatter, "{error}"),
            Self::Terminal(error) => write!(formatter, "{error}"),
            Self::ActionPolicy(error) => write!(formatter, "{error}"),
            Self::UnsupportedMessage => {
                formatter.write_str("ExecutionPort message belongs to another production seam")
            }
        }
    }
}

impl std::error::Error for DurableExecutionPortError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Service(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::SessionBinding(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Terminal(error) => Some(error),
            Self::ActionPolicy(error) => Some(error),
            Self::Configuration | Self::UnsupportedMessage => None,
        }
    }
}

/// The closed set of Worker messages whose canonical owner is composed beside
/// the base Registry/Delivery/runtime ingress.
///
/// `ProductSession` terminal handling receives the already loaded immutable Job
/// and opaque accepted-dispatch authority. Other variants remain typed inside
/// the generated union and are delegated only after this ingress has selected
/// their Worker-to-Control-Plane direction.
pub enum DurableExecutionPortSupplement<'message> {
    /// `ProductSession` chat `WorkerSession`/`CodexThread` binding owner.
    ProductSessionBinding {
        job: &'message ExecutionJob,
        dispatch: &'message ExecutionDispatchAuthority,
        message: &'message SessionBindingMessage,
    },
    /// `ProductSession` chat terminal/cancel projection owner.
    ProductSessionOutcome {
        job: &'message ExecutionJob,
        dispatch: &'message ExecutionDispatchAuthority,
        message: &'message JobOutcomeMessage,
    },
    /// Canonical owner for a job-scoped cancel acknowledgement, model/action
    /// message, Artifact message, or interaction request.
    JobScopedWorkerMessage {
        dispatch: &'message ExecutionDispatchAuthority,
        message: &'message ExecutionPortMessage,
    },
    /// Canonical owner for a Worker capability update without a Job lease.
    WorkerMessage(&'message ExecutionPortMessage),
}

/// Borrowed access to the exact production objects already held by the Server
/// application lock.
pub struct DurableExecutionPortContext<'application> {
    control_plane: &'application mut ControlPlane,
    storage: &'application mut SqliteStorage,
    repository_scope: &'application RepositoryScope,
    server_time: &'application Instant,
}

impl DurableExecutionPortContext<'_> {
    /// Returns the one running Control Plane used by HTTP/WS and Worker ingress.
    pub fn control_plane(&mut self) -> &mut ControlPlane {
        self.control_plane
    }

    /// Returns the Registry/product connection to the same local database.
    pub fn storage(&mut self) -> &mut SqliteStorage {
        self.storage
    }

    /// Returns the Server-configured repository scope, never Worker input.
    #[must_use]
    pub const fn repository_scope(&self) -> &RepositoryScope {
        self.repository_scope
    }

    /// Returns the authoritative ingress time captured by the Server clock.
    #[must_use]
    pub const fn server_time(&self) -> &Instant {
        self.server_time
    }

    /// Validates the sealed lease at the trusted ingress time for a first-seen
    /// owner message.
    ///
    /// The owner must call this only after its own receipt identity and body
    /// digest lookup proved that the message is new. Exact receipt replay must
    /// return the stored result before calling this helper. The trusted clock
    /// must not be copied into the owner request identity, digest, or
    /// replay-comparison body, because its value necessarily changes across an
    /// exact replay.
    ///
    /// # Errors
    ///
    /// Returns an authority error when Server time predates lease issuance or
    /// is at/after lease expiry.
    pub fn validate_first_seen_dispatch(
        &self,
        authority: &ExecutionDispatchAuthority,
    ) -> Result<(), DurableExecutionPortError> {
        let issued_at = instant_millis(&authority.lease().issued_at)
            .map_err(DurableExecutionPortError::Storage)?;
        let expires_at = instant_millis(&authority.lease().expires_at)
            .map_err(DurableExecutionPortError::Storage)?;
        let server_time =
            instant_millis(self.server_time).map_err(DurableExecutionPortError::Storage)?;
        if server_time < issued_at || server_time >= expires_at {
            return Err(DurableExecutionPortError::Storage(
                StorageError::invalid_input("Server time is outside the accepted dispatch lease"),
            ));
        }
        Ok(())
    }
}

/// Production composition seam for canonical Worker message owners that need
/// dependencies beyond the base Control Plane/Registry pair.
///
/// Implementations are installed inside the same ingress core; they are not a
/// second transport or state authority.
pub trait DurableExecutionPortDelegate {
    /// Accepts one message selected by [`DurableExecutionPortIngress`].
    ///
    /// Every job-scoped owner must resolve its exact durable receipt and body
    /// digest first. An exact replay returns that original result even after
    /// lease expiry; a changed body conflicts. Only a first-seen message may
    /// validate the sealed lease against [`DurableExecutionPortContext::server_time`].
    /// The trusted clock never enters the owner digest.
    /// Worker-controlled `sentAt` is an audited fact, never authorization.
    ///
    /// # Errors
    ///
    /// Returns canonical owner errors through the shared ingress error type.
    fn accept(
        &mut self,
        context: DurableExecutionPortContext<'_>,
        supplement: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError>;
}

/// Borrowed production core for one Worker-to-Control-Plane dispatch.
///
/// The Server can create this value while holding its one application-state
/// lock, so HTTP/WS operations and the embedded Worker share the exact same
/// `ControlPlane`, product storage, outbox publisher, and durable event hub.
pub struct DurableExecutionPortIngress<'application> {
    control_plane: &'application mut ControlPlane,
    storage: &'application mut SqliteStorage,
    repository_scope: &'application RepositoryScope,
    server_time: Instant,
    delegate: Option<&'application mut dyn DurableExecutionPortDelegate>,
}

impl<'application> DurableExecutionPortIngress<'application> {
    /// Binds Worker ingress to the exact local database owned by the running
    /// Control Plane.
    ///
    /// # Errors
    ///
    /// Rejects adapter-injected or differently rooted storage connections.
    pub fn new(
        control_plane: &'application mut ControlPlane,
        storage: &'application mut SqliteStorage,
        repository_scope: &'application RepositoryScope,
        server_time: Instant,
    ) -> Result<Self, DurableExecutionPortError> {
        if control_plane.local_database_path() != Some(storage.database_path()) {
            return Err(DurableExecutionPortError::Configuration);
        }
        Ok(Self {
            control_plane,
            storage,
            repository_scope,
            server_time,
            delegate: None,
        })
    }

    /// Binds the base durable ingress and all supplementary canonical owners to
    /// the same production application objects.
    ///
    /// # Errors
    ///
    /// Rejects the same split-database composition as [`Self::new`].
    pub fn with_delegate(
        control_plane: &'application mut ControlPlane,
        storage: &'application mut SqliteStorage,
        repository_scope: &'application RepositoryScope,
        server_time: Instant,
        delegate: &'application mut dyn DurableExecutionPortDelegate,
    ) -> Result<Self, DurableExecutionPortError> {
        let mut ingress = Self::new(control_plane, storage, repository_scope, server_time)?;
        ingress.delegate = Some(delegate);
        Ok(ingress)
    }

    /// Routes one generated Worker message through its canonical durable seam.
    ///
    /// # Errors
    ///
    /// Returns infrastructure/composition failures. Runtime and terminal
    /// authority rejections are returned as generated acknowledgement DTOs.
    pub fn handle(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        match message {
            ExecutionPortMessage::WorkerRegisterMessage(_)
            | ExecutionPortMessage::WorkerHeartbeatMessage(_) => {
                let response = match message {
                    ExecutionPortMessage::WorkerRegisterMessage(register) => {
                        ExecutionPortService::new(self.storage, self.server_time.clone())
                            .register_local_worker_for_scope(register, self.repository_scope)
                            .map(ExecutionPortMessage::WorkerRegistrationResultMessage)
                    }
                    ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat) => {
                        ExecutionPortService::new(self.storage, self.server_time.clone())
                            .record_heartbeat(heartbeat)
                            .map(ExecutionPortMessage::WorkerHeartbeatAckMessage)
                    }
                    _ => unreachable!("Worker registration/heartbeat match is exhaustive"),
                }
                .map_err(DurableExecutionPortError::Service)?;
                Ok(vec![response])
            }
            ExecutionPortMessage::JobDispatchResultMessage(result) => {
                ExecutionPortService::new(self.storage, self.server_time.clone())
                    .accept_dispatch_result(result.clone())
                    .map_err(DurableExecutionPortError::Service)?;
                Ok(Vec::new())
            }
            ExecutionPortMessage::SessionBindingMessage(binding) => {
                let authority = self.dispatch_authority(&binding.lease.job_id)?;
                require_exact_dispatch_message(
                    &binding.lease,
                    &binding.worker_session_id,
                    &binding.session_identity.worker_session_id,
                    &authority,
                )?;
                let (_, job) = load_durable_execution_job(self.storage, &binding.lease.job_id)
                    .map_err(DurableExecutionPortError::Storage)?;
                if matches!(job.scope, ExecutionScope::ProductSessionExecutionScope(_)) {
                    return self.delegate_supplement(
                        DurableExecutionPortSupplement::ProductSessionBinding {
                            job: &job,
                            dispatch: &authority,
                            message: binding,
                        },
                    );
                }
                let authority = seal_session_binding_authority(&authority);
                self.control_plane
                    .commit_delivery_session_binding(binding, &authority, &self.server_time)
                    .map_err(DurableExecutionPortError::SessionBinding)?;
                Ok(Vec::new())
            }
            ExecutionPortMessage::RuntimeEventMessage(runtime) => {
                let authority = self.dispatch_authority(&runtime.lease.job_id)?;
                let authority = seal_session_binding_authority(&authority);
                let acknowledgement = self
                    .control_plane
                    .accept_runtime_event(
                        self.repository_scope,
                        runtime,
                        &authority,
                        &self.server_time,
                    )
                    .map_err(DurableExecutionPortError::Runtime)?;
                Ok(vec![ExecutionPortMessage::RuntimeAckMessage(
                    acknowledgement,
                )])
            }
            ExecutionPortMessage::JobOutcomeMessage(outcome) => {
                self.accept_terminal_outcome(outcome)
            }
            ExecutionPortMessage::ArtifactOpenMessage(artifact) => {
                self.delegate_job_scoped(&artifact.lease.job_id, message)
            }
            ExecutionPortMessage::ArtifactChunkMessage(artifact) => {
                self.delegate_job_scoped(&artifact.lease.job_id, message)
            }
            ExecutionPortMessage::ModelOpenMessage(model) => {
                self.delegate_job_scoped(&model.lease.job_id, message)
            }
            ExecutionPortMessage::ModelAckMessage(model) => {
                self.delegate_job_scoped(&model.lease.job_id, message)
            }
            ExecutionPortMessage::InputRequestMessage(request) => {
                self.delegate_job_scoped(&request.lease.job_id, message)
            }
            ExecutionPortMessage::ApprovalRequestMessage(request) => {
                self.delegate_job_scoped(&request.lease.job_id, message)
            }
            ExecutionPortMessage::JobCancelAckMessage(acknowledgement) => {
                self.delegate_job_scoped(&acknowledgement.lease.job_id, message)
            }
            ExecutionPortMessage::ActionEnforcementRequestMessage(request) => {
                self.delegate_job_scoped(&request.lease.job_id, message)
            }
            ExecutionPortMessage::WorkerCapabilitiesMessage(_) => {
                self.delegate_supplement(DurableExecutionPortSupplement::WorkerMessage(message))
            }
            _ => Err(DurableExecutionPortError::UnsupportedMessage),
        }
    }

    fn dispatch_authority(
        &mut self,
        job_id: &winwincode_domain::ExecutionJobId,
    ) -> Result<ExecutionDispatchAuthority, DurableExecutionPortError> {
        self.storage
            .execution_registry()
            .and_then(|registry| registry.load_dispatch_authority(job_id))
            .map_err(DurableExecutionPortError::Storage)?
            .ok_or_else(|| {
                DurableExecutionPortError::Storage(StorageError::invalid_input(
                    "accepted dispatch authority is missing",
                ))
            })
    }

    fn delegate_job_scoped(
        &mut self,
        job_id: &winwincode_domain::ExecutionJobId,
        message: &ExecutionPortMessage,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        let dispatch = self.dispatch_authority(job_id)?;
        let (lease, worker_session_id, identity_worker_session_id) =
            delegated_message_authority(message)
                .ok_or(DurableExecutionPortError::UnsupportedMessage)?;
        require_exact_dispatch_message(
            lease,
            worker_session_id,
            identity_worker_session_id,
            &dispatch,
        )?;
        self.delegate_supplement(DurableExecutionPortSupplement::JobScopedWorkerMessage {
            dispatch: &dispatch,
            message,
        })
    }

    fn delegate_supplement(
        &mut self,
        supplement: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        let Some(delegate) = self.delegate.as_deref_mut() else {
            return Err(DurableExecutionPortError::UnsupportedMessage);
        };
        delegate.accept(
            DurableExecutionPortContext {
                control_plane: self.control_plane,
                storage: self.storage,
                repository_scope: self.repository_scope,
                server_time: &self.server_time,
            },
            supplement,
        )
    }

    fn accept_terminal_outcome(
        &mut self,
        message: &JobOutcomeMessage,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        let authority = match self.dispatch_authority(&message.lease.job_id) {
            Ok(authority) => authority,
            Err(DurableExecutionPortError::Storage(error))
                if error.kind() == StorageErrorKind::InvalidInput =>
            {
                return Ok(outcome_output(outcome_rejection(
                    message,
                    JobOutcomeAckMessageStatus::RejectedConflict,
                    ExecutionPortErrorCode::JobDispatchConflict,
                    "job.outcome has no accepted dispatch authority",
                )));
            }
            Err(error) => return Err(error),
        };
        if let Some(rejection) = outcome_authority_rejection(message, &authority) {
            return Ok(outcome_output(rejection));
        }
        let (_, job) = load_durable_execution_job(self.storage, &message.lease.job_id)
            .map_err(DurableExecutionPortError::Storage)?;
        if matches!(job.scope, ExecutionScope::ProductSessionExecutionScope(_)) {
            return self.delegate_supplement(
                DurableExecutionPortSupplement::ProductSessionOutcome {
                    job: &job,
                    dispatch: &authority,
                    message,
                },
            );
        }
        let facts = Self::terminal_facts(message, &authority, &job)?;
        let replayed = match self.commit_terminal(message, &facts)? {
            Ok(replayed) => replayed,
            Err(rejection) => return Ok(outcome_output(rejection)),
        };
        self.finish_delivery_execution_resources(message, &job, &authority)?;
        Ok(outcome_output(outcome_success(message, replayed)))
    }

    /// Delivery terminal ownership has the same local admission, Worker slot,
    /// and queue settlement as `ProductSession` Chat. The terminal transaction
    /// above commits only the Delivery state; this second durable owner closes
    /// the execution resources from the exact accepted dispatch and stored
    /// reservation. It is replay-safe because each component accepts its
    /// already-terminal state as an idempotent no-op.
    fn finish_delivery_execution_resources(
        &mut self,
        message: &JobOutcomeMessage,
        job: &ExecutionJob,
        dispatch: &ExecutionDispatchAuthority,
    ) -> Result<(), DurableExecutionPortError> {
        let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
            return Err(DurableExecutionPortError::Storage(
                StorageError::invalid_input("Delivery terminal owner received another Job scope"),
            ));
        };
        let execution_scope = ExecutionQueueScope {
            organization_id: self.repository_scope.organization_id.clone(),
            workspace_id: self.repository_scope.workspace_id.clone(),
            project_id: self.repository_scope.project_id.clone(),
            repository_id: self.repository_scope.repository_id.clone(),
            product_session_id: job_scope.product_session_id.clone(),
            delivery_id: Some(job_scope.delivery_id.clone()),
        };
        let reservation = self
            .storage
            .execution_admission()
            .map_err(|_| {
                DurableExecutionPortError::Storage(StorageError::invalid_input(
                    "Delivery terminal admission cannot be opened",
                ))
            })?
            .load_reservation_by_job(&message.lease.job_id)
            .map_err(|_| {
                DurableExecutionPortError::Storage(StorageError::invalid_input(
                    "Delivery terminal admission cannot be read",
                ))
            })?
            .ok_or_else(|| {
                DurableExecutionPortError::Storage(StorageError::invalid_input(
                    "Delivery terminal admission is missing",
                ))
            })?;
        if reservation.scope != execution_scope {
            return Err(DurableExecutionPortError::Storage(
                StorageError::invalid_input("Delivery terminal admission scope differs"),
            ));
        }
        let slot = self
            .storage
            .worker_session_slots()
            .map_err(|_| {
                DurableExecutionPortError::Storage(StorageError::invalid_input(
                    "Delivery terminal Worker slot cannot be opened",
                ))
            })?
            .load(dispatch.worker_session_id())
            .map_err(|_| {
                DurableExecutionPortError::Storage(StorageError::invalid_input(
                    "Delivery terminal Worker slot cannot be read",
                ))
            })?
            .ok_or_else(|| {
                DurableExecutionPortError::Storage(StorageError::invalid_input(
                    "Delivery terminal Worker slot is missing",
                ))
            })?;
        let lease = dispatch.lease();
        let slot_authority = &slot.authority;
        if slot_authority.worker_id != lease.worker_id
            || slot_authority.worker_instance_id != lease.worker_instance_id
            || slot_authority.worker_session_id != *dispatch.worker_session_id()
            || slot_authority.job_id != lease.job_id
            || slot_authority.lease_id != lease.lease_id
            || slot_authority.attempt != lease.attempt
            || slot_authority.fencing_token != lease.fencing_token
            || message
                .outcome
                .codex_thread_id
                .as_ref()
                .is_some_and(|thread_id| thread_id != &slot_authority.codex_thread_id)
        {
            return Err(DurableExecutionPortError::Storage(
                StorageError::invalid_input("Delivery terminal Worker slot authority differs"),
            ));
        }
        crate::product_session_execution_application::finish_execution_resources(
            self.storage,
            message,
            &execution_scope,
            &reservation.worker_pool_id,
            slot_authority,
        )
    }

    fn terminal_facts(
        message: &JobOutcomeMessage,
        authority: &ExecutionDispatchAuthority,
        job: &ExecutionJob,
    ) -> Result<DeliveryTerminalOutcomeFacts, DurableExecutionPortError> {
        let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
            return Err(DurableExecutionPortError::Storage(
                StorageError::invalid_input("Delivery terminal owner received another Job scope"),
            ));
        };
        Ok(seal_dispatch_terminal_outcome(
            authority,
            WorkerTerminalOutcomeReport {
                stage_run_id: job_scope.stage_run_id.clone(),
                status: terminal_status(&message.outcome.status),
                codex_thread_id: message.outcome.codex_thread_id.clone(),
                finished_at_millis: instant_millis(&message.outcome.finished_at)
                    .map_err(DurableExecutionPortError::Storage)?,
                last_event_sequence: message.outcome.last_event_sequence.clone(),
                artifacts: message
                    .outcome
                    .artifacts
                    .iter()
                    .map(|artifact| TerminalArtifactReference {
                        artifact_id: artifact.artifact_id.clone(),
                        digest: artifact.digest.clone(),
                    })
                    .collect(),
            },
        ))
    }

    fn commit_terminal(
        &mut self,
        message: &JobOutcomeMessage,
        facts: &DeliveryTerminalOutcomeFacts,
    ) -> Result<Result<bool, JobOutcomeAckMessage>, DurableExecutionPortError> {
        let commit = match self.control_plane.commit_delivery_terminal_outcome(
            self.repository_scope,
            message,
            facts,
            &self.server_time,
        ) {
            Ok(commit) => commit,
            Err(DeliveryTerminalOutcomeCommitError::Storage(error))
                if matches!(
                    error.kind(),
                    StorageErrorKind::InvalidInput
                        | StorageErrorKind::RequestConflict
                        | StorageErrorKind::RevisionConflict
                ) =>
            {
                return Ok(Err(outcome_rejection(
                    message,
                    JobOutcomeAckMessageStatus::RejectedConflict,
                    ExecutionPortErrorCode::MessageConflict,
                    "job.outcome conflicts with canonical durable state",
                )));
            }
            Err(error) => return Err(DurableExecutionPortError::Terminal(error)),
        };
        Ok(Ok(commit.receipt().idempotent_replay))
    }
}

fn delegated_message_authority(
    message: &ExecutionPortMessage,
) -> Option<(
    &ExecutionLeaseStamp,
    &winwincode_domain::WorkerSessionId,
    &winwincode_domain::WorkerSessionId,
)> {
    macro_rules! authority {
        ($message:expr) => {
            Some((
                &$message.lease,
                &$message.worker_session_id,
                &$message.session_identity.worker_session_id,
            ))
        };
    }
    match message {
        ExecutionPortMessage::ArtifactOpenMessage(message) => authority!(message),
        ExecutionPortMessage::ArtifactChunkMessage(message) => authority!(message),
        ExecutionPortMessage::ModelOpenMessage(message) => authority!(message),
        ExecutionPortMessage::ModelAckMessage(message) => authority!(message),
        ExecutionPortMessage::InputRequestMessage(message) => authority!(message),
        ExecutionPortMessage::ApprovalRequestMessage(message) => authority!(message),
        ExecutionPortMessage::JobCancelAckMessage(message) => authority!(message),
        ExecutionPortMessage::ActionEnforcementRequestMessage(message)
            if message.job_id == message.lease.job_id =>
        {
            authority!(message)
        }
        _ => None,
    }
}

fn require_exact_dispatch_message(
    lease: &ExecutionLeaseStamp,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    identity_worker_session_id: &winwincode_domain::WorkerSessionId,
    authority: &ExecutionDispatchAuthority,
) -> Result<(), DurableExecutionPortError> {
    let accepted = authority.lease();
    let attempt = u64::try_from(lease.attempt).ok();
    let exact = lease.job_id == accepted.job_id
        && lease.lease_id == accepted.lease_id
        && lease.worker_id == accepted.worker_id
        && lease.worker_instance_id == accepted.worker_instance_id
        && attempt == Some(accepted.attempt)
        && lease.fencing_token == accepted.fencing_token
        && lease.issued_at == accepted.issued_at
        && lease.expires_at == accepted.expires_at
        && worker_session_id == authority.worker_session_id()
        && identity_worker_session_id == worker_session_id;
    if exact {
        Ok(())
    } else {
        Err(DurableExecutionPortError::Storage(
            StorageError::invalid_input(
                "Worker message differs from its accepted dispatch authority",
            ),
        ))
    }
}

impl ExecutionPortCore for DurableExecutionPortIngress<'_> {
    type Error = DurableExecutionPortError;
    type Output = Vec<ExecutionPortMessage>;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        self.handle(message)
    }
}

fn outcome_authority_rejection(
    message: &JobOutcomeMessage,
    authority: &ExecutionDispatchAuthority,
) -> Option<JobOutcomeAckMessage> {
    let lease = authority.lease();
    if message.lease.worker_instance_id != lease.worker_instance_id {
        return Some(outcome_rejection(
            message,
            JobOutcomeAckMessageStatus::RejectedWorkerInstance,
            ExecutionPortErrorCode::WorkerInstanceChanged,
            "job.outcome uses another Worker instance",
        ));
    }
    if message.lease.fencing_token != lease.fencing_token
        && decimal_is_less(&message.lease.fencing_token.0, &lease.fencing_token.0)
    {
        return Some(outcome_rejection(
            message,
            JobOutcomeAckMessageStatus::RejectedStaleFencingToken,
            ExecutionPortErrorCode::StaleFencingToken,
            "job.outcome uses a stale fencing token",
        ));
    }
    let attempt = u64::try_from(message.lease.attempt).ok();
    let exact = message.lease.job_id == lease.job_id
        && message.lease.lease_id == lease.lease_id
        && message.lease.worker_id == lease.worker_id
        && message.lease.worker_instance_id == lease.worker_instance_id
        && attempt == Some(lease.attempt)
        && message.lease.fencing_token == lease.fencing_token
        && message.lease.issued_at == lease.issued_at
        && message.lease.expires_at == lease.expires_at
        && message.worker_session_id == *authority.worker_session_id();
    (!exact).then(|| {
        outcome_rejection(
            message,
            JobOutcomeAckMessageStatus::RejectedConflict,
            ExecutionPortErrorCode::JobDispatchConflict,
            "job.outcome differs from its accepted dispatch authority",
        )
    })
}

fn outcome_rejection(
    message: &JobOutcomeMessage,
    status: JobOutcomeAckMessageStatus,
    code: ExecutionPortErrorCode,
    explanation: &'static str,
) -> JobOutcomeAckMessage {
    JobOutcomeAckMessage {
        error: Some(ExecutionPortError {
            code,
            message: explanation.to_owned(),
            retryable: false,
        }),
        kind: JobOutcomeAckMessageKind::JobOutcomeAck,
        lease: message.lease.clone(),
        message_id: derived_message_id(OUTCOME_ACK_NAMESPACE, &message.message_id),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: message.sent_at.clone(),
        session_identity: message.session_identity.clone(),
        status,
        worker_session_id: message.worker_session_id.clone(),
    }
}

fn outcome_success(message: &JobOutcomeMessage, replayed: bool) -> JobOutcomeAckMessage {
    JobOutcomeAckMessage {
        error: None,
        kind: JobOutcomeAckMessageKind::JobOutcomeAck,
        lease: message.lease.clone(),
        message_id: derived_message_id(OUTCOME_ACK_NAMESPACE, &message.message_id),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: message.sent_at.clone(),
        session_identity: message.session_identity.clone(),
        status: if replayed {
            JobOutcomeAckMessageStatus::Duplicate
        } else {
            JobOutcomeAckMessageStatus::Accepted
        },
        worker_session_id: message.worker_session_id.clone(),
    }
}

/// Builds the same canonical success acknowledgement for the composed
/// `ProductSession` terminal owner as for the built-in Delivery owner.
pub(crate) fn product_session_outcome_output(
    message: &JobOutcomeMessage,
    replayed: bool,
) -> Vec<ExecutionPortMessage> {
    outcome_output(outcome_success(message, replayed))
}

fn outcome_output(acknowledgement: JobOutcomeAckMessage) -> Vec<ExecutionPortMessage> {
    vec![ExecutionPortMessage::JobOutcomeAckMessage(acknowledgement)]
}

const fn terminal_status(status: &ExecutionOutcomeStatus) -> TerminalOutcomeStatus {
    match status {
        ExecutionOutcomeStatus::Succeeded => TerminalOutcomeStatus::Succeeded,
        ExecutionOutcomeStatus::Failed => TerminalOutcomeStatus::Failed,
        ExecutionOutcomeStatus::InfrastructureError => TerminalOutcomeStatus::InfrastructureError,
        ExecutionOutcomeStatus::Cancelled => TerminalOutcomeStatus::Cancelled,
    }
}

fn decimal_is_less(candidate: &str, current: &str) -> bool {
    let candidate = candidate.trim_start_matches('0');
    let current = current.trim_start_matches('0');
    candidate.len() < current.len() || (candidate.len() == current.len() && candidate < current)
}

fn derived_message_id(namespace: &[u8], input: &ExecutionMessageId) -> ExecutionMessageId {
    ExecutionMessageId(derived_identifier(namespace, input, "xmsg"))
}

fn derived_identifier(namespace: &[u8], input: &ExecutionMessageId, prefix: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace);
    hasher.update([0]);
    hasher.update(input.0.as_bytes());
    let digest = hasher.finalize();
    let suffix = digest
        .iter()
        .take(26)
        .map(|byte| char::from(CROCKFORD_BASE32[usize::from(byte & 31)]))
        .collect::<String>();
    format!("{prefix}_{suffix}")
}
