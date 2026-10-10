// SPDX-License-Identifier: Apache-2.0

//! Authenticated HTTPS exchange for a separated Execution Worker.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use winwincode_control_plane::{
    ProductStateStorage, RemoteWorkerAuthenticationError, RemoteWorkerAuthenticator,
    RemoteWorkerCredential, RemoteWorkerPoolAdapter, RemoteWorkerPoolError,
    RemoteWorkerPoolErrorKind, RemoteWorkerPrincipal,
};
use winwincode_domain::{ExecutionMessageId, Instant, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_execution_port::transport::{
    EndpointSide, ExecutionPortCore, FrameDirection, FrameError, RemoteExchangeDelivery,
    RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter, TypedFrame,
    execution_message_id,
};
use winwincode_storage::{
    SqliteStorage, WorkerPoolId, WorkerRegistryScope, WorkerSessionCredentialState,
};

use crate::{
    RepositoryRuntimeScheduler, RuntimeControlOutbound, RuntimeSupervisorError,
    WorkerSessionCredentialErrorKind, WorkerSessionCredentialService,
};

const MAX_PENDING_REMOTE_DELIVERIES: usize = 256;

/// Stable, secret-free remote transport failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteWorkerTransportError {
    message: &'static str,
    status: u16,
}

impl RemoteWorkerTransportError {
    const fn new(message: &'static str) -> Self {
        Self {
            message,
            status: 503,
        }
    }
    const fn invalid_frame() -> Self {
        Self {
            message: "remote Worker frame is invalid",
            status: 400,
        }
    }
    #[allow(
        clippy::needless_pass_by_value,
        reason = "owned error callback for Result::map_err"
    )]
    fn frame_error(error: FrameError) -> Self {
        if matches!(error, FrameError::TooLarge) {
            Self {
                message: "remote Worker message exceeds wire budget",
                status: 413,
            }
        } else {
            Self::invalid_frame()
        }
    }
    const fn authentication() -> Self {
        Self {
            message: "remote Worker authentication rejected",
            status: 401,
        }
    }
    fn from_pool(error: RemoteWorkerPoolError) -> Self {
        match error.kind() {
            RemoteWorkerPoolErrorKind::AuthenticationRejected
            | RemoteWorkerPoolErrorKind::AuthenticationRevoked => Self::authentication(),
            RemoteWorkerPoolErrorKind::InvalidConnection
            | RemoteWorkerPoolErrorKind::UnsupportedMessage => Self {
                message: "remote Worker frame rejected",
                status: 400,
            },
            RemoteWorkerPoolErrorKind::AuthenticationUnavailable
            | RemoteWorkerPoolErrorKind::Registry => {
                Self::new("remote Worker authority is unavailable")
            }
        }
    }
    /// HTTP category distinguishes permanent rejection from retryable authority failures.
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        self.status
    }
}

impl fmt::Display for RemoteWorkerTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for RemoteWorkerTransportError {}

/// HTTP boundary used by the Server route without exposing the route through
/// the generated public API.
pub trait RemoteWorkerExchangePort: Send + Sync {
    /// Financial-only reconciliation under the explicit trusted Device key.
    /// # Errors
    /// Rejects missing authority, altered receipts, or unavailable accounting.
    fn accounting(
        &self,
        _token: &str,
        _body: Option<&[u8]>,
        _offset: u64,
        _now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
        Err(RemoteWorkerTransportError::authentication())
    }
    /// Applies one authenticated bounded exchange.
    ///
    /// # Errors
    ///
    /// Returns only a stable category; credentials and frame contents are
    /// never included in diagnostics.
    fn exchange(
        &self,
        credential: Vec<u8>,
        request_body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError>;
}

/// Credential authority backed by one operator-owned mode-0600 file.
pub struct FileRemoteWorkerAuthenticator {
    credential_path: PathBuf,
    principal: RemoteWorkerPrincipal,
    credential_fingerprint: winwincode_domain::Sha256Digest,
    expires_at: Instant,
}

impl FileRemoteWorkerAuthenticator {
    /// Loads only a SHA-256 fingerprint from a mode-0600 credential file.
    ///
    /// # Errors
    ///
    /// Rejects missing, empty, oversized, broadly-readable, or expired
    /// credential configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        credential_path: impl Into<PathBuf>,
        worker_id: WorkerId,
        worker_pool_id: WorkerPoolId,
        scope: WorkerRegistryScope,
        issuer: String,
        subject: String,
        security_zone: String,
        expires_at: Instant,
        now: &Instant,
    ) -> Result<Self, RemoteWorkerTransportError> {
        let expires_at_value = time::OffsetDateTime::parse(
            &expires_at.0,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker credential expiry is invalid")
        })?;
        let now_value =
            time::OffsetDateTime::parse(&now.0, &time::format_description::well_known::Rfc3339)
                .map_err(|_| RemoteWorkerTransportError::new("remote Worker clock is invalid"))?;
        if expires_at_value <= now_value {
            return Err(RemoteWorkerTransportError::new(
                "remote Worker credential is expired",
            ));
        }
        let credential_path = credential_path.into();
        let proof = read_private_credential(&credential_path)?;
        let fingerprint =
            winwincode_domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(&proof)));
        let principal = RemoteWorkerPrincipal::new(
            worker_id,
            worker_pool_id,
            scope,
            issuer,
            subject,
            fingerprint.clone(),
            security_zone,
        )
        .map_err(|_| RemoteWorkerTransportError::new("remote Worker identity is invalid"))?;
        Ok(Self {
            credential_path,
            principal,
            credential_fingerprint: fingerprint,
            expires_at,
        })
    }

    fn current_fingerprint(
        &self,
    ) -> Result<winwincode_domain::Sha256Digest, RemoteWorkerAuthenticationError> {
        let proof = read_private_credential(&self.credential_path)
            .map_err(|_| RemoteWorkerAuthenticationError::unavailable())?;
        Ok(winwincode_domain::Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(&proof)
        )))
    }
}

impl RemoteWorkerAuthenticator for FileRemoteWorkerAuthenticator {
    fn authenticate(
        &self,
        credential: &RemoteWorkerCredential,
        now: &Instant,
    ) -> Result<RemoteWorkerPrincipal, RemoteWorkerAuthenticationError> {
        if now.0 >= self.expires_at.0 {
            return Err(RemoteWorkerAuthenticationError::revoked());
        }
        let supplied = winwincode_domain::Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(credential.expose_for_verification())
        ));
        if supplied != self.credential_fingerprint || supplied != self.current_fingerprint()? {
            return Err(RemoteWorkerAuthenticationError::rejected());
        }
        Ok(self.principal.clone())
    }

    fn ensure_active(
        &self,
        principal: &RemoteWorkerPrincipal,
        now: &Instant,
    ) -> Result<(), RemoteWorkerAuthenticationError> {
        if now.0 >= self.expires_at.0 || principal != &self.principal {
            return Err(RemoteWorkerAuthenticationError::revoked());
        }
        if self.current_fingerprint()? != self.credential_fingerprint {
            return Err(RemoteWorkerAuthenticationError::revoked());
        }
        Ok(())
    }
}

