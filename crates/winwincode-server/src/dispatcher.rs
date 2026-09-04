// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use serde_json::Value;
use winwincode_api::generated::{
    Actor, CommandAcceptedResponse, CommandCompletedEnvelope, CommandCompletedResponse,
    CommandEnvelope, CommandName, CommandRequest, ControlPlaneWebSocketClientFrame, QueryEnvelope,
    QueryName, QueryRequest, QueryResultEnvelope, QueryResultResponse, Scope,
};

use crate::{ApiError, AuthenticatedPrincipal, ControlPlaneApiPort, EventSubscription};

/// Product-owned command application selected from the generated command enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandFamily {
    Session,
    Delivery,
    Settings,
    CredentialReference,
    Approval,
    Worker,
    Publication,
    Enterprise,
    Collaboration,
    ProviderAccount,
}

impl CommandFamily {
    #[must_use]
    pub const fn from_name(name: &CommandName) -> Self {
        match name {
            CommandName::SessionCreate
            | CommandName::ChatSubmit
            | CommandName::InputRespond
            | CommandName::SessionCancel
            | CommandName::SessionClose => Self::Session,
            CommandName::DeliveryCreate
            | CommandName::DeliveryUpdateSpec
            | CommandName::DeliveryApproveTaskBreakdown
            | CommandName::DeliveryAdvance
            | CommandName::DeliveryResolveAttention
            | CommandName::DeliverySubmitVerdict => Self::Delivery,
            CommandName::SettingsUpdate => Self::Settings,
            CommandName::CredentialReferenceCreate
            | CommandName::CredentialReferenceRotate
            | CommandName::CredentialReferenceRevoke
            | CommandName::CredentialReferenceDelete => Self::CredentialReference,
            CommandName::ApprovalDecide => Self::Approval,
            CommandName::WorkerDrain | CommandName::WorkerEnable => Self::Worker,
            CommandName::PublicationPublish | CommandName::PublicationCancel => Self::Publication,
            CommandName::EnterpriseOrganizationUpdate
            | CommandName::EnterpriseMembershipUpdate
            | CommandName::EnterpriseTeamUpdate
            | CommandName::EnterpriseRoleUpdate
            | CommandName::EnterpriseProjectRepositoryUpdate
            | CommandName::EnterprisePolicyUpdate
            | CommandName::EnterpriseFleetUpdate
            | CommandName::EnterpriseIntegrationUpdate
            | CommandName::EnterpriseIdentityUpdate => Self::Enterprise,
            CommandName::CollaborationNotificationAck
            | CommandName::CollaborationPresenceUpdate => Self::Collaboration,
            CommandName::ProviderAccountConnectionStart
            | CommandName::ProviderAccountConnectionComplete
            | CommandName::ProviderAccountConnectionRefresh
            | CommandName::ProviderAccountConnectionRevoke
            | CommandName::ProviderAccountPoolUpsert
            | CommandName::ProviderAccountPoolDisable => Self::ProviderAccount,
        }
    }
}

/// Product-owned query application selected from the generated query enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryFamily {
    Session,
    Runtime,
    Delivery,
    Settings,
    CredentialReference,
    Approval,
    Worker,
    Publication,
    Enterprise,
    Collaboration,
    ProviderAccount,
}

impl QueryFamily {
    #[must_use]
    pub const fn from_name(name: &QueryName) -> Self {
        match name {
            QueryName::SessionList
            | QueryName::SessionGet
            | QueryName::SessionMessagesList
            | QueryName::SessionInteractionsList => Self::Session,
            QueryName::RuntimeProjectionGet => Self::Runtime,
            QueryName::DeliveryList | QueryName::DeliveryGet => Self::Delivery,
            QueryName::SettingsGet => Self::Settings,
            QueryName::CredentialReferenceList | QueryName::CredentialReferenceGet => {
                Self::CredentialReference
            }
            QueryName::ApprovalList | QueryName::ApprovalGet => Self::Approval,
            QueryName::WorkerList | QueryName::WorkerGet => Self::Worker,
            QueryName::PublicationList | QueryName::PublicationGet => Self::Publication,
            QueryName::EnterpriseOrganizationList
            | QueryName::EnterpriseMembershipList
            | QueryName::EnterpriseTeamList
            | QueryName::EnterpriseRoleList
            | QueryName::EnterpriseProjectList
            | QueryName::EnterprisePolicyList
            | QueryName::EnterpriseFleetList
            | QueryName::EnterpriseUsageList
            | QueryName::EnterpriseAuditList
            | QueryName::EnterpriseIntegrationList
            | QueryName::EnterpriseIdentityList => Self::Enterprise,
            QueryName::CollaborationActivityList
            | QueryName::CollaborationNotificationList
            | QueryName::CollaborationPresenceList => Self::Collaboration,
            QueryName::ProviderAccountConnectionList
            | QueryName::ProviderAccountConnectionGet
            | QueryName::ProviderAccountPoolList
            | QueryName::ProviderAccountPoolGet => Self::ProviderAccount,
        }
    }
}

