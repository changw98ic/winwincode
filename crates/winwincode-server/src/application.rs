// SPDX-License-Identifier: Apache-2.0

//! Production application registry behind the generated HTTP and WebSocket dispatcher.
//!
//! The registry owns no second business model. Product services and the
//! `ControlPlane` use separate connections to the same authoritative `SQLite`
//! database, while the durable event hub owns transport cursors only.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use winwincode_api::generated::{
    CommandCompletedResponse, CommandRequest, ControlPlaneWebSocketClientFrame, ErrorCode,
    QueryRequest, QueryResultResponse, Scope,
};
use winwincode_control_plane::credential_reference::{
    CredentialReferenceError, CredentialReferenceErrorKind, CredentialReferenceService,
};
use winwincode_control_plane::strongflow_projection::{
    StrongFlowProjectionError, StrongFlowProjectionQueryPort,
};
use winwincode_control_plane::{
    ChatInteractionApiService, ChatInteractionServiceError, ChatInteractionServiceErrorCode,
    CollaborationClock, CollaborationClockError, CollaborationError, CollaborationErrorKind,
    CollaborationService, ControlPlane, DeliveryApplicationError, DurableWorkerInteractionOutbound,
    EnterpriseRbacService, ModelRequestPoolConfig, ModelRouteAvailabilityError,
    ModelRouteAvailabilityErrorKind, ModelRouteAvailabilityService, ModelSettingsError,
    ModelSettingsErrorKind, ModelSettingsService, ProductSessionApiClock, ProductSessionApiService,
    ProductSessionExecutionConfig, ProductSessionServiceError, ProductSessionServiceErrorCode,
    PublicationCommandError, ScopeWorkerHealthEventPort, WorkerManagementService,
    WorkerManagementServiceError, WorkerManagementServiceErrorKind,
};
use winwincode_domain::{ControlPlaneWebSocketAuthorizationEpoch, Instant};
use winwincode_storage::{ProductStateStorage, SqliteStorage};

use crate::{
    ApiError, AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, DurableEventHub,
    EnterpriseManagementApplicationPort, EventSubscription, HealthyRuntimeHealth, QueryFamily,
    RuntimeHealthPort, TypedControlPlaneApiPort, UnavailableEnterpriseManagementApplication,
};

const AUTHORIZATION_EPOCH: i64 = 1;

/// Time source used only at application-command and liveness boundaries.
pub trait StandaloneApplicationClock: Send + Sync {
    #[must_use]
    fn now_millis(&self) -> u64;

    #[must_use]
    fn now_instant(&self) -> Instant;
}

/// System clock used by the production composition.
pub struct SystemStandaloneApplicationClock;

impl StandaloneApplicationClock for SystemStandaloneApplicationClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    }

    fn now_instant(&self) -> Instant {
        millis_to_instant(self.now_millis())
    }
}

struct ProductSessionClockAdapter<'clock>(&'clock dyn StandaloneApplicationClock);

impl ProductSessionApiClock for ProductSessionClockAdapter<'_> {
    fn now(&mut self) -> Instant {
        self.0.now_instant()
    }
}

struct CollaborationClockAdapter(Arc<dyn StandaloneApplicationClock>);

impl CollaborationClock for CollaborationClockAdapter {
    fn now_millis(&mut self) -> Result<u64, CollaborationClockError> {
        Ok(self.0.now_millis())
    }
}

pub(crate) struct ApplicationState {
    pub(crate) control_plane: ControlPlane,
    pub(crate) storage: SqliteStorage,
    pub(crate) worker_outbound: DurableWorkerInteractionOutbound,
    /// Immutable startup execution authority shared by every Chat command and
    /// by the supervised Worker runtime. It is resolved once at composition
    /// time; request handlers never inspect the checkout or create defaults.
    pub(crate) execution_config: ProductSessionExecutionConfig,
    /// Immutable startup bounds used to interpret the durable request-pool
    /// authority for the secret-safe `ModelRoute` availability projection.
    pub(crate) model_request_pool_config: Option<ModelRequestPoolConfig>,
}

struct ApplicationComposition {
    enterprise: Arc<dyn EnterpriseManagementApplicationPort>,
    collaboration: Arc<CollaborationService>,
    execution_config: ProductSessionExecutionConfig,
}

/// One production registry for generated HTTP commands, queries, and WS frames.
pub struct StandaloneControlPlaneApplication {
    state: Arc<Mutex<Option<ApplicationState>>>,
    hub: Arc<DurableEventHub>,
    clock: Arc<dyn StandaloneApplicationClock>,
    enterprise: Arc<dyn EnterpriseManagementApplicationPort>,
    collaboration: Arc<CollaborationService>,
    runtime: Arc<dyn RuntimeHealthPort>,
}

impl StandaloneControlPlaneApplication {
    /// Composes already-open product services around one `SQLite` authority.
    ///
    /// The service and Worker-outbound connections are rejected unless both
    /// name the canonical local database owned by the supplied `ControlPlane`.
    ///
    /// # Errors
    ///
    /// Rejects any different product-state database.
    pub fn new(
        control_plane: ControlPlane,
        storage: SqliteStorage,
        worker_outbound: DurableWorkerInteractionOutbound,
        hub: Arc<DurableEventHub>,
        execution_config: ProductSessionExecutionConfig,
    ) -> Result<Self, ApiError> {
        Self::new_with_clock_and_enterprise(
            control_plane,
            storage,
            worker_outbound,
            hub,
            Arc::new(SystemStandaloneApplicationClock),
            Arc::new(UnavailableEnterpriseManagementApplication),
            execution_config,
        )
    }

