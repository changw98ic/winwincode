// SPDX-License-Identifier: Apache-2.0

//! Authenticated remote Worker connections over the canonical registry.

use std::fmt;

use winwincode_domain::{Instant, Sha256Digest, WorkerId, WorkerInstanceId};
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

const MAX_REMOTE_CREDENTIAL_BYTES: usize = 16 * 1024;

/// Opaque connection credential that is zeroed when released.
///
/// Debug and serialization are intentionally omitted so an authentication
/// proof cannot enter diagnostics or durable state by accident.
pub struct RemoteWorkerCredential {
    bytes: Vec<u8>,
}

impl RemoteWorkerCredential {
    /// Wraps one bounded non-empty transport authentication proof.
    ///
    /// # Errors
    ///
    /// Rejects empty or oversized proofs before an authenticator is called.
    pub fn new(bytes: Vec<u8>) -> Result<Self, RemoteWorkerAuthenticationError> {
        if bytes.is_empty() || bytes.len() > MAX_REMOTE_CREDENTIAL_BYTES {
            return Err(RemoteWorkerAuthenticationError::rejected());
        }
        Ok(Self { bytes })
    }

    /// Exposes proof bytes only to the injected authentication implementation.
    #[must_use]
    pub fn expose_for_verification(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for RemoteWorkerCredential {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

/// Stable authentication rejection category without provider diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteWorkerAuthenticationErrorKind {
    Rejected,
    Revoked,
    Unavailable,
}

/// Secret-free error returned by an injected connection authenticator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteWorkerAuthenticationError {
    kind: RemoteWorkerAuthenticationErrorKind,
}

impl RemoteWorkerAuthenticationError {
    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            kind: RemoteWorkerAuthenticationErrorKind::Rejected,
        }
    }

    #[must_use]
    pub const fn revoked() -> Self {
        Self {
            kind: RemoteWorkerAuthenticationErrorKind::Revoked,
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: RemoteWorkerAuthenticationErrorKind::Unavailable,
        }
    }

    #[must_use]
    pub const fn kind(self) -> RemoteWorkerAuthenticationErrorKind {
        self.kind
    }
}

impl fmt::Display for RemoteWorkerAuthenticationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("remote Worker authentication failed")
    }
}

impl std::error::Error for RemoteWorkerAuthenticationError {}

/// Secret-free identity and pool authority returned after credential checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteWorkerPrincipal {
    worker_id: WorkerId,
    worker_pool_id: WorkerPoolId,
    scope: WorkerRegistryScope,
    issuer: String,
    subject: String,
    credential_fingerprint: Sha256Digest,
    security_zone: String,
}

impl RemoteWorkerPrincipal {
    /// Constructs the only identity accepted by a remote pool connection.
    ///
    /// # Errors
    ///
    /// Rejects malformed IDs, scope, fingerprint, or bounded transport text.
    pub fn new(
        worker_id: WorkerId,
        worker_pool_id: WorkerPoolId,
        scope: WorkerRegistryScope,
        issuer: String,
        subject: String,
        credential_fingerprint: Sha256Digest,
        security_zone: String,
    ) -> Result<Self, RemoteWorkerAuthenticationError> {
        let principal = Self {
            worker_id,
            worker_pool_id,
            scope,
            issuer,
            subject,
            credential_fingerprint,
            security_zone,
        };
        validate_principal(&principal)?;
        Ok(principal)
    }

    #[must_use]
    pub const fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    #[must_use]
    pub const fn worker_pool_id(&self) -> &WorkerPoolId {
        &self.worker_pool_id
    }

    #[must_use]
    pub const fn scope(&self) -> &WorkerRegistryScope {
        &self.scope
    }

    fn authentication_identity(&self) -> WorkerAuthenticationIdentity {
        WorkerAuthenticationIdentity::TransportPrincipal {
            issuer: self.issuer.clone(),
            subject: self.subject.clone(),
            credential_fingerprint: self.credential_fingerprint.clone(),
        }
    }
}

