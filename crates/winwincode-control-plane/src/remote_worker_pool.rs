// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use winwincode_domain::{Instant, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::{
    ExecutionPortMessage, JobDispatchResultMessage, WorkerHeartbeatAckMessage,
    WorkerHeartbeatMessage, WorkerRegisterMessage, WorkerRegistrationResultMessage,
    WorkerRegistrationResultMessageStatus,
};
use winwincode_storage::{
    ExecutionRegistry, SqliteStorage, StorageError, WorkerAuthenticationIdentity, WorkerPoolId,
    WorkerRegistryScope,
};

use crate::{
    ActionPolicyEnforcementErrorKind, DEFAULT_HEARTBEAT_INTERVAL_MS, ExecutionPortService,
    ExecutionPortServiceError,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteWorkerPrincipal {
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_pool_id: WorkerPoolId,
    scope: WorkerRegistryScope,
    client_node_id: String,
    worker_session_id: String,
}

impl RemoteWorkerPrincipal {
    pub fn new(
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        worker_pool_id: WorkerPoolId,
        scope: WorkerRegistryScope,
        client_node_id: String,
        worker_session_id: String,
    ) -> Result<Self, RemoteWorkerPoolError> {
        canonical_id(&worker_id.0, "wrk_")?;
        canonical_id(&worker_instance_id.0, "wki_")?;
        canonical_id(&worker_pool_id.0, "wpl_")?;
        canonical_id(&client_node_id, "cnd_")?;
        canonical_id(&worker_session_id, "wsn_")?;
        validate_scope(&scope)?;
        Ok(Self {
            worker_id,
            worker_instance_id,
            worker_pool_id,
            scope,
            client_node_id,
            worker_session_id,
        })
    }

    #[must_use]
    pub const fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    #[must_use]
    pub const fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.worker_instance_id
    }

    #[must_use]
    pub const fn worker_pool_id(&self) -> &WorkerPoolId {
        &self.worker_pool_id
    }

    #[must_use]
    pub const fn scope(&self) -> &WorkerRegistryScope {
        &self.scope
    }

    #[must_use]
    pub fn worker_session_id(&self) -> &str {
        &self.worker_session_id
    }

    fn authentication_identity(&self) -> WorkerAuthenticationIdentity {
        WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: format!("device:{}", self.client_node_id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteWorkerConnectionState {
    Connected,
    Registered,
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisteredWorkerBinding {
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
}

pub struct RemoteWorkerConnection {
    principal: RemoteWorkerPrincipal,
    state: RemoteWorkerConnectionState,
    binding: Option<RegisteredWorkerBinding>,
}

impl RemoteWorkerConnection {
    #[must_use]
    pub const fn state(&self) -> RemoteWorkerConnectionState {
        self.state
    }

    #[must_use]
    pub const fn principal(&self) -> &RemoteWorkerPrincipal {
        &self.principal
    }

    #[must_use]
    pub fn registered_instance(&self) -> Option<&WorkerInstanceId> {
        self.binding
            .as_ref()
            .map(|binding| &binding.worker_instance_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteWorkerPoolErrorKind {
    InvalidConnection,
    UnsupportedMessage,
    Registry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteWorkerPoolError {
    kind: RemoteWorkerPoolErrorKind,
}

impl RemoteWorkerPoolError {
    const fn new(kind: RemoteWorkerPoolErrorKind) -> Self {
        Self { kind }
    }

    #[must_use]
    pub const fn kind(self) -> RemoteWorkerPoolErrorKind {
        self.kind
    }
}

impl fmt::Display for RemoteWorkerPoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("remote Worker pool operation failed")
    }
}

impl std::error::Error for RemoteWorkerPoolError {}

pub struct RemoteWorkerPoolAdapter<'storage> {
    storage: &'storage mut SqliteStorage,
    heartbeat_interval_ms: i64,
}

impl<'storage> RemoteWorkerPoolAdapter<'storage> {
    #[must_use]
    pub const fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self {
            storage,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
        }
    }

    pub fn with_heartbeat_interval(
        storage: &'storage mut SqliteStorage,
        heartbeat_interval_ms: i64,
    ) -> Result<Self, RemoteWorkerPoolError> {
        if !(1_000..=300_000).contains(&heartbeat_interval_ms) {
            return Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::InvalidConnection,
            ));
        }
        Ok(Self {
            storage,
            heartbeat_interval_ms,
        })
    }

    pub fn connect(&self, principal: RemoteWorkerPrincipal) -> RemoteWorkerConnection {
        RemoteWorkerConnection {
            principal,
            state: RemoteWorkerConnectionState::Connected,
            binding: None,
        }
    }

    pub fn resume(
        &mut self,
        principal: RemoteWorkerPrincipal,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
    ) -> Result<RemoteWorkerConnection, RemoteWorkerPoolError> {
        if principal.worker_id() != worker_id
            || principal.worker_instance_id() != worker_instance_id
        {
            return Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::InvalidConnection,
            ));
        }
        let registry = ExecutionRegistry::new(self.storage).map_err(registry_error)?;
        let worker = registry
            .load_worker(worker_id)
            .map_err(registry_error)?
            .ok_or_else(|| {
                RemoteWorkerPoolError::new(RemoteWorkerPoolErrorKind::InvalidConnection)
            })?;
        let placement = registry
            .load_authenticated_worker_placement(worker_id, worker_instance_id)
            .map_err(registry_error)?
            .ok_or_else(|| {
                RemoteWorkerPoolError::new(RemoteWorkerPoolErrorKind::InvalidConnection)
            })?;
        if worker.protocol_version != winwincode_storage::EXECUTION_PROTOCOL_VERSION
            || worker.worker_instance_id != *worker_instance_id
            || worker.management_scope != *principal.scope()
            || worker.authentication_identity != principal.authentication_identity()
            || placement.worker_pool_id != *principal.worker_pool_id()
            || placement.management_scope != *principal.scope()
            || placement.authentication_identity != principal.authentication_identity()
        {
            return Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::InvalidConnection,
            ));
        }
        Ok(RemoteWorkerConnection {
            principal,
            state: RemoteWorkerConnectionState::Registered,
            binding: Some(RegisteredWorkerBinding {
                worker_id: worker_id.clone(),
                worker_instance_id: worker_instance_id.clone(),
            }),
        })
    }

    pub fn authorize_registered_message(
        &mut self,
        connection: &mut RemoteWorkerConnection,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        _now: &Instant,
    ) -> Result<(), RemoteWorkerPoolError> {
        Self::ensure_connection_active(connection)?;
        require_binding(connection, worker_id, worker_instance_id)
    }

    pub fn accept(
        &mut self,
        connection: &mut RemoteWorkerConnection,
        message: &ExecutionPortMessage,
        now: &Instant,
    ) -> Result<ExecutionPortMessage, RemoteWorkerPoolError> {
        Self::ensure_connection_active(connection)?;
        match message {
            ExecutionPortMessage::WorkerRegisterMessage(message) => self
                .register(connection, message, now)
                .map(ExecutionPortMessage::WorkerRegistrationResultMessage),
            ExecutionPortMessage::WorkerHeartbeatMessage(message) => self
                .heartbeat(connection, message, now)
                .map(ExecutionPortMessage::WorkerHeartbeatAckMessage),
            ExecutionPortMessage::JobDispatchResultMessage(message) => self
                .dispatch_result(connection, message, now)
                .map(ExecutionPortMessage::JobDispatchResultMessage),
            _ => Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::UnsupportedMessage,
            )),
        }
    }

    pub fn disconnect(
        &mut self,
        connection: &mut RemoteWorkerConnection,
    ) -> Result<bool, RemoteWorkerPoolError> {
        if connection.state == RemoteWorkerConnectionState::Disconnected {
            return Ok(false);
        }
        connection.state = RemoteWorkerConnectionState::Disconnected;
        let changed = self.disconnect_registered_binding(connection)?;
        Ok(changed)
    }

    fn ensure_connection_active(
        connection: &RemoteWorkerConnection,
    ) -> Result<(), RemoteWorkerPoolError> {
        if connection.state == RemoteWorkerConnectionState::Disconnected {
            return Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::InvalidConnection,
            ));
        }
        Ok(())
    }

    fn register(
        &mut self,
        connection: &mut RemoteWorkerConnection,
        message: &WorkerRegisterMessage,
        now: &Instant,
    ) -> Result<WorkerRegistrationResultMessage, RemoteWorkerPoolError> {
        if message.worker_id != *connection.principal.worker_id()
            || connection.principal.worker_instance_id() != &message.worker_instance_id
        {
            return Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::InvalidConnection,
            ));
        }
        if let Some(binding) = &connection.binding
            && (binding.worker_id != message.worker_id
                || binding.worker_instance_id != message.worker_instance_id)
        {
            return Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::InvalidConnection,
            ));
        }
        let mut service = ExecutionPortService::with_heartbeat_interval(
            self.storage,
            now.clone(),
            self.heartbeat_interval_ms,
        )
        .map_err(|error| execution_port_error(&error))?;
        let response = service
            .register_authenticated_remote_worker(
                message,
                connection.principal.authentication_identity(),
                connection.principal.scope(),
                connection.principal.worker_pool_id(),
                "device-client".to_owned(),
            )
            .map_err(|error| execution_port_error(&error))?;
        if matches!(
            response.status,
            WorkerRegistrationResultMessageStatus::Accepted
                | WorkerRegistrationResultMessageStatus::Duplicate
        ) {
            connection.binding = Some(RegisteredWorkerBinding {
                worker_id: response.worker_id.clone(),
                worker_instance_id: response.worker_instance_id.clone(),
            });
            connection.state = RemoteWorkerConnectionState::Registered;
        }
        Ok(response)
    }

    fn heartbeat(
        &mut self,
        connection: &RemoteWorkerConnection,
        message: &WorkerHeartbeatMessage,
        now: &Instant,
    ) -> Result<WorkerHeartbeatAckMessage, RemoteWorkerPoolError> {
        require_binding(connection, &message.worker_id, &message.worker_instance_id)?;
        ExecutionPortService::with_heartbeat_interval(
            self.storage,
            now.clone(),
            self.heartbeat_interval_ms,
        )
        .map_err(|error| execution_port_error(&error))?
        .record_heartbeat(message)
        .map_err(|error| execution_port_error(&error))
    }

    fn dispatch_result(
        &mut self,
        connection: &RemoteWorkerConnection,
        message: &JobDispatchResultMessage,
        now: &Instant,
    ) -> Result<JobDispatchResultMessage, RemoteWorkerPoolError> {
        require_binding(
            connection,
            &message.lease.worker_id,
            &message.lease.worker_instance_id,
        )?;
        ExecutionPortService::with_heartbeat_interval(
            self.storage,
            now.clone(),
            self.heartbeat_interval_ms,
        )
        .map_err(|error| execution_port_error(&error))?
        .accept_dispatch_result(message.clone())
        .map_err(|error| execution_port_error(&error))
    }

    fn disconnect_registered_binding(
        &mut self,
        connection: &RemoteWorkerConnection,
    ) -> Result<bool, RemoteWorkerPoolError> {
        let Some(binding) = &connection.binding else {
            return Ok(false);
        };
        let worker = ExecutionRegistry::new(self.storage)
            .map_err(registry_error)?
            .mark_worker_disconnected(&binding.worker_id, &binding.worker_instance_id)
            .map_err(registry_error)?;
        Ok(worker.is_some())
    }
}