    /// Composes a canonical enterprise application behind the same generated dispatcher.
    ///
    /// # Errors
    ///
    /// Rejects the same invalid local composition as [`Self::new`].
    pub fn new_with_enterprise(
        control_plane: ControlPlane,
        storage: SqliteStorage,
        worker_outbound: DurableWorkerInteractionOutbound,
        hub: Arc<DurableEventHub>,
        enterprise: Arc<dyn EnterpriseManagementApplicationPort>,
        execution_config: ProductSessionExecutionConfig,
    ) -> Result<Self, ApiError> {
        Self::new_with_clock_and_enterprise(
            control_plane,
            storage,
            worker_outbound,
            hub,
            Arc::new(SystemStandaloneApplicationClock),
            enterprise,
            execution_config,
        )
    }

    /// Composes enterprise and collaboration services that share the same
    /// durable authority and current RBAC service.
    ///
    /// # Errors
    ///
    /// Rejects the same invalid local composition as [`Self::new`].
    pub fn new_with_enterprise_and_collaboration(
        control_plane: ControlPlane,
        storage: SqliteStorage,
        worker_outbound: DurableWorkerInteractionOutbound,
        hub: Arc<DurableEventHub>,
        enterprise: Arc<dyn EnterpriseManagementApplicationPort>,
        collaboration: Arc<CollaborationService>,
        execution_config: ProductSessionExecutionConfig,
    ) -> Result<Self, ApiError> {
        Self::compose(
            control_plane,
            storage,
            worker_outbound,
            hub,
            Arc::new(SystemStandaloneApplicationClock),
            ApplicationComposition {
                enterprise,
                collaboration,
                execution_config,
            },
        )
    }

    /// Same composition with an injected clock for deterministic tests.
    ///
    /// # Errors
    ///
    /// Rejects a service connection not opened on the expected authoritative database path.
    pub fn new_with_clock(
        control_plane: ControlPlane,
        storage: SqliteStorage,
        worker_outbound: DurableWorkerInteractionOutbound,
        hub: Arc<DurableEventHub>,
        clock: Arc<dyn StandaloneApplicationClock>,
        execution_config: ProductSessionExecutionConfig,
    ) -> Result<Self, ApiError> {
        Self::new_with_clock_and_enterprise(
            control_plane,
            storage,
            worker_outbound,
            hub,
            clock,
            Arc::new(UnavailableEnterpriseManagementApplication),
            execution_config,
        )
    }

    /// Deterministic composition with an injected enterprise application port.
    ///
    /// # Errors
    ///
    /// Rejects a service connection not opened on the expected authoritative database path.
    pub fn new_with_clock_and_enterprise(
        control_plane: ControlPlane,
        storage: SqliteStorage,
        worker_outbound: DurableWorkerInteractionOutbound,
        hub: Arc<DurableEventHub>,
        clock: Arc<dyn StandaloneApplicationClock>,
        enterprise: Arc<dyn EnterpriseManagementApplicationPort>,
        execution_config: ProductSessionExecutionConfig,
    ) -> Result<Self, ApiError> {
        let data_directory = storage
            .database_path()
            .parent()
            .ok_or_else(application_configuration_invalid)?;
        let rbac = Arc::new(EnterpriseRbacService::new(Box::new(
            SqliteStorage::open(data_directory).map_err(|_| application_configuration_invalid())?,
        )));
        let collaboration = Arc::new(CollaborationService::with_clock(
            SqliteStorage::open(data_directory).map_err(|_| application_configuration_invalid())?,
            rbac,
            Box::new(CollaborationClockAdapter(Arc::clone(&clock))),
        ));
        Self::compose(
            control_plane,
            storage,
            worker_outbound,
            hub,
            clock,
            ApplicationComposition {
                enterprise,
                collaboration,
                execution_config,
            },
        )
    }

    fn compose(
        control_plane: ControlPlane,
        storage: SqliteStorage,
        worker_outbound: DurableWorkerInteractionOutbound,
        hub: Arc<DurableEventHub>,
        clock: Arc<dyn StandaloneApplicationClock>,
        composition: ApplicationComposition,
    ) -> Result<Self, ApiError> {
        if control_plane.local_database_path() != Some(storage.database_path())
            || worker_outbound.database_path() != storage.database_path()
            || composition.collaboration.database_path() != storage.database_path()
        {
            return Err(application_configuration_invalid());
        }
        Ok(Self {
            state: Arc::new(Mutex::new(Some(ApplicationState {
                control_plane,
                storage,
                worker_outbound,
                execution_config: composition.execution_config,
                model_request_pool_config: None,
            }))),
            hub,
            clock,
            enterprise: composition.enterprise,
            collaboration: composition.collaboration,
            runtime: Arc::new(HealthyRuntimeHealth),
        })
    }

    /// Attaches the sole supervised runtime health source to this application.
    ///
    /// The handle is read synchronously by the HTTP health endpoint; runtime
    /// lifecycle remains owned by the composition root and is never started
    /// from a synchronous application callback.
    #[must_use]
    pub fn with_runtime_health(mut self, runtime: Arc<dyn RuntimeHealthPort>) -> Self {
        self.runtime = runtime;
        self
    }

    /// Attaches the same immutable request-pool bounds used by the supervised
    /// model execution runtime.
    ///
    /// # Errors
    ///
    /// Returns service unavailable if the application state lock is poisoned
    /// or the application has already been shut down.
    pub fn with_model_request_pool_config(
        self,
        config: ModelRequestPoolConfig,
    ) -> Result<Self, ApiError> {
        {
            let mut guard = self.state()?;
            let state = guard.as_mut().ok_or_else(service_unavailable)?;
            state.model_request_pool_config = Some(config);
        }
        Ok(self)
    }

