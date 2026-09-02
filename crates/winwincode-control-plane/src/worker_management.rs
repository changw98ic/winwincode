// SPDX-License-Identifier: Apache-2.0

//! Generated Worker management commands and queries over the canonical registry.
//!
//! [`WorkerManagementService`] maps public DTOs to the single durable
//! [`ExecutionRegistry`](winwincode_storage::ExecutionRegistry). Public event
//! construction is injected so the storage adapter and this application seam
//! can land independently from the scope-stream schema migration.

use std::collections::HashSet;
use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, ControlPlaneWebSocketWorkerHealthChangedEvent,
    ControlPlaneWebSocketWorkerHealthChangedEventTypeValue, PageInfo, Scope, WorkerDrainCommand,
    WorkerDrainCompletedResponse, WorkerDrainCompletedResponseCommand,
    WorkerDrainCompletedResponseOutcome, WorkerEnableCommand, WorkerEnableCompletedResponse,
    WorkerEnableCompletedResponseCommand, WorkerEnableCompletedResponseOutcome, WorkerGetQuery,
    WorkerGetResultResponse, WorkerGetResultResponseQuery, WorkerListQuery,
    WorkerListResultResponse, WorkerListResultResponseQuery, WorkerPage, WorkerPageKind,
    WorkerProjection,
};
use winwincode_domain::{
    ControlPlaneEventId, Instant, OpaqueCursor, Revision, Sha256Digest, WorkerId,
};
use winwincode_storage::{
    NewOutboxEvent, ProjectionEventStream, PublicEventScope, PublicEventSource, ReceiptScopeKey,
    SqliteStorage, StorageError, StorageErrorKind, WorkerHealth, WorkerManagementCommand,
    WorkerManagementPageCursor, WorkerManagementReceipt, WorkerManagementSnapshot,
    WorkerManagementState, WorkerOperationalState, WorkerRegistryScope,
};

use crate::{command_receipt, command_receipt_identity, public_event_actor, public_event_scope};

/// Canonical public topic emitted after a Worker management transition.
pub const WORKER_HEALTH_CHANGED_TOPIC: &str = "worker-health.changed.v1";

const SERVICE_COMPONENT: &str = "worker-management-service";
const CURSOR_SCHEMA: &str = "winwincode.worker-page.v1";
const MAX_CURSOR_BYTES: usize = 2_048;

/// Complete secret-free input for the scope-stream event adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerHealthEventRequest {
    pub event_id: ControlPlaneEventId,
    pub event: ControlPlaneWebSocketWorkerHealthChangedEvent,
    pub scope: PublicEventScope,
    pub occurred_at: Instant,
    pub source: PublicEventSource,
}

/// Event adapter failure category with no provider or credential detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerHealthEventPortErrorKind {
    InvalidEvent,
    Unavailable,
}

/// Bounded failure returned by the scope-stream event adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerHealthEventPortError {
    kind: WorkerHealthEventPortErrorKind,
    message: &'static str,
}

impl WorkerHealthEventPortError {
    #[must_use]
    pub const fn invalid_event() -> Self {
        Self {
            kind: WorkerHealthEventPortErrorKind::InvalidEvent,
            message: "Worker health event is invalid",
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: WorkerHealthEventPortErrorKind::Unavailable,
            message: "Worker health event stream is unavailable",
        }
    }

    #[must_use]
    pub const fn kind(&self) -> WorkerHealthEventPortErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerHealthEventPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for WorkerHealthEventPortError {}

/// Produces the canonical public scope-stream outbox event.
pub trait WorkerHealthEventPort {
    /// Returns one validated event that storage appends beside the state and
    /// command receipt in the same transaction.
    ///
    /// # Errors
    ///
    /// Returns a bounded error when the canonical scope stream cannot be used.
    fn prepare_worker_health_event(
        &self,
        request: &WorkerHealthEventRequest,
    ) -> Result<NewOutboxEvent, WorkerHealthEventPortError>;
}

/// Canonical scope-stream event adapter used by Server composition.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScopeWorkerHealthEventPort;

impl WorkerHealthEventPort for ScopeWorkerHealthEventPort {
    fn prepare_worker_health_event(
        &self,
        request: &WorkerHealthEventRequest,
    ) -> Result<NewOutboxEvent, WorkerHealthEventPortError> {
        let payload = serde_json::to_vec(&request.event)
            .map_err(|_| WorkerHealthEventPortError::invalid_event())?;
        NewOutboxEvent::public_projection(
            request.event_id.clone(),
            WORKER_HEALTH_CHANGED_TOPIC,
            payload,
            ProjectionEventStream::Scope,
            request.scope.clone(),
            request.occurred_at.clone(),
            request.source.clone(),
        )
        .map_err(|_| WorkerHealthEventPortError::invalid_event())
    }
}

/// Stable application-service failure categories for HTTP mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerManagementServiceErrorKind {
    InvalidRequest,
    NotFound,
    WrongState,
    RevisionConflict,
    RequestConflict,
    EventUnavailable,
    Storage,
}