/// Exact generated response forms accepted from a command application.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandDispatchResponse {
    Accepted(CommandAcceptedResponse),
    Completed(Box<CommandCompletedResponse>),
}

/// Typed application boundary behind the public HTTP/WebSocket listener.
///
/// Implementations authorize a generated scope before a handler is called.
/// Business validation, persistence, and idempotency remain in the selected
/// Control Plane application.
pub trait TypedControlPlaneApiPort: Send + Sync {
    /// Reports whether the application and its supervised execution runtime are healthy.
    ///
    /// Implementations keep this check synchronous and side-effect free. A
    /// runtime supervisor may override it to fail closed after any worker,
    /// launcher, or durable-outbox task exits unexpectedly.
    ///
    /// # Errors
    ///
    /// Returns an error when the application or supervised runtime is faulted.
    fn health(&self) -> Result<(), ApiError> {
        Ok(())
    }

    /// # Errors
    ///
    /// Rejects a principal that has no authority over the exact generated scope.
    fn authorize_scope(
        &self,
        principal: &AuthenticatedPrincipal,
        scope: &Scope,
    ) -> Result<(), ApiError>;

    /// # Errors
    ///
    /// Returns a typed application error without transport or credential data.
    fn command(
        &self,
        principal: &AuthenticatedPrincipal,
        family: CommandFamily,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError>;

    /// # Errors
    ///
    /// Returns a typed application error without transport or credential data.
    fn query(
        &self,
        principal: &AuthenticatedPrincipal,
        family: QueryFamily,
        request: QueryRequest,
    ) -> Result<QueryResultResponse, ApiError>;

    /// # Errors
    ///
    /// Rejects an invalid or unauthorized generated subscription frame.
    fn subscribe(
        &self,
        principal: &AuthenticatedPrincipal,
        first_frame: ControlPlaneWebSocketClientFrame,
    ) -> Result<EventSubscription, ApiError>;

    /// # Errors
    ///
    /// Rejects an invalid or unauthorized generated control frame.
    fn event_control(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: ControlPlaneWebSocketClientFrame,
    ) -> Result<Vec<Value>, ApiError>;

    /// # Errors
    ///
    /// Returns a redacted application shutdown failure.
    fn shutdown(&self) -> Result<(), ApiError>;
}

/// Strict generated-contract adapter used by the one public listener.
pub struct GeneratedContractDispatcher {
    application: Arc<dyn TypedControlPlaneApiPort>,
}

impl GeneratedContractDispatcher {
    #[must_use]
    pub fn new(application: Arc<dyn TypedControlPlaneApiPort>) -> Self {
        Self { application }
    }
}

impl ControlPlaneApiPort for GeneratedContractDispatcher {
    fn health(&self) -> Result<(), ApiError> {
        self.application.health()
    }

    fn command(
        &self,
        principal: &AuthenticatedPrincipal,
        request: Value,
    ) -> Result<Value, ApiError> {
        let envelope: CommandEnvelope = decode(request.clone())?;
        validate_actor(principal, &envelope.actor)?;
        validate_session_scope(principal, &envelope.scope)?;
        let typed: CommandRequest = decode(request)?;
        self.application
            .authorize_scope(principal, &envelope.scope)?;
        let response = self.application.command(
            principal,
            CommandFamily::from_name(&envelope.command),
            typed,
        )?;
        encode_command_response(response, &envelope)
    }

    fn query(&self, principal: &AuthenticatedPrincipal, request: Value) -> Result<Value, ApiError> {
        let envelope: QueryEnvelope = decode(request.clone())?;
        validate_actor(principal, &envelope.actor)?;
        validate_session_scope(principal, &envelope.scope)?;
        let typed: QueryRequest = decode(request)?;
        self.application
            .authorize_scope(principal, &envelope.scope)?;
        let response =
            self.application
                .query(principal, QueryFamily::from_name(&envelope.query), typed)?;
        encode_query_response(response, &envelope)
    }

