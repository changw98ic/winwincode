// SPDX-License-Identifier: Apache-2.0

//! Durable application boundary for Chat input and approval interactions.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ApprovalDecideCommand, ApprovalDecideCompletedResponse,
    ApprovalDecideCompletedResponseCommand, ApprovalDecideCompletedResponseOutcome,
    ApprovalGetQuery, ApprovalGetResultResponse, ApprovalListQuery, ApprovalListResultResponse,
    ApprovalProjection, ChatInteractionListQuery, ChatInteractionListResultResponse,
    ControlPlaneWebSocketApprovalChangedEvent, ControlPlaneWebSocketApprovalChangedEventTypeValue,
    ControlPlaneWebSocketChatInteractionsInvalidatedEvent, InputRespondCommand,
    InputRespondCompletedResponse, InputRespondCompletedResponseCommand,
    InputRespondCompletedResponseOutcome, ProductSessionProjection, ServiceAccountActor,
    ServiceAccountActorKind, SystemActor, SystemActorKind,
};
use winwincode_domain::{
    ControlPlaneEventId, ExecutionMessageId, Instant, ProductSessionId, RequestId, Revision,
    SchemaVersion, Sha256Digest, SystemActorId,
};
use winwincode_domain::{RepositoryScope, UserActor, UserActorKind};
use winwincode_execution_port::generated::{
    ApprovalDecisionMessage, ApprovalDecisionMessageDecision, ApprovalDecisionMessageKind,
    ApprovalDecisionMessageScope, ApprovalRequestMessage, ExecutionLeaseStamp,
    ExecutionPortMessage, InputRequestMessage, InputResponseMessage, InputResponseMessageKind,
    InputResponseMessageStatus,
};
use winwincode_storage::{
    ExecutionLeaseRecord, ExecutionQueueScope, ExecutionReservationRecord,
    ExecutionReservationState, NewOutboxEvent, ProjectionEventStream, PublicEventActor,
    PublicEventScope, PublicEventSource, ReceiptIdentity, ReceiptScopeKey, StateCommit,
    StateRevisionGuard, StorageError, StorageErrorKind, WorkerPoolId, WorkerSlotAuthority,
    WorkerSlotRecord, WorkerSlotState, public_receipt_identity,
};

use crate::chat_interaction_projection::{
    ChatInteractionProjectionError, ChatInteractionProjectionEvent,
    ChatInteractionProjectionLedger, ChatInteractionProjectionSnapshot, ProjectionWriteStatus,
};
use crate::gate_interaction_service::{
    GateCandidateIdentity, GateHumanDecision, GateInteractionActor, GateInteractionAuthority,
    GateInteractionCommandContext, GateInteractionRecord, GateInteractionService,
    GateInteractionServiceError, GateInteractionServiceErrorCode, GateInteractionState,
    GateInteractionSubject, RespondGateInteractionCommand,
};
use crate::product_session_service::{deterministic_event_id, query_scope};
use crate::{
    CredentialLeakGate, CredentialOutputBoundary, ProductSessionApiClock,
    ProductSessionCommandContext, ProductSessionPersistence, ProductSessionService,
    ProductSessionServiceError, ProductSessionServiceErrorCode, product_session_command_context,
};

const CHAT_INTERACTION_SCHEMA_VERSION: u8 = 1;
const CHAT_INTERACTION_RECEIPT_TOPIC: &str = "chat-interaction.receipt.internal.v1";
const CHAT_INTERACTION_INVALIDATED_TOPIC: &str = "chat-interactions.invalidated.v1";
const APPROVAL_CHANGED_TOPIC: &str = "approval.changed.v1";
const WORKER_RECEIPT_ACTOR_ID: &str = "sys_00000000000000000000000001";
const MAX_DECISION_REASON_BYTES: usize = 2_000;

/// Canonical Approval fact used only to rebuild the collaboration Inbox.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct CollaborationApprovalSourceRecord {
    pub projection: ApprovalProjection,
    pub candidate: Option<GateCandidateIdentity>,
    pub delivery_id: Option<winwincode_domain::DeliveryId>,
}

/// One scope-wide Approval cut and its exact durable catalog guard.
pub(crate) struct CollaborationApprovalSourceSnapshot {
    pub revision: u64,
    pub snapshot_sha256: Sha256Digest,
    pub state_guard: StateRevisionGuard,
    pub approvals: Vec<CollaborationApprovalSourceRecord>,
}

/// Exact Control Plane facts supplied with one Worker input request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerInteractionAuthority {
    pub execution_scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub product_session_revision: u64,
    pub job_revision: u64,
    pub worker_slot_revision: u64,
}

/// Records one typed Worker input request.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordInputInteractionCommand {
    pub authority: WorkerInteractionAuthority,
    pub request: InputRequestMessage,
}

/// Records one typed Worker approval request after its Gate fact exists.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordApprovalInteractionCommand {
    pub public_scope: PublicEventScope,
    pub request: ApprovalRequestMessage,
}

/// Whether this exact call changed the public interaction snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatInteractionWriteStatus {
    Applied,
    Duplicate,
}

/// Replay-safe result of accepting a Worker interaction request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatInteractionMutationReceipt {
    pub status: ChatInteractionWriteStatus,
    pub revision: Revision,
    pub product_session_id: ProductSessionId,
    pub replayed: bool,
}

/// Durable result plus the exact Worker response. Replays deterministically
/// rebuild it from the command and durable authority without storing the value.
#[derive(Clone, Debug, PartialEq)]
pub struct InputRespondMutationReceipt {
    pub current_revision: Revision,
    pub previous_revision: Revision,
    pub product_session: ProductSessionProjection,
    pub occurred_at: Instant,
    pub worker_response: Option<InputResponseMessage>,
    pub replayed: bool,
}

/// Durable result plus the exact Worker decision. Replays deterministically
/// rebuild it from the command and durable authority.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalDecideMutationReceipt {
    pub current_revision: Revision,
    pub previous_revision: Revision,
    pub approval: ApprovalProjection,
    pub occurred_at: Instant,
    pub worker_decision: Option<ApprovalDecisionMessage>,
    pub replayed: bool,
}

/// Stable outbound delivery failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerInteractionDeliveryErrorKind {
    Unavailable,
    Rejected,
}

/// Bounded failure returned by the canonical Control Plane-to-Worker port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerInteractionDeliveryError {
    kind: WorkerInteractionDeliveryErrorKind,
    message: String,
}

impl WorkerInteractionDeliveryError {
    #[must_use]
    pub fn new(kind: WorkerInteractionDeliveryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> WorkerInteractionDeliveryErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerInteractionDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkerInteractionDeliveryError {}

/// Unique outbound seam for replay-stable Control Plane-to-Worker messages.
/// Implementations route by the exact binding and treat a stable `messageId`
/// duplicate as already delivered.
pub trait WorkerInteractionOutboundPort: Send {
    /// Delivers one canonical `ExecutionPort` message.
    ///
    /// # Errors
    ///
    /// Returns a bounded failure when the exact Worker route is unavailable
    /// or rejects the message.
    fn deliver(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<(), WorkerInteractionDeliveryError>;
}

/// Stable application failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatInteractionServiceErrorCode {
    InvalidInput,
    NotFound,
    RequestConflict,
    RevisionConflict,
    AuthorityMismatch,
    ActorMismatch,
    Expired,
    WrongState,
    CorruptState,
    CredentialLeak,
    WorkerDelivery,
    Storage,
}

/// Fail-closed durable Chat interaction error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatInteractionServiceError {
    code: ChatInteractionServiceErrorCode,
    message: String,
}

impl ChatInteractionServiceError {
    #[must_use]
    pub const fn code(&self) -> ChatInteractionServiceErrorCode {
        self.code
    }
}

impl fmt::Display for ChatInteractionServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ChatInteractionServiceError {}

/// Durable application service for Worker requests, browser decisions, and
/// restart-stable interaction queries.
pub struct ChatInteractionService<'storage> {
    storage: &'storage mut dyn ProductSessionPersistence,
    output_gate: CredentialLeakGate,
}

