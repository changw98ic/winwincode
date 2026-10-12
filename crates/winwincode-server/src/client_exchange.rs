// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use winwincode_client_port::domain::ApplyResult;
use winwincode_client_port::domain::ApplyStrategy;
use winwincode_client_port::domain::ClientArchitecture;
use winwincode_client_port::domain::ClientControlMessageKind;
use winwincode_client_port::domain::ClientPlatformTarget;
use winwincode_client_port::domain::PresenceState;
use winwincode_client_port::domain::RepositoryAvailability as WireRepositoryAvailability;
use winwincode_client_port::domain::RepositoryDirtyState as WireRepositoryDirtyState;
use winwincode_client_port::domain::WorkerLaunchAckStatus;
use winwincode_client_port::exchange::AckCursor;
use winwincode_client_port::exchange::CommandIdentity;
use winwincode_client_port::exchange::DEFAULT_MAX_FRAME_BYTES;
use winwincode_client_port::exchange::DedupRegister;
use winwincode_client_port::exchange::DedupVerdict;
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::exchange::FrameIdentity;
use winwincode_client_port::exchange::SequenceVerdict;
use winwincode_client_port::managed_app::ManagedAppStatus;
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::ClientCandidateApplyResultPayload;
use winwincode_client_port::messages::ClientCandidateRetainedPayload;
use winwincode_client_port::messages::ClientEnrollPayload;
use winwincode_client_port::messages::ClientToServerEnvelope;
use winwincode_client_port::messages::ClientToServerMessage;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_client_port::messages::ServerToClientMessage;
use winwincode_control_plane::AccessGrantService;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ClientRegistryServiceErrorKind;
use winwincode_control_plane::ConnectCodeService;
use winwincode_control_plane::LocalCandidateService;
use winwincode_control_plane::OccupancyLeaseState;
use winwincode_control_plane::RepositoryAccessGrantService;
use winwincode_control_plane::RepositoryAvailability;
use winwincode_control_plane::RepositoryBindingService;
use winwincode_control_plane::RepositoryDirtyState;
use winwincode_control_plane::WorkerLaunchGrantService;
use winwincode_domain::Instant;
use winwincode_storage::ClientDownlinkAppend;
use winwincode_storage::ClientExchangeCursors;
use winwincode_storage::ClientNodeRecord;
use winwincode_storage::ClientNodeRegistration;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::ConnectCodePublication;
use winwincode_storage::ConnectCodeRevocation;
use winwincode_storage::LaunchAckSettlement;
use winwincode_storage::LocalApplyResult;
use winwincode_storage::LocalApplySettlement;
use winwincode_storage::LocalApplyStrategy;
use winwincode_storage::LocalCandidateRetained;
use winwincode_storage::ManagedAppStatusRecord;
use winwincode_storage::OccupancyReleaseReason;
use winwincode_storage::ProductStateStorage;
use winwincode_storage::RepositoryAccessGrantIssuance;
use winwincode_storage::RepositoryBindingProjection;
use winwincode_storage::RepositoryGrantPermissions;
use winwincode_storage::RepositoryScanOutcome;
use winwincode_storage::SqliteStorage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientExchangeConfig {
    pub max_frame_bytes: usize,
    pub max_frames_per_exchange: usize,
    pub heartbeat_interval_ms: u32,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCredentialDelivery {
    pub client_node_id: String,
    pub worker_launch_grant_id: String,
    pub worker_session_id: String,
    pub credential_digest: String,
    pub worker_credential: String,
    pub expires_at: Instant,
}

impl fmt::Debug for WorkerCredentialDelivery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerCredentialDelivery")
            .field("client_node_id", &self.client_node_id)
            .field("worker_launch_grant_id", &self.worker_launch_grant_id)
            .field("worker_session_id", &self.worker_session_id)
            .field("credential_digest", &self.credential_digest)
            .field("worker_credential", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Default for ClientExchangeConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_frames_per_exchange: 64,
            heartbeat_interval_ms: 5_000,
        }
    }
}

