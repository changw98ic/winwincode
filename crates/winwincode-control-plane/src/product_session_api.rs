use super::product_session_command_context;
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
use winwincode_api::generated::{
    SessionUpdateCommand, SessionUpdateCompletedResponse, SessionUpdateCompletedResponseCommand,
    SessionUpdateCompletedResponseOutcome,
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

pub trait ProductSessionApiClock {
    fn now(&mut self) -> Instant;
}

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

    pub fn update_metadata(
        &mut self,
        command: SessionUpdateCommand,
    ) -> Result<SessionUpdateCompletedResponse, ProductSessionServiceError> {
        let event_id = deterministic_event_id(
            "session.update",
            &command.actor,
            &command.scope,
            &command.request_id,
        )?;
        let context = product_session_command_context(
            &command.actor,
            &command.scope,
            command.request_id.clone(),
            &command.expected_revision,
            event_id,
            self.clock.now(),
        )?;
        let receipt = self.service.update_metadata(
            &context,
            &command.payload.product_session_id,
            &command.payload.title,
            command.payload.archived,
        )?;
        let result = receipt.record.projection()?;
        Ok(SessionUpdateCompletedResponse {
            command: SessionUpdateCompletedResponseCommand::SessionUpdate,
            current_revision: result.revision.clone(),
            outcome: SessionUpdateCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision,
            request_id: command.request_id,
            result,
            schema_version: command.schema_version,
        })
    }

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