impl<'storage> ChatInteractionService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ProductSessionPersistence) -> Self {
        Self {
            storage,
            output_gate: CredentialLeakGate::new(),
        }
    }

    #[must_use]
    pub fn with_output_gate(
        storage: &'storage mut dyn ProductSessionPersistence,
        output_gate: &CredentialLeakGate,
    ) -> Self {
        Self {
            storage,
            output_gate: output_gate.fingerprint_snapshot(),
        }
    }

    /// Persists one Worker `input.request`, its receipt, and its public
    /// invalidation in one transaction.
    ///
    /// # Errors
    ///
    /// Rejects stale runtime authority, conflicting replay, unsafe public
    /// output, or corrupt durable state before writing.
    pub fn record_input(
        &mut self,
        command: &RecordInputInteractionCommand,
    ) -> Result<ChatInteractionMutationReceipt, ChatInteractionServiceError> {
        let sent_at = command.request.sent_at.clone();
        self.record_input_at(command, &sent_at)
    }

    /// Persists one Worker input request using the Server's trusted ingress
    /// clock for lease validation.
    ///
    /// The Worker `sentAt` remains part of the receipt identity and public
    /// event fact, but it is never the authority used to decide whether the
    /// current lease is still valid. Exact receipt replay is resolved before
    /// this trusted-time check, so a response remains replayable after expiry.
    ///
    /// # Errors
    ///
    /// Rejects stale runtime authority, conflicting replay, unsafe public
    /// output, or corrupt durable state before writing.
    pub fn record_input_at(
        &mut self,
        command: &RecordInputInteractionCommand,
        trusted_now: &Instant,
    ) -> Result<ChatInteractionMutationReceipt, ChatInteractionServiceError> {
        let context = worker_context(
            "input.request",
            &command.request.message_id,
            &command.request.request_id,
            &command.request.sent_at,
            public_scope_from_execution(&command.authority.execution_scope),
        )?;
        let digest = command_digest("input.request", &command.request)?;
        if let Some(receipt) = self.replay_worker(&context.receipt_identity, &digest)? {
            return Ok(receipt);
        }
        let authority = persisted_authority_from_input(command)?;
        self.require_current_runtime(
            context.receipt_identity.scope_key(),
            &authority,
            trusted_now,
        )?;
        let source = authority.public_source();
        let mut catalog = self.load_catalog(context.receipt_identity.scope_key())?;
        let mut ledger = ChatInteractionProjectionLedger::restore(catalog.snapshot.clone())
            .map_err(projection_error)?;
        let write = ledger
            .record_input_request(&command.request)
            .map_err(projection_error)?;
        let status = write_status(write.status);
        if status == ChatInteractionWriteStatus::Applied {
            catalog
                .authorities
                .insert(input_key(&command.request.input_request_id.0), authority);
            catalog.snapshot = ledger.snapshot();
        }
        self.commit_worker(
            &context,
            digest,
            MutationKind::InputRecorded,
            catalog,
            &ChatInteractionMutationReceipt {
                status,
                revision: write.revision,
                product_session_id: write.product_session_id,
                replayed: false,
            },
            None,
            source,
        )
    }

    /// Persists one Worker `approval.request` after re-reading the existing
    /// pending Gate fact and current runtime authority.
    ///
    /// # Errors
    ///
    /// Rejects missing, terminal, stale, cross-scope, or mismatched Gate and
    /// Worker facts before writing.
    pub fn record_approval(
        &mut self,
        command: &RecordApprovalInteractionCommand,
    ) -> Result<ChatInteractionMutationReceipt, ChatInteractionServiceError> {
        let sent_at = command.request.sent_at.clone();
        self.record_approval_at(command, &sent_at)
    }

    /// Persists one Worker approval request using the Server's trusted ingress
    /// clock for Gate and runtime lease validation.
    ///
    /// # Errors
    ///
    /// Rejects missing, terminal, stale, cross-scope, or mismatched Gate and
    /// Worker facts before writing.
    pub fn record_approval_at(
        &mut self,
        command: &RecordApprovalInteractionCommand,
        trusted_now: &Instant,
    ) -> Result<ChatInteractionMutationReceipt, ChatInteractionServiceError> {
        let context = worker_context(
            "approval.request",
            &command.request.message_id,
            &command.request.request_id,
            &command.request.sent_at,
            command.public_scope.clone(),
        )?;
        let digest = command_digest("approval.request", &command.request)?;
        if let Some(receipt) = self.replay_worker(&context.receipt_identity, &digest)? {
            return Ok(receipt);
        }
        let subject = GateInteractionSubject::Approval(command.request.approval_id.clone());
        let gate_record = {
            let mut gate = GateInteractionService::new(self.storage);
            let record = gate
                .get(context.receipt_identity.scope_key(), &subject)
                .map_err(gate_error)?
                .ok_or_else(|| not_found("Approval Gate fact does not exist"))?;
            gate.require_current_pending(
                context.receipt_identity.scope_key(),
                &record,
                trusted_now,
            )
            .map_err(gate_error)?;
            record
        };
        let authority = persisted_authority_from_approval(&gate_record, &command.request)?;
        self.require_current_runtime(
            context.receipt_identity.scope_key(),
            &authority,
            trusted_now,
        )?;
        let source = authority.public_source();
        let mut catalog = self.load_catalog(context.receipt_identity.scope_key())?;
        let mut ledger = ChatInteractionProjectionLedger::restore(catalog.snapshot.clone())
            .map_err(projection_error)?;
        let write = ledger
            .record_approval_request(&command.request)
            .map_err(projection_error)?;
        let status = write_status(write.status);
        if status == ChatInteractionWriteStatus::Applied {
            catalog
                .authorities
                .insert(approval_key(&command.request.approval_id.0), authority);
            catalog.approval_requested_by.insert(
                command.request.approval_id.0.clone(),
                actor_from_gate(&gate_record.authorized_actor),
            );
            catalog.snapshot = ledger.snapshot();
        }
        let projection = ledger
            .approval(&command.request.approval_id, &command.request.sent_at)
            .map_err(projection_error)?
            .ok_or_else(|| corrupt("recorded Approval projection is missing"))?;
        self.commit_worker(
            &context,
            digest,
            MutationKind::ApprovalRecorded,
            catalog,
            &ChatInteractionMutationReceipt {
                status,
                revision: write.revision,
                product_session_id: write.product_session_id,
                replayed: false,
            },
            Some(approval_changed(
                &projection,
                actor_from_gate(&gate_record.authorized_actor),
                None,
            )),
            source,
        )
    }

    /// Applies one generated `input.respond` command and returns the public
    /// result plus a replay-stable `ExecutionPort` response.
    ///
    /// # Errors
    ///
    /// Rejects mismatched actor scope, interaction revision, runtime binding,
    /// current lease, or `ProductSession` authority.
    pub fn respond_input(
        &mut self,
        context: &ProductSessionCommandContext,
        command: &InputRespondCommand,
    ) -> Result<InputRespondMutationReceipt, ChatInteractionServiceError> {
        let digest = command_digest("input.respond", command)?;
        if let Some(mut receipt) = self.replay_input(&context.receipt_identity, &digest)? {
            let catalog = self.load_catalog(context.receipt_identity.scope_key())?;
            let authority = catalog
                .authorities
                .get(&input_key(&command.payload.input_request_id.0))
                .ok_or_else(|| corrupt("replayed Input authority is missing"))?;
            receipt.worker_response = Some(input_response_message(
                command,
                authority,
                &receipt.occurred_at,
            )?);
            return Ok(receipt);
        }
        let mut catalog = self.load_catalog(context.receipt_identity.scope_key())?;
        let authority = catalog
            .authorities
            .get(&input_key(&command.payload.input_request_id.0))
            .cloned()
            .ok_or_else(|| not_found("Input request does not exist"))?;
        self.require_current_runtime(
            context.receipt_identity.scope_key(),
            &authority,
            &context.occurred_at,
        )?;
        let mut ledger = ChatInteractionProjectionLedger::restore(catalog.snapshot.clone())
            .map_err(projection_error)?;
        let current_revision = ledger
            .apply_input_response(
                &command.expected_revision,
                &command.payload,
                &context.occurred_at,
            )
            .map_err(projection_error)?;
        catalog.snapshot = ledger.snapshot();
        let product_session = ProductSessionService::new(self.storage)
            .get(
                context.receipt_identity.scope_key(),
                &command.payload.product_session_id,
            )
            .map_err(product_session_error)?
            .ok_or_else(|| not_found("ProductSession does not exist"))?
            .projection()
            .map_err(product_session_error)?;
        let mut receipt = self.commit_input(
            context,
            digest,
            catalog,
            &InputRespondMutationReceipt {
                current_revision,
                previous_revision: command.expected_revision.clone(),
                product_session,
                occurred_at: context.occurred_at.clone(),
                worker_response: None,
                replayed: false,
            },
        )?;
        receipt.worker_response = Some(input_response_message(
            command,
            &authority,
            &receipt.occurred_at,
        )?);
        Ok(receipt)
    }

    /// Applies one generated `approval.decide` command. The Chat snapshot is
    /// the primary state and the canonical Gate state is one secondary CAS
    /// mutation in the same receipt transaction.
    ///
    /// # Errors
    ///
    /// Rejects stale binding, actor, Gate, lease, revision, or request replay.
    pub fn decide_approval(
        &mut self,
        context: &ProductSessionCommandContext,
        command: &ApprovalDecideCommand,
    ) -> Result<ApprovalDecideMutationReceipt, ChatInteractionServiceError> {
        if command.payload.reason.is_empty()
            || command.payload.reason.len() > MAX_DECISION_REASON_BYTES
        {
            return Err(invalid("Approval decision reason is invalid"));
        }
        let digest = command_digest("approval.decide", command)?;
        if let Some(mut receipt) = self.replay_approval(&context.receipt_identity, &digest)? {
            let catalog = self.load_catalog(context.receipt_identity.scope_key())?;
            let authority = catalog
                .authorities
                .get(&approval_key(&command.payload.approval_id.0))
                .ok_or_else(|| corrupt("replayed Approval authority is missing"))?;
            receipt.worker_decision = Some(approval_decision_message(
                command,
                authority,
                &receipt.occurred_at,
            )?);
            return Ok(receipt);
        }
        let mut catalog = self.load_catalog(context.receipt_identity.scope_key())?;
        let authority = catalog
            .authorities
            .get(&approval_key(&command.payload.approval_id.0))
            .cloned()
            .ok_or_else(|| not_found("Approval does not exist"))?;
        self.require_current_runtime(
            context.receipt_identity.scope_key(),
            &authority,
            &context.occurred_at,
        )?;
        let requested_by = catalog
            .approval_requested_by
            .get(&command.payload.approval_id.0)
            .cloned()
            .ok_or_else(|| corrupt("Approval requester is missing"))?;
        let gate_authority = authority
            .gate_authority
            .clone()
            .ok_or_else(|| corrupt("Approval has no Gate authority"))?;
        let gate_actor = gate_actor_from_api(&command.actor);
        let reason_sha256 = sha256(command.payload.reason.as_bytes());
        let gate_command = RespondGateInteractionCommand {
            context: GateInteractionCommandContext {
                receipt_identity: context.receipt_identity.clone(),
                event_id: context.event_id.clone(),
                occurred_at: context.occurred_at.clone(),
            },
            subject: GateInteractionSubject::Approval(command.payload.approval_id.clone()),
            authority: gate_authority,
            actor: gate_actor,
            decision: match command.payload.decision.as_str() {
                "approve" => GateHumanDecision::Approve { reason_sha256 },
                "reject" => GateHumanDecision::Reject { reason_sha256 },
                _ => return Err(invalid("Approval decision is invalid")),
            },
            responded_at: context.occurred_at.clone(),
        };
        let prepared = GateInteractionService::new(self.storage)
            .prepare_response(&gate_command)
            .map_err(gate_error)?;
        let mut ledger = ChatInteractionProjectionLedger::restore(catalog.snapshot.clone())
            .map_err(projection_error)?;
        let current_revision = ledger
            .apply_approval_decision(
                &command.expected_revision,
                &command.payload,
                &context.occurred_at,
            )
            .map_err(projection_error)?;
        catalog.snapshot = ledger.snapshot();
        let approval = ledger
            .approval(&command.payload.approval_id, &context.occurred_at)
            .map_err(projection_error)?
            .ok_or_else(|| corrupt("resolved Approval projection is missing"))?;
        if gate_state_name(prepared.record().state) != approval.state {
            return Err(corrupt(
                "Gate and public Approval terminal states do not match",
            ));
        }
        let mut receipt = self.commit_approval(
            context,
            digest,
            catalog,
            prepared.state_mutation().map_err(gate_error)?,
            &ApprovalDecideMutationReceipt {
                current_revision,
                previous_revision: command.expected_revision.clone(),
                approval: approval.clone(),
                occurred_at: context.occurred_at.clone(),
                worker_decision: None,
                replayed: false,
            },
            approval_changed(&approval, requested_by, Some(command.actor.clone())),
        )?;
        receipt.worker_decision = Some(approval_decision_message(
            command,
            &authority,
            &receipt.occurred_at,
        )?);
        Ok(receipt)
    }

    /// Runs `session.interactions.list` against one restart-restored snapshot.
    ///
    /// # Errors
    ///
    /// Returns a bounded query, storage, or durable-state failure.
    pub fn interactions(
        &self,
        scope: &ReceiptScopeKey,
        query: &ChatInteractionListQuery,
        now: &Instant,
    ) -> Result<ChatInteractionListResultResponse, ChatInteractionServiceError> {
        ChatInteractionProjectionLedger::restore(self.load_catalog(scope)?.snapshot)
            .map_err(projection_error)?
            .query(query, now)
            .map_err(projection_error)
    }

    /// Runs `approval.get` against one restart-restored snapshot.
    ///
    /// # Errors
    ///
    /// Returns a bounded query, storage, or durable-state failure.
    pub fn approval_get(
        &self,
        scope: &ReceiptScopeKey,
        query: &ApprovalGetQuery,
        now: &Instant,
    ) -> Result<ApprovalGetResultResponse, ChatInteractionServiceError> {
        ChatInteractionProjectionLedger::restore(self.load_catalog(scope)?.snapshot)
            .map_err(projection_error)?
            .approval_get(query, now)
            .map_err(projection_error)
    }

    /// Runs `approval.list` against one restart-restored snapshot.
    ///
    /// # Errors
    ///
    /// Returns a bounded query, storage, or durable-state failure.
    pub fn approval_list(
        &self,
        scope: &ReceiptScopeKey,
        query: &ApprovalListQuery,
        now: &Instant,
    ) -> Result<ApprovalListResultResponse, ChatInteractionServiceError> {
        ChatInteractionProjectionLedger::restore(self.load_catalog(scope)?.snapshot)
            .map_err(projection_error)?
            .approval_list(query, now)
            .map_err(projection_error)
    }

    fn require_current_runtime(
        &mut self,
        scope: &ReceiptScopeKey,
        authority: &PersistedInteractionAuthority,
        now: &Instant,
    ) -> Result<(), ChatInteractionServiceError> {
        if now.0 < authority.lease.issued_at.0 || now.0 >= authority.lease.expires_at.0 {
            return Err(error(
                ChatInteractionServiceErrorCode::Expired,
                "Worker lease is outside its accepted time window",
            ));
        }
        let current = self
            .storage
            .load_worker_interaction_source(
                &authority.execution_scope,
                &authority.worker_pool_id,
                &authority.runtime.worker_session_id,
            )
            .map_err(storage_error)?
            .ok_or_else(|| authority_mismatch("current Worker authority does not exist"))?;
        require_current_source(authority, &current.0, &current.1, &current.2)?;
        let session = ProductSessionService::new(self.storage)
            .get(scope, &authority.execution_scope.product_session_id)
            .map_err(product_session_error)?
            .ok_or_else(|| authority_mismatch("ProductSession does not exist in this scope"))?;
        require_product_session(&session, authority)
    }

    fn load_catalog(
        &self,
        scope: &ReceiptScopeKey,
    ) -> Result<PersistedChatInteractionCatalog, ChatInteractionServiceError> {
        let Some(state) = self
            .storage
            .load_state(&catalog_stream_id(scope))
            .map_err(storage_error)?
        else {
            return Ok(PersistedChatInteractionCatalog::default());
        };
        let catalog: PersistedChatInteractionCatalog = serde_json::from_slice(&state.payload)
            .map_err(|error| {
                corrupt(format!(
                    "Chat interaction catalog cannot be decoded: {error}"
                ))
            })?;
        catalog.validate(state.revision)?;
        Ok(catalog)
    }

    fn replay_worker(
        &self,
        identity: &ReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<ChatInteractionMutationReceipt>, ChatInteractionServiceError> {
        self.storage
            .load_receipt(identity, digest)
            .map_err(storage_error)?
            .map(|receipt| decode_worker_receipt(&receipt.events, true))
            .transpose()
    }

    fn replay_input(
        &self,
        identity: &ReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<InputRespondMutationReceipt>, ChatInteractionServiceError> {
        self.storage
            .load_receipt(identity, digest)
            .map_err(storage_error)?
            .map(|receipt| decode_input_receipt(&receipt.events, true))
            .transpose()
    }

    fn replay_approval(
        &self,
        identity: &ReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<ApprovalDecideMutationReceipt>, ChatInteractionServiceError> {
        self.storage
            .load_receipt(identity, digest)
            .map_err(storage_error)?
            .map(|receipt| decode_approval_receipt(&receipt.events, true))
            .transpose()
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_worker(
        &mut self,
        context: &WorkerCommandContext,
        digest: Sha256Digest,
        kind: MutationKind,
        catalog: PersistedChatInteractionCatalog,
        receipt: &ChatInteractionMutationReceipt,
        approval_event: Option<ControlPlaneWebSocketApprovalChangedEvent>,
        source: PublicEventSource,
    ) -> Result<ChatInteractionMutationReceipt, ChatInteractionServiceError> {
        let expected_revision = catalog.revision;
        let catalog = catalog.next_revision()?;
        let receipt_event =
            PersistedReceiptEvent::worker(kind, catalog.revision, &context.occurred_at, receipt);
        let events =
            self.worker_events(context, &catalog, &receipt_event, approval_event, source)?;
        let commit = StateCommit::new(
            context.receipt_identity.clone(),
            digest,
            catalog_stream_id(context.receipt_identity.scope_key()),
            expected_revision,
            encode_catalog(&catalog)?,
            events,
        );
        let stored = self.storage.commit(&commit).map_err(storage_error)?;
        decode_worker_receipt(&stored.events, stored.idempotent_replay)
    }

    fn commit_input(
        &mut self,
        context: &ProductSessionCommandContext,
        digest: Sha256Digest,
        catalog: PersistedChatInteractionCatalog,
        receipt: &InputRespondMutationReceipt,
    ) -> Result<InputRespondMutationReceipt, ChatInteractionServiceError> {
        let expected_revision = catalog.revision;
        let catalog = catalog.next_revision()?;
        let receipt_event = PersistedReceiptEvent::input(catalog.revision, receipt);
        let events = self.control_plane_events(
            context,
            &catalog,
            &receipt_event,
            &receipt.product_session.id,
            None,
        )?;
        let commit = StateCommit::new(
            context.receipt_identity.clone(),
            digest,
            catalog_stream_id(context.receipt_identity.scope_key()),
            expected_revision,
            encode_catalog(&catalog)?,
            events,
        );
        let stored = self.storage.commit(&commit).map_err(storage_error)?;
        decode_input_receipt(&stored.events, stored.idempotent_replay)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_approval(
        &mut self,
        context: &ProductSessionCommandContext,
        digest: Sha256Digest,
        catalog: PersistedChatInteractionCatalog,
        gate_mutation: winwincode_storage::StateMutation,
        receipt: &ApprovalDecideMutationReceipt,
        approval_event: ControlPlaneWebSocketApprovalChangedEvent,
    ) -> Result<ApprovalDecideMutationReceipt, ChatInteractionServiceError> {
        let expected_revision = catalog.revision;
        let catalog = catalog.next_revision()?;
        let receipt_event = PersistedReceiptEvent::approval(catalog.revision, receipt);
        let events = self.control_plane_events(
            context,
            &catalog,
            &receipt_event,
            &receipt.approval.binding.product_session_id,
            Some(approval_event),
        )?;
        let commit = StateCommit::new(
            context.receipt_identity.clone(),
            digest,
            catalog_stream_id(context.receipt_identity.scope_key()),
            expected_revision,
            encode_catalog(&catalog)?,
            events,
        )
        .with_state_mutation(gate_mutation);
        let stored = self.storage.commit(&commit).map_err(storage_error)?;
        decode_approval_receipt(&stored.events, stored.idempotent_replay)
    }

    fn worker_events(
        &self,
        context: &WorkerCommandContext,
        catalog: &PersistedChatInteractionCatalog,
        receipt: &PersistedReceiptEvent,
        approval: Option<ControlPlaneWebSocketApprovalChangedEvent>,
        source: PublicEventSource,
    ) -> Result<Vec<NewOutboxEvent>, ChatInteractionServiceError> {
        let product_session_id = receipt.product_session_id.clone();
        let mut events = vec![internal_receipt_event(&context.event_id, receipt)?];
        if receipt.write_status != Some(ChatInteractionWriteStatus::Duplicate) {
            events.push(public_event(
                context.event_id.clone(),
                CHAT_INTERACTION_INVALIDATED_TOPIC,
                invalidation(catalog, product_session_id.clone())?,
                product_session_id.clone(),
                context.public_scope.clone(),
                context.occurred_at.clone(),
                source.clone(),
                &self.output_gate,
            )?);
            if let Some(approval) = approval {
                events.push(public_event(
                    derived_event_id(&context.event_id, APPROVAL_CHANGED_TOPIC),
                    APPROVAL_CHANGED_TOPIC,
                    approval,
                    product_session_id,
                    context.public_scope.clone(),
                    context.occurred_at.clone(),
                    source,
                    &self.output_gate,
                )?);
            }
        }
        Ok(events)
    }

    fn control_plane_events(
        &self,
        context: &ProductSessionCommandContext,
        catalog: &PersistedChatInteractionCatalog,
        receipt: &PersistedReceiptEvent,
        product_session_id: &ProductSessionId,
        approval: Option<ControlPlaneWebSocketApprovalChangedEvent>,
    ) -> Result<Vec<NewOutboxEvent>, ChatInteractionServiceError> {
        let source = PublicEventSource::ControlPlane {
            actor: context.public_actor.clone(),
            component: "chat-interaction-service".to_owned(),
        };
        let mut events = vec![internal_receipt_event(&context.event_id, receipt)?];
        events.push(public_event(
            context.event_id.clone(),
            CHAT_INTERACTION_INVALIDATED_TOPIC,
            invalidation(catalog, product_session_id.clone())?,
            product_session_id.clone(),
            context.public_scope.clone(),
            context.occurred_at.clone(),
            source.clone(),
            &self.output_gate,
        )?);
        if let Some(approval) = approval {
            events.push(public_event(
                derived_event_id(&context.event_id, APPROVAL_CHANGED_TOPIC),
                APPROVAL_CHANGED_TOPIC,
                approval,
                product_session_id.clone(),
                context.public_scope.clone(),
                context.occurred_at.clone(),
                source,
                &self.output_gate,
            )?);
        }
        Ok(events)
    }
}

/// Generated HTTP adapter. Server composition injects only the durable ports
/// and trusted clock.
pub struct ChatInteractionApiService<'storage, 'clock, 'outbound> {
    service: ChatInteractionService<'storage>,
    clock: &'clock mut dyn ProductSessionApiClock,
    outbound: &'outbound mut dyn WorkerInteractionOutboundPort,
}

impl<'storage, 'clock, 'outbound> ChatInteractionApiService<'storage, 'clock, 'outbound> {
    #[must_use]
    pub fn new(
        storage: &'storage mut dyn ProductSessionPersistence,
        clock: &'clock mut dyn ProductSessionApiClock,
        outbound: &'outbound mut dyn WorkerInteractionOutboundPort,
    ) -> Self {
        Self {
            service: ChatInteractionService::new(storage),
            clock,
            outbound,
        }
    }

    /// Executes generated `input.respond`.
    ///
    /// # Errors
    ///
    /// Returns a validation, durable-state, or Worker-delivery failure.
    pub fn respond_input(
        &mut self,
        command: InputRespondCommand,
    ) -> Result<InputRespondCompletedResponse, ChatInteractionServiceError> {
        let event_id = api_event_id(
            "input.respond",
            &command.actor,
            &command.scope,
            &command.request_id,
        )?;
        let context = api_context(
            &command.actor,
            &command.scope,
            command.request_id.clone(),
            &command.expected_revision,
            event_id,
            self.clock.now(),
        )?;
        let receipt = self.service.respond_input(&context, &command)?;
        let response = receipt
            .worker_response
            .ok_or_else(|| corrupt("Input response Worker message is missing"))?;
        self.outbound
            .deliver(&ExecutionPortMessage::InputResponseMessage(response))
            .map_err(delivery_error)?;
        Ok(InputRespondCompletedResponse {
            command: InputRespondCompletedResponseCommand::InputRespond,
            current_revision: receipt.current_revision,
            outcome: InputRespondCompletedResponseOutcome::Completed,
            previous_revision: receipt.previous_revision,
            request_id: command.request_id,
            result: receipt.product_session,
            schema_version: command.schema_version,
        })
    }

    /// Executes generated `approval.decide`.
    ///
    /// # Errors
    ///
    /// Returns a validation, durable-state, or Worker-delivery failure.
    pub fn decide_approval(
        &mut self,
        command: ApprovalDecideCommand,
    ) -> Result<ApprovalDecideCompletedResponse, ChatInteractionServiceError> {
        let event_id = api_event_id(
            "approval.decide",
            &command.actor,
            &command.scope,
            &command.request_id,
        )?;
        let context = api_context(
            &command.actor,
            &command.scope,
            command.request_id.clone(),
            &command.expected_revision,
            event_id,
            self.clock.now(),
        )?;
        let receipt = self.service.decide_approval(&context, &command)?;
        let decision = receipt
            .worker_decision
            .ok_or_else(|| corrupt("Approval decision Worker message is missing"))?;
        self.outbound
            .deliver(&ExecutionPortMessage::ApprovalDecisionMessage(decision))
            .map_err(delivery_error)?;
        Ok(ApprovalDecideCompletedResponse {
            command: ApprovalDecideCompletedResponseCommand::ApprovalDecide,
            current_revision: receipt.current_revision,
            outcome: ApprovalDecideCompletedResponseOutcome::Completed,
            previous_revision: receipt.previous_revision,
            request_id: command.request_id,
            result: receipt.approval,
            schema_version: command.schema_version,
        })
    }

    /// Executes generated `session.interactions.list`.
    ///
    /// # Errors
    ///
    /// Returns a bounded query, storage, or durable-state failure.
    pub fn interactions(
        &mut self,
        query: &ChatInteractionListQuery,
    ) -> Result<ChatInteractionListResultResponse, ChatInteractionServiceError> {
        let scope = api_query_scope(&query.actor, &query.scope, query.request_id.clone())?;
        self.service.interactions(&scope, query, &self.clock.now())
    }

    /// Executes generated `approval.get`.
    ///
    /// # Errors
    ///
    /// Returns a bounded query, storage, or durable-state failure.
    pub fn approval_get(
        &mut self,
        query: &ApprovalGetQuery,
    ) -> Result<ApprovalGetResultResponse, ChatInteractionServiceError> {
        let scope = api_query_scope(&query.actor, &query.scope, query.request_id.clone())?;
        self.service.approval_get(&scope, query, &self.clock.now())
    }

    /// Executes generated `approval.list`.
    ///
    /// # Errors
    ///
    /// Returns a bounded query, storage, or durable-state failure.
    pub fn approval_list(
        &mut self,
        query: &ApprovalListQuery,
    ) -> Result<ApprovalListResultResponse, ChatInteractionServiceError> {
        let scope = api_query_scope(&query.actor, &query.scope, query.request_id.clone())?;
        self.service.approval_list(&scope, query, &self.clock.now())
    }
}

#[derive(Clone, Debug)]
struct WorkerCommandContext {
    receipt_identity: ReceiptIdentity,
    event_id: ControlPlaneEventId,
    occurred_at: Instant,
    public_scope: PublicEventScope,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedInteractionAuthority {
    execution_scope: ExecutionQueueScope,
    worker_pool_id: WorkerPoolId,
    product_session_revision: u64,
    job_revision: u64,
    worker_slot_revision: u64,
    runtime: WorkerSlotAuthority,
    lease: ExecutionLeaseStamp,
    session_identity: winwincode_domain::SessionIdentity,
    gate_authority: Option<GateInteractionAuthority>,
}

impl PersistedInteractionAuthority {
    fn public_source(&self) -> PublicEventSource {
        PublicEventSource::SessionExecutionWorker {
            worker_id: self.runtime.worker_id.clone(),
            worker_session_id: self.runtime.worker_session_id.clone(),
            lease_id: self.runtime.lease_id.clone(),
            codex_thread_id: self.runtime.codex_thread_id.clone(),
            session_identity: self.session_identity.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedChatInteractionCatalog {
    schema_version: u8,
    revision: u64,
    snapshot: ChatInteractionProjectionSnapshot,
    authorities: BTreeMap<String, PersistedInteractionAuthority>,
    approval_requested_by: BTreeMap<String, Actor>,
}

impl Default for PersistedChatInteractionCatalog {
    fn default() -> Self {
        Self {
            schema_version: CHAT_INTERACTION_SCHEMA_VERSION,
            revision: 0,
            snapshot: ChatInteractionProjectionSnapshot::default(),
            authorities: BTreeMap::new(),
            approval_requested_by: BTreeMap::new(),
        }
    }
}

impl PersistedChatInteractionCatalog {
    fn next_revision(mut self) -> Result<Self, ChatInteractionServiceError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| corrupt("Chat interaction catalog revision overflowed"))?;
        Ok(self)
    }

    fn validate(&self, stored_revision: u64) -> Result<(), ChatInteractionServiceError> {
        if self.schema_version != CHAT_INTERACTION_SCHEMA_VERSION
            || self.revision == 0
            || self.revision != stored_revision
        {
            return Err(corrupt(
                "Chat interaction catalog contract or revision is inconsistent",
            ));
        }
        ChatInteractionProjectionLedger::restore(self.snapshot.clone())
            .map_err(projection_error)?;
        let mut expected_authorities = BTreeSet::new();
        let mut expected_approval_actors = BTreeSet::new();
        for event in &self.snapshot.events {
            match event {
                ChatInteractionProjectionEvent::InputRecorded { projection, .. } => {
                    expected_authorities.insert(input_key(&projection.input_request_id.0));
                }
                ChatInteractionProjectionEvent::ApprovalRecorded { projection, .. } => {
                    expected_authorities.insert(approval_key(&projection.id.0));
                    expected_approval_actors.insert(projection.id.0.clone());
                }
                ChatInteractionProjectionEvent::InputResolved { .. }
                | ChatInteractionProjectionEvent::ApprovalResolved { .. } => {}
            }
        }
        if self.authorities.keys().cloned().collect::<BTreeSet<_>>() != expected_authorities
            || self
                .approval_requested_by
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
                != expected_approval_actors
        {
            return Err(corrupt("Chat interaction authority index is inconsistent"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MutationKind {
    InputRecorded,
    ApprovalRecorded,
    InputResponded,
    ApprovalDecided,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedReceiptEvent {
    schema_version: u8,
    kind: MutationKind,
    catalog_revision: u64,
    projection_revision: Revision,
    product_session_id: ProductSessionId,
    occurred_at: Instant,
    write_status: Option<ChatInteractionWriteStatus>,
    product_session: Option<ProductSessionProjection>,
    approval: Option<ApprovalProjection>,
}

impl PersistedReceiptEvent {
    fn worker(
        kind: MutationKind,
        catalog_revision: u64,
        occurred_at: &Instant,
        receipt: &ChatInteractionMutationReceipt,
    ) -> Self {
        Self {
            schema_version: CHAT_INTERACTION_SCHEMA_VERSION,
            kind,
            catalog_revision,
            projection_revision: receipt.revision.clone(),
            product_session_id: receipt.product_session_id.clone(),
            occurred_at: occurred_at.clone(),
            write_status: Some(receipt.status),
            product_session: None,
            approval: None,
        }
    }

    fn input(catalog_revision: u64, receipt: &InputRespondMutationReceipt) -> Self {
        Self {
            schema_version: CHAT_INTERACTION_SCHEMA_VERSION,
            kind: MutationKind::InputResponded,
            catalog_revision,
            projection_revision: receipt.current_revision.clone(),
            product_session_id: receipt.product_session.id.clone(),
            occurred_at: receipt.occurred_at.clone(),
            write_status: None,
            product_session: Some(receipt.product_session.clone()),
            approval: None,
        }
    }

    fn approval(catalog_revision: u64, receipt: &ApprovalDecideMutationReceipt) -> Self {
        Self {
            schema_version: CHAT_INTERACTION_SCHEMA_VERSION,
            kind: MutationKind::ApprovalDecided,
            catalog_revision,
            projection_revision: receipt.current_revision.clone(),
            product_session_id: receipt.approval.binding.product_session_id.clone(),
            occurred_at: receipt.occurred_at.clone(),
            write_status: None,
            product_session: None,
            approval: Some(receipt.approval.clone()),
        }
    }
}

fn worker_context(
    operation: &str,
    message_id: &ExecutionMessageId,
    request_id: &RequestId,
    occurred_at: &Instant,
    public_scope: PublicEventScope,
) -> Result<WorkerCommandContext, ChatInteractionServiceError> {
    let actor = PublicEventActor::System {
        id: SystemActorId(WORKER_RECEIPT_ACTOR_ID.to_owned()),
    };
    let receipt_identity = public_receipt_identity(&actor, &public_scope, request_id.clone())
        .map_err(storage_error)?;
    Ok(WorkerCommandContext {
        receipt_identity,
        event_id: worker_event_id(operation, message_id),
        occurred_at: occurred_at.clone(),
        public_scope,
    })
}

fn persisted_authority_from_input(
    command: &RecordInputInteractionCommand,
) -> Result<PersistedInteractionAuthority, ChatInteractionServiceError> {
    let request = &command.request;
    let runtime = runtime_from_request(
        &request.lease,
        &request.worker_session_id,
        &request.session_identity,
    )?;
    Ok(PersistedInteractionAuthority {
        execution_scope: command.authority.execution_scope.clone(),
        worker_pool_id: command.authority.worker_pool_id.clone(),
        product_session_revision: command.authority.product_session_revision,
        job_revision: command.authority.job_revision,
        worker_slot_revision: command.authority.worker_slot_revision,
        runtime,
        lease: request.lease.clone(),
        session_identity: request.session_identity.clone(),
        gate_authority: None,
    })
}

fn persisted_authority_from_approval(
    record: &GateInteractionRecord,
    request: &ApprovalRequestMessage,
) -> Result<PersistedInteractionAuthority, ChatInteractionServiceError> {
    if record.subject != GateInteractionSubject::Approval(request.approval_id.clone())
        || record.state != GateInteractionState::Pending
    {
        return Err(authority_mismatch(
            "Approval request does not match one pending Gate fact",
        ));
    }
    let runtime = runtime_from_request(
        &request.lease,
        &request.worker_session_id,
        &request.session_identity,
    )?;
    let gate = &record.authority;
    if runtime != gate.runtime
        || request.lease.expires_at != gate.lease_expires_at
        || request.session_identity.product_session_id != gate.execution_scope.product_session_id
        || request.session_identity.stage_run_id != gate.stage_run_id
    {
        return Err(authority_mismatch(
            "Approval request runtime differs from its Gate fact",
        ));
    }
    Ok(PersistedInteractionAuthority {
        execution_scope: gate.execution_scope.clone(),
        worker_pool_id: gate.worker_pool_id.clone(),
        product_session_revision: gate.product_session_revision,
        job_revision: gate.job_revision,
        worker_slot_revision: gate.worker_slot_revision,
        runtime,
        lease: request.lease.clone(),
        session_identity: request.session_identity.clone(),
        gate_authority: Some(gate.clone()),
    })
}

fn runtime_from_request(
    lease: &ExecutionLeaseStamp,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    session_identity: &winwincode_domain::SessionIdentity,
) -> Result<WorkerSlotAuthority, ChatInteractionServiceError> {
    let attempt = u64::try_from(lease.attempt)
        .map_err(|_| invalid("Worker lease attempt must be positive"))?;
    if attempt == 0
        || worker_session_id != &session_identity.worker_session_id
        || lease.expires_at.0 <= lease.issued_at.0
    {
        return Err(authority_mismatch(
            "Worker request carries an invalid runtime binding",
        ));
    }
    Ok(WorkerSlotAuthority {
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: worker_session_id.clone(),
        codex_thread_id: session_identity.codex_thread_id.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        attempt,
        fencing_token: lease.fencing_token.clone(),
    })
}

fn require_current_source(
    expected: &PersistedInteractionAuthority,
    slot: &WorkerSlotRecord,
    reservation: &ExecutionReservationRecord,
    lease: &ExecutionLeaseRecord,
) -> Result<(), ChatInteractionServiceError> {
    let stage_scope_matches = expected.session_identity.stage_run_id.is_some()
        == expected.execution_scope.delivery_id.is_some();
    if expected.session_identity.product_session_id != expected.execution_scope.product_session_id
        || expected.session_identity.worker_session_id != expected.runtime.worker_session_id
        || expected.session_identity.codex_thread_id != expected.runtime.codex_thread_id
        || !stage_scope_matches
        || slot.state != WorkerSlotState::Running
        || slot.authority != expected.runtime
        || slot.revision != expected.worker_slot_revision
        || reservation.state != ExecutionReservationState::Running
        || reservation.scope != expected.execution_scope
        || reservation.worker_pool_id != expected.worker_pool_id
        || reservation.job_id != expected.runtime.job_id
        || reservation.revision != expected.job_revision
        || lease.job_id != expected.lease.job_id
        || lease.lease_id != expected.lease.lease_id
        || lease.worker_id != expected.lease.worker_id
        || lease.worker_instance_id != expected.lease.worker_instance_id
        || i64::try_from(lease.attempt).ok() != Some(expected.lease.attempt)
        || lease.fencing_token != expected.lease.fencing_token
        || lease.issued_at != expected.lease.issued_at
        || lease.expires_at != expected.lease.expires_at
    {
        return Err(authority_mismatch(
            "Worker slot, lease, fence, or reservation is no longer current",
        ));
    }
    Ok(())
}

fn require_product_session(
    record: &crate::ProductSessionRecord,
    authority: &PersistedInteractionAuthority,
) -> Result<(), ChatInteractionServiceError> {
    let session = record.session();
    if session.revision() != authority.product_session_revision
        || session.id() != &authority.execution_scope.product_session_id
        || session.project_id() != &authority.execution_scope.project_id
        || session.repository_id() != &authority.execution_scope.repository_id
    {
        return Err(authority_mismatch(
            "ProductSession identity or revision is stale",
        ));
    }
    let stage_run_id = authority.session_identity.stage_run_id.as_ref();
    if !record.bindings().iter().any(|durable| {
        let binding = durable.binding();
        binding.execution_job_id() == &authority.runtime.job_id
            && binding.product_session_id() == &authority.execution_scope.product_session_id
            && binding.stage_run_id() == stage_run_id
            && binding.delivery_id() == authority.execution_scope.delivery_id.as_ref()
            && binding.worker_session_id() == Some(&authority.runtime.worker_session_id)
            && binding.codex_thread_id() == Some(&authority.runtime.codex_thread_id)
            && durable.slot().authority == authority.runtime
    }) {
        return Err(authority_mismatch(
            "ProductSession has no exact runtime binding",
        ));
    }
    Ok(())
}

fn input_response_message(
    command: &InputRespondCommand,
    authority: &PersistedInteractionAuthority,
    now: &Instant,
) -> Result<InputResponseMessage, ChatInteractionServiceError> {
    let status = match command.payload.status.as_str() {
        "provided" => InputResponseMessageStatus::Provided,
        "cancelled" => InputResponseMessageStatus::Cancelled,
        _ => return Err(invalid("Input response status is invalid")),
    };
    Ok(InputResponseMessage {
        input_request_id: command.payload.input_request_id.clone(),
        kind: InputResponseMessageKind::InputResponse,
        lease: authority.lease.clone(),
        message_id: response_message_id("input.response", &command.request_id),
        responded_at: now.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now.clone(),
        session_identity: command.payload.session_identity.clone(),
        status,
        value: command.payload.value.clone(),
        worker_session_id: command.payload.worker_session_id.clone(),
    })
}

fn approval_decision_message(
    command: &ApprovalDecideCommand,
    authority: &PersistedInteractionAuthority,
    now: &Instant,
) -> Result<ApprovalDecisionMessage, ChatInteractionServiceError> {
    let decision = match command.payload.decision.as_str() {
        "approve" => ApprovalDecisionMessageDecision::Approved,
        "reject" => ApprovalDecisionMessageDecision::Denied,
        _ => return Err(invalid("Approval decision is invalid")),
    };
    Ok(ApprovalDecisionMessage {
        approval_id: command.payload.approval_id.clone(),
        decided_at: now.clone(),
        decision,
        kind: ApprovalDecisionMessageKind::ApprovalDecision,
        lease: authority.lease.clone(),
        message_id: response_message_id("approval.decision", &command.request_id),
        reason: Some(command.payload.reason.clone()),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: ApprovalDecisionMessageScope::Once,
        sent_at: now.clone(),
        session_identity: command.payload.binding.session_identity.clone(),
        worker_session_id: command.payload.binding.worker_session_id.clone(),
    })
}

fn decode_worker_receipt(
    events: &[winwincode_storage::OutboxEvent],
    replayed: bool,
) -> Result<ChatInteractionMutationReceipt, ChatInteractionServiceError> {
    let event = decode_internal_receipt(events)?;
    let status = event
        .write_status
        .ok_or_else(|| corrupt("Worker interaction receipt has no write status"))?;
    if !matches!(
        event.kind,
        MutationKind::InputRecorded | MutationKind::ApprovalRecorded
    ) {
        return Err(corrupt("Worker interaction receipt kind is invalid"));
    }
    Ok(ChatInteractionMutationReceipt {
        status,
        revision: event.projection_revision,
        product_session_id: event.product_session_id,
        replayed,
    })
}

fn decode_input_receipt(
    events: &[winwincode_storage::OutboxEvent],
    replayed: bool,
) -> Result<InputRespondMutationReceipt, ChatInteractionServiceError> {
    let event = decode_internal_receipt(events)?;
    if event.kind != MutationKind::InputResponded {
        return Err(corrupt("Input response receipt kind is invalid"));
    }
    let previous_revision = previous_revision(&event.projection_revision)?;
    let product_session = event
        .product_session
        .ok_or_else(|| corrupt("Input response receipt has no ProductSession"))?;
    Ok(InputRespondMutationReceipt {
        current_revision: event.projection_revision,
        previous_revision,
        product_session,
        occurred_at: event.occurred_at,
        worker_response: None,
        replayed,
    })
}

fn decode_approval_receipt(
    events: &[winwincode_storage::OutboxEvent],
    replayed: bool,
) -> Result<ApprovalDecideMutationReceipt, ChatInteractionServiceError> {
    let event = decode_internal_receipt(events)?;
    if event.kind != MutationKind::ApprovalDecided {
        return Err(corrupt("Approval decision receipt kind is invalid"));
    }
    let previous_revision = previous_revision(&event.projection_revision)?;
    let approval = event
        .approval
        .ok_or_else(|| corrupt("Approval decision receipt has no projection"))?;
    Ok(ApprovalDecideMutationReceipt {
        current_revision: event.projection_revision,
        previous_revision,
        approval,
        occurred_at: event.occurred_at,
        worker_decision: None,
        replayed,
    })
}

fn previous_revision(current: &Revision) -> Result<Revision, ChatInteractionServiceError> {
    current
        .0
        .checked_sub(1)
        .map(Revision)
        .ok_or_else(|| corrupt("interaction receipt revision underflowed"))
}

fn decode_internal_receipt(
    events: &[winwincode_storage::OutboxEvent],
) -> Result<PersistedReceiptEvent, ChatInteractionServiceError> {
    let mut matching = events
        .iter()
        .filter(|event| event.topic == CHAT_INTERACTION_RECEIPT_TOPIC);
    let event = matching
        .next()
        .ok_or_else(|| corrupt("Chat interaction receipt event is missing"))?;
    if matching.next().is_some() {
        return Err(corrupt("Chat interaction receipt event is duplicated"));
    }
    let decoded: PersistedReceiptEvent = serde_json::from_slice(&event.payload)
        .map_err(|error| corrupt(format!("Chat interaction receipt is invalid: {error}")))?;
    if decoded.schema_version != CHAT_INTERACTION_SCHEMA_VERSION {
        return Err(corrupt("Chat interaction receipt schema is invalid"));
    }
    Ok(decoded)
}

fn internal_receipt_event(
    event_id: &ControlPlaneEventId,
    receipt: &PersistedReceiptEvent,
) -> Result<NewOutboxEvent, ChatInteractionServiceError> {
    let payload = serde_json::to_vec(receipt).map_err(|error| {
        corrupt(format!(
            "Chat interaction receipt cannot be encoded: {error}"
        ))
    })?;
    Ok(NewOutboxEvent::internal(
        format!("internal:chat-interaction:{}", event_id.0),
        CHAT_INTERACTION_RECEIPT_TOPIC,
        payload,
    ))
}

#[allow(clippy::too_many_arguments)]
fn public_event<T: Serialize>(
    event_id: ControlPlaneEventId,
    topic: &'static str,
    payload: T,
    product_session_id: ProductSessionId,
    scope: PublicEventScope,
    occurred_at: Instant,
    source: PublicEventSource,
    output_gate: &CredentialLeakGate,
) -> Result<NewOutboxEvent, ChatInteractionServiceError> {
    output_gate
        .inspect_serializable(CredentialOutputBoundary::WebSocket, &payload)
        .map_err(|leak| {
            error(
                ChatInteractionServiceErrorCode::CredentialLeak,
                format!("public interaction output was rejected: {leak}"),
            )
        })?;
    let payload = serde_json::to_vec(&payload).map_err(|error| {
        corrupt(format!(
            "Chat interaction public event cannot be encoded: {error}"
        ))
    })?;
    NewOutboxEvent::public_projection(
        event_id,
        topic,
        payload,
        ProjectionEventStream::ProductSession(product_session_id),
        scope,
        occurred_at,
        source,
    )
    .map_err(storage_error)
}

fn invalidation(
    catalog: &PersistedChatInteractionCatalog,
    product_session_id: ProductSessionId,
) -> Result<ControlPlaneWebSocketChatInteractionsInvalidatedEvent, ChatInteractionServiceError> {
    ChatInteractionProjectionLedger::restore(catalog.snapshot.clone())
        .map_err(projection_error)?
        .invalidation(product_session_id)
        .map_err(projection_error)
}

fn approval_changed(
    approval: &ApprovalProjection,
    requested_by: Actor,
    decided_by: Option<Actor>,
) -> ControlPlaneWebSocketApprovalChangedEvent {
    ControlPlaneWebSocketApprovalChangedEvent {
        approval_id: approval.id.clone(),
        decided_by,
        decision_reason: None,
        product_session_id: approval.binding.product_session_id.clone(),
        requested_by,
        state: approval.state.clone(),
        subject: approval.subject.clone(),
        type_value: ControlPlaneWebSocketApprovalChangedEventTypeValue::ApprovalChangedV1,
    }
}

fn actor_from_gate(actor: &GateInteractionActor) -> Actor {
    match actor {
        GateInteractionActor::User(id) => Actor::UserActor(UserActor {
            id: id.clone(),
            kind: UserActorKind::User,
        }),
        GateInteractionActor::ServiceAccount(id) => {
            Actor::ServiceAccountActor(ServiceAccountActor {
                id: id.clone(),
                kind: ServiceAccountActorKind::ServiceAccount,
            })
        }
        GateInteractionActor::System(id) => Actor::SystemActor(SystemActor {
            id: id.clone(),
            kind: SystemActorKind::System,
        }),
    }
}

fn gate_actor_from_api(actor: &Actor) -> GateInteractionActor {
    match actor {
        Actor::UserActor(value) => GateInteractionActor::User(value.id.clone()),
        Actor::ServiceAccountActor(value) => GateInteractionActor::ServiceAccount(value.id.clone()),
        Actor::SystemActor(value) => GateInteractionActor::System(value.id.clone()),
    }
}

const fn gate_state_name(state: GateInteractionState) -> &'static str {
    match state {
        GateInteractionState::Pending => "pending",
        GateInteractionState::Approved => "approved",
        GateInteractionState::Rejected => "rejected",
        GateInteractionState::Expired => "expired",
        GateInteractionState::AttentionResolved => "resolved",
    }
}

fn public_scope_from_execution(scope: &ExecutionQueueScope) -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn api_context(
    actor: &Actor,
    scope: &RepositoryScope,
    request_id: RequestId,
    expected_revision: &Revision,
    event_id: ControlPlaneEventId,
    occurred_at: Instant,
) -> Result<ProductSessionCommandContext, ChatInteractionServiceError> {
    product_session_command_context(
        actor,
        scope,
        request_id,
        expected_revision,
        event_id,
        occurred_at,
    )
    .map_err(product_session_error)
}

fn api_event_id(
    operation: &str,
    actor: &Actor,
    scope: &RepositoryScope,
    request_id: &RequestId,
) -> Result<ControlPlaneEventId, ChatInteractionServiceError> {
    deterministic_event_id(operation, actor, scope, request_id).map_err(product_session_error)
}

fn api_query_scope(
    actor: &Actor,
    scope: &RepositoryScope,
    request_id: RequestId,
) -> Result<ReceiptScopeKey, ChatInteractionServiceError> {
    query_scope(actor, scope, request_id).map_err(product_session_error)
}

fn command_digest<T: Serialize>(
    operation: &str,
    command: &T,
) -> Result<Sha256Digest, ChatInteractionServiceError> {
    let bytes = serde_json::to_vec(&(operation, command)).map_err(|error| {
        corrupt(format!(
            "Chat interaction command cannot be encoded: {error}"
        ))
    })?;
    Ok(sha256(&bytes))
}

fn encode_catalog(
    catalog: &PersistedChatInteractionCatalog,
) -> Result<Vec<u8>, ChatInteractionServiceError> {
    serde_json::to_vec(catalog)
        .map_err(|error| corrupt(format!("Chat interaction state cannot be encoded: {error}")))
}

/// Loads one complete, secret-safe Approval cut for collaboration projection.
///
/// # Errors
///
/// Rejects corrupt projection/authority joins or unavailable durable storage.
pub(crate) fn collaboration_approval_snapshot(
    storage: &dyn ProductSessionPersistence,
    scope: &ReceiptScopeKey,
    now: &Instant,
) -> Result<CollaborationApprovalSourceSnapshot, ChatInteractionServiceError> {
    let stream_id = catalog_stream_id(scope);
    let Some(state) = storage.load_state(&stream_id).map_err(storage_error)? else {
        return Ok(CollaborationApprovalSourceSnapshot {
            revision: 0,
            snapshot_sha256: sha256(b"[]"),
            state_guard: StateRevisionGuard::new(stream_id, 0).map_err(storage_error)?,
            approvals: Vec::new(),
        });
    };
    let catalog: PersistedChatInteractionCatalog =
        serde_json::from_slice(&state.payload).map_err(|error| {
            corrupt(format!(
                "Chat interaction catalog cannot be decoded: {error}"
            ))
        })?;
    catalog.validate(state.revision)?;
    let ledger = ChatInteractionProjectionLedger::restore(catalog.snapshot.clone())
        .map_err(projection_error)?;
    let approval_ids = catalog
        .snapshot
        .events
        .iter()
        .fold(BTreeMap::new(), |mut ids, event| {
            if let ChatInteractionProjectionEvent::ApprovalRecorded { projection, .. } = event {
                ids.insert(projection.id.0.clone(), projection.id.clone());
            }
            ids
        });
    let mut approvals = Vec::with_capacity(approval_ids.len());
    for approval_id in approval_ids.into_values() {
        let projection = ledger
            .approval(&approval_id, now)
            .map_err(projection_error)?
            .ok_or_else(|| corrupt("Approval projection disappeared from its durable catalog"))?;
        let authority = catalog
            .authorities
            .get(&approval_key(&approval_id.0))
            .ok_or_else(|| corrupt("Approval authority disappeared from its durable catalog"))?;
        let gate = authority.gate_authority.as_ref().ok_or_else(|| {
            corrupt("Approval source does not carry its canonical Gate authority")
        })?;
        if projection.binding.product_session_id != gate.execution_scope.product_session_id {
            return Err(corrupt(
                "Approval projection and Gate authority identify different ProductSessions",
            ));
        }
        approvals.push(CollaborationApprovalSourceRecord {
            projection,
            candidate: gate.gate.candidate.clone(),
            delivery_id: gate.execution_scope.delivery_id.clone(),
        });
    }
    approvals.sort_by(|left, right| left.projection.id.0.cmp(&right.projection.id.0));
    let encoded = serde_json::to_vec(&approvals)
        .map_err(|error| corrupt(format!("Approval source cut cannot be encoded: {error}")))?;
    Ok(CollaborationApprovalSourceSnapshot {
        revision: state.revision,
        snapshot_sha256: sha256(&encoded),
        state_guard: StateRevisionGuard::new(stream_id, state.revision).map_err(storage_error)?,
        approvals,
    })
}

fn catalog_stream_id(scope: &ReceiptScopeKey) -> String {
    format!("chat-interactions:{:x}", Sha256::digest(scope.as_bytes()))
}

fn input_key(id: &str) -> String {
    format!("input:{id}")
}

fn approval_key(id: &str) -> String {
    format!("approval:{id}")
}

fn worker_event_id(operation: &str, message_id: &ExecutionMessageId) -> ControlPlaneEventId {
    let digest = Sha256::digest(
        [
            b"winwincode.chat-interaction.worker-event.v1\0".as_slice(),
            operation.as_bytes(),
            b"\0",
            message_id.0.as_bytes(),
        ]
        .concat(),
    );
    ControlPlaneEventId(format!("evt_{digest:X}")[..30].to_owned())
}

fn derived_event_id(base: &ControlPlaneEventId, namespace: &str) -> ControlPlaneEventId {
    let digest = Sha256::digest(
        [
            b"winwincode.chat-interaction.public-event.v1\0".as_slice(),
            namespace.as_bytes(),
            b"\0",
            base.0.as_bytes(),
        ]
        .concat(),
    );
    ControlPlaneEventId(format!("evt_{digest:X}")[..30].to_owned())
}

fn response_message_id(operation: &str, request_id: &RequestId) -> ExecutionMessageId {
    let digest = Sha256::digest(
        [
            b"winwincode.chat-interaction.execution-message.v1\0".as_slice(),
            operation.as_bytes(),
            b"\0",
            request_id.0.as_bytes(),
        ]
        .concat(),
    );
    ExecutionMessageId(format!("xmsg_{digest:X}")[..31].to_owned())
}

const fn write_status(status: ProjectionWriteStatus) -> ChatInteractionWriteStatus {
    match status {
        ProjectionWriteStatus::Applied => ChatInteractionWriteStatus::Applied,
        ProjectionWriteStatus::Duplicate => ChatInteractionWriteStatus::Duplicate,
    }
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn error(
    code: ChatInteractionServiceErrorCode,
    message: impl Into<String>,
) -> ChatInteractionServiceError {
    ChatInteractionServiceError {
        code,
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> ChatInteractionServiceError {
    error(ChatInteractionServiceErrorCode::InvalidInput, message)
}

fn not_found(message: impl Into<String>) -> ChatInteractionServiceError {
    error(ChatInteractionServiceErrorCode::NotFound, message)
}

fn authority_mismatch(message: impl Into<String>) -> ChatInteractionServiceError {
    error(ChatInteractionServiceErrorCode::AuthorityMismatch, message)
}

fn corrupt(message: impl Into<String>) -> ChatInteractionServiceError {
    error(ChatInteractionServiceErrorCode::CorruptState, message)
}

#[allow(clippy::needless_pass_by_value)]
fn storage_error(failure: StorageError) -> ChatInteractionServiceError {
    let code = match failure.kind() {
        StorageErrorKind::RequestConflict => ChatInteractionServiceErrorCode::RequestConflict,
        StorageErrorKind::RevisionConflict => ChatInteractionServiceErrorCode::RevisionConflict,
        _ => ChatInteractionServiceErrorCode::Storage,
    };
    error(code, format!("Chat interaction storage failed: {failure}"))
}

#[allow(clippy::needless_pass_by_value)]
fn delivery_error(failure: WorkerInteractionDeliveryError) -> ChatInteractionServiceError {
    let code = match failure.kind() {
        WorkerInteractionDeliveryErrorKind::Unavailable => {
            ChatInteractionServiceErrorCode::WorkerDelivery
        }
        WorkerInteractionDeliveryErrorKind::Rejected => {
            ChatInteractionServiceErrorCode::AuthorityMismatch
        }
    };
    error(
        code,
        format!("Worker interaction delivery failed: {failure}"),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn product_session_error(failure: ProductSessionServiceError) -> ChatInteractionServiceError {
    let code = match failure.code() {
        ProductSessionServiceErrorCode::NotFound => ChatInteractionServiceErrorCode::NotFound,
        ProductSessionServiceErrorCode::RequestConflict => {
            ChatInteractionServiceErrorCode::RequestConflict
        }
        ProductSessionServiceErrorCode::RevisionConflict => {
            ChatInteractionServiceErrorCode::RevisionConflict
        }
        ProductSessionServiceErrorCode::BindingIdentityMismatch
        | ProductSessionServiceErrorCode::ActorMismatch => {
            ChatInteractionServiceErrorCode::AuthorityMismatch
        }
        ProductSessionServiceErrorCode::CredentialLeak => {
            ChatInteractionServiceErrorCode::CredentialLeak
        }
        ProductSessionServiceErrorCode::CorruptState => {
            ChatInteractionServiceErrorCode::CorruptState
        }
        _ => ChatInteractionServiceErrorCode::InvalidInput,
    };
    error(code, failure.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn gate_error(failure: GateInteractionServiceError) -> ChatInteractionServiceError {
    let code = match failure.code() {
        GateInteractionServiceErrorCode::NotFound => ChatInteractionServiceErrorCode::NotFound,
        GateInteractionServiceErrorCode::RequestConflict => {
            ChatInteractionServiceErrorCode::RequestConflict
        }
        GateInteractionServiceErrorCode::RevisionConflict => {
            ChatInteractionServiceErrorCode::RevisionConflict
        }
        GateInteractionServiceErrorCode::AuthorityMismatch
        | GateInteractionServiceErrorCode::SubjectMismatch => {
            ChatInteractionServiceErrorCode::AuthorityMismatch
        }
        GateInteractionServiceErrorCode::ActorMismatch => {
            ChatInteractionServiceErrorCode::ActorMismatch
        }
        GateInteractionServiceErrorCode::Expired => ChatInteractionServiceErrorCode::Expired,
        GateInteractionServiceErrorCode::AlreadyResolved => {
            ChatInteractionServiceErrorCode::WrongState
        }
        GateInteractionServiceErrorCode::CorruptState => {
            ChatInteractionServiceErrorCode::CorruptState
        }
        GateInteractionServiceErrorCode::Storage => ChatInteractionServiceErrorCode::Storage,
        GateInteractionServiceErrorCode::InvalidInput
        | GateInteractionServiceErrorCode::DecisionNotRoutable => {
            ChatInteractionServiceErrorCode::InvalidInput
        }
    };
    error(code, failure.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn projection_error(failure: ChatInteractionProjectionError) -> ChatInteractionServiceError {
    let code = match &failure {
        ChatInteractionProjectionError::UnknownInteraction => {
            ChatInteractionServiceErrorCode::NotFound
        }
        ChatInteractionProjectionError::RevisionConflict { .. } => {
            ChatInteractionServiceErrorCode::RevisionConflict
        }
        ChatInteractionProjectionError::BindingMismatch(_) => {
            ChatInteractionServiceErrorCode::AuthorityMismatch
        }
        ChatInteractionProjectionError::Expired => ChatInteractionServiceErrorCode::Expired,
        ChatInteractionProjectionError::StateConflict => {
            ChatInteractionServiceErrorCode::WrongState
        }
        ChatInteractionProjectionError::SnapshotConflict => {
            ChatInteractionServiceErrorCode::CorruptState
        }
        _ => ChatInteractionServiceErrorCode::InvalidInput,
    };
    error(code, failure.to_string())
}