    fn subscribe(
        &self,
        principal: &AuthenticatedPrincipal,
        first_frame: Value,
    ) -> Result<EventSubscription, ApiError> {
        let typed: ControlPlaneWebSocketClientFrame = decode_frame(first_frame)?;
        let scope = initial_frame_scope(&typed).ok_or_else(|| {
            ApiError::new(
                400,
                "SUBSCRIPTION_REQUIRED",
                "first WebSocket frame must subscribe or resume",
            )
        })?;
        validate_session_scope(principal, scope)?;
        self.application.authorize_scope(principal, scope)?;
        self.application.subscribe(principal, typed)
    }

    fn event_control(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: Value,
    ) -> Result<Vec<Value>, ApiError> {
        let typed: ControlPlaneWebSocketClientFrame = decode_frame(frame)?;
        if let Some(scope) = control_frame_scope(&typed)? {
            validate_session_scope(principal, scope)?;
            self.application.authorize_scope(principal, scope)?;
        }
        self.application.event_control(principal, typed)
    }

    fn shutdown(&self) -> Result<(), ApiError> {
        self.application.shutdown()
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| {
        ApiError::new(
            400,
            "INVALID_REQUEST",
            "request does not match the generated Control Plane contract",
        )
    })
}

fn decode_frame<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| {
        ApiError::new(
            400,
            "INVALID_FRAME",
            "frame does not match the generated Control Plane contract",
        )
    })
}

fn validate_actor(principal: &AuthenticatedPrincipal, actor: &Actor) -> Result<(), ApiError> {
    if principal.actor() == actor {
        return Ok(());
    }
    Err(ApiError::new(
        403,
        "PERMISSION_DENIED",
        "authenticated identity does not match the request actor",
    ))
}

fn validate_session_scope(
    principal: &AuthenticatedPrincipal,
    scope: &Scope,
) -> Result<(), ApiError> {
    if principal.authorizes(scope) {
        return Ok(());
    }
    Err(ApiError::new(
        403,
        "PERMISSION_DENIED",
        "authenticated session does not authorize the request scope",
    ))
}

fn encode_command_response(
    response: CommandDispatchResponse,
    request: &CommandEnvelope,
) -> Result<Value, ApiError> {
    match response {
        CommandDispatchResponse::Accepted(response) => {
            if response.request_id != request.request_id
                || response.command != request.command
                || response.schema_version != request.schema_version
            {
                return Err(invalid_application_response());
            }
            serde_json::to_value(response).map_err(|_| invalid_application_response())
        }
        CommandDispatchResponse::Completed(response) => {
            let value =
                serde_json::to_value(response).map_err(|_| invalid_application_response())?;
            let response: CommandCompletedEnvelope = decode_application_response(value.clone())?;
            if response.request_id != request.request_id
                || response.command != request.command
                || response.schema_version != request.schema_version
            {
                return Err(invalid_application_response());
            }
            Ok(value)
        }
    }
}

fn encode_query_response(
    response: QueryResultResponse,
    request: &QueryEnvelope,
) -> Result<Value, ApiError> {
    let value = serde_json::to_value(response).map_err(|_| invalid_application_response())?;
    let response: QueryResultEnvelope = decode_application_response(value.clone())?;
    if response.request_id != request.request_id
        || response.query != request.query
        || response.schema_version != request.schema_version
    {
        return Err(invalid_application_response());
    }
    Ok(value)
}

fn decode_application_response<T: serde::de::DeserializeOwned>(
    value: Value,
) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| invalid_application_response())
}

fn invalid_application_response() -> ApiError {
    ApiError::new(
        500,
        "APPLICATION_RESPONSE_INVALID",
        "application response does not match the request",
    )
}

fn initial_frame_scope(frame: &ControlPlaneWebSocketClientFrame) -> Option<&Scope> {
    match frame {
        ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketSubscribeFrame(frame) => {
            Some(&frame.subscription.scope)
        }
        ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketResumeFrame(frame) => {
            Some(&frame.subscription.scope)
        }
        ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketAckFrame(_)
        | ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketPongFrame(_) => None,
    }
}

fn control_frame_scope(
    frame: &ControlPlaneWebSocketClientFrame,
) -> Result<Option<&Scope>, ApiError> {
    match frame {
        ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketAckFrame(frame) => {
            Ok(Some(&frame.cursor.scope))
        }
        ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketPongFrame(_) => Ok(None),
        ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketSubscribeFrame(_)
        | ControlPlaneWebSocketClientFrame::ControlPlaneWebSocketResumeFrame(_) => {
            Err(ApiError::new(
                409,
                "WRONG_STATE",
                "subscribe and resume require a new WebSocket connection",
            ))
        }
    }
}
