// SPDX-License-Identifier: Apache-2.0

//! Application lifecycle host for the `WinWinCode` Control Plane.
//!
//! This crate owns application composition, Delivery persistence adapters, and
//! durable event publication. It has no dependency on Codex Core, an HTTP
//! server, or an Execution Worker runtime.

mod artifact_transaction;
mod candidate_source;
mod delivery_command_transaction;
pub mod delivery_execution;
mod delivery_transaction;
pub mod execution_port_service;
mod publication_policy;
mod publication_preparation;
mod rework_transaction;
mod runtime_event_transaction;
mod session_binding_transaction;
pub mod session_identity;
pub mod strongflow_projection;
mod task_breakdown_transaction;
mod terminal_outcome_transaction;
mod verdict_transaction;

pub use artifact_transaction::ArtifactMessageError;
pub use candidate_source::CandidateResolutionError;
pub use delivery_command_transaction::{DeliveryCommandFacts, DeliverySpecFacts};
pub use execution_port_service::{
    DEFAULT_HEARTBEAT_INTERVAL_MS, ExecutionPortService, ExecutionPortServiceError,
    RuntimeReplayRequestCommand,
};
pub use publication_policy::PublicationCommandError;
pub use publication_preparation::{PreparedPublication, PublicationPreparationError};
pub use runtime_event_transaction::RuntimeMessageError;
pub use session_identity::{
    SessionBindingAcceptance, SessionIdentityAdapterError, validate_session_binding,
};

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use delivery_execution::{
    DeliveryExecutionDispatchReceipt, DeliveryExecutionError, DeliveryExecutionPortError,
    ExecutionJobDispatcher, PendingDeliveryExecution,
};
pub use session_binding_transaction::{
    DeliverySessionBindingCommitError, DeliverySessionBindingCommitReceipt,
};
use sha2::{Digest, Sha256};
pub use terminal_outcome_transaction::{
    DeliveryTerminalOutcomeCommitError, DeliveryTerminalOutcomeCommitReceipt,
};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, ControlPlaneWebSocketDeliveryChangedEvent,
    ControlPlaneWebSocketDeliveryChangedEventTypeValue, ControlPlaneWebSocketEventType,
    RepositoryScope, RepositoryScopeKind, Scope,
};
use winwincode_audit::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOrigin, AuditRetention, AuditScope,
    AuditState, AuditStore, AuditSubject,
};
use winwincode_delivery::application::{CoordinationError, verdict::SubmitVerdictFacts};
use winwincode_delivery::domain::Delivery;
use winwincode_domain::{
    ControlPlaneEventId, DeliveryId, OrganizationId, ProjectId, RepositoryId, Revision,
    Sha256Digest, SystemActorId, WorkspaceId,
};
use winwincode_execution_port::generated as execution_port;
pub use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactObjectStore,
    ArtifactStore, CommitReceipt, DurableOutboxEvent, GitSourceResolver, LoadedAggregateJournal,
    NewOutboxEvent, OutboxEvent, PendingAuditEvent, ProductStateStorage, ProjectionEventCursor,
    ProjectionEventStream, ProjectionEventStreamKey, StorageError, StorageErrorKind, StoredState,
};
use winwincode_storage::{
    LocalArtifactObjectStore, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

/// Deterministic trusted-fact fixtures for exercising the typed Control Plane
/// seam without exposing production authority constructors.
#[cfg(feature = "test-support")]
pub mod test_support {
    use winwincode_api::generated::{CommandEnvelope, RepositoryScope};
    use winwincode_delivery::{
        application::{attention::ResolvedAttentionTransition, stage::StageAdvanceResult},
        domain::{DeliverySourceRef, RepositoryRef},
    };

    use super::{
        DeliveryCommandFacts, DeliverySpecFacts, StorageError,
        delivery_command_transaction::TrustedDeliverySpecFacts,
    };

    /// Complete product-owned Spec semantics used by integration fixtures.
    #[derive(Clone, Debug, PartialEq)]
    pub struct DeliverySpecFactsFixture {
        pub repository_scope: RepositoryScope,
        pub now_millis: u64,
        pub repository: RepositoryRef,
        pub source_ref: Option<DeliverySourceRef>,
        pub scope: Vec<String>,
        pub out_of_scope: Vec<String>,
        pub constraints: Vec<String>,
        pub max_rework_attempts: u64,
        pub criterion_verification_methods: Vec<(String, String)>,
    }

    /// Adapter-confirmed repository authority used by sealed stage fixtures.
    #[derive(Clone, Debug, PartialEq)]
    pub struct DeliveryRepositoryFactsFixture {
        pub repository_scope: RepositoryScope,
        pub repository: RepositoryRef,
        pub source_ref: Option<DeliverySourceRef>,
    }

    /// Binds trusted repository, time, and exact criterion verification facts
    /// to one create or Spec-replacement command.
    ///
    /// # Errors
    ///
    /// Returns an error when the command and adapter-confirmed repository
    /// scope do not identify the same canonical authority.
    pub fn delivery_spec_command_facts(
        command: &CommandEnvelope,
        fixture: DeliverySpecFactsFixture,
    ) -> Result<DeliveryCommandFacts, StorageError> {
        let repository_scope = fixture.repository_scope.clone();
        DeliveryCommandFacts::specification_from_trusted_adapter(
            command,
            repository_scope,
            DeliverySpecFacts::from_trusted_adapter(TrustedDeliverySpecFacts {
                now_millis: fixture.now_millis,
                repository: fixture.repository,
                source_ref: fixture.source_ref,
                scope: fixture.scope,
                out_of_scope: fixture.out_of_scope,
                constraints: fixture.constraints,
                max_rework_attempts: fixture.max_rework_attempts,
                criterion_verification_methods: fixture.criterion_verification_methods,
            }),
        )
    }

    /// Binds one production-sealed human stage transition to its exact command.
    ///
    /// # Errors
    ///
    /// Returns an error when the command and adapter-confirmed repository
    /// scope do not identify the same canonical authority.
    pub fn delivery_advance_command_facts(
        command: &CommandEnvelope,
        repository: DeliveryRepositoryFactsFixture,
        transition: StageAdvanceResult,
    ) -> Result<DeliveryCommandFacts, StorageError> {
        DeliveryCommandFacts::advance_from_trusted_adapter(
            command,
            repository.repository_scope,
            repository.repository,
            repository.source_ref,
            transition,
        )
    }

    /// Binds one production-sealed Attention transition to its exact command.
    ///
    /// # Errors
    ///
    /// Returns an error when the command and adapter-confirmed repository
    /// scope do not identify the same canonical authority.
    pub fn delivery_attention_command_facts(
        command: &CommandEnvelope,
        repository: DeliveryRepositoryFactsFixture,
        transition: ResolvedAttentionTransition,
    ) -> Result<DeliveryCommandFacts, StorageError> {
        DeliveryCommandFacts::attention_from_trusted_adapter(
            command,
            repository.repository_scope,
            repository.repository,
            repository.source_ref,
            transition,
        )
    }
}

const ACTOR_KEY_PREFIX: &[u8] = b"winwincode.command-receipt.actor.v1";
const SCOPE_KEY_PREFIX: &[u8] = b"winwincode.command-receipt.scope.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryChangeKind {
    Created,
    Advanced,
    Reworked,
}

impl DeliveryChangeKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Advanced => "advanced",
            Self::Reworked => "reworked",
        }
    }
}

/// Canonical state and outbox values produced by one validated application command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateChange {
    pub stream_id: String,
    pub state: Vec<u8>,
    pub events: Vec<NewOutboxEvent>,
}