/// Injected enterprise identity authority.
///
/// `authenticate` validates the connection proof once. `ensure_active` is
/// called before every Worker message so revocation closes an existing
/// connection instead of waiting for a reconnect.
pub trait RemoteWorkerAuthenticator: Send + Sync {
    /// Resolves one proof to a secret-free exact Worker principal.
    ///
    /// # Errors
    ///
    /// Returns a bounded rejected, revoked, or unavailable result.
    fn authenticate(
        &self,
        credential: &RemoteWorkerCredential,
        now: &Instant,
    ) -> Result<RemoteWorkerPrincipal, RemoteWorkerAuthenticationError>;

    /// Revalidates an established principal against current revocation state.
    ///
    /// # Errors
    ///
    /// Returns revoked when the connection must be closed immediately.
    fn ensure_active(
        &self,
        principal: &RemoteWorkerPrincipal,
        now: &Instant,
    ) -> Result<(), RemoteWorkerAuthenticationError>;
}

/// In-memory lifecycle of one authenticated transport connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteWorkerConnectionState {
    Authenticated,
    Registered,
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisteredWorkerBinding {
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
}

/// One short-lived connection handle; durable identity remains in the Worker Registry.
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

/// Stable remote pool adapter error categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteWorkerPoolErrorKind {
    AuthenticationRejected,
    AuthenticationRevoked,
    AuthenticationUnavailable,
    InvalidConnection,
    UnsupportedMessage,
    Registry,
}

/// Bounded adapter failure that never contains credential or provider text.
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

/// Transport-neutral remote pool adapter over the canonical Execution Port.
pub struct RemoteWorkerPoolAdapter<'storage, 'authenticator> {
    storage: &'storage mut SqliteStorage,
    authenticator: &'authenticator dyn RemoteWorkerAuthenticator,
    heartbeat_interval_ms: i64,
}

impl<'storage, 'authenticator> RemoteWorkerPoolAdapter<'storage, 'authenticator> {
    /// Uses the canonical heartbeat interval.
    #[must_use]
    pub const fn new(
        storage: &'storage mut SqliteStorage,
        authenticator: &'authenticator dyn RemoteWorkerAuthenticator,
    ) -> Self {
        Self {
            storage,
            authenticator,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
        }
    }