/// Credential authority for Device Client-launched Worker sessions.
pub struct WorkerSessionRemoteAuthenticator {
    data_directory: PathBuf,
    scope: WorkerRegistryScope,
}

impl WorkerSessionRemoteAuthenticator {
    #[must_use]
    pub fn new(data_directory: impl Into<PathBuf>, scope: WorkerRegistryScope) -> Self {
        Self {
            data_directory: data_directory.into(),
            scope,
        }
    }
}

impl RemoteWorkerAuthenticator for WorkerSessionRemoteAuthenticator {
    fn authenticate(
        &self,
        credential: &RemoteWorkerCredential,
        now: &Instant,
    ) -> Result<RemoteWorkerPrincipal, RemoteWorkerAuthenticationError> {
        let mut storage = SqliteStorage::open(&self.data_directory)
            .map_err(|_| RemoteWorkerAuthenticationError::unavailable())?;
        let record = WorkerSessionCredentialService::new(&mut storage)
            .verify_credential(credential.expose_for_verification(), now)
            .map_err(|error| session_authentication_error(&error))?;
        let worker_pool_id = device_session_pool(&mut storage, &record)?;
        RemoteWorkerPrincipal::new_bound(
            WorkerId(record.worker_id),
            WorkerInstanceId(record.worker_instance_id),
            worker_pool_id,
            self.scope.clone(),
            "winwincode-server".to_owned(),
            format!("worker-session:{}", record.worker_session_id),
            winwincode_domain::Sha256Digest(record.credential_digest),
            "device-client".to_owned(),
        )
    }

    fn ensure_active(
        &self,
        principal: &RemoteWorkerPrincipal,
        now: &Instant,
    ) -> Result<(), RemoteWorkerAuthenticationError> {
        if principal.scope() != &self.scope {
            return Err(RemoteWorkerAuthenticationError::revoked());
        }
        let mut storage = SqliteStorage::open(&self.data_directory)
            .map_err(|_| RemoteWorkerAuthenticationError::unavailable())?;
        let record = storage
            .worker_session_credential_ledger()
            .map_err(|_| RemoteWorkerAuthenticationError::unavailable())?
            .find_by_digest(&principal.credential_fingerprint().0)
            .map_err(|_| RemoteWorkerAuthenticationError::unavailable())?
            .ok_or_else(RemoteWorkerAuthenticationError::revoked)?;
        if principal.worker_pool_id() != &device_session_pool(&mut storage, &record)?
            || record.state != WorkerSessionCredentialState::Active
            || record.expires_at.0 <= now.0
            || record.worker_id != principal.worker_id().0
            || principal.worker_instance_id().map(|id| id.0.as_str())
                != Some(record.worker_instance_id.as_str())
        {
            return Err(RemoteWorkerAuthenticationError::revoked());
        }
        Ok(())
    }
}

fn device_session_pool(
    storage: &mut SqliteStorage,
    record: &winwincode_storage::WorkerSessionCredentialRecord,
) -> Result<WorkerPoolId, RemoteWorkerAuthenticationError> {
    let grant = winwincode_control_plane::WorkerLaunchGrantService::new(storage)
        .snapshot(&record.worker_launch_grant_id)
        .map_err(|_| RemoteWorkerAuthenticationError::unavailable())?
        .ok_or_else(RemoteWorkerAuthenticationError::revoked)?;
    let lease = winwincode_control_plane::ClientOccupancyService::new(storage)
        .active_lease_for_node(&grant.client_node_id)
        .map_err(|_| RemoteWorkerAuthenticationError::unavailable())?
        .ok_or_else(RemoteWorkerAuthenticationError::revoked)?;
    let occupancy_authorized = match lease.state {
        winwincode_control_plane::OccupancyLeaseState::Occupied
        | winwincode_control_plane::OccupancyLeaseState::Draining => true,
        winwincode_control_plane::OccupancyLeaseState::RecoveryPending => {
            grant.state == winwincode_storage::WorkerLaunchGrantState::Consumed
        }
        _ => false,
    };
    if !grant.state.is_non_terminal()
        || lease.occupancy_lease_id != grant.occupancy_lease_id
        || lease.fencing_token != grant.occupancy_fencing_token
        || !occupancy_authorized
    {
        return Err(RemoteWorkerAuthenticationError::revoked());
    }
    if grant.worker_session_id != record.worker_session_id
        || grant.worker_id != record.worker_id
        || grant.worker_instance_id != record.worker_instance_id
    {
        return Err(RemoteWorkerAuthenticationError::revoked());
    }
    Ok(WorkerPoolId(
        if grant.work_run_id.is_some() {
            winwincode_control_plane::STRONGFLOW_DEVICE_WORKER_POOL_ID
        } else {
            winwincode_control_plane::QUICK_DEVICE_WORKER_POOL_ID
        }
        .to_owned(),
    ))
}

/// Keeps the configured fleet Worker and Device-launched Workers on the one
/// canonical Execution Port endpoint.
pub struct CompositeRemoteWorkerAuthenticator {
    fleet: FileRemoteWorkerAuthenticator,
    sessions: WorkerSessionRemoteAuthenticator,
}

impl CompositeRemoteWorkerAuthenticator {
    #[must_use]
    pub const fn new(
        fleet: FileRemoteWorkerAuthenticator,
        sessions: WorkerSessionRemoteAuthenticator,
    ) -> Self {
        Self { fleet, sessions }
    }
}

impl RemoteWorkerAuthenticator for CompositeRemoteWorkerAuthenticator {
    fn authenticate(
        &self,
        credential: &RemoteWorkerCredential,
        now: &Instant,
    ) -> Result<RemoteWorkerPrincipal, RemoteWorkerAuthenticationError> {
        match self.fleet.authenticate(credential, now) {
            Ok(principal) => Ok(principal),
            Err(error)
                if error.kind()
                    == winwincode_control_plane::RemoteWorkerAuthenticationErrorKind::Rejected =>
            {
                self.sessions.authenticate(credential, now)
            }
            Err(error) => Err(error),
        }
    }

    fn ensure_active(
        &self,
        principal: &RemoteWorkerPrincipal,
        now: &Instant,
    ) -> Result<(), RemoteWorkerAuthenticationError> {
        if principal == &self.fleet.principal {
            self.fleet.ensure_active(principal, now)
        } else {
            self.sessions.ensure_active(principal, now)
        }
    }
}