    pub(crate) fn shared_runtime_state(&self) -> Arc<Mutex<Option<ApplicationState>>> {
        Arc::clone(&self.state)
    }

    pub(crate) fn runtime_clock(&self) -> Arc<dyn StandaloneApplicationClock> {
        Arc::clone(&self.clock)
    }

    pub(crate) fn runtime_hub(&self) -> Arc<DurableEventHub> {
        Arc::clone(&self.hub)
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, Option<ApplicationState>>, ApiError> {
        self.state.lock().map_err(|_| service_unavailable())
    }

    fn credential_command(
        &self,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        let now_millis = self.clock.now_millis();
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let response = match request {
            CommandRequest::CredentialReferenceCreateCommand(command) => {
                let response = CredentialReferenceService::new(&mut state.storage)
                    .create(&command, now_millis)
                    .map_err(|error| credential_error(&error))?;
                CommandCompletedResponse::CredentialReferenceCreateCompletedResponse(response)
            }
            CommandRequest::CredentialReferenceRotateCommand(command) => {
                let response = CredentialReferenceService::new(&mut state.storage)
                    .rotate(&command, now_millis)
                    .map_err(|error| credential_error(&error))?;
                CommandCompletedResponse::CredentialReferenceRotateCompletedResponse(response)
            }
            CommandRequest::CredentialReferenceRevokeCommand(command) => {
                let response = CredentialReferenceService::new(&mut state.storage)
                    .revoke(&command, now_millis)
                    .map_err(|error| credential_error(&error))?;
                CommandCompletedResponse::CredentialReferenceRevokeCompletedResponse(response)
            }
            CommandRequest::CredentialReferenceDeleteCommand(command) => {
                let response = CredentialReferenceService::new(&mut state.storage)
                    .delete(&command, now_millis)
                    .map_err(|error| credential_error(&error))?;
                CommandCompletedResponse::CredentialReferenceDeleteCompletedResponse(response)
            }
            _ => return Err(application_variant_mismatch()),
        };
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| error.api_error())?;
        Ok(CommandDispatchResponse::Completed(Box::new(response)))
    }