impl StateChange {
    #[must_use]
    pub fn new(
        stream_id: impl Into<String>,
        state: impl Into<Vec<u8>>,
        events: Vec<NewOutboxEvent>,
    ) -> Self {
        Self {
            stream_id: stream_id.into(),
            state: state.into(),
            events,
        }
    }
}

/// Local Control Plane process configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneConfig {
    data_directory: PathBuf,
    temporary_parent: PathBuf,
}

impl ControlPlaneConfig {
    #[must_use]
    pub fn local(data_directory: impl AsRef<Path>) -> Self {
        let data_directory = data_directory.as_ref().to_path_buf();
        Self {
            temporary_parent: data_directory.join(".control-plane-runtime"),
            data_directory,
        }
    }

    /// Overrides the parent under which the instance-owned temporary root is created.
    #[must_use]
    pub fn with_temporary_parent(mut self, temporary_parent: impl AsRef<Path>) -> Self {
        self.temporary_parent = temporary_parent.as_ref().to_path_buf();
        self
    }

    #[must_use]
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    #[must_use]
    pub fn temporary_parent(&self) -> &Path {
        &self.temporary_parent
    }
}

/// Error returned by an event publisher adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventPublishError {
    message: String,
}

impl EventPublishError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EventPublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EventPublishError {}

/// Event transport owned and closed by the Control Plane lifecycle.
pub trait EventPublisher: Send {
    /// Publishes one durable event. Implementations must deduplicate by
    /// `event.event_id` because a crash after publish and before acknowledgement
    /// can cause the same outbox event to be offered again.
    ///
    /// # Errors
    ///
    /// Returns a transport-specific failure without acknowledging the event.
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError>;

    /// Closes the event transport and releases its resources.
    ///
    /// # Errors
    ///
    /// Returns a transport-specific failure if deterministic close fails.
    fn close(&mut self) -> Result<(), EventPublishError> {
        Ok(())
    }
}

/// Failure while draining the durable outbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutboxError {
    Publish(EventPublishError),
    Acknowledge(StorageError),
    Audit(winwincode_audit::AuditError),
}

impl fmt::Display for OutboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Publish(error) => write!(formatter, "event publication failed: {error}"),
            Self::Acknowledge(error) => {
                write!(
                    formatter,
                    "event publication acknowledgement failed: {error}"
                )
            }
            Self::Audit(error) => write!(formatter, "audit event flush failed: {error}"),
        }
    }
}

impl std::error::Error for OutboxError {}

/// Control Plane startup failure. All resources are closed before this is returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartError {
    message: String,
}

impl StartError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StartError {}

/// Control Plane commit failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitError {
    /// No state was committed.
    Storage(StorageError),
    /// State and outbox were committed, but the event remains pending.
    PublicationPending {
        receipt: Box<CommitReceipt>,
        source: OutboxError,
    },
}

impl CommitError {
    #[must_use]
    pub fn committed_receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::Storage(_) => None,
            Self::PublicationPending { receipt, .. } => Some(receipt.as_ref()),
        }
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "state commit failed: {error}"),
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "state committed, but its outbox event remains pending: {source}"
            ),
        }
    }
}

impl std::error::Error for CommitError {}

/// Failure of the canonical base Delivery command transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryCommandCommitError {
    /// No durable member committed because command or storage validation failed.
    Storage(StorageError),
    /// The scoped Delivery required by a mutation does not exist.
    NotFound { delivery_id: DeliveryId },
    /// A different scoped request tried to create an existing Delivery.
    AlreadyExists { delivery_id: DeliveryId },
    /// The atomic transaction committed and only publication remains pending.
    PublicationPending {
        receipt: Box<CommitReceipt>,
        source: OutboxError,
    },
}

impl DeliveryCommandCommitError {
    #[must_use]
    pub fn public_code(&self) -> winwincode_api::generated::ErrorCode {
        use winwincode_api::generated::ErrorCode;
        match self {
            Self::NotFound { .. } => ErrorCode::ResourceNotFound,
            Self::AlreadyExists { .. } => ErrorCode::WrongState,
            Self::PublicationPending { .. } => ErrorCode::ServiceUnavailable,
            Self::Storage(error) => match error.kind() {
                StorageErrorKind::InvalidInput => ErrorCode::InvalidRequest,
                StorageErrorKind::RevisionConflict => ErrorCode::RevisionConflict,
                StorageErrorKind::RequestConflict => ErrorCode::IdempotencyConflict,
                StorageErrorKind::JournalNotFound => ErrorCode::ResourceNotFound,
                StorageErrorKind::RequestReplayMissing
                | StorageErrorKind::JournalAlreadyExists
                | StorageErrorKind::JournalConflict
                | StorageErrorKind::EventCursorExpired
                | StorageErrorKind::Adapter
                | StorageErrorKind::Closed => ErrorCode::ServiceUnavailable,
            },
        }
    }

    #[must_use]
    pub fn public_details(&self) -> winwincode_api::generated::ErrorDetails {
        use winwincode_api::generated::ErrorDetailValue;
        let mut details = winwincode_api::generated::ErrorDetails::new();
        if let Self::NotFound { delivery_id } | Self::AlreadyExists { delivery_id } = self {
            details.insert(
                "field".to_owned(),
                ErrorDetailValue::Variant4("deliveryId".to_owned()),
            );
            details.insert(
                "deliveryId".to_owned(),
                ErrorDetailValue::Variant4(delivery_id.0.clone()),
            );
        }
        details
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::PublicationPending { .. })
            || matches!(self, Self::Storage(error) if matches!(
                error.kind(),
                StorageErrorKind::RequestReplayMissing
                    | StorageErrorKind::JournalAlreadyExists
                    | StorageErrorKind::JournalConflict
                    | StorageErrorKind::EventCursorExpired
                    | StorageErrorKind::Adapter
                    | StorageErrorKind::Closed
            ))
    }

    #[must_use]
    pub fn committed_receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::PublicationPending { receipt, .. } => Some(receipt),
            Self::Storage(_) | Self::NotFound { .. } | Self::AlreadyExists { .. } => None,
        }
    }
}

impl From<StorageError> for DeliveryCommandCommitError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl fmt::Display for DeliveryCommandCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "Delivery command failed: {error}"),
            Self::NotFound { delivery_id } => {
                write!(formatter, "Delivery {} was not found", delivery_id.0)
            }
            Self::AlreadyExists { delivery_id } => {
                write!(formatter, "Delivery {} already exists", delivery_id.0)
            }
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "Delivery command committed, but publication remains pending: {source}"
            ),
        }
    }
}

impl std::error::Error for DeliveryCommandCommitError {}

/// Failure of the specialized atomic Delivery verdict command.
#[derive(Debug)]
pub enum DeliveryVerdictCommitError {
    /// Sealed candidate, verification, or Evidence facts were stale or invalid.
    Coordination(CoordinationError),
    /// No Delivery journal, state, receipt, or event fact committed.
    Storage(StorageError),
    /// The complete transaction committed; publication remains in the outbox.
    PublicationPending {
        receipt: Box<CommitReceipt>,
        source: OutboxError,
    },
}

impl DeliveryVerdictCommitError {
    #[must_use]
    pub fn committed_receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::PublicationPending { receipt, .. } => Some(receipt),
            Self::Coordination(_) | Self::Storage(_) => None,
        }
    }
}

impl fmt::Display for DeliveryVerdictCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coordination(error) => write!(formatter, "verdict computation failed: {error}"),
            Self::Storage(error) => write!(formatter, "verdict transaction failed: {error}"),
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "verdict transaction committed, but its event remains pending: {source}"
            ),
        }
    }
}

