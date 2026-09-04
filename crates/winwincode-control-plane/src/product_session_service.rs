// SPDX-License-Identifier: Apache-2.0

//! Durable multi-`ProductSession` commands and exact runtime bindings.
//!
//! The service owns application commands only. `ProductSession` lifecycle and
//! `SessionBinding` identity rules remain in `winwincode-session`; canonical
//! state, scoped command receipts, and events remain in `winwincode-storage`;
//! Worker capacity and runtime identity remain in the existing durable Worker
//! slot and execution-admission records.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ChatMessageProjection, ControlPlaneWebSocketProductSessionChangedEvent,
    ControlPlaneWebSocketProductSessionChangedEventTypeValue,
    ControlPlaneWebSocketProductSessionMessageAppendedEvent,
    ControlPlaneWebSocketProductSessionMessageAppendedEventTypeValue, ProductSessionProjection,
    ProviderAccountSource, RepositoryScope, SessionCloseCommand, SessionCreateCommand,
    SessionModelSelection,
};
use winwincode_domain::{
    ChatMessageId, ControlPlaneEventId, DeliveryId, DeliveryTaskId, ExecutionJobId, Instant,
    ModelExchangeId, ProductSessionId, ProjectId, RepositoryId, RequestId, Revision, Sha256Digest,
    StageRunId, WorkerSessionId,
};
use winwincode_session::{
    AuthenticatedActor, BindingScope, ExecutionCancellationRoutes, ExecutionRoute,
    InteractionRouter, InteractionRoutingError, ModelStreamCancellationRoute, ProductSession,
    ProductSessionCreate, ProductSessionError, ProductSessionState, RouteWriteStatus,
    RuntimeRouteAuthority, RuntimeSourceIdentity, SessionBinding, SessionBindingError,
    SessionBindingIdentity, SessionCancellationRequest, SessionCancellationSnapshot,
    WorkerCancellationRoute,
};
use winwincode_storage::{
    CommitReceipt, ExecutionJobRecord, ExecutionJobState, ExecutionLeaseRecord,
    ExecutionQueueScope, ExecutionReservationRecord, ExecutionReservationState,
    ExecutionScopeReplacementAuthority, NewOutboxEvent, ProductStateStorage, ProjectionEventStream,
    PublicEventActor, PublicEventScope, PublicEventSource, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, StateCommit, StateRevisionGuard, StorageError, StorageErrorKind, WorkerPoolId,
    WorkerSlotAuthority, WorkerSlotRecord, WorkerSlotState,
};

#[path = "product_session_api.rs"]
mod api;
#[path = "product_session_chat.rs"]
mod chat;
#[path = "product_session_execution_job.rs"]
mod execution_job;
pub use api::{ProductSessionApiClock, ProductSessionApiService};
pub(crate) use api::{deterministic_event_id, query_scope};
pub use chat::{
    AppendAssistantMessageCommand, AssistantMessageMutationReceipt, AssistantMessageState,
    CancelProductSessionCommand, ChatSubmitMutationReceipt, ProductSessionCancellationReceipt,
    ProductSessionMessagePage, ProductSessionPageRead, ProductSessionPageRequest,
    ProductSessionTurnIntent, ProductSessionTurnState, ProductSessionTurnTerminalOutcome,
    RecordAssistantTerminalCommand, SubmitChatMessageCommand, product_session_state_filters,
};
use chat::{
    PersistedCancellation, PersistedChatMessage, PersistedExecutionCancellationRoutes,
    PersistedTurnIntent, PersistedTurnState,
};
use execution_job::PreparedProductSessionExecution;
pub use execution_job::ProductSessionExecutionConfig;

pub const PRODUCT_SESSION_SERVICE_SCHEMA_VERSION: u8 = 4;
const PRODUCT_SESSION_CHANGED_TOPIC: &str = "product-session.changed.v1";
const PRODUCT_SESSION_MESSAGE_APPENDED_TOPIC: &str = "product-session.message.appended.v1";
const PRODUCT_SESSION_RECEIPT_TOPIC: &str = "product-session.receipt.internal.v4";

/// Storage seam joining canonical state to an already durable Worker slot.
///
/// Implementations must return only an exact-scope admission record. A missing
/// record is an identity mismatch, never permission to infer a scope from the
/// `ExecutionJobId`.
pub trait ProductSessionPersistence: ProductStateStorage {
    /// Loads the immutable queue entry and proves its original submission
    /// receipt still names the same Chat request.
    ///
    /// # Errors
    ///
    /// Returns a storage error for a partial/corrupt state-to-job join.
    fn load_product_session_execution_job(
        &mut self,
        scope: &ExecutionQueueScope,
        job_id: &ExecutionJobId,
        request_id: &RequestId,
    ) -> Result<Option<ExecutionJobRecord>, StorageError>;

    /// Loads one exact Worker slot and its exact-scope running reservation.
    ///
    /// # Errors
    ///
    /// Returns a storage error when either authoritative source cannot be read.
    fn load_worker_binding_source(
        &mut self,
        scope: &ExecutionQueueScope,
        worker_pool_id: &WorkerPoolId,
        worker_session_id: &WorkerSessionId,
    ) -> Result<Option<(WorkerSlotRecord, ExecutionReservationRecord)>, StorageError>;

    /// Loads the same exact Worker binding plus its current durable Job lease.
    /// Interactive input and approval commands use this stronger read before
    /// accepting a Worker request or a browser response.
    ///
    /// # Errors
    ///
    /// Returns a storage error when any authoritative source cannot be read.
    fn load_worker_interaction_source(
        &mut self,
        scope: &ExecutionQueueScope,
        worker_pool_id: &WorkerPoolId,
        worker_session_id: &WorkerSessionId,
    ) -> Result<
        Option<(
            WorkerSlotRecord,
            ExecutionReservationRecord,
            ExecutionLeaseRecord,
        )>,
        StorageError,
    >;
}

impl ProductSessionPersistence for SqliteStorage {
    fn load_product_session_execution_job(
        &mut self,
        scope: &ExecutionQueueScope,
        job_id: &ExecutionJobId,
        request_id: &RequestId,
    ) -> Result<Option<ExecutionJobRecord>, StorageError> {
        let queue = self.execution_queue()?;
        let job = queue.load_job(scope, job_id)?;
        let has_receipt = queue.has_request(scope, request_id)?;
        match job {
            Some(job)
                if has_receipt
                    && job.job_id == *job_id
                    && job.submission_request_id == *request_id =>
            {
                Ok(Some(job))
            }
            None if !has_receipt => Ok(None),
            Some(_) | None => Err(StorageError::adapter(
                "ProductSession execution job and submission receipt differ",
            )),
        }
    }

    fn load_worker_binding_source(
        &mut self,
        scope: &ExecutionQueueScope,
        worker_pool_id: &WorkerPoolId,
        worker_session_id: &WorkerSessionId,
    ) -> Result<Option<(WorkerSlotRecord, ExecutionReservationRecord)>, StorageError> {
        let slot = self
            .worker_session_slots()
            .map_err(|error| {
                StorageError::adapter(format!("Worker slot cannot be opened: {error}"))
            })?
            .load(worker_session_id)
            .map_err(|error| {
                StorageError::adapter(format!("Worker slot cannot be read: {error}"))
            })?;
        let Some(slot) = slot else {
            return Ok(None);
        };
        let reservation = self
            .execution_admission()
            .map_err(|error| {
                StorageError::adapter(format!("execution admission cannot be opened: {error}"))
            })?
            .load_reservation(scope, worker_pool_id, &slot.authority.job_id)
            .map_err(|error| {
                StorageError::adapter(format!("execution reservation cannot be read: {error}"))
            })?;
        Ok(reservation.map(|reservation| (slot, reservation)))
    }

    fn load_worker_interaction_source(
        &mut self,
        scope: &ExecutionQueueScope,
        worker_pool_id: &WorkerPoolId,
        worker_session_id: &WorkerSessionId,
    ) -> Result<
        Option<(
            WorkerSlotRecord,
            ExecutionReservationRecord,
            ExecutionLeaseRecord,
        )>,
        StorageError,
    > {
        let Some((slot, reservation)) =
            self.load_worker_binding_source(scope, worker_pool_id, worker_session_id)?
        else {
            return Ok(None);
        };
        let lease = self
            .execution_registry()?
            .load_lease(&slot.authority.job_id)?;
        Ok(lease.map(|lease| (slot, reservation, lease)))
    }
}