    fn credential_query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        match request {
            QueryRequest::CredentialReferenceGetQuery(query) => {
                CredentialReferenceService::new(&mut state.storage)
                    .get(&query)
                    .map(QueryResultResponse::CredentialReferenceGetResultResponse)
                    .map_err(|error| credential_error(&error))
            }
            QueryRequest::CredentialReferenceListQuery(query) => {
                CredentialReferenceService::new(&mut state.storage)
                    .list(&query)
                    .map(QueryResultResponse::CredentialReferenceListResultResponse)
                    .map_err(|error| credential_error(&error))
            }
            _ => Err(application_variant_mismatch()),
        }
    }

    fn session_command(
        &self,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let response = {
            let mut clock = ProductSessionClockAdapter(self.clock.as_ref());
            let mut service = ProductSessionApiService::new(
                &mut state.storage,
                &mut clock,
                &state.execution_config,
            );
            match request {
                CommandRequest::SessionCreateCommand(command) => service
                    .create(command)
                    .map(CommandCompletedResponse::SessionCreateCompletedResponse),
                CommandRequest::ChatSubmitCommand(command) => service
                    .submit_chat(command)
                    .map(CommandCompletedResponse::ChatSubmitCompletedResponse),
                CommandRequest::SessionCancelCommand(command) => service
                    .cancel(command)
                    .map(CommandCompletedResponse::SessionCancelCompletedResponse),
                CommandRequest::SessionCloseCommand(command) => service
                    .close(command)
                    .map(CommandCompletedResponse::SessionCloseCompletedResponse),
                _ => return Err(application_variant_mismatch()),
            }
        }
        .map_err(|error| product_session_error(&error))?;
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| error.api_error())?;
        Ok(CommandDispatchResponse::Completed(Box::new(response)))
    }

    fn session_query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let mut clock = ProductSessionClockAdapter(self.clock.as_ref());
        let service =
            ProductSessionApiService::new(&mut state.storage, &mut clock, &state.execution_config);
        match request {
            QueryRequest::SessionGetQuery(query) => service
                .get(query)
                .map(QueryResultResponse::SessionGetResultResponse),
            QueryRequest::SessionListQuery(query) => service
                .list(query)
                .map(QueryResultResponse::SessionListResultResponse),
            QueryRequest::SessionMessagesListQuery(query) => service
                .messages(query)
                .map(QueryResultResponse::SessionMessagesListResultResponse),
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| product_session_error(&error))
    }

    fn interaction_command(
        &self,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let response = {
            let mut clock = ProductSessionClockAdapter(self.clock.as_ref());
            let mut service = ChatInteractionApiService::new(
                &mut state.storage,
                &mut clock,
                &mut state.worker_outbound,
            );
            match request {
                CommandRequest::InputRespondCommand(command) => service
                    .respond_input(command)
                    .map(CommandCompletedResponse::InputRespondCompletedResponse),
                CommandRequest::ApprovalDecideCommand(command) => service
                    .decide_approval(command)
                    .map(CommandCompletedResponse::ApprovalDecideCompletedResponse),
                _ => return Err(application_variant_mismatch()),
            }
        }
        .map_err(|error| chat_interaction_error(&error))?;
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| error.api_error())?;
        Ok(CommandDispatchResponse::Completed(Box::new(response)))
    }

    fn interaction_query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let mut clock = ProductSessionClockAdapter(self.clock.as_ref());
        let mut service = ChatInteractionApiService::new(
            &mut state.storage,
            &mut clock,
            &mut state.worker_outbound,
        );
        match request {
            QueryRequest::ChatInteractionListQuery(query) => service
                .interactions(&query)
                .map(QueryResultResponse::ChatInteractionListResultResponse),
            QueryRequest::ApprovalGetQuery(query) => service
                .approval_get(&query)
                .map(QueryResultResponse::ApprovalGetResultResponse),
            QueryRequest::ApprovalListQuery(query) => service
                .approval_list(&query)
                .map(QueryResultResponse::ApprovalListResultResponse),
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| chat_interaction_error(&error))
    }

    fn delivery_command(
        &self,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let response = match request {
            CommandRequest::DeliveryCreateCommand(command) => state
                .control_plane
                .delivery_create(&command)
                .map(CommandCompletedResponse::DeliveryCreateCompletedResponse),
            CommandRequest::DeliveryUpdateSpecCommand(command) => state
                .control_plane
                .delivery_update_spec(&command)
                .map(CommandCompletedResponse::DeliveryUpdateSpecCompletedResponse),
            CommandRequest::DeliveryApproveTaskBreakdownCommand(command) => state
                .control_plane
                .delivery_approve_task_breakdown(&command)
                .map(CommandCompletedResponse::DeliveryApproveTaskBreakdownCompletedResponse),
            CommandRequest::DeliveryAdvanceCommand(command) => state
                .control_plane
                .delivery_advance(&command)
                .map(CommandCompletedResponse::DeliveryAdvanceCompletedResponse),
            CommandRequest::DeliveryResolveAttentionCommand(command) => state
                .control_plane
                .delivery_resolve_attention(&command)
                .map(CommandCompletedResponse::DeliveryResolveAttentionCompletedResponse),
            CommandRequest::DeliverySubmitVerdictCommand(command) => state
                .control_plane
                .delivery_submit_verdict(&command)
                .map(CommandCompletedResponse::DeliverySubmitVerdictCompletedResponse),
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| delivery_application_error(&error))?;
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| error.api_error())?;
        Ok(CommandDispatchResponse::Completed(Box::new(response)))
    }

    fn worker_command(&self, request: CommandRequest) -> Result<CommandDispatchResponse, ApiError> {
        let occurred_at = self.clock.now_instant();
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let events = ScopeWorkerHealthEventPort;
        let response = {
            let mut service = WorkerManagementService::new(&mut state.storage, &events);
            match request {
                CommandRequest::WorkerDrainCommand(command) => service
                    .drain(&command, &occurred_at)
                    .map(CommandCompletedResponse::WorkerDrainCompletedResponse),
                CommandRequest::WorkerEnableCommand(command) => service
                    .enable(&command, &occurred_at)
                    .map(CommandCompletedResponse::WorkerEnableCompletedResponse),
                _ => return Err(application_variant_mismatch()),
            }
        }
        .map_err(|error| worker_management_error(&error))?;
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| error.api_error())?;
        Ok(CommandDispatchResponse::Completed(Box::new(response)))
    }

    fn worker_query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        let observed_at = self.clock.now_instant();
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let events = ScopeWorkerHealthEventPort;
        let mut service = WorkerManagementService::new(&mut state.storage, &events);
        match request {
            QueryRequest::WorkerListQuery(query) => service
                .list(&query, &observed_at)
                .map(QueryResultResponse::WorkerListResultResponse),
            QueryRequest::WorkerGetQuery(query) => service
                .get(&query, &observed_at)
                .map(QueryResultResponse::WorkerGetResultResponse),
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| worker_management_error(&error))
    }

    fn publication_query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        match request {
            QueryRequest::PublicationListQuery(query) => state
                .control_plane
                .publication_list(&query)
                .map(QueryResultResponse::PublicationListResultResponse),
            QueryRequest::PublicationGetQuery(query) => state
                .control_plane
                .publication_get(&query)
                .map(QueryResultResponse::PublicationGetResultResponse),
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| publication_error(&error))
    }

    fn publication_command(
        &self,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        let occurred_at_millis = self.clock.now_millis();
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let response = match request {
            CommandRequest::PublicationPublishCommand(command) => state
                .control_plane
                .publication_publish(&command)
                .map(CommandCompletedResponse::PublicationPublishCompletedResponse),
            CommandRequest::PublicationCancelCommand(command) => state
                .control_plane
                .publication_cancel(&command, occurred_at_millis)
                .map(CommandCompletedResponse::PublicationCancelCompletedResponse),
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| publication_error(&error))?;
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| error.api_error())?;
        Ok(CommandDispatchResponse::Completed(Box::new(response)))
    }

    fn settings_command(
        &self,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        let occurred_at = self.clock.now_instant();
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let response = match request {
            CommandRequest::SettingsUpdateCommand(command) => {
                ModelSettingsService::new(&mut state.storage)
                    .update_generated(&command, occurred_at)
                    .map(CommandCompletedResponse::SettingsUpdateCompletedResponse)
            }
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| model_settings_error(&error))?;
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| error.api_error())?;
        Ok(CommandDispatchResponse::Completed(Box::new(response)))
    }

    fn settings_query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        match request {
            QueryRequest::SettingsGetQuery(query) => ModelSettingsService::new(&mut state.storage)
                .get(&query)
                .map(QueryResultResponse::SettingsGetResultResponse)
                .map_err(|error| model_settings_error(&error)),
            QueryRequest::ModelRouteAvailabilityListQuery(query) => {
                ModelRouteAvailabilityService::new(
                    &mut state.storage,
                    state.model_request_pool_config,
                )
                .list(&query)
                .map(QueryResultResponse::ModelRouteAvailabilityListResultResponse)
                .map_err(|error| model_route_availability_error(&error))
            }
            _ => Err(application_variant_mismatch()),
        }
    }

    fn collaboration_command(
        &self,
        principal: &AuthenticatedPrincipal,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        let response = match request {
            CommandRequest::CollaborationNotificationAckCommand(command) => self
                .collaboration
                .notification_ack(principal.authorized_scopes(), &command)
                .map(CommandCompletedResponse::CollaborationNotificationAckCompletedResponse),
            CommandRequest::CollaborationPresenceUpdateCommand(command) => self
                .collaboration
                .presence_update(principal.authorized_scopes(), &command)
                .map(CommandCompletedResponse::CollaborationPresenceUpdateCompletedResponse),
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| collaboration_error(&error))?;
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        self.hub
            .publish_pending(&mut state.storage)
            .map_err(|error| error.api_error())?;
        Ok(CommandDispatchResponse::Completed(Box::new(response)))
    }

    fn collaboration_query(
        &self,
        principal: &AuthenticatedPrincipal,
        request: QueryRequest,
    ) -> Result<QueryResultResponse, ApiError> {
        match request {
            QueryRequest::CollaborationActivityListQuery(query) => self
                .collaboration
                .activity_list(principal.authorized_scopes(), &query)
                .map(QueryResultResponse::CollaborationActivityListResultResponse),
            QueryRequest::CollaborationNotificationListQuery(query) => self
                .collaboration
                .notification_list(principal.authorized_scopes(), &query)
                .map(QueryResultResponse::CollaborationNotificationListResultResponse),
            QueryRequest::CollaborationPresenceListQuery(query) => self
                .collaboration
                .presence_list(principal.authorized_scopes(), &query)
                .map(QueryResultResponse::CollaborationPresenceListResultResponse),
            _ => return Err(application_variant_mismatch()),
        }
        .map_err(|error| collaboration_error(&error))
    }

    fn strongflow_query(&self, request: QueryRequest) -> Result<QueryResultResponse, ApiError> {
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        match request {
            QueryRequest::DeliveryGetQuery(query) => state
                .control_plane
                .delivery_get(&query)
                .map_err(|error| strongflow_error(&error)),
            QueryRequest::RuntimeProjectionGetQuery(query) => state
                .control_plane
                .runtime_projection_get(&query)
                .map_err(|error| strongflow_error(&error)),
            QueryRequest::CandidateFilesListQuery(query) => state
                .control_plane
                .candidate_files_list(&query)
                .map_err(|error| strongflow_error(&error)),
            QueryRequest::CandidateDiffGetQuery(query) => state
                .control_plane
                .candidate_diff_get(&query)
                .map_err(|error| strongflow_error(&error)),
            QueryRequest::CandidateHistoryListQuery(query) => state
                .control_plane
                .candidate_history_list(&query)
                .map_err(|error| strongflow_error(&error)),
            QueryRequest::CandidateHistoricalReviewGetQuery(query) => state
                .control_plane
                .candidate_historical_review_get(&query)
                .map_err(|error| strongflow_error(&error)),
            QueryRequest::EvidenceGetQuery(query) => state
                .control_plane
                .evidence_get(&query)
                .map_err(|error| strongflow_error(&error)),
            QueryRequest::EvidenceArtifactContentGetQuery(query) => state
                .control_plane
                .evidence_artifact_content_get(&query)
                .map_err(|error| strongflow_error(&error)),
            QueryRequest::DeliveryListQuery(query) => state
                .control_plane
                .delivery_list(&query)
                .map(QueryResultResponse::DeliveryListResultResponse)
                .map_err(|error| delivery_application_error(&error)),
            _ => Err(application_variant_mismatch()),
        }
    }
}

