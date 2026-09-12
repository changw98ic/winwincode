// SPDX-License-Identifier: Apache-2.0

//! Authenticated HTTPS exchange for a separated Execution Worker.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use winwincode_control_plane::{
    ProductStateStorage, RemoteWorkerAuthenticationError, RemoteWorkerAuthenticator,
    RemoteWorkerCredential, RemoteWorkerPoolAdapter, RemoteWorkerPrincipal,
};
use winwincode_domain::{ExecutionMessageId, Instant, WorkerId, WorkerInstanceId};
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_execution_port::transport::{
    EndpointSide, ExecutionPortCore, FrameDirection, RemoteExchangeDelivery, RemoteExchangeRequest,
    RemoteExchangeResponse, RemoteTransportAdapter, TypedFrame, execution_message_id,
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
}

impl RemoteWorkerTransportError {
    const fn new(message: &'static str) -> Self {
        Self { message }
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
    worker_pool_id: WorkerPoolId,
    scope: WorkerRegistryScope,
}

impl WorkerSessionRemoteAuthenticator {
    #[must_use]
    pub fn new(
        data_directory: impl Into<PathBuf>,
        worker_pool_id: WorkerPoolId,
        scope: WorkerRegistryScope,
    ) -> Self {
        Self {
            data_directory: data_directory.into(),
            worker_pool_id,
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
        RemoteWorkerPrincipal::new_bound(
            WorkerId(record.worker_id),
            WorkerInstanceId(record.worker_instance_id),
            self.worker_pool_id.clone(),
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
        if principal.worker_pool_id() != &self.worker_pool_id || principal.scope() != &self.scope {
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
        if record.state != WorkerSessionCredentialState::Active
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

#[derive(Default)]
struct RemoteDeliveryQueue {
    pending: Mutex<Vec<(WorkerId, ExecutionMessageId, Vec<u8>)>>,
}

impl RemoteDeliveryQueue {
    fn acknowledge(
        &self,
        worker_id: &WorkerId,
        ids: &[ExecutionMessageId],
    ) -> Result<(), RemoteWorkerTransportError> {
        let mut pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        for id in ids {
            pending.retain(|(target, pending_id, _)| target != worker_id || pending_id != id);
        }
        Ok(())
    }

    fn snapshot(
        &self,
        worker_id: &WorkerId,
    ) -> Result<Vec<RemoteExchangeDelivery>, RemoteWorkerTransportError> {
        let pending = self.pending.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker delivery queue is unavailable")
        })?;
        Ok(pending
            .iter()
            .filter(|(target, _, _)| target == worker_id)
            .take(winwincode_execution_port::transport::MAX_REMOTE_DELIVERIES)
            .map(|(_, delivery_id, frame)| RemoteExchangeDelivery {
                delivery_id: delivery_id.clone(),
                frame: frame.clone(),
            })
            .collect())
    }

    fn enqueue_for(
        &self,
        worker_id: &WorkerId,
        message: ExecutionPortMessage,
    ) -> Result<(), RuntimeSupervisorError> {
        let delivery_id = execution_message_id(&message).map_err(|_| remote_queue_failure())?;
        let frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, message)
            .and_then(|frame| RemoteTransportAdapter::<NoopCore>::encode(&frame))
            .map_err(|_| remote_queue_failure())?;
        let mut pending = self.pending.lock().map_err(|_| remote_queue_failure())?;
        if pending
            .iter()
            .any(|(target, id, _)| target == worker_id && id == &delivery_id)
        {
            return Ok(());
        }
        if pending.len() >= MAX_PENDING_REMOTE_DELIVERIES {
            return Err(remote_queue_failure());
        }
        pending.push((worker_id.clone(), delivery_id, frame));
        Ok(())
    }
}

struct WorkerDeliveryQueue<'queue> {
    queue: &'queue RemoteDeliveryQueue,
    worker_id: WorkerId,
}

impl RuntimeControlOutbound for WorkerDeliveryQueue<'_> {
    fn enqueue_control(&self, message: ExecutionPortMessage) -> Result<(), RuntimeSupervisorError> {
        self.queue.enqueue_for(&self.worker_id, message)
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
            data_directory: data_directory.into(),
            authenticator,
            scheduler: Mutex::new(scheduler),
            core: Mutex::new(core),
            queue: RemoteDeliveryQueue::default(),
        }
    }
}