fn session_authentication_error(
    error: &crate::WorkerSessionCredentialError,
) -> RemoteWorkerAuthenticationError {
    match error.kind() {
        WorkerSessionCredentialErrorKind::AuthenticationRejected
        | WorkerSessionCredentialErrorKind::UnknownCredential => {
            RemoteWorkerAuthenticationError::rejected()
        }
        _ => RemoteWorkerAuthenticationError::unavailable(),
    }
}

#[cfg(unix)]
fn read_private_credential(path: &Path) -> Result<Vec<u8>, RemoteWorkerTransportError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path)
        .map_err(|_| RemoteWorkerTransportError::new("remote Worker credential is unavailable"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(RemoteWorkerTransportError::new(
            "remote Worker credential permissions are invalid",
        ));
    }
    let bytes = fs::read(path)
        .map_err(|_| RemoteWorkerTransportError::new("remote Worker credential is unavailable"))?;
    if bytes.is_empty() || bytes.len() > 16 * 1024 {
        return Err(RemoteWorkerTransportError::new(
            "remote Worker credential is invalid",
        ));
    }
    Ok(bytes)
}

type RemoteDelivery = (WorkerId, WorkerInstanceId, ExecutionMessageId, Vec<u8>);

#[derive(Default)]
struct RemoteDeliveryQueue {
    pending: Mutex<Vec<RemoteDelivery>>,
    authorities: Mutex<HashMap<(WorkerId, WorkerInstanceId), RemoteWorkerPrincipal>>,
}

impl RemoteDeliveryQueue {
    fn retire_inactive(
        &self,
        authenticator: &dyn RemoteWorkerAuthenticator,
        now: &Instant,
    ) -> Result<(), RemoteWorkerTransportError> {
        // Only the credential authority may retire unacknowledged controls.
        // Authority unavailability preserves them for the durable scheduler to replay.
        let mut authorities = self.authorities.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker authority cache is unavailable")
        })?;
        let mut pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        authorities.retain(|(worker, instance), principal| {
            let retired = authenticator.ensure_active(principal, now).is_err_and(|error| matches!(error.kind(), winwincode_control_plane::RemoteWorkerAuthenticationErrorKind::Revoked | winwincode_control_plane::RemoteWorkerAuthenticationErrorKind::Rejected));
            if retired { pending.retain(|(target, boot, _, _)| target != worker || boot != instance); }
            !retired && pending.iter().any(|(target, boot, _, _)| target == worker && boot == instance)
        });
        Ok(())
    }

    fn prepare_ingress(
        &self,
        request: &RemoteExchangeRequest,
        principal: &RemoteWorkerPrincipal,
    ) -> Result<bool, RemoteWorkerTransportError> {
        self.acknowledge(
            request.worker_id(),
            request.worker_instance_id(),
            request.acknowledgements(),
        )?;
        let pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        let can_accept = pending
            .iter()
            .filter(|(worker, instance, _, _)| {
                worker == request.worker_id() && instance == request.worker_instance_id()
            })
            .count()
            <= MAX_PENDING_REMOTE_DELIVERIES
                - winwincode_execution_port::transport::MAX_REMOTE_DELIVERIES;
        drop(pending);
        self.authorities
            .lock()
            .map_err(|_| {
                RemoteWorkerTransportError::new("remote Worker authority cache is unavailable")
            })?
            .insert(
                (
                    request.worker_id().clone(),
                    request.worker_instance_id().clone(),
                ),
                principal.clone(),
            );
        Ok(can_accept)
    }
    fn retire_other_instances(
        &self,
        worker: &WorkerId,
        instance: &WorkerInstanceId,
    ) -> Result<(), RemoteWorkerTransportError> {
        let mut pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        pending.retain(|(target, boot, _, _)| target != worker || boot == instance);
        Ok(())
    }
    fn acknowledge(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
        ids: &[ExecutionMessageId],
    ) -> Result<(), RemoteWorkerTransportError> {
        let mut pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        for id in ids {
            pending.retain(|(target, instance, pending_id, _)| {
                target != worker_id || instance != instance_id || pending_id != id
            });
        }
        Ok(())
    }

    fn snapshot(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
    ) -> Result<Vec<RemoteExchangeDelivery>, RemoteWorkerTransportError> {
        let pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        let mut deliveries = Vec::new();
        let mut bytes = RemoteExchangeResponse::with_acceptance(Vec::new(), false)
            .and_then(|response| response.encode())
            .map_err(RemoteWorkerTransportError::frame_error)?
            .len();
        for (_, _, delivery_id, frame) in pending
            .iter()
            .filter(|(target, instance, _, _)| target == worker_id && instance == instance_id)
        {
            let delivery = RemoteExchangeDelivery {
                delivery_id: delivery_id.clone(),
                frame: frame.clone(),
            };
            let size = serde_json::to_vec(&delivery)
                .map_err(|_| RemoteWorkerTransportError::invalid_frame())?
                .len()
                + usize::from(!deliveries.is_empty());
            if deliveries.len() == winwincode_execution_port::transport::MAX_REMOTE_DELIVERIES
                || bytes + size > winwincode_execution_port::transport::MAX_REMOTE_RESPONSE_BYTES
            {
                break;
            }
            bytes += size;
            deliveries.push(delivery);
        }
        Ok(deliveries)
    }

    fn enqueue_for(
        &self,
        worker_id: &WorkerId,
        instance_id: &WorkerInstanceId,
        mut message: ExecutionPortMessage,
    ) -> Result<(), RuntimeSupervisorError> {
        separate_heartbeat_receipt_identity(&mut message)?;
        let delivery_id = execution_message_id(&message).map_err(|_| remote_queue_failure())?;
        let frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, message)
            .and_then(|frame| RemoteTransportAdapter::<NoopCore>::encode(&frame))
            .map_err(|_| remote_queue_failure())?;
        RemoteExchangeResponse::new(vec![RemoteExchangeDelivery {
            delivery_id: delivery_id.clone(),
            frame: frame.clone(),
        }])
        .map_err(|_| remote_queue_failure())?;
        let mut pending = self.pending.lock().map_err(|_| remote_queue_failure())?;
        if pending.iter().any(|(target, instance, id, _)| {
            target == worker_id && instance == instance_id && id == &delivery_id
        }) {
            return Ok(());
        }
        if pending
            .iter()
            .filter(|(target, instance, _, _)| target == worker_id && instance == instance_id)
            .count()
            >= MAX_PENDING_REMOTE_DELIVERIES
        {
            return Err(remote_queue_failure());
        }
        pending.push((worker_id.clone(), instance_id.clone(), delivery_id, frame));
        Ok(())
    }
}