impl TypedControlPlaneApiPort for StandaloneControlPlaneApplication {
    fn health(&self) -> Result<(), ApiError> {
        if !self.runtime.is_healthy() {
            return Err(ApiError::new(
                503,
                "SERVICE_UNAVAILABLE",
                "execution runtime is unavailable",
            ));
        }
        let guard = self.state.lock().map_err(|_| service_unavailable())?;
        if guard.is_none() {
            return Err(service_unavailable());
        }
        Ok(())
    }

    fn authorize_scope(
        &self,
        principal: &AuthenticatedPrincipal,
        scope: &Scope,
    ) -> Result<(), ApiError> {
        if principal.authorizes(scope) {
            return Ok(());
        }
        Err(ApiError::new(
            403,
            "PERMISSION_DENIED",
            "authenticated identity is not authorized for this application",
        ))
    }

    fn command(
        &self,
        principal: &AuthenticatedPrincipal,
        family: CommandFamily,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        match family {
            CommandFamily::CredentialReference => self.credential_command(request),
            CommandFamily::Session => {
                if matches!(&request, CommandRequest::InputRespondCommand(_)) {
                    self.interaction_command(request)
                } else {
                    self.session_command(request)
                }
            }
            CommandFamily::Delivery => self.delivery_command(request),
            CommandFamily::Settings => self.settings_command(request),
            CommandFamily::Approval => self.interaction_command(request),
            CommandFamily::Worker => self.worker_command(request),
            CommandFamily::Publication => self.publication_command(request),
            CommandFamily::Enterprise => self.enterprise.command(request),
            CommandFamily::Collaboration => self.collaboration_command(principal, request),
        }
    }

