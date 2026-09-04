// SPDX-License-Identifier: Apache-2.0

//! Generated HTTP-contract adapter for the canonical `ProductSession` service.

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ChatSubmitCommand, ChatSubmitCompletedResponse, ChatSubmitCompletedResponseCommand,
    ChatSubmitCompletedResponseOutcome, PageInfo, SessionCancelCommand,
    SessionCancelCompletedResponse, SessionCancelCompletedResponseCommand,
    SessionCancelCompletedResponseOutcome, SessionCloseCommand, SessionCloseCompletedResponse,
    SessionCloseCompletedResponseCommand, SessionCloseCompletedResponseOutcome,
    SessionCreateCommand, SessionCreateCompletedResponse, SessionCreateCompletedResponseCommand,
    SessionCreateCompletedResponseOutcome, SessionGetQuery, SessionGetResultResponse,
    SessionGetResultResponseQuery, SessionListQuery, SessionListResultResponse,
    SessionListResultResponseQuery, SessionMessagesListQuery, SessionMessagesListResultResponse,
    SessionMessagesListResultResponseQuery,
};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{ControlPlaneEventId, Instant, RequestId};
use winwincode_storage::{
    PublicEventActor, PublicEventScope, ReceiptScopeKey, public_receipt_identity,
};

use super::{
    CancelProductSessionCommand, CloseProductSessionCommand, CreateProductSessionCommand,
    ProductSessionExecutionConfig, ProductSessionPageRequest, ProductSessionPersistence,
    ProductSessionService, ProductSessionServiceError, ProductSessionServiceErrorCode,
    SubmitChatMessageCommand, corrupt, product_session_state_filters, service_error, storage_error,
};

/// Trusted time source injected once at the Control Plane composition root.
pub trait ProductSessionApiClock {
    fn now(&mut self) -> Instant;
}

/// Sole generated-command/query adapter for the canonical `ProductSession` state.
///
/// The adapter owns envelope mapping, stable event identity, pagination, and
/// public response construction. The Server only authenticates and dispatches.
pub struct ProductSessionApiService<'storage, 'clock, 'execution> {
    service: ProductSessionService<'storage>,
    clock: &'clock mut dyn ProductSessionApiClock,
    execution: &'execution ProductSessionExecutionConfig,
}

impl<'storage, 'clock, 'execution> ProductSessionApiService<'storage, 'clock, 'execution> {
    #[must_use]
    pub fn new(
        storage: &'storage mut dyn ProductSessionPersistence,
        clock: &'clock mut dyn ProductSessionApiClock,
        execution: &'execution ProductSessionExecutionConfig,
    ) -> Self {
        Self {
            service: ProductSessionService::new(storage),
            clock,
            execution,
        }
    }

    /// Creates a generated API adapter with the exact resolved-secret
    /// fingerprints already held by the composition root.
    #[must_use]
    pub fn with_output_gate(
        storage: &'storage mut dyn ProductSessionPersistence,
        clock: &'clock mut dyn ProductSessionApiClock,
        execution: &'execution ProductSessionExecutionConfig,
        output_gate: &crate::CredentialLeakGate,
    ) -> Self {
        Self {
            service: ProductSessionService::with_output_gate(storage, output_gate),
            clock,
            execution,
        }
    }

    /// Executes `session.create` and returns its exact generated response.
    ///
    /// # Errors
    ///
    /// Returns the canonical `ProductSession` service error for an invalid,
    /// conflicting, or non-durable command.
    pub fn create(
        &mut self,
        command: SessionCreateCommand,
    ) -> Result<SessionCreateCompletedResponse, ProductSessionServiceError> {
        let schema_version = command.schema_version.clone();
        let request_id = command.request_id.clone();
        let previous_revision = command.expected_revision.clone();
        let event_id = deterministic_event_id(
            "session.create",
            &command.actor,
            &command.scope,
            &request_id,
        )?;
        let occurred_at = self.clock.now();
        let receipt = self.service.create(&CreateProductSessionCommand::from_api(
            command,
            event_id,
            occurred_at,
        )?)?;
        let result = receipt.record.projection()?;
        Ok(SessionCreateCompletedResponse {
            command: SessionCreateCompletedResponseCommand::SessionCreate,
            current_revision: result.revision.clone(),
            outcome: SessionCreateCompletedResponseOutcome::Completed,
            previous_revision,
            request_id,
            result,
            schema_version,
        })
    }