// A retryable gap and the eventual positive ACK must coexist until separately
// confirmed. Accepted/Duplicate share one identity; neither can erase a gap or
// be erased by a delayed confirmation of that gap.
fn separate_heartbeat_receipt_identity(
    message: &mut ExecutionPortMessage,
) -> Result<(), RuntimeSupervisorError> {
    use winwincode_execution_port::generated::{
        ExecutionPortErrorCode, WorkerHeartbeatAckMessageStatus,
    };
    let ExecutionPortMessage::WorkerHeartbeatAckMessage(ack) = message else {
        return Ok(());
    };
    let class = match ack.status {
        WorkerHeartbeatAckMessageStatus::Accepted | WorkerHeartbeatAckMessageStatus::Duplicate
            if ack.error.is_none() =>
        {
            "accepted"
        }
        WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance
            if ack.error.as_ref().is_some_and(|error| {
                error.code == ExecutionPortErrorCode::SequenceGap && error.retryable
            }) =>
        {
            "gap"
        }
        _ => return Ok(()),
    };
    let identity = serde_json::to_vec(&(
        "worker-heartbeat-receipt-v1",
        &ack.message_id,
        &ack.worker_id,
        &ack.worker_instance_id,
        &ack.heartbeat_sequence,
        class,
    ))
    .map_err(|_| remote_queue_failure())?;
    ack.message_id = ExecutionMessageId(format!(
        "xmsg_{}",
        crate::runtime::crockford_26(&Sha256::digest(identity))
    ));
    Ok(())
}

struct WorkerDeliveryQueue<'queue> {
    queue: &'queue RemoteDeliveryQueue,
    worker_id: WorkerId,
    instance_id: WorkerInstanceId,
}

impl RuntimeControlOutbound for WorkerDeliveryQueue<'_> {
    fn enqueue_control(&self, message: ExecutionPortMessage) -> Result<(), RuntimeSupervisorError> {
        self.queue
            .enqueue_for(&self.worker_id, &self.instance_id, message)
    }
}

fn remote_queue_failure() -> RuntimeSupervisorError {
    RuntimeSupervisorError::transport_unavailable()
}

struct NoopCore;

impl ExecutionPortCore for NoopCore {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

/// Production exchange over the Server's one application, scheduler, and
/// durable storage directory.
pub struct ProductionRemoteWorkerExchange<Core> {
    accounting_key:
        Option<winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey>,
    exchange_gate: Mutex<()>,
    data_directory: PathBuf,
    authenticator: Arc<dyn RemoteWorkerAuthenticator>,
    scheduler: Mutex<RepositoryRuntimeScheduler>,
    core: Mutex<Core>,
    queue: RemoteDeliveryQueue,
}

impl<Core> ProductionRemoteWorkerExchange<Core> {
    #[must_use]
    pub fn new(
        data_directory: impl Into<PathBuf>,
        authenticator: Arc<dyn RemoteWorkerAuthenticator>,
        scheduler: RepositoryRuntimeScheduler,
        core: Core,
    ) -> Self {
        Self {
            accounting_key: None,
            exchange_gate: Mutex::new(()),
            data_directory: data_directory.into(),
            authenticator,
            scheduler: Mutex::new(scheduler),
            core: Mutex::new(core),
            queue: RemoteDeliveryQueue::default(),
        }
    }
    /// Enables the separate financial-only Provider receipt consumer.
    #[must_use]
    pub fn with_accounting_key(
        mut self,
        key: winwincode_execution_port::action_enforcement::ActionEnforcementSigningKey,
    ) -> Self {
        self.accounting_key = Some(key);
        self
    }
}

impl<Core> RemoteWorkerExchangePort for ProductionRemoteWorkerExchange<Core>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send,
    Core::Error: Send + fmt::Display,
{
    fn accounting(
        &self,
        token: &str,
        body: Option<&[u8]>,
        offset: u64,
        now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
        let key = self
            .accounting_key
            .as_ref()
            .ok_or_else(RemoteWorkerTransportError::authentication)?;
        let expected = key.accounting_query_token();
        if token.len() != expected.0.len()
            || token
                .bytes()
                .zip(expected.0.bytes())
                .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
                != 0
        {
            return Err(RemoteWorkerTransportError::authentication());
        }
        let mut storage = SqliteStorage::open(&self.data_directory)
            .map_err(|_| RemoteWorkerTransportError::new("accounting storage unavailable"))?;
        if let Some(body) = body {
            let statement =
                winwincode_execution_port::accounting::AttemptAccountingStatement::decode(body)
                    .map_err(|_| RemoteWorkerTransportError::invalid_frame())?;
            let verified = statement
                .verified(key)
                .map_err(|_| RemoteWorkerTransportError::authentication())?;
            storage
                .reconcile_provider_attempt(&verified, &now)
                .map_err(|error| {
                    if error.kind() == winwincode_storage::StorageErrorKind::InvalidInput {
                        RemoteWorkerTransportError::invalid_frame()
                    } else {
                        RemoteWorkerTransportError::new("accounting import unavailable")
                    }
                })?;
            Ok(b"{}".to_vec())
        } else {
            let leases = storage.pending_accounting_leases(offset).map_err(|_| {
                RemoteWorkerTransportError::new("accounting lease query unavailable")
            })?;
            let next_offset = if leases.len() == 100 {
                Some(
                    offset
                        .checked_add(100)
                        .ok_or_else(RemoteWorkerTransportError::invalid_frame)?,
                )
            } else {
                None
            };
            serde_json::to_vec(
                &winwincode_execution_port::accounting::PendingAccountingPage {
                    leases,
                    next_offset,
                },
            )
            .map_err(|_| RemoteWorkerTransportError::invalid_frame())
        }
    }

    fn exchange(
        &self,
        credential: Vec<u8>,
        request_body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
        let _gate = self.exchange_gate.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker exchange is unavailable")
        })?;
        let request = RemoteExchangeRequest::decode(request_body)
            .map_err(RemoteWorkerTransportError::frame_error)?;
        let credential = RemoteWorkerCredential::new(credential)
            .map_err(|_| RemoteWorkerTransportError::authentication())?;
        let frame = RemoteTransportAdapter::<NoopCore>::decode(request.frame())
            .map_err(RemoteWorkerTransportError::frame_error)?;
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker Registry is unavailable")
        })?;
        let (responses, principal, frame_accepted) =
            self.accept_ingress(&mut storage, &credential, &request, &frame, &now)?;
        Box::new(storage)
            .close()
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker Registry close failed"))?;

        if responses.iter().any(|message|matches!(message,ExecutionPortMessage::WorkerRegistrationResultMessage(result)
            if result.error.is_none() && matches!(result.status,winwincode_execution_port::generated::WorkerRegistrationResultMessageStatus::Accepted|winwincode_execution_port::generated::WorkerRegistrationResultMessageStatus::Duplicate))) {
            self.queue.retire_other_instances(request.worker_id(),request.worker_instance_id())?;
        }

        self.queue.acknowledge(
            request.worker_id(),
            request.worker_instance_id(),
            request.acknowledgements(),
        )?;
        for response in responses {
            self.queue
                .enqueue_for(request.worker_id(), request.worker_instance_id(), response)
                .map_err(|_| RemoteWorkerTransportError::new("remote Worker queue is full"))?;
        }
        let mut scheduler = self.scheduler.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker scheduler is unavailable")
        })?;
        scheduler
            .acknowledge_remote(
                &now,
                request.worker_id(),
                request.worker_instance_id(),
                request.acknowledgements(),
            )
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker scheduler failed"))?;
        // Registration leaves Registry health at `registered` by default.
        // Device WorkerSessions are live Client-spawned processes that already
        // hold a launch grant; mark them healthy on the accepting registration
        // so the same exchange can drive the identity-bound queued Job claim
        // (StrongFlow WorkRun executor jobs are never locally claimable).
        // A later scheduler failure must not erase an accepted registration:
        // the Worker already joined the Registry and will retry drive on the
        // next heartbeat.
        let drive_result = if frame_accepted {
            let worker_queue = WorkerDeliveryQueue {
                queue: &self.queue,
                worker_id: request.worker_id().clone(),
                instance_id: request.worker_instance_id().clone(),
            };
            scheduler.drive_remote_for(
                &now,
                &worker_queue,
                request.worker_id().clone(),
                request.worker_instance_id().clone(),
                principal.worker_pool_id().clone(),
            )
        } else {
            Ok(())
        };
        if let Err(error) = drive_result
            && std::env::var_os("WWC_DEBUG_RUNTIME").is_some()
        {
            eprintln!("remote Worker scheduler drive after exchange failed: {error}");
        }
        let deliveries = self
            .queue
            .snapshot(request.worker_id(), request.worker_instance_id())?;
        let response = if request.supports_acceptance_receipt() {
            RemoteExchangeResponse::with_acceptance(deliveries, frame_accepted)
        } else {
            RemoteExchangeResponse::new(deliveries)
        }
        .map_err(RemoteWorkerTransportError::frame_error)?;
        response
            .encode()
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker response is invalid"))
    }
}