impl std::error::Error for DeliveryVerdictCommitError {}

impl From<StorageError> for DeliveryVerdictCommitError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Successful deterministic shutdown facts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub published_event_count: usize,
}

/// Shutdown failure. The lifecycle still attempts every close step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownError {
    failures: Vec<String>,
}

impl ShutdownError {
    #[must_use]
    pub fn failures(&self) -> &[String] {
        &self.failures
    }
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Control Plane shutdown had {} failure(s): {}",
            self.failures.len(),
            self.failures.join("; ")
        )
    }
}

impl std::error::Error for ShutdownError {}

/// Running Control Plane application host.
///
/// The host is deliberately synchronous in this phase: it does not detach
/// background tasks, so shutdown has a finite and observable ownership chain.
pub struct ControlPlane {
    storage: Option<Box<dyn ProductStateStorage>>,
    audit_store: Option<AuditStore>,
    artifact_store: Option<ArtifactStore>,
    git_source_resolver: Option<Box<dyn GitSourceResolver>>,
    publisher: Option<Box<dyn EventPublisher>>,
    temporary_root: Option<OwnedTemporaryRoot>,
    strongflow_sources: Option<strongflow_projection::StrongFlowProjectionSources>,
}

impl ControlPlane {
    /// Opens and migrates the local `SQLite` database, replays durable outbox
    /// events, and only then returns a running Control Plane.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing owned resources when storage open,
    /// migration, or durable outbox replay fails.
    #[allow(
        clippy::too_many_lines,
        reason = "startup keeps each owned resource's failure cleanup explicit"
    )]
    pub fn start_local(
        config: ControlPlaneConfig,
        mut publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let ControlPlaneConfig {
            data_directory,
            temporary_parent,
        } = config;
        let temporary_root = match OwnedTemporaryRoot::create(&temporary_parent) {
            Ok(temporary_root) => temporary_root,
            Err(error) => {
                let cleanup = close_publisher(&mut publisher);
                return Err(StartError::new(format!(
                    "failed to create the owned temporary root: {error}{cleanup}"
                )));
            }
        };
        let storage = match SqliteStorage::open(&data_directory) {
            Ok(storage) => storage,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                let cleanup = cleanup_suffix(&cleanup_failures);
                return Err(StartError::new(format!(
                    "failed to open Control Plane storage: {error}{cleanup}"
                )));
            }
        };
        let audit_store = match AuditStore::open(data_directory.join("audit")) {
            Ok(store) => store,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = Box::new(storage).close() {
                    cleanup_failures.push(format!("storage close also failed: {close_error}"));
                }
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                return Err(StartError::new(format!(
                    "failed to open immutable audit storage: {error}{}",
                    cleanup_suffix(&cleanup_failures)
                )));
            }
        };
        let object_store = match LocalArtifactObjectStore::open(data_directory.join("artifacts")) {
            Ok(store) => store,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = Box::new(storage).close() {
                    cleanup_failures.push(format!("storage close also failed: {close_error}"));
                }
                if let Err(close_error) = audit_store.close() {
                    cleanup_failures.push(format!("audit store close also failed: {close_error}"));
                }
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                return Err(StartError::new(format!(
                    "failed to open local Artifact object storage: {error}{}",
                    cleanup_suffix(&cleanup_failures)
                )));
            }
        };
        let artifact_store = match ArtifactStore::open(
            data_directory.join("artifact-catalog"),
            Box::new(object_store),
        ) {
            Ok(store) => store,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = Box::new(storage).close() {
                    cleanup_failures.push(format!("storage close also failed: {close_error}"));
                }
                if let Err(close_error) = audit_store.close() {
                    cleanup_failures.push(format!("audit store close also failed: {close_error}"));
                }
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                return Err(StartError::new(format!(
                    "failed to open Artifact metadata catalog: {error}{}",
                    cleanup_suffix(&cleanup_failures)
                )));
            }
        };
        Self::start_with_resources(
            Box::new(storage),
            Some(audit_store),
            Some(artifact_store),
            publisher,
            temporary_root,
        )
    }

    /// Composes the application with a storage adapter at the `PostgreSQL`-ready seam.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing both adapters when durable outbox
    /// replay fails.
    pub fn start(
        storage: Box<dyn ProductStateStorage>,
        mut publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let temporary_parent = std::env::temp_dir().join("winwincode-control-plane");
        let temporary_root = match OwnedTemporaryRoot::create(temporary_parent) {
            Ok(temporary_root) => temporary_root,
            Err(error) => {
                let mut failures = Vec::new();
                if let Err(close_error) = publisher.close() {
                    failures.push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(close_error) = storage.close() {
                    failures.push(format!("storage close also failed: {close_error}"));
                }
                return Err(StartError::new(format!(
                    "failed to create the owned temporary root: {error}{}",
                    cleanup_suffix(&failures)
                )));
            }
        };
        Self::start_with_resources(storage, None, None, publisher, temporary_root)
    }

    /// Composes the application with explicit product-state and Artifact adapters.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing every owned adapter when startup
    /// outbox replay fails.
    pub fn start_with_artifacts(
        storage: Box<dyn ProductStateStorage>,
        artifact_store: ArtifactStore,
        mut publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let temporary_parent = std::env::temp_dir().join("winwincode-control-plane");
        let temporary_root = match OwnedTemporaryRoot::create(temporary_parent) {
            Ok(temporary_root) => temporary_root,
            Err(error) => {
                let mut failures = Vec::new();
                if let Err(close_error) = publisher.close() {
                    failures.push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(close_error) = storage.close() {
                    failures.push(format!("storage close also failed: {close_error}"));
                }
                if let Err(close_error) = artifact_store.close() {
                    failures.push(format!("Artifact store close also failed: {close_error}"));
                }
                return Err(StartError::new(format!(
                    "failed to create the owned temporary root: {error}{}",
                    cleanup_suffix(&failures)
                )));
            }
        };
        Self::start_with_resources(
            storage,
            None,
            Some(artifact_store),
            publisher,
            temporary_root,
        )
    }

    fn start_with_resources(
        storage: Box<dyn ProductStateStorage>,
        audit_store: Option<AuditStore>,
        artifact_store: Option<ArtifactStore>,
        publisher: Box<dyn EventPublisher>,
        temporary_root: OwnedTemporaryRoot,
    ) -> Result<Self, StartError> {
        let mut control_plane = Self {
            storage: Some(storage),
            audit_store,
            artifact_store,
            git_source_resolver: None,
            publisher: Some(publisher),
            temporary_root: Some(temporary_root),
            strongflow_sources: None,
        };
        if let Err(error) = control_plane.flush_outbox() {
            let cleanup_failures = control_plane.close_resources();
            let cleanup = if cleanup_failures.is_empty() {
                String::new()
            } else {
                format!("; cleanup also failed: {}", cleanup_failures.join("; "))
            };
            return Err(StartError::new(format!(
                "failed to replay the durable outbox before startup: {error}{cleanup}"
            )));
        }
        Ok(control_plane)
    }

    /// Returns the instance-owned temporary root while the host is running.
    ///
    /// # Panics
    ///
    /// Panics only if the internal lifecycle invariant is broken and a running
    /// host has already lost ownership of its temporary root. Shutdown consumes
    /// the host, so callers cannot observe a normally released root through this
    /// method.
    #[must_use]
    pub fn temporary_root(&self) -> &Path {
        self.temporary_root
            .as_ref()
            .expect("a running Control Plane always owns a temporary root")
            .path()
    }

    /// Installs the trusted runtime-ledger and publication read adapters before
    /// the typed `StrongFlow` query port is exposed to a transport.
    ///
    /// # Errors
    ///
    /// Returns an error if an adapter set was already installed. Replacing a
    /// live authority would make previously issued read cursors ambiguous.
    pub fn install_strongflow_projection_sources(
        &mut self,
        sources: strongflow_projection::StrongFlowProjectionSources,
    ) -> Result<(), strongflow_projection::StrongFlowProjectionError> {
        if self.strongflow_sources.is_some() {
            return Err(
                strongflow_projection::StrongFlowProjectionError::invalid_request(
                    "StrongFlow projection sources are already installed",
                ),
            );
        }
        self.strongflow_sources = Some(sources);
        Ok(())
    }

    /// Installs the single trusted source resolver used to reconstruct Git
    /// candidate identities from complete Artifact bytes.
    ///
    /// # Errors
    ///
    /// Rejects replacement of a live resolver so one process cannot reinterpret
    /// an already accepted Artifact through another source authority.
    pub fn install_git_source_resolver(
        &mut self,
        resolver: Box<dyn GitSourceResolver>,
    ) -> Result<(), CandidateResolutionError> {
        if self.git_source_resolver.is_some() {
            return Err(CandidateResolutionError::Storage(
                StorageError::invalid_input("Git source resolver is already installed"),
            ));
        }
        self.git_source_resolver = Some(resolver);
        Ok(())
    }

    /// Commits one canonical HTTP command's state and outbox first, then
    /// publishes pending events.
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::Storage`] when nothing was committed, or
    /// [`CommitError::PublicationPending`] when state is durable and its event
    /// must be replayed.
    pub fn commit(
        &mut self,
        command: &CommandEnvelope,
        change: StateChange,
    ) -> Result<CommitReceipt, CommitError> {
        if delivery_command(&command.command)
            || change.stream_id.starts_with("delivery:")
            || change.stream_id.starts_with("runtime:")
        {
            return Err(CommitError::Storage(StorageError::invalid_input(
                "Delivery and runtime state streams require a typed atomic transaction",
            )));
        }
        if change.events.iter().any(|event| {
            event.projection_stream().is_some()
                || reserved_public_projection_topic(&event.topic)
                || reserved_delivery_transaction_topic(&event.topic)
        }) {
            return Err(CommitError::Storage(StorageError::invalid_input(
                "Delivery and public projection events require a typed Control Plane transaction",
            )));
        }
        let commit = storage_commit(command, change).map_err(CommitError::Storage)?;
        let receipt = self
            .storage_mut()
            .map_err(CommitError::Storage)?
            .commit(&commit)
            .map_err(CommitError::Storage)?;
        drop(commit);
        self.flush_outbox()
            .map_err(|source| CommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Atomically commits one canonical non-dispatch Delivery command and its
    /// public Delivery event through the single typed Delivery transaction.
    ///
    /// # Errors
    ///
    /// Returns before persistence for an unsupported command, invalid scope,
    /// stale revision, or failed atomic member. Publication failure retains
    /// the committed event for startup replay.
    pub fn commit_delivery_command(
        &mut self,
        command: &CommandEnvelope,
        facts: &DeliveryCommandFacts,
    ) -> Result<CommitReceipt, DeliveryCommandCommitError> {
        let receipt = {
            let storage = self
                .storage_mut()
                .map_err(DeliveryCommandCommitError::Storage)?;
            delivery_command_transaction::execute(storage, command, facts)?
        };
        self.flush_outbox()
            .map_err(|source| DeliveryCommandCommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Atomically commits one `delivery.advance` journal record, canonical
    /// snapshot, scoped command receipt, and execution-job outbox intent before
    /// offering the exact committed job to the `ExecutionPort` dispatcher.
    ///
    /// # Errors
    ///
    /// Returns without dispatch when any pre-commit member fails. A dispatch or
    /// acknowledgement failure carries the committed receipt and leaves the
    /// durable event pending for startup replay.
    pub fn commit_delivery_execution(
        &mut self,
        command: &CommandEnvelope,
        pending: &PendingDeliveryExecution,
        dispatcher: &mut dyn ExecutionJobDispatcher,
    ) -> Result<DeliveryExecutionDispatchReceipt, DeliveryExecutionError> {
        let receipt = {
            let storage = self.storage_mut().map_err(|error| {
                DeliveryExecutionError::Commit(DeliveryExecutionPortError::new(error.to_string()))
            })?;
            delivery_transaction::execute(storage, command, pending, dispatcher)?
        };
        self.flush_outbox().map_err(|source| {
            DeliveryExecutionError::ProjectionPublicationAfterDispatch {
                commit: Box::new(receipt.commit.clone()),
                source: DeliveryExecutionPortError::new(source.to_string()),
            }
        })?;
        Ok(receipt)
    }

    /// Persists one authoritative Worker `session.binding` message as the two
    /// canonical Delivery mutations that attach its `WorkerSession` and then its
    /// `CodexThread`.
    ///
    /// The generated message is the only wire input. The second argument is an
    /// opaque scheduler-owned `SessionBinding` authority; the message cannot
    /// authorize itself.
    ///
    /// # Errors
    ///
    /// Returns before the first write for a foreign job, stale binding, lease
    /// mismatch, or malformed message. If the `WorkerSession` phase committed
    /// but the `CodexThread` phase failed, the returned error carries the first
    /// durable receipt so an exact retry can continue receipt-first.
    pub fn commit_delivery_session_binding(
        &mut self,
        message: &execution_port::SessionBindingMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
    ) -> Result<DeliverySessionBindingCommitReceipt, DeliverySessionBindingCommitError> {
        let commit = {
            let storage = self
                .storage_mut()
                .map_err(DeliverySessionBindingCommitError::Storage)?;
            session_binding_transaction::execute(storage, message, authority)?
        };
        self.flush_outbox().map_err(|source| {
            DeliverySessionBindingCommitError::PublicationPending {
                commit: Box::new(commit.clone()),
                source,
            }
        })?;
        Ok(commit)
    }

    /// Accepts one generated Worker `runtime.event` only after joining its
    /// durable Delivery-stage `ExecutionJob` to the current four-identity
    /// `SessionBinding`. Rejected input returns a stable generated ack and
    /// does not write Delivery state, journal, receipt, or outbox members.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the runtime ledger cannot be read or
    /// committed, or a publication-pending error when an accepted ack was
    /// committed but its outbox notification still needs flushing.
    pub fn accept_runtime_event(
        &mut self,
        scope: &RepositoryScope,
        message: &execution_port::RuntimeEventMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
    ) -> Result<execution_port::RuntimeAckMessage, RuntimeMessageError> {
        let ack = {
            let storage = self.storage_mut().map_err(RuntimeMessageError::Storage)?;
            runtime_event_transaction::execute(storage, scope, message, authority)?
        };
        if !matches!(
            ack.status,
            execution_port::LeaseWriteStatus::Accepted
                | execution_port::LeaseWriteStatus::Duplicate
        ) {
            return Ok(ack);
        }
        self.flush_outbox()
            .map_err(|source| RuntimeMessageError::PublicationPending {
                ack: Box::new(ack.clone()),
                source,
            })?;
        Ok(ack)
    }

    /// Accepts one generated `artifact.open` after exact durable Job, scope,
    /// `SessionBinding`, and scheduler authority validation.
    ///
    /// # Errors
    ///
    /// Returns before metadata persistence for a foreign/stale lease, missing
    /// durable Job, incomplete `SessionBinding`, or invalid immutable descriptor.
    pub fn accept_artifact_open(
        &mut self,
        scope: &RepositoryScope,
        message: &execution_port::ArtifactOpenMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
    ) -> Result<execution_port::ArtifactAckMessage, ArtifactMessageError> {
        let storage = self.storage.as_deref().ok_or_else(|| {
            ArtifactMessageError::Storage(StorageError::adapter("Control Plane storage is closed"))
        })?;
        let artifacts = self.artifact_store.as_mut().ok_or_else(|| {
            ArtifactMessageError::Storage(StorageError::adapter(
                "Control Plane Artifact store is not configured",
            ))
        })?;
        artifact_transaction::accept_open(storage, artifacts, scope, message, authority)
    }

    /// Accepts one generated `artifact.chunk` through the same lease-scoped
    /// authority and content-addressed Artifact interface as `artifact.open`.
    ///
    /// # Errors
    ///
    /// Returns without accepting bytes for a gap, changed chunk, invalid
    /// digest/base64 payload, stale lease, or foreign Artifact identity.
    pub fn accept_artifact_chunk(
        &mut self,
        scope: &RepositoryScope,
        message: &execution_port::ArtifactChunkMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
    ) -> Result<execution_port::ArtifactAckMessage, ArtifactMessageError> {
        let storage = self.storage.as_deref().ok_or_else(|| {
            ArtifactMessageError::Storage(StorageError::adapter("Control Plane storage is closed"))
        })?;
        let artifacts = self.artifact_store.as_mut().ok_or_else(|| {
            ArtifactMessageError::Storage(StorageError::adapter(
                "Control Plane Artifact store is not configured",
            ))
        })?;
        artifact_transaction::accept_chunk(storage, artifacts, scope, message, authority)
    }

    /// Rebuilds and freezes the current Delivery candidate from one exact,
    /// complete Artifact and its successful fenced Worker outcome.
    ///
    /// This is a derived read: it does not append Delivery state or let a
    /// caller supply commit/tree/diff/path identities.
    ///
    /// # Errors
    ///
    /// Rejects missing/foreign/corrupt Artifact bytes, mismatched scope or
    /// provenance, stale terminal facts, and source identities that differ
    /// from the current Delivery Spec.
    pub fn resolve_delivery_candidate(
        &self,
        scope: &RepositoryScope,
        delivery_id: &DeliveryId,
        artifact_id: &winwincode_domain::ArtifactId,
        artifact_digest: &Sha256Digest,
        terminal_facts: &winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts,
    ) -> Result<winwincode_delivery::domain::FrozenDeliveryCandidate, CandidateResolutionError>
    {
        let storage = self.storage.as_deref().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane storage is closed",
            ))
        })?;
        let artifacts = self.artifact_store.as_ref().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane Artifact store is not configured",
            ))
        })?;
        let resolver = self.git_source_resolver.as_deref().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane Git source resolver is not configured",
            ))
        })?;
        candidate_source::resolve(
            storage,
            artifacts,
            resolver,
            scope,
            delivery_id,
            artifact_id,
            artifact_digest,
            terminal_facts,
        )
    }

    /// Persists one lease-fenced Worker `job.outcome` through the only typed
    /// terminal Delivery transaction.
    ///
    /// Receipt replay is resolved before current Delivery, journal, durable job,
    /// or replacement authority is read. A new message is joined to its exact
    /// durable dispatch intent and opaque scheduler/Worker facts before the
    /// canonical terminal transition is committed.
    ///
    /// # Errors
    ///
    /// Returns before persistence for a stale/foreign lease, binding, thread,
    /// sequence, Artifact, message time, or non-terminal stage transition.
    pub fn commit_delivery_terminal_outcome(
        &mut self,
        scope: &RepositoryScope,
        message: &execution_port::JobOutcomeMessage,
        facts: &winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts,
    ) -> Result<DeliveryTerminalOutcomeCommitReceipt, DeliveryTerminalOutcomeCommitError> {
        let commit = {
            let storage = self
                .storage_mut()
                .map_err(DeliveryTerminalOutcomeCommitError::Storage)?;
            terminal_outcome_transaction::execute(storage, scope, message, facts)?
        };
        self.flush_outbox().map_err(|source| {
            DeliveryTerminalOutcomeCommitError::PublicationPending {
                commit: Box::new(commit.clone()),
                source,
            }
        })?;
        Ok(commit)
    }

    /// Atomically commits a constructor-derived bounded-rework clarification
    /// without creating or dispatching an `ExecutionJob`.
    ///
    /// # Errors
    ///
    /// Returns a storage error before any durable write when the command,
    /// transition, revision, receipt, or journal publication is not exact.
    pub fn commit_delivery_rework_clarification(
        &mut self,
        command: &CommandEnvelope,
        transition: &winwincode_delivery::application::stage::StageAdvanceResult,
    ) -> Result<CommitReceipt, CommitError> {
        let receipt = {
            let storage = self.storage_mut().map_err(CommitError::Storage)?;
            rework_transaction::execute(storage, command, transition)
                .map_err(CommitError::Storage)?
        };
        self.flush_outbox()
            .map_err(|source| CommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Atomically promotes the exact ordered task graph sealed by the current
    /// approved solution review, then publishes only its committed outbox row.
    ///
    /// # Errors
    ///
    /// Returns before persistence for a stale review, changed revision, or any
    /// failed atomic member. Publication failure retains the committed event
    /// for replay.
    pub fn commit_delivery_task_breakdown(
        &mut self,
        command: &CommandEnvelope,
    ) -> Result<CommitReceipt, CommitError> {
        let receipt = {
            let storage = self.storage_mut().map_err(CommitError::Storage)?;
            task_breakdown_transaction::execute(storage, command).map_err(CommitError::Storage)?
        };
        self.flush_outbox()
            .map_err(|source| CommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Recomputes and atomically commits one Delivery's Evidence, Verdict,
    /// blocking Attention, task state, status, scoped receipt, journal record,
    /// and immutable outbox event.
    ///
    /// # Errors
    ///
    /// Returns before persistence for stale authoritative facts or when any
    /// atomic member fails. Publication failure carries the committed receipt
    /// and leaves the one event pending for replay.
    pub fn commit_delivery_verdict(
        &mut self,
        command: &CommandEnvelope,
        facts: SubmitVerdictFacts<'_>,
    ) -> Result<CommitReceipt, DeliveryVerdictCommitError> {
        let receipt = {
            let storage = self
                .storage_mut()
                .map_err(DeliveryVerdictCommitError::Storage)?;
            verdict_transaction::execute(storage, command, facts)?
        };
        self.flush_outbox()
            .map_err(|source| DeliveryVerdictCommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Loads canonical state through the configured storage adapter.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the adapter read fails.
    pub fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        self.storage_ref()?.load_state(stream_id)
    }

    /// Stops accepting work by consuming the host, flushes the outbox, closes
    /// the publisher, and finally closes storage.
    ///
    /// # Errors
    ///
    /// Returns [`ShutdownError`] after attempting every close step when outbox
    /// flush or adapter close fails.
    pub fn shutdown(mut self) -> Result<ShutdownReport, ShutdownError> {
        let mut failures = Vec::new();
        let published_event_count = match self.flush_outbox() {
            Ok(count) => count,
            Err(error) => {
                failures.push(format!("outbox flush failed: {error}"));
                0
            }
        };
        failures.extend(self.close_resources());
        if failures.is_empty() {
            Ok(ShutdownReport {
                published_event_count,
            })
        } else {
            Err(ShutdownError { failures })
        }
    }

    fn flush_outbox(&mut self) -> Result<usize, OutboxError> {
        self.flush_pending_audit_events()?;
        let events = self
            .storage_ref()
            .map_err(OutboxError::Acknowledge)?
            .pending_events()
            .map_err(OutboxError::Acknowledge)?;
        let mut published = 0;
        for event in events {
            self.publisher_mut()
                .map_err(|error| OutboxError::Publish(EventPublishError::new(error.to_string())))?
                .publish(&event)
                .map_err(OutboxError::Publish)?;
            self.storage_mut()
                .map_err(OutboxError::Acknowledge)?
                .mark_published(&event.event_id)
                .map_err(OutboxError::Acknowledge)?;
            published += 1;
        }
        Ok(published)
    }

    /// Flushes the durable audit bridge into the one immutable audit hash
    /// chain. The payload is a complete canonical `AuditEvent`; the `SQLite`
    /// row is only a pending marker and remains for idempotent crash recovery.
    fn flush_pending_audit_events(&mut self) -> Result<(), OutboxError> {
        let pending = self
            .storage_ref()
            .map_err(OutboxError::Acknowledge)?
            .pending_audit_events()
            .map_err(OutboxError::Acknowledge)?;
        for pending_event in pending {
            let event: AuditEvent =
                serde_json::from_slice(pending_event.payload()).map_err(|_| {
                    OutboxError::Acknowledge(StorageError::adapter(
                        "pending audit event payload is not canonical JSON",
                    ))
                })?;
            let canonical_payload = serde_json::to_vec(&event).map_err(|_| {
                OutboxError::Acknowledge(StorageError::adapter(
                    "pending audit event cannot be canonically encoded",
                ))
            })?;
            if canonical_payload != pending_event.payload()
                || event.event_id().as_str() != pending_event.event_id()
            {
                return Err(OutboxError::Acknowledge(StorageError::invalid_input(
                    "pending audit event does not match its canonical event identity",
                )));
            }
            self.audit_store
                .as_mut()
                .ok_or_else(|| OutboxError::Audit(winwincode_audit::AuditError::unavailable()))?
                .append(&event)
                .map_err(OutboxError::Audit)?;
            self.storage_mut()
                .map_err(OutboxError::Acknowledge)?
                .mark_audit_event_persisted(pending_event.event_id())
                .map_err(OutboxError::Acknowledge)?;
        }
        Ok(())
    }

    fn storage_ref(&self) -> Result<&dyn ProductStateStorage, StorageError> {
        self.storage
            .as_deref()
            .ok_or_else(|| StorageError::adapter("Control Plane storage is closed"))
    }

    fn storage_mut(&mut self) -> Result<&mut (dyn ProductStateStorage + 'static), StorageError> {
        self.storage
            .as_deref_mut()
            .ok_or_else(|| StorageError::adapter("Control Plane storage is closed"))
    }

    fn publisher_mut(&mut self) -> Result<&mut (dyn EventPublisher + 'static), StartError> {
        self.publisher
            .as_deref_mut()
            .ok_or_else(|| StartError::new("Control Plane event publisher is closed"))
    }

    fn close_resources(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        if let Some(mut publisher) = self.publisher.take()
            && let Err(error) = publisher.close()
        {
            failures.push(format!("event publisher close failed: {error}"));
        }
        if let Some(storage) = self.storage.take()
            && let Err(error) = storage.close()
        {
            failures.push(format!("storage close failed: {error}"));
        }
        if let Some(audit_store) = self.audit_store.take()
            && let Err(error) = audit_store.close()
        {
            failures.push(format!("audit store close failed: {error}"));
        }
        if let Some(artifact_store) = self.artifact_store.take()
            && let Err(error) = artifact_store.close()
        {
            failures.push(format!("Artifact store close failed: {error}"));
        }
        if let Some(temporary_root) = self.temporary_root.take()
            && let Err(error) = temporary_root.release()
        {
            failures.push(format!("temporary root release failed: {error}"));
        }
        failures
    }
}

fn reserved_public_projection_topic(topic: &str) -> bool {
    serde_json::from_value::<ControlPlaneWebSocketEventType>(serde_json::Value::String(
        topic.to_owned(),
    ))
    .is_ok()
}

fn reserved_delivery_transaction_topic(topic: &str) -> bool {
    topic.starts_with("delivery.") || topic.starts_with("runtime.")
}

pub(crate) fn storage_commit(
    command: &CommandEnvelope,
    change: StateChange,
) -> Result<StateCommit, StorageError> {
    let (receipt_identity, command_digest) = command_receipt(command)?;
    let expected_revision = u64::try_from(command.expected_revision.0).map_err(|_| {
        StorageError::invalid_input("command expectedRevision must not be negative")
    })?;

    Ok(StateCommit::new(
        receipt_identity,
        command_digest,
        change.stream_id,
        expected_revision,
        change.state,
        change.events,
    ))
}

const EXECUTION_AUDIT_SYSTEM_ACTOR: &str = "sys_00000000000000000000000000";
const EXECUTION_AUDIT_ORIGIN: &str = "control-plane.execution-port";

/// Encodes one complete execution audit event for the durable storage bridge.
///
/// The storage crate receives only the canonical event bytes and its event id;
/// all event semantics remain owned by `winwincode-audit`, and the immutable
/// `AuditStore` is the sole audit authority after the bridge is flushed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execution_audit_event(
    event_id: AuditEventId,
    occurred_at_millis: u64,
    request_id: winwincode_domain::RequestId,
    scope: &RepositoryScope,
    action: AuditAction,
    before: &Delivery,
    after: &Delivery,
    subject: AuditSubject,
    result_code: &str,
) -> Result<PendingAuditEvent, StorageError> {
    let before = delivery_state_digest(before)?;
    let after = delivery_state_digest(after)?;
    let state = AuditState::changed(Some(before), after).map_err(|error| {
        StorageError::invalid_input(format!("execution audit state is invalid: {error}"))
    })?;
    execution_audit_event_with_state(
        event_id,
        occurred_at_millis,
        request_id,
        scope,
        action,
        state,
        subject,
        result_code,
    )
}

/// Encodes one execution audit event from a state transition owned by the
/// caller. Runtime ledger transitions use this seam because they do not mutate
/// the Delivery aggregate.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execution_audit_event_with_state(
    event_id: AuditEventId,
    occurred_at_millis: u64,
    request_id: winwincode_domain::RequestId,
    scope: &RepositoryScope,
    action: AuditAction,
    state: AuditState,
    subject: AuditSubject,
    result_code: &str,
) -> Result<PendingAuditEvent, StorageError> {
    let scope = AuditScope::repository(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .map_err(|error| {
        StorageError::invalid_input(format!("execution audit scope is invalid: {error}"))
    })?;
    let origin = AuditOrigin::local(EXECUTION_AUDIT_ORIGIN).map_err(|error| {
        StorageError::invalid_input(format!("execution audit origin is invalid: {error}"))
    })?;
    let event = AuditEvent::state_change(
        event_id,
        occurred_at_millis,
        AuditActor::System(SystemActorId(EXECUTION_AUDIT_SYSTEM_ACTOR.to_owned())),
        scope,
        request_id,
        action,
        state,
        origin,
        subject,
        result_code,
        AuditRetention::Indefinite,
    )
    .map_err(|error| {
        StorageError::invalid_input(format!("execution audit event is invalid: {error}"))
    })?;
    let event_id = event.event_id().as_str().to_owned();
    let payload = serde_json::to_vec(&event).map_err(|error| {
        StorageError::adapter(format!("failed to encode execution audit event: {error}"))
    })?;
    PendingAuditEvent::new(event_id, payload)
}

fn delivery_state_digest(delivery: &Delivery) -> Result<Sha256Digest, StorageError> {
    let payload = delivery.encode_json().map_err(|error| {
        StorageError::adapter(format!("failed to encode Delivery for audit: {error}"))
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(payload)
    )))
}

/// Reconstructs the repository scope encoded in a durable receipt key. This
/// is used only where the generated Worker binding message carries no scope;
/// malformed or non-repository keys are rejected rather than guessed.
pub(crate) fn repository_scope_from_receipt_key(
    key: &ReceiptScopeKey,
) -> Result<RepositoryScope, StorageError> {
    let mut offset = 0;
    let prefix = decode_key_field(key.as_bytes(), &mut offset)?;
    let tag = decode_key_field(key.as_bytes(), &mut offset)?;
    let organization_id = decode_key_field(key.as_bytes(), &mut offset)?;
    let workspace_id = decode_key_field(key.as_bytes(), &mut offset)?;
    let project_id = decode_key_field(key.as_bytes(), &mut offset)?;
    let repository_id = decode_key_field(key.as_bytes(), &mut offset)?;
    if offset != key.as_bytes().len() || prefix != SCOPE_KEY_PREFIX || tag != b"repository" {
        return Err(StorageError::invalid_input(
            "receipt scope key is not a canonical repository scope",
        ));
    }
    let organization_id = String::from_utf8(organization_id).map_err(|_| {
        StorageError::invalid_input("repository scope organization id is not UTF-8")
    })?;
    let workspace_id = String::from_utf8(workspace_id)
        .map_err(|_| StorageError::invalid_input("repository scope workspace id is not UTF-8"))?;
    let project_id = String::from_utf8(project_id)
        .map_err(|_| StorageError::invalid_input("repository scope project id is not UTF-8"))?;
    let repository_id = String::from_utf8(repository_id)
        .map_err(|_| StorageError::invalid_input("repository scope repository id is not UTF-8"))?;
    require_canonical_id(&organization_id, "org_", "repository scope organizationId")?;
    require_canonical_id(&workspace_id, "wsp_", "repository scope workspaceId")?;
    require_canonical_id(&project_id, "prj_", "repository scope projectId")?;
    require_canonical_id(&repository_id, "rep_", "repository scope repositoryId")?;
    Ok(RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(organization_id),
        workspace_id: WorkspaceId(workspace_id),
        project_id: ProjectId(project_id),
        repository_id: RepositoryId(repository_id),
    })
}

fn decode_key_field(bytes: &[u8], offset: &mut usize) -> Result<Vec<u8>, StorageError> {
    let length_bytes = bytes
        .get(*offset..(*offset).saturating_add(8))
        .ok_or_else(|| {
            StorageError::invalid_input("receipt scope key has a truncated field length")
        })?;
    let length = u64::from_be_bytes(
        length_bytes
            .try_into()
            .expect("receipt scope key length is exactly eight bytes"),
    );
    *offset = (*offset)
        .checked_add(8)
        .ok_or_else(|| StorageError::invalid_input("receipt scope key offset overflow"))?;
    let length = usize::try_from(length)
        .map_err(|_| StorageError::invalid_input("receipt scope key field is too large"))?;
    let end = (*offset)
        .checked_add(length)
        .ok_or_else(|| StorageError::invalid_input("receipt scope key field overflows"))?;
    let field = bytes
        .get(*offset..end)
        .ok_or_else(|| StorageError::invalid_input("receipt scope key has a truncated field"))?;
    *offset = end;
    Ok(field.to_vec())
}

pub(crate) fn delivery_changed_event(
    command: &CommandEnvelope,
    delivery_id: &DeliveryId,
    delivery_revision: u64,
    change_kind: DeliveryChangeKind,
) -> Result<NewOutboxEvent, StorageError> {
    let Scope::RepositoryScope(scope) = &command.scope else {
        return Err(StorageError::invalid_input(
            "Delivery change events require repository scope",
        ));
    };
    let scope_key = repository_scope_key(scope)?;
    delivery_changed_event_for_scope(&scope_key, delivery_id, delivery_revision, change_kind)
}

pub(crate) fn delivery_changed_event_for_scope(
    scope_key: &ReceiptScopeKey,
    delivery_id: &DeliveryId,
    delivery_revision: u64,
    change_kind: DeliveryChangeKind,
) -> Result<NewOutboxEvent, StorageError> {
    let revision = i64::try_from(delivery_revision)
        .map(Revision)
        .map_err(|_| StorageError::invalid_input("Delivery revision exceeds the public range"))?;
    let payload = ControlPlaneWebSocketDeliveryChangedEvent {
        change_kind: change_kind.as_str().to_owned(),
        delivery_id: delivery_id.clone(),
        revision,
        type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    };
    let payload = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!("failed to encode Delivery change event: {error}"))
    })?;
    let topic = delivery_changed_topic()?;
    let event_id = delivery_changed_event_id(scope_key, &payload);
    Ok(NewOutboxEvent::projection(
        event_id,
        topic,
        payload,
        ProjectionEventStream::Delivery(delivery_id.clone()),
    ))
}

pub(crate) fn validate_delivery_changed_receipt(
    receipt: &CommitReceipt,
    delivery_id: &DeliveryId,
    delivery_revision: u64,
    change_kind: DeliveryChangeKind,
) -> Result<(), StorageError> {
    let topic = delivery_changed_topic()?;
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == topic)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(StorageError::invalid_input(
            "durable receipt must contain exactly one Delivery change event",
        ));
    };
    let payload: ControlPlaneWebSocketDeliveryChangedEvent = serde_json::from_slice(&event.payload)
        .map_err(|_| {
            StorageError::invalid_input("durable Delivery change event is not canonical")
        })?;
    if serde_json::to_vec(&payload).map_err(|_| {
        StorageError::invalid_input("durable Delivery change event is not canonical")
    })? != event.payload
        || payload.type_value
            != ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1
        || payload.delivery_id != *delivery_id
        || payload.revision.0 != i64::try_from(delivery_revision).unwrap_or(-1)
        || payload.change_kind != change_kind.as_str()
    {
        return Err(StorageError::invalid_input(
            "durable Delivery change event does not match committed Delivery facts",
        ));
    }
    let cursor = event.projection_cursor.as_ref().ok_or_else(|| {
        StorageError::invalid_input("durable Delivery change event has no stream cursor")
    })?;
    if cursor.sequence() == 0
        || cursor.event_id().map(|value| value.0.as_str()) != Some(event.event_id.as_str())
        || cursor.key().scope_key() != receipt.receipt_identity.scope_key()
        || cursor.key().stream() != &ProjectionEventStream::Delivery(delivery_id.clone())
        || event.event_id
            != delivery_changed_event_id(receipt.receipt_identity.scope_key(), &event.payload).0
    {
        return Err(StorageError::invalid_input(
            "durable Delivery change event cursor is not exact",
        ));
    }
    Ok(())
}

fn delivery_changed_topic() -> Result<String, StorageError> {
    match serde_json::to_value(
        ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    )
    .map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode the generated Delivery change event type: {error}"
        ))
    })? {
        serde_json::Value::String(topic) => Ok(topic),
        _ => Err(StorageError::adapter(
            "generated Delivery change event type did not encode as a string",
        )),
    }
}

fn delivery_changed_event_id(scope_key: &ReceiptScopeKey, payload: &[u8]) -> ControlPlaneEventId {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.delivery-changed-event.v1\0");
    digest.update((scope_key.as_bytes().len() as u64).to_be_bytes());
    digest.update(scope_key.as_bytes());
    digest.update((payload.len() as u64).to_be_bytes());
    digest.update(payload);
    ControlPlaneEventId(format!("evt_{:x}", digest.finalize()))
}

pub(crate) fn command_receipt(
    command: &CommandEnvelope,
) -> Result<(ReceiptIdentity, Sha256Digest), StorageError> {
    let actor_key = receipt_actor_key(&command.actor)?;
    let scope_key = receipt_scope_key(&command.scope)?;
    require_canonical_id(&command.request_id.0, "req_", "command requestId")?;
    let receipt_identity = ReceiptIdentity::new(actor_key, scope_key, command.request_id.clone())?;
    let serialized = serde_json::to_vec(command).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode the canonical command digest: {error}"
        ))
    })?;
    let digest = Sha256::digest(serialized);
    let command_digest = Sha256Digest(format!("sha256:{digest:x}"));
    Ok((receipt_identity, command_digest))
}

fn delivery_command(command: &CommandName) -> bool {
    matches!(
        command,
        CommandName::DeliveryCreate
            | CommandName::DeliveryUpdateSpec
            | CommandName::DeliveryApproveTaskBreakdown
            | CommandName::DeliveryAdvance
            | CommandName::DeliveryResolveAttention
            | CommandName::DeliverySubmitVerdict
    )
}

fn receipt_actor_key(actor: &Actor) -> Result<ReceiptActorKey, StorageError> {
    let (tag, id) = match actor {
        Actor::UserActor(actor) => {
            require_canonical_id(&actor.id.0, "usr_", "command user actor id")?;
            (b"user".as_slice(), actor.id.0.as_str())
        }
        Actor::ServiceAccountActor(actor) => {
            require_canonical_id(&actor.id.0, "svc_", "command service account actor id")?;
            (b"service_account".as_slice(), actor.id.0.as_str())
        }
        Actor::SystemActor(actor) => {
            require_canonical_id(&actor.id.0, "sys_", "command system actor id")?;
            (b"system".as_slice(), actor.id.0.as_str())
        }
    };
    ReceiptActorKey::from_encoded(encode_key(ACTOR_KEY_PREFIX, tag, &[id]))
}

fn receipt_scope_key(scope: &Scope) -> Result<ReceiptScopeKey, StorageError> {
    let encoded = match scope {
        Scope::OrganizationScope(scope) => {
            require_canonical_id(
                &scope.organization_id.0,
                "org_",
                "command scope organizationId",
            )?;
            encode_key(
                SCOPE_KEY_PREFIX,
                b"organization",
                &[scope.organization_id.0.as_str()],
            )
        }
        Scope::WorkspaceScope(scope) => {
            require_canonical_id(
                &scope.organization_id.0,
                "org_",
                "command scope organizationId",
            )?;
            require_canonical_id(&scope.workspace_id.0, "wsp_", "command scope workspaceId")?;
            encode_key(
                SCOPE_KEY_PREFIX,
                b"workspace",
                &[
                    scope.organization_id.0.as_str(),
                    scope.workspace_id.0.as_str(),
                ],
            )
        }
        Scope::ProjectScope(scope) => {
            require_canonical_id(
                &scope.organization_id.0,
                "org_",
                "command scope organizationId",
            )?;
            require_canonical_id(&scope.workspace_id.0, "wsp_", "command scope workspaceId")?;
            require_canonical_id(&scope.project_id.0, "prj_", "command scope projectId")?;
            encode_key(
                SCOPE_KEY_PREFIX,
                b"project",
                &[
                    scope.organization_id.0.as_str(),
                    scope.workspace_id.0.as_str(),
                    scope.project_id.0.as_str(),
                ],
            )
        }
        Scope::RepositoryScope(scope) => {
            return repository_scope_key(scope);
        }
    };
    ReceiptScopeKey::from_encoded(encoded)
}

pub(crate) fn repository_scope_key(
    scope: &RepositoryScope,
) -> Result<ReceiptScopeKey, StorageError> {
    require_canonical_id(
        &scope.organization_id.0,
        "org_",
        "command scope organizationId",
    )?;
    require_canonical_id(&scope.workspace_id.0, "wsp_", "command scope workspaceId")?;
    require_canonical_id(&scope.project_id.0, "prj_", "command scope projectId")?;
    require_canonical_id(&scope.repository_id.0, "rep_", "command scope repositoryId")?;
    ReceiptScopeKey::from_encoded(encode_key(
        SCOPE_KEY_PREFIX,
        b"repository",
        &[
            scope.organization_id.0.as_str(),
            scope.workspace_id.0.as_str(),
            scope.project_id.0.as_str(),
            scope.repository_id.0.as_str(),
        ],
    ))
}

fn require_canonical_id(value: &str, prefix: &str, label: &str) -> Result<(), StorageError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(StorageError::invalid_input(format!(
            "{label} is not canonical"
        )));
    };
    if suffix.len() != 26 || !suffix.bytes().all(is_crockford_base32) {
        return Err(StorageError::invalid_input(format!(
            "{label} is not canonical"
        )));
    }
    Ok(())
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn encode_key(prefix: &[u8], tag: &[u8], values: &[&str]) -> Vec<u8> {
    let mut encoded = Vec::new();
    append_key_field(&mut encoded, prefix);
    append_key_field(&mut encoded, tag);
    for value in values {
        append_key_field(&mut encoded, value.as_bytes());
    }
    encoded
}

fn append_key_field(encoded: &mut Vec<u8>, value: &[u8]) {
    encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
    encoded.extend_from_slice(value);
}

const OWNERSHIP_MARKER: &str = ".winwincode-control-plane-owner";
static NEXT_TEMPORARY_ROOT_ID: AtomicU64 = AtomicU64::new(1);

struct OwnedTemporaryRoot {
    path: PathBuf,
    marker: String,
}

impl OwnedTemporaryRoot {
    fn create(parent: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let parent = parent.as_ref();
        fs::create_dir_all(parent)?;
        // Phase 2.1 deliberately does not enumerate or remove roots left by a
        // previous process. A PID or old-looking marker is not proof that a
        // lease is stale. Each instance only creates and releases its own exact
        // marker/path pair until a real renewable lease is introduced.
        loop {
            let instance_id = NEXT_TEMPORARY_ROOT_ID.fetch_add(1, Ordering::Relaxed);
            let marker = format!(
                "winwincode-control-plane\npid={}\ninstance={instance_id}\n",
                std::process::id()
            );
            let path = parent.join(format!("instance-{}-{instance_id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let marker_path = path.join(OWNERSHIP_MARKER);
                    let marker_result = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&marker_path)
                        .and_then(|mut file| {
                            file.write_all(marker.as_bytes())?;
                            file.sync_all()
                        });
                    if let Err(error) = marker_result {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error);
                    }
                    return Ok(Self { path, marker });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn release(self) -> Result<(), std::io::Error> {
        let marker_path = self.path.join(OWNERSHIP_MARKER);
        let actual_marker = fs::read_to_string(&marker_path)?;
        if actual_marker != self.marker {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "temporary root ownership marker does not match this instance",
            ));
        }
        fs::remove_dir_all(self.path)
    }
}

fn close_publisher(publisher: &mut Box<dyn EventPublisher>) -> String {
    publisher.close().err().map_or_else(String::new, |error| {
        format!("; event publisher close also failed: {error}")
    })
}

fn cleanup_suffix(failures: &[String]) -> String {
    if failures.is_empty() {
        String::new()
    } else {
        format!("; cleanup also failed: {}", failures.join("; "))
    }
}