    fn query(
        &self,
        principal: &AuthenticatedPrincipal,
        family: QueryFamily,
        request: QueryRequest,
    ) -> Result<QueryResultResponse, ApiError> {
        match family {
            QueryFamily::Delivery | QueryFamily::Runtime => self.strongflow_query(request),
            QueryFamily::CredentialReference => self.credential_query(request),
            QueryFamily::Worker => self.worker_query(request),
            QueryFamily::Session => {
                if matches!(&request, QueryRequest::ChatInteractionListQuery(_)) {
                    self.interaction_query(request)
                } else {
                    self.session_query(request)
                }
            }
            QueryFamily::Settings => self.settings_query(request),
            QueryFamily::Approval => self.interaction_query(request),
            QueryFamily::Publication => self.publication_query(request),
            QueryFamily::Enterprise => self.enterprise.query(request),
            QueryFamily::Collaboration => self.collaboration_query(principal, request),
        }
    }

    fn subscribe(
        &self,
        principal: &AuthenticatedPrincipal,
        first_frame: ControlPlaneWebSocketClientFrame,
    ) -> Result<EventSubscription, ApiError> {
        let scope = initial_scope(&first_frame).ok_or_else(application_variant_mismatch)?;
        self.authorize_scope(principal, scope)?;
        self.hub
            .grant_authorization(
                principal,
                scope,
                &ControlPlaneWebSocketAuthorizationEpoch(AUTHORIZATION_EPOCH),
            )
            .map_err(|error| error.api_error())?;
        self.hub.subscribe(principal, first_frame)
    }

    fn event_control(
        &self,
        principal: &AuthenticatedPrincipal,
        frame: ControlPlaneWebSocketClientFrame,
    ) -> Result<Vec<Value>, ApiError> {
        self.hub.event_control(principal, frame)
    }

    fn shutdown(&self) -> Result<(), ApiError> {
        let state = self.state.lock().map_err(|_| service_unavailable())?.take();
        let Some(state) = state else {
            return Ok(());
        };
        let mut failures = Vec::new();
        if state.control_plane.shutdown().is_err() {
            failures.push("Control Plane");
        }
        if Box::new(state.storage).close().is_err() {
            failures.push("application storage");
        }
        if state.worker_outbound.close().is_err() {
            failures.push("Worker outbound storage");
        }
        if self.hub.close().is_err() {
            failures.push("event hub");
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ApiError::new(
                500,
                "SHUTDOWN_FAILED",
                "application resources did not close cleanly",
            ))
        }
    }
}

fn initial_scope(frame: &ControlPlaneWebSocketClientFrame) -> Option<&Scope> {
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

fn credential_error(error: &CredentialReferenceError) -> ApiError {
    match error.kind() {
        CredentialReferenceErrorKind::InvalidRequest => ApiError::new(
            400,
            "INVALID_REQUEST",
            "credential reference request is invalid",
        ),
        CredentialReferenceErrorKind::ScopeDenied => ApiError::new(
            403,
            "PERMISSION_DENIED",
            "credential reference scope is not authorized",
        ),
        CredentialReferenceErrorKind::NotFound => resource_not_found(),
        CredentialReferenceErrorKind::Revoked | CredentialReferenceErrorKind::WrongState => {
            ApiError::new(
                409,
                "WRONG_STATE",
                "credential reference state rejects the operation",
            )
        }
        CredentialReferenceErrorKind::RevisionConflict => ApiError::new(
            409,
            "REVISION_CONFLICT",
            "credential reference revision changed",
        ),
        CredentialReferenceErrorKind::RequestConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "requestId was already used with different input",
        ),
        CredentialReferenceErrorKind::CursorInvalid => ApiError::new(
            400,
            "INVALID_REQUEST",
            "credential reference page cursor is invalid",
        ),
        CredentialReferenceErrorKind::CredentialLeak | CredentialReferenceErrorKind::Storage => {
            service_unavailable()
        }
    }
}

fn product_session_error(error: &ProductSessionServiceError) -> ApiError {
    match error.code() {
        ProductSessionServiceErrorCode::InvalidInput => {
            ApiError::new(400, "INVALID_REQUEST", "ProductSession request is invalid")
        }
        ProductSessionServiceErrorCode::NotFound => resource_not_found(),
        ProductSessionServiceErrorCode::RevisionConflict => {
            ApiError::new(409, "REVISION_CONFLICT", "ProductSession revision changed")
        }
        ProductSessionServiceErrorCode::RequestConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "requestId was already used with different input",
        ),
        ProductSessionServiceErrorCode::CursorInvalid => ApiError::new(
            409,
            "READ_CURSOR_EXPIRED",
            "ProductSession read cursor is no longer valid",
        ),
        ProductSessionServiceErrorCode::ActorMismatch => ApiError::new(
            403,
            "PERMISSION_DENIED",
            "ProductSession actor is not authorized",
        ),
        ProductSessionServiceErrorCode::AlreadyExists
        | ProductSessionServiceErrorCode::InvalidState
        | ProductSessionServiceErrorCode::BindingIdentityMismatch
        | ProductSessionServiceErrorCode::BindingConflict
        | ProductSessionServiceErrorCode::WorkerSlotNotRunning
        | ProductSessionServiceErrorCode::MessageLimitExceeded
        | ProductSessionServiceErrorCode::StreamSequenceConflict => ApiError::new(
            409,
            "WRONG_STATE",
            "ProductSession state rejects the operation",
        ),
        ProductSessionServiceErrorCode::CredentialLeak
        | ProductSessionServiceErrorCode::CorruptState
        | ProductSessionServiceErrorCode::Storage => service_unavailable(),
    }
}