/// Bounded Worker management error safe for a public error envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerManagementServiceError {
    kind: WorkerManagementServiceErrorKind,
    message: &'static str,
}

impl WorkerManagementServiceError {
    const fn new(kind: WorkerManagementServiceErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid_request() -> Self {
        Self::new(
            WorkerManagementServiceErrorKind::InvalidRequest,
            "Worker management request is invalid",
        )
    }

    const fn not_found() -> Self {
        Self::new(
            WorkerManagementServiceErrorKind::NotFound,
            "Worker was not found in the requested scope",
        )
    }

    const fn wrong_state() -> Self {
        Self::new(
            WorkerManagementServiceErrorKind::WrongState,
            "Worker is already in the requested management state",
        )
    }

    const fn cursor_invalid() -> Self {
        Self::new(
            WorkerManagementServiceErrorKind::InvalidRequest,
            "Worker page cursor is invalid or stale",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> WorkerManagementServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerManagementServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for WorkerManagementServiceError {}

/// Narrow generated-DTO adapter over the single durable Worker Registry.
pub struct WorkerManagementService<'storage, 'events> {
    storage: &'storage mut SqliteStorage,
    events: &'events dyn WorkerHealthEventPort,
}

impl<'storage, 'events> WorkerManagementService<'storage, 'events> {
    #[must_use]
    pub const fn new(
        storage: &'storage mut SqliteStorage,
        events: &'events dyn WorkerHealthEventPort,
    ) -> Self {
        Self { storage, events }
    }

    /// Drains one exact scoped Worker and returns the durable projection.
    ///
    /// # Errors
    ///
    /// Rejects malformed, foreign, stale, repeated-state, or conflicting
    /// commands. Public event preparation must succeed before the atomic
    /// registry transaction begins.
    pub fn drain(
        &mut self,
        command: &WorkerDrainCommand,
        occurred_at: &Instant,
    ) -> Result<WorkerDrainCompletedResponse, WorkerManagementServiceError> {
        validate_reason(&command.payload.reason)?;
        let receipt = self.manage(
            CommandName::WorkerDrain,
            &command.actor,
            &command.scope,
            &command.request_id,
            &command.schema_version,
            &command.expected_revision,
            &command.payload,
            &command.payload.worker_id,
            WorkerManagementState::Draining,
            occurred_at,
        )?;
        Ok(WorkerDrainCompletedResponse {
            command: WorkerDrainCompletedResponseCommand::WorkerDrain,
            current_revision: revision(receipt.worker.revision)?,
            outcome: WorkerDrainCompletedResponseOutcome::Completed,
            previous_revision: revision(receipt.previous_revision)?,
            request_id: command.request_id.clone(),
            result: worker_projection(&receipt.worker)?,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Enables one exact scoped Worker and returns the durable projection.
    ///
    /// # Errors
    ///
    /// Rejects malformed, foreign, stale, repeated-state, or conflicting
    /// commands and event preparation failures.
    pub fn enable(
        &mut self,
        command: &WorkerEnableCommand,
        occurred_at: &Instant,
    ) -> Result<WorkerEnableCompletedResponse, WorkerManagementServiceError> {
        let receipt = self.manage(
            CommandName::WorkerEnable,
            &command.actor,
            &command.scope,
            &command.request_id,
            &command.schema_version,
            &command.expected_revision,
            &command.payload,
            &command.payload.worker_id,
            WorkerManagementState::Enabled,
            occurred_at,
        )?;
        Ok(WorkerEnableCompletedResponse {
            command: WorkerEnableCompletedResponseCommand::WorkerEnable,
            current_revision: revision(receipt.worker.revision)?,
            outcome: WorkerEnableCompletedResponseOutcome::Completed,
            previous_revision: revision(receipt.previous_revision)?,
            request_id: command.request_id.clone(),
            result: worker_projection(&receipt.worker)?,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Lists a scope-local, filter-bound, stable Worker page.
    ///
    /// # Errors
    ///
    /// Rejects invalid actors, scopes, filters, limits, or foreign/stale
    /// cursors and propagates durable registry failures.
    pub fn list(
        &mut self,
        query: &WorkerListQuery,
        observed_at: &Instant,
    ) -> Result<WorkerListResultResponse, WorkerManagementServiceError> {
        let scope_key = validate_query_identity(
            &query.actor,
            &query.scope,
            &query.request_id,
            query.page.limit,
        )?;
        let scope = worker_scope(&query.scope);
        let states = worker_states(&query.parameters.states)?;
        let cursor = decode_cursor(query.page.cursor.as_ref(), &scope_key, &states)?;
        let limit = usize::try_from(query.page.limit)
            .map_err(|_| WorkerManagementServiceError::invalid_request())?;
        let page = self
            .storage
            .execution_registry()
            .and_then(|registry| {
                registry.list_managed_workers(&scope, &states, cursor.as_ref(), limit, observed_at)
            })
            .map_err(|error| storage_error(&error, false))?;
        let next_cursor = page
            .next_cursor
            .as_ref()
            .map(|cursor| encode_cursor(cursor, &scope_key, &states))
            .transpose()?;
        let items = page
            .workers
            .iter()
            .map(worker_projection)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WorkerListResultResponse {
            page: PageInfo {
                has_more: next_cursor.is_some(),
                next_cursor,
            },
            query: WorkerListResultResponseQuery::WorkerList,
            request_id: query.request_id.clone(),
            result: WorkerPage {
                items,
                kind: WorkerPageKind::WorkerPage,
            },
            schema_version: query.schema_version.clone(),
        })
    }

    /// Loads one exact Worker projection from one exact scope.
    ///
    /// # Errors
    ///
    /// Rejects paginated exact reads, invalid identity/scope, missing Workers,
    /// and durable registry failures.
    pub fn get(
        &mut self,
        query: &WorkerGetQuery,
        observed_at: &Instant,
    ) -> Result<WorkerGetResultResponse, WorkerManagementServiceError> {
        validate_query_identity(
            &query.actor,
            &query.scope,
            &query.request_id,
            query.page.limit,
        )?;
        if query.page.cursor.is_some() || query.page.limit != 1 {
            return Err(WorkerManagementServiceError::invalid_request());
        }
        let worker = self
            .storage
            .execution_registry()
            .and_then(|registry| {
                registry.load_managed_worker(
                    &worker_scope(&query.scope),
                    &query.parameters.worker_id,
                    observed_at,
                )
            })
            .map_err(|error| storage_error(&error, false))?
            .ok_or_else(WorkerManagementServiceError::not_found)?;
        Ok(WorkerGetResultResponse {
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            query: WorkerGetResultResponseQuery::WorkerGet,
            request_id: query.request_id.clone(),
            result: worker_projection(&worker)?,
            schema_version: query.schema_version.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn manage<T: Serialize>(
        &mut self,
        command_name: CommandName,
        actor: &Actor,
        scope: &Scope,
        request_id: &winwincode_domain::RequestId,
        schema_version: &winwincode_domain::SchemaVersion,
        expected_revision: &Revision,
        payload: &T,
        worker_id: &WorkerId,
        target_state: WorkerManagementState,
        occurred_at: &Instant,
    ) -> Result<WorkerManagementReceipt, WorkerManagementServiceError> {
        let expected_revision = u64::try_from(expected_revision.0)
            .map_err(|_| WorkerManagementServiceError::invalid_request())?;
        let worker_scope = worker_scope(scope);
        let envelope = CommandEnvelope {
            actor: actor.clone(),
            command: command_name,
            expected_revision: Revision(
                i64::try_from(expected_revision)
                    .map_err(|_| WorkerManagementServiceError::invalid_request())?,
            ),
            payload: serde_json::to_value(payload)
                .map_err(|_| WorkerManagementServiceError::invalid_request())?,
            request_id: request_id.clone(),
            schema_version: schema_version.clone(),
            scope: scope.clone(),
        };
        let (receipt_identity, command_digest) =
            command_receipt(&envelope).map_err(|error| storage_error(&error, false))?;
        if let Some(replay) = self
            .storage
            .execution_registry()
            .and_then(|registry| {
                registry.replay_worker_management(&receipt_identity, &command_digest)
            })
            .map_err(|error| storage_error(&error, false))?
        {
            return Ok(replay);
        }
        let (current, reported_available_capacity) = {
            let registry = self
                .storage
                .execution_registry()
                .map_err(|error| storage_error(&error, false))?;
            let current = registry
                .load_managed_worker(&worker_scope, worker_id, occurred_at)
                .map_err(|error| storage_error(&error, false))?
                .ok_or_else(WorkerManagementServiceError::not_found)?;
            let worker = registry
                .load_worker(worker_id)
                .map_err(|error| storage_error(&error, false))?
                .ok_or_else(WorkerManagementServiceError::not_found)?;
            if worker.management_scope != worker_scope {
                return Err(WorkerManagementServiceError::not_found());
            }
            (current, worker.available_slots)
        };
        let predicted = predicted_snapshot(
            &current,
            target_state,
            reported_available_capacity,
            occurred_at,
        );
        let event_request =
            worker_health_event_request(actor, scope, &command_digest, &predicted, occurred_at)?;
        let public_event = self
            .events
            .prepare_worker_health_event(&event_request)
            .map_err(event_port_error)?;
        validate_prepared_event(&public_event, &event_request)?;
        let command = WorkerManagementCommand {
            receipt_identity,
            command_digest,
            scope: worker_scope,
            worker_id: worker_id.clone(),
            expected_revision,
            target_state,
            occurred_at: occurred_at.clone(),
            public_event,
        };
        self.storage
            .execution_registry()
            .and_then(|mut registry| registry.manage_worker(&command))
            .map_err(|error| storage_error(&error, true))
    }
}

fn validate_reason(reason: &str) -> Result<(), WorkerManagementServiceError> {
    if reason.is_empty()
        || reason.len() > 2_000
        || reason.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(WorkerManagementServiceError::invalid_request());
    }
    Ok(())
}

fn validate_query_identity(
    actor: &Actor,
    scope: &Scope,
    request_id: &winwincode_domain::RequestId,
    limit: i64,
) -> Result<ReceiptScopeKey, WorkerManagementServiceError> {
    if !(1..=200).contains(&limit) {
        return Err(WorkerManagementServiceError::invalid_request());
    }
    command_receipt_identity(actor, scope, request_id.clone())
        .map(|identity| identity.scope_key().clone())
        .map_err(|error| storage_error(&error, false))
}

fn worker_scope(scope: &Scope) -> WorkerRegistryScope {
    match scope {
        Scope::OrganizationScope(scope) => WorkerRegistryScope::Organization {
            organization_id: scope.organization_id.clone(),
        },
        Scope::WorkspaceScope(scope) => WorkerRegistryScope::Workspace {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
        },
        Scope::ProjectScope(scope) => WorkerRegistryScope::Project {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
        },
        Scope::RepositoryScope(scope) => WorkerRegistryScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
    }
}

fn worker_states(
    states: &[String],
) -> Result<Vec<WorkerOperationalState>, WorkerManagementServiceError> {
    let mut seen = HashSet::new();
    for state in states {
        if !seen.insert(state.as_str()) {
            return Err(WorkerManagementServiceError::invalid_request());
        }
        if !matches!(state.as_str(), "enabled" | "draining" | "offline") {
            return Err(WorkerManagementServiceError::invalid_request());
        }
    }
    let mut result = Vec::with_capacity(states.len());
    for (name, state) in [
        ("enabled", WorkerOperationalState::Enabled),
        ("draining", WorkerOperationalState::Draining),
        ("offline", WorkerOperationalState::Offline),
    ] {
        if seen.contains(name) {
            result.push(state);
        }
    }
    Ok(result)
}

fn predicted_snapshot(
    current: &WorkerManagementSnapshot,
    target_state: WorkerManagementState,
    reported_available_capacity: u64,
    occurred_at: &Instant,
) -> WorkerManagementSnapshot {
    let operational_state = if target_state == WorkerManagementState::Draining {
        WorkerOperationalState::Draining
    } else if current.health == WorkerHealth::TimedOut {
        WorkerOperationalState::Offline
    } else {
        WorkerOperationalState::Enabled
    };
    WorkerManagementSnapshot {
        worker_id: current.worker_id.clone(),
        scope: current.scope.clone(),
        management_state: target_state,
        operational_state,
        health: current.health,
        revision: current.revision.saturating_add(1),
        capacity: current.capacity,
        available_capacity: if operational_state == WorkerOperationalState::Enabled {
            reported_available_capacity
        } else {
            0
        },
        active_lease_count: current.active_lease_count,
        last_heartbeat_at: current.last_heartbeat_at.clone(),
        observed_at: occurred_at.clone(),
    }
}

fn worker_health_event_request(
    actor: &Actor,
    scope: &Scope,
    command_digest: &Sha256Digest,
    worker: &WorkerManagementSnapshot,
    occurred_at: &Instant,
) -> Result<WorkerHealthEventRequest, WorkerManagementServiceError> {
    Ok(WorkerHealthEventRequest {
        event_id: worker_health_event_id(command_digest),
        event: ControlPlaneWebSocketWorkerHealthChangedEvent {
            active_lease_count: i64::try_from(worker.active_lease_count)
                .map_err(|_| WorkerManagementServiceError::invalid_request())?,
            available_capacity: i64::try_from(worker.available_capacity)
                .map_err(|_| WorkerManagementServiceError::invalid_request())?,
            capability_labels: None,
            observed_at: occurred_at.clone(),
            status: worker_event_status(worker).to_owned(),
            type_value:
                ControlPlaneWebSocketWorkerHealthChangedEventTypeValue::WorkerHealthChangedV1,
            worker_id: worker.worker_id.clone(),
        },
        scope: public_event_scope(scope),
        occurred_at: occurred_at.clone(),
        source: PublicEventSource::ControlPlane {
            actor: public_event_actor(actor),
            component: SERVICE_COMPONENT.to_owned(),
        },
    })
}

fn worker_health_event_id(command_digest: &Sha256Digest) -> ControlPlaneEventId {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.worker-health-event.v1\0");
    digest.update(command_digest.0.as_bytes());
    ControlPlaneEventId(format!("evt_{:x}", digest.finalize()))
}

fn worker_event_status(worker: &WorkerManagementSnapshot) -> &'static str {
    match worker.operational_state {
        WorkerOperationalState::Draining => "draining",
        WorkerOperationalState::Offline => "offline",
        WorkerOperationalState::Enabled => match worker.health {
            WorkerHealth::Registered => "registering",
            WorkerHealth::Healthy => "healthy",
            WorkerHealth::TimedOut => "offline",
        },
    }
}

fn validate_prepared_event(
    event: &NewOutboxEvent,
    request: &WorkerHealthEventRequest,
) -> Result<(), WorkerManagementServiceError> {
    let context = event
        .public_context()
        .ok_or_else(WorkerManagementServiceError::invalid_request)?;
    let decoded =
        serde_json::from_slice::<ControlPlaneWebSocketWorkerHealthChangedEvent>(&event.payload)
            .map_err(|_| WorkerManagementServiceError::invalid_request())?;
    if event.event_id != request.event_id.0
        || event.topic != WORKER_HEALTH_CHANGED_TOPIC
        || event.projection_stream() != Some(&ProjectionEventStream::Scope)
        || context.scope() != &request.scope
        || context.occurred_at() != &request.occurred_at
        || context.source() != &request.source
        || decoded != request.event
    {
        return Err(WorkerManagementServiceError::invalid_request());
    }
    Ok(())
}

fn worker_projection(
    worker: &WorkerManagementSnapshot,
) -> Result<WorkerProjection, WorkerManagementServiceError> {
    Ok(WorkerProjection {
        capacity: i64::try_from(worker.capacity)
            .map_err(|_| WorkerManagementServiceError::invalid_request())?,
        id: worker.worker_id.clone(),
        last_heartbeat_at: worker.last_heartbeat_at.clone(),
        revision: revision(worker.revision)?,
        state: match worker.operational_state {
            WorkerOperationalState::Enabled => "enabled",
            WorkerOperationalState::Draining => "draining",
            WorkerOperationalState::Offline => "offline",
        }
        .to_owned(),
    })
}

fn revision(value: u64) -> Result<Revision, WorkerManagementServiceError> {
    i64::try_from(value)
        .map(Revision)
        .map_err(|_| WorkerManagementServiceError::invalid_request())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerPageCursor {
    schema: String,
    scope_sha256: String,
    states_sha256: String,
    worker_id: WorkerId,
    upper_bound_worker_id: WorkerId,
}

fn encode_cursor(
    cursor: &WorkerManagementPageCursor,
    scope: &ReceiptScopeKey,
    states: &[WorkerOperationalState],
) -> Result<OpaqueCursor, WorkerManagementServiceError> {
    let encoded = WorkerPageCursor {
        schema: CURSOR_SCHEMA.to_owned(),
        scope_sha256: digest_bytes(scope.as_bytes()),
        states_sha256: digest_json(states)?,
        worker_id: cursor.worker_id.clone(),
        upper_bound_worker_id: cursor.upper_bound_worker_id.clone(),
    };
    serde_json::to_vec(&encoded)
        .map(|bytes| OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
        .map_err(|_| WorkerManagementServiceError::cursor_invalid())
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
    scope: &ReceiptScopeKey,
    states: &[WorkerOperationalState],
) -> Result<Option<WorkerManagementPageCursor>, WorkerManagementServiceError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.0.len() > MAX_CURSOR_BYTES {
        return Err(WorkerManagementServiceError::cursor_invalid());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.0.as_bytes())
        .map_err(|_| WorkerManagementServiceError::cursor_invalid())?;
    let decoded: WorkerPageCursor = serde_json::from_slice(&bytes)
        .map_err(|_| WorkerManagementServiceError::cursor_invalid())?;
    if decoded.schema != CURSOR_SCHEMA
        || decoded.scope_sha256 != digest_bytes(scope.as_bytes())
        || decoded.states_sha256 != digest_json(states)?
    {
        return Err(WorkerManagementServiceError::cursor_invalid());
    }
    Ok(Some(WorkerManagementPageCursor {
        worker_id: decoded.worker_id,
        upper_bound_worker_id: decoded.upper_bound_worker_id,
    }))
}

fn digest_json<T: Serialize + ?Sized>(value: &T) -> Result<String, WorkerManagementServiceError> {
    serde_json::to_vec(value)
        .map(|bytes| digest_bytes(&bytes))
        .map_err(|_| WorkerManagementServiceError::cursor_invalid())
}

fn digest_bytes(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

fn event_port_error(error: WorkerHealthEventPortError) -> WorkerManagementServiceError {
    match error.kind() {
        WorkerHealthEventPortErrorKind::InvalidEvent => {
            WorkerManagementServiceError::invalid_request()
        }
        WorkerHealthEventPortErrorKind::Unavailable => WorkerManagementServiceError::new(
            WorkerManagementServiceErrorKind::EventUnavailable,
            "Worker health event stream is unavailable",
        ),
    }
}

fn storage_error(error: &StorageError, management: bool) -> WorkerManagementServiceError {
    let kind = match error.kind() {
        StorageErrorKind::RevisionConflict => WorkerManagementServiceErrorKind::RevisionConflict,
        StorageErrorKind::RequestConflict => WorkerManagementServiceErrorKind::RequestConflict,
        StorageErrorKind::InvalidInput if management => {
            return WorkerManagementServiceError::wrong_state();
        }
        StorageErrorKind::InvalidInput => WorkerManagementServiceErrorKind::InvalidRequest,
        StorageErrorKind::RequestReplayMissing
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::EventCursorExpired
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => WorkerManagementServiceErrorKind::Storage,
    };
    WorkerManagementServiceError::new(kind, "Worker management operation failed")
}