    /// Executes `chat.submit` and returns its exact generated response.
    ///
    /// # Errors
    ///
    /// Returns the canonical `ProductSession` service error for an invalid,
    /// conflicting, or non-durable command.
    pub fn submit_chat(
        &mut self,
        command: ChatSubmitCommand,
    ) -> Result<ChatSubmitCompletedResponse, ProductSessionServiceError> {
        let schema_version = command.schema_version.clone();
        let request_id = command.request_id.clone();
        let previous_revision = command.expected_revision.clone();
        let event_id =
            deterministic_event_id("chat.submit", &command.actor, &command.scope, &request_id)?;
        let occurred_at = self.clock.now();
        let receipt = self
            .service
            .submit_chat(&SubmitChatMessageCommand::from_api(
                command,
                event_id,
                occurred_at,
                self.execution,
            )?)?;
        let result = receipt.mutation.record.projection()?;
        Ok(ChatSubmitCompletedResponse {
            command: ChatSubmitCompletedResponseCommand::ChatSubmit,
            current_revision: result.revision.clone(),
            outcome: ChatSubmitCompletedResponseOutcome::Completed,
            previous_revision,
            request_id,
            result,
            schema_version,
        })
    }

    /// Executes `session.cancel` and returns its exact generated response.
    ///
    /// # Errors
    ///
    /// Returns the canonical `ProductSession` service error for stale authority,
    /// actor mismatch, conflict, or a non-durable command.
    pub fn cancel(
        &mut self,
        command: SessionCancelCommand,
    ) -> Result<SessionCancelCompletedResponse, ProductSessionServiceError> {
        let schema_version = command.schema_version.clone();
        let request_id = command.request_id.clone();
        let previous_revision = command.expected_revision.clone();
        let event_id = deterministic_event_id(
            "session.cancel",
            &command.actor,
            &command.scope,
            &request_id,
        )?;
        let occurred_at = self.clock.now();
        let receipt = self
            .service
            .cancel_session(&CancelProductSessionCommand::from_api(
                command,
                event_id,
                occurred_at,
            )?)?;
        let result = receipt.mutation.record.projection()?;
        Ok(SessionCancelCompletedResponse {
            command: SessionCancelCompletedResponseCommand::SessionCancel,
            current_revision: result.revision.clone(),
            outcome: SessionCancelCompletedResponseOutcome::Completed,
            previous_revision,
            request_id,
            result,
            schema_version,
        })
    }

    /// Executes `session.close` independently from cancellation.
    ///
    /// # Errors
    ///
    /// Returns the canonical `ProductSession` service error for an invalid,
    /// stale, conflicting, or non-durable command.
    pub fn close(
        &mut self,
        command: SessionCloseCommand,
    ) -> Result<SessionCloseCompletedResponse, ProductSessionServiceError> {
        let schema_version = command.schema_version.clone();
        let request_id = command.request_id.clone();
        let previous_revision = command.expected_revision.clone();
        let event_id =
            deterministic_event_id("session.close", &command.actor, &command.scope, &request_id)?;
        let occurred_at = self.clock.now();
        let receipt = self.service.close(&CloseProductSessionCommand::from_api(
            command,
            event_id,
            occurred_at,
        )?)?;
        let result = receipt.record.projection()?;
        Ok(SessionCloseCompletedResponse {
            command: SessionCloseCompletedResponseCommand::SessionClose,
            current_revision: result.revision.clone(),
            outcome: SessionCloseCompletedResponseOutcome::Completed,
            previous_revision,
            request_id,
            result,
            schema_version,
        })
    }