fn worker_management_error(error: &WorkerManagementServiceError) -> ApiError {
    match error.kind() {
        WorkerManagementServiceErrorKind::InvalidRequest => {
            ApiError::new(400, "INVALID_REQUEST", "Worker request is invalid")
        }
        WorkerManagementServiceErrorKind::NotFound => resource_not_found(),
        WorkerManagementServiceErrorKind::WrongState => {
            ApiError::new(409, "WRONG_STATE", "Worker state rejects the operation")
        }
        WorkerManagementServiceErrorKind::RevisionConflict => {
            ApiError::new(409, "REVISION_CONFLICT", "Worker revision changed")
        }
        WorkerManagementServiceErrorKind::RequestConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "requestId was already used with different input",
        ),
        WorkerManagementServiceErrorKind::EventUnavailable
        | WorkerManagementServiceErrorKind::Storage => service_unavailable(),
    }
}

fn chat_interaction_error(error: &ChatInteractionServiceError) -> ApiError {
    match error.code() {
        ChatInteractionServiceErrorCode::InvalidInput => {
            ApiError::new(400, "INVALID_REQUEST", "interaction request is invalid")
        }
        ChatInteractionServiceErrorCode::NotFound => resource_not_found(),
        ChatInteractionServiceErrorCode::RequestConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "requestId was already used with different input",
        ),
        ChatInteractionServiceErrorCode::RevisionConflict => {
            ApiError::new(409, "REVISION_CONFLICT", "interaction revision changed")
        }
        ChatInteractionServiceErrorCode::AuthorityMismatch
        | ChatInteractionServiceErrorCode::ActorMismatch => ApiError::new(
            403,
            "PERMISSION_DENIED",
            "interaction authority is not authorized",
        ),
        ChatInteractionServiceErrorCode::Expired | ChatInteractionServiceErrorCode::WrongState => {
            ApiError::new(
                409,
                "WRONG_STATE",
                "interaction state rejects the operation",
            )
        }
        ChatInteractionServiceErrorCode::WorkerDelivery => ApiError::new(
            503,
            "SERVICE_UNAVAILABLE",
            "Worker interaction delivery is temporarily unavailable",
        ),
        ChatInteractionServiceErrorCode::CorruptState
        | ChatInteractionServiceErrorCode::CredentialLeak
        | ChatInteractionServiceErrorCode::Storage => service_unavailable(),
    }
}

fn model_settings_error(error: &ModelSettingsError) -> ApiError {
    match error.kind() {
        ModelSettingsErrorKind::InvalidRequest => {
            ApiError::new(400, "INVALID_REQUEST", "settings request is invalid")
        }
        ModelSettingsErrorKind::ScopeDenied => {
            ApiError::new(403, "PERMISSION_DENIED", "settings scope is not authorized")
        }
        ModelSettingsErrorKind::RevisionConflict => {
            ApiError::new(409, "REVISION_CONFLICT", "settings revision changed")
        }
        ModelSettingsErrorKind::RequestConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "requestId was already used with different input",
        ),
        ModelSettingsErrorKind::AlreadyMigrated => {
            ApiError::new(409, "WRONG_STATE", "settings state rejects the operation")
        }
        ModelSettingsErrorKind::NoConfiguredRoute
        | ModelSettingsErrorKind::ProviderNotFound
        | ModelSettingsErrorKind::ProviderDisabled
        | ModelSettingsErrorKind::ModelNotFound
        | ModelSettingsErrorKind::ModelDisabled => ApiError::new(
            503,
            "TRUSTED_FACTS_UNAVAILABLE",
            "model routing facts are unavailable",
        ),
        ModelSettingsErrorKind::CredentialLeak | ModelSettingsErrorKind::Storage => {
            service_unavailable()
        }
    }
}

fn model_route_availability_error(error: &ModelRouteAvailabilityError) -> ApiError {
    match error.kind() {
        ModelRouteAvailabilityErrorKind::InvalidRequest => ApiError::new(
            400,
            "INVALID_REQUEST",
            "ModelRoute availability request is invalid",
        ),
        ModelRouteAvailabilityErrorKind::ScopeDenied => ApiError::new(
            403,
            "PERMISSION_DENIED",
            "ModelRoute availability scope is not authorized",
        ),
        ModelRouteAvailabilityErrorKind::CredentialLeak
        | ModelRouteAvailabilityErrorKind::Storage => ApiError::new(
            503,
            "TRUSTED_FACTS_UNAVAILABLE",
            "ModelRoute availability facts are unavailable",
        ),
    }
}

fn delivery_application_error(error: &DeliveryApplicationError) -> ApiError {
    match error.code() {
        ErrorCode::InvalidRequest => {
            ApiError::new(400, "INVALID_REQUEST", "Delivery request is invalid")
        }
        ErrorCode::AuthenticationRequired => ApiError::new(
            401,
            "AUTHENTICATION_REQUIRED",
            "Delivery authentication is required",
        ),
        ErrorCode::PermissionDenied => {
            ApiError::new(403, "PERMISSION_DENIED", "Delivery scope is not authorized")
        }
        ErrorCode::ResourceNotFound => resource_not_found(),
        ErrorCode::IdempotencyConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "requestId was already used with different input",
        ),
        ErrorCode::RevisionConflict => {
            ApiError::new(409, "REVISION_CONFLICT", "Delivery revision changed")
        }
        ErrorCode::ReadCursorExpired => ApiError::new(
            409,
            "READ_CURSOR_EXPIRED",
            "Delivery read cursor is no longer retained",
        ),
        ErrorCode::CandidateStale => {
            ApiError::new(409, "CANDIDATE_STALE", "Delivery candidate is stale")
        }
        ErrorCode::WrongState => {
            ApiError::new(409, "WRONG_STATE", "Delivery state rejects the operation")
        }
        ErrorCode::RateLimited => ApiError::new(
            429,
            "RATE_LIMITED",
            "Delivery service rate limit was reached",
        ),
        ErrorCode::TrustedFactsUnavailable => ApiError::new(
            503,
            "TRUSTED_FACTS_UNAVAILABLE",
            "Delivery trusted facts are unavailable",
        ),
        ErrorCode::ServiceUnavailable | ErrorCode::InternalError => service_unavailable(),
    }
}