    /// Uses an explicit one-second to five-minute heartbeat interval.
    ///
    /// # Errors
    ///
    /// Rejects values outside the canonical Execution Port range.
    pub fn with_heartbeat_interval(
        storage: &'storage mut SqliteStorage,
        authenticator: &'authenticator dyn RemoteWorkerAuthenticator,
        heartbeat_interval_ms: i64,
    ) -> Result<Self, RemoteWorkerPoolError> {
        if !(1_000..=300_000).contains(&heartbeat_interval_ms) {
            return Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::InvalidConnection,
            ));
        }
        Ok(Self {
            storage,
            authenticator,
            heartbeat_interval_ms,
        })
    }

    /// Authenticates a new connection without persisting its proof.
    ///
    /// # Errors
    ///
    /// Returns a stable authentication result from the injected authority.
    pub fn connect(
        &self,
        credential: &RemoteWorkerCredential,
        now: &Instant,
    ) -> Result<RemoteWorkerConnection, RemoteWorkerPoolError> {
        let principal = self
            .authenticator
            .authenticate(credential, now)
            .map_err(authentication_error)?;
        Ok(RemoteWorkerConnection {
            principal,
            state: RemoteWorkerConnectionState::Authenticated,
            binding: None,
        })
    }

    /// Re-authenticates a transport request and restores its exact durable
    /// Worker process binding after the previous network connection ended.
    ///
    /// This does not register a new process and does not change Registry
    /// health.  The credential principal, Worker record, and authenticated
    /// pool placement must all describe the same process.
    ///
    /// # Errors
    ///
    /// Rejects revoked credentials and any principal, instance, pool, scope,
    /// or authentication identity mismatch.
    pub fn resume(
        &mut self,
        credential: &RemoteWorkerCredential,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        now: &Instant,
    ) -> Result<RemoteWorkerConnection, RemoteWorkerPoolError> {
        let principal = self
            .authenticator
            .authenticate(credential, now)
            .map_err(authentication_error)?;
        if principal.worker_id() != worker_id {
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
        if worker.worker_instance_id != *worker_instance_id
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

    /// Revalidates one registered request before its canonical frame is
    /// forwarded to the shared Execution Port ingress.
    ///
    /// # Errors
    ///
    /// Rejects a revoked identity, disconnected connection, or foreign Worker
    /// process binding.
    pub fn authorize_registered_message(
        &mut self,
        connection: &mut RemoteWorkerConnection,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        now: &Instant,
    ) -> Result<(), RemoteWorkerPoolError> {
        self.ensure_connection_active(connection, now)?;
        require_binding(connection, worker_id, worker_instance_id)
    }

    /// Applies one generated Worker message through the canonical Execution Port.
    ///
    /// # Errors
    ///
    /// Rejects revoked identities, foreign process identities, unsupported
    /// messages, protocol failures, and unavailable durable registry state.
    pub fn accept(
        &mut self,
        connection: &mut RemoteWorkerConnection,
        message: &ExecutionPortMessage,
        now: &Instant,
    ) -> Result<ExecutionPortMessage, RemoteWorkerPoolError> {
        self.ensure_connection_active(connection, now)?;
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

    /// Explicitly closes a connection and marks its exact registered process offline.
    ///
    /// # Errors
    ///
    /// Returns a registry error when durable health cannot be updated.
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
        &mut self,
        connection: &mut RemoteWorkerConnection,
        now: &Instant,
    ) -> Result<(), RemoteWorkerPoolError> {
        if connection.state == RemoteWorkerConnectionState::Disconnected {
            return Err(RemoteWorkerPoolError::new(
                RemoteWorkerPoolErrorKind::InvalidConnection,
            ));
        }
        if let Err(error) = self.authenticator.ensure_active(&connection.principal, now) {
            connection.state = RemoteWorkerConnectionState::Disconnected;
            self.disconnect_registered_binding(connection)?;
            return Err(authentication_error(error));
        }
        Ok(())
    }

    fn register(
        &mut self,
        connection: &mut RemoteWorkerConnection,
        message: &WorkerRegisterMessage,
        now: &Instant,
    ) -> Result<WorkerRegistrationResultMessage, RemoteWorkerPoolError> {
        if message.worker_id != *connection.principal.worker_id() {
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
                connection.principal.security_zone.clone(),
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

fn authentication_error(error: RemoteWorkerAuthenticationError) -> RemoteWorkerPoolError {
    let kind = match error.kind() {
        RemoteWorkerAuthenticationErrorKind::Rejected => {
            RemoteWorkerPoolErrorKind::AuthenticationRejected
        }
        RemoteWorkerAuthenticationErrorKind::Revoked => {
            RemoteWorkerPoolErrorKind::AuthenticationRevoked
        }
        RemoteWorkerAuthenticationErrorKind::Unavailable => {
            RemoteWorkerPoolErrorKind::AuthenticationUnavailable
        }
    };
    RemoteWorkerPoolError::new(kind)
}

fn execution_port_error(error: &ExecutionPortServiceError) -> RemoteWorkerPoolError {
    let kind = match error {
        ExecutionPortServiceError::UnsupportedMessage => {
            RemoteWorkerPoolErrorKind::UnsupportedMessage
        }
        ExecutionPortServiceError::Storage(_)
        | ExecutionPortServiceError::ClaimRejected(_)
        | ExecutionPortServiceError::EnterpriseQuotaRejected
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

fn validate_principal(
    principal: &RemoteWorkerPrincipal,
) -> Result<(), RemoteWorkerAuthenticationError> {
    canonical_id(&principal.worker_id.0, "wrk_")?;
    canonical_id(&principal.worker_pool_id.0, "wpl_")?;
    validate_scope(&principal.scope)?;
    for value in [
        principal.issuer.as_str(),
        principal.subject.as_str(),
        principal.security_zone.as_str(),
    ] {
        if value.is_empty()
            || value.len() > 200
            || value.trim() != value
            || !value.is_ascii()
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(RemoteWorkerAuthenticationError::rejected());
        }
    }
    let digest = principal
        .credential_fingerprint
        .0
        .strip_prefix("sha256:")
        .ok_or_else(RemoteWorkerAuthenticationError::rejected)?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RemoteWorkerAuthenticationError::rejected());
    }
    Ok(())
}

fn validate_scope(scope: &WorkerRegistryScope) -> Result<(), RemoteWorkerAuthenticationError> {
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

fn canonical_id(value: &str, prefix: &str) -> Result<(), RemoteWorkerAuthenticationError> {
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
        Err(RemoteWorkerAuthenticationError::rejected())
    }
}