impl<Core> ProductionRemoteWorkerExchange<Core>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send,
    Core::Error: Send + fmt::Display,
{
    #[allow(clippy::too_many_lines)]
    fn accept_ingress(
        &self,
        storage: &mut SqliteStorage,
        credential: &RemoteWorkerCredential,
        request: &RemoteExchangeRequest,
        frame: &TypedFrame,
        now: &Instant,
    ) -> Result<(Vec<ExecutionPortMessage>, RemoteWorkerPrincipal, bool), RemoteWorkerTransportError>
    {
        let frame_identity = match frame.message() {
            ExecutionPortMessage::WorkerRegisterMessage(message) => {
                Some((&message.worker_id, &message.worker_instance_id))
            }
            ExecutionPortMessage::WorkerHeartbeatMessage(message) => {
                Some((&message.worker_id, &message.worker_instance_id))
            }
            ExecutionPortMessage::JobDispatchResultMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::SessionBindingMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::RuntimeEventMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ArtifactOpenMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ArtifactChunkMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ModelOpenMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ModelChunkMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ModelAckMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::InputRequestMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ApprovalRequestMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::JobCancelAckMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::JobOutcomeMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::ActionEnforcementRequestMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::SnapshotFreezeRequestMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            ExecutionPortMessage::SnapshotFreezeReceiptMessage(m) => {
                Some((&m.lease.worker_id, &m.lease.worker_instance_id))
            }
            _ => None,
        };
        if frame_identity.is_some_and(|(worker, instance)| {
            worker != request.worker_id() || instance != request.worker_instance_id()
        }) {
            return Err(RemoteWorkerTransportError::invalid_frame());
        }
        let mut pool = RemoteWorkerPoolAdapter::new(storage, self.authenticator.as_ref());
        let mut connection = if matches!(
            frame.message(),
            ExecutionPortMessage::WorkerRegisterMessage(_)
        ) {
            pool.connect(credential, now)
        } else {
            pool.resume(
                credential,
                request.worker_id(),
                request.worker_instance_id(),
                now,
            )
        }
        .map_err(RemoteWorkerTransportError::from_pool)?;
        let principal = connection.principal().clone();
        if principal.worker_id() != request.worker_id()
            || principal
                .worker_instance_id()
                .is_some_and(|instance| instance != request.worker_instance_id())
        {
            return Err(RemoteWorkerTransportError::authentication());
        }
        self.queue
            .retire_inactive(self.authenticator.as_ref(), now)?;

        // The exchange gate keeps this response reservation through business commit.
        // ACKs are accepted only after authenticating the exact Worker instance.
        let can_accept = self.queue.prepare_ingress(request, &principal)?;
        if !can_accept {
            if request.supports_acceptance_receipt() {
                return Ok((Vec::new(), principal, false));
            }
            return Err(RemoteWorkerTransportError::new(
                "remote Worker response backlog must be acknowledged",
            ));
        }

        let responses = if matches!(
            frame.message(),
            ExecutionPortMessage::WorkerRegisterMessage(_)
        ) {
            Ok(vec![
                pool.accept(&mut connection, frame.message(), now)
                    .map_err(RemoteWorkerTransportError::from_pool)?,
            ])
        } else {
            // Heartbeats must reach the shared Core's durable lease renewal.
            // Authenticate their frame identity as well as the envelope.
            let (worker_id, instance_id) = match frame.message() {
                ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat) => {
                    (&heartbeat.worker_id, &heartbeat.worker_instance_id)
                }
                _ => (request.worker_id(), request.worker_instance_id()),
            };
            pool.authorize_registered_message(&mut connection, worker_id, instance_id, now)
                .map_err(RemoteWorkerTransportError::from_pool)?;
            let mut core = self.core.lock().map_err(|_| {
                RemoteWorkerTransportError::new("remote Worker ingress is unavailable")
            })?;
            log_dispatch_result(frame.message());
            let encoded = RemoteTransportAdapter::<NoopCore>::encode(frame)
                .map_err(|_| RemoteWorkerTransportError::invalid_frame())?;
            RemoteTransportAdapter::new(&mut *core, EndpointSide::ControlPlane)
                .accept(&encoded)
                .map_err(|error| {
                    log_ingress_rejection(frame, &error);
                    RemoteWorkerTransportError::new("remote Worker ingress rejected a frame")
                })
        }?;
        if let ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat) = frame.message()
            && heartbeat_was_accepted(heartbeat, &responses)
        {
            WorkerSessionCredentialService::new(storage)
                .renew_after_accepted_heartbeat(
                    &principal.credential_fingerprint().0,
                    heartbeat,
                    now,
                )
                .map_err(|_| {
                    RemoteWorkerTransportError::new(
                        "remote Worker credential renewal is unavailable",
                    )
                })?;
        }
        Ok((responses, principal, true))
    }
}

