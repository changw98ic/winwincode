// SPDX-License-Identifier: Apache-2.0

//! Production application registry behind the generated HTTP and WebSocket dispatcher.
//!
//! The registry owns no second business model. Product services and the
//! `ControlPlane` use separate connections to the same authoritative `SQLite`
//! database, while the durable event hub owns transport cursors only.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandCompletedResponse, CommandEnvelope, CommandRequest,
    ControlPlaneWebSocketClientFrame, ErrorCode, QueryRequest, QueryResultResponse, Scope,
    WorkRunCancelCommand, WorkRunStartCommand,
};
use winwincode_control_plane::credential_reference::{
    CredentialReferenceError, CredentialReferenceErrorKind, CredentialReferenceService,
};
use winwincode_control_plane::device_session_gate::{
    DeviceSessionGateApproval, DeviceSessionGateDenial, authorize_product_session_turn,
};
use winwincode_control_plane::strongflow_projection::{
    StrongFlowProjectionError, StrongFlowProjectionQueryPort,
};
use winwincode_control_plane::{
    ChatInteractionApiService, ChatInteractionServiceError, ChatInteractionServiceErrorCode,
    CollaborationClock, CollaborationClockError, CollaborationError, CollaborationErrorKind,
    CollaborationService, ControlPlane, DeliveryApplicationError, DurableWorkerInteractionOutbound,
    ModelRequestPoolConfig, ModelRouteAvailabilityError, ModelRouteAvailabilityErrorKind,
    ModelRouteAvailabilityService, ModelSettingsError, ModelSettingsErrorKind,
    ModelSettingsService, ProductSessionApiClock, ProductSessionApiService,
    ProductSessionExecutionConfig, ProductSessionServiceError, ProductSessionServiceErrorCode,
    PublicationCommandError, QuickDeviceDispatchError, QuickDeviceDispatchErrorKind,
    RepositoryExecutionScheduler, RepositoryExecutionSchedulerError, ScopeWorkerHealthEventPort,
    StrongflowDeviceDispatchError, StrongflowDeviceDispatchErrorKind, WorkRunCancellationRequest,
    WorkerManagementService, WorkerManagementServiceError, WorkerManagementServiceErrorKind,
    dispatch_turn_to_device_worker, dispatch_work_run_to_device_worker, workrun_cancel_response,
};
use winwincode_domain::{
    ControlPlaneWebSocketAuthorizationEpoch, Instant, ProductSessionId, Sha256Digest, WorkRunId,
};
use winwincode_storage::{ProductStateStorage, RepositorySchedulerScope, SqliteStorage};

use crate::{
    ApiError, AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, DurableEventHub,
    EventSubscription, HealthyRuntimeHealth, QueryFamily, RuntimeHealthPort,
    TypedControlPlaneApiPort,
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
    collaboration: Arc<CollaborationService>,
    execution_config: ProductSessionExecutionConfig,
}