    /// Executes `session.get` against the canonical durable aggregate.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope/page facts, unknown sessions, corrupt state, and
    /// storage failures.
    pub fn get(
        &self,
        query: SessionGetQuery,
    ) -> Result<SessionGetResultResponse, ProductSessionServiceError> {
        let page = ProductSessionPageRequest::try_from(query.page)?;
        if page.cursor.is_some() {
            return Err(service_error(
                ProductSessionServiceErrorCode::CursorInvalid,
                "session.get does not accept a pagination cursor",
            ));
        }
        let scope = query_scope(&query.actor, &query.scope, query.request_id.clone())?;
        let record = self
            .service
            .get(&scope, &query.parameters.product_session_id)?
            .ok_or_else(super::not_found)?;
        Ok(SessionGetResultResponse {
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            query: SessionGetResultResponseQuery::SessionGet,
            request_id: query.request_id,
            result: record.projection()?,
            schema_version: query.schema_version,
        })
    }

    /// Executes `session.list` with stable scope/filter-bound pagination.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope, state filters, page facts, corrupt state, and
    /// storage failures.
    pub fn list(
        &self,
        query: SessionListQuery,
    ) -> Result<SessionListResultResponse, ProductSessionServiceError> {
        let scope = query_scope(&query.actor, &query.scope, query.request_id.clone())?;
        let page = ProductSessionPageRequest::try_from(query.page)?;
        let states = product_session_state_filters(&query.parameters.states)?;
        let (result, page) = self.service.list_page(&scope, &states, &page)?.into_api()?;
        Ok(SessionListResultResponse {
            page,
            query: SessionListResultResponseQuery::SessionList,
            request_id: query.request_id,
            result,
            schema_version: query.schema_version,
        })
    }

    /// Executes `session.messages.list` against the public-only message ledger.
    ///
    /// # Errors
    ///
    /// Rejects invalid scope/page facts, unknown sessions, corrupt state, and
    /// storage failures.
    pub fn messages(
        &self,
        query: SessionMessagesListQuery,
    ) -> Result<SessionMessagesListResultResponse, ProductSessionServiceError> {
        let scope = query_scope(&query.actor, &query.scope, query.request_id.clone())?;
        let page = ProductSessionPageRequest::try_from(query.page)?;
        let (result, page) = self
            .service
            .messages_page(&scope, &query.parameters.product_session_id, &page)?
            .into_api();
        Ok(SessionMessagesListResultResponse {
            page,
            query: SessionMessagesListResultResponseQuery::SessionMessagesList,
            request_id: query.request_id,
            result,
            schema_version: query.schema_version,
        })
    }
}

pub(crate) fn deterministic_event_id(
    operation: &str,
    actor: &Actor,
    scope: &RepositoryScope,
    request_id: &RequestId,
) -> Result<ControlPlaneEventId, ProductSessionServiceError> {
    let bytes = serde_json::to_vec(&(
        "winwincode.product-session.api-event.v1",
        operation,
        actor,
        scope,
        request_id,
    ))
    .map_err(|error| {
        corrupt(format!(
            "ProductSession event identity cannot be encoded: {error}"
        ))
    })?;
    let encoded = format!("{:X}", Sha256::digest(bytes));
    Ok(ControlPlaneEventId(format!("evt_{}", &encoded[..26])))
}

pub(crate) fn query_scope(
    actor: &Actor,
    scope: &RepositoryScope,
    request_id: RequestId,
) -> Result<ReceiptScopeKey, ProductSessionServiceError> {
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
    public_receipt_identity(&public_actor, &public_scope, request_id)
        .map(|identity| identity.scope_key().clone())
        .map_err(|error| storage_error(&error))
}