fn heartbeat_was_accepted(
    heartbeat: &winwincode_execution_port::generated::WorkerHeartbeatMessage,
    responses: &[ExecutionPortMessage],
) -> bool {
    use winwincode_execution_port::generated::WorkerHeartbeatAckMessageStatus;
    if heartbeat.active_leases.is_empty() {
        return false;
    }
    let mut matching = responses.iter().filter_map(|message| {
        let ExecutionPortMessage::WorkerHeartbeatAckMessage(ack) = message else {
            return None;
        };
        (ack.message_id == heartbeat.message_id
            && ack.worker_id == heartbeat.worker_id
            && ack.worker_instance_id == heartbeat.worker_instance_id
            && ack.heartbeat_sequence == heartbeat.heartbeat_sequence)
            .then_some(ack)
    });
    let accepted = matching.next().is_some_and(|ack| {
        ack.status == WorkerHeartbeatAckMessageStatus::Accepted && ack.error.is_none()
    });
    accepted && matching.next().is_none()
}

fn log_dispatch_result(message: &ExecutionPortMessage) {
    if std::env::var_os("WWC_DEBUG_RUNTIME").is_some()
        && let ExecutionPortMessage::JobDispatchResultMessage(result) = message
    {
        eprintln!(
            "remote Worker dispatch result status: {:?}; error: {:?}",
            result.status, result.error
        );
    }
}