impl ClientExchangeConfig {
    pub fn try_new(
        max_frame_bytes: usize,
        max_frames_per_exchange: usize,
        heartbeat_interval_ms: u32,
    ) -> Result<Self, ClientExchangeError> {
        if max_frame_bytes == 0 || max_frames_per_exchange == 0 {
            return Err(ClientExchangeError::invalid_request());
        }
        Ok(Self {
            max_frame_bytes,
            max_frames_per_exchange,
            heartbeat_interval_ms,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientExchangeErrorKind {
    Authentication,
    InvalidRequest,
    Unavailable,
    CommandRejected {
        status: winwincode_client_port::domain::CommandAckStatus,
        code: winwincode_client_port::domain::ClientControlErrorCode,
        current_revision: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientExchangeError {
    kind: ClientExchangeErrorKind,
}

impl ClientExchangeError {
    const fn new(kind: ClientExchangeErrorKind) -> Self {
        Self { kind }
    }

    const fn authentication() -> Self {
        Self::new(ClientExchangeErrorKind::Authentication)
    }

    const fn invalid_request() -> Self {
        Self::new(ClientExchangeErrorKind::InvalidRequest)
    }

    const fn unavailable() -> Self {
        Self::new(ClientExchangeErrorKind::Unavailable)
    }

    #[must_use]
    pub const fn is_authentication(&self) -> bool {
        matches!(self.kind, ClientExchangeErrorKind::Authentication)
    }

    #[must_use]
    pub const fn is_invalid_request(&self) -> bool {
        matches!(self.kind, ClientExchangeErrorKind::InvalidRequest)
    }
}

impl fmt::Display for ClientExchangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.kind {
            ClientExchangeErrorKind::Authentication => "device credential authentication failed",
            ClientExchangeErrorKind::InvalidRequest => "client exchange request is invalid",
            ClientExchangeErrorKind::Unavailable => "client exchange storage is unavailable",
            ClientExchangeErrorKind::CommandRejected { .. } => "client command was rejected",
        };
        formatter.write_str(reason)
    }
}

impl std::error::Error for ClientExchangeError {}

pub trait ClientExchangePort: Send + Sync {
    fn exchange(
        &self,
        credential: Option<Vec<u8>>,
        request_body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, ClientExchangeError>;

    fn authenticate_device(
        &self,
        credential: &[u8],
        client_node_id: &str,
    ) -> Result<(), ClientExchangeError>;

    fn publish_worker_credential(
        &self,
        delivery: WorkerCredentialDelivery,
    ) -> Result<(), ClientExchangeError>;

    fn managed_app_status(&self, _client_node_id: &str, _run_id: &str) -> Option<ManagedAppStatus> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct ClientExchangeApplication {
    data_directory: PathBuf,
    config: ClientExchangeConfig,
    exchange_lock: Arc<Mutex<()>>,
}

impl ClientExchangeApplication {
    pub fn open(
        data_directory: impl Into<PathBuf>,
        config: &ClientExchangeConfig,
    ) -> Result<Self, ClientExchangeError> {
        let config = ClientExchangeConfig::try_new(
            config.max_frame_bytes,
            config.max_frames_per_exchange,
            config.heartbeat_interval_ms,
        )?;
        Ok(Self {
            data_directory: data_directory.into(),
            config,
            exchange_lock: Arc::new(Mutex::new(())),
        })
    }

    fn apply(
        &self,
        credential: Option<&[u8]>,
        request_body: &[u8],
        now: &Instant,
    ) -> Result<ExchangeResponseBody, ClientExchangeError> {
        let _exchange_guard = self
            .exchange_lock
            .lock()
            .map_err(|_| ClientExchangeError::unavailable())?;
        let request = parse_request(request_body)?;
        let codec = FrameCodec::new(self.config.max_frame_bytes);
        let envelopes = decode_frames(&codec, &request, self.config.max_frames_per_exchange)?;
        if credential.is_none()
            && envelopes
                .iter()
                .any(|envelope| !matches!(envelope.message, ClientToServerMessage::Enroll(_)))
        {
            return Err(ClientExchangeError::authentication());
        }
        let node_id = envelopes[0].client_node_id.clone();
        let mut storage = SqliteStorage::open(&self.data_directory)
            .map_err(|_| ClientExchangeError::unavailable())?;
        let response = match credential {
            None => self.enroll_exchange(
                &mut storage,
                &node_id,
                &envelopes,
                request.ack_sequence,
                now,
            ),
            Some(raw) => self.authenticated_exchange(
                &mut storage,
                &node_id,
                std::str::from_utf8(raw)
                    .ok()
                    .and_then(decode_credential_secret)
                    .as_ref(),
                &envelopes,
                &request,
                now,
            ),
        };
        Box::new(storage)
            .close()
            .map_err(|_| ClientExchangeError::unavailable())?;
        response
    }

    #[allow(clippy::too_many_lines)]
    fn enroll_exchange(
        &self,
        storage: &mut SqliteStorage,
        node_id: &str,
        envelopes: &[ClientToServerEnvelope],
        client_ack: u64,
        now: &Instant,
    ) -> Result<ExchangeResponseBody, ClientExchangeError> {
        for envelope in envelopes {
            if !matches!(envelope.message, ClientToServerMessage::Enroll(_)) {
                return Err(ClientExchangeError::authentication());
            }
        }
        let record = {
            let mut registry = ClientRegistryService::new(storage);
            if is_canonical_client_node_id(node_id) {
                registry
                    .snapshot(node_id)
                    .map_err(|_| ClientExchangeError::unavailable())?
            } else {
                None
            }
        };
        let payload = enroll_payload(&envelopes[0])?;
        match record {
            None => {
                if payload.command.expected_revision != 0 || client_ack != 0 {
                    return Err(ClientExchangeError::invalid_request());
                }
                self.create_enrollment(storage, None, payload, envelopes, now)
            }
            Some(record)
                if record.presence_state == ClientPresenceState::PendingEnrollment
                    && record.device_credential_digest.is_none() =>
            {
                let cursors = {
                    let mut registry = ClientRegistryService::new(storage);
                    cursors_or_unavailable(&mut registry, node_id)?
                };
                let high_water = outbox_high_water_or_unavailable(storage, node_id)?;
                if client_ack > cursors.server_to_client_ack_sequence.max(high_water) {
                    return Err(ClientExchangeError::invalid_request());
                }
                self.create_enrollment(storage, Some(&record), payload, envelopes, now)
            }
            Some(_) => Err(ClientExchangeError::authentication()),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn create_enrollment(
        &self,
        storage: &mut SqliteStorage,
        existing: Option<&ClientNodeRecord>,
        payload: &ClientEnrollPayload,
        envelopes: &[ClientToServerEnvelope],
        now: &Instant,
    ) -> Result<ExchangeResponseBody, ClientExchangeError> {
        let instance = &envelopes[0].client_instance_id;
        let (credential_hex, digest) = issue_credential()?;
        let (node_id, mut public_client_id, expected_revision) = match existing {
            None => (
                generate_prefixed_id("cnd_")?,
                generate_public_client_id()?,
                0,
            ),
            Some(record) => (
                record.client_node_id.clone(),
                record.public_client_id.clone(),
                record.revision,
            ),
        };
        let fresh = existing.is_none();
        let receipt = loop {
            let registration = ClientNodeRegistration::try_new(
                node_id.clone(),
                public_client_id.clone(),
                payload.display_name.clone(),
                platform_name(payload.platform),
                architecture_name(payload.architecture),
                payload.client_version.clone(),
                Some(digest.clone()),
                Some(instance.clone()),
                0,
            )
            .map_err(|_| ClientExchangeError::invalid_request())?;
            let mut registry = ClientRegistryService::new(storage);
            match registry.register(&registration, expected_revision, now) {
                Ok(receipt) => break receipt,
                Err(error)
                    if fresh
                        && error.kind() == ClientRegistryServiceErrorKind::IdentityConflict =>
                {
                    public_client_id = generate_public_client_id()?;
                }
                Err(_) => return Err(ClientExchangeError::unavailable()),
            }
        };
        let node_id = receipt.record.client_node_id.clone();

        let mut settler = BatchSettler::new(instance, 0);
        let mut replay_hint = None;
        for envelope in envelopes {
            let identity = FrameCodec::envelope_identity(envelope)
                .map_err(|_| ClientExchangeError::invalid_request())?;
            let command = command_identity(&envelope.message, &identity.payload_digest);
            match settler.ingest(instance, &identity, command.as_ref()) {
                BatchOutcome::Accepted => {}
                BatchOutcome::Gap {
                    replay_from_sequence,
                } => {
                    replay_hint = Some(replay_from_sequence);
                    break;
                }
                BatchOutcome::Duplicate | BatchOutcome::Refused => break,
            }
        }
        let ack_sequence = settler.ack_sequence();

        ClientRegistryService::new(storage)
            .advance_exchange_cursors(&node_id, ack_sequence, 0)
            .map_err(|_| ClientExchangeError::unavailable())?;

        Ok(ExchangeResponseBody {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence,
            replay_from_sequence: replay_hint,
            frames: Vec::new(),
            enrollment: Some(EnrollmentIssuance {
                client_node_id: node_id,
                public_client_id: receipt.record.public_client_id,
                device_credential: credential_hex,
                device_credential_digest: digest,
                heartbeat_interval_ms: self.config.heartbeat_interval_ms,
                server_time: now.0.clone(),
            }),
            worker_credentials: Vec::new(),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn authenticated_exchange(
        &self,
        storage: &mut SqliteStorage,
        node_id: &str,
        secret: Option<&[u8; 32]>,
        envelopes: &[ClientToServerEnvelope],
        request: &ExchangeRequestBody,
        now: &Instant,
    ) -> Result<ExchangeResponseBody, ClientExchangeError> {
        let client_ack = request.ack_sequence;
        let Some(secret) = secret else {
            return Err(ClientExchangeError::authentication());
        };
        if !is_canonical_client_node_id(node_id) {
            return Err(ClientExchangeError::authentication());
        }
        let record = {
            let mut registry = ClientRegistryService::new(storage);
            registry
                .snapshot(node_id)
                .map_err(|_| ClientExchangeError::unavailable())?
                .ok_or(ClientExchangeError::authentication())?
        };
        let Some(stored_digest) = record.device_credential_digest.as_deref() else {
            return Err(ClientExchangeError::authentication());
        };
        if record.presence_state == ClientPresenceState::Revoked {
            return Err(ClientExchangeError::authentication());
        }
        if !credential_digest_matches(secret, stored_digest) {
            return Err(ClientExchangeError::authentication());
        }
        let cursors = {
            let mut registry = ClientRegistryService::new(storage);
            cursors_or_unavailable(&mut registry, node_id)?
        };
        let outbox_high_water = outbox_high_water_or_unavailable(storage, node_id)?;
        if client_ack > cursors.server_to_client_ack_sequence.max(outbox_high_water) {
            return Err(ClientExchangeError::invalid_request());
        }
        let effective_instance = record
            .current_instance_id
            .clone()
            .ok_or(ClientExchangeError::unavailable())?;

        let mut settler =
            BatchSettler::new(&effective_instance, cursors.client_to_server_ack_sequence);
        let mut replay_hint = None;
        for envelope in envelopes {
            if matches!(envelope.message, ClientToServerMessage::Enroll(_)) {
                break;
            }
            if matches!(envelope.message, ClientToServerMessage::Hello(_))
                && envelope.client_instance_id != effective_instance
            {
                supersede_instance(storage, &record, envelope, now)?;
                settler.take_over_instance(&envelope.client_instance_id);
            }
            let identity = FrameCodec::envelope_identity(envelope)
                .map_err(|_| ClientExchangeError::invalid_request())?;
            let command = command_identity(&envelope.message, &identity.payload_digest);
            match settler.ingest(&envelope.client_instance_id, &identity, command.as_ref()) {
                BatchOutcome::Accepted | BatchOutcome::Duplicate => {
                    let mut reply = None;
                    let key = command
                        .as_ref()
                        .map(|identity| identity.idempotency_key.as_str());
                    let completed = storage
                        .client_node_registry()
                        .and_then(|mut registry| {
                            registry.reserve_exchange_frame(
                                node_id,
                                envelope.sequence,
                                &envelope.message_id,
                                &identity.payload_digest,
                                key,
                            )
                        })
                        .map_err(|error| match error.kind() {
                            winwincode_storage::ClientRegistryErrorKind::IdentityConflict => {
                                ClientExchangeError::invalid_request()
                            }
                            _ => ClientExchangeError::unavailable(),
                        })?;
                    if !completed && envelope.sequence > cursors.client_to_server_ack_sequence {
                        let result = apply_effect_with_status(
                            storage,
                            &self.data_directory,
                            node_id,
                            envelope,
                            now,
                        );
                        match result {
                            Err(error)
                                if matches!(
                                    error.kind,
                                    ClientExchangeErrorKind::CommandRejected { .. }
                                ) && key.is_some() =>
                            {
                                reply = Some(command_rejection_reply(
                                    storage, node_id, envelope, error, now,
                                )?);
                            }
                            result => result?,
                        }
                    }
                    storage
                        .client_node_registry()
                        .and_then(|mut registry| {
                            registry.complete_exchange_frame_with_reply(
                                node_id,
                                envelope.sequence,
                                key,
                                reply.as_ref().map(|reply| (reply, now)),
                            )
                        })
                        .map_err(|_| ClientExchangeError::unavailable())?;
                }
                BatchOutcome::Gap {
                    replay_from_sequence,
                } => {
                    replay_hint = Some(replay_from_sequence);
                    break;
                }
                BatchOutcome::Refused => break,
            }
        }
        let ack_sequence = settler.ack_sequence();
        let cursors = {
            let mut registry = ClientRegistryService::new(storage);
            registry
                .advance_exchange_cursors(node_id, ack_sequence, client_ack)
                .map_err(|_| ClientExchangeError::unavailable())?
        };

        let mut downlink = storage
            .client_downlink_outbox()
            .map_err(|_| ClientExchangeError::unavailable())?;
        let batch = downlink
            .deliverable(
                node_id,
                cursors.server_to_client_ack_sequence,
                self.config.max_frames_per_exchange,
            )
            .map_err(|_| ClientExchangeError::unavailable())?;
        let frames = batch
            .iter()
            .map(|frame| serde_json::from_str(&frame.frame))
            .collect::<Result<Vec<Value>, _>>()
            .map_err(|_| ClientExchangeError::unavailable())?;
        let worker_credentials = self.worker_credentials_for_frames(node_id, &frames, now)?;
        downlink
            .retain_through(node_id, cursors.server_to_client_ack_sequence)
            .map_err(|_| ClientExchangeError::unavailable())?;

        Ok(ExchangeResponseBody {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence,
            replay_from_sequence: replay_hint,
            frames,
            enrollment: None,
            worker_credentials,
        })
    }

    fn worker_credentials_for_frames(
        &self,
        node_id: &str,
        frames: &[Value],
        now: &Instant,
    ) -> Result<Vec<WorkerCredentialResponse>, ClientExchangeError> {
        let launches = frames
            .iter()
            .filter(|frame| {
                frame.get("kind").and_then(Value::as_str) == Some("client.worker.launch")
            })
            .filter_map(|frame| frame.get("payload")?.get("launchGrant"))
            .filter_map(|grant| {
                Some((
                    grant.get("workerLaunchGrantId")?.as_str()?,
                    grant.get("workerSessionId")?.as_str()?,
                    grant.get("credentialDigest")?.as_str()?,
                    grant.get("expiresAt").and_then(Value::as_str),
                ))
            })
            .collect::<Vec<_>>();
        let mut responses = Vec::new();
        for (grant_id, session_id, digest, expires_at) in launches {
            if expires_at.is_some_and(|deadline| deadline <= now.0.as_str()) {
                continue;
            }
            let Ok(bytes) =
                winwincode_control_plane::PrivateLaunchMaterialStore::open(&self.data_directory)
                    .and_then(|store| store.get(grant_id))
            else {
                continue;
            };
            let entry: WorkerCredentialDelivery =
                serde_json::from_slice(&bytes).map_err(|_| ClientExchangeError::unavailable())?;
            if entry.client_node_id != node_id
                || entry.worker_launch_grant_id != grant_id
                || entry.worker_session_id != session_id
                || entry.credential_digest != digest
            {
                return Err(ClientExchangeError::unavailable());
            }
            if entry.expires_at.0 <= now.0 {
                continue;
            }
            responses.push(WorkerCredentialResponse {
                worker_launch_grant_id: entry.worker_launch_grant_id,
                worker_session_id: entry.worker_session_id,
                credential_digest: entry.credential_digest,
                worker_credential: entry.worker_credential,
            });
        }
        Ok(responses)
    }
}

fn outbox_high_water_or_unavailable(
    storage: &mut SqliteStorage,
    node_id: &str,
) -> Result<u64, ClientExchangeError> {
    storage
        .client_downlink_outbox()
        .map_err(|_| ClientExchangeError::unavailable())?
        .high_water(node_id)
        .map_err(|_| ClientExchangeError::unavailable())
}

fn cursors_or_unavailable(
    registry: &mut ClientRegistryService<'_>,
    node_id: &str,
) -> Result<ClientExchangeCursors, ClientExchangeError> {
    registry
        .exchange_cursors(node_id)
        .map_err(|_| ClientExchangeError::unavailable())?
        .ok_or(ClientExchangeError::unavailable())
}

enum BatchOutcome {
    Accepted,
    Duplicate,
    Gap { replay_from_sequence: u64 },
    Refused,
}

struct BatchSettler {
    instance: String,
    ack: AckCursor,
    dedup: DedupRegister,
}

impl BatchSettler {
    fn new(instance: impl Into<String>, ack_sequence: u64) -> Self {
        Self {
            instance: instance.into(),
            ack: AckCursor::from_ack(ack_sequence),
            dedup: DedupRegister::new(),
        }
    }

    fn take_over_instance(&mut self, instance: &str) {
        self.instance = instance.to_string();
    }

    fn ack_sequence(&self) -> u64 {
        self.ack.ack_sequence()
    }

    fn ingest(
        &mut self,
        client_instance_id: &str,
        identity: &FrameIdentity,
        command: Option<&CommandIdentity>,
    ) -> BatchOutcome {
        if identity.sequence == 0
            || identity.message_id.is_empty()
            || identity.payload_digest.is_empty()
        {
            return BatchOutcome::Refused;
        }
        if self.instance != client_instance_id {
            return BatchOutcome::Refused;
        }
        match self.ack.observe(identity.sequence) {
            SequenceVerdict::Zero => BatchOutcome::Refused,
            SequenceVerdict::Gap {
                replay_from_sequence,
            } => BatchOutcome::Gap {
                replay_from_sequence,
            },
            SequenceVerdict::Duplicate => match self.dedup.check_frame(identity) {
                DedupVerdict::Conflict => BatchOutcome::Refused,
                DedupVerdict::Duplicate | DedupVerdict::New => BatchOutcome::Duplicate,
            },
            SequenceVerdict::Accept => {
                if command.is_some_and(|command| {
                    self.dedup.check_command(command) == DedupVerdict::Conflict
                }) {
                    return BatchOutcome::Refused;
                }
                if self.dedup.check_frame(identity) == DedupVerdict::Conflict {
                    return BatchOutcome::Refused;
                }
                self.dedup.record_frame(identity);
                if let Some(command) = command {
                    self.dedup.record_command(command);
                }
                let _ = self.ack.advance(identity.sequence);
                BatchOutcome::Accepted
            }
        }
    }
}

impl ClientExchangePort for ClientExchangeApplication {
    fn exchange(
        &self,
        credential: Option<Vec<u8>>,
        request_body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, ClientExchangeError> {
        let response = self.apply(credential.as_deref(), request_body, &now)?;
        serde_json::to_vec(&response).map_err(|_| ClientExchangeError::unavailable())
    }

    fn authenticate_device(
        &self,
        credential: &[u8],
        client_node_id: &str,
    ) -> Result<(), ClientExchangeError> {
        let secret = std::str::from_utf8(credential)
            .ok()
            .and_then(decode_credential_secret)
            .ok_or_else(ClientExchangeError::authentication)?;
        if !is_canonical_client_node_id(client_node_id) {
            return Err(ClientExchangeError::authentication());
        }
        let mut storage = SqliteStorage::open(&self.data_directory)
            .map_err(|_| ClientExchangeError::unavailable())?;
        let record = ClientRegistryService::new(&mut storage)
            .snapshot(client_node_id)
            .map_err(|_| ClientExchangeError::unavailable())?
            .ok_or_else(ClientExchangeError::authentication)?;
        let accepted = record.presence_state != ClientPresenceState::Revoked
            && record
                .device_credential_digest
                .as_deref()
                .is_some_and(|digest| credential_digest_matches(&secret, digest));
        Box::new(storage)
            .close()
            .map_err(|_| ClientExchangeError::unavailable())?;
        if accepted {
            Ok(())
        } else {
            Err(ClientExchangeError::authentication())
        }
    }

    fn publish_worker_credential(
        &self,
        delivery: WorkerCredentialDelivery,
    ) -> Result<(), ClientExchangeError> {
        let secret = decode_credential_secret(&delivery.worker_credential)
            .ok_or_else(ClientExchangeError::invalid_request)?;
        if !credential_digest_matches(&secret, &delivery.credential_digest) {
            return Err(ClientExchangeError::invalid_request());
        }
        let bytes =
            serde_json::to_vec(&delivery).map_err(|_| ClientExchangeError::unavailable())?;
        winwincode_control_plane::PrivateLaunchMaterialStore::open(&self.data_directory)
            .and_then(|store| store.put(&delivery.worker_launch_grant_id, &bytes))
            .map_err(|_| ClientExchangeError::unavailable())?;
        Ok(())
    }

    fn managed_app_status(&self, client_node_id: &str, run_id: &str) -> Option<ManagedAppStatus> {
        let storage = SqliteStorage::open(&self.data_directory).ok()?;
        let record = storage
            .load_managed_app_status(client_node_id, run_id)
            .ok()??;
        serde_json::from_slice(&record.status_json).ok()
    }
}

#[derive(Debug, Deserialize)]
struct ExchangeRequestBody {
    #[serde(rename = "schemaVersion")]
    schema_version: String,
    #[serde(default)]
    frames: Vec<Value>,
    #[serde(rename = "ackSequence", default)]
    ack_sequence: u64,
}

#[derive(Debug, Serialize)]
struct ExchangeResponseBody {
    #[serde(rename = "schemaVersion")]
    schema_version: String,
    #[serde(rename = "ackSequence")]
    ack_sequence: u64,
    #[serde(rename = "replayFromSequence", skip_serializing_if = "Option::is_none")]
    replay_from_sequence: Option<u64>,
    frames: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enrollment: Option<EnrollmentIssuance>,
    #[serde(rename = "workerCredentials", skip_serializing_if = "Vec::is_empty")]
    worker_credentials: Vec<WorkerCredentialResponse>,
}

#[derive(Serialize)]
struct WorkerCredentialResponse {
    #[serde(rename = "workerLaunchGrantId")]
    worker_launch_grant_id: String,
    #[serde(rename = "workerSessionId")]
    worker_session_id: String,
    #[serde(rename = "credentialDigest")]
    credential_digest: String,
    #[serde(rename = "workerCredential")]
    worker_credential: String,
}

impl fmt::Debug for WorkerCredentialResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerCredentialResponse")
            .field("worker_launch_grant_id", &self.worker_launch_grant_id)
            .field("worker_session_id", &self.worker_session_id)
            .field("credential_digest", &self.credential_digest)
            .field("worker_credential", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct EnrollmentIssuance {
    #[serde(rename = "clientNodeId")]
    client_node_id: String,
    #[serde(rename = "publicClientId")]
    public_client_id: String,
    #[serde(rename = "deviceCredential")]
    device_credential: String,
    #[serde(rename = "deviceCredentialDigest")]
    device_credential_digest: String,
    #[serde(rename = "heartbeatIntervalMs")]
    heartbeat_interval_ms: u32,
    #[serde(rename = "serverTime")]
    server_time: String,
}

fn parse_request(request_body: &[u8]) -> Result<ExchangeRequestBody, ClientExchangeError> {
    serde_json::from_slice(request_body).map_err(|_| ClientExchangeError::invalid_request())
}

fn decode_frames(
    codec: &FrameCodec,
    request: &ExchangeRequestBody,
    max_frames: usize,
) -> Result<Vec<ClientToServerEnvelope>, ClientExchangeError> {
    if request.frames.is_empty() || request.frames.len() > max_frames {
        return Err(ClientExchangeError::invalid_request());
    }
    if request.schema_version != CLIENT_CONTROL_PORT_SCHEMA_VERSION {
        return Err(ClientExchangeError::invalid_request());
    }
    let mut envelopes = Vec::with_capacity(request.frames.len());
    let mut node_id = None;
    for frame in &request.frames {
        let bytes =
            serde_json::to_vec(frame).map_err(|_| ClientExchangeError::invalid_request())?;
        let envelope: ClientToServerEnvelope = codec
            .decode(&bytes)
            .map_err(|_| ClientExchangeError::invalid_request())?;
        if envelope.schema_version != CLIENT_CONTROL_PORT_SCHEMA_VERSION {
            return Err(ClientExchangeError::invalid_request());
        }
        match &node_id {
            None => node_id = Some(envelope.client_node_id.clone()),
            Some(routed) if routed != &envelope.client_node_id => {
                return Err(ClientExchangeError::invalid_request());
            }
            Some(_) => {}
        }
        envelopes.push(envelope);
    }
    Ok(envelopes)
}

fn enroll_payload(
    envelope: &ClientToServerEnvelope,
) -> Result<&ClientEnrollPayload, ClientExchangeError> {
    match &envelope.message {
        ClientToServerMessage::Enroll(payload) => Ok(payload),
        _ => Err(ClientExchangeError::authentication()),
    }
}

fn command_identity(
    message: &ClientToServerMessage,
    payload_digest: &str,
) -> Option<CommandIdentity> {
    let key = match message {
        ClientToServerMessage::Enroll(payload) => &payload.command.idempotency_key,
        ClientToServerMessage::ConnectCodePublished(payload) => &payload.command.idempotency_key,

        ClientToServerMessage::RepositoryUpsert(payload) => &payload.command.idempotency_key,
        ClientToServerMessage::RepositoryRemoved(payload) => &payload.command.idempotency_key,
        ClientToServerMessage::OccupancyAck(payload) => &payload.occupancy.command.idempotency_key,
        ClientToServerMessage::OccupancyRejected(payload) => {
            &payload.occupancy.command.idempotency_key
        }
        ClientToServerMessage::WorkerLaunchAck(payload) => {
            &payload.occupancy.command.idempotency_key
        }
        ClientToServerMessage::CandidateRetained(payload) => {
            &payload.occupancy.command.idempotency_key
        }
        ClientToServerMessage::CandidateApplyResult(payload) => {
            &payload.occupancy.command.idempotency_key
        }
        ClientToServerMessage::ProviderReport(_)
        | ClientToServerMessage::RepositoryRegistered { .. }
        | ClientToServerMessage::ExtensionReport(_)
        | ClientToServerMessage::Hello(_)
        | ClientToServerMessage::Heartbeat(_)
        | ClientToServerMessage::RepositoryStatus(_)
        | ClientToServerMessage::WorkerState(_)
        | ClientToServerMessage::WorkerReconcile(_)
        | ClientToServerMessage::ManagedAppStatus(_)
        | ClientToServerMessage::CommandAck(_) => return None,
    };
    Some(CommandIdentity::new(key.clone(), payload_digest))
}

fn require_durable_presence(
    error: &winwincode_control_plane::ClientRegistryServiceError,
) -> Result<(), ClientExchangeError> {
    use winwincode_control_plane::ClientRegistryServiceErrorKind;
    match error.kind() {
        ClientRegistryServiceErrorKind::PresenceTransition
        | ClientRegistryServiceErrorKind::RevisionConflict => Ok(()),
        _ => Err(ClientExchangeError::unavailable()),
    }
}

fn apply_connection_policy(
    storage: &mut SqliteStorage,
    node_id: &str,
    accepting_connections: bool,
    lock_state: winwincode_client_port::domain::ClientLockState,
) -> Result<ClientNodeRecord, ClientExchangeError> {
    use winwincode_client_port::domain::ClientLockState as WireLockState;
    use winwincode_storage::ClientLockState;

    let mut registry = ClientRegistryService::new(storage);
    let record = registry
        .snapshot(node_id)
        .map_err(|_| ClientExchangeError::unavailable())?
        .ok_or_else(ClientExchangeError::unavailable)?;
    let lock_state = match lock_state {
        WireLockState::Unlocked => ClientLockState::Unlocked,
        WireLockState::Locked => ClientLockState::Locked,
    };
    registry
        .update_connection_policy(node_id, accepting_connections, lock_state, record.revision)
        .map_err(|_| ClientExchangeError::unavailable())
}

#[cfg(test)]
fn apply_effect(
    storage: &mut SqliteStorage,
    data_directory: &Path,
    node_id: &str,
    envelope: &ClientToServerEnvelope,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    apply_effect_with_status(storage, data_directory, node_id, envelope, now)
}

#[allow(clippy::too_many_lines)]
fn apply_effect_with_status(
    storage: &mut SqliteStorage,
    data_directory: &Path,
    node_id: &str,
    envelope: &ClientToServerEnvelope,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    match &envelope.message {
        ClientToServerMessage::WorkerState(payload) => {
            settle_worker_exit(storage, node_id, payload, now)
        }
        ClientToServerMessage::WorkerReconcile(payload) => {
            settle_worker_reconciliation(storage, node_id, payload, &envelope.occurred_at, now)
        }
        ClientToServerMessage::RepositoryRegistered { receipt } => {
            crate::device_providers::observe_repository_registration(storage, node_id, receipt)
                .map_err(|_| ClientExchangeError::unavailable())
        }
        ClientToServerMessage::ExtensionReport(report) => {
            crate::device_providers::observe_extensions(storage, node_id, report)
                .map_err(|_| ClientExchangeError::unavailable())
        }
        ClientToServerMessage::ProviderReport(report) => {
            crate::device_providers::observe(storage, node_id, report)
                .map_err(|_| ClientExchangeError::unavailable())
        }
        ClientToServerMessage::Hello(payload) => {
            let record = ClientRegistryService::new(storage)
                .snapshot(node_id)
                .map_err(|_| ClientExchangeError::unavailable())?;
            if let Some(record) = record {
                supersede_instance(storage, &record, envelope, now)?;
            }
            let record = apply_connection_policy(
                storage,
                node_id,
                payload.accepting_connections,
                payload.lock_state,
            )?;
            let mut registry = ClientRegistryService::new(storage);
            if let Some(target) = presence_target(payload.presence_state)
                && let Err(error) = registry.update_presence(node_id, target, record.revision)
            {
                require_durable_presence(&error)?;
            }
            Ok(())
        }
        ClientToServerMessage::Heartbeat(payload) => {
            let record = apply_connection_policy(
                storage,
                node_id,
                payload.accepting_connections,
                payload.lock_state,
            )?;
            let mut registry = ClientRegistryService::new(storage);
            if let Err(error) = registry.heartbeat(
                node_id,
                payload.capacity.running_worker_sessions,
                now,
                record.revision,
            ) {
                require_durable_presence(&error)?;
            }
            if payload.capacity.running_worker_sessions == 0 {
                let mut occupancy = ClientOccupancyService::new(storage);
                if let Some(lease) = occupancy
                    .active_lease_for_node(node_id)
                    .map_err(|_| ClientExchangeError::unavailable())?
                    && lease.state == OccupancyLeaseState::Draining
                {
                    occupancy
                        .drain_complete(&lease.occupancy_lease_id)
                        .map_err(|_| ClientExchangeError::unavailable())?;
                }
            }
            Ok(())
        }
        ClientToServerMessage::ConnectCodePublished(payload) => {
            let Ok(publication) = ConnectCodePublication::try_new(
                payload.connect_code_id.clone(),
                payload.code_digest.clone(),
                node_id,
                &envelope.client_instance_id,
                payload.generation,
                Instant(payload.expires_at.clone()),
                payload.remaining_attempts,
            ) else {
                return Err(rejected_command(None));
            };
            let mut connect = ConnectCodeService::new(storage);
            match connect
                .active_code_for_client(node_id)
                .map_err(|_| ClientExchangeError::unavailable())?
            {
                None if payload.generation == 1 => {
                    connect
                        .publish(&publication, now)
                        .map_err(|error| domain_command_error(matches!(error.kind(),
                            winwincode_control_plane::ClientConnectServiceErrorKind::Storage
                            | winwincode_control_plane::ClientConnectServiceErrorKind::CorruptState)))?;
                }
                Some(current) if current.connect_code_id == payload.connect_code_id => {
                    if current.code_digest != payload.code_digest
                        || current.generation != payload.generation
                        || current.expires_at.0 != payload.expires_at
                    {
                        return Err(rejected_command(None));
                    }
                }
                Some(current) if payload.generation == current.generation + 1 => {
                    let revocation =
                        ConnectCodeRevocation::try_new(current.connect_code_id, current.revision)
                            .map_err(|_| ClientExchangeError::unavailable())?;
                    connect
                        .refresh_code(&revocation, &publication, now)
                        .map_err(|error| domain_command_error(matches!(error.kind(),
                            winwincode_control_plane::ClientConnectServiceErrorKind::Storage
                            | winwincode_control_plane::ClientConnectServiceErrorKind::CorruptState)))?;
                }
                None | Some(_) => return Err(rejected_command(None)),
            }
            Ok(())
        }
        ClientToServerMessage::RepositoryUpsert(payload) => {
            let repository = &payload.repository;
            let projection = RepositoryBindingProjection::try_new(
                repository.repository_binding_id.clone(),
                node_id,
                repository.display_name.clone(),
                (!repository.default_branch.is_empty()).then(|| repository.default_branch.clone()),
                (!repository.head_commit.is_empty()).then(|| repository.head_commit.clone()),
                repository_dirty_state(repository.dirty_state),
                repository_availability(repository.availability),
                repository.repository_fingerprint.clone(),
            )
            .map_err(|_| rejected_command(None))?;
            let binding_id = repository.repository_binding_id.clone();
            let binding_exists = {
                let mut bindings = RepositoryBindingService::new(storage);
                let current_revision = bindings
                    .snapshot(&binding_id)
                    .map_err(|_| ClientExchangeError::unavailable())?
                    .map(|record| record.revision);
                bindings
                    .upsert(
                        &projection,
                        Some(&Instant(repository.last_scanned_at.clone())),
                        payload.command.expected_revision,
                        now,
                    )
                    .map_err(|error| repository_command_error(&error, current_revision))?;
                bindings
                    .snapshot(&binding_id)
                    .map_err(|_| ClientExchangeError::unavailable())?
                    .is_some_and(|binding| binding.client_node_id == node_id)
            };
            if !binding_exists {
                return Ok(());
            }
            let managers = AccessGrantService::new(storage)
                .active_grants_for_client(node_id)
                .map_err(|_| ClientExchangeError::unavailable())?;
            for manager in managers
                .into_iter()
                .filter(|grant| grant.permissions.can_manage())
            {
                let already_granted = RepositoryAccessGrantService::new(storage)
                    .active_grants_for_binding(&binding_id)
                    .map_err(|_| ClientExchangeError::unavailable())?
                    .into_iter()
                    .any(|grant| grant.user_id == manager.user_id);
                if already_granted {
                    continue;
                }
                let issuance = RepositoryAccessGrantIssuance::try_new(
                    generate_prefixed_id("rag_")?,
                    &binding_id,
                    &manager.user_id,
                    &manager.user_id,
                )
                .map_err(|_| ClientExchangeError::unavailable())?;
                RepositoryAccessGrantService::new(storage)
                    .create_grant(&issuance, RepositoryGrantPermissions::UseManage, now)
                    .map_err(|_| ClientExchangeError::unavailable())?;
            }
            Ok(())
        }
        ClientToServerMessage::RepositoryRemoved(payload) => {
            let mut bindings = RepositoryBindingService::new(storage);
            if let Some(binding) = bindings
                .snapshot(&payload.repository_binding_id)
                .map_err(|_| ClientExchangeError::unavailable())?
            {
                if binding.client_node_id != node_id {
                    return Err(rejected_command(None));
                }
                if binding.revision != payload.command.expected_revision {
                    return Err(revision_conflict(Some(binding.revision)));
                }
                bindings
                    .remove(&payload.repository_binding_id)
                    .map_err(|error| repository_command_error(&error, Some(binding.revision)))?;
            }
            Ok(())
        }
        ClientToServerMessage::RepositoryStatus(payload) => {
            let Ok(outcome) = RepositoryScanOutcome::try_new(
                repository_availability(payload.availability),
                repository_dirty_state(payload.dirty_state),
            ) else {
                return Ok(());
            };
            let mut bindings = RepositoryBindingService::new(storage);
            let Some(binding) = bindings
                .snapshot(&payload.repository_binding_id)
                .map_err(|_| ClientExchangeError::unavailable())?
            else {
                return Ok(());
            };
            if binding.client_node_id != node_id {
                return Ok(());
            }
            bindings
                .update_availability(
                    &payload.repository_binding_id,
                    &outcome,
                    &Instant(payload.last_scanned_at.clone()),
                    binding.revision,
                )
                .map_err(|_| ClientExchangeError::unavailable())?;
            Ok(())
        }
        ClientToServerMessage::OccupancyAck(payload) => {
            require_occupancy_sender(storage, node_id, &payload.occupancy.occupancy_lease_id)?;
            crate::client_occupancy::observe_client_mirror_revision(
                data_directory,
                node_id,
                payload.occupancy.command.expected_revision,
                now,
            )
            .map_err(|_| ClientExchangeError::unavailable())?;
            let mut occupancy = ClientOccupancyService::new(storage);
            occupancy.record_acknowledgement(
                &payload.occupancy.occupancy_lease_id,
                payload.occupancy.occupancy_fencing_token,
                None,
                now,
            ).map_err(|error| domain_command_error(matches!(error.kind(),
                winwincode_control_plane::ClientOccupancyServiceErrorKind::Storage | winwincode_control_plane::ClientOccupancyServiceErrorKind::CorruptState)))?;
            Ok(())
        }
        ClientToServerMessage::OccupancyRejected(payload) => {
            require_occupancy_sender(storage, node_id, &payload.occupancy.occupancy_lease_id)?;
            crate::client_occupancy::observe_client_mirror_revision(
                data_directory,
                node_id,
                payload.occupancy.command.expected_revision,
                now,
            )
            .map_err(|_| ClientExchangeError::unavailable())?;
            let mut occupancy = ClientOccupancyService::new(storage);
            occupancy.reject_offer(
                &payload.occupancy.occupancy_lease_id,
                payload.occupancy.occupancy_fencing_token,
                OccupancyReleaseReason::ClientRejected,
                now,
            ).map_err(|error| domain_command_error(matches!(error.kind(),
                winwincode_control_plane::ClientOccupancyServiceErrorKind::Storage | winwincode_control_plane::ClientOccupancyServiceErrorKind::CorruptState)))?;
            Ok(())
        }
        ClientToServerMessage::WorkerLaunchAck(payload) => {
            settle_worker_launch_ack(storage, data_directory, node_id, payload, now)?;
            Ok(())
        }
        ClientToServerMessage::CandidateRetained(payload) => {
            crate::client_occupancy::observe_client_mirror_revision(
                data_directory,
                node_id,
                payload.occupancy.command.expected_revision,
                now,
            )
            .map_err(|_| ClientExchangeError::unavailable())?;
            settle_candidate_retained(storage, node_id, payload, now)?;
            Ok(())
        }
        ClientToServerMessage::CandidateApplyResult(payload) => {
            crate::client_occupancy::observe_client_mirror_revision(
                data_directory,
                node_id,
                payload.occupancy.command.expected_revision,
                now,
            )
            .map_err(|_| ClientExchangeError::unavailable())?;
            settle_candidate_apply_result(storage, node_id, payload, now)?;
            Ok(())
        }
        ClientToServerMessage::CommandAck(payload) => {
            observe_command_ack_revision(data_directory, node_id, payload, now)
        }
        ClientToServerMessage::ManagedAppStatus(payload) => {
            let status_json = serde_json::to_vec(&payload.status)
                .map_err(|_| ClientExchangeError::unavailable())?;
            storage
                .save_managed_app_status(&ManagedAppStatusRecord {
                    client_node_id: node_id.to_owned(),
                    run_id: payload.status.run_id.clone(),
                    status_json,
                    occurred_at: envelope.occurred_at.clone(),
                })
                .map_err(|_| ClientExchangeError::unavailable())?;
            Ok(())
        }
        ClientToServerMessage::Enroll(_) => Err(ClientExchangeError::invalid_request()),
    }
}

fn repository_availability(value: WireRepositoryAvailability) -> RepositoryAvailability {
    match value {
        WireRepositoryAvailability::Available => RepositoryAvailability::Available,
        WireRepositoryAvailability::Dirty => RepositoryAvailability::Dirty,
        WireRepositoryAvailability::Unavailable => RepositoryAvailability::Unavailable,
        WireRepositoryAvailability::Moved => RepositoryAvailability::Moved,
        WireRepositoryAvailability::InvalidGit => RepositoryAvailability::InvalidGit,
        WireRepositoryAvailability::PermissionDenied => RepositoryAvailability::PermissionDenied,
        WireRepositoryAvailability::ScanFailed => RepositoryAvailability::ScanFailed,
    }
}

fn rejected_command(current_revision: Option<u64>) -> ClientExchangeError {
    ClientExchangeError::new(ClientExchangeErrorKind::CommandRejected {
        status: winwincode_client_port::domain::CommandAckStatus::RejectedWrongState,
        code: winwincode_client_port::domain::ClientControlErrorCode::WrongState,
        current_revision,
    })
}

fn domain_command_error(storage_failure: bool) -> ClientExchangeError {
    if storage_failure {
        ClientExchangeError::unavailable()
    } else {
        rejected_command(None)
    }
}

fn require_occupancy_sender(
    storage: &mut SqliteStorage,
    node: &str,
    lease: &str,
) -> Result<(), ClientExchangeError> {
    let lease = ClientOccupancyService::new(storage)
        .snapshot(lease)
        .map_err(|_| ClientExchangeError::unavailable())?
        .ok_or_else(|| rejected_command(None))?;
    if lease.client_node_id != node {
        return Err(rejected_command(None));
    }
    Ok(())
}

fn revision_conflict(current_revision: Option<u64>) -> ClientExchangeError {
    ClientExchangeError::new(ClientExchangeErrorKind::CommandRejected {
        status: winwincode_client_port::domain::CommandAckStatus::RejectedRevisionConflict,
        code: winwincode_client_port::domain::ClientControlErrorCode::RevisionConflict,
        current_revision,
    })
}

fn repository_command_error(
    error: &winwincode_control_plane::RepositoryBindingServiceError,
    current_revision: Option<u64>,
) -> ClientExchangeError {
    use winwincode_control_plane::RepositoryBindingServiceErrorKind;
    match error.kind() {
        RepositoryBindingServiceErrorKind::Storage
        | RepositoryBindingServiceErrorKind::CorruptState => ClientExchangeError::unavailable(),
        RepositoryBindingServiceErrorKind::RevisionConflict => revision_conflict(current_revision),
        _ => rejected_command(current_revision),
    }
}

fn command_rejection_reply(
    storage: &mut SqliteStorage,
    node_id: &str,
    envelope: &ClientToServerEnvelope,
    error: ClientExchangeError,
    now: &Instant,
) -> Result<ClientDownlinkAppend, ClientExchangeError> {
    let ClientExchangeErrorKind::CommandRejected {
        status,
        code,
        current_revision,
    } = error.kind
    else {
        return Err(error);
    };
    let value =
        serde_json::to_value(&envelope.message).map_err(|_| ClientExchangeError::unavailable())?;
    let command_kind = serde_json::from_value(value["kind"].clone())
        .map_err(|_| ClientExchangeError::unavailable())?;
    let cursor = cursors_or_unavailable(&mut ClientRegistryService::new(storage), node_id)?;
    let downlink = storage
        .client_downlink_outbox()
        .map_err(|_| ClientExchangeError::unavailable())?;
    let sequence = downlink
        .high_water(node_id)
        .map_err(|_| ClientExchangeError::unavailable())?
        .max(cursor.server_to_client_ack_sequence)
        + 1;
    let receipt = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: generate_prefixed_id("cmsg_")?,
        client_node_id: node_id.to_owned(),
        client_instance_id: envelope.client_instance_id.clone(),
        sequence,
        occurred_at: now.0.clone(),
        message: ServerToClientMessage::CommandAck(
            winwincode_client_port::messages::ClientCommandAckPayload {
                command_kind,
                command_message_id: envelope.message_id.clone(),
                status,
                current_revision,
                error: Some(winwincode_client_port::domain::ClientControlError {
                    code,
                    message:
                        "Server rejected the command; refresh the current state before retrying"
                            .into(),
                    retryable: false,
                }),
            },
        ),
    };
    let frame = serde_json::to_string(&receipt).map_err(|_| ClientExchangeError::unavailable())?;
    let append = ClientDownlinkAppend::try_new(node_id, receipt.message_id, sequence, frame)
        .map_err(|_| ClientExchangeError::unavailable())?;
    Ok(append)
}

fn settle_worker_reconciliation(
    storage: &mut SqliteStorage,
    node_id: &str,
    payload: &winwincode_client_port::messages::ClientWorkerReconcilePayload,
    reported_at: &str,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    use winwincode_client_port::domain::WorkerReconcileState;
    use winwincode_storage::OccupancyReconcileTarget;
    let Some(lease) = ClientOccupancyService::new(storage)
        .active_lease_for_node(node_id)
        .map_err(|_| ClientExchangeError::unavailable())?
    else {
        return Ok(());
    };
    if lease.state != OccupancyLeaseState::RecoveryPending
        || payload.occupancy_lease_id.as_deref() != Some(lease.occupancy_lease_id.as_str())
    {
        return Ok(());
    }
    let Some((was_draining, recovery_started_at)) = storage
        .client_occupancy_ledger()
        .and_then(|mut ledger| ledger.recovery_context(&lease.occupancy_lease_id, now))
        .map_err(|_| ClientExchangeError::unavailable())?
    else {
        return Ok(());
    };
    if reported_at < recovery_started_at.0.as_str() || reported_at > now.0.as_str() {
        return Ok(());
    }
    let bindings = storage
        .device_execution_binding_ledger()
        .and_then(|ledger| ledger.active_for_node(node_id))
        .map_err(|_| ClientExchangeError::unavailable())?;
    let mut seen = std::collections::HashSet::new();
    for report in &payload.workers {
        if !seen.insert(&report.worker_session_id)
            || report.reconcile_state == WorkerReconcileState::Unknown
            || report.observed_at < recovery_started_at.0
            || report.observed_at > now.0
        {
            return Ok(());
        }
        if !worker_reconciliation_authority_matches(storage, node_id, &lease, report)? {
            return Ok(());
        }
    }
    if bindings.iter().any(|binding| {
        binding.occupancy_lease_id != lease.occupancy_lease_id
            || !seen.contains(&binding.worker_session_id)
    }) {
        return Ok(());
    }
    if payload.workers.iter().any(|report| {
        matches!(
            report.reconcile_state,
            WorkerReconcileState::Missing | WorkerReconcileState::Terminal
        ) && bindings
            .iter()
            .any(|binding| binding.worker_session_id == report.worker_session_id)
    }) {
        return Ok(());
    }
    let target = if !was_draining
        && payload
            .workers
            .iter()
            .any(|report| report.reconcile_state == WorkerReconcileState::StillRunning)
    {
        OccupancyReconcileTarget::ResumeOccupied
    } else {
        OccupancyReconcileTarget::ResumeDraining
    };
    ClientOccupancyService::new(storage)
        .reconcile_resume(&lease.occupancy_lease_id, target, None, now)
        .map_err(|_| ClientExchangeError::unavailable())?;
    Ok(())
}

fn worker_reconciliation_authority_matches(
    storage: &mut SqliteStorage,
    node_id: &str,
    lease: &winwincode_storage::OccupancyLeaseRecord,
    report: &winwincode_client_port::messages::ClientWorkerReconciliation,
) -> Result<bool, ClientExchangeError> {
    use winwincode_client_port::domain::WorkerReconcileState;
    let binding = winwincode_control_plane::DeviceExecutionBindingService::new(storage)
        .snapshot(&report.worker_session_id)
        .map_err(|_| ClientExchangeError::unavailable())?;
    let grant = if let Some(binding) = &binding {
        if binding.client_node_id != node_id
            || binding.occupancy_lease_id != lease.occupancy_lease_id
            || binding.occupancy_fencing_token != lease.fencing_token
        {
            return Ok(false);
        }
        WorkerLaunchGrantService::new(storage).snapshot(&binding.worker_launch_grant_id)
    } else {
        WorkerLaunchGrantService::new(storage).active_grant_for_session(&report.worker_session_id)
    }
    .map_err(|_| ClientExchangeError::unavailable())?;
    let Some(grant) = grant else {
        return Ok(false);
    };
    if grant.client_node_id != node_id
        || grant.worker_session_id != report.worker_session_id
        || grant.occupancy_lease_id != lease.occupancy_lease_id
        || grant.occupancy_fencing_token != lease.fencing_token
        || grant.holder_user_id != lease.holder_user_id
        || grant.worker_instance_id != report.worker_instance_id
    {
        return Ok(false);
    }
    let exited = storage
        .worker_launch_grant_ledger()
        .and_then(|ledger| ledger.has_trusted_exit(&grant.worker_launch_grant_id))
        .map_err(|_| ClientExchangeError::unavailable())?;
    if report.reconcile_state == WorkerReconcileState::StillRunning {
        let current = storage
            .client_node_registry()
            .and_then(|registry| registry.snapshot(node_id))
            .map_err(|_| ClientExchangeError::unavailable())?;
        if exited
            || grant.state != winwincode_storage::WorkerLaunchGrantState::Consumed
            || current.and_then(|node| node.current_instance_id).as_deref()
                != Some(grant.client_instance_id.as_str())
        {
            return Ok(false);
        }
    } else if !exited {
        return Ok(false);
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)]
fn settle_worker_exit(
    storage: &mut SqliteStorage,
    node_id: &str,
    payload: &winwincode_client_port::messages::ClientWorkerStatePayload,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    use winwincode_client_port::domain::ClientWorkerRunState;
    use winwincode_control_plane::DeviceExecutionBindingService;
    use winwincode_storage::{DeviceExecutionBindingRelease, DeviceExecutionBindingState};
    if !matches!(
        payload.state,
        ClientWorkerRunState::Stopped
            | ClientWorkerRunState::Crashed
            | ClientWorkerRunState::Missing
    ) {
        return Ok(());
    }
    let Some(occupancy) = payload.occupancy_lease_id.as_deref() else {
        return Ok(());
    };
    let closed = if let Some(closure) = &payload.process_closure {
        let closure = winwincode_storage::DeviceLaunchProcessClosure {
            worker_launch_grant_id: closure.worker_launch_grant_id.clone(),
            client_instance_id: closure.client_instance_id.clone(),
            reporting_client_instance_id: closure.reporting_client_instance_id.clone(),
            occupancy_fencing_token: closure.occupancy_fencing_token,
            never_started: closure.never_started,
            process_boot_digest: closure.process_boot_digest.clone(),
        };
        storage
            .worker_launch_grant_ledger()
            .map_err(|_| ClientExchangeError::unavailable())?
            .observe_device_process_closure(
                node_id,
                &payload.worker_session_id,
                &payload.worker_instance_id,
                occupancy,
                &closure,
                now,
            )
            .map_err(|_| ClientExchangeError::unavailable())?
    } else {
        WorkerLaunchGrantService::new(storage)
            .observe_trusted_exit(
                node_id,
                &payload.worker_session_id,
                &payload.worker_instance_id,
                occupancy,
                now,
            )
            .map_err(|_| ClientExchangeError::unavailable())?
    };
    if !closed {
        return Ok(());
    }
    let grant = if let Some(closure) = &payload.process_closure {
        WorkerLaunchGrantService::new(storage).snapshot(&closure.worker_launch_grant_id)
    } else {
        WorkerLaunchGrantService::new(storage).active_grant_for_session(&payload.worker_session_id)
    };
    if let Some(grant) = grant.map_err(|_| ClientExchangeError::unavailable())? {
        let mut credentials = crate::WorkerSessionCredentialService::new(storage);
        if credentials
            .status_for_session(&payload.worker_session_id)
            .map_err(|_| ClientExchangeError::unavailable())?
            .is_some_and(|record| {
                record.state == winwincode_storage::WorkerSessionCredentialState::Active
            })
        {
            credentials
                .revoke_for_session(
                    &payload.worker_session_id,
                    &grant.holder_user_id,
                    Some("trusted Device process exit"),
                    now,
                )
                .map_err(|_| ClientExchangeError::unavailable())?;
        }
    }
    let Some(binding) = DeviceExecutionBindingService::new(storage)
        .snapshot(&payload.worker_session_id)
        .map_err(|_| ClientExchangeError::unavailable())?
    else {
        return Ok(());
    };
    let Some(grant) = WorkerLaunchGrantService::new(storage)
        .snapshot(&binding.worker_launch_grant_id)
        .map_err(|_| ClientExchangeError::unavailable())?
    else {
        return Ok(());
    };
    if binding.state == DeviceExecutionBindingState::Released
        || binding.client_node_id != node_id
        || payload.occupancy_lease_id.as_deref() != Some(binding.occupancy_lease_id.as_str())
        || payload.worker_instance_id != grant.worker_instance_id
    {
        return Ok(());
    }
    let release = DeviceExecutionBindingRelease::try_new(
        &binding.worker_session_id,
        binding.device_execution_binding_id.replace("deb_", "req_"),
        binding.revision,
        now.clone(),
    )
    .map_err(|_| ClientExchangeError::unavailable())?;
    DeviceExecutionBindingService::new(storage)
        .release(&release, now)
        .map_err(|_| ClientExchangeError::unavailable())?;
    Ok(())
}

const fn repository_dirty_state(value: WireRepositoryDirtyState) -> RepositoryDirtyState {
    match value {
        WireRepositoryDirtyState::Clean => RepositoryDirtyState::Clean,
        WireRepositoryDirtyState::Dirty => RepositoryDirtyState::Dirty,
    }
}

fn observe_command_ack_revision(
    data_directory: &Path,
    node_id: &str,
    payload: &winwincode_client_port::messages::ClientCommandAckPayload,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    if matches!(
        payload.command_kind,
        ClientControlMessageKind::OccupancyRelease | ClientControlMessageKind::OccupancyForceFence
    ) && let Some(revision) = payload.current_revision
    {
        crate::client_occupancy::observe_client_mirror_revision(
            data_directory,
            node_id,
            revision,
            now,
        )
        .map_err(|_| ClientExchangeError::unavailable())?;
    }
    Ok(())
}

fn settle_worker_launch_ack(
    storage: &mut SqliteStorage,
    data_directory: &Path,
    node_id: &str,
    payload: &winwincode_client_port::messages::ClientWorkerLaunchAckPayload,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    let grant = WorkerLaunchGrantService::new(storage)
        .snapshot(&payload.worker_launch_grant_id)
        .map_err(|_| ClientExchangeError::unavailable())?
        .ok_or_else(|| rejected_command(None))?;
    if grant.client_node_id != node_id {
        return Err(rejected_command(None));
    }
    crate::client_occupancy::observe_client_mirror_revision(
        data_directory,
        node_id,
        payload.occupancy.command.expected_revision,
        now,
    )
    .map_err(|_| ClientExchangeError::unavailable())?;
    let accepted = matches!(
        payload.status,
        WorkerLaunchAckStatus::Accepted | WorkerLaunchAckStatus::Duplicate
    );
    let Ok(settlement) = LaunchAckSettlement::try_new(
        &payload.worker_launch_grant_id,
        &payload.occupancy.occupancy_lease_id,
        payload.occupancy.occupancy_fencing_token,
        &payload.worker_session_id,
        &payload.worker_id,
        &payload.worker_instance_id,
        accepted,
        (!accepted).then(|| launch_rejection_reason(payload.status, payload.error.as_ref())),
    ) else {
        return Err(rejected_command(None));
    };
    WorkerLaunchGrantService::new(storage)
        .settle_device_launch_ack(node_id, &settlement, now)
        .map_err(|error| {
            domain_command_error(matches!(
                error.kind(),
                winwincode_control_plane::WorkerLaunchGrantServiceErrorKind::Storage
                    | winwincode_control_plane::WorkerLaunchGrantServiceErrorKind::CorruptState
            ))
        })?;
    if !accepted
        && let Some(closure) = &payload.process_closure
        && closure.worker_launch_grant_id == payload.worker_launch_grant_id
    {
        settle_worker_exit(
            storage,
            node_id,
            &winwincode_client_port::messages::ClientWorkerStatePayload {
                occupancy_lease_id: Some(payload.occupancy.occupancy_lease_id.clone()),
                worker_session_id: payload.worker_session_id.clone(),
                worker_instance_id: payload.worker_instance_id.clone(),
                state: winwincode_client_port::domain::ClientWorkerRunState::Stopped,
                exit_code: None,
                observed_at: now.0.clone(),
                process_closure: Some(closure.clone()),
            },
            now,
        )?;
    }
    Ok(())
}

fn launch_rejection_reason(
    status: WorkerLaunchAckStatus,
    error: Option<&winwincode_client_port::domain::ClientControlError>,
) -> String {
    let status_text = serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "rejected".to_owned());
    match error {
        Some(error) => serde_json::to_value(error.code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .map(|code| format!("{status_text}:{code}"))
            .unwrap_or(status_text),
        None => status_text,
    }
}

fn settle_candidate_retained(
    storage: &mut SqliteStorage,
    node_id: &str,
    payload: &ClientCandidateRetainedPayload,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    let receipt = &payload.receipt;
    let Ok(command) = LocalCandidateRetained::try_new(
        receipt.local_candidate_receipt_id.clone(),
        node_id,
        receipt.repository_binding_id.clone(),
        receipt.candidate_ref.clone(),
        receipt.candidate_commit.clone(),
        receipt.local_ref_name.clone(),
    ) else {
        return Err(rejected_command(None));
    };
    let mut candidates = LocalCandidateService::new(storage);
    candidates.record_retained(&command, now).map_err(|error| {
        domain_command_error(matches!(
            error.kind(),
            winwincode_control_plane::LocalCandidateServiceErrorKind::Storage
                | winwincode_control_plane::LocalCandidateServiceErrorKind::CorruptState
        ))
    })?;
    Ok(())
}

fn settle_candidate_apply_result(
    storage: &mut SqliteStorage,
    node_id: &str,
    payload: &ClientCandidateApplyResultPayload,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    let fenced = {
        let mut occupancy = ClientOccupancyService::new(storage);
        occupancy
            .active_lease_for_node(node_id)
            .map_err(|_| ClientExchangeError::unavailable())?
            .is_some_and(|lease| {
                lease.occupancy_lease_id == payload.occupancy.occupancy_lease_id
                    && lease.fencing_token == payload.occupancy.occupancy_fencing_token
            })
    };
    if !fenced {
        return Err(rejected_command(None));
    }
    let receipt = &payload.receipt;
    let mut candidates = LocalCandidateService::new(storage);
    let Some(candidate) = candidates
        .candidate_for_ref(node_id, &receipt.candidate_ref)
        .map_err(|_| ClientExchangeError::unavailable())?
    else {
        return Err(rejected_command(None));
    };
    let Ok(settlement) = LocalApplySettlement::try_new(
        receipt.local_apply_receipt_id.clone(),
        candidate.local_candidate_receipt_id.clone(),
        node_id,
        receipt.repository_binding_id.clone(),
        receipt.candidate_ref.clone(),
        receipt.target_branch.clone(),
        receipt.expected_head.clone(),
        storage_strategy(receipt.strategy),
        storage_result(receipt.result),
        receipt.resulting_commit.clone(),
        receipt.conflict_artifact_ref.clone(),
    ) else {
        return Err(rejected_command(None));
    };
    candidates
        .record_apply_result(&settlement, now)
        .map_err(|error| {
            domain_command_error(matches!(
                error.kind(),
                winwincode_control_plane::LocalCandidateServiceErrorKind::Storage
                    | winwincode_control_plane::LocalCandidateServiceErrorKind::CorruptState
            ))
        })?;
    Ok(())
}

const fn storage_strategy(strategy: ApplyStrategy) -> LocalApplyStrategy {
    match strategy {
        ApplyStrategy::CreateBranch => LocalApplyStrategy::CreateBranch,
        ApplyStrategy::FastForward => LocalApplyStrategy::FastForward,
        ApplyStrategy::CherryPick => LocalApplyStrategy::CherryPick,
        ApplyStrategy::Merge => LocalApplyStrategy::Merge,
    }
}

const fn storage_result(result: ApplyResult) -> LocalApplyResult {
    match result {
        ApplyResult::Retained => LocalApplyResult::Retained,
        ApplyResult::BranchCreated => LocalApplyResult::BranchCreated,
        ApplyResult::Applied => LocalApplyResult::Applied,
        ApplyResult::BaseStale => LocalApplyResult::BaseStale,
        ApplyResult::WorkingTreeDirty => LocalApplyResult::WorkingTreeDirty,
        ApplyResult::MergeConflict => LocalApplyResult::MergeConflict,
        ApplyResult::CandidateMissing => LocalApplyResult::CandidateMissing,
        ApplyResult::PermissionDenied => LocalApplyResult::PermissionDenied,
        ApplyResult::Discarded => LocalApplyResult::Discarded,
        ApplyResult::Failed => LocalApplyResult::Failed,
    }
}

fn supersede_instance(
    storage: &mut SqliteStorage,
    record: &ClientNodeRecord,
    envelope: &ClientToServerEnvelope,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    let ClientToServerMessage::Hello(payload) = &envelope.message else {
        return Ok(());
    };
    let mut registry = ClientRegistryService::new(storage);
    let current = registry
        .snapshot(&record.client_node_id)
        .map_err(|_| ClientExchangeError::unavailable())?;
    let Some(current) = current else {
        return Ok(());
    };
    if current.client_version == payload.client_version
        && current.current_instance_id.as_deref() == Some(envelope.client_instance_id.as_str())
        && current.max_concurrent_worker_sessions == payload.capacity.max_concurrent_worker_sessions
    {
        return Ok(());
    }
    let Ok(registration) = ClientNodeRegistration::try_new(
        current.client_node_id.clone(),
        current.public_client_id.clone(),
        current.display_name.clone(),
        current.platform.clone(),
        current.architecture.clone(),
        payload.client_version.clone(),
        current.device_credential_digest.clone(),
        Some(envelope.client_instance_id.clone()),
        payload.capacity.max_concurrent_worker_sessions,
    ) else {
        return Ok(());
    };
    registry
        .register(&registration, current.revision, now)
        .map_err(|_| ClientExchangeError::unavailable())?;
    Ok(())
}

const fn presence_target(state: PresenceState) -> Option<ClientPresenceState> {
    match state {
        PresenceState::Online => Some(ClientPresenceState::Online),
        PresenceState::Degraded => Some(ClientPresenceState::Degraded),
        PresenceState::Offline => Some(ClientPresenceState::Offline),
        PresenceState::Locked => Some(ClientPresenceState::Locked),
        PresenceState::PendingEnrollment | PresenceState::Revoked => None,
    }
}

fn issue_credential() -> Result<(String, String), ClientExchangeError> {
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).map_err(|_| ClientExchangeError::unavailable())?;
    Ok((hex_encode(&secret), credential_digest(&secret)))
}

fn credential_digest(secret: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(secret))
}

fn credential_digest_matches(secret: &[u8; 32], stored_digest: &str) -> bool {
    constant_time_eq(
        credential_digest(secret).as_bytes(),
        stored_digest.as_bytes(),
    )
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left_byte, right_byte) in left.iter().zip(right.iter()) {
        difference |= left_byte ^ right_byte;
    }
    difference == 0
}

fn decode_credential_secret(raw: &str) -> Option<[u8; 32]> {
    let bytes = raw.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut secret = [0_u8; 32];
    for (index, slot) in secret.iter_mut().enumerate() {
        let high = hex_nibble(bytes[index * 2])?;
        let low = hex_nibble(bytes[index * 2 + 1])?;
        *slot = high << 4 | low;
    }
    Some(secret)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn generate_prefixed_id(prefix: &str) -> Result<String, ClientExchangeError> {
    let mut random = [0_u8; 13];
    getrandom::fill(&mut random).map_err(|_| ClientExchangeError::unavailable())?;
    let mut identity = String::with_capacity(prefix.len() + 26);
    identity.push_str(prefix);
    for byte in random {
        identity.push(IDENTITY_ALPHABET[usize::from(byte >> 4)] as char);
        identity.push(IDENTITY_ALPHABET[usize::from(byte & 0x0f)] as char);
    }
    Ok(identity)
}

fn generate_public_client_id() -> Result<String, ClientExchangeError> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|_| ClientExchangeError::unavailable())?;
    let value = u64::from_be_bytes(random) % 10_000_000_000;
    Ok(format!("{value:0>10}"))
}

pub(crate) fn is_canonical_client_node_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("cnd_") else {
        return false;
    };
    suffix.len() == 26
        && suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                )
        })
}

const fn platform_name(platform: ClientPlatformTarget) -> &'static str {
    match platform {
        ClientPlatformTarget::Aarch64AppleDarwin => "aarch64-apple-darwin",
        ClientPlatformTarget::X8664AppleDarwin => "x86_64-apple-darwin",
        ClientPlatformTarget::Aarch64UnknownLinuxGnu => "aarch64-unknown-linux-gnu",
        ClientPlatformTarget::X8664UnknownLinuxGnu => "x86_64-unknown-linux-gnu",
    }
}

const fn architecture_name(architecture: ClientArchitecture) -> &'static str {
    match architecture {
        ClientArchitecture::Aarch64 => "aarch64",
        ClientArchitecture::X8664 => "x86_64",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use winwincode_client_port::messages::ClientHeartbeatPayload;
    use winwincode_client_port::messages::ClientHelloPayload;

    use super::*;

    #[test]
    fn constant_time_eq_matches_only_equal_slices() {
        assert!(constant_time_eq(b"sha256:aa", b"sha256:aa"));
        assert!(!constant_time_eq(b"sha256:aa", b"sha256:ab"));
        assert!(!constant_time_eq(b"sha256:aa", b"sha256:aaa"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn credential_hex_decodes_both_cases_and_rejects_bad_shapes() {
        let secret = [0xab_u8; 32];
        let lower = hex_encode(&secret);
        assert_eq!(lower.len(), 64);
        assert_eq!(decode_credential_secret(&lower), Some(secret));
        assert_eq!(
            decode_credential_secret(&lower.to_uppercase()),
            Some(secret)
        );
        assert_eq!(decode_credential_secret(&lower[..63]), None);
        let mut corrupted = lower.clone();
        corrupted.replace_range(0..1, "g");
        assert_eq!(decode_credential_secret(&corrupted), None);
        assert_eq!(decode_credential_secret(""), None);
    }

    #[test]
    fn issued_credential_digest_matches_its_secret() {
        let (material, digest) = issue_credential().expect("entropy");
        let secret = decode_credential_secret(&material).expect("hex material");
        assert!(digest.starts_with("sha256:"));
        assert!(credential_digest_matches(&secret, &digest));
        let mut other = secret;
        other[0] ^= 1;
        assert!(!credential_digest_matches(&other, &digest));
    }

    #[test]
    fn worker_credential_is_delivered_only_with_its_launch_frame() {
        let directory = std::env::temp_dir().join(format!(
            "worker-launch-credential-{}",
            generate_prefixed_id("wlg_").unwrap()
        ));
        let application =
            ClientExchangeApplication::open(&directory, &ClientExchangeConfig::default())
                .expect("valid application");
        let secret = [0x42_u8; 32];
        let material = hex_encode(&secret);
        let digest = credential_digest(&secret);
        application
            .publish_worker_credential(WorkerCredentialDelivery {
                client_node_id: "cnd_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                worker_launch_grant_id: "wlg_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                worker_session_id: "wsn_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                credential_digest: digest.clone(),
                worker_credential: material.clone(),
                expires_at: Instant("2100-01-01T00:00:00.000Z".to_owned()),
            })
            .expect("publish credential");
        let launch = serde_json::json!({
            "kind": "client.worker.launch",
            "payload": {"launchGrant": {
                "workerLaunchGrantId": "wlg_AAAAAAAAAAAAAAAAAAAAAAAAAA",
                "workerSessionId": "wsn_AAAAAAAAAAAAAAAAAAAAAAAAAA",
                "credentialDigest": digest,
            }}
        });
        assert!(
            application
                .worker_credentials_for_frames(
                    "cnd_AAAAAAAAAAAAAAAAAAAAAAAAAA",
                    &[],
                    &Instant("2026-09-12T00:00:00.000Z".to_owned()),
                )
                .expect("unrelated response")
                .is_empty()
        );
        let delivered = application
            .worker_credentials_for_frames(
                "cnd_AAAAAAAAAAAAAAAAAAAAAAAAAA",
                &[launch],
                &Instant("2026-09-12T00:00:00.000Z".to_owned()),
            )
            .expect("matching response");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].worker_credential, material);
        drop(application);
        let reopened =
            ClientExchangeApplication::open(&directory, &ClientExchangeConfig::default()).unwrap();
        let again=reopened.worker_credentials_for_frames("cnd_AAAAAAAAAAAAAAAAAAAAAAAAAA",&[serde_json::json!({"kind":"client.worker.launch","payload":{"launchGrant":{"workerLaunchGrantId":"wlg_AAAAAAAAAAAAAAAAAAAAAAAAAA","workerSessionId":"wsn_AAAAAAAAAAAAAAAAAAAAAAAAAA","credentialDigest":digest}}})],&Instant("2026-09-12T00:00:00.000Z".into())).unwrap();
        assert_eq!(again[0].worker_credential, material);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn generated_ids_are_canonical_crockford_or_digits() {
        let node_id = generate_prefixed_id("cnd_").expect("entropy");
        assert_eq!(node_id.len(), 30);
        assert!(node_id.starts_with("cnd_"));
        assert!(
            node_id[4..]
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        );
        let public_client_id = generate_public_client_id().expect("entropy");
        assert_eq!(public_client_id.len(), 10);
        assert!(public_client_id.bytes().all(|byte| byte.is_ascii_digit()));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn command_identities_cover_exactly_the_client_command_kinds() {
        let command = winwincode_client_port::messages::CommandContext {
            expected_revision: 1,
            idempotency_key: "idem_1".to_owned(),
        };
        let occupancy = winwincode_client_port::messages::OccupancyCommandContext {
            command: command.clone(),
            occupancy_lease_id: "lease_1".to_owned(),
            occupancy_fencing_token: 1,
        };
        let commands: Vec<ClientToServerMessage> = vec![
            ClientToServerMessage::Enroll(Box::new(ClientEnrollPayload {
                command: command.clone(),
                display_name: "device".to_owned(),
                platform: ClientPlatformTarget::Aarch64AppleDarwin,
                architecture: ClientArchitecture::Aarch64,
                client_version: "0.1.0".to_owned(),
            })),
            ClientToServerMessage::ConnectCodePublished(
                winwincode_client_port::messages::ClientConnectCodePublishedPayload {
                    command: command.clone(),
                    connect_code_id: "code_1".to_owned(),
                    code_digest: "sha256:aa".to_owned(),
                    generation: 1,
                    expires_at: "2026-01-02T12:00:00.000Z".to_owned(),
                    remaining_attempts: 5,
                },
            ),
            ClientToServerMessage::RepositoryUpsert(
                winwincode_client_port::messages::ClientRepositoryUpsertPayload {
                    command: command.clone(),
                    repository: winwincode_client_port::domain::RepositoryBindingProjection {
                        repository_binding_id: "rb_1".to_owned(),
                        display_name: "repo".to_owned(),
                        repository_kind: winwincode_client_port::domain::RepositoryKind::Git,
                        default_branch: "main".to_owned(),
                        head_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                        dirty_state: winwincode_client_port::domain::RepositoryDirtyState::Clean,
                        availability:
                            winwincode_client_port::domain::RepositoryAvailability::Available,
                        repository_fingerprint: "sha256:cc".to_owned(),
                        last_scanned_at: "2026-01-02T12:00:00.000Z".to_owned(),
                    },
                },
            ),
            ClientToServerMessage::RepositoryRemoved(
                winwincode_client_port::messages::ClientRepositoryRemovedPayload {
                    command: command.clone(),
                    repository_binding_id: "rb_1".to_owned(),
                },
            ),
            ClientToServerMessage::OccupancyAck(
                winwincode_client_port::messages::ClientOccupancyAckPayload {
                    occupancy: occupancy.clone(),
                },
            ),
            ClientToServerMessage::OccupancyRejected(
                winwincode_client_port::messages::ClientOccupancyRejectedPayload {
                    occupancy: occupancy.clone(),
                    reason:
                        winwincode_client_port::domain::OccupancyRejectReason::CapacityExhausted,
                },
            ),
            ClientToServerMessage::WorkerLaunchAck(Box::new(
                winwincode_client_port::messages::ClientWorkerLaunchAckPayload {
                    process_closure: None,
                    occupancy: occupancy.clone(),
                    worker_launch_grant_id: "wlg_1".to_owned(),
                    worker_session_id: "wsn_1".to_owned(),
                    worker_id: "worker_1".to_owned(),
                    worker_instance_id: "wki_1".to_owned(),
                    status: WorkerLaunchAckStatus::Accepted,
                    error: None,
                },
            )),
            ClientToServerMessage::CandidateRetained(ClientCandidateRetainedPayload {
                occupancy: occupancy.clone(),
                worker_session_id: "wsn_1".to_owned(),
                receipt: winwincode_client_port::domain::LocalCandidateReceipt {
                    local_candidate_receipt_id: "lcr_1".to_owned(),
                    candidate_ref: "cand_1".to_owned(),
                    repository_binding_id: "rb_1".to_owned(),
                    candidate_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    local_ref_name: "refs/winwincode/candidates/cand_1".to_owned(),
                    state: winwincode_client_port::domain::LocalCandidateState::Retained,
                    created_at: "2026-01-02T12:00:00.000Z".to_owned(),
                    revision: 1,
                },
            }),
            ClientToServerMessage::CandidateApplyResult(ClientCandidateApplyResultPayload {
                occupancy,
                receipt: winwincode_client_port::domain::LocalApplyReceipt {
                    local_apply_receipt_id: "lar_1".to_owned(),
                    candidate_ref: "cand_1".to_owned(),
                    repository_binding_id: "rb_1".to_owned(),
                    target_branch: "main".to_owned(),
                    expected_head: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    strategy: ApplyStrategy::CherryPick,
                    result: ApplyResult::Applied,
                    resulting_commit: None,
                    conflict_artifact_ref: None,
                    created_at: "2026-01-02T12:00:00.000Z".to_owned(),
                    revision: 1,
                },
            }),
        ];
        assert_eq!(commands.len(), 9, "the client command kinds");
        for message in &commands {
            let identity = command_identity(message, "sha256:dd");
            assert_eq!(
                identity.expect("command identity").idempotency_key,
                "idem_1"
            );
        }

        let reports: Vec<ClientToServerMessage> = vec![
            ClientToServerMessage::Hello(ClientHelloPayload {
                client_version: "0.1.0".to_owned(),
                capacity: winwincode_client_port::domain::ClientCapacityReport {
                    max_concurrent_worker_sessions: 1,
                    running_worker_sessions: 0,
                    reserved_worker_sessions: 0,
                    draining_worker_sessions: 0,
                },
                accepting_connections: true,
                lock_state: winwincode_client_port::domain::ClientLockState::Unlocked,
                presence_state: PresenceState::Online,
            }),
            ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
                capacity: winwincode_client_port::domain::ClientCapacityReport {
                    max_concurrent_worker_sessions: 1,
                    running_worker_sessions: 0,
                    reserved_worker_sessions: 0,
                    draining_worker_sessions: 0,
                },
                accepting_connections: true,
                lock_state: winwincode_client_port::domain::ClientLockState::Unlocked,
                presence_state: PresenceState::Online,
                occupancy_lease_id: None,
            }),
            ClientToServerMessage::RepositoryStatus(
                winwincode_client_port::messages::ClientRepositoryStatusPayload {
                    repository_binding_id: "rb_1".to_owned(),
                    availability: winwincode_client_port::domain::RepositoryAvailability::Available,
                    head_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    dirty_state: winwincode_client_port::domain::RepositoryDirtyState::Clean,
                    last_scanned_at: "2026-01-02T12:00:00.000Z".to_owned(),
                },
            ),
            ClientToServerMessage::WorkerState(
                winwincode_client_port::messages::ClientWorkerStatePayload {
                    process_closure: None,
                    occupancy_lease_id: None,
                    worker_session_id: "wsn_1".to_owned(),
                    worker_instance_id: "wki_1".to_owned(),
                    state: winwincode_client_port::domain::ClientWorkerRunState::Running,
                    exit_code: None,
                    observed_at: "2026-01-02T12:00:00.000Z".to_owned(),
                },
            ),
            ClientToServerMessage::WorkerReconcile(
                winwincode_client_port::messages::ClientWorkerReconcilePayload {
                    occupancy_lease_id: None,
                    workers: Vec::new(),
                },
            ),
            ClientToServerMessage::CommandAck(
                winwincode_client_port::messages::ClientCommandAckPayload {
                    command_kind: ClientControlMessageKind::ClientLock,
                    command_message_id: "msg_1".to_owned(),
                    status: winwincode_client_port::domain::CommandAckStatus::Accepted,
                    current_revision: Some(2),
                    error: None,
                },
            ),
        ];
        assert_eq!(reports.len(), 6, "the client report kinds");
        for message in &reports {
            assert!(command_identity(message, "sha256:dd").is_none());
        }
    }

    #[test]
    fn presence_targets_exclude_client_unreportable_states() {
        assert_eq!(
            presence_target(PresenceState::Online),
            Some(ClientPresenceState::Online)
        );
        assert_eq!(
            presence_target(PresenceState::Degraded),
            Some(ClientPresenceState::Degraded)
        );
        assert_eq!(
            presence_target(PresenceState::Offline),
            Some(ClientPresenceState::Offline)
        );
        assert_eq!(
            presence_target(PresenceState::Locked),
            Some(ClientPresenceState::Locked)
        );
        assert_eq!(presence_target(PresenceState::PendingEnrollment), None);
        assert_eq!(presence_target(PresenceState::Revoked), None);
    }

    #[test]
    fn hello_projects_capacity_for_the_enrolled_instance() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-client-exchange-hello-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let mut storage = SqliteStorage::open(&root).expect("storage opens");
        let node = "cnd_00000000000000000000000001";
        let instance = "cix_00000000000000000000000001";
        let now = Instant("2026-09-12T00:00:00.000Z".to_owned());
        let registration = ClientNodeRegistration::try_new(
            node,
            "0000000001",
            "Fixture Device",
            "aarch64-apple-darwin",
            "aarch64",
            "0.1.0",
            None,
            Some(instance.to_owned()),
            0,
        )
        .expect("registration");
        ClientRegistryService::new(&mut storage)
            .register(&registration, 0, &now)
            .expect("register client");
        let hello = ClientToServerEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: "msg_00000000000000000000000001".to_owned(),
            client_node_id: node.to_owned(),
            client_instance_id: instance.to_owned(),
            sequence: 1,
            occurred_at: now.0.clone(),
            message: ClientToServerMessage::Hello(ClientHelloPayload {
                client_version: "0.1.0".to_owned(),
                capacity: winwincode_client_port::domain::ClientCapacityReport {
                    max_concurrent_worker_sessions: 1,
                    running_worker_sessions: 0,
                    reserved_worker_sessions: 0,
                    draining_worker_sessions: 0,
                },
                accepting_connections: true,
                lock_state: winwincode_client_port::domain::ClientLockState::Unlocked,
                presence_state: PresenceState::Online,
            }),
        };

        apply_effect(&mut storage, &root, node, &hello, &now).expect("hello effect");

        let record = ClientRegistryService::new(&mut storage)
            .snapshot(node)
            .expect("snapshot")
            .expect("client");
        assert_eq!(record.max_concurrent_worker_sessions, 1);
        assert_eq!(record.presence_state, ClientPresenceState::Online);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn config_bounds_are_validated() {
        assert!(ClientExchangeConfig::try_new(0, 1, 1_000).is_err());
        assert!(ClientExchangeConfig::try_new(1024, 0, 1_000).is_err());
        let config = ClientExchangeConfig::try_new(1024, 8, 1_000).expect("valid bounds");
        assert_eq!(config.max_frame_bytes, 1024);
        assert_eq!(config.max_frames_per_exchange, 8);
        assert_eq!(
            ClientExchangeConfig::default().max_frame_bytes,
            DEFAULT_MAX_FRAME_BYTES
        );
    }

    #[test]
    fn malformed_request_bodies_are_invalid_requests() {
        let directory = std::env::temp_dir().join(format!(
            "worker-launch-credential-{}",
            generate_prefixed_id("wlg_").unwrap()
        ));
        let application =
            ClientExchangeApplication::open(&directory, &ClientExchangeConfig::default())
                .expect("valid application");
        let now = Instant("2026-01-02T12:00:00.000Z".to_owned());
        for body in ["", "not json", "{}", r#"{"frames":[]}"#] {
            let error = application
                .apply(None, body.as_bytes(), &now)
                .expect_err("rejected body");
            assert!(error.is_invalid_request(), "{error}");
        }
    }

    #[test]
    fn enroll_rejects_non_enroll_batches_without_a_credential() {
        let directory = std::env::temp_dir().join(format!(
            "worker-launch-credential-{}",
            generate_prefixed_id("wlg_").unwrap()
        ));
        let application =
            ClientExchangeApplication::open(&directory, &ClientExchangeConfig::default())
                .expect("valid application");
        let now = Instant("2026-01-02T12:00:00.000Z".to_owned());
        let body = serde_json::json!({
            "schemaVersion": CLIENT_CONTROL_PORT_SCHEMA_VERSION,
            "frames": [{
                "schemaVersion": CLIENT_CONTROL_PORT_SCHEMA_VERSION,
                "messageId": "msg_1",
                "clientNodeId": "cnd_AAAAAAAAAAAAAAAAAAAAAAAA1",
                "clientInstanceId": "cix_AAAAAAAAAAAAAAAAAAAAAAA1",
                "sequence": 1,
                "occurredAt": "2026-01-02T12:00:00.000Z",
                "kind": "client.hello",
                "payload": {
                    "clientVersion": "0.1.0",
                    "capacity": {
                        "maxConcurrentWorkerSessions": 1,
                        "runningWorkerSessions": 0,
                        "reservedWorkerSessions": 0,
                        "drainingWorkerSessions": 0
                    },
                    "acceptingConnections": true,
                    "lockState": "unlocked",
                    "presenceState": "online"
                }
            }],
            "ackSequence": 0
        })
        .to_string();
        let error = application
            .apply(None, body.as_bytes(), &now)
            .expect_err("heartbeat without a credential");
        assert!(error.is_authentication(), "{error}");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the repository report test keeps replay and owner-fencing checks together"
    )]
    fn repository_reports_are_idempotent_and_owner_fenced() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-client-exchange-repository-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let mut storage = SqliteStorage::open(&root).expect("storage opens");
        let node = "cnd_00000000000000000000000001";
        let foreign_node = "cnd_00000000000000000000000002";
        let instance = "cix_00000000000000000000000001";
        let binding_id = "rbd_00000000000000000000000001";
        let first = Instant("2026-09-07T00:00:00.000Z".to_owned());
        let second = Instant("2026-09-07T00:01:00.000Z".to_owned());
        let registration = ClientNodeRegistration::try_new(
            node,
            "0000000001",
            "Fixture Device",
            "aarch64-apple-darwin",
            "aarch64",
            "1.0.0",
            None,
            Some(instance.to_owned()),
            1,
        )
        .expect("registration");
        ClientRegistryService::new(&mut storage)
            .register(&registration, 0, &first)
            .expect("register client");
        let user_id = "usr_00000000000000000000000001";
        let client_grant = winwincode_storage::AccessGrantIssuance::try_new(
            "cag_00000000000000000000000001",
            node,
            user_id,
            user_id,
            winwincode_storage::GrantTrustMode::Trusted,
            None,
        )
        .expect("client grant issuance");
        AccessGrantService::new(&mut storage)
            .create_grant(
                &client_grant,
                winwincode_storage::GrantSource::Administrator,
                winwincode_storage::GrantPermissions::USE_MANAGE_SHARE,
                &first,
            )
            .expect("client manager grant");

        let envelope = |sender: &str, message_id: &str, message| ClientToServerEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: message_id.to_owned(),
            client_node_id: sender.to_owned(),
            client_instance_id: instance.to_owned(),
            sequence: 1,
            occurred_at: first.0.clone(),
            message,
        };
        let upsert = envelope(
            node,
            "msg_repository_upsert",
            ClientToServerMessage::RepositoryUpsert(
                winwincode_client_port::messages::ClientRepositoryUpsertPayload {
                    command: winwincode_client_port::messages::CommandContext {
                        expected_revision: 0,
                        idempotency_key: "repository-upsert".to_owned(),
                    },
                    repository: winwincode_client_port::domain::RepositoryBindingProjection {
                        repository_binding_id: binding_id.to_owned(),
                        display_name: "Fixture Repository".to_owned(),
                        repository_kind: winwincode_client_port::domain::RepositoryKind::Git,
                        default_branch: "main".to_owned(),
                        head_commit: "1111111111111111111111111111111111111111".to_owned(),
                        dirty_state: WireRepositoryDirtyState::Clean,
                        availability: WireRepositoryAvailability::Available,
                        repository_fingerprint: format!("sha256:{}", "a".repeat(64)),
                        last_scanned_at: first.0.clone(),
                    },
                },
            ),
        );
        apply_effect(&mut storage, &root, node, &upsert, &first).expect("upsert effect");
        apply_effect(&mut storage, &root, node, &upsert, &first).expect("upsert replay");
        let binding = RepositoryBindingService::new(&mut storage)
            .snapshot(binding_id)
            .expect("snapshot")
            .expect("binding exists");
        assert_eq!(binding.revision, 1);
        let grants = RepositoryAccessGrantService::new(&mut storage)
            .active_grants_for_binding(binding_id)
            .expect("repository grants");
        assert_eq!(grants.len(), 1, "upsert replay keeps one bootstrap grant");
        assert_eq!(grants[0].user_id, user_id);
        assert_eq!(grants[0].permissions, RepositoryGrantPermissions::UseManage);

        let status = envelope(
            node,
            "msg_repository_status",
            ClientToServerMessage::RepositoryStatus(
                winwincode_client_port::messages::ClientRepositoryStatusPayload {
                    repository_binding_id: binding_id.to_owned(),
                    availability: WireRepositoryAvailability::Dirty,
                    head_commit: binding.head_commit.clone().expect("head"),
                    dirty_state: WireRepositoryDirtyState::Dirty,
                    last_scanned_at: second.0.clone(),
                },
            ),
        );
        apply_effect(&mut storage, &root, node, &status, &second).expect("status effect");
        apply_effect(&mut storage, &root, node, &status, &second).expect("status replay");
        let binding = RepositoryBindingService::new(&mut storage)
            .snapshot(binding_id)
            .expect("snapshot")
            .expect("binding exists");
        assert_eq!(binding.revision, 2);
        assert_eq!(binding.dirty_state, RepositoryDirtyState::Dirty);

        let removed = envelope(
            foreign_node,
            "msg_repository_removed_foreign",
            ClientToServerMessage::RepositoryRemoved(
                winwincode_client_port::messages::ClientRepositoryRemovedPayload {
                    command: winwincode_client_port::messages::CommandContext {
                        expected_revision: 2,
                        idempotency_key: "repository-remove".to_owned(),
                    },
                    repository_binding_id: binding_id.to_owned(),
                },
            ),
        );
        apply_effect(&mut storage, &root, foreign_node, &removed, &second)
            .expect_err("foreign remove must return a rejection");
        assert!(
            RepositoryBindingService::new(&mut storage)
                .snapshot(binding_id)
                .expect("snapshot")
                .is_some()
        );

        let removed = envelope(node, "msg_repository_removed", removed.message);
        apply_effect(&mut storage, &root, node, &removed, &second).expect("remove effect");
        apply_effect(&mut storage, &root, node, &removed, &second).expect("remove replay");
        assert!(
            RepositoryBindingService::new(&mut storage)
                .snapshot(binding_id)
                .expect("snapshot")
                .is_none()
        );
        Box::new(storage).close().expect("close storage");
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    #[allow(clippy::too_many_lines)]
    fn recovery_reconciles_closed_source_anchors_without_job_bindings() {
        use winwincode_client_port::domain::WorkerReconcileState;
        use winwincode_client_port::messages::{
            ClientWorkerReconcilePayload, ClientWorkerReconciliation,
        };
        use winwincode_storage::{
            AccessGrantIssuance, GrantPermissions, GrantSource, GrantTrustMode,
            LaunchGrantIssuance, OccupancyClaim, RepositoryAccessGrantIssuance,
            RepositoryBindingProjection,
        };
        let root = std::env::temp_dir().join(format!(
            "reconcile-source-anchor-{}",
            generate_prefixed_id("wlg_").unwrap()
        ));
        let mut storage = SqliteStorage::open(&root).unwrap();
        let id = |prefix: &str| format!("{prefix}_00000000000000000000000001");
        let (node, instance, user, binding, occupancy) =
            (id("cnd"), id("cix"), id("usr"), id("rbd"), id("ocl"));
        let now = Instant("2027-01-15T08:00:00.000Z".into());
        let later = Instant("2027-01-15T08:01:00.000Z".into());
        storage
            .client_node_registry()
            .unwrap()
            .register(
                &ClientNodeRegistration::try_new(
                    &node,
                    "0000000001",
                    "recovery fixture",
                    "aarch64-apple-darwin",
                    "aarch64",
                    "1.0.0",
                    None,
                    Some(instance.clone()),
                    4,
                )
                .unwrap(),
                0,
                &now,
            )
            .unwrap();
        storage
            .client_node_registry()
            .unwrap()
            .update_presence(&node, ClientPresenceState::Online, 1)
            .unwrap();
        storage
            .client_connect_ledger()
            .unwrap()
            .create_grant(
                &AccessGrantIssuance::try_new(
                    id("cag"),
                    &node,
                    &user,
                    &user,
                    GrantTrustMode::Trusted,
                    None,
                )
                .unwrap(),
                GrantSource::Administrator,
                GrantPermissions::USE,
                &now,
            )
            .unwrap();
        storage
            .repository_binding_ledger()
            .unwrap()
            .upsert(
                &RepositoryBindingProjection::try_new(
                    &binding,
                    &node,
                    "fixture",
                    Some("main".into()),
                    Some("a".repeat(40)),
                    RepositoryDirtyState::Clean,
                    RepositoryAvailability::Available,
                    format!("sha256:{}", "a".repeat(64)),
                )
                .unwrap(),
                None,
                0,
                &now,
            )
            .unwrap();
        storage
            .repository_binding_ledger()
            .unwrap()
            .create_grant(
                &RepositoryAccessGrantIssuance::try_new(id("rag"), &binding, &user, &user).unwrap(),
                RepositoryGrantPermissions::Use,
                &now,
            )
            .unwrap();
        let lease = storage
            .client_occupancy_ledger()
            .unwrap()
            .atomic_claim(
                &OccupancyClaim::try_new(&occupancy, &node, &user, id("req")).unwrap(),
                &now,
            )
            .unwrap();
        storage
            .client_occupancy_ledger()
            .unwrap()
            .record_acknowledgement(&occupancy, lease.fencing_token, None, &now)
            .unwrap();
        let grant = WorkerLaunchGrantService::new(&mut storage)
            .issue(
                &LaunchGrantIssuance::try_new(
                    id("wlg"),
                    &node,
                    &instance,
                    &user,
                    &occupancy,
                    lease.fencing_token,
                    &binding,
                    id("wsn"),
                    id("wrk"),
                    id("wki"),
                    format!("sha256:{}", "b".repeat(64)),
                    Some(id("psn")),
                    None,
                    Instant("2027-01-15T09:00:00.000Z".into()),
                )
                .unwrap(),
                &now,
            )
            .unwrap();
        WorkerLaunchGrantService::new(&mut storage)
            .settle_device_launch_ack(
                &node,
                &LaunchAckSettlement::try_new(
                    &grant.worker_launch_grant_id,
                    &occupancy,
                    lease.fencing_token,
                    &grant.worker_session_id,
                    &grant.worker_id,
                    &grant.worker_instance_id,
                    true,
                    None,
                )
                .unwrap(),
                &now,
            )
            .unwrap();
        assert!(
            storage
                .device_execution_binding_ledger()
                .unwrap()
                .snapshot(&grant.worker_session_id)
                .unwrap()
                .is_none()
        );
        let revision = storage
            .client_node_registry()
            .unwrap()
            .snapshot(&node)
            .unwrap()
            .unwrap()
            .revision;
        storage
            .client_node_registry()
            .unwrap()
            .update_presence(&node, ClientPresenceState::Offline, revision)
            .unwrap();
        storage
            .client_occupancy_ledger()
            .unwrap()
            .mark_recovery_pending(
                &occupancy,
                &Instant("2027-01-15T08:05:00.000Z".into()),
                &later,
            )
            .unwrap();
        let mut payload = ClientWorkerReconcilePayload {
            occupancy_lease_id: Some(occupancy.clone()),
            workers: vec![ClientWorkerReconciliation {
                worker_session_id: grant.worker_session_id.clone(),
                worker_instance_id: grant.worker_instance_id.clone(),
                reconcile_state: WorkerReconcileState::Terminal,
                observed_at: later.0.clone(),
            }],
        };
        settle_worker_reconciliation(&mut storage, &node, &payload, &later.0, &later).unwrap();
        assert_eq!(
            storage
                .client_occupancy_ledger()
                .unwrap()
                .snapshot(&occupancy)
                .unwrap()
                .unwrap()
                .state,
            OccupancyLeaseState::RecoveryPending,
            "a terminal claim requires a durable exit"
        );
        settle_worker_exit(
            &mut storage,
            &node,
            &winwincode_client_port::messages::ClientWorkerStatePayload {
                occupancy_lease_id: Some(occupancy.clone()),
                worker_session_id: grant.worker_session_id.clone(),
                worker_instance_id: grant.worker_instance_id.clone(),
                state: winwincode_client_port::domain::ClientWorkerRunState::Stopped,
                exit_code: Some(0),
                observed_at: later.0.clone(),
                process_closure: Some(
                    winwincode_client_port::messages::ClientWorkerProcessClosure {
                        worker_launch_grant_id: grant.worker_launch_grant_id.clone(),
                        client_instance_id: instance.clone(),
                        reporting_client_instance_id: instance.clone(),
                        occupancy_fencing_token: lease.fencing_token,
                        never_started: false,
                        process_boot_digest: Some(format!("sha256:{}", "c".repeat(64))),
                    },
                ),
            },
            &later,
        )
        .unwrap();
        assert!(
            storage
                .worker_launch_grant_ledger()
                .unwrap()
                .has_trusted_exit(&grant.worker_launch_grant_id)
                .unwrap()
        );
        payload.workers[0].worker_instance_id = "wki_00000000000000000000000002".into();
        settle_worker_reconciliation(&mut storage, &node, &payload, &later.0, &later).unwrap();
        assert_eq!(
            storage
                .client_occupancy_ledger()
                .unwrap()
                .snapshot(&occupancy)
                .unwrap()
                .unwrap()
                .state,
            OccupancyLeaseState::RecoveryPending,
            "a foreign process cannot reconcile the anchor"
        );
        payload.workers[0].worker_instance_id = grant.worker_instance_id;
        settle_worker_reconciliation(&mut storage, &node, &payload, &later.0, &later).unwrap();
        let recovered = storage
            .client_occupancy_ledger()
            .unwrap()
            .snapshot(&occupancy)
            .unwrap()
            .unwrap();
        assert_eq!(
            recovered.state,
            OccupancyLeaseState::Draining,
            "a durably closed launch anchor needs no Job binding to finish reconciliation"
        );
        assert_eq!(recovered.fencing_token, lease.fencing_token);
        drop(storage);
        fs::remove_dir_all(root).unwrap();
    }
}
