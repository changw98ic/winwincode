// SPDX-License-Identifier: Apache-2.0

//! Chat ledger, exact cancellation routing, and public reads for the canonical
//! [`super::ProductSessionService`].

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    ChatMessagePage, ChatMessagePageKind, ChatSubmitCommand, PageInfo, PageRequest,
    ProductSessionPage, ProductSessionPageKind, SessionCancelCommand,
};
use winwincode_domain::{OpaqueCursor, RequestId};
use winwincode_execution_port::generated::{ExecutionOutcomeStatus, ExecutionOutcomeUsage};

use super::{
    AuthenticatedActor, ChatMessageId, ChatMessageProjection, ExecutionCancellationRoutes,
    ExecutionJobId, ExecutionJobState, ExecutionQueueScope, ExecutionReservationRecord,
    ExecutionReservationState, ExecutionRoute, Instant, InteractionRouter, InteractionRoutingError,
    ModelExchangeId, ModelStreamCancellationRoute, MutationKind,
    PRODUCT_SESSION_SERVICE_SCHEMA_VERSION, PersistedBindingIdentity, PersistedProductSession,
    PersistedSessionBinding, ProductSessionCommandContext, ProductSessionExecutionConfig,
    ProductSessionId, ProductSessionMutationReceipt, ProductSessionRecord, ProductSessionService,
    ProductSessionServiceError, ProductSessionServiceErrorCode, ProductSessionState,
    PublicEventActor, PublicEventScope, ReceiptScopeKey, RouteWriteStatus, RuntimeRouteAuthority,
    SessionBindingIdentity, SessionCancellationRequest, SessionCancellationSnapshot,
    SessionModelSelection, StageRunId, WorkerCancellationRoute, WorkerPoolId, WorkerSessionId,
    WorkerSlotAuthority, WorkerSlotRecord, WorkerSlotState, binding_mismatch, canonical_id,
    command_digest, context_digest_fields, corrupt, domain_error, inspect_public_output, not_found,
    product_session_command_context, require_revision, service_error, session_state_label,
    storage_error,
};

const MAX_MESSAGE_BYTES: usize = 1_048_576;
const MAX_MESSAGES_PER_SESSION: usize = 10_000;
const MAX_PAGE_SIZE: usize = 200;
const MAX_CURSOR_BYTES: usize = 2_048;

/// One accepted user message and the schedulable turn it created.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmitChatMessageCommand {
    pub context: ProductSessionCommandContext,
    pub product_session_id: ProductSessionId,
    pub message: String,
    pub execution_config: ProductSessionExecutionConfig,
}

impl SubmitChatMessageCommand {
    /// Converts one generated `chat.submit` command into the canonical service command.
    ///
    /// # Errors
    ///
    /// Rejects invalid command-envelope authority or revision facts.
    pub fn from_api(
        command: ChatSubmitCommand,
        event_id: winwincode_domain::ControlPlaneEventId,
        occurred_at: Instant,
        execution_config: &ProductSessionExecutionConfig,
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
            message: command.payload.message,
            execution_config: execution_config.clone(),
        })
    }
}

/// Public terminal state accepted from the already gated model-stream adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssistantMessageState {
    Streaming,
    Completed,
    Cancelled,
    Failed,
}

impl AssistantMessageState {
    const fn label(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    const fn terminal(self) -> bool {
        !matches!(self, Self::Streaming)
    }
}

/// One ordered public assistant-text update bound to an exact live runtime.
/// Provider responses, reasoning, and tool payloads are intentionally absent.
#[derive(Clone, Debug, PartialEq)]
pub struct AppendAssistantMessageCommand {
    pub context: ProductSessionCommandContext,
    pub product_session_id: ProductSessionId,
    pub binding_identity: SessionBindingIdentity,
    pub runtime_authority: WorkerSlotAuthority,
    pub execution_scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub model_exchange_id: ModelExchangeId,
    pub stream_sequence: u64,
    pub public_text_delta: String,
    pub state: AssistantMessageState,
    pub terminal_outcome: Option<ProductSessionTurnTerminalOutcome>,
}

/// Immutable terminal Worker facts for one exact bound Chat turn.
/// Public assistant text is deliberately absent.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordAssistantTerminalCommand {
    pub context: ProductSessionCommandContext,
    pub product_session_id: ProductSessionId,
    pub binding_identity: SessionBindingIdentity,
    pub runtime_authority: WorkerSlotAuthority,
    pub execution_scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub model_exchange_id: ModelExchangeId,
    pub terminal_outcome: ProductSessionTurnTerminalOutcome,
}

/// Exact authenticated cancellation of one current session revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelProductSessionCommand {
    pub context: ProductSessionCommandContext,
    pub product_session_id: ProductSessionId,
    pub actor: AuthenticatedActor,
    pub reason: String,
}

impl CancelProductSessionCommand {
    /// Converts one generated `session.cancel` command into the canonical service command.
    ///
    /// # Errors
    ///
    /// Rejects invalid command-envelope authority or revision facts.
    pub fn from_api(
        command: SessionCancelCommand,
        event_id: winwincode_domain::ControlPlaneEventId,
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
        let actor = actor_from_public(&context.public_actor);
        Ok(Self {
            context,
            product_session_id: command.payload.product_session_id,
            actor,
            reason: command.payload.reason,
        })
    }
}

/// Turn state retained beside the public user message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductSessionTurnState {
    Pending,
    Bound,
    Completed,
    Cancelled,
    Failed,
}

/// Immutable Worker terminal facts retained beside the one public assistant
/// message. The Worker summary is intentionally absent.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductSessionTurnTerminalOutcome {
    pub status: ExecutionOutcomeStatus,
    pub usage: Option<ExecutionOutcomeUsage>,
    pub last_event_sequence: winwincode_domain::ExecutionAckSequence,
    pub finished_at: Instant,
}

/// Secret-safe scheduler intent produced atomically by `chat.submit`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSessionTurnIntent {
    pub request_id: RequestId,
    pub product_session_id: ProductSessionId,
    pub user_message_id: ChatMessageId,
    pub execution_job_id: ExecutionJobId,
    pub session_revision: u64,
    pub model_selection: SessionModelSelection,
    pub model_exchange_id: Option<ModelExchangeId>,
    pub requested_at: Instant,
    pub state: ProductSessionTurnState,
    pub terminal_outcome: Option<ProductSessionTurnTerminalOutcome>,
}

/// Replay-safe `chat.submit` result.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatSubmitMutationReceipt {
    pub mutation: ProductSessionMutationReceipt,
    pub message: ChatMessageProjection,
    pub turn_intent: ProductSessionTurnIntent,
}

/// Replay-safe public assistant update result.
#[derive(Clone, Debug, PartialEq)]
pub struct AssistantMessageMutationReceipt {
    pub mutation: ProductSessionMutationReceipt,
    pub message: ChatMessageProjection,
}

/// Replay-safe session cancellation with exact queue, Worker, and model routes.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSessionCancellationReceipt {
    pub mutation: ProductSessionMutationReceipt,
    pub routing: winwincode_session::SessionCancellationReceipt,
}

/// Stable public read page request. Limits match the generated HTTP contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductSessionPageRequest {
    pub cursor: Option<OpaqueCursor>,
    pub limit: u16,
}

/// One stable page of sessions in ProductSession-id order.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSessionPageRead {
    pub items: Vec<ProductSessionRecord>,
    pub next_cursor: Option<OpaqueCursor>,
}

impl ProductSessionPageRead {
    #[must_use]
    pub const fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }

    /// Converts the service page into the generated public result and page metadata.
    ///
    /// # Errors
    ///
    /// Rejects a durable session revision outside the generated public range.
    pub fn into_api(self) -> Result<(ProductSessionPage, PageInfo), ProductSessionServiceError> {
        let has_more = self.has_more();
        let items = self
            .items
            .into_iter()
            .map(|record| record.projection())
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            ProductSessionPage {
                items,
                kind: ProductSessionPageKind::ProductSessionPage,
            },
            PageInfo {
                has_more,
                next_cursor: self.next_cursor,
            },
        ))
    }
}