impl<Core> RemoteWorkerExchangePort for ProductionRemoteWorkerExchange<Core>
where
    Core: ExecutionPortCore<Output = Vec<ExecutionPortMessage>> + Send,
    Core::Error: Send + fmt::Display,
{
    fn exchange(
        &self,
        credential: Vec<u8>,
        request_body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, RemoteWorkerTransportError> {
        let request = RemoteExchangeRequest::decode(request_body)
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker frame is invalid"))?;
        let credential = RemoteWorkerCredential::new(credential)
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker credential is invalid"))?;
        let frame = RemoteTransportAdapter::<NoopCore>::decode(request.frame())
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker frame is invalid"))?;
        let is_registration = matches!(
            frame.message(),
            ExecutionPortMessage::WorkerRegisterMessage(_)
        );
        let mut storage = SqliteStorage::open(&self.data_directory).map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker Registry is unavailable")
        })?;
        let (responses, principal) =
            self.accept_ingress(&mut storage, &credential, &request, &frame, &now)?;
        Box::new(storage)
            .close()
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker Registry close failed"))?;

        self.queue
            .acknowledge(request.worker_id(), request.acknowledgements())?;
        for response in responses {
            self.queue
                .enqueue_for(request.worker_id(), response)
                .map_err(|_| RemoteWorkerTransportError::new("remote Worker queue is full"))?;
        }
        let mut scheduler = self.scheduler.lock().map_err(|_| {
            RemoteWorkerTransportError::new("remote Worker scheduler is unavailable")
        })?;
        scheduler
            .acknowledge_remote(&now, request.acknowledgements())
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker scheduler failed"))?;
        // Registration leaves Registry health at `registered`. Returning its
        // acceptance must not be coupled to a scheduler tick that requires a
        // healthy slot; the first authenticated heartbeat makes the Worker
        // healthy and drives the queued work immediately afterward.
        if !is_registration {
            let worker_queue = WorkerDeliveryQueue {
                queue: &self.queue,
                worker_id: request.worker_id().clone(),
            };
            scheduler
                .drive_remote_for(
                    &now,
                    &worker_queue,
                    request.worker_id().clone(),
                    request.worker_instance_id().clone(),
                    principal.worker_pool_id().clone(),
                )
                .map_err(|_| RemoteWorkerTransportError::new("remote Worker scheduler failed"))?;
        }
        let response = RemoteExchangeResponse::new(self.queue.snapshot(request.worker_id())?)
            .map_err(|_| RemoteWorkerTransportError::new("remote Worker response is invalid"))?;
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
    fn accept_ingress(
        &self,
        storage: &mut SqliteStorage,
        credential: &RemoteWorkerCredential,
        request: &RemoteExchangeRequest,
        frame: &TypedFrame,
        now: &Instant,
    ) -> Result<(Vec<ExecutionPortMessage>, RemoteWorkerPrincipal), RemoteWorkerTransportError>
    {
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
        .map_err(|_| RemoteWorkerTransportError::new("remote Worker authentication failed"))?;
        let principal = connection.principal().clone();

        let responses = match frame.message() {
            ExecutionPortMessage::WorkerRegisterMessage(_)
            | ExecutionPortMessage::WorkerHeartbeatMessage(_) => Ok(vec![
                pool.accept(&mut connection, frame.message(), now)
                    .map_err(|_| {
                        RemoteWorkerTransportError::new("remote Worker Registry rejected a frame")
                    })?,
            ]),
            _ => {
                pool.authorize_registered_message(
                    &mut connection,
                    request.worker_id(),
                    request.worker_instance_id(),
                    now,
                )
                .map_err(|_| {
                    RemoteWorkerTransportError::new("remote Worker authentication failed")
                })?;
                let mut core = self.core.lock().map_err(|_| {
                    RemoteWorkerTransportError::new("remote Worker ingress is unavailable")
                })?;
                log_dispatch_result(frame.message());
                let encoded = RemoteTransportAdapter::<NoopCore>::encode(frame).map_err(|_| {
                    RemoteWorkerTransportError::new("remote Worker frame is invalid")
                })?;
                RemoteTransportAdapter::new(&mut *core, EndpointSide::ControlPlane)
                    .accept(&encoded)
                    .map_err(|error| {
                        log_ingress_rejection(frame, &error);
                        RemoteWorkerTransportError::new("remote Worker ingress rejected a frame")
                    })
            }
        }?;
        Ok((responses, principal))
    }
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

    use winwincode_control_plane::STRONGFLOW_DEVICE_WORKER_POOL_ID;
    use winwincode_domain::{ExecutionSequence, SchemaVersion};
    use winwincode_execution_port::generated::{
        WorkerHeartbeatAckMessage, WorkerHeartbeatAckMessageKind, WorkerHeartbeatAckMessageStatus,
    };

    use super::*;

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
                response(
                    "2026-09-12T00:00:00.000Z",
                    WorkerHeartbeatAckMessageStatus::Accepted,
                ),
            )
            .expect("first response queues");
        queue
            .enqueue_for(
                &worker_id,
                response(
                    "2026-09-12T00:00:01.000Z",
                    WorkerHeartbeatAckMessageStatus::Duplicate,
                ),
            )
            .expect("duplicate response reuses the pending delivery");

        assert_eq!(queue.snapshot(&worker_id).expect("queue snapshot").len(), 1);
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
    fn device_worker_credential_authenticates_its_exact_process_and_revokes() {
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
                "ws_00000000000000000000000001",
                "wrk_00000000000000000000000001",
                "wki_00000000000000000000000001",
                "wlg_00000000000000000000000001",
                material.credential_digest(),
                &now,
            )
            .expect("issue session credential");
        drop(storage);
        let authenticator = WorkerSessionRemoteAuthenticator::new(
            &root,
            WorkerPoolId(STRONGFLOW_DEVICE_WORKER_POOL_ID.to_owned()),
            WorkerRegistryScope::local_default(),
        );
        let proof = RemoteWorkerCredential::new(material.material().as_bytes().to_vec())
            .expect("credential proof");
        let principal = authenticator
            .authenticate(&proof, &now)
            .expect("authenticate device worker");
        assert_eq!(principal.worker_id().0, "wrk_00000000000000000000000001");
        assert_eq!(
            principal.worker_instance_id().map(|id| id.0.as_str()),
            Some("wki_00000000000000000000000001")
        );
        authenticator
            .ensure_active(&principal, &now)
            .expect("credential remains active");

        let mut storage = SqliteStorage::open(&root).expect("storage");
        WorkerSessionCredentialService::new(&mut storage)
            .revoke_for_session(
                "ws_00000000000000000000000001",
                "usr_00000000000000000000000001",
                Some("test"),
                &Instant("2026-09-12T00:01:00.000Z".to_owned()),
            )
            .expect("revoke credential");
        drop(storage);
        assert_eq!(
            authenticator
                .ensure_active(&principal, &Instant("2026-09-12T00:01:00.000Z".to_owned()))
                .expect_err("revoked credential must fail")
                .kind(),
            winwincode_control_plane::RemoteWorkerAuthenticationErrorKind::Revoked
        );
        fs::remove_dir_all(root).expect("credential test release");
    }
}