fn log_ingress_rejection<Error>(frame: &TypedFrame, error: &Error)
where
    Error: fmt::Display,
{
    if std::env::var_os("WWC_DEBUG_RUNTIME").is_none() {
        return;
    }
    let kind = serde_json::to_value(frame.message())
        .ok()
        .and_then(|value| {
            value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    eprintln!("remote Worker ingress rejected kind: {kind}; category: {error}");
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use winwincode_domain::{ExecutionSequence, SchemaVersion};
    use winwincode_execution_port::generated::{
        WorkerHeartbeatAckMessage, WorkerHeartbeatAckMessageKind, WorkerHeartbeatAckMessageStatus,
    };

    use super::*;

    fn queue_message(
        seed: usize,
        worker: &WorkerId,
        instance: &WorkerInstanceId,
        padding: usize,
    ) -> ExecutionPortMessage {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut value = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["kind"] == "worker.heartbeat_ack")
            .unwrap()
            .clone();
        value["messageId"] = serde_json::json!(format!("xmsg_{seed:026}"));
        value["workerId"] = serde_json::json!(worker.0);
        value["workerInstanceId"] = serde_json::json!(instance.0);
        if padding > 0 {
            value["error"] = serde_json::json!({"code":"INFRASTRUCTURE_ERROR","message":"z".repeat(padding),"retryable":true});
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn heartbeat_negative_and_positive_receipts_have_separate_delivery_lifetimes() {
        use winwincode_execution_port::generated::{ExecutionPortError, ExecutionPortErrorCode};
        let queue = RemoteDeliveryQueue::default();
        let worker = WorkerId("wrk_00000000000000000000000001".into());
        let instance = WorkerInstanceId("wki_00000000000000000000000001".into());
        let ExecutionPortMessage::WorkerHeartbeatAckMessage(mut gap) =
            queue_message(1, &worker, &instance, 0)
        else {
            unreachable!()
        };
        gap.status = WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance;
        gap.error = Some(ExecutionPortError {
            code: ExecutionPortErrorCode::SequenceGap,
            message: "retry original sequence".into(),
            retryable: true,
        });
        let mut accepted = gap.clone();
        accepted.error = None;
        accepted.status = WorkerHeartbeatAckMessageStatus::Accepted;
        queue
            .enqueue_for(
                &worker,
                &instance,
                ExecutionPortMessage::WorkerHeartbeatAckMessage(gap.clone()),
            )
            .unwrap();
        queue
            .enqueue_for(
                &worker,
                &instance,
                ExecutionPortMessage::WorkerHeartbeatAckMessage(accepted.clone()),
            )
            .unwrap();
        let page = queue.snapshot(&worker, &instance).unwrap();
        assert_eq!(
            page.len(),
            2,
            "a retryable negative reply cannot hide later acceptance"
        );
        assert_ne!(page[0].delivery_id, page[1].delivery_id);
        queue
            .enqueue_for(
                &worker,
                &instance,
                ExecutionPortMessage::WorkerHeartbeatAckMessage(gap),
            )
            .unwrap();
        accepted.status = WorkerHeartbeatAckMessageStatus::Duplicate;
        queue
            .enqueue_for(
                &worker,
                &instance,
                ExecutionPortMessage::WorkerHeartbeatAckMessage(accepted),
            )
            .unwrap();
        assert_eq!(queue.snapshot(&worker, &instance).unwrap().len(), 2);
        // A confirmation of the old negative receipt must leave acceptance queued.
        queue
            .pending
            .lock()
            .unwrap()
            .retain(|(_, _, id, _)| id != &page[0].delivery_id);
        let remaining = queue.snapshot(&worker, &instance).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].delivery_id, page[1].delivery_id);
        for delivery in remaining {
            let frame = RemoteTransportAdapter::<NoopCore>::decode(&delivery.frame).unwrap();
            assert_eq!(
                execution_message_id(frame.message()).unwrap(),
                delivery.delivery_id
            );
        }
    }

    #[test]
    fn byte_limited_pages_can_drain_a_backpressured_worker() {
        let queue = RemoteDeliveryQueue::default();
        let worker = WorkerId("wrk_00000000000000000000000001".into());
        let instance = WorkerInstanceId("wki_00000000000000000000000001".into());
        let principal = RemoteWorkerPrincipal::new_bound(
            worker.clone(),
            instance.clone(),
            WorkerPoolId("wpl_00000000000000000000000001".into()),
            WorkerRegistryScope::local_default(),
            "issuer".into(),
            "subject".into(),
            winwincode_domain::Sha256Digest(format!("sha256:{}", "e".repeat(64))),
            "zone".into(),
        )
        .unwrap();
        for seed in 0..MAX_PENDING_REMOTE_DELIVERIES {
            queue
                .enqueue_for(
                    &worker,
                    &instance,
                    queue_message(seed, &worker, &instance, 32 * 1024),
                )
                .unwrap();
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut heartbeat = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == "worker.heartbeat")
            .unwrap()
            .clone();
        heartbeat["workerId"] = serde_json::json!(worker.0);
        heartbeat["workerInstanceId"] = serde_json::json!(instance.0);
        let frame = RemoteTransportAdapter::<NoopCore>::encode(
            &TypedFrame::new(
                FrameDirection::WorkerToControlPlane,
                serde_json::from_value(heartbeat).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let mut delivered = 0;
        loop {
            let page = queue.snapshot(&worker, &instance).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() < 128);
            let ids = page.into_iter().map(|d| d.delivery_id).collect::<Vec<_>>();
            delivered += ids.len();
            let request =
                RemoteExchangeRequest::new(worker.clone(), instance.clone(), ids, frame.clone())
                    .unwrap();
            assert!(
                queue.prepare_ingress(&request, &principal).is_ok(),
                "ACK and pull must remain available under backpressure"
            );
            assert!(delivered <= 256);
        }
        assert_eq!(delivered, 256);
    }

    #[test]
    fn dead_worker_backlog_cannot_block_another_worker_and_pages_keep_the_suffix() {
        let queue = RemoteDeliveryQueue::default();
        let dead = WorkerId("wrk_00000000000000000000000001".into());
        let live = WorkerId("wrk_00000000000000000000000002".into());
        let instance = WorkerInstanceId("wki_00000000000000000000000001".into());
        for seed in 0..MAX_PENDING_REMOTE_DELIVERIES {
            queue
                .enqueue_for(&dead, &instance, queue_message(seed, &dead, &instance, 0))
                .unwrap();
        }
        assert!(
            queue
                .enqueue_for(&dead, &instance, queue_message(300, &dead, &instance, 0))
                .is_err()
        );
        for seed in 400..408 {
            queue
                .enqueue_for(
                    &live,
                    &instance,
                    queue_message(seed, &live, &instance, 200 * 1024),
                )
                .unwrap();
        }
        let page = queue.snapshot(&live, &instance).unwrap();
        assert!(!page.is_empty() && page.len() < 8);
        let encoded = RemoteExchangeResponse::new(page.clone())
            .unwrap()
            .encode()
            .unwrap();
        assert!(encoded.len() <= winwincode_execution_port::transport::MAX_REMOTE_RESPONSE_BYTES);
        assert_eq!(
            RemoteExchangeResponse::decode(&encoded)
                .unwrap()
                .deliveries(),
            page
        );
        assert_eq!(
            queue.snapshot(&live, &instance).unwrap(),
            page,
            "lost response retains the same page"
        );
        queue
            .acknowledge(
                &live,
                &instance,
                &page
                    .iter()
                    .map(|delivery| delivery.delivery_id.clone())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert!(
            !queue.snapshot(&live, &instance).unwrap().is_empty(),
            "undelivered suffix stays queued"
        );
        assert_eq!(
            queue
                .pending
                .lock()
                .unwrap()
                .iter()
                .filter(|(worker, _, _, _)| worker == &dead)
                .count(),
            256
        );
    }

    #[test]
    fn retirement_requires_authoritative_revocation_and_backpressure_precedes_ingress() {
        struct Authority {
            retired: WorkerId,
        }
        impl RemoteWorkerAuthenticator for Authority {
            fn authenticate(
                &self,
                _: &RemoteWorkerCredential,
                _: &Instant,
            ) -> Result<RemoteWorkerPrincipal, RemoteWorkerAuthenticationError> {
                Err(RemoteWorkerAuthenticationError::rejected())
            }
            fn ensure_active(
                &self,
                principal: &RemoteWorkerPrincipal,
                _: &Instant,
            ) -> Result<(), RemoteWorkerAuthenticationError> {
                if principal.worker_id() == &self.retired {
                    Err(RemoteWorkerAuthenticationError::revoked())
                } else {
                    Ok(())
                }
            }
        }
        let queue = RemoteDeliveryQueue::default();
        let dead = WorkerId("wrk_00000000000000000000000001".into());
        let live = WorkerId("wrk_00000000000000000000000002".into());
        let instance = WorkerInstanceId("wki_00000000000000000000000001".into());
        for worker in [&dead, &live] {
            let principal = RemoteWorkerPrincipal::new_bound(
                worker.clone(),
                instance.clone(),
                WorkerPoolId("wpl_00000000000000000000000001".into()),
                WorkerRegistryScope::local_default(),
                "issuer".into(),
                "subject".into(),
                winwincode_domain::Sha256Digest(format!("sha256:{}", "e".repeat(64))),
                "zone".into(),
            )
            .unwrap();
            queue
                .authorities
                .lock()
                .unwrap()
                .insert((worker.clone(), instance.clone()), principal);
            queue
                .enqueue_for(worker, &instance, queue_message(1, worker, &instance, 0))
                .unwrap();
        }
        queue
            .retire_inactive(
                &Authority {
                    retired: dead.clone(),
                },
                &Instant("2026-10-02T00:00:00.000Z".into()),
            )
            .unwrap();
        assert!(queue.snapshot(&dead, &instance).unwrap().is_empty());
        assert_eq!(queue.snapshot(&live, &instance).unwrap().len(), 1);
        for seed in 2..=130 {
            queue
                .enqueue_for(&live, &instance, queue_message(seed, &live, &instance, 0))
                .unwrap();
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut heartbeat = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["kind"] == "worker.heartbeat")
            .unwrap()
            .clone();
        heartbeat["workerId"] = serde_json::json!(live.0);
        heartbeat["workerInstanceId"] = serde_json::json!(instance.0);
        let frame = TypedFrame::new(
            FrameDirection::WorkerToControlPlane,
            serde_json::from_value(heartbeat).unwrap(),
        )
        .unwrap();
        let request = RemoteExchangeRequest::new(
            live.clone(),
            instance.clone(),
            Vec::new(),
            RemoteTransportAdapter::<NoopCore>::encode(&frame).unwrap(),
        )
        .unwrap();
        let principal = queue
            .authorities
            .lock()
            .unwrap()
            .get(&(live, instance))
            .unwrap()
            .clone();
        assert!(!queue.prepare_ingress(&request, &principal).unwrap());
    }

    #[test]
    fn renewal_requires_one_matching_fresh_accepted_lease_heartbeat() {
        use winwincode_domain::{ExecutionAckSequence, ExecutionJobId, FencingToken, LeaseId};
        use winwincode_execution_port::generated::{
            ActiveLeaseSummary, WorkerCapacity, WorkerHeartbeatMessage, WorkerHeartbeatMessageKind,
        };
        let mut heartbeat = WorkerHeartbeatMessage {
            active_leases: vec![ActiveLeaseSummary {
                attempt: 1,
                expires_at: Instant("2030-01-01T01:00:00.000Z".into()),
                fencing_token: FencingToken("1".into()),
                job_id: ExecutionJobId("job_test".into()),
                last_event_sequence: ExecutionAckSequence(0),
                lease_id: LeaseId("lease_test".into()),
            }],
            capacity: WorkerCapacity {
                available_slots: 0,
                running_jobs: 1,
            },
            heartbeat_sequence: ExecutionSequence(7),
            kind: WorkerHeartbeatMessageKind::WorkerHeartbeat,
            message_id: ExecutionMessageId("xmsg_00000000000000000000000007".into()),
            observed_at: Instant("2030-01-01T00:00:00.000Z".into()),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2030-01-01T00:00:00.000Z".into()),
            worker_id: WorkerId("wrk_test".into()),
            worker_instance_id: WorkerInstanceId("wki_test".into()),
        };
        let ack = WorkerHeartbeatAckMessage {
            error: None,
            heartbeat_sequence: heartbeat.heartbeat_sequence.clone(),
            kind: WorkerHeartbeatAckMessageKind::WorkerHeartbeatAck,
            message_id: heartbeat.message_id.clone(),
            next_heartbeat_within_ms: 1000,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: heartbeat.sent_at.clone(),
            server_time: heartbeat.sent_at.clone(),
            status: WorkerHeartbeatAckMessageStatus::Accepted,
            worker_id: heartbeat.worker_id.clone(),
            worker_instance_id: heartbeat.worker_instance_id.clone(),
        };
        let responses = vec![ExecutionPortMessage::WorkerHeartbeatAckMessage(ack.clone())];
        assert!(heartbeat_was_accepted(&heartbeat, &responses));
        for status in [
            WorkerHeartbeatAckMessageStatus::Duplicate,
            WorkerHeartbeatAckMessageStatus::RejectedWorkerInstance,
        ] {
            let mut rejected = ack.clone();
            rejected.status = status;
            assert!(!heartbeat_was_accepted(
                &heartbeat,
                &[ExecutionPortMessage::WorkerHeartbeatAckMessage(rejected)]
            ));
        }
        assert!(!heartbeat_was_accepted(
            &heartbeat,
            &[responses[0].clone(), responses[0].clone()]
        ));
        heartbeat.heartbeat_sequence = ExecutionSequence(8);
        assert!(!heartbeat_was_accepted(&heartbeat, &responses));
        heartbeat.heartbeat_sequence = ExecutionSequence(7);
        heartbeat.active_leases.clear();
        assert!(!heartbeat_was_accepted(&heartbeat, &responses));
    }

    #[test]
    fn duplicate_delivery_id_replays_the_first_response() {
        let queue = RemoteDeliveryQueue::default();
        let worker_id = WorkerId("wrk_00000000000000000000000001".to_owned());
        let response = |sent_at: &str, status| {
            ExecutionPortMessage::WorkerHeartbeatAckMessage(WorkerHeartbeatAckMessage {
                error: None,
                heartbeat_sequence: ExecutionSequence(1),
                kind: WorkerHeartbeatAckMessageKind::WorkerHeartbeatAck,
                message_id: ExecutionMessageId("msg_00000000000000000000000001".to_owned()),
                next_heartbeat_within_ms: 1_000,
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: Instant(sent_at.to_owned()),
                server_time: Instant(sent_at.to_owned()),
                status,
                worker_id: worker_id.clone(),
                worker_instance_id: WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
            })
        };
        queue
            .enqueue_for(
                &worker_id,
                &WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
                response(
                    "2026-09-12T00:00:00.000Z",
                    WorkerHeartbeatAckMessageStatus::Accepted,
                ),
            )
            .expect("first response queues");
        queue
            .enqueue_for(
                &worker_id,
                &WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
                response(
                    "2026-09-12T00:00:01.000Z",
                    WorkerHeartbeatAckMessageStatus::Duplicate,
                ),
            )
            .expect("duplicate response reuses the pending delivery");

        assert_eq!(
            queue
                .snapshot(
                    &worker_id,
                    &WorkerInstanceId("wki_00000000000000000000000001".to_owned())
                )
                .expect("queue snapshot")
                .len(),
            1
        );
        let old = WorkerInstanceId("wki_00000000000000000000000001".into());
        let next = WorkerInstanceId("wki_00000000000000000000000002".into());
        assert!(queue.snapshot(&worker_id, &next).unwrap().is_empty());
        queue
            .acknowledge(
                &worker_id,
                &next,
                &[ExecutionMessageId("msg_00000000000000000000000001".into())],
            )
            .unwrap();
        assert_eq!(queue.snapshot(&worker_id, &old).unwrap().len(), 1);
        queue.retire_other_instances(&worker_id, &next).unwrap();
        assert!(queue.snapshot(&worker_id, &old).unwrap().is_empty());
    }

    #[test]
    fn credential_file_is_private_short_lived_and_revalidated_after_rotation() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-remote-credential-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("credential test directory");
        let path = root.join("worker.token");
        fs::write(&path, b"fixture-remote-token").expect("credential write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("private credential mode");
        let now = Instant("2026-08-30T00:00:00.000Z".to_owned());
        let authenticator = FileRemoteWorkerAuthenticator::open(
            &path,
            WorkerId("wrk_00000000000000000000000001".to_owned()),
            WorkerPoolId("wpl_00000000000000000000000001".to_owned()),
            WorkerRegistryScope::local_default(),
            "fixture-issuer".to_owned(),
            "fixture-subject".to_owned(),
            "fixture-zone".to_owned(),
            Instant("2026-08-30T01:00:00.000Z".to_owned()),
            &now,
        )
        .expect("private active credential");
        let credential =
            RemoteWorkerCredential::new(b"fixture-remote-token".to_vec()).expect("bounded proof");
        let principal = authenticator
            .authenticate(&credential, &now)
            .expect("credential authentication");
        fs::write(&path, b"rotated-remote-token").expect("credential rotation");
        assert_eq!(
            authenticator
                .ensure_active(&principal, &now)
                .expect_err("rotation revokes established request")
                .kind(),
            winwincode_control_plane::RemoteWorkerAuthenticationErrorKind::Revoked
        );
        fs::remove_dir_all(root).expect("credential test release");
    }

    #[test]
    fn device_worker_credential_requires_its_durable_launch_grant() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-device-worker-credential-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let now = Instant("2026-09-12T00:00:00.000Z".to_owned());
        let material = crate::issue_credential_material().expect("credential material");
        let mut storage = SqliteStorage::open(&root).expect("storage");
        WorkerSessionCredentialService::new(&mut storage)
            .issue_for_launch(
                "wsn_00000000000000000000000001",
                "wrk_00000000000000000000000001",
                "wki_00000000000000000000000001",
                "wlg_00000000000000000000000001",
                material.credential_digest(),
                &now,
            )
            .expect("issue session credential");
        drop(storage);
        let authenticator =
            WorkerSessionRemoteAuthenticator::new(&root, WorkerRegistryScope::local_default());
        let proof = RemoteWorkerCredential::new(material.material().as_bytes().to_vec())
            .expect("credential proof");
        assert_eq!(
            authenticator
                .authenticate(&proof, &now)
                .expect_err("orphaned credential must not register a Worker")
                .kind(),
            winwincode_control_plane::RemoteWorkerAuthenticationErrorKind::Revoked
        );
        fs::remove_dir_all(root).expect("credential test release");
    }
}