/// Common authority and concurrency facts for one mutating command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSessionCommandContext {
    pub receipt_identity: ReceiptIdentity,
    pub expected_revision: u64,
    pub event_id: ControlPlaneEventId,
    pub occurred_at: Instant,
    pub public_actor: PublicEventActor,
    pub public_scope: PublicEventScope,
}

/// Builds the canonical service context from one generated repository command
/// envelope and server-owned event facts.
///
/// # Errors
///
/// Rejects negative revisions and non-canonical actor, scope, or request IDs.
pub fn product_session_command_context(
    actor: &Actor,
    scope: &RepositoryScope,
    request_id: RequestId,
    expected_revision: &Revision,
    event_id: ControlPlaneEventId,
    occurred_at: Instant,
) -> Result<ProductSessionCommandContext, ProductSessionServiceError> {
    let public_actor = match actor {
        Actor::UserActor(actor) => PublicEventActor::User {
            id: actor.id.clone(),
        },
        Actor::ServiceAccountActor(actor) => PublicEventActor::ServiceAccount {
            id: actor.id.clone(),
        },
        Actor::SystemActor(actor) => PublicEventActor::System {
            id: actor.id.clone(),
        },
    };
    let public_scope = PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    };
    let expected_revision = u64::try_from(expected_revision.0).map_err(|_| {
        service_error(
            ProductSessionServiceErrorCode::InvalidInput,
            "ProductSession expectedRevision must not be negative",
        )
    })?;
    let receipt_identity =
        winwincode_storage::public_receipt_identity(&public_actor, &public_scope, request_id)
            .map_err(|error| storage_error(&error))?;
    Ok(ProductSessionCommandContext {
        receipt_identity,
        expected_revision,
        event_id,
        occurred_at,
        public_actor,
        public_scope,
    })
}

/// Creates one independent `ProductSession` in the command scope.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateProductSessionCommand {
    pub context: ProductSessionCommandContext,
    pub product_session_id: ProductSessionId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub title: String,
    pub model_selection: SessionModelSelection,
}

impl CreateProductSessionCommand {
    /// Converts the generated `session.create` command without duplicating
    /// public actor or repository-scope mapping in the transport layer.
    ///
    /// # Errors
    ///
    /// Rejects invalid command-envelope authority or revision facts.
    pub fn from_api(
        command: SessionCreateCommand,
        event_id: ControlPlaneEventId,
        occurred_at: Instant,
    ) -> Result<Self, ProductSessionServiceError> {
        let context = product_session_command_context(
            &command.actor,
            &command.scope,
            command.request_id,
            &command.expected_revision,
            event_id,
            occurred_at,
        )?;
        Ok(Self {
            context,
            product_session_id: command.payload.product_session_id,
            project_id: command.payload.project_id,
            repository_id: command.payload.repository_id,
            title: command.payload.title,
            model_selection: command.payload.model_selection,
        })
    }
}

/// Continues one `ProductSession` with a new, fully explicit runtime binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContinueProductSessionCommand {
    pub context: ProductSessionCommandContext,
    pub product_session_id: ProductSessionId,
    pub binding_identity: SessionBindingIdentity,
    pub runtime_authority: WorkerSlotAuthority,
    pub execution_scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub model_exchange_id: ModelExchangeId,
}

/// Replaces the one running execution binding using only a scheduler-sealed
/// predecessor/successor lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceProductSessionExecutionBindingCommand {
    pub context: ProductSessionCommandContext,
    pub product_session_id: ProductSessionId,
    pub binding_identity: SessionBindingIdentity,
    pub runtime_authority: WorkerSlotAuthority,
    pub execution_scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub model_exchange_id: ModelExchangeId,
    pub replacement: ExecutionScopeReplacementAuthority,
}

/// Forks session-visible state without copying runtime identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkProductSessionCommand {
    pub context: ProductSessionCommandContext,
    pub source_product_session_id: ProductSessionId,
    pub product_session_id: ProductSessionId,
    pub title: String,
}

/// Closes one exact `ProductSession` revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloseProductSessionCommand {
    pub context: ProductSessionCommandContext,
    pub product_session_id: ProductSessionId,
}

impl CloseProductSessionCommand {
    /// Converts one generated `session.close` command into the canonical service command.
    ///
    /// # Errors
    ///
    /// Rejects invalid command-envelope authority or revision facts.
    pub fn from_api(
        command: SessionCloseCommand,
        event_id: ControlPlaneEventId,
        occurred_at: Instant,
    ) -> Result<Self, ProductSessionServiceError> {
        let context = product_session_command_context(
            &command.actor,
            &command.scope,
            command.request_id,
            &command.expected_revision,
            event_id,
            occurred_at,
        )?;
        Ok(Self {
            context,
            product_session_id: command.payload.product_session_id,
        })
    }
}

/// One durable binding and the exact Worker-slot facts accepted with it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableSessionBinding {
    binding: SessionBinding,
    slot: WorkerSlotRecord,
    execution_scope: ExecutionQueueScope,
    worker_pool_id: WorkerPoolId,
    model_exchange_id: ModelExchangeId,
    bound_at: Instant,
}

impl DurableSessionBinding {
    #[must_use]
    pub const fn binding(&self) -> &SessionBinding {
        &self.binding
    }

    #[must_use]
    pub const fn slot(&self) -> &WorkerSlotRecord {
        &self.slot
    }

    #[must_use]
    pub const fn execution_scope(&self) -> &ExecutionQueueScope {
        &self.execution_scope
    }

    #[must_use]
    pub const fn worker_pool_id(&self) -> &WorkerPoolId {
        &self.worker_pool_id
    }

    #[must_use]
    pub const fn model_exchange_id(&self) -> &ModelExchangeId {
        &self.model_exchange_id
    }

    #[must_use]
    pub const fn bound_at(&self) -> &Instant {
        &self.bound_at
    }
}

/// Complete query result for one `ProductSession`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSessionRecord {
    session: ProductSession,
    forked_from: Option<ProductSessionId>,
    model_selection: SessionModelSelection,
    bindings: Vec<DurableSessionBinding>,
    messages: Vec<ChatMessageProjection>,
    turn_intents: Vec<ProductSessionTurnIntent>,
    cancellation_routes: Vec<ExecutionCancellationRoutes>,
    cancellation_request_id: Option<RequestId>,
}

impl ProductSessionRecord {
    #[must_use]
    pub const fn session(&self) -> &ProductSession {
        &self.session
    }

    #[must_use]
    pub const fn forked_from(&self) -> Option<&ProductSessionId> {
        self.forked_from.as_ref()
    }

    /// Returns the exact provider-neutral route selected when the session was
    /// created. It contains only a Credential reference, never secret bytes.
    #[must_use]
    pub const fn model_selection(&self) -> &SessionModelSelection {
        &self.model_selection
    }

    #[must_use]
    pub fn bindings(&self) -> &[DurableSessionBinding] {
        &self.bindings
    }

    /// Returns the bounded public message ledger in stable sequence order.
    #[must_use]
    pub fn messages(&self) -> &[ChatMessageProjection] {
        &self.messages
    }

    /// Returns every durable turn intent in submission order.
    #[must_use]
    pub fn turn_intents(&self) -> &[ProductSessionTurnIntent] {
        &self.turn_intents
    }

    /// Returns the exact routes emitted by the accepted cancellation, if any.
    #[must_use]
    pub fn cancellation_routes(&self) -> &[ExecutionCancellationRoutes] {
        &self.cancellation_routes
    }

    #[must_use]
    pub const fn cancellation_request_id(&self) -> Option<&RequestId> {
        self.cancellation_request_id.as_ref()
    }

    /// Converts the durable aggregate into the generated public projection.
    ///
    /// # Errors
    ///
    /// Returns corruption when the durable revision exceeds the public integer range.
    pub fn projection(&self) -> Result<ProductSessionProjection, ProductSessionServiceError> {
        Ok(ProductSessionProjection {
            id: self.session.id().clone(),
            project_id: self.session.project_id().clone(),
            repository_id: self.session.repository_id().clone(),
            revision: Revision(
                i64::try_from(self.session.revision())
                    .map_err(|_| corrupt("ProductSession revision is outside the public range"))?,
            ),
            state: session_state_label(self.session.state()).to_owned(),
            title: self.session.title().to_owned(),
            updated_at: self.session.updated_at().clone(),
            model_selection: self.model_selection.clone(),
        })
    }
}