fn publication_error(error: &PublicationCommandError) -> ApiError {
    match error.public_code() {
        ErrorCode::InvalidRequest => {
            ApiError::new(400, "INVALID_REQUEST", "Publication request is invalid")
        }
        ErrorCode::AuthenticationRequired => ApiError::new(
            401,
            "AUTHENTICATION_REQUIRED",
            "Publication authentication is required",
        ),
        ErrorCode::PermissionDenied => ApiError::new(
            403,
            "PERMISSION_DENIED",
            "Publication policy denies the operation",
        ),
        ErrorCode::ResourceNotFound => resource_not_found(),
        ErrorCode::IdempotencyConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "requestId was already used with different input",
        ),
        ErrorCode::RevisionConflict => {
            ApiError::new(409, "REVISION_CONFLICT", "Publication revision changed")
        }
        ErrorCode::ReadCursorExpired => ApiError::new(
            409,
            "READ_CURSOR_EXPIRED",
            "Publication read cursor is no longer retained",
        ),
        ErrorCode::CandidateStale => {
            ApiError::new(409, "CANDIDATE_STALE", "Publication candidate is stale")
        }
        ErrorCode::WrongState => ApiError::new(
            409,
            "WRONG_STATE",
            "Publication state rejects the operation",
        ),
        ErrorCode::RateLimited => ApiError::new(
            429,
            "RATE_LIMITED",
            "Publication service rate limit was reached",
        ),
        ErrorCode::TrustedFactsUnavailable => ApiError::new(
            503,
            "TRUSTED_FACTS_UNAVAILABLE",
            "Publication trusted facts are unavailable",
        ),
        ErrorCode::ServiceUnavailable | ErrorCode::InternalError => service_unavailable(),
    }
}

fn strongflow_error(error: &StrongFlowProjectionError) -> ApiError {
    use StrongFlowProjectionError::{
        CandidateStale, Internal, InvalidRequest, PermissionDenied, ReadCursorExpired,
        ResourceNotFound, RevisionConflict, ServiceUnavailable, TrustedFactsUnavailable,
    };
    match error {
        InvalidRequest(_) => ApiError::new(400, "INVALID_REQUEST", "StrongFlow query is invalid"),
        PermissionDenied(_) => ApiError::new(
            403,
            "PERMISSION_DENIED",
            "StrongFlow scope is not authorized",
        ),
        ResourceNotFound(_) => resource_not_found(),
        RevisionConflict(_) => {
            ApiError::new(409, "REVISION_CONFLICT", "StrongFlow read cut changed")
        }
        CandidateStale(_) => {
            ApiError::new(409, "CANDIDATE_STALE", "Candidate review binding is stale")
        }
        ReadCursorExpired(_) => ApiError::new(
            409,
            "READ_CURSOR_EXPIRED",
            "StrongFlow read cursor is no longer retained",
        ),
        TrustedFactsUnavailable(_) => ApiError::new(
            503,
            "TRUSTED_FACTS_UNAVAILABLE",
            "StrongFlow trusted facts are unavailable",
        ),
        ServiceUnavailable(_) | Internal(_) => service_unavailable(),
    }
}

fn collaboration_error(error: &CollaborationError) -> ApiError {
    match error.kind() {
        CollaborationErrorKind::InvalidRequest => {
            ApiError::new(400, "INVALID_REQUEST", "Collaboration request is invalid")
        }
        CollaborationErrorKind::PermissionDenied => ApiError::new(
            403,
            "PERMISSION_DENIED",
            "Collaboration permission is denied",
        ),
        CollaborationErrorKind::RevisionConflict => {
            ApiError::new(409, "REVISION_CONFLICT", "Collaboration revision changed")
        }
        CollaborationErrorKind::RequestConflict => ApiError::new(
            409,
            "IDEMPOTENCY_CONFLICT",
            "requestId was already used with different input",
        ),
        CollaborationErrorKind::CursorInvalid => ApiError::new(
            409,
            "READ_CURSOR_EXPIRED",
            "Collaboration read cursor is no longer valid",
        ),
        CollaborationErrorKind::Storage | CollaborationErrorKind::Corrupt => service_unavailable(),
    }
}

fn resource_not_found() -> ApiError {
    ApiError::new(
        404,
        "RESOURCE_NOT_FOUND",
        "requested resource was not found",
    )
}

fn service_unavailable() -> ApiError {
    ApiError::new(
        503,
        "SERVICE_UNAVAILABLE",
        "application service is temporarily unavailable",
    )
}

fn application_configuration_invalid() -> ApiError {
    ApiError::new(
        500,
        "APPLICATION_CONFIGURATION_INVALID",
        "application services do not share one storage authority",
    )
}

fn application_variant_mismatch() -> ApiError {
    ApiError::new(
        500,
        "APPLICATION_RESPONSE_INVALID",
        "generated application route does not match its request",
    )
}

fn millis_to_instant(value: u64) -> Instant {
    let value = value.min(253_402_300_799_999);
    let seconds = value / 1_000;
    let millis = value % 1_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    ))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