/// One production registry for generated HTTP commands, queries, and WS frames.
pub struct StandaloneControlPlaneApplication {
    state: Arc<Mutex<Option<ApplicationState>>>,
    hub: Arc<DurableEventHub>,
    clock: Arc<dyn StandaloneApplicationClock>,
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
        Self::new_with_clock(
            control_plane,
            storage,
            worker_outbound,
            hub,
            Arc::new(SystemStandaloneApplicationClock),
            execution_config,
        )
    }

    /// Composes a collaboration service that shares the same durable authority.
    ///
    /// # Errors
    ///
    /// Rejects the same invalid local composition as [`Self::new`].
    pub fn new_with_collaboration(
        control_plane: ControlPlane,
        storage: SqliteStorage,
        worker_outbound: DurableWorkerInteractionOutbound,
        hub: Arc<DurableEventHub>,
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
        let data_directory = storage
            .database_path()
            .parent()
            .ok_or_else(application_configuration_invalid)?;
        let collaboration = Arc::new(CollaborationService::with_clock(
            SqliteStorage::open(data_directory).map_err(|_| application_configuration_invalid())?,
            Box::new(CollaborationClockAdapter(Arc::clone(&clock))),
        ));
        Self::compose(
            control_plane,
            storage,
            worker_outbound,
            hub,
            clock,
            ApplicationComposition {
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
        // FLOW-100.3: the ProductSession continue permission gate. A session
        // bound to device execution continues only for its current occupancy
        // holder while the repository binding stays visible under the
        // dual-authorization projection; sessions without a device anchor
        // pass through unchanged.
        // FLOW-100.4: the approval is the dispatch trigger — an approved
        // Chat turn of a device-anchored session executes on the launched
        // Device WorkerSession instead of the local embedded worker.
        let device_dispatch = match &request {
            CommandRequest::ChatSubmitCommand(command) => product_session_turn_gate(
                &mut state.storage,
                &command.actor,
                &command.payload.product_session_id,
            )?
            .map(|_: DeviceSessionGateApproval| {
                (
                    command.payload.product_session_id.clone(),
                    command.request_id.clone(),
                )
            }),
            _ => None,
        };
        let dispatch_now = self.clock.now_instant();
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
        // FLOW-100.4: route the committed turn to the session's Device
        // WorkerSession. The durable dispatch never blocks the accepted
        // Chat receipt on a transport action; ordinary admission
        // backpressure keeps the turn queued for the device worker.
        if let Some((product_session_id, request_id)) = device_dispatch {
            dispatch_turn_to_device_worker(
                &mut state.storage,
                &product_session_id,
                &request_id,
                &dispatch_now,
            )
            .map_err(|error| quick_device_dispatch_error(&error))?;
        }
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
        let dispatch_now = self.clock.now_instant();
        // FLOW-100.5: the acting browser user of a `workrun.start` is the
        // candidate device dispatcher; service and system actors never route
        // to a device.
        let advance_actor = match &request {
            CommandRequest::WorkRunStartCommand(command) => match &command.actor {
                Actor::UserActor(user) => Some(user.id.0.clone()),
                _ => None,
            },
            _ => None,
        };
        let advance_command = match &request {
            CommandRequest::WorkRunStartCommand(command) => Some(command.clone()),
            _ => None,
        };
        let mut guard = self.state()?;
        let state = guard.as_mut().ok_or_else(service_unavailable)?;
        let mut response = match request {
            CommandRequest::WorkRunCancelCommand(command) => {
                cancel_workrun(&mut state.storage, &command, &dispatch_now)?
            }
            request => match request {
                CommandRequest::DeliveryCreateCommand(command) => state
                    .control_plane
                    .delivery_create(&command)
                    .map(CommandCompletedResponse::DeliveryCreateCompletedResponse),
                CommandRequest::DeliveryUpdateSpecCommand(command) => state
                    .control_plane
                    .delivery_update_spec(&command)
                    .map(CommandCompletedResponse::DeliveryUpdateSpecCompletedResponse),
                CommandRequest::WorkItemsCreateCommand(command) => state
                    .control_plane
                    .work_items_create(&command)
                    .map(CommandCompletedResponse::WorkItemsCreateCompletedResponse),
                CommandRequest::WorkRunStartCommand(command) => state
                    .control_plane
                    .workrun_start(&command)
                    .map(CommandCompletedResponse::WorkRunStartCompletedResponse),
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
            .map_err(|error| delivery_application_error(&error))?,
        };
        // FLOW-100.5: route the committed Codex WorkRun job of the advanced
        // Delivery WorkRun to its Device WorkerSession. The WorkRun's durable
        // launch anchor decides: with an anchor, the routing glue binds the
        // launched worker session and attaches the job's device facts (after
        // the FLOW-100.3 permission gate approves the acting user); without
        // one, the WorkRun keeps the supervised local execution path. The
        // durable dispatch never blocks the committed Delivery receipt on a
        // transport action; ordinary admission backpressure keeps the WorkRun
        // job queued for the device worker.
        let advance_work_run_id = match advance_command.as_ref() {
            Some(command) => resolve_advance_work_run(&mut state.storage, command)?,
            None => None,
        };
        if let Some(work_run_id) = advance_work_run_id.as_ref()
            && let CommandCompletedResponse::WorkRunStartCompletedResponse(completed) =
                &mut response
        {
            completed.result.active_work_run_id = Some(work_run_id.clone());
        }
        if let Some(user_id) = advance_actor
            && let Some(work_run_id) = advance_work_run_id.as_ref()
        {
            dispatch_work_run_to_device_worker(
                &mut state.storage,
                Some(&user_id),
                work_run_id,
                &dispatch_now,
            )
            .map_err(|error| strongflow_device_dispatch_error(&error))?;
        }
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
            QueryRequest::WorkRunGetQuery(query) => state
                .control_plane
                .workrun_get(&query)
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

fn cancel_workrun(
    storage: &mut SqliteStorage,
    command: &WorkRunCancelCommand,
    requested_at: &Instant,
) -> Result<CommandCompletedResponse, ApiError> {
    let expected_delivery_revision = u64::try_from(command.expected_revision.0)
        .map_err(|_| ApiError::new(400, "INVALID_REQUEST", "expectedRevision is invalid"))?;
    let command_bytes = serde_json::to_vec(command)
        .map_err(|_| ApiError::new(400, "INVALID_REQUEST", "command cannot be encoded"))?;
    let public_command_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(command_bytes)));
    let request = WorkRunCancellationRequest {
        scope: RepositorySchedulerScope {
            organization_id: command.scope.organization_id.clone(),
            workspace_id: command.scope.workspace_id.clone(),
            project_id: command.scope.project_id.clone(),
            repository_id: command.scope.repository_id.clone(),
        },
        delivery_id: command.payload.delivery_id.clone(),
        work_run_id: command.payload.work_run_id.clone(),
        request_id: command.request_id.clone(),
        expected_delivery_revision,
        requested_at: requested_at.clone(),
        public_command_digest,
    };
    RepositoryExecutionScheduler::new(storage)
        .request_cancellation_for_work_run(&request)
        .map_err(|error| workrun_cancel_error(&error))?;
    let state_row = storage
        .load_state(&format!("delivery:{}", command.payload.delivery_id.0))
        .map_err(|error| ApiError::new(500, "INTERNAL_ERROR", error.to_string()))?
        .ok_or_else(resource_not_found)?;
    let delivery = winwincode_delivery::domain::Delivery::decode_json(&state_row.payload)
        .map_err(|error| ApiError::new(500, "INTERNAL_ERROR", error.to_string()))?;
    workrun_cancel_response(command, &delivery)
        .map(CommandCompletedResponse::WorkRunCancelCompletedResponse)
        .map_err(|error| delivery_application_error(&error))
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

/// FLOW-100.3: the `ProductSession` continue permission gate entry. Only a
/// signed-in user actor is gated; service and system actors pass through.
/// The returned approval is `Some` exactly when the session is
/// device-anchored and the gate approved its continuation (FLOW-100.4 uses
/// it as the device dispatch trigger).
fn product_session_turn_gate(
    storage: &mut SqliteStorage,
    actor: &Actor,
    product_session_id: &ProductSessionId,
) -> Result<Option<DeviceSessionGateApproval>, ApiError> {
    let Actor::UserActor(user) = actor else {
        return Ok(None);
    };
    authorize_product_session_turn(storage, user.id.0.as_str(), product_session_id.0.as_str())
        .map_err(|error| device_session_gate_error(&error))
}

/// Maps one gate denial onto the central gate wire error code
/// (`OCCUPANCY_REQUIRED` / `ACCESS_DENIED` / `BINDING_NOT_VISIBLE`).
fn device_session_gate_error(error: &DeviceSessionGateDenial) -> ApiError {
    ApiError::new(
        error.http_status(),
        error.wire_code(),
        "ProductSession device execution gate denied the request",
    )
}

/// Maps one Quick device dispatch failure onto the wire error codes. The
/// committed Chat turn stays queued and an exact retry re-runs the dispatch,
/// so a temporary failure presents as service unavailability while a dead
/// anchor or conflicting dispatch presents as wrong state.
fn quick_device_dispatch_error(error: &QuickDeviceDispatchError) -> ApiError {
    match error.kind() {
        QuickDeviceDispatchErrorKind::InvalidInput => ApiError::new(
            400,
            "INVALID_REQUEST",
            "device execution dispatch request is invalid",
        ),
        QuickDeviceDispatchErrorKind::AnchorNotLive
        | QuickDeviceDispatchErrorKind::WorkerSessionEnded
        | QuickDeviceDispatchErrorKind::DispatchConflict => ApiError::new(
            409,
            "WRONG_STATE",
            "the device worker session cannot execute this turn",
        ),
        QuickDeviceDispatchErrorKind::AdmissionUnavailable
        | QuickDeviceDispatchErrorKind::CorruptState
        | QuickDeviceDispatchErrorKind::Storage => service_unavailable(),
    }
}

/// Maps one `StrongFlow` device routing failure onto the wire error codes. The
/// committed Delivery advance stays durable and an exact retry re-runs the
/// routing, so a temporary failure presents as service unavailability, a
/// gate denial carries the central gate wire code, and a dead anchor or
/// conflicting dispatch presents as wrong state.
fn strongflow_device_dispatch_error(error: &StrongflowDeviceDispatchError) -> ApiError {
    match error.kind() {
        StrongflowDeviceDispatchErrorKind::InvalidInput => ApiError::new(
            400,
            "INVALID_REQUEST",
            "device WorkRun dispatch request is invalid",
        ),
        StrongflowDeviceDispatchErrorKind::GateDenied => {
            let denial = error
                .gate_denial()
                .expect("a gate denial carries its facts");
            ApiError::new(
                denial.http_status(),
                denial.wire_code(),
                "ProductSession device execution gate denied the request",
            )
        }
        StrongflowDeviceDispatchErrorKind::AnchorNotLive
        | StrongflowDeviceDispatchErrorKind::WorkerSessionEnded
        | StrongflowDeviceDispatchErrorKind::DispatchConflict => ApiError::new(
            409,
            "WRONG_STATE",
            "the device worker session cannot execute this WorkRun",
        ),
        StrongflowDeviceDispatchErrorKind::AdmissionUnavailable
        | StrongflowDeviceDispatchErrorKind::CorruptState
        | StrongflowDeviceDispatchErrorKind::Storage => service_unavailable(),
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

fn resolve_advance_work_run(
    storage: &mut SqliteStorage,
    command: &WorkRunStartCommand,
) -> Result<Option<WorkRunId>, ApiError> {
    let envelope: CommandEnvelope =
        serde_json::from_value(serde_json::to_value(command).map_err(|_| {
            ApiError::new(500, "INTERNAL_ERROR", "advance command cannot be encoded")
        })?)
        .map_err(|_| ApiError::new(500, "INTERNAL_ERROR", "advance command identity is invalid"))?;
    let identity = winwincode_control_plane::command_receipt_identity(
        &envelope.actor,
        &envelope.scope,
        envelope.request_id.clone(),
    )
    .map_err(|_| ApiError::new(503, "SERVICE_UNAVAILABLE", "advance receipt is unavailable"))?;
    let serialized = serde_json::to_vec(&envelope)
        .map_err(|_| ApiError::new(500, "INTERNAL_ERROR", "advance command cannot be encoded"))?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(serialized)));
    let receipt = storage
        .load_receipt(&identity, &digest)
        .map_err(|_| ApiError::new(503, "SERVICE_UNAVAILABLE", "advance receipt is unavailable"))?;
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    if receipt.stream_id != format!("delivery:{}", command.payload.delivery_id.0) {
        return Err(ApiError::new(
            409,
            "WRONG_STATE",
            "advance receipt belongs to another Delivery",
        ));
    }
    dispatch_work_run_from_events(&receipt.events)
}

fn dispatch_work_run_from_events(
    events: &[winwincode_storage::OutboxEvent],
) -> Result<Option<WorkRunId>, ApiError> {
    for event in events {
        if event.topic != "execution.job.dispatch" {
            continue;
        }
        let job: winwincode_execution_port::generated::ExecutionJob =
            serde_json::from_slice(&event.payload).map_err(|_| {
                ApiError::new(
                    503,
                    "SERVICE_UNAVAILABLE",
                    "advance dispatch intent is invalid",
                )
            })?;
        let winwincode_execution_port::generated::ExecutionScope::WorkRunExecutionScope(scope) =
            job.scope
        else {
            return Err(ApiError::new(
                409,
                "WRONG_STATE",
                "advance dispatch intent is not a WorkRun",
            ));
        };
        return Ok(Some(scope.work_run_id));
    }
    Ok(None)
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

fn workrun_cancel_error(error: &RepositoryExecutionSchedulerError) -> ApiError {
    match error {
        RepositoryExecutionSchedulerError::StaleDeliveryRevision => {
            ApiError::new(409, "REVISION_CONFLICT", "Delivery revision changed")
        }
        RepositoryExecutionSchedulerError::Storage(_) => {
            ApiError::new(409, "WRONG_STATE", "WorkRun cancellation was rejected")
        }
        RepositoryExecutionSchedulerError::InvalidExecutionJob(_)
        | RepositoryExecutionSchedulerError::MissingCancellationAuthority(_) => ApiError::new(
            409,
            "WRONG_STATE",
            "WorkRun cancellation authority is unavailable",
        ),
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

#[cfg(test)]
mod tests {
    use super::dispatch_work_run_from_events;
    use winwincode_domain::{
        ExecutionJobId, Instant, ProductSessionId, RepositoryId, Revision, Sha256Digest,
        WorkContractId, WorkItemId, WorkRunId,
    };
    use winwincode_execution_port::generated::{
        ExecutionJob, ExecutionLimits, ExecutionScope, ExecutionWorkspace,
        ExecutionWorkspaceWriteMode, WorkRunExecutionScope, WorkRunExecutionScopeKind,
    };
    use winwincode_storage::OutboxEvent;

    fn dispatch_event(job_id: &str, work_run_id: &str) -> OutboxEvent {
        let job = ExecutionJob {
            attempt: 1,
            execution_profile: "requirements".to_owned(),
            goal: "test".to_owned(),
            job_id: ExecutionJobId(job_id.to_owned()),
            limits: ExecutionLimits {
                deadline_at: Instant("2027-01-15T09:00:00.000Z".to_owned()),
                max_artifact_bytes: 1,
                max_runtime_seconds: 1,
            },
            payload_digest: Sha256Digest(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_owned(),
            ),
            scope: ExecutionScope::WorkRunExecutionScope(WorkRunExecutionScope {
                attempt: 1,
                kind: WorkRunExecutionScopeKind::WorkRun,
                product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
                rework_authorization: None,
                work_contract_id: WorkContractId("wct_00000000000000000000000000".to_owned()),
                work_contract_revision: Revision(1),
                work_item_id: WorkItemId("wit_00000000000000000000000000".to_owned()),
                work_item_revision: Revision(1),
                work_run_id: WorkRunId(work_run_id.to_owned()),
            }),
            work_input: None,
            workspace: ExecutionWorkspace {
                checkout_revision: "fixture".to_owned(),
                repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
                write_mode: ExecutionWorkspaceWriteMode::ReadOnly,
            },
        };
        OutboxEvent {
            sequence: 1,
            event_id: format!("event-{job_id}"),
            topic: "execution.job.dispatch".to_owned(),
            payload: serde_json::to_vec(&job).expect("encode dispatch job"),
            projection_cursor: None,
            public_context: None,
        }
    }

    #[test]
    fn same_delivery_jobs_keep_distinct_workrun_identity_on_replay() {
        let first = dispatch_event(
            "job_00000000000000000000000001",
            "wrn_00000000000000000000000001",
        );
        let second = dispatch_event(
            "job_00000000000000000000000002",
            "wrn_00000000000000000000000002",
        );
        assert_eq!(
            dispatch_work_run_from_events(std::slice::from_ref(&first)).expect("first receipt"),
            Some(WorkRunId("wrn_00000000000000000000000001".to_owned()))
        );
        assert_eq!(
            dispatch_work_run_from_events(std::slice::from_ref(&second))
                .expect("second receipt replay"),
            Some(WorkRunId("wrn_00000000000000000000000002".to_owned()))
        );
    }

    #[test]
    fn replay_ignores_a_foreign_same_delivery_job_event() {
        let foreign = dispatch_event(
            "job_00000000000000000000000003",
            "wrn_00000000000000000000000003",
        );
        let mut own = dispatch_event(
            "job_00000000000000000000000004",
            "wrn_00000000000000000000000004",
        );
        own.topic = "execution.job.dispatch".to_owned();
        let mut unrelated = foreign;
        unrelated.topic = "other.internal.event".to_owned();
        assert_eq!(
            dispatch_work_run_from_events(&[unrelated, own]).expect("exact replay"),
            Some(WorkRunId("wrn_00000000000000000000000004".to_owned()))
        );
    }
}