/// One stable page of public messages in sequence order.
#[derive(Clone, Debug, PartialEq)]
pub struct ProductSessionMessagePage {
    pub items: Vec<ChatMessageProjection>,
    pub next_cursor: Option<OpaqueCursor>,
}

impl ProductSessionMessagePage {
    #[must_use]
    pub const fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }

    /// Converts the public-only ledger page into the generated query result.
    #[must_use]
    pub fn into_api(self) -> (ChatMessagePage, PageInfo) {
        let has_more = self.has_more();
        (
            ChatMessagePage {
                items: self.items,
                kind: ChatMessagePageKind::ChatMessagePage,
            },
            PageInfo {
                has_more,
                next_cursor: self.next_cursor,
            },
        )
    }
}

impl TryFrom<PageRequest> for ProductSessionPageRequest {
    type Error = ProductSessionServiceError;

    fn try_from(page: PageRequest) -> Result<Self, Self::Error> {
        let request = Self {
            cursor: page.cursor,
            limit: u16::try_from(page.limit).map_err(|_| {
                service_error(
                    ProductSessionServiceErrorCode::InvalidInput,
                    "ProductSession page limit must be between 1 and 200",
                )
            })?,
        };
        validate_page_request(&request)?;
        Ok(request)
    }
}

/// Parses the generated `session.list` state labels into the domain lifecycle enum.
///
/// # Errors
///
/// Rejects unknown labels even if a malformed request bypassed schema validation.
pub fn product_session_state_filters(
    states: &[String],
) -> Result<Vec<ProductSessionState>, ProductSessionServiceError> {
    states
        .iter()
        .map(|state| match state.as_str() {
            "idle" => Ok(ProductSessionState::Idle),
            "running" => Ok(ProductSessionState::Running),
            "waiting_for_input" => Ok(ProductSessionState::WaitingForInput),
            "waiting_for_approval" => Ok(ProductSessionState::WaitingForApproval),
            "cancelled" => Ok(ProductSessionState::Cancelled),
            "closed" => Ok(ProductSessionState::Closed),
            "failed" => Ok(ProductSessionState::Failed),
            _ => Err(service_error(
                ProductSessionServiceErrorCode::InvalidInput,
                "session.list contains an unknown ProductSession state",
            )),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PersistedTurnState {
    Pending,
    Bound,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PersistedTurnIntent {
    request_id: RequestId,
    product_session_id: ProductSessionId,
    user_message_id: ChatMessageId,
    pub(super) execution_job_id: ExecutionJobId,
    session_revision: u64,
    model_selection: SessionModelSelection,
    pub(super) model_exchange_id: Option<ModelExchangeId>,
    requested_at: Instant,
    pub(super) state: PersistedTurnState,
    pub(super) terminal_outcome: Option<ProductSessionTurnTerminalOutcome>,
}

impl PersistedTurnIntent {
    pub(super) fn to_domain(&self) -> ProductSessionTurnIntent {
        ProductSessionTurnIntent {
            request_id: self.request_id.clone(),
            product_session_id: self.product_session_id.clone(),
            user_message_id: self.user_message_id.clone(),
            execution_job_id: self.execution_job_id.clone(),
            session_revision: self.session_revision,
            model_selection: self.model_selection.clone(),
            model_exchange_id: self.model_exchange_id.clone(),
            requested_at: self.requested_at.clone(),
            state: match self.state {
                PersistedTurnState::Pending => ProductSessionTurnState::Pending,
                PersistedTurnState::Bound => ProductSessionTurnState::Bound,
                PersistedTurnState::Completed => ProductSessionTurnState::Completed,
                PersistedTurnState::Cancelled => ProductSessionTurnState::Cancelled,
                PersistedTurnState::Failed => ProductSessionTurnState::Failed,
            },
            terminal_outcome: self.terminal_outcome.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PersistedChatMessage {
    pub(super) projection: ChatMessageProjection,
    model_exchange_id: Option<ModelExchangeId>,
    last_stream_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PersistedCancellation {
    pub(super) request_id: RequestId,
    pub(super) routes: Vec<PersistedExecutionCancellationRoutes>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedRuntimeRouteAuthority {
    lease_id: winwincode_domain::LeaseId,
    worker_id: winwincode_domain::WorkerId,
    worker_instance_id: winwincode_domain::WorkerInstanceId,
    worker_session_id: WorkerSessionId,
    attempt: u64,
    fencing_token: winwincode_domain::FencingToken,
}

impl PersistedRuntimeRouteAuthority {
    fn from_domain(runtime: &RuntimeRouteAuthority) -> Self {
        Self {
            lease_id: runtime.lease_id.clone(),
            worker_id: runtime.worker_id.clone(),
            worker_instance_id: runtime.worker_instance_id.clone(),
            worker_session_id: runtime.worker_session_id.clone(),
            attempt: runtime.attempt,
            fencing_token: runtime.fencing_token.clone(),
        }
    }

    fn to_domain(&self) -> RuntimeRouteAuthority {
        RuntimeRouteAuthority {
            lease_id: self.lease_id.clone(),
            worker_id: self.worker_id.clone(),
            worker_instance_id: self.worker_instance_id.clone(),
            worker_session_id: self.worker_session_id.clone(),
            attempt: self.attempt,
            fencing_token: self.fencing_token.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PersistedExecutionCancellationRoutes {
    product_session_id: ProductSessionId,
    stage_run_id: Option<StageRunId>,
    execution_job_id: ExecutionJobId,
    job_revision: u64,
    worker_authority: Option<PersistedRuntimeRouteAuthority>,
    worker_slot_revision: Option<u64>,
    model_exchange_id: Option<ModelExchangeId>,
}

impl PersistedExecutionCancellationRoutes {
    fn from_domain(routes: &ExecutionCancellationRoutes) -> Self {
        Self {
            product_session_id: routes.job.product_session_id.clone(),
            stage_run_id: routes.job.stage_run_id.clone(),
            execution_job_id: routes.job.execution_job_id.clone(),
            job_revision: routes.job.expected_revision,
            worker_authority: routes
                .worker
                .as_ref()
                .map(|worker| PersistedRuntimeRouteAuthority::from_domain(&worker.runtime)),
            worker_slot_revision: routes
                .worker
                .as_ref()
                .map(|worker| worker.expected_revision),
            model_exchange_id: routes
                .model_stream
                .as_ref()
                .map(|model| model.model_exchange_id.clone()),
        }
    }

    pub(super) fn to_domain(
        &self,
    ) -> Result<ExecutionCancellationRoutes, ProductSessionServiceError> {
        if self.job_revision == 0
            || matches!(
                (&self.worker_authority, self.worker_slot_revision),
                (Some(_), None) | (None, Some(_))
            )
            || self
                .worker_slot_revision
                .is_some_and(|revision| revision == 0)
            || (self.model_exchange_id.is_some() && self.worker_authority.is_none())
        {
            return Err(corrupt(
                "persisted ProductSession cancellation route is incomplete",
            ));
        }
        let worker = self
            .worker_authority
            .as_ref()
            .zip(self.worker_slot_revision)
            .map(|(authority, expected_revision)| WorkerCancellationRoute {
                product_session_id: self.product_session_id.clone(),
                stage_run_id: self.stage_run_id.clone(),
                execution_job_id: self.execution_job_id.clone(),
                runtime: authority.to_domain(),
                expected_revision,
            });
        let model_stream = self
            .model_exchange_id
            .as_ref()
            .and_then(|model_exchange_id| {
                self.worker_authority
                    .as_ref()
                    .map(|authority| ModelStreamCancellationRoute {
                        product_session_id: self.product_session_id.clone(),
                        stage_run_id: self.stage_run_id.clone(),
                        execution_job_id: self.execution_job_id.clone(),
                        runtime: authority.to_domain(),
                        model_exchange_id: model_exchange_id.clone(),
                    })
            });
        Ok(ExecutionCancellationRoutes {
            job: winwincode_session::JobCancellationRoute {
                product_session_id: self.product_session_id.clone(),
                stage_run_id: self.stage_run_id.clone(),
                execution_job_id: self.execution_job_id.clone(),
                expected_revision: self.job_revision,
            },
            worker,
            model_stream,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageCursor {
    schema_version: u8,
    kind: PageCursorKind,
    scope_sha256: String,
    catalog_revision: u64,
    filter_sha256: String,
    after: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PageCursorKind {
    Sessions,
    Messages,
}

impl ProductSessionService<'_> {
    /// Appends one public user message and creates exactly one schedulable turn.
    ///
    /// # Errors
    ///
    /// Rejects stale revisions, unsafe/unbounded text, invalid lifecycle state,
    /// changed-body replays, and storage failures.
    pub fn submit_chat(
        &mut self,
        command: &SubmitChatMessageCommand,
    ) -> Result<ChatSubmitMutationReceipt, ProductSessionServiceError> {
        let digest = command_digest("chat_submit", submit_digest_fields(command))?;
        if let Some(replay) = self.replay(&command.context, &digest, MutationKind::ChatSubmitted)? {
            let receipt = chat_submit_receipt(replay)?;
            self.require_chat_execution_replay(&command.context, &receipt.turn_intent)?;
            return Ok(receipt);
        }
        validate_message_text(&command.message, false)?;
        inspect_public_output(&self.output_gate, &command.message)?;
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        let persisted = catalog
            .sessions
            .get_mut(&command.product_session_id.0)
            .ok_or_else(not_found)?;
        require_revision(&persisted.session, command.context.expected_revision)?;
        let execution = command.execution_config.prepare(
            &command.context,
            &command.product_session_id,
            &command.message,
            &persisted.model_selection,
        )?;
        if persisted.messages.len() >= MAX_MESSAGES_PER_SESSION {
            return Err(service_error(
                ProductSessionServiceErrorCode::MessageLimitExceeded,
                "ProductSession message limit was reached",
            ));
        }
        match persisted.session.state() {
            ProductSessionState::Idle | ProductSessionState::Failed => persisted
                .session
                .begin_turn(command.context.occurred_at.clone()),
            ProductSessionState::WaitingForInput => persisted
                .session
                .resume(command.context.occurred_at.clone()),
            ProductSessionState::Running
            | ProductSessionState::WaitingForApproval
            | ProductSessionState::Cancelled
            | ProductSessionState::Closed => {
                return Err(service_error(
                    ProductSessionServiceErrorCode::InvalidState,
                    "ProductSession cannot accept another Chat turn in its current state",
                ));
            }
        }
        .map_err(|error| domain_error(&error))?;
        let message_id = deterministic_message_id(
            "user",
            command.context.receipt_identity.request_id().0.as_bytes(),
        );
        let message = ChatMessageProjection {
            content: command.message.clone(),
            created_at: command.context.occurred_at.clone(),
            id: message_id.clone(),
            product_session_id: command.product_session_id.clone(),
            role: "user".to_owned(),
            sequence: next_message_sequence(&persisted.messages)?,
            state: "completed".to_owned(),
            updated_at: command.context.occurred_at.clone(),
        };
        persisted.messages.push(PersistedChatMessage {
            projection: message,
            model_exchange_id: None,
            last_stream_sequence: 0,
        });
        persisted.turn_intents.push(PersistedTurnIntent {
            request_id: command.context.receipt_identity.request_id().clone(),
            product_session_id: command.product_session_id.clone(),
            user_message_id: message_id,
            execution_job_id: execution.job.job_id.clone(),
            session_revision: persisted.session.revision(),
            model_selection: persisted.model_selection.clone(),
            model_exchange_id: None,
            requested_at: command.context.occurred_at.clone(),
            state: PersistedTurnState::Pending,
            terminal_outcome: None,
        });
        let persisted = persisted.clone();
        let mutation = self.commit(
            &command.context,
            digest,
            MutationKind::ChatSubmitted,
            catalog,
            &command.product_session_id,
            persisted,
            Some(&execution),
        )?;
        chat_submit_receipt(mutation)
    }

    fn require_chat_execution_replay(
        &mut self,
        context: &ProductSessionCommandContext,
        turn: &ProductSessionTurnIntent,
    ) -> Result<(), ProductSessionServiceError> {
        let repository =
            crate::repository_scope_from_receipt_key(context.receipt_identity.scope_key())
                .map_err(|error| storage_error(&error))?;
        let scope = ExecutionQueueScope {
            organization_id: repository.organization_id,
            workspace_id: repository.workspace_id,
            project_id: repository.project_id,
            repository_id: repository.repository_id,
            product_session_id: turn.product_session_id.clone(),
            delivery_id: None,
        };
        let job = self
            .storage
            .load_product_session_execution_job(&scope, &turn.execution_job_id, &turn.request_id)
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| corrupt("chat.submit receipt lost its atomic ExecutionJob"))?;
        let decoded: winwincode_execution_port::generated::ExecutionJob =
            serde_json::from_slice(&job.dispatch_payload)
                .map_err(|_| corrupt("chat.submit ExecutionJob cannot be decoded"))?;
        let exact_scope = matches!(
            &decoded.scope,
            winwincode_execution_port::generated::ExecutionScope::ProductSessionExecutionScope(
                job_scope
            ) if job_scope.product_session_id == turn.product_session_id
        );
        if decoded.job_id != turn.execution_job_id
            || decoded.job_id != job.job_id
            || decoded.payload_digest != job.payload_digest
            || decoded.workspace.repository_id != scope.repository_id
            || !exact_scope
        {
            return Err(corrupt(
                "chat.submit receipt and canonical ExecutionJob are inconsistent",
            ));
        }
        Ok(())
    }

    /// Appends one ordered, already public assistant-text delta.
    ///
    /// # Errors
    ///
    /// Rejects stale Worker/lease/fence/model identities before writing, gaps
    /// or duplicate stream sequences, unsafe text, and invalid terminal state.
    #[allow(clippy::too_many_lines)]
    pub fn append_assistant_message(
        &mut self,
        command: &AppendAssistantMessageCommand,
    ) -> Result<AssistantMessageMutationReceipt, ProductSessionServiceError> {
        let digest = command_digest("assistant_update", assistant_digest_fields(command))?;
        if let Some(replay) =
            self.replay(&command.context, &digest, MutationKind::AssistantUpdated)?
        {
            return assistant_receipt(replay, &command.model_exchange_id);
        }
        validate_assistant_command(command)?;
        inspect_public_output(&self.output_gate, &command.public_text_delta)?;
        let current = self
            .storage
            .load_worker_binding_source(
                &command.execution_scope,
                &command.worker_pool_id,
                &command.runtime_authority.worker_session_id,
            )
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| binding_mismatch("assistant update has no current Worker authority"))?;
        validate_runtime_authority(
            &command.runtime_authority,
            &command.execution_scope,
            &command.worker_pool_id,
            &current.0,
            &current.1,
        )?;
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        let persisted = catalog
            .sessions
            .get_mut(&command.product_session_id.0)
            .ok_or_else(not_found)?;
        require_revision(&persisted.session, command.context.expected_revision)?;
        if persisted.session.state() != ProductSessionState::Running {
            return Err(service_error(
                ProductSessionServiceErrorCode::InvalidState,
                "assistant update requires a running ProductSession",
            ));
        }
        require_persisted_binding(persisted, command, &current.0, &current.1)?;
        let message_id =
            deterministic_message_id("assistant", command.model_exchange_id.0.as_bytes());
        let message_index = persisted
            .messages
            .iter()
            .position(|message| message.projection.id == message_id);
        match message_index {
            None => {
                if command.stream_sequence != 1
                    || persisted.messages.len() >= MAX_MESSAGES_PER_SESSION
                {
                    return Err(stream_conflict());
                }
                persisted.messages.push(PersistedChatMessage {
                    projection: ChatMessageProjection {
                        content: command.public_text_delta.clone(),
                        created_at: command.context.occurred_at.clone(),
                        id: message_id,
                        product_session_id: command.product_session_id.clone(),
                        role: "assistant".to_owned(),
                        sequence: next_message_sequence(&persisted.messages)?,
                        state: command.state.label().to_owned(),
                        updated_at: command.context.occurred_at.clone(),
                    },
                    model_exchange_id: Some(command.model_exchange_id.clone()),
                    last_stream_sequence: 1,
                });
            }
            Some(index) => {
                let message = &mut persisted.messages[index];
                if message.model_exchange_id.as_ref() != Some(&command.model_exchange_id)
                    || message.projection.state != "streaming"
                    || command.stream_sequence
                        != message
                            .last_stream_sequence
                            .checked_add(1)
                            .ok_or_else(stream_conflict)?
                {
                    return Err(stream_conflict());
                }
                let next_len = message
                    .projection
                    .content
                    .len()
                    .checked_add(command.public_text_delta.len())
                    .ok_or_else(message_limit)?;
                if next_len > MAX_MESSAGE_BYTES {
                    return Err(message_limit());
                }
                message
                    .projection
                    .content
                    .push_str(&command.public_text_delta);
                inspect_public_output(&self.output_gate, &message.projection.content)?;
                command
                    .state
                    .label()
                    .clone_into(&mut message.projection.state);
                message.projection.updated_at = command.context.occurred_at.clone();
                message.last_stream_sequence = command.stream_sequence;
            }
        }
        if command.state.terminal() {
            finish_turn(persisted, command)?;
        }
        let persisted = persisted.clone();
        let mutation = self.commit(
            &command.context,
            digest,
            MutationKind::AssistantUpdated,
            catalog,
            &command.product_session_id,
            persisted,
            None,
        )?;
        assistant_receipt(mutation, &command.model_exchange_id)
    }

    /// Records immutable Worker terminal facts beside the one public assistant
    /// message without treating the Worker summary as public text.
    ///
    /// # Errors
    ///
    /// Rejects stale runtime authority, changed replay, invalid Usage, or an
    /// outcome that differs from the exact bound Chat turn.
    #[allow(clippy::too_many_lines)]
    pub fn record_assistant_terminal(
        &mut self,
        command: &RecordAssistantTerminalCommand,
    ) -> Result<AssistantMessageMutationReceipt, ProductSessionServiceError> {
        let digest = command_digest("assistant_terminal", terminal_digest_fields(command))?;
        if let Some(replay) =
            self.replay(&command.context, &digest, MutationKind::AssistantUpdated)?
        {
            return assistant_receipt(replay, &command.model_exchange_id);
        }
        validate_terminal_command(command)?;
        let current = self
            .storage
            .load_worker_binding_source(
                &command.execution_scope,
                &command.worker_pool_id,
                &command.runtime_authority.worker_session_id,
            )
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| binding_mismatch("assistant terminal has no Worker authority"))?;
        validate_terminal_runtime_authority(command, &current.0, &current.1)?;
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        let persisted = catalog
            .sessions
            .get_mut(&command.product_session_id.0)
            .ok_or_else(not_found)?;
        require_revision(&persisted.session, command.context.expected_revision)?;
        require_persisted_terminal_binding(persisted, command)?;
        let public_state = terminal_public_state(&command.terminal_outcome.status);
        let message_id =
            deterministic_message_id("assistant", command.model_exchange_id.0.as_bytes());
        match persisted
            .messages
            .iter_mut()
            .find(|message| message.projection.id == message_id)
        {
            Some(message)
                if message.model_exchange_id.as_ref() == Some(&command.model_exchange_id)
                    && (message.projection.state == "streaming"
                        || (public_state == "cancelled"
                            && message.projection.state == "cancelled")) =>
            {
                public_state.clone_into(&mut message.projection.state);
                message.projection.updated_at = command.context.occurred_at.clone();
            }
            Some(_) => return Err(stream_conflict()),
            None => {
                if persisted.messages.len() >= MAX_MESSAGES_PER_SESSION {
                    return Err(message_limit());
                }
                persisted.messages.push(PersistedChatMessage {
                    projection: ChatMessageProjection {
                        content: String::new(),
                        created_at: command.context.occurred_at.clone(),
                        id: message_id,
                        product_session_id: command.product_session_id.clone(),
                        role: "assistant".to_owned(),
                        sequence: next_message_sequence(&persisted.messages)?,
                        state: public_state.to_owned(),
                        updated_at: command.context.occurred_at.clone(),
                    },
                    model_exchange_id: Some(command.model_exchange_id.clone()),
                    last_stream_sequence: 1,
                });
            }
        }
        finish_terminal_turn(persisted, command)?;
        let persisted = persisted.clone();
        let mutation = self.commit(
            &command.context,
            digest,
            MutationKind::AssistantUpdated,
            catalog,
            &command.product_session_id,
            persisted,
            None,
        )?;
        assistant_receipt(mutation, &command.model_exchange_id)
    }

    pub(crate) fn replay_assistant_terminal(
        &self,
        command: &RecordAssistantTerminalCommand,
    ) -> Result<Option<AssistantMessageMutationReceipt>, ProductSessionServiceError> {
        let digest = command_digest("assistant_terminal", terminal_digest_fields(command))?;
        self.replay(&command.context, &digest, MutationKind::AssistantUpdated)?
            .map(|receipt| assistant_receipt(receipt, &command.model_exchange_id))
            .transpose()
    }

    pub(crate) fn last_assistant_stream_sequence(
        &self,
        scope: &ReceiptScopeKey,
        product_session_id: &ProductSessionId,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<u64, ProductSessionServiceError> {
        let catalog = self.load_catalog(scope)?;
        let persisted = catalog
            .sessions
            .get(&product_session_id.0)
            .ok_or_else(not_found)?;
        Ok(persisted
            .messages
            .iter()
            .filter(|message| message.model_exchange_id.as_ref() == Some(model_exchange_id))
            .map(|message| message.last_stream_sequence)
            .max()
            .unwrap_or(0))
    }

    /// Cancels one exact current revision and returns only exact route commands.
    ///
    /// # Errors
    ///
    /// Rejects actor/scope/revision/runtime mismatches and changed-body replays
    /// before changing the session or emitting any cancellation route.
    pub fn cancel_session(
        &mut self,
        command: &CancelProductSessionCommand,
    ) -> Result<ProductSessionCancellationReceipt, ProductSessionServiceError> {
        let digest = command_digest("cancel", cancel_digest_fields(command))?;
        if let Some(replay) = self.replay(&command.context, &digest, MutationKind::Cancelled)? {
            return cancellation_receipt(replay, RouteWriteStatus::Duplicate);
        }
        if actor_from_public(&command.context.public_actor) != command.actor {
            return Err(service_error(
                ProductSessionServiceErrorCode::ActorMismatch,
                "session cancellation actor does not match the authenticated command actor",
            ));
        }
        let mut catalog = self.load_catalog(command.context.receipt_identity.scope_key())?;
        let persisted_snapshot = catalog
            .sessions
            .get(&command.product_session_id.0)
            .ok_or_else(not_found)?
            .clone();
        require_revision(
            &persisted_snapshot.session,
            command.context.expected_revision,
        )?;
        if matches!(
            persisted_snapshot.session.state(),
            ProductSessionState::Cancelled | ProductSessionState::Closed
        ) {
            return Err(service_error(
                ProductSessionServiceErrorCode::InvalidState,
                "ProductSession is already terminal under another command",
            ));
        }
        let active_executions =
            self.active_cancellation_routes(&persisted_snapshot, &command.context.public_scope)?;
        let mut interaction_router = InteractionRouter::default();
        interaction_router
            .register_cancellation_scope(SessionCancellationSnapshot {
                product_session_id: command.product_session_id.clone(),
                revision: persisted_snapshot.session.revision(),
                authorized_actor: command.actor.clone(),
                active_executions,
            })
            .map_err(|error| routing_error(&error))?;
        let cancellation = interaction_router
            .cancel_session(&SessionCancellationRequest {
                request_id: command.context.receipt_identity.request_id().clone(),
                actor: command.actor.clone(),
                product_session_id: command.product_session_id.clone(),
                expected_revision: command.context.expected_revision,
                reason: command.reason.clone(),
                requested_at: command.context.occurred_at.clone(),
            })
            .map_err(|error| routing_error(&error))?;
        let persisted = catalog
            .sessions
            .get_mut(&command.product_session_id.0)
            .ok_or_else(not_found)?;
        persisted
            .session
            .cancel(&command.reason, command.context.occurred_at.clone())
            .map_err(|error| domain_error(&error))?;
        if persisted.session.revision() != cancellation.current_revision {
            return Err(corrupt(
                "ProductSession and cancellation router revisions diverged",
            ));
        }
        for message in &mut persisted.messages {
            if message.projection.role == "assistant" && message.projection.state == "streaming" {
                "cancelled".clone_into(&mut message.projection.state);
                message.projection.updated_at = command.context.occurred_at.clone();
            }
        }
        for turn in &mut persisted.turn_intents {
            if matches!(
                turn.state,
                PersistedTurnState::Pending | PersistedTurnState::Bound
            ) {
                turn.state = PersistedTurnState::Cancelled;
            }
        }
        persisted.cancellation = Some(PersistedCancellation {
            request_id: command.context.receipt_identity.request_id().clone(),
            routes: cancellation
                .routes
                .iter()
                .map(PersistedExecutionCancellationRoutes::from_domain)
                .collect(),
        });
        let persisted = persisted.clone();
        let mutation = self.commit(
            &command.context,
            digest,
            MutationKind::Cancelled,
            catalog,
            &command.product_session_id,
            persisted,
            None,
        )?;
        cancellation_receipt(mutation, RouteWriteStatus::Applied)
    }

    /// Reads a stable filtered session page. A cursor from another scope,
    /// filter, or catalog revision is rejected rather than silently drifting.
    ///
    /// # Errors
    ///
    /// Rejects invalid/stale cursors, invalid limits, corrupt state, and storage failures.
    pub fn list_page(
        &self,
        scope: &ReceiptScopeKey,
        states: &[ProductSessionState],
        page: &ProductSessionPageRequest,
    ) -> Result<ProductSessionPageRead, ProductSessionServiceError> {
        let limit = validate_page_request(page)?;
        let catalog = self.load_catalog(scope)?;
        let mut filters = states
            .iter()
            .copied()
            .map(session_state_label)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        filters.sort_unstable();
        filters.dedup();
        let filter_sha256 = digest_json(&filters)?;
        let after = decode_cursor(
            page.cursor.as_ref(),
            PageCursorKind::Sessions,
            scope,
            catalog.revision,
            &filter_sha256,
        )?;
        let mut records = catalog
            .sessions
            .iter()
            .filter(|(id, session)| {
                after.as_ref().is_none_or(|after| *id > after)
                    && (filters.is_empty()
                        || filters
                            .binary_search(&session_state_label(session.session.state()).to_owned())
                            .is_ok())
            })
            .map(|(_, session)| session.to_record())
            .take(limit + 1)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = records.len() > limit;
        records.truncate(limit);
        let next_cursor = has_more
            .then(|| records.last().map(|record| record.session().id().0.clone()))
            .flatten()
            .map(|after| {
                encode_cursor(
                    PageCursorKind::Sessions,
                    scope,
                    catalog.revision,
                    &filter_sha256,
                    after,
                )
            })
            .transpose()?;
        Ok(ProductSessionPageRead {
            items: records,
            next_cursor,
        })
    }

    /// Reads a stable public-only message page for one exact session.
    ///
    /// # Errors
    ///
    /// Rejects an unknown session, invalid/stale cursor, corrupt state, or storage failure.
    pub fn messages_page(
        &self,
        scope: &ReceiptScopeKey,
        product_session_id: &ProductSessionId,
        page: &ProductSessionPageRequest,
    ) -> Result<ProductSessionMessagePage, ProductSessionServiceError> {
        let limit = validate_page_request(page)?;
        let catalog = self.load_catalog(scope)?;
        let persisted = catalog
            .sessions
            .get(&product_session_id.0)
            .ok_or_else(not_found)?;
        let filter_sha256 = digest_json(&product_session_id.0)?;
        let after = decode_cursor(
            page.cursor.as_ref(),
            PageCursorKind::Messages,
            scope,
            catalog.revision,
            &filter_sha256,
        )?
        .map(|value| value.parse::<i64>().map_err(|_| cursor_invalid()))
        .transpose()?;
        let mut messages = persisted
            .messages
            .iter()
            .filter(|message| after.is_none_or(|after| message.projection.sequence > after))
            .map(|message| message.projection.clone())
            .take(limit + 1)
            .collect::<Vec<_>>();
        let has_more = messages.len() > limit;
        messages.truncate(limit);
        let next_cursor = has_more
            .then(|| messages.last().map(|message| message.sequence.to_string()))
            .flatten()
            .map(|after| {
                encode_cursor(
                    PageCursorKind::Messages,
                    scope,
                    catalog.revision,
                    &filter_sha256,
                    after,
                )
            })
            .transpose()?;
        Ok(ProductSessionMessagePage {
            items: messages,
            next_cursor,
        })
    }

    fn active_cancellation_routes(
        &mut self,
        persisted: &PersistedProductSession,
        public_scope: &PublicEventScope,
    ) -> Result<Vec<ExecutionRoute>, ProductSessionServiceError> {
        let Some(binding) = persisted.bindings.last() else {
            return self.queued_cancellation_route(persisted, public_scope);
        };
        let current = self
            .storage
            .load_worker_binding_source(
                &binding.reservation.scope,
                &binding.reservation.worker_pool_id,
                &binding.slot.authority.worker_session_id,
            )
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| {
                binding_mismatch("session cancellation has no current Worker authority")
            })?;
        validate_persisted_runtime(binding, &current.0, &current.1)?;
        let identity = binding.identity.to_domain()?;
        Ok(vec![ExecutionRoute {
            product_session_id: persisted.session.id().clone(),
            stage_run_id: identity.stage_run_id().cloned(),
            execution_job_id: binding.slot.authority.job_id.clone(),
            job_revision: current.1.revision,
            runtime: Some(runtime_route(&current.0.authority)),
            worker_slot_revision: Some(current.0.revision),
            model_exchange_id: Some(binding.model_exchange_id.clone()),
        }])
    }

    fn queued_cancellation_route(
        &mut self,
        persisted: &PersistedProductSession,
        public_scope: &PublicEventScope,
    ) -> Result<Vec<ExecutionRoute>, ProductSessionServiceError> {
        let Some(turn) = persisted
            .turn_intents
            .iter()
            .rev()
            .find(|turn| turn.state == PersistedTurnState::Pending)
        else {
            return Ok(Vec::new());
        };
        let PublicEventScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } = public_scope
        else {
            return Err(binding_mismatch(
                "ProductSession cancellation requires repository scope",
            ));
        };
        if persisted.session.project_id() != project_id
            || persisted.session.repository_id() != repository_id
        {
            return Err(binding_mismatch(
                "ProductSession cancellation scope differs from the queued turn",
            ));
        }
        let execution_scope = ExecutionQueueScope {
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            repository_id: repository_id.clone(),
            product_session_id: persisted.session.id().clone(),
            delivery_id: None,
        };
        let job = self
            .storage
            .load_product_session_execution_job(
                &execution_scope,
                &turn.execution_job_id,
                &turn.request_id,
            )
            .map_err(|error| storage_error(&error))?
            .ok_or_else(|| binding_mismatch("queued Chat turn has no canonical ExecutionJob"))?;
        if job.state != ExecutionJobState::Queued {
            return Err(binding_mismatch(
                "unbound Chat turn is not a queued ExecutionJob",
            ));
        }
        Ok(vec![ExecutionRoute {
            product_session_id: persisted.session.id().clone(),
            stage_run_id: None,
            execution_job_id: job.job_id,
            job_revision: job.revision,
            runtime: None,
            worker_slot_revision: None,
            model_exchange_id: None,
        }])
    }
}

pub(super) fn validate_persisted_chat(
    session: &PersistedProductSession,
) -> Result<(), ProductSessionServiceError> {
    if session.messages.len() > MAX_MESSAGES_PER_SESSION {
        return Err(corrupt("ProductSession message ledger is over its bound"));
    }
    for (index, message) in session.messages.iter().enumerate() {
        let expected_sequence = i64::try_from(index + 1)
            .map_err(|_| corrupt("ProductSession message sequence overflowed"))?;
        if message.projection.sequence != expected_sequence
            || message.projection.product_session_id != *session.session.id()
            || !canonical_id(&message.projection.id.0, "msg_")
            || message.projection.content.len() > MAX_MESSAGE_BYTES
            || !matches!(message.projection.role.as_str(), "user" | "assistant")
            || !matches!(
                message.projection.state.as_str(),
                "streaming" | "completed" | "cancelled" | "failed"
            )
            || (message.projection.role == "user"
                && (message.projection.state != "completed"
                    || message.model_exchange_id.is_some()
                    || message.last_stream_sequence != 0))
            || (message.projection.role == "assistant"
                && (message.model_exchange_id.is_none() || message.last_stream_sequence == 0))
        {
            return Err(corrupt("ProductSession message ledger is inconsistent"));
        }
    }
    for turn in &session.turn_intents {
        if turn.product_session_id != *session.session.id()
            || turn.model_selection != session.model_selection
            || !canonical_id(&turn.execution_job_id.0, "job_")
            || !session.messages.iter().any(|message| {
                message.projection.id == turn.user_message_id && message.projection.role == "user"
            })
        {
            return Err(corrupt("ProductSession turn intent is inconsistent"));
        }
        match (&turn.state, turn.terminal_outcome.as_ref()) {
            (
                PersistedTurnState::Pending
                | PersistedTurnState::Bound
                | PersistedTurnState::Cancelled,
                None,
            ) => {}
            (PersistedTurnState::Completed, Some(outcome))
                if outcome.status == ExecutionOutcomeStatus::Succeeded
                    && outcome.usage.is_some() =>
            {
                validate_terminal_usage(outcome)?;
                require_terminal_message(session, turn, "completed")?;
            }
            (PersistedTurnState::Cancelled, Some(outcome))
                if outcome.status == ExecutionOutcomeStatus::Cancelled =>
            {
                validate_terminal_usage(outcome)?;
                require_terminal_message(session, turn, "cancelled")?;
            }
            (PersistedTurnState::Failed, Some(outcome))
                if matches!(
                    outcome.status,
                    ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError
                ) =>
            {
                validate_terminal_usage(outcome)?;
                require_terminal_message(session, turn, "failed")?;
            }
            _ => {
                return Err(corrupt(
                    "ProductSession turn terminal outcome is inconsistent",
                ));
            }
        }
    }
    if session
        .turn_intents
        .iter()
        .enumerate()
        .any(|(index, turn)| {
            session.turn_intents[..index]
                .iter()
                .any(|prior| prior.execution_job_id == turn.execution_job_id)
        })
    {
        return Err(corrupt(
            "ProductSession turn ExecutionJob identities are duplicated",
        ));
    }
    Ok(())
}

fn require_terminal_message(
    session: &PersistedProductSession,
    turn: &PersistedTurnIntent,
    expected_state: &str,
) -> Result<(), ProductSessionServiceError> {
    let exchange = turn
        .model_exchange_id
        .as_ref()
        .ok_or_else(|| corrupt("terminal ProductSession turn has no model exchange"))?;
    if session.messages.iter().any(|message| {
        message.projection.role == "assistant"
            && message.projection.state == expected_state
            && message.model_exchange_id.as_ref() == Some(exchange)
    }) {
        Ok(())
    } else {
        Err(corrupt(
            "ProductSession terminal outcome has no matching assistant message",
        ))
    }
}

fn chat_submit_receipt(
    mutation: ProductSessionMutationReceipt,
) -> Result<ChatSubmitMutationReceipt, ProductSessionServiceError> {
    let message = mutation
        .record
        .messages()
        .last()
        .filter(|message| message.role == "user")
        .cloned()
        .ok_or_else(|| corrupt("chat.submit receipt has no user message"))?;
    let turn_intent = mutation
        .record
        .turn_intents()
        .last()
        .cloned()
        .ok_or_else(|| corrupt("chat.submit receipt has no turn intent"))?;
    Ok(ChatSubmitMutationReceipt {
        mutation,
        message,
        turn_intent,
    })
}

fn assistant_receipt(
    mutation: ProductSessionMutationReceipt,
    model_exchange_id: &ModelExchangeId,
) -> Result<AssistantMessageMutationReceipt, ProductSessionServiceError> {
    let message_id = deterministic_message_id("assistant", model_exchange_id.0.as_bytes());
    let message = mutation
        .record
        .messages()
        .iter()
        .find(|message| message.id == message_id)
        .cloned()
        .ok_or_else(|| corrupt("assistant receipt has no matching public message"))?;
    Ok(AssistantMessageMutationReceipt { mutation, message })
}

fn cancellation_receipt(
    mutation: ProductSessionMutationReceipt,
    status: RouteWriteStatus,
) -> Result<ProductSessionCancellationReceipt, ProductSessionServiceError> {
    let previous_revision = mutation
        .record
        .session()
        .revision()
        .checked_sub(1)
        .ok_or_else(|| corrupt("session cancellation revision underflowed"))?;
    let routing = winwincode_session::SessionCancellationReceipt {
        status,
        request_id: mutation
            .record
            .cancellation_request_id()
            .cloned()
            .ok_or_else(|| corrupt("session cancellation receipt has no request identity"))?,
        product_session_id: mutation.record.session().id().clone(),
        previous_revision,
        current_revision: mutation.record.session().revision(),
        routes: mutation.record.cancellation_routes().to_vec(),
    };
    Ok(ProductSessionCancellationReceipt { mutation, routing })
}

fn submit_digest_fields(command: &SubmitChatMessageCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id.0,
        "message": command.message,
    })
}

fn assistant_digest_fields(command: &AppendAssistantMessageCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id.0,
        "bindingIdentity": PersistedBindingIdentity::from_domain(&command.binding_identity),
        "runtimeAuthority": command.runtime_authority,
        "executionScope": command.execution_scope,
        "workerPoolId": command.worker_pool_id,
        "modelExchangeId": command.model_exchange_id.0,
        "streamSequence": command.stream_sequence,
        "publicTextDelta": command.public_text_delta,
        "state": command.state.label(),
        "terminalOutcome": command.terminal_outcome,
    })
}

fn terminal_digest_fields(command: &RecordAssistantTerminalCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id.0,
        "bindingIdentity": PersistedBindingIdentity::from_domain(&command.binding_identity),
        "runtimeAuthority": command.runtime_authority,
        "executionScope": command.execution_scope,
        "workerPoolId": command.worker_pool_id,
        "modelExchangeId": command.model_exchange_id.0,
        "terminalOutcome": command.terminal_outcome,
    })
}

fn cancel_digest_fields(command: &CancelProductSessionCommand) -> serde_json::Value {
    serde_json::json!({
        "context": context_digest_fields(&command.context),
        "productSessionId": command.product_session_id.0,
        "reason": command.reason,
    })
}

fn validate_message_text(
    message: &str,
    allow_empty: bool,
) -> Result<(), ProductSessionServiceError> {
    if (!allow_empty && message.is_empty()) || message.len() > MAX_MESSAGE_BYTES {
        return Err(message_limit());
    }
    Ok(())
}

fn validate_assistant_command(
    command: &AppendAssistantMessageCommand,
) -> Result<(), ProductSessionServiceError> {
    validate_message_text(&command.public_text_delta, command.state.terminal())?;
    if command.stream_sequence == 0
        || command.stream_sequence > 9_007_199_254_740_991
        || command.binding_identity.product_session_id() != &command.product_session_id
        || command.execution_scope.product_session_id != command.product_session_id
        || command.binding_identity.execution_job_id() != &command.runtime_authority.job_id
    {
        return Err(binding_mismatch(
            "assistant update identities or stream sequence are invalid",
        ));
    }
    match (command.state, command.terminal_outcome.as_ref()) {
        (AssistantMessageState::Streaming, None) => {}
        (AssistantMessageState::Completed, Some(outcome))
            if outcome.status == ExecutionOutcomeStatus::Succeeded && outcome.usage.is_some() =>
        {
            validate_terminal_usage(outcome)?;
        }
        (AssistantMessageState::Cancelled, Some(outcome))
            if outcome.status == ExecutionOutcomeStatus::Cancelled =>
        {
            validate_terminal_usage(outcome)?;
        }
        (AssistantMessageState::Failed, Some(outcome))
            if matches!(
                outcome.status,
                ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError
            ) =>
        {
            validate_terminal_usage(outcome)?;
        }
        _ => {
            return Err(binding_mismatch(
                "assistant terminal state and immutable Worker outcome differ",
            ));
        }
    }
    Ok(())
}

fn validate_terminal_usage(
    outcome: &ProductSessionTurnTerminalOutcome,
) -> Result<(), ProductSessionServiceError> {
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
    if outcome.last_event_sequence.0 < 0
        || outcome.last_event_sequence.0 > MAX_SAFE_INTEGER
        || outcome.usage.as_ref().is_some_and(|usage| {
            !(0..=MAX_SAFE_INTEGER).contains(&usage.runtime_millis)
                || !(0..=MAX_SAFE_INTEGER).contains(&usage.tokens)
                || !(0..=MAX_SAFE_INTEGER).contains(&usage.cost_microunits)
        })
    {
        return Err(binding_mismatch(
            "assistant terminal usage is outside the canonical range",
        ));
    }
    Ok(())
}

fn validate_terminal_command(
    command: &RecordAssistantTerminalCommand,
) -> Result<(), ProductSessionServiceError> {
    if command.binding_identity.product_session_id() != &command.product_session_id
        || command.execution_scope.product_session_id != command.product_session_id
        || command.binding_identity.execution_job_id() != &command.runtime_authority.job_id
    {
        return Err(binding_mismatch(
            "assistant terminal identities are inconsistent",
        ));
    }
    if command.terminal_outcome.status == ExecutionOutcomeStatus::Succeeded
        && command.terminal_outcome.usage.is_none()
    {
        return Err(binding_mismatch(
            "successful assistant terminal has no immutable Usage",
        ));
    }
    validate_terminal_usage(&command.terminal_outcome)
}

fn validate_terminal_runtime_authority(
    command: &RecordAssistantTerminalCommand,
    slot: &WorkerSlotRecord,
    reservation: &ExecutionReservationRecord,
) -> Result<(), ProductSessionServiceError> {
    let slot_state_matches = match command.terminal_outcome.status {
        ExecutionOutcomeStatus::Cancelled => matches!(
            slot.state,
            WorkerSlotState::Running | WorkerSlotState::Cancelling
        ),
        ExecutionOutcomeStatus::Succeeded
        | ExecutionOutcomeStatus::Failed
        | ExecutionOutcomeStatus::InfrastructureError => slot.state == WorkerSlotState::Running,
    };
    if !slot_state_matches
        || reservation.state != ExecutionReservationState::Running
        || slot.authority != command.runtime_authority
        || reservation.scope != command.execution_scope
        || reservation.worker_pool_id != command.worker_pool_id
        || reservation.job_id != command.runtime_authority.job_id
    {
        return Err(binding_mismatch(
            "assistant terminal has stale Worker, lease, fence, or reservation authority",
        ));
    }
    Ok(())
}

fn require_persisted_terminal_binding(
    session: &PersistedProductSession,
    command: &RecordAssistantTerminalCommand,
) -> Result<(), ProductSessionServiceError> {
    let identity = PersistedBindingIdentity::from_domain(&command.binding_identity);
    if session.bindings.iter().any(|binding| {
        binding.identity == identity
            && binding.model_exchange_id == command.model_exchange_id
            && binding.slot.authority == command.runtime_authority
            && binding.reservation.scope == command.execution_scope
            && binding.reservation.worker_pool_id == command.worker_pool_id
    }) {
        Ok(())
    } else {
        Err(binding_mismatch(
            "assistant terminal is not bound to this ProductSession turn",
        ))
    }
}

const fn terminal_public_state(status: &ExecutionOutcomeStatus) -> &'static str {
    match status {
        ExecutionOutcomeStatus::Succeeded => "completed",
        ExecutionOutcomeStatus::Cancelled => "cancelled",
        ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError => "failed",
    }
}

fn require_persisted_binding(
    session: &PersistedProductSession,
    command: &AppendAssistantMessageCommand,
    slot: &WorkerSlotRecord,
    reservation: &ExecutionReservationRecord,
) -> Result<(), ProductSessionServiceError> {
    let identity = PersistedBindingIdentity::from_domain(&command.binding_identity);
    let Some(binding) = session.bindings.iter().find(|binding| {
        binding.identity == identity
            && binding.model_exchange_id == command.model_exchange_id
            && binding.slot.authority == command.runtime_authority
    }) else {
        return Err(binding_mismatch(
            "assistant update is not bound to this ProductSession turn",
        ));
    };
    validate_persisted_runtime(binding, slot, reservation)
}

fn validate_persisted_runtime(
    binding: &PersistedSessionBinding,
    slot: &WorkerSlotRecord,
    reservation: &ExecutionReservationRecord,
) -> Result<(), ProductSessionServiceError> {
    if slot.state != WorkerSlotState::Running
        || reservation.state != ExecutionReservationState::Running
        || slot.authority != binding.slot.authority
        || reservation.scope != binding.reservation.scope
        || reservation.worker_pool_id != binding.reservation.worker_pool_id
        || reservation.job_id != binding.reservation.job_id
    {
        return Err(binding_mismatch(
            "current Worker slot differs from the sealed ProductSession binding",
        ));
    }
    Ok(())
}

fn validate_runtime_authority(
    authority: &WorkerSlotAuthority,
    scope: &ExecutionQueueScope,
    pool: &WorkerPoolId,
    slot: &WorkerSlotRecord,
    reservation: &ExecutionReservationRecord,
) -> Result<(), ProductSessionServiceError> {
    if slot.state != WorkerSlotState::Running
        || reservation.state != ExecutionReservationState::Running
        || &slot.authority != authority
        || &reservation.scope != scope
        || &reservation.worker_pool_id != pool
        || reservation.job_id != authority.job_id
    {
        return Err(binding_mismatch(
            "assistant update has stale Worker, lease, fence, or reservation authority",
        ));
    }
    Ok(())
}

fn finish_turn(
    persisted: &mut PersistedProductSession,
    command: &AppendAssistantMessageCommand,
) -> Result<(), ProductSessionServiceError> {
    let turn = persisted
        .turn_intents
        .iter_mut()
        .rev()
        .find(|turn| {
            turn.state == PersistedTurnState::Bound
                && turn.execution_job_id == command.runtime_authority.job_id
                && turn.model_exchange_id.as_ref() == Some(&command.model_exchange_id)
        })
        .ok_or_else(|| binding_mismatch("assistant terminal has no exact bound Chat turn"))?;
    if turn.terminal_outcome.is_some() {
        return Err(stream_conflict());
    }
    turn.state = match command.state {
        AssistantMessageState::Completed => PersistedTurnState::Completed,
        AssistantMessageState::Cancelled => PersistedTurnState::Cancelled,
        AssistantMessageState::Failed => PersistedTurnState::Failed,
        AssistantMessageState::Streaming => return Ok(()),
    };
    turn.terminal_outcome.clone_from(&command.terminal_outcome);
    match command.state {
        AssistantMessageState::Completed | AssistantMessageState::Cancelled => persisted
            .session
            .complete_turn(command.context.occurred_at.clone()),
        AssistantMessageState::Failed => persisted
            .session
            .fail("model_stream_failed", command.context.occurred_at.clone()),
        AssistantMessageState::Streaming => return Ok(()),
    }
    .map_err(|error| domain_error(&error))
}

fn finish_terminal_turn(
    persisted: &mut PersistedProductSession,
    command: &RecordAssistantTerminalCommand,
) -> Result<(), ProductSessionServiceError> {
    let turn = persisted
        .turn_intents
        .iter_mut()
        .rev()
        .find(|turn| {
            turn.execution_job_id == command.runtime_authority.job_id
                && turn.model_exchange_id.as_ref() == Some(&command.model_exchange_id)
        })
        .ok_or_else(|| binding_mismatch("assistant terminal has no exact bound Chat turn"))?;
    if turn.terminal_outcome.is_some() {
        return Err(stream_conflict());
    }
    let cancelled_reconciliation = turn.state == PersistedTurnState::Cancelled
        && persisted.session.state() == ProductSessionState::Cancelled
        && command.terminal_outcome.status == ExecutionOutcomeStatus::Cancelled;
    if cancelled_reconciliation {
        turn.terminal_outcome = Some(command.terminal_outcome.clone());
        return Ok(());
    }
    if turn.state != PersistedTurnState::Bound
        || persisted.session.state() != ProductSessionState::Running
    {
        return Err(binding_mismatch(
            "assistant terminal differs from the active Chat turn",
        ));
    }
    turn.state = match command.terminal_outcome.status {
        ExecutionOutcomeStatus::Succeeded => PersistedTurnState::Completed,
        ExecutionOutcomeStatus::Cancelled => PersistedTurnState::Cancelled,
        ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError => {
            PersistedTurnState::Failed
        }
    };
    turn.terminal_outcome = Some(command.terminal_outcome.clone());
    match command.terminal_outcome.status {
        ExecutionOutcomeStatus::Succeeded | ExecutionOutcomeStatus::Cancelled => persisted
            .session
            .complete_turn(command.context.occurred_at.clone()),
        ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError => persisted
            .session
            .fail("model_stream_failed", command.context.occurred_at.clone()),
    }
    .map_err(|error| domain_error(&error))
}

fn next_message_sequence(
    messages: &[PersistedChatMessage],
) -> Result<i64, ProductSessionServiceError> {
    let next = messages.last().map_or(1_i64, |message| {
        message.projection.sequence.saturating_add(1)
    });
    if next <= 0 || next > 9_007_199_254_740_991 {
        return Err(corrupt("ProductSession message sequence overflowed"));
    }
    Ok(next)
}

fn deterministic_message_id(namespace: &str, identity: &[u8]) -> ChatMessageId {
    let mut hasher = Sha256::new();
    hasher.update(b"winwincode.chat-message.v1\0");
    hasher.update(namespace.as_bytes());
    hasher.update([0]);
    hasher.update(identity);
    let encoded = format!("{:X}", hasher.finalize());
    ChatMessageId(format!("msg_{}", &encoded[..26]))
}

fn actor_from_public(actor: &PublicEventActor) -> AuthenticatedActor {
    match actor {
        PublicEventActor::User { id } => AuthenticatedActor::User(id.clone()),
        PublicEventActor::ServiceAccount { id } => AuthenticatedActor::ServiceAccount(id.clone()),
        PublicEventActor::System { id } => AuthenticatedActor::System(id.clone()),
    }
}

fn runtime_route(authority: &WorkerSlotAuthority) -> RuntimeRouteAuthority {
    RuntimeRouteAuthority {
        lease_id: authority.lease_id.clone(),
        worker_id: authority.worker_id.clone(),
        worker_instance_id: authority.worker_instance_id.clone(),
        worker_session_id: authority.worker_session_id.clone(),
        attempt: authority.attempt,
        fencing_token: authority.fencing_token.clone(),
    }
}

fn routing_error(error: &InteractionRoutingError) -> ProductSessionServiceError {
    let code = match error {
        InteractionRoutingError::ActorMismatch => ProductSessionServiceErrorCode::ActorMismatch,
        InteractionRoutingError::RevisionConflict { .. } => {
            ProductSessionServiceErrorCode::RevisionConflict
        }
        InteractionRoutingError::IdempotencyConflict => {
            ProductSessionServiceErrorCode::RequestConflict
        }
        InteractionRoutingError::InvalidField(_) => ProductSessionServiceErrorCode::InvalidInput,
        InteractionRoutingError::BindingMismatch
        | InteractionRoutingError::UnknownProductSession => {
            ProductSessionServiceErrorCode::BindingIdentityMismatch
        }
        InteractionRoutingError::DuplicateRegistration
        | InteractionRoutingError::UnknownInteraction
        | InteractionRoutingError::SubjectMismatch
        | InteractionRoutingError::DecisionKindMismatch
        | InteractionRoutingError::AttentionDecisionNotAllowed
        | InteractionRoutingError::AlreadyResolved
        | InteractionRoutingError::SessionAlreadyCancelled => {
            ProductSessionServiceErrorCode::InvalidState
        }
    };
    service_error(code, format!("ProductSession routing failed: {error}"))
}

fn validate_page_request(
    page: &ProductSessionPageRequest,
) -> Result<usize, ProductSessionServiceError> {
    let limit = usize::from(page.limit);
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(cursor_invalid());
    }
    Ok(limit)
}

fn encode_cursor(
    kind: PageCursorKind,
    scope: &ReceiptScopeKey,
    catalog_revision: u64,
    filter_sha256: &str,
    after: String,
) -> Result<OpaqueCursor, ProductSessionServiceError> {
    let cursor = PageCursor {
        schema_version: PRODUCT_SESSION_SERVICE_SCHEMA_VERSION,
        kind,
        scope_sha256: scope_digest(scope),
        catalog_revision,
        filter_sha256: filter_sha256.to_owned(),
        after,
    };
    let bytes = serde_json::to_vec(&cursor)
        .map_err(|error| corrupt(format!("ProductSession cursor cannot be encoded: {error}")))?;
    Ok(OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
    kind: PageCursorKind,
    scope: &ReceiptScopeKey,
    catalog_revision: u64,
    filter_sha256: &str,
) -> Result<Option<String>, ProductSessionServiceError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.0.len() > MAX_CURSOR_BYTES {
        return Err(cursor_invalid());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.0.as_bytes())
        .map_err(|_| cursor_invalid())?;
    let decoded: PageCursor = serde_json::from_slice(&bytes).map_err(|_| cursor_invalid())?;
    if decoded.schema_version != PRODUCT_SESSION_SERVICE_SCHEMA_VERSION
        || decoded.kind != kind
        || decoded.scope_sha256 != scope_digest(scope)
        || decoded.catalog_revision != catalog_revision
        || decoded.filter_sha256 != filter_sha256
        || decoded.after.is_empty()
    {
        return Err(cursor_invalid());
    }
    Ok(Some(decoded.after))
}

fn scope_digest(scope: &ReceiptScopeKey) -> String {
    format!("sha256:{:x}", Sha256::digest(scope.as_bytes()))
}

fn digest_json(value: &impl Serialize) -> Result<String, ProductSessionServiceError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| corrupt(format!("ProductSession read filter cannot encode: {error}")))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn cursor_invalid() -> ProductSessionServiceError {
    service_error(
        ProductSessionServiceErrorCode::CursorInvalid,
        "ProductSession page cursor is invalid or stale",
    )
}

fn message_limit() -> ProductSessionServiceError {
    service_error(
        ProductSessionServiceErrorCode::MessageLimitExceeded,
        "ProductSession public message is outside its size bound",
    )
}

fn stream_conflict() -> ProductSessionServiceError {
    service_error(
        ProductSessionServiceErrorCode::StreamSequenceConflict,
        "ProductSession assistant stream sequence is stale or contains a gap",
    )
}