fn require_binding(
    connection: &RemoteWorkerConnection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<(), RemoteWorkerPoolError> {
    match &connection.binding {
        Some(binding)
            if connection.state == RemoteWorkerConnectionState::Registered
                && binding.worker_id == *worker_id
                && binding.worker_instance_id == *worker_instance_id =>
        {
            Ok(())
        }
        _ => Err(RemoteWorkerPoolError::new(
            RemoteWorkerPoolErrorKind::InvalidConnection,
        )),
    }
}

fn execution_port_error(error: &ExecutionPortServiceError) -> RemoteWorkerPoolError {
    let kind = match error {
        ExecutionPortServiceError::UnsupportedMessage => {
            RemoteWorkerPoolErrorKind::UnsupportedMessage
        }
        ExecutionPortServiceError::Storage(_)
        | ExecutionPortServiceError::ClaimRejected(_)
        | ExecutionPortServiceError::WorkerLifecycle(_)
        | ExecutionPortServiceError::AuthorityRejected(_)
        | ExecutionPortServiceError::JobMismatch(_) => RemoteWorkerPoolErrorKind::Registry,
        ExecutionPortServiceError::ActionPolicy(error) => match error.kind() {
            ActionPolicyEnforcementErrorKind::InvalidRequest
            | ActionPolicyEnforcementErrorKind::AuthorityRejected => {
                RemoteWorkerPoolErrorKind::InvalidConnection
            }
            ActionPolicyEnforcementErrorKind::Policy
            | ActionPolicyEnforcementErrorKind::Storage
            | ActionPolicyEnforcementErrorKind::CorruptReceipt => {
                RemoteWorkerPoolErrorKind::Registry
            }
        },
        ExecutionPortServiceError::Protocol(_) => RemoteWorkerPoolErrorKind::InvalidConnection,
    };
    RemoteWorkerPoolError::new(kind)
}

fn registry_error(_error: StorageError) -> RemoteWorkerPoolError {
    RemoteWorkerPoolError::new(RemoteWorkerPoolErrorKind::Registry)
}

fn validate_scope(scope: &WorkerRegistryScope) -> Result<(), RemoteWorkerPoolError> {
    match scope {
        WorkerRegistryScope::Organization { organization_id } => {
            canonical_id(&organization_id.0, "org_")
        }
        WorkerRegistryScope::Workspace {
            organization_id,
            workspace_id,
        } => {
            canonical_id(&organization_id.0, "org_")?;
            canonical_id(&workspace_id.0, "wsp_")
        }
        WorkerRegistryScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            canonical_id(&organization_id.0, "org_")?;
            canonical_id(&workspace_id.0, "wsp_")?;
            canonical_id(&project_id.0, "prj_")
        }
        WorkerRegistryScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            canonical_id(&organization_id.0, "org_")?;
            canonical_id(&workspace_id.0, "wsp_")?;
            canonical_id(&project_id.0, "prj_")?;
            canonical_id(&repository_id.0, "rep_")
        }
    }
}

fn canonical_id(value: &str, prefix: &str) -> Result<(), RemoteWorkerPoolError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    });
    if valid {
        Ok(())
    } else {
        Err(RemoteWorkerPoolError::new(
            RemoteWorkerPoolErrorKind::InvalidConnection,
        ))
    }
}