/// Replay-safe mutation result.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSessionMutationReceipt {
    pub record: ProductSessionRecord,
    pub catalog_revision: u64,
    pub replayed: bool,
}

/// Exact durable `ProductSession` facts used by guarded collaboration writes.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSessionAuthoritySeal {
    pub record: ProductSessionRecord,
    pub target_revision: u64,
    pub target_sha256: Sha256Digest,
    pub state_guard: StateRevisionGuard,
}

/// Stable command/read failure code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductSessionServiceErrorCode {
    InvalidInput,
    NotFound,
    AlreadyExists,
    RevisionConflict,
    RequestConflict,
    InvalidState,
    BindingIdentityMismatch,
    BindingConflict,
    WorkerSlotNotRunning,
    MessageLimitExceeded,
    StreamSequenceConflict,
    CursorInvalid,
    CredentialLeak,
    ActorMismatch,
    CorruptState,
    Storage,
}

/// `ProductSession` application-service failure with a bounded message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSessionServiceError {
    code: ProductSessionServiceErrorCode,
    message: String,
}

impl ProductSessionServiceError {
    fn new(code: ProductSessionServiceErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn code(&self) -> ProductSessionServiceErrorCode {
        self.code
    }
}

impl fmt::Display for ProductSessionServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProductSessionServiceError {}

/// Application service for scope-isolated `ProductSession` commands and reads.
pub struct ProductSessionService<'storage> {
    storage: &'storage mut dyn ProductSessionPersistence,
    output_gate: crate::CredentialLeakGate,
}

impl<'storage> ProductSessionService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ProductSessionPersistence) -> Self {
        Self {
            storage,
            output_gate: crate::CredentialLeakGate::new(),
        }
    }

    /// Creates a service whose public message boundary also knows the
    /// fingerprints of credentials resolved by the Provider Gateway.
    #[must_use]
    pub fn with_output_gate(
        storage: &'storage mut dyn ProductSessionPersistence,
        output_gate: &crate::CredentialLeakGate,
    ) -> Self {
        Self {
            storage,
            output_gate: output_gate.fingerprint_snapshot(),
        }
    }

    /// Creates one session. `expected_revision` must be zero.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, duplicate identities, conflicts, and storage failures.
    pub fn create(
        &mut self,
        command: &CreateProductSessionCommand,
    ) -> Result<ProductSessionMutationReceipt, ProductSessionServiceError> {
        let digest = command_digest("create", command_digest_fields(command))?;
        if let Some(replay) = self.replay(&command.context, &digest, MutationKind::Created)? {
            return Ok(replay);
        }
        if command.context.expected_revision != 0 {
            return Err(service_error(
                ProductSessionServiceErrorCode::RevisionConflict,
                "ProductSession create requires expected revision zero",
            ));
        }
        validate_create_scope(command)?;
        validate_model_selection(&command.model_selection)?;
        inspect_public_output(&self.output_gate, &command.model_selection)?;
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        if catalog.sessions.contains_key(&command.product_session_id.0) {
            return Err(service_error(
                ProductSessionServiceErrorCode::AlreadyExists,
                "ProductSession identity already exists in this scope",
            ));
        }
        let session = ProductSession::create(ProductSessionCreate {
            product_session_id: command.product_session_id.clone(),
            project_id: command.project_id.clone(),
            repository_id: command.repository_id.clone(),
            title: command.title.clone(),
            now: command.context.occurred_at.clone(),
        })
        .map_err(|error| domain_error(&error))?;
        let persisted = PersistedProductSession {
            session,
            forked_from: None,
            model_selection: command.model_selection.clone(),
            bindings: Vec::new(),
            messages: Vec::new(),
            turn_intents: Vec::new(),
            cancellation: None,
        };
        catalog
            .sessions
            .insert(command.product_session_id.0.clone(), persisted.clone());
        self.commit(
            &command.context,
            digest,
            MutationKind::Created,
            catalog,
            &command.product_session_id,
            persisted,
            None,
        )
    }

    /// Continues one session using only an exact, currently running Worker
    /// slot and its exact-scope admission record.
    ///
    /// # Errors
    ///
    /// Rejects stale session revisions, inferred/foreign identities, reused
    /// bindings, non-running slots, invalid transitions, and storage failures.
    pub fn continue_session(
        &mut self,
        command: &ContinueProductSessionCommand,
    ) -> Result<ProductSessionMutationReceipt, ProductSessionServiceError> {
        let digest = command_digest("continue", continue_digest_fields(command))?;
        if let Some(replay) = self.replay(&command.context, &digest, MutationKind::Continued)? {
            return Ok(replay);
        }
        validate_continue_shape(command)?;
        let source = self
            .storage
            .load_worker_binding_source(
                &command.execution_scope,
                &command.worker_pool_id,
                &command.runtime_authority.worker_session_id,
            )
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| {
                service_error(
                    ProductSessionServiceErrorCode::BindingIdentityMismatch,
                    "no exact Worker slot and execution reservation match the supplied identities",
                )
            })?;
        let (slot, reservation) = source;
        validate_worker_source(command, &slot, &reservation)?;
        let binding = build_binding(&command.binding_identity, &slot)?;

        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        ensure_binding_is_unique(&catalog, &binding)?;
        let persisted = catalog
            .sessions
            .get_mut(&command.product_session_id.0)
            .ok_or_else(not_found)?;
        if persisted.session.project_id() != &command.execution_scope.project_id
            || persisted.session.repository_id() != &command.execution_scope.repository_id
        {
            return Err(binding_mismatch(
                "execution scope does not match the ProductSession project and repository",
            ));
        }
        require_revision(&persisted.session, command.context.expected_revision)?;
        let pending_turn = persisted
            .turn_intents
            .iter()
            .rposition(|turn| turn.state == PersistedTurnState::Pending);
        if pending_turn.is_some_and(|index| {
            persisted.turn_intents[index].execution_job_id != command.runtime_authority.job_id
        }) {
            return Err(binding_mismatch(
                "Worker binding does not name the Chat turn ExecutionJob",
            ));
        }
        continue_lifecycle(
            &mut persisted.session,
            command.context.occurred_at.clone(),
            pending_turn.is_some(),
        )?;
        persisted.bindings.push(PersistedSessionBinding {
            identity: PersistedBindingIdentity::from_domain(&command.binding_identity),
            slot,
            reservation,
            model_exchange_id: command.model_exchange_id.clone(),
            bound_at: command.context.occurred_at.clone(),
        });
        if let Some(index) = pending_turn {
            let turn = &mut persisted.turn_intents[index];
            turn.state = PersistedTurnState::Bound;
            turn.model_exchange_id = Some(command.model_exchange_id.clone());
        }
        let persisted = persisted.clone();
        self.commit(
            &command.context,
            digest,
            MutationKind::Continued,
            catalog,
            &command.product_session_id,
            persisted,
            None,
        )
    }

    pub(crate) fn replay_continue_session(
        &self,
        command: &ContinueProductSessionCommand,
    ) -> Result<Option<ProductSessionMutationReceipt>, ProductSessionServiceError> {
        let digest = command_digest("continue", continue_digest_fields(command))?;
        self.replay(&command.context, &digest, MutationKind::Continued)
    }

    /// Rotates one existing running binding to the scheduler-sealed successor
    /// without creating a second Chat turn or binding.
    ///
    /// # Errors
    ///
    /// Rejects a foreign predecessor, successor, scope, slot, reservation,
    /// receipt, or `ProductSession` revision before committing any state.
    pub(crate) fn replace_execution_binding(
        &mut self,
        command: &ReplaceProductSessionExecutionBindingCommand,
    ) -> Result<ProductSessionMutationReceipt, ProductSessionServiceError> {
        let digest = command_digest(
            "replace_execution_binding",
            replace_execution_binding_digest_fields(command),
        )?;
        if let Some(replay) = self.replay(
            &command.context,
            &digest,
            MutationKind::ExecutionBindingReplaced,
        )? {
            return Ok(replay);
        }
        validate_replacement_shape(command)?;
        let source = self
            .storage
            .load_worker_binding_source(
                &command.execution_scope,
                &command.worker_pool_id,
                &command.runtime_authority.worker_session_id,
            )
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| {
                binding_mismatch(
                    "no exact successor Worker slot and execution reservation match the replacement",
                )
            })?;
        let (slot, reservation) = source;
        validate_replacement_worker_source(command, &slot, &reservation)?;
        let binding = build_binding(&command.binding_identity, &slot)?;

        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        ensure_replacement_binding_is_unique(&catalog, &binding)?;
        let persisted = catalog
            .sessions
            .get_mut(&command.product_session_id.0)
            .ok_or_else(not_found)?;
        require_revision(&persisted.session, command.context.expected_revision)?;
        let binding_index = replacement_binding_index(persisted, command)?;
        validate_replacement_predecessor(&persisted.bindings[binding_index], command)?;
        persisted.bindings[binding_index] = PersistedSessionBinding {
            identity: PersistedBindingIdentity::from_domain(&command.binding_identity),
            slot,
            reservation,
            model_exchange_id: command.model_exchange_id.clone(),
            bound_at: command.context.occurred_at.clone(),
        };
        replace_bound_turn_exchange(persisted, command)?;
        let persisted = persisted.clone();
        self.commit(
            &command.context,
            digest,
            MutationKind::ExecutionBindingReplaced,
            catalog,
            &command.product_session_id,
            persisted,
            None,
        )
    }

    pub(crate) fn replay_replace_execution_binding(
        &self,
        command: &ReplaceProductSessionExecutionBindingCommand,
    ) -> Result<Option<ProductSessionMutationReceipt>, ProductSessionServiceError> {
        let digest = command_digest(
            "replace_execution_binding",
            replace_execution_binding_digest_fields(command),
        )?;
        self.replay(
            &command.context,
            &digest,
            MutationKind::ExecutionBindingReplaced,
        )
    }

    /// Forks one session without copying any `ExecutionJob`, `WorkerSession`, or
    /// `CodexThread` identity.
    ///
    /// # Errors
    ///
    /// Rejects a missing/stale source, duplicate target, invalid target, and storage failures.
    pub fn fork(
        &mut self,
        command: &ForkProductSessionCommand,
    ) -> Result<ProductSessionMutationReceipt, ProductSessionServiceError> {
        let digest = command_digest("fork", fork_digest_fields(command))?;
        if let Some(replay) = self.replay(&command.context, &digest, MutationKind::Forked)? {
            return Ok(replay);
        }
        if command.source_product_session_id == command.product_session_id {
            return Err(service_error(
                ProductSessionServiceErrorCode::InvalidInput,
                "fork source and target ProductSession must differ",
            ));
        }
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        if catalog.sessions.contains_key(&command.product_session_id.0) {
            return Err(service_error(
                ProductSessionServiceErrorCode::AlreadyExists,
                "fork target ProductSession already exists",
            ));
        }
        let source = catalog
            .sessions
            .get(&command.source_product_session_id.0)
            .ok_or_else(not_found)?;
        require_revision(&source.session, command.context.expected_revision)?;
        if source.session.state() == ProductSessionState::Closed {
            return Err(service_error(
                ProductSessionServiceErrorCode::InvalidState,
                "closed ProductSession cannot be forked",
            ));
        }
        let session = ProductSession::create(ProductSessionCreate {
            product_session_id: command.product_session_id.clone(),
            project_id: source.session.project_id().clone(),
            repository_id: source.session.repository_id().clone(),
            title: command.title.clone(),
            now: command.context.occurred_at.clone(),
        })
        .map_err(|error| domain_error(&error))?;
        let model_selection = source.model_selection.clone();
        let persisted = PersistedProductSession {
            session,
            forked_from: Some(command.source_product_session_id.clone()),
            model_selection,
            bindings: Vec::new(),
            messages: Vec::new(),
            turn_intents: Vec::new(),
            cancellation: None,
        };
        catalog
            .sessions
            .insert(command.product_session_id.0.clone(), persisted.clone());
        self.commit(
            &command.context,
            digest,
            MutationKind::Forked,
            catalog,
            &command.product_session_id,
            persisted,
            None,
        )
    }

    /// Closes one exact session revision.
    ///
    /// # Errors
    ///
    /// Rejects missing/stale/non-closable sessions and storage failures.
    pub fn close(
        &mut self,
        command: &CloseProductSessionCommand,
    ) -> Result<ProductSessionMutationReceipt, ProductSessionServiceError> {
        let digest = command_digest("close", close_digest_fields(command))?;
        if let Some(replay) = self.replay(&command.context, &digest, MutationKind::Closed)? {
            return Ok(replay);
        }
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        let persisted = catalog
            .sessions
            .get_mut(&command.product_session_id.0)
            .ok_or_else(not_found)?;
        require_revision(&persisted.session, command.context.expected_revision)?;
        if persisted.session.state() == ProductSessionState::Closed {
            return Err(service_error(
                ProductSessionServiceErrorCode::InvalidState,
                "ProductSession is already closed under another command",
            ));
        }
        persisted
            .session
            .close(command.context.occurred_at.clone())
            .map_err(|error| domain_error(&error))?;
        let persisted = persisted.clone();
        self.commit(
            &command.context,
            digest,
            MutationKind::Closed,
            catalog,
            &command.product_session_id,
            persisted,
            None,
        )
    }

    /// Reads one exact `ProductSession` from one exact command scope.
    ///
    /// # Errors
    ///
    /// Returns a corruption/storage error when durable state cannot be decoded.
    pub fn get(
        &self,
        scope: &ReceiptScopeKey,
        product_session_id: &ProductSessionId,
    ) -> Result<Option<ProductSessionRecord>, ProductSessionServiceError> {
        self.load_catalog(scope)?
            .sessions
            .get(&product_session_id.0)
            .map(PersistedProductSession::to_record)
            .transpose()
    }

    /// Loads one exact session together with the catalog revision guard that
    /// protects its durable record.
    ///
    /// # Errors
    ///
    /// Returns not-found, corruption, or storage errors without synthesizing
    /// a revision from the public projection.
    pub fn authority_seal(
        &self,
        scope: &ReceiptScopeKey,
        product_session_id: &ProductSessionId,
    ) -> Result<ProductSessionAuthoritySeal, ProductSessionServiceError> {
        let stream_id = catalog_stream_id(scope);
        let stored = self
            .storage
            .load_state(&stream_id)
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| {
                service_error(
                    ProductSessionServiceErrorCode::NotFound,
                    "ProductSession was not found in this repository scope",
                )
            })?;
        let catalog: PersistedProductSessionCatalog = serde_json::from_slice(&stored.payload)
            .map_err(|error| {
                corrupt(format!("ProductSession catalog cannot be decoded: {error}"))
            })?;
        catalog.validate(stored.revision)?;
        if stored.stream_id != stream_id {
            return Err(corrupt("ProductSession catalog stream identity is corrupt"));
        }
        let persisted = catalog.sessions.get(&product_session_id.0).ok_or_else(|| {
            service_error(
                ProductSessionServiceErrorCode::NotFound,
                "ProductSession was not found in this repository scope",
            )
        })?;
        let target_bytes = serde_json::to_vec(persisted).map_err(|error| {
            corrupt(format!(
                "ProductSession authority cannot be encoded: {error}"
            ))
        })?;
        Ok(ProductSessionAuthoritySeal {
            record: persisted.to_record()?,
            target_revision: persisted.session.revision(),
            target_sha256: Sha256Digest(format!("sha256:{:x}", Sha256::digest(target_bytes))),
            state_guard: StateRevisionGuard::new(stored.stream_id, stored.revision)
                .map_err(|error| storage_error(&error))?,
        })
    }

    /// Lists all sessions in deterministic ProductSession-id order.
    ///
    /// # Errors
    ///
    /// Returns a corruption/storage error when durable state cannot be decoded.
    pub fn list(
        &self,
        scope: &ReceiptScopeKey,
    ) -> Result<Vec<ProductSessionRecord>, ProductSessionServiceError> {
        self.load_catalog(scope)?
            .sessions
            .values()
            .map(PersistedProductSession::to_record)
            .collect()
    }

    fn replay(
        &self,
        context: &ProductSessionCommandContext,
        digest: &Sha256Digest,
        kind: MutationKind,
    ) -> Result<Option<ProductSessionMutationReceipt>, ProductSessionServiceError> {
        self.storage
            .load_receipt(&context.receipt_identity, digest)
            .map_err(|error| storage_error(&error))?
            .map(|receipt| decode_receipt(&receipt, kind, true))
            .transpose()
    }

    fn load_catalog(
        &self,
        scope: &ReceiptScopeKey,
    ) -> Result<PersistedProductSessionCatalog, ProductSessionServiceError> {
        let stream_id = catalog_stream_id(scope);
        let Some(state) = self
            .storage
            .load_state(&stream_id)
            .map_err(|error| storage_error(&error))?
        else {
            return Ok(PersistedProductSessionCatalog::default());
        };
        let catalog: PersistedProductSessionCatalog = serde_json::from_slice(&state.payload)
            .map_err(|error| {
                corrupt(format!("ProductSession catalog cannot be decoded: {error}"))
            })?;
        catalog.validate(state.revision)?;
        Ok(catalog)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        context: &ProductSessionCommandContext,
        digest: Sha256Digest,
        kind: MutationKind,
        mut catalog: PersistedProductSessionCatalog,
        product_session_id: &ProductSessionId,
        persisted: PersistedProductSession,
        execution_job: Option<&PreparedProductSessionExecution>,
    ) -> Result<ProductSessionMutationReceipt, ProductSessionServiceError> {
        let expected_catalog_revision = catalog.revision;
        catalog.revision = catalog
            .revision
            .checked_add(1)
            .ok_or_else(|| corrupt("ProductSession catalog revision overflowed"))?;
        let record = persisted.to_record()?;
        let event = PersistedMutationEvent {
            schema_version: PRODUCT_SESSION_SERVICE_SCHEMA_VERSION,
            kind,
            catalog_revision: catalog.revision,
            record: persisted,
        };
        let event_bytes = serde_json::to_vec(&event)
            .map_err(|error| corrupt(format!("ProductSession event cannot be encoded: {error}")))?;
        let state = serde_json::to_vec(&catalog).map_err(|error| {
            corrupt(format!("ProductSession catalog cannot be encoded: {error}"))
        })?;
        let mut events = mutation_outbox_events(
            &self.output_gate,
            context,
            kind,
            &record,
            product_session_id,
            event_bytes,
        )?;
        if let Some(execution) = execution_job {
            events.push(NewOutboxEvent::internal(
                format!("execution-job:{}", execution.job.job_id.0),
                crate::delivery_transaction::EXECUTION_JOB_TOPIC,
                serde_json::to_vec(&execution.job)
                    .map_err(|_| corrupt("ProductSession ExecutionJob event cannot be encoded"))?,
            ));
        }
        let commit = StateCommit::new(
            context.receipt_identity.clone(),
            digest,
            catalog_stream_id(context.receipt_identity.scope_key()),
            expected_catalog_revision,
            state,
            events,
        );
        let receipt = match execution_job {
            Some(execution_job) => self
                .storage
                .commit_with_execution_job(&commit, &execution_job.submission)
                .map(|receipt| receipt.state),
            None => self.storage.commit(&commit),
        }
        .map_err(|error| storage_error(&error))?;
        let decoded = decode_receipt(&receipt, kind, receipt.idempotent_replay)?;
        if !receipt.idempotent_replay && decoded.record != record {
            return Err(corrupt(
                "committed ProductSession event does not match the accepted state",
            ));
        }
        Ok(decoded)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MutationKind {
    Created,
    Continued,
    ExecutionBindingReplaced,
    Forked,
    ChatSubmitted,
    AssistantUpdated,
    Cancelled,
    Closed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedMutationEvent {
    schema_version: u8,
    kind: MutationKind,
    catalog_revision: u64,
    record: PersistedProductSession,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedProductSessionCatalog {
    schema_version: u8,
    revision: u64,
    sessions: BTreeMap<String, PersistedProductSession>,
}

impl Default for PersistedProductSessionCatalog {
    fn default() -> Self {
        Self {
            schema_version: PRODUCT_SESSION_SERVICE_SCHEMA_VERSION,
            revision: 0,
            sessions: BTreeMap::new(),
        }
    }
}

impl PersistedProductSessionCatalog {
    fn validate(&self, stored_revision: u64) -> Result<(), ProductSessionServiceError> {
        if self.schema_version != PRODUCT_SESSION_SERVICE_SCHEMA_VERSION
            || self.revision == 0
            || self.revision != stored_revision
        {
            return Err(corrupt(
                "ProductSession catalog contract or revision is inconsistent",
            ));
        }
        for (key, session) in &self.sessions {
            if key != &session.session.id().0 {
                return Err(corrupt(
                    "ProductSession catalog key does not match its canonical identity",
                ));
            }
            session.to_record()?;
        }
        ensure_persisted_bindings_unique(self)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedProductSession {
    session: ProductSession,
    forked_from: Option<ProductSessionId>,
    model_selection: SessionModelSelection,
    bindings: Vec<PersistedSessionBinding>,
    messages: Vec<PersistedChatMessage>,
    turn_intents: Vec<PersistedTurnIntent>,
    cancellation: Option<PersistedCancellation>,
}

impl PersistedProductSession {
    fn to_record(&self) -> Result<ProductSessionRecord, ProductSessionServiceError> {
        if self
            .forked_from
            .as_ref()
            .is_some_and(|source| source == self.session.id())
        {
            return Err(corrupt("ProductSession cannot be forked from itself"));
        }
        validate_model_selection(&self.model_selection)?;
        chat::validate_persisted_chat(self)?;
        Ok(ProductSessionRecord {
            session: self.session.clone(),
            forked_from: self.forked_from.clone(),
            model_selection: self.model_selection.clone(),
            bindings: self
                .bindings
                .iter()
                .map(PersistedSessionBinding::to_domain)
                .collect::<Result<_, _>>()?,
            messages: self
                .messages
                .iter()
                .map(|message| message.projection.clone())
                .collect(),
            turn_intents: self
                .turn_intents
                .iter()
                .map(PersistedTurnIntent::to_domain)
                .collect(),
            cancellation_routes: self.cancellation.as_ref().map_or_else(
                || Ok(Vec::new()),
                |cancellation| {
                    cancellation
                        .routes
                        .iter()
                        .map(PersistedExecutionCancellationRoutes::to_domain)
                        .collect()
                },
            )?,
            cancellation_request_id: self
                .cancellation
                .as_ref()
                .map(|cancellation| cancellation.request_id.clone()),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedSessionBinding {
    identity: PersistedBindingIdentity,
    slot: WorkerSlotRecord,
    reservation: ExecutionReservationRecord,
    model_exchange_id: ModelExchangeId,
    bound_at: Instant,
}

impl PersistedSessionBinding {
    fn to_domain(&self) -> Result<DurableSessionBinding, ProductSessionServiceError> {
        if self.slot.state != WorkerSlotState::Running {
            return Err(corrupt(
                "persisted ProductSession binding did not originate from a running Worker slot",
            ));
        }
        let identity = self.identity.to_domain()?;
        if identity.execution_job_id() != &self.slot.authority.job_id {
            return Err(corrupt(
                "persisted ProductSession binding and Worker slot name different jobs",
            ));
        }
        if self.reservation.job_id != self.slot.authority.job_id
            || self.reservation.scope.product_session_id != *identity.product_session_id()
            || self.reservation.state != ExecutionReservationState::Running
        {
            return Err(corrupt(
                "persisted ProductSession binding reservation is inconsistent",
            ));
        }
        let binding = build_binding(&identity, &self.slot)?;
        Ok(DurableSessionBinding {
            binding,
            slot: self.slot.clone(),
            execution_scope: self.reservation.scope.clone(),
            worker_pool_id: self.reservation.worker_pool_id.clone(),
            model_exchange_id: self.model_exchange_id.clone(),
            bound_at: self.bound_at.clone(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "scope", deny_unknown_fields)]
enum PersistedBindingIdentity {
    ProductSession {
        product_session_id: ProductSessionId,
        execution_job_id: ExecutionJobId,
    },
    DeliveryStage {
        delivery_id: DeliveryId,
        delivery_task_id: Option<DeliveryTaskId>,
        stage_run_id: StageRunId,
        product_session_id: ProductSessionId,
        execution_job_id: ExecutionJobId,
    },
}

impl PersistedBindingIdentity {
    fn from_domain(identity: &SessionBindingIdentity) -> Self {
        match identity.scope() {
            BindingScope::ProductSession => Self::ProductSession {
                product_session_id: identity.product_session_id().clone(),
                execution_job_id: identity.execution_job_id().clone(),
            },
            BindingScope::DeliveryStage {
                delivery_id,
                delivery_task_id,
                stage_run_id,
            } => Self::DeliveryStage {
                delivery_id: delivery_id.clone(),
                delivery_task_id: delivery_task_id.clone(),
                stage_run_id: stage_run_id.clone(),
                product_session_id: identity.product_session_id().clone(),
                execution_job_id: identity.execution_job_id().clone(),
            },
        }
    }

    fn to_domain(&self) -> Result<SessionBindingIdentity, ProductSessionServiceError> {
        match self {
            Self::ProductSession {
                product_session_id,
                execution_job_id,
            } => SessionBindingIdentity::product_session(
                product_session_id.clone(),
                execution_job_id.clone(),
            ),
            Self::DeliveryStage {
                delivery_id,
                delivery_task_id,
                stage_run_id,
                product_session_id,
                execution_job_id,
            } => SessionBindingIdentity::delivery_stage(
                delivery_id.clone(),
                delivery_task_id.clone(),
                stage_run_id.clone(),
                product_session_id.clone(),
                execution_job_id.clone(),
            ),
        }
        .map_err(|error| binding_error(&error))
    }
}

fn validate_continue_shape(
    command: &ContinueProductSessionCommand,
) -> Result<(), ProductSessionServiceError> {
    if command.binding_identity.product_session_id() != &command.product_session_id
        || command.execution_scope.product_session_id != command.product_session_id
        || command.binding_identity.execution_job_id() != &command.runtime_authority.job_id
    {
        return Err(binding_mismatch(
            "ProductSession, SessionBinding, ExecutionJob, and Worker slot identities differ",
        ));
    }
    match command.binding_identity.scope() {
        BindingScope::ProductSession if command.execution_scope.delivery_id.is_none() => Ok(()),
        BindingScope::DeliveryStage { delivery_id, .. }
            if command.execution_scope.delivery_id.as_ref() == Some(delivery_id) =>
        {
            Ok(())
        }
        _ => Err(binding_mismatch(
            "SessionBinding stage scope does not match the durable execution scope",
        )),
    }
}

fn validate_replacement_shape(
    command: &ReplaceProductSessionExecutionBindingCommand,
) -> Result<(), ProductSessionServiceError> {
    if command.binding_identity.product_session_id() != &command.product_session_id
        || command.execution_scope.product_session_id != command.product_session_id
        || command.binding_identity.execution_job_id() != &command.runtime_authority.job_id
        || command.replacement.job_id() != &command.runtime_authority.job_id
        || command.replacement.scope() != &command.execution_scope
        || command.replacement.stage_run_id().is_some()
        || command.replacement.replacement_attempt() != command.runtime_authority.attempt
        || command.replacement.previous_attempt().checked_add(1)
            != Some(command.runtime_authority.attempt)
    {
        return Err(binding_mismatch(
            "ProductSession replacement scope, attempt, Job, or binding identity differs",
        ));
    }
    if !matches!(
        command.binding_identity.scope(),
        BindingScope::ProductSession
    ) {
        return Err(binding_mismatch(
            "ProductSession replacement contains a Delivery binding",
        ));
    }
    let successor = command.replacement.replacement_lease();
    if successor.job_id != command.runtime_authority.job_id
        || successor.worker_id != command.runtime_authority.worker_id
        || successor.worker_instance_id != command.runtime_authority.worker_instance_id
        || successor.lease_id != command.runtime_authority.lease_id
        || successor.attempt != command.runtime_authority.attempt
        || successor.fencing_token != command.runtime_authority.fencing_token
    {
        return Err(binding_mismatch(
            "ProductSession replacement successor differs from the running Worker slot",
        ));
    }
    if command.replacement.predecessor_slot().is_none()
        || command.replacement.previous_worker_session_id().is_none()
    {
        return Err(binding_mismatch(
            "running ProductSession replacement has no sealed predecessor session",
        ));
    }
    Ok(())
}

fn validate_worker_source(
    command: &ContinueProductSessionCommand,
    slot: &WorkerSlotRecord,
    reservation: &ExecutionReservationRecord,
) -> Result<(), ProductSessionServiceError> {
    if slot.state != WorkerSlotState::Running
        || reservation.state != ExecutionReservationState::Running
    {
        return Err(service_error(
            ProductSessionServiceErrorCode::WorkerSlotNotRunning,
            "ProductSession continue requires a running Worker slot and reservation",
        ));
    }
    if slot.authority != command.runtime_authority
        || reservation.scope != command.execution_scope
        || reservation.worker_pool_id != command.worker_pool_id
        || reservation.job_id != slot.authority.job_id
    {
        return Err(binding_mismatch(
            "supplied runtime identity does not equal the durable Worker slot and reservation",
        ));
    }
    Ok(())
}

fn validate_replacement_worker_source(
    command: &ReplaceProductSessionExecutionBindingCommand,
    slot: &WorkerSlotRecord,
    reservation: &ExecutionReservationRecord,
) -> Result<(), ProductSessionServiceError> {
    if slot.state != WorkerSlotState::Running
        || reservation.state != ExecutionReservationState::Running
    {
        return Err(service_error(
            ProductSessionServiceErrorCode::WorkerSlotNotRunning,
            "ProductSession replacement requires a running successor slot and reservation",
        ));
    }
    if slot.authority != command.runtime_authority
        || reservation.scope != command.execution_scope
        || reservation.worker_pool_id != command.worker_pool_id
        || reservation.job_id != slot.authority.job_id
    {
        return Err(binding_mismatch(
            "replacement successor differs from the durable Worker slot and reservation",
        ));
    }
    Ok(())
}

fn ensure_replacement_binding_is_unique(
    catalog: &PersistedProductSessionCatalog,
    candidate: &SessionBinding,
) -> Result<(), ProductSessionServiceError> {
    for persisted in catalog
        .sessions
        .values()
        .flat_map(|session| session.bindings.iter())
    {
        let existing = persisted.to_domain()?;
        let binding = existing.binding();
        if binding.execution_job_id() == candidate.execution_job_id() {
            continue;
        }
        if binding.worker_session_id() == candidate.worker_session_id()
            || binding.codex_thread_id() == candidate.codex_thread_id()
        {
            return Err(service_error(
                ProductSessionServiceErrorCode::BindingConflict,
                "replacement WorkerSession or CodexThread is already bound",
            ));
        }
    }
    Ok(())
}

fn replacement_binding_index(
    persisted: &PersistedProductSession,
    command: &ReplaceProductSessionExecutionBindingCommand,
) -> Result<usize, ProductSessionServiceError> {
    let mut matches = persisted
        .bindings
        .iter()
        .enumerate()
        .filter_map(|(index, binding)| {
            let identity = binding.identity.to_domain().ok()?;
            (identity.execution_job_id() == command.binding_identity.execution_job_id())
                .then_some(index)
        });
    let index = matches.next().ok_or_else(|| {
        binding_mismatch("ProductSession replacement predecessor binding is missing")
    })?;
    if matches.next().is_some() {
        return Err(corrupt(
            "ProductSession contains duplicate bindings for one ExecutionJob",
        ));
    }
    Ok(index)
}

fn validate_replacement_predecessor(
    persisted: &PersistedSessionBinding,
    command: &ReplaceProductSessionExecutionBindingCommand,
) -> Result<(), ProductSessionServiceError> {
    let predecessor = command
        .replacement
        .predecessor_slot()
        .ok_or_else(|| binding_mismatch("ProductSession replacement predecessor is missing"))?;
    let identity = persisted.identity.to_domain()?;
    if identity != command.binding_identity
        || &persisted.slot.authority != predecessor
        || persisted.reservation.scope != command.execution_scope
        || persisted.reservation.worker_pool_id != command.worker_pool_id
        || command.replacement.previous_worker_session_id()
            != Some(&persisted.slot.authority.worker_session_id)
    {
        return Err(binding_mismatch(
            "ProductSession replacement predecessor differs from the durable binding",
        ));
    }
    Ok(())
}

fn replace_bound_turn_exchange(
    persisted: &mut PersistedProductSession,
    command: &ReplaceProductSessionExecutionBindingCommand,
) -> Result<(), ProductSessionServiceError> {
    let mut matches = persisted
        .turn_intents
        .iter_mut()
        .filter(|turn| turn.execution_job_id == command.runtime_authority.job_id);
    let turn = matches
        .next()
        .ok_or_else(|| binding_mismatch("ProductSession replacement Chat turn is missing"))?;
    if matches.next().is_some() || turn.state != PersistedTurnState::Bound {
        return Err(binding_mismatch(
            "ProductSession replacement Chat turn is not the unique bound turn",
        ));
    }
    turn.model_exchange_id = Some(command.model_exchange_id.clone());
    Ok(())
}

fn build_binding(
    identity: &SessionBindingIdentity,
    slot: &WorkerSlotRecord,
) -> Result<SessionBinding, ProductSessionServiceError> {
    let source = RuntimeSourceIdentity::execution_worker(
        slot.authority.lease_id.clone(),
        slot.authority.worker_id.clone(),
        slot.authority.worker_instance_id.clone(),
        slot.authority.worker_session_id.clone(),
    )
    .map_err(|error| binding_error(&error))?;
    SessionBinding::pending(identity.clone())
        .and_then(|binding| binding.accept_worker_session(slot.authority.worker_session_id.clone()))
        .and_then(|binding| binding.accept_codex_thread(slot.authority.codex_thread_id.clone()))
        .and_then(|binding| binding.with_source_identity(source))
        .map_err(|error| binding_error(&error))
}

fn continue_lifecycle(
    session: &mut ProductSession,
    now: Instant,
    has_pending_turn: bool,
) -> Result<(), ProductSessionServiceError> {
    match session.state() {
        ProductSessionState::Idle | ProductSessionState::Failed => session.begin_turn(now),
        ProductSessionState::WaitingForInput | ProductSessionState::WaitingForApproval => {
            session.resume(now)
        }
        ProductSessionState::Running if has_pending_turn => return Ok(()),
        ProductSessionState::Running
        | ProductSessionState::Cancelled
        | ProductSessionState::Closed => {
            return Err(service_error(
                ProductSessionServiceErrorCode::InvalidState,
                "ProductSession state cannot accept a new continuation",
            ));
        }
    }
    .map_err(|error| domain_error(&error))
}

fn ensure_binding_is_unique(
    catalog: &PersistedProductSessionCatalog,
    candidate: &SessionBinding,
) -> Result<(), ProductSessionServiceError> {
    for persisted in catalog
        .sessions
        .values()
        .flat_map(|session| session.bindings.iter())
    {
        let existing = persisted.to_domain()?;
        let binding = existing.binding();
        if binding.execution_job_id() == candidate.execution_job_id()
            || binding.worker_session_id() == candidate.worker_session_id()
            || binding.codex_thread_id() == candidate.codex_thread_id()
        {
            return Err(service_error(
                ProductSessionServiceErrorCode::BindingConflict,
                "ExecutionJob, WorkerSession, or CodexThread is already bound",
            ));
        }
    }
    Ok(())
}

fn ensure_persisted_bindings_unique(
    catalog: &PersistedProductSessionCatalog,
) -> Result<(), ProductSessionServiceError> {
    let bindings = catalog
        .sessions
        .values()
        .flat_map(|session| session.bindings.iter())
        .map(PersistedSessionBinding::to_domain)
        .collect::<Result<Vec<_>, _>>()?;
    for (index, binding) in bindings.iter().enumerate() {
        if bindings.iter().skip(index + 1).any(|other| {
            binding.binding().execution_job_id() == other.binding().execution_job_id()
                || binding.binding().worker_session_id() == other.binding().worker_session_id()
                || binding.binding().codex_thread_id() == other.binding().codex_thread_id()
        }) {
            return Err(corrupt(
                "ProductSession catalog contains a reused runtime identity",
            ));
        }
    }
    Ok(())
}

fn decode_receipt(
    receipt: &CommitReceipt,
    expected_kind: MutationKind,
    replayed: bool,
) -> Result<ProductSessionMutationReceipt, ProductSessionServiceError> {
    let mut internal_events = receipt
        .events
        .iter()
        .filter(|event| event.topic == PRODUCT_SESSION_RECEIPT_TOPIC);
    let event = internal_events
        .next()
        .ok_or_else(|| corrupt("ProductSession command receipt has no internal replay event"))?;
    if internal_events.next().is_some() {
        return Err(corrupt(
            "ProductSession command receipt has multiple internal replay events",
        ));
    }
    let persisted: PersistedMutationEvent = serde_json::from_slice(&event.payload)
        .map_err(|error| corrupt(format!("ProductSession receipt cannot be decoded: {error}")))?;
    if persisted.schema_version != PRODUCT_SESSION_SERVICE_SCHEMA_VERSION
        || persisted.kind != expected_kind
        || persisted.catalog_revision != receipt.revision
    {
        return Err(corrupt(
            "ProductSession receipt contract, command, or revision is inconsistent",
        ));
    }
    Ok(ProductSessionMutationReceipt {
        record: persisted.record.to_record()?,
        catalog_revision: persisted.catalog_revision,
        replayed,
    })
}

fn mutation_outbox_events(
    output_gate: &crate::CredentialLeakGate,
    context: &ProductSessionCommandContext,
    kind: MutationKind,
    record: &ProductSessionRecord,
    product_session_id: &ProductSessionId,
    internal_payload: Vec<u8>,
) -> Result<Vec<NewOutboxEvent>, ProductSessionServiceError> {
    let internal_event = NewOutboxEvent::internal(
        format!("internal:{}", context.event_id.0),
        PRODUCT_SESSION_RECEIPT_TOPIC,
        internal_payload,
    );
    let public_projection = record.projection()?;
    let changed_projection = ControlPlaneWebSocketProductSessionChangedEvent {
        product_session_id: public_projection.id,
        revision: public_projection.revision,
        status: public_session_status(record.session().state()).to_owned(),
        title: Some(public_projection.title),
        type_value:
            ControlPlaneWebSocketProductSessionChangedEventTypeValue::ProductSessionChangedV1,
    };
    inspect_public_output(output_gate, &changed_projection)?;
    let changed_payload = serde_json::to_vec(&changed_projection).map_err(|error| {
        corrupt(format!(
            "ProductSession public event cannot be encoded: {error}"
        ))
    })?;
    let changed_event = public_outbox_event(
        context,
        context.event_id.clone(),
        PRODUCT_SESSION_CHANGED_TOPIC,
        changed_payload,
        product_session_id,
    )?;
    let mut events = vec![internal_event, changed_event];
    if matches!(
        kind,
        MutationKind::ChatSubmitted | MutationKind::AssistantUpdated
    ) {
        let message = record.messages().last().ok_or_else(|| {
            corrupt("ProductSession message mutation has no public message projection")
        })?;
        let message_projection = ControlPlaneWebSocketProductSessionMessageAppendedEvent {
            message: message.clone(),
            product_session_id: product_session_id.clone(),
            type_value:
                ControlPlaneWebSocketProductSessionMessageAppendedEventTypeValue::ProductSessionMessageAppendedV1,
        };
        inspect_public_output(output_gate, &message_projection)?;
        let message_payload = serde_json::to_vec(&message_projection).map_err(|error| {
            corrupt(format!(
                "ProductSession public message event cannot be encoded: {error}"
            ))
        })?;
        events.push(public_outbox_event(
            context,
            derived_public_event_id(&context.event_id, PRODUCT_SESSION_MESSAGE_APPENDED_TOPIC),
            PRODUCT_SESSION_MESSAGE_APPENDED_TOPIC,
            message_payload,
            product_session_id,
        )?);
    }
    Ok(events)
}

fn public_outbox_event(
    context: &ProductSessionCommandContext,
    event_id: ControlPlaneEventId,
    topic: &'static str,
    payload: Vec<u8>,
    product_session_id: &ProductSessionId,
) -> Result<NewOutboxEvent, ProductSessionServiceError> {
    NewOutboxEvent::public_projection(
        event_id,
        topic,
        payload,
        ProjectionEventStream::ProductSession(product_session_id.clone()),
        context.public_scope.clone(),
        context.occurred_at.clone(),
        PublicEventSource::ControlPlane {
            actor: context.public_actor.clone(),
            component: "product-session-service".to_owned(),
        },
    )
    .map_err(|error| storage_error(&error))
}

fn derived_public_event_id(base: &ControlPlaneEventId, namespace: &str) -> ControlPlaneEventId {
    let digest = Sha256::digest(
        [
            b"winwincode.product-session.public-event.v1\0".as_slice(),
            namespace.as_bytes(),
            b"\0",
            base.0.as_bytes(),
        ]
        .concat(),
    );
    let encoded = format!("{digest:X}");
    ControlPlaneEventId(format!("evt_{}", &encoded[..26]))
}

fn require_revision(
    session: &ProductSession,
    expected_revision: u64,
) -> Result<(), ProductSessionServiceError> {
    if session.revision() != expected_revision {
        return Err(service_error(
            ProductSessionServiceErrorCode::RevisionConflict,
            format!(
                "ProductSession expected revision {expected_revision}, current revision {}",
                session.revision()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn catalog_stream_id(scope: &ReceiptScopeKey) -> String {
    format!("product-sessions:{:x}", Sha256::digest(scope.as_bytes()))
}

pub(crate) fn load_product_session_model_selection(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    product_session_id: &ProductSessionId,
) -> Result<SessionModelSelection, ProductSessionServiceError> {
    let scope_key = crate::repository_scope_key(scope).map_err(|error| storage_error(&error))?;
    let stream_id = catalog_stream_id(&scope_key);
    let state = storage
        .load_state(&stream_id)
        .map_err(|error| storage_error(&error))?
        .ok_or_else(|| {
            service_error(
                ProductSessionServiceErrorCode::NotFound,
                "ProductSession was not found in this repository scope",
            )
        })?;
    let catalog: PersistedProductSessionCatalog = serde_json::from_slice(&state.payload)
        .map_err(|_| corrupt("ProductSession catalog cannot be decoded"))?;
    catalog.validate(state.revision)?;
    catalog
        .sessions
        .get(&product_session_id.0)
        .map(|session| session.model_selection.clone())
        .ok_or_else(|| {
            service_error(
                ProductSessionServiceErrorCode::NotFound,
                "ProductSession was not found in this repository scope",
            )
        })
}

fn command_digest(
    kind: &str,
    fields: serde_json::Value,
) -> Result<Sha256Digest, ProductSessionServiceError> {
    let bytes = serde_json::to_vec(&(kind, fields))
        .map_err(|error| corrupt(format!("ProductSession command cannot be encoded: {error}")))?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn context_digest_fields(context: &ProductSessionCommandContext) -> serde_json::Value {
    serde_json::json!({
        "expectedRevision": context.expected_revision,
    })
}

fn command_digest_fields(command: &CreateProductSessionCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id.0,
        "projectId": command.project_id.0,
        "repositoryId": command.repository_id.0,
        "title": command.title,
        "modelSelection": command.model_selection,
    })
}

fn continue_digest_fields(command: &ContinueProductSessionCommand) -> serde_json::Value {
    let identity = PersistedBindingIdentity::from_domain(&command.binding_identity);
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id.0,
        "bindingIdentity": identity,
        "runtimeAuthority": command.runtime_authority,
        "executionScope": command.execution_scope,
        "workerPoolId": command.worker_pool_id,
        "modelExchangeId": command.model_exchange_id.0,
    })
}

fn replace_execution_binding_digest_fields(
    command: &ReplaceProductSessionExecutionBindingCommand,
) -> serde_json::Value {
    let identity = PersistedBindingIdentity::from_domain(&command.binding_identity);
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id.0,
        "bindingIdentity": identity,
        "runtimeAuthority": command.runtime_authority,
        "executionScope": command.execution_scope,
        "workerPoolId": command.worker_pool_id,
        "modelExchangeId": command.model_exchange_id.0,
        "replacementReceiptId": command.replacement.receipt_id(),
        "replacementReceiptDigest": command.replacement.receipt_digest(),
        "replacementPredecessorLease": command.replacement.predecessor_lease(),
        "replacementPredecessorSlot": command.replacement.predecessor_slot(),
        "replacementSuccessorLease": command.replacement.replacement_lease(),
    })
}

fn fork_digest_fields(command: &ForkProductSessionCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "sourceProductSessionId": command.source_product_session_id.0,
        "productSessionId": command.product_session_id.0,
        "title": command.title,
    })
}

fn close_digest_fields(command: &CloseProductSessionCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id.0,
    })
}

fn validate_model_selection(
    selection: &SessionModelSelection,
) -> Result<(), ProductSessionServiceError> {
    let source_is_valid = match &selection.account_source {
        ProviderAccountSource::SystemDefaultProviderAccountSource(_) => true,
        ProviderAccountSource::PersonalProviderAccountSource(source) => {
            canonical_id(&source.account_connection_id.0, "pac_")
        }
        ProviderAccountSource::EnterpriseProviderAccountPoolSource(source) => {
            canonical_id(&source.account_pool_id.0, "pap_")
        }
    };
    if selection.provider_id.is_empty()
        || selection.provider_id.chars().count() > 128
        || selection.model_id.is_empty()
        || selection.model_id.chars().count() > 256
        || !source_is_valid
    {
        return Err(service_error(
            ProductSessionServiceErrorCode::InvalidInput,
            "ProductSession model selection is invalid",
        ));
    }
    Ok(())
}

fn validate_create_scope(
    command: &CreateProductSessionCommand,
) -> Result<(), ProductSessionServiceError> {
    match &command.context.public_scope {
        PublicEventScope::Repository {
            project_id,
            repository_id,
            ..
        } if project_id == &command.project_id && repository_id == &command.repository_id => Ok(()),
        PublicEventScope::Organization { .. }
        | PublicEventScope::Workspace { .. }
        | PublicEventScope::Project { .. }
        | PublicEventScope::Repository { .. } => Err(service_error(
            ProductSessionServiceErrorCode::InvalidInput,
            "ProductSession create scope differs from its project or repository",
        )),
    }
}

fn inspect_public_output<T: Serialize + ?Sized>(
    gate: &crate::CredentialLeakGate,
    value: &T,
) -> Result<(), ProductSessionServiceError> {
    gate.inspect_serializable(crate::CredentialOutputBoundary::Persistence, value)
        .map_err(|_| {
            service_error(
                ProductSessionServiceErrorCode::CredentialLeak,
                "ProductSession public output was rejected by the Credential gate",
            )
        })
}

fn session_state_label(state: ProductSessionState) -> &'static str {
    match state {
        ProductSessionState::Idle => "idle",
        ProductSessionState::Running => "running",
        ProductSessionState::WaitingForInput => "waiting_for_input",
        ProductSessionState::WaitingForApproval => "waiting_for_approval",
        ProductSessionState::Cancelled => "cancelled",
        ProductSessionState::Closed => "closed",
        ProductSessionState::Failed => "failed",
    }
}

fn public_session_status(state: ProductSessionState) -> &'static str {
    match state {
        ProductSessionState::Idle | ProductSessionState::Running => "active",
        ProductSessionState::WaitingForInput => "waiting-input",
        ProductSessionState::WaitingForApproval => "waiting-approval",
        ProductSessionState::Closed => "completed",
        ProductSessionState::Failed => "failed",
        ProductSessionState::Cancelled => "cancelled",
    }
}

fn canonical_id(value: &str, prefix: &str) -> bool {
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

fn storage_error(error: &StorageError) -> ProductSessionServiceError {
    let code = match error.kind() {
        StorageErrorKind::RevisionConflict => ProductSessionServiceErrorCode::RevisionConflict,
        StorageErrorKind::RequestConflict | StorageErrorKind::RequestReplayMissing => {
            ProductSessionServiceErrorCode::RequestConflict
        }
        StorageErrorKind::InvalidInput => ProductSessionServiceErrorCode::InvalidInput,
        StorageErrorKind::Adapter
        | StorageErrorKind::Closed
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::EventCursorExpired => ProductSessionServiceErrorCode::Storage,
    };
    service_error(code, format!("ProductSession storage failed: {error}"))
}

fn domain_error(error: &ProductSessionError) -> ProductSessionServiceError {
    let code = match error {
        ProductSessionError::InvalidTransition { .. } => {
            ProductSessionServiceErrorCode::InvalidState
        }
        ProductSessionError::InvalidIdentity(_)
        | ProductSessionError::InvalidTitle
        | ProductSessionError::InvalidReason
        | ProductSessionError::InvalidInstant => ProductSessionServiceErrorCode::InvalidInput,
        ProductSessionError::RevisionOverflow => ProductSessionServiceErrorCode::CorruptState,
    };
    service_error(code, error.to_string())
}

fn binding_error(error: &SessionBindingError) -> ProductSessionServiceError {
    service_error(
        ProductSessionServiceErrorCode::BindingIdentityMismatch,
        error.to_string(),
    )
}

fn not_found() -> ProductSessionServiceError {
    service_error(
        ProductSessionServiceErrorCode::NotFound,
        "ProductSession does not exist in this scope",
    )
}

fn binding_mismatch(message: impl Into<String>) -> ProductSessionServiceError {
    service_error(
        ProductSessionServiceErrorCode::BindingIdentityMismatch,
        message,
    )
}

fn corrupt(message: impl Into<String>) -> ProductSessionServiceError {
    service_error(ProductSessionServiceErrorCode::CorruptState, message)
}

fn service_error(
    code: ProductSessionServiceErrorCode,
    message: impl Into<String>,
) -> ProductSessionServiceError {
    ProductSessionServiceError::new(code, message)
}
