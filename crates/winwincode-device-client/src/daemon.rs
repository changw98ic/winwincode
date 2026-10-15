// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientControlError, ClientControlErrorCode,
    ClientControlMessageKind, ClientLockState, ClientOccupancyReleaseMode, ClientPlatformTarget,
    CommandAckStatus, OccupancyRejectReason, PresenceState, WorkerLaunchAckStatus,
    WorkerLaunchGrant, WorkerLaunchGrantState,
};
use winwincode_client_port::exchange::{
    AckCursor, FrameCodec, FrameCodecError, FrameOutbox, OutboxBatch, OutboxError, OutboxSession,
    OutboxSnapshot, OutboxStateError, SequenceVerdict,
};
use winwincode_client_port::managed_app::{ManagedAppCommand, ManagedAppOperation};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientCommandAckPayload, ClientEnrollPayload,
    ClientHeartbeatPayload, ClientHelloPayload, ClientManagedAppStatusPayload,
    ClientOccupancyAckPayload, ClientOccupancyRejectedPayload, ClientToServerEnvelope,
    ClientToServerMessage, ClientWorkerLaunchAckPayload, CommandContext, OccupancyCommandContext,
    ServerClientLockPayload, ServerOccupancyForceFencePayload, ServerOccupancyOfferPayload,
    ServerOccupancyReleasePayload, ServerRepositoryRescanPayload, ServerToClientEnvelope,
    ServerToClientMessage, ServerWorkerLaunchPayload, ServerWorkerStopPayload,
};

use crate::connect_code::{self, PublishedConnectCode};
use crate::fencing::{
    FencedCommandKind, FencingGuard, FencingRejection, FencingTicket, FencingVerdict,
};
use crate::identity::{IdentityRecord, IssuedEnrollment, adopt_enrollment};
use crate::repository_exchange;
use crate::store::{
    ClientInboxCursorUpdate, ConnectCodeStateRecord, ConnectionPolicyRecord, DeviceStore,
    DeviceStoreError, DeviceStoreErrorKind, OccupancyMirrorAdvance, OccupancyMirrorRecord,
    OccupancyMirrorUpdate, OccupancyReleaseIntentOutcome, OccupancyReleaseIntentRecord,
    ServerProfileRecord, WorkerStopIntentOutcome, WorkerStopIntentRecord, envelope_kind,
};
use crate::supervisor::{SessionSupervisor, SpawnOutcome, SpawnRequest, SupervisorError};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerCapacitySnapshot {
    pub running_worker_sessions: u32,
    pub reserved_worker_sessions: u32,
}

pub trait WorkerCapacitySource: Send + Sync {
    fn worker_capacity(&self) -> WorkerCapacitySnapshot;
}

pub trait LeaseWorkerController: Send + Sync {
    fn stop_lease_workers(&self, occupancy_lease_id: &str) -> usize;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerLaunchDirectories {
    pub source_directory: PathBuf,
    pub data_directory: PathBuf,
    pub worker_root: PathBuf,
}

pub trait WorkerLaunchMaterialSource: Send + Sync {
    fn launch_directories(
        &self,
        worker_session_id: &str,
        repository_binding_id: &str,
    ) -> Option<WorkerLaunchDirectories>;
}

pub trait ManagedAppRepositorySource: Send + Sync {
    fn repository_root(&self, repository_binding_id: &str) -> Option<PathBuf>;
}

const WAKE_SLICE: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeTransportError {
    message: String,
    failure: winwincode_network::NetworkFailure,
}

impl ExchangeTransportError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            failure: winwincode_network::NetworkFailure::new(
                winwincode_network::ErrorKind::TransportInterrupted,
                winwincode_network::Acceptance::Unknown,
                winwincode_network::Phase::ResponseHeaders,
            ),
        }
    }

    #[must_use]
    pub fn from_network(failure: winwincode_network::NetworkFailure) -> Self {
        Self {
            message: failure.to_string(),
            failure,
        }
    }

    #[must_use]
    pub const fn network_failure(&self) -> winwincode_network::NetworkFailure {
        self.failure
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ExchangeTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "exchange transport failure: {}", self.message)
    }
}

impl std::error::Error for ExchangeTransportError {}

pub trait ExchangeTransport: Send + Sync {
    fn exchange(&self, request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError>;
    fn worker_exchange(&self, _request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
        Err(ExchangeTransportError::from_network(
            winwincode_network::NetworkFailure::new(
                winwincode_network::ErrorKind::RequestInvalid,
                winwincode_network::Acceptance::NotSent,
                winwincode_network::Phase::Connect,
            ),
        ))
    }
    fn exchange_authorized(
        &self,
        request_bytes: &[u8],
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        if !can_start() {
            return Err(ExchangeTransportError::from_network(
                winwincode_network::NetworkFailure::new(
                    winwincode_network::ErrorKind::Cancelled,
                    winwincode_network::Acceptance::NotSent,
                    winwincode_network::Phase::Connect,
                ),
            ));
        }
        self.exchange(request_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeRequest {
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    pub frames: Vec<serde_json::Value>,
    #[serde(rename = "ackSequence")]
    pub ack_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentIssuance {
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    #[serde(rename = "publicClientId")]
    pub public_client_id: String,
    #[serde(rename = "heartbeatIntervalMs")]
    pub heartbeat_interval_ms: u32,
    #[serde(rename = "serverTime")]
    pub server_time: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeResponse {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    #[serde(rename = "ackSequence")]
    pub ack_sequence: u64,
    #[serde(rename = "replayFromSequence", default)]
    pub replay_from_sequence: Option<u64>,
    #[serde(default)]
    pub frames: Vec<serde_json::Value>,
    #[serde(default)]
    pub enrollment: Option<EnrollmentIssuance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    pub server_profile_id: String,
    pub base_url: String,
    pub server_display_name: String,
    pub device_display_name: String,
    pub platform: ClientPlatformTarget,
    pub architecture: ClientArchitecture,
    pub client_version: String,
    pub heartbeat_interval: Duration,
    pub enroll_poll_interval: Duration,
    pub max_frames_per_exchange: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub capacity: ClientCapacityReport,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            server_profile_id: "default".to_owned(),
            base_url: String::new(),
            server_display_name: "WinWinCode Server".to_owned(),
            device_display_name: "WinWinCode Device".to_owned(),
            platform: ClientPlatformTarget::Aarch64AppleDarwin,
            architecture: ClientArchitecture::Aarch64,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            heartbeat_interval: Duration::from_secs(5),
            enroll_poll_interval: Duration::from_secs(1),
            max_frames_per_exchange: 16,
            initial_backoff: Duration::from_millis(winwincode_network::defaults().initial_delay_ms),
            max_backoff: Duration::from_millis(
                winwincode_network::defaults().max_connection_delay_ms,
            ),
            capacity: ClientCapacityReport {
                max_concurrent_worker_sessions: 0,
                running_worker_sessions: 0,
                reserved_worker_sessions: 0,
                draining_worker_sessions: 0,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    Config(String),
    Network(winwincode_network::NetworkFailure),
    Store(DeviceStoreError),
    CorruptOutbox(OutboxStateError),
    Protocol(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(formatter, "device daemon config error: {message}"),
            Self::Network(failure) => write!(formatter, "device daemon {failure}"),
            Self::Store(error) => write!(formatter, "device daemon store failure: {error}"),
            Self::CorruptOutbox(error) => {
                write!(formatter, "device daemon outbox is corrupt: {error:?}")
            }
            Self::Protocol(message) => {
                write!(formatter, "device daemon protocol failure: {message}")
            }
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<DeviceStoreError> for DaemonError {
    fn from(error: DeviceStoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonStatus {
    pub enrolled: bool,
    pub exchanges_started: u64,
    pub exchanges_succeeded: u64,
    pub consecutive_failures: u64,
    pub heartbeats_enqueued: u64,
    pub connect_codes_published: u64,
    pub client_lock_commands_applied: u64,
    pub repository_rescans_applied: u64,
    pub repository_rescans_rejected: u64,
    pub occupancy_offers_acked: u64,
    pub occupancy_offers_rejected: u64,
    pub occupancy_release_intents_recorded: u64,
    pub occupancy_force_fences_applied: u64,
    pub worker_launches_accepted: u64,
    pub worker_launches_rejected: u64,
    pub workers_stopped_on_release: u64,
    pub frames_sent: u64,
    pub acked_through: u64,
    pub downlink_accepted_through: u64,
    pub unhandled_downlink_commands: u64,
    pub replays: u64,
    pub current_backoff: Duration,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    Exchanged {
        frames_sent: usize,
        acked_through: u64,
        downlink_frames: usize,
    },
    Waiting {
        ready_in: Duration,
    },
    Retrying {
        after: Duration,
        reason: String,
    },
}

pub struct DeviceDaemon {
    config: DaemonConfig,
    store: DeviceStore,
    transport: Arc<dyn ExchangeTransport>,
    network_journal: winwincode_network::journal::RequestJournal,
    network_clock: (Instant, u64),
    codec: FrameCodec,
    session: OutboxSession,
    device_id: String,
    node_id: String,
    instance_id: String,
    enroll_idempotency_key: String,
    enrolled: bool,
    enroll_frame_pending: bool,
    hello_announced: bool,
    reported_worker_exits: std::collections::HashSet<String>,
    configuration_pending: Option<PendingConfigurationApply>,
    delivery_cursor: u64,
    downlink: AckCursor,
    heartbeat_interval: Duration,
    next_heartbeat_at: Instant,
    next_attempt_at: Instant,
    consecutive_failures: u64,
    connection_policy: ConnectionPolicyRecord,
    occupancy_mirror: Option<OccupancyMirrorRecord>,
    worker_capacity_source: Option<Arc<dyn WorkerCapacitySource>>,
    lease_worker_controller: Option<Arc<dyn LeaseWorkerController>>,
    worker_supervisor: Option<SessionSupervisor>,
    worker_launch_material: Option<Arc<dyn WorkerLaunchMaterialSource>>,
    managed_app_supervisor: Option<crate::managed_app::ManagedAppSupervisor>,
    managed_app_repository: Option<Arc<dyn ManagedAppRepositorySource>>,
    status: DaemonStatus,
}

impl DeviceDaemon {
    pub fn start(
        config: DaemonConfig,
        store: DeviceStore,
        transport: Arc<dyn ExchangeTransport>,
        identity: &IdentityRecord,
    ) -> Result<Self, DaemonError> {
        validate_config(&config)?;
        let device_id = identity.identity().device_id().to_owned();
        let enrolled_node_id = identity.identity().client_node_id().to_owned();
        let enrolled = !enrolled_node_id.is_empty();
        let node_id = if enrolled {
            enrolled_node_id
        } else {
            device_id.clone()
        };
        let instance_id = identity.current_instance_id().to_owned();
        let mut store = store;
        store
            .bind_outbox_stream(&node_id, &instance_id)
            .map_err(DaemonError::Store)?;
        let enroll_idempotency_key = format!("enroll-{device_id}-{instance_id}");
        let heartbeat_interval = config.heartbeat_interval;
        let connection_policy =
            connect_code::connection_policy(&store).map_err(DaemonError::Store)?;
        let occupancy_mirror = store.occupancy_mirror().map_err(DaemonError::Store)?;
        let network_journal = winwincode_network::journal::RequestJournal::open(
            &store
                .database_path()
                .with_file_name("network-requests.sqlite3"),
        )
        .map_err(DaemonError::Network)?;
        let mut daemon = Self {
            config,
            store,
            transport,
            network_journal,
            network_clock: (Instant::now(), winwincode_network::journal::now_millis()),
            codec: FrameCodec::default(),
            session: OutboxSession::new(),
            device_id,
            node_id,
            instance_id,
            enroll_idempotency_key,
            enrolled,
            enroll_frame_pending: false,
            hello_announced: false,
            reported_worker_exits: std::collections::HashSet::new(),
            configuration_pending: None,
            delivery_cursor: 0,
            downlink: AckCursor::new(),
            heartbeat_interval,
            next_heartbeat_at: Instant::now() + heartbeat_interval,
            next_attempt_at: Instant::now(),
            consecutive_failures: 0,
            connection_policy,
            occupancy_mirror,
            worker_capacity_source: None,
            lease_worker_controller: None,
            worker_supervisor: None,
            worker_launch_material: None,
            managed_app_supervisor: None,
            managed_app_repository: None,
            status: DaemonStatus {
                enrolled,
                ..DaemonStatus::default()
            },
        };
        if !daemon.enrolled {
            daemon.enroll_frame_pending = daemon
                .store
                .pending_outbox_envelopes()?
                .iter()
                .any(|entry| entry.kind == "client.enroll");
        }
        if daemon.enrolled
            && daemon
                .store
                .server_profile(&daemon.config.server_profile_id)?
                .is_none()
        {
            let stamp = now_rfc3339();
            daemon
                .store
                .upsert_server_profile(&ServerProfileRecord {
                    server_profile_id: daemon.config.server_profile_id.clone(),
                    base_url: daemon.config.base_url.clone(),
                    display_name: daemon.config.server_display_name.clone(),
                    created_at: stamp.clone(),
                    last_connected_at: Some(stamp),
                })
                .map_err(DaemonError::Store)?;
        }
        daemon.restore_downlink_cursor()?;
        Ok(daemon)
    }

    #[must_use]
    pub fn status(&self) -> &DaemonStatus {
        &self.status
    }

    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.node_id
    }

    #[must_use]
    pub fn client_instance_id(&self) -> &str {
        &self.instance_id
    }

    #[must_use]
    pub const fn is_enrolled(&self) -> bool {
        self.enrolled
    }

    pub fn shutdown_workers(&self) -> Result<(), DaemonError> {
        if let Some(supervisor) = &self.worker_supervisor {
            supervisor
                .shutdown()
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        }
        Ok(())
    }

    fn stop_occupancy_workers(&self, occupancy_lease_id: &str) -> Result<u64, DaemonError> {
        if let Some(supervisor) = &self.worker_supervisor {
            let outcomes = supervisor
                .stop_lease_workers(occupancy_lease_id)
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            if let Some(error) = outcomes.iter().find_map(|outcome| outcome.error.as_ref()) {
                return Err(DaemonError::Protocol(error.clone()));
            }
            return Ok(outcomes.iter().filter(|outcome| outcome.stopped).count() as u64);
        }
        Ok(self
            .lease_worker_controller
            .as_ref()
            .map_or(0, |controller| {
                controller.stop_lease_workers(occupancy_lease_id) as u64
            }))
    }

    fn stop_fenced_workers(&self, occupancy_lease_id: &str, token: u64) -> Result<(), DaemonError> {
        let Some(supervisor) = &self.worker_supervisor else {
            return Ok(());
        };
        for worker in supervisor
            .worker_processes()
            .map_err(|error| DaemonError::Protocol(error.to_string()))?
        {
            if worker.state != crate::supervisor::WORKER_STATE_RUNNING {
                continue;
            }
            let current = self
                .process_closure_for_record(&worker)
                .is_some_and(|closure| {
                    worker.occupancy_lease_id == occupancy_lease_id
                        && closure.occupancy_fencing_token == token
                });
            if !current {
                supervisor
                    .stop_recovered(
                        &worker.worker_session_id,
                        &worker.worker_id,
                        &worker.occupancy_lease_id,
                        true,
                    )
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn store_mut(&mut self) -> &mut DeviceStore {
        &mut self.store
    }

    pub fn outbox_snapshot(&mut self) -> Result<OutboxSnapshot, DaemonError> {
        FrameOutbox::load(&mut self.store)
            .map_err(DaemonError::Store)
            .map(Option::unwrap_or_default)
    }

    fn provider_directory(&self) -> Result<PathBuf, DaemonError> {
        self.store
            .database_path()
            .parent()
            .map(|path| path.join("providers"))
            .ok_or_else(|| {
                DaemonError::Protocol("device Provider directory is unavailable".to_owned())
            })
    }

    fn publish_provider_state(
        &mut self,
        receipt: Option<winwincode_api::generated::DeviceProviderReceipt>,
    ) -> Result<(), DaemonError> {
        let snapshot = winwincode_provider::DeviceProviderStore::open(&self.provider_directory()?)
            .and_then(|store| store.snapshot(&self.node_id))
            .map_err(|_| {
                DaemonError::Protocol("device Provider state is unavailable".to_owned())
            })?;
        self.enqueue(ClientToServerMessage::ProviderReport(Box::new(
            winwincode_api::generated::DeviceProviderReport { snapshot, receipt },
        )))?;
        Ok(())
    }

    fn publish_extension_state(
        &mut self,
        receipt: Option<winwincode_api::generated::DeviceExtensionReceipt>,
    ) -> Result<(), DaemonError> {
        let snapshot = winwincode_provider::DeviceProviderStore::open(&self.provider_directory()?)
            .and_then(|store| store.extension_snapshot(&self.node_id))
            .map_err(|_| {
                DaemonError::Protocol("device extension state is unavailable".to_owned())
            })?;
        self.enqueue(ClientToServerMessage::ExtensionReport(Box::new(
            winwincode_api::generated::DeviceExtensionReport { snapshot, receipt },
        )))?;
        Ok(())
    }

    pub fn enqueue(&mut self, message: ClientToServerMessage) -> Result<u64, DaemonError> {
        let expected = self
            .session
            .next_sequence(&mut self.store)
            .map_err(fatal_outbox)?;
        let kind = envelope_kind(&message).map_err(DaemonError::Store)?;
        let envelope = ClientToServerEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: self.frame_message_id(&kind, expected),
            client_node_id: self.node_id.clone(),
            client_instance_id: self.instance_id.clone(),
            sequence: expected,
            occurred_at: now_rfc3339(),
            message,
        };
        let stored = self
            .codec
            .encode_envelope(&envelope)
            .map_err(|error| DaemonError::Protocol(format!("frame encoding failed: {error:?}")))?;
        self.session
            .enqueue(&mut self.store, expected, &stored)
            .map_err(fatal_outbox)?;
        Ok(expected)
    }

    #[must_use]
    pub fn into_store(self) -> DeviceStore {
        self.store
    }

    pub fn publish_connect_code(&mut self) -> Result<PublishedConnectCode, DaemonError> {
        self.publish_connect_code_with_ttl(connect_code::CONNECT_CODE_TTL)
    }

    pub fn publish_connect_code_with_ttl(
        &mut self,
        ttl: Duration,
    ) -> Result<PublishedConnectCode, DaemonError> {
        if !self.enrolled {
            return Err(DaemonError::Protocol(
                "the connect code publication requires an adopted enrollment".to_owned(),
            ));
        }
        let now = OffsetDateTime::now_utc();
        let published =
            connect_code::publish_connect_code(&mut self.store, &self.instance_id, now, ttl)
                .map_err(map_connect_code_error)?;
        connect_code::enqueue_published_frame(
            &mut self.store,
            &self.node_id,
            &self.instance_id,
            &published.record,
            OffsetDateTime::now_utc(),
        )
        .map_err(map_connect_code_error)?;
        self.status.connect_codes_published += 1;
        Ok(published)
    }

    pub fn lock_client(&mut self) -> Result<ConnectionPolicyRecord, DaemonError> {
        self.apply_policy(false, ClientLockState::Locked)
    }

    pub fn unlock_client(&mut self) -> Result<ConnectionPolicyRecord, DaemonError> {
        self.apply_policy(true, ClientLockState::Unlocked)
    }

    pub fn set_accepting_connections(
        &mut self,
        accepting: bool,
    ) -> Result<ConnectionPolicyRecord, DaemonError> {
        let lock_state = self.connection_policy.lock_state;
        self.apply_policy(accepting, lock_state)
    }

    fn apply_policy(
        &mut self,
        accepting_connections: bool,
        lock_state: ClientLockState,
    ) -> Result<ConnectionPolicyRecord, DaemonError> {
        let record = connect_code::set_connection_policy(
            &mut self.store,
            accepting_connections,
            lock_state,
            OffsetDateTime::now_utc(),
        )
        .map_err(DaemonError::Store)?;
        self.connection_policy = record.clone();
        Ok(record)
    }

    pub fn connect_code_state(&self) -> Result<Option<ConnectCodeStateRecord>, DaemonError> {
        self.store.connect_code_state().map_err(DaemonError::Store)
    }

    pub fn connection_policy(&self) -> Result<ConnectionPolicyRecord, DaemonError> {
        connect_code::connection_policy(&self.store).map_err(DaemonError::Store)
    }

    #[must_use]
    pub const fn occupancy_mirror(&self) -> Option<&OccupancyMirrorRecord> {
        self.occupancy_mirror.as_ref()
    }

    #[must_use]
    pub fn fencing_guard(&self) -> FencingGuard {
        FencingGuard::new(self.occupancy_mirror.clone())
    }

    pub fn verify_fencing_ticket(&self, ticket: &FencingTicket) -> Result<(), FencingRejection> {
        self.fencing_guard().verify_ticket(ticket)
    }

    pub fn set_worker_capacity_source(&mut self, source: Arc<dyn WorkerCapacitySource>) {
        self.worker_capacity_source = Some(source);
    }

    pub fn set_lease_worker_controller(&mut self, controller: Arc<dyn LeaseWorkerController>) {
        self.lease_worker_controller = Some(controller);
    }

    pub fn set_worker_supervisor(&mut self, supervisor: SessionSupervisor) {
        self.worker_supervisor = Some(supervisor);
    }

    pub fn recover_worker_stop_intents(&mut self) -> Result<u64, DaemonError> {
        let pending = self
            .store
            .pending_worker_stop_intents()
            .map_err(DaemonError::Store)?;
        if pending.is_empty() {
            return Ok(0);
        }
        if self.worker_supervisor.is_none() {
            return Err(DaemonError::Protocol(
                "worker stop recovery requires a wired supervisor".to_owned(),
            ));
        }
        let mut recovered = 0_u64;
        for intent in pending {
            let payload = ServerWorkerStopPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: intent.expected_revision,
                        idempotency_key: intent.idempotency_key,
                    },
                    occupancy_lease_id: intent.occupancy_lease_id,
                    occupancy_fencing_token: intent.fencing_token,
                },
                worker_session_id: intent.worker_session_id,
                worker_id: intent.worker_id,
                reason: intent.reason,
            };
            self.apply_worker_stop(&payload, &intent.command_message_id)
                .map_err(DaemonError::from)?;
            recovered += 1;
        }
        Ok(recovered)
    }

    pub fn set_worker_launch_material_source(
        &mut self,
        source: Arc<dyn WorkerLaunchMaterialSource>,
    ) {
        self.worker_launch_material = Some(source);
    }

    pub fn set_managed_app_supervisor(
        &mut self,
        supervisor: crate::managed_app::ManagedAppSupervisor,
        repository: Arc<dyn ManagedAppRepositorySource>,
    ) {
        self.managed_app_supervisor = Some(supervisor);
        self.managed_app_repository = Some(repository);
    }

    fn current_capacity(&self) -> ClientCapacityReport {
        let mut capacity = self.config.capacity;
        if let Some(source) = self.worker_capacity_source.as_ref() {
            let snapshot = source.worker_capacity();
            capacity.running_worker_sessions = snapshot.running_worker_sessions;
            capacity.reserved_worker_sessions = snapshot.reserved_worker_sessions;
        }
        capacity
    }

    pub fn run(&mut self, shutdown: &AtomicBool) -> Result<DaemonStatus, DaemonError> {
        while !shutdown.load(Ordering::Relaxed) {
            let outcome =
                self.tick_authorized(Instant::now(), &|| !shutdown.load(Ordering::Acquire))?;
            let ready_in = match outcome {
                TickOutcome::Waiting { ready_in }
                | TickOutcome::Retrying {
                    after: ready_in, ..
                } => ready_in,
                TickOutcome::Exchanged { .. } => Duration::ZERO,
            };
            Self::sleep_interruptibly(ready_in.min(Duration::from_millis(25)), shutdown);
        }
        Ok(self.status.clone())
    }

    pub fn tick(&mut self, now: Instant) -> Result<TickOutcome, DaemonError> {
        self.tick_authorized(now, &|| true)
    }

    fn tick_authorized(
        &mut self,
        now: Instant,
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<TickOutcome, DaemonError> {
        if let Some(supervisor) = &self.worker_supervisor {
            supervisor
                .reap()
                .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        }
        let result = self.exchange_tick(now, can_start);
        if matches!(&result, Err(DaemonError::Network(_))) {
            self.shutdown_workers()?;
        }
        result
    }

    fn exchange_tick(
        &mut self,
        now: Instant,
        can_start: &(dyn Fn() -> bool + Sync),
    ) -> Result<TickOutcome, DaemonError> {
        if now < self.next_attempt_at {
            return Ok(TickOutcome::Waiting {
                ready_in: self.next_attempt_at.duration_since(now),
            });
        }
        self.ensure_pending_reports(now)?;
        let mut batch = match self.session.deliverable(
            &mut self.store,
            self.delivery_cursor,
            self.config.max_frames_per_exchange,
        ) {
            Ok(batch) => batch,
            Err(OutboxError::CorruptState(state)) => return Err(DaemonError::CorruptOutbox(state)),
            Err(OutboxError::Store(store)) => return Err(DaemonError::Store(store)),
            Err(other) => {
                return Err(DaemonError::Protocol(format!(
                    "outbox delivery request failed: {other:?}"
                )));
            }
        };
        if batch.frames.is_empty() {
            let ready_in = if self.enrolled {
                self.idle_ready_in(now)
            } else {
                self.config.enroll_poll_interval
            };
            return Ok(TickOutcome::Waiting { ready_in });
        }
        let operation = format!("{}:{}", self.device_id, batch.frames[0].sequence);
        let bound = self
            .network_journal
            .bind_envelope(
                operation.as_bytes(),
                &winwincode_network::journal::EnvelopeCursor {
                    last_sequence: batch.frames.last().map_or(0, |frame| frame.sequence),
                    ack_sequence: self.downlink.ack_sequence(),
                    ..Default::default()
                },
            )
            .map_err(DaemonError::Network)?;
        batch
            .frames
            .retain(|frame| frame.sequence <= bound.last_sequence);
        let mut request = self.build_request(&batch)?;
        request.ack_sequence = bound.ack_sequence;
        let request_bytes = serde_json::to_vec(&request)
            .map_err(|error| DaemonError::Protocol(format!("request encoding failed: {error}")))?;
        let network_now = self.network_now(now);
        let sequence = match self
            .network_journal
            .prepare(
                operation.as_bytes(),
                winwincode_network::Replay::ReplayExact,
                winwincode_network::defaults().max_attempts,
                network_now,
            )
            .map_err(DaemonError::Network)?
        {
            winwincode_network::journal::QueuePermit::Ready(sequence) => sequence,
            winwincode_network::journal::QueuePermit::Waiting(delay, failure) => {
                self.next_attempt_at = now + delay;
                self.status.current_backoff = delay;
                self.status.last_error = Some(failure.to_string());
                return Ok(self.retry_outcome());
            }
            winwincode_network::journal::QueuePermit::Stopped(failure) => {
                return Err(DaemonError::Network(failure));
            }
        };
        self.status.exchanges_started += 1;
        let response_bytes = match self
            .transport
            .exchange_authorized(&request_bytes, can_start)
        {
            Ok(bytes) => bytes,
            Err(error) => {
                return self.record_exchange_failure(
                    &operation,
                    sequence,
                    &error,
                    now,
                    network_now,
                );
            }
        };
        let response =
            self.decode_exchange_response(&response_bytes, &operation, sequence, network_now)?;
        self.network_journal
            .finish(operation.as_bytes(), sequence, None, network_now)
            .map_err(DaemonError::Network)?;
        if let Some(replay_from) = response.replay_from_sequence {
            self.apply_gap(&response, replay_from, &batch, now)
        } else {
            self.apply_accepted(&response, &batch, now)
        }
    }

    fn network_now(&self, now: Instant) -> u64 {
        self.network_clock
            .1
            .saturating_add(winwincode_network::duration_millis(
                now.saturating_duration_since(self.network_clock.0),
            ))
    }

    fn record_exchange_failure(
        &mut self,
        operation: &str,
        sequence: u64,
        error: &ExchangeTransportError,
        now: Instant,
        network_now: u64,
    ) -> Result<TickOutcome, DaemonError> {
        let reason = format!("exchange transport failed: {}", error.message());
        self.network_journal
            .finish_with_delay_cap(
                operation.as_bytes(),
                sequence,
                Some(error.network_failure()),
                network_now,
                Some(self.config.max_backoff),
            )
            .map_err(DaemonError::Network)?;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.status.consecutive_failures = self.consecutive_failures;
        self.status.last_error = Some(reason);
        if let winwincode_network::journal::QueuePermit::Waiting(delay, _) = self
            .network_journal
            .prepare(
                operation.as_bytes(),
                winwincode_network::Replay::ReplayExact,
                winwincode_network::defaults().max_attempts,
                network_now,
            )
            .map_err(DaemonError::Network)?
        {
            self.next_attempt_at = now + delay;
            self.status.current_backoff = delay;
        } else {
            return Err(DaemonError::Network(error.network_failure()));
        }
        Ok(self.retry_outcome())
    }

    fn decode_exchange_response(
        &mut self,
        bytes: &[u8],
        operation: &str,
        sequence: u64,
        network_now: u64,
    ) -> Result<ExchangeResponse, DaemonError> {
        let decoded = serde_json::from_slice::<ExchangeResponse>(bytes)
            .map_err(|error| {
                (
                    "exchange response is not valid JSON".to_owned(),
                    winwincode_network::NetworkDiagnostic::json(&error),
                )
            })
            .and_then(|response| {
                if response.schema_version == CLIENT_CONTROL_PORT_SCHEMA_VERSION {
                    Ok(response)
                } else {
                    Err((
                        "exchange response schema version is invalid".to_owned(),
                        winwincode_network::NetworkDiagnostic::new(
                            winwincode_network::DiagnosticCode::SchemaVersion,
                        ),
                    ))
                }
            });
        match decoded {
            Ok(response) => Ok(response),
            Err((reason, diagnostic)) => {
                let failure = winwincode_network::NetworkFailure::new(
                    if diagnostic.code == winwincode_network::DiagnosticCode::SchemaVersion {
                        winwincode_network::ErrorKind::RequestInvalid
                    } else {
                        winwincode_network::ErrorKind::ProtocolInvalid
                    },
                    winwincode_network::Acceptance::ResponseReceived,
                    winwincode_network::Phase::Decode,
                )
                .with_diagnostic(diagnostic);
                self.network_journal
                    .finish(operation.as_bytes(), sequence, Some(failure), network_now)
                    .map_err(DaemonError::Network)?;
                self.status.last_error = Some(format!("{reason}; {failure}"));
                Err(DaemonError::Network(failure))
            }
        }
    }

    fn ensure_pending_reports(&mut self, now: Instant) -> Result<(), DaemonError> {
        if !self.enrolled {
            if !self.enroll_frame_pending {
                self.enqueue(ClientToServerMessage::Enroll(Box::new(
                    ClientEnrollPayload {
                        command: CommandContext {
                            expected_revision: 0,
                            idempotency_key: self.enroll_idempotency_key.clone(),
                        },
                        display_name: self.config.device_display_name.clone(),
                        platform: self.config.platform,
                        architecture: self.config.architecture,
                        client_version: self.config.client_version.clone(),
                    },
                )))?;
                self.enroll_frame_pending = true;
            }
            return Ok(());
        }
        if !self.hello_announced {
            self.enqueue(ClientToServerMessage::Hello(ClientHelloPayload {
                client_version: self.config.client_version.clone(),
                capacity: self.current_capacity(),
                accepting_connections: self.connection_policy.accepting_connections,
                lock_state: self.connection_policy.lock_state,
                presence_state: PresenceState::Online,
            }))?;
            self.publish_provider_state(None)?;
            self.publish_extension_state(None)?;
            self.publish_worker_exits()?;
            self.publish_worker_reconciliation()?;
            self.hello_announced = true;
            self.next_heartbeat_at = now + self.heartbeat_interval;
            return Ok(());
        }
        if now >= self.next_heartbeat_at {
            self.enqueue(ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
                capacity: self.current_capacity(),
                accepting_connections: self.connection_policy.accepting_connections,
                lock_state: self.connection_policy.lock_state,
                presence_state: PresenceState::Online,
                occupancy_lease_id: self
                    .occupancy_mirror
                    .as_ref()
                    .map(|mirror| mirror.occupancy_lease_id.clone()),
            }))?;
            self.status.heartbeats_enqueued += 1;
            self.publish_worker_exits()?;
            self.publish_worker_reconciliation()?;
            self.next_heartbeat_at = now + self.heartbeat_interval;
        }
        Ok(())
    }

    fn publish_worker_reconciliation(&mut self) -> Result<(), DaemonError> {
        use crate::supervisor::WorkerReconcileVerdict;
        use winwincode_client_port::domain::WorkerReconcileState;
        use winwincode_client_port::messages::{
            ClientWorkerReconcilePayload, ClientWorkerReconciliation,
        };
        let (Some(supervisor), Some(mirror)) = (&self.worker_supervisor, &self.occupancy_mirror)
        else {
            return Ok(());
        };
        let lease_id = mirror.occupancy_lease_id.clone();
        let reports = supervisor
            .reconcile_pending(&lease_id)
            .map_err(|_| DaemonError::Protocol("worker reconciliation failed".into()))?;
        let reported_records = reports
            .iter()
            .map(|report| report.record.clone())
            .collect::<Vec<_>>();
        let observed_at = now_rfc3339();
        let workers = reports
            .into_iter()
            .map(|report| ClientWorkerReconciliation {
                worker_session_id: report.worker_session_id,
                worker_instance_id: report.worker_instance_id,
                reconcile_state: match report.verdict {
                    WorkerReconcileVerdict::StillRunning => WorkerReconcileState::StillRunning,
                    WorkerReconcileVerdict::Terminal => WorkerReconcileState::Terminal,
                    WorkerReconcileVerdict::Missing => WorkerReconcileState::Missing,
                    WorkerReconcileVerdict::Unknown => WorkerReconcileState::Unknown,
                },
                observed_at: observed_at.clone(),
            })
            .collect();
        self.enqueue(ClientToServerMessage::WorkerReconcile(
            ClientWorkerReconcilePayload {
                occupancy_lease_id: Some(lease_id),
                workers,
            },
        ))?;
        for record in reported_records {
            self.store.worker_report_queued("reconcile", &record)?;
        }
        Ok(())
    }

    fn publish_worker_exits(&mut self) -> Result<(), DaemonError> {
        for worker in self.store.worker_report_candidates("exit", None)? {
            use winwincode_client_port::domain::ClientWorkerRunState;
            let state = match worker.state.as_str() {
                "exited" => ClientWorkerRunState::Stopped,
                "crashed" => ClientWorkerRunState::Crashed,
                "missing" => ClientWorkerRunState::Missing,
                _ => continue,
            };
            if self
                .reported_worker_exits
                .contains(&worker.worker_instance_id)
            {
                self.store.worker_report_queued("exit", &worker)?;
                continue;
            }
            let retained =
                crate::candidate_registry::retain_worker_candidates(self, &worker).is_ok();
            let closure = self.process_closure_for_record(&worker);
            if closure.is_some()
                && let Some(supervisor) = &self.worker_supervisor
            {
                supervisor
                    .recover_terminated_calls(&worker)
                    .map_err(|error| DaemonError::Protocol(error.to_string()))?;
            }
            let report_record = worker.clone();
            self.enqueue(ClientToServerMessage::WorkerState(
                winwincode_client_port::messages::ClientWorkerStatePayload {
                    process_closure: closure,
                    occupancy_lease_id: Some(worker.occupancy_lease_id),
                    worker_session_id: worker.worker_session_id,
                    worker_instance_id: worker.worker_instance_id.clone(),
                    state,
                    exit_code: worker.exit_code.and_then(|code| i32::try_from(code).ok()),
                    observed_at: worker.last_observed_at,
                },
            ))?;
            if retained {
                self.reported_worker_exits.insert(worker.worker_instance_id);
                self.store.worker_report_queued("exit", &report_record)?;
            }
        }
        for (grant, bytes) in self.store.pending_worker_launch_receipts()? {
            let stored: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| DaemonError::Protocol("Worker launch receipt is corrupt".into()))?;
            let ack = decode_worker_launch_receipt_ack(&stored)?;
            let ClientToServerMessage::WorkerLaunchAck(ack) = ack else {
                self.store.worker_launch_report_queued(&grant)?;
                continue;
            };
            let Some(mut closure) = ack.process_closure.clone() else {
                self.store.worker_launch_report_queued(&grant)?;
                continue;
            };
            if !closure.never_started
                || closure.process_boot_digest.is_some()
                || self.store.worker_process(&ack.worker_session_id)?.is_some()
                || self.reported_worker_exits.contains(&ack.worker_instance_id)
            {
                self.store.worker_launch_report_queued(&grant)?;
                continue;
            }
            closure
                .reporting_client_instance_id
                .clone_from(&self.instance_id);
            self.enqueue(ClientToServerMessage::WorkerState(
                winwincode_client_port::messages::ClientWorkerStatePayload {
                    occupancy_lease_id: Some(ack.occupancy.occupancy_lease_id),
                    worker_session_id: ack.worker_session_id,
                    worker_instance_id: ack.worker_instance_id.clone(),
                    state: winwincode_client_port::domain::ClientWorkerRunState::Stopped,
                    exit_code: None,
                    observed_at: now_rfc3339(),
                    process_closure: Some(closure),
                },
            ))?;
            self.reported_worker_exits.insert(ack.worker_instance_id);
            self.store.worker_launch_report_queued(&grant)?;
        }
        Ok(())
    }

    fn build_request(&self, batch: &OutboxBatch) -> Result<ExchangeRequest, DaemonError> {
        let mut values = Vec::with_capacity(batch.frames.len());
        for frame in &batch.frames {
            let value: serde_json::Value =
                self.codec
                    .decode(&frame.frame)
                    .map_err(|error: FrameCodecError| {
                        DaemonError::Protocol(format!("stored frame is undecodable: {error:?}"))
                    })?;
            values.push(value);
        }
        Ok(ExchangeRequest {
            client_node_id: self.node_id.clone(),
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            frames: values,
            ack_sequence: self.downlink.ack_sequence(),
        })
    }

    fn apply_accepted(
        &mut self,
        response: &ExchangeResponse,
        batch: &OutboxBatch,
        now: Instant,
    ) -> Result<TickOutcome, DaemonError> {
        let ack = match self
            .session
            .acknowledge(&mut self.store, response.ack_sequence)
        {
            Ok(ack) => ack,
            Err(error) => match retriable_ack_error(error) {
                Ok(reason) => {
                    self.register_failure(now, reason);
                    return Ok(self.retry_outcome());
                }
                Err(fatal) => return Err(fatal),
            },
        };
        self.session
            .compact_confirmed(&mut self.store)
            .map_err(fatal_outbox)?;
        self.delivery_cursor = ack;
        self.status.frames_sent += u64::try_from(batch.frames.len()).unwrap_or(u64::MAX);
        self.status.acked_through = ack;
        self.status.exchanges_succeeded += 1;
        self.reset_backoff();

        if let Some(issuance) = &response.enrollment {
            self.accept_enrollment(issuance)?;
        }
        let downlink_frames = match self.ingest_downlink(&response.frames) {
            Ok(count) => count,
            Err(DownlinkFailure::Retriable(reason)) => {
                self.register_failure(now, reason);
                return Ok(self.retry_outcome());
            }
            Err(DownlinkFailure::Fatal(error)) => return Err(error),
        };
        Ok(TickOutcome::Exchanged {
            frames_sent: batch.frames.len(),
            acked_through: ack,
            downlink_frames,
        })
    }

    fn apply_gap(
        &mut self,
        response: &ExchangeResponse,
        replay_from: u64,
        batch: &OutboxBatch,
        now: Instant,
    ) -> Result<TickOutcome, DaemonError> {
        if replay_from == 0 || replay_from > batch.highest_sequence + 1 {
            let reason = format!(
                "gap replayFromSequence {replay_from} is outside the retained stream \
                 through {}",
                batch.highest_sequence
            );
            self.register_failure(now, reason);
            return Ok(self.retry_outcome());
        }
        if response.enrollment.is_some() {
            let reason = "enrollment issuance on a gapped exchange is not actionable".to_owned();
            self.register_failure(now, reason);
            return Ok(self.retry_outcome());
        }
        self.delivery_cursor = replay_from - 1;
        self.status.replays += 1;
        self.status.exchanges_succeeded += 1;
        self.reset_backoff();
        let downlink_frames = match self.ingest_downlink(&response.frames) {
            Ok(count) => count,
            Err(DownlinkFailure::Retriable(reason)) => {
                self.register_failure(now, reason);
                return Ok(self.retry_outcome());
            }
            Err(DownlinkFailure::Fatal(error)) => return Err(error),
        };
        Ok(TickOutcome::Exchanged {
            frames_sent: 0,
            acked_through: response.ack_sequence,
            downlink_frames,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_downlink(&mut self, frames: &[serde_json::Value]) -> Result<usize, DownlinkFailure> {
        let started = Instant::now();
        let mut accepted = 0_usize;
        for (visited, value) in frames.iter().enumerate() {
            if visited > 0 && (visited >= 32 || started.elapsed() >= Duration::from_millis(25)) {
                break;
            }
            let envelope: ServerToClientEnvelope =
                decode_downlink_frame(&self.codec, value).map_err(DownlinkFailure::Retriable)?;
            if matches!(
                self.downlink.observe(envelope.sequence),
                SequenceVerdict::Accept
            ) && let ServerToClientMessage::CandidateApply(payload) = &envelope.message
            {
                if envelope.client_node_id != self.node_id {
                    return Err(DownlinkFailure::Retriable(
                        "candidate command belongs to another device".into(),
                    ));
                }
                let current = self.occupancy_mirror.as_ref();
                let stamp = &payload.occupancy;
                let current_authority = current.is_some_and(|mirror| {
                    mirror.holder_user_id.as_deref() == Some(payload.requester_user_id.as_str())
                        && mirror.mirror_revision == stamp.command.expected_revision
                }) && matches!(
                    self.fencing_guard().authorize_command(
                        FencedCommandKind::CandidateApply,
                        &stamp.occupancy_lease_id,
                        stamp.occupancy_fencing_token
                    ),
                    FencingVerdict::Authorized(_)
                );
                if envelope.client_instance_id != self.instance_id || !current_authority {
                    let current_revision = current.map(|mirror| mirror.mirror_revision);
                    self.enqueue(command_ack_message(
                        ClientControlMessageKind::CandidateApply,
                        &envelope.message_id,
                        CommandAckStatus::RejectedStaleFencingToken,
                        current_revision,
                        Some(ClientControlError {
                            code: if envelope.client_instance_id == self.instance_id {
                                ClientControlErrorCode::StaleFencingToken
                            } else {
                                ClientControlErrorCode::DeviceInstanceChanged
                            },
                            message: "the candidate command has obsolete execution authority"
                                .into(),
                            retryable: false,
                        }),
                    ))
                    .map_err(DownlinkFailure::Fatal)?;
                    self.advance_downlink_cursor(&envelope)?;
                    accepted += 1;
                    continue;
                }
                self.apply_candidate_command(payload)
                    .map_err(DownlinkFailure::Fatal)?;
            }
            if matches!(
                self.downlink.observe(envelope.sequence),
                SequenceVerdict::Accept
            ) && let ServerToClientMessage::RepositoryRegister(payload) = &envelope.message
            {
                if envelope.client_node_id != self.node_id
                    || envelope.client_instance_id != self.instance_id
                {
                    return Err(DownlinkFailure::Retriable(
                        "repository command belongs to another device instance".to_owned(),
                    ));
                }
                let directory = self.provider_directory().map_err(DownlinkFailure::Fatal)?;
                let receipt = repository_exchange::apply_repository_registration(
                    &mut self.store,
                    &directory,
                    &self.node_id,
                    &self.instance_id,
                    payload,
                )
                .map_err(DownlinkFailure::Fatal)?;
                self.enqueue(ClientToServerMessage::RepositoryRegistered { receipt })
                    .map_err(DownlinkFailure::Fatal)?;
            }
            if matches!(
                self.downlink.observe(envelope.sequence),
                SequenceVerdict::Accept
            ) && let ServerToClientMessage::ProviderApply(payload)
            | ServerToClientMessage::ExtensionApply(payload) = &envelope.message
            {
                let extension =
                    matches!(&envelope.message, ServerToClientMessage::ExtensionApply(_));
                if envelope.client_node_id != self.node_id
                    || envelope.client_instance_id != self.instance_id
                    || payload.command.idempotency_key != payload.encrypted.request_id
                    || i64::try_from(payload.command.expected_revision).ok()
                        != Some(payload.encrypted.expected_revision)
                {
                    return Err(DownlinkFailure::Retriable(
                        "Provider command belongs to another device instance".to_owned(),
                    ));
                }
                if self.configuration_pending.as_ref().is_some_and(|pending| {
                    pending.envelope != payload.encrypted || pending.extension != extension
                }) {
                    return Err(DownlinkFailure::Retriable(
                        "Provider command changed while executing".to_owned(),
                    ));
                }
                if self.configuration_pending.is_none() {
                    let directory = self.provider_directory().map_err(DownlinkFailure::Fatal)?;
                    let device = self.node_id.clone();
                    let payload = payload.encrypted.clone();
                    self.configuration_pending = Some(PendingConfigurationApply {
                        envelope: payload.clone(),
                        extension,
                        worker: thread::Builder::new()
                            .name("device-configuration".to_owned())
                            .spawn(move || {
                                winwincode_provider::DeviceProviderStore::open(&directory).and_then(
                                    |mut store| {
                                        if extension {
                                            store
                                                .apply_extension(&device, &payload)
                                                .map(ConfigurationReceipt::Extension)
                                        } else {
                                            store
                                                .apply(&device, &payload)
                                                .map(ConfigurationReceipt::Provider)
                                        }
                                    },
                                )
                            })
                            .map_err(|_| {
                                DownlinkFailure::Retriable(
                                    "device Provider command could not start".to_owned(),
                                )
                            })?,
                    });
                }
                if !self
                    .configuration_pending
                    .as_ref()
                    .is_some_and(|pending| pending.worker.is_finished())
                {
                    self.next_heartbeat_at = Instant::now() + Duration::from_millis(500);
                    return Ok(accepted);
                }
                let receipt = self
                    .configuration_pending
                    .take()
                    .ok_or_else(|| {
                        DownlinkFailure::Retriable("device Provider command is missing".to_owned())
                    })?
                    .worker
                    .join()
                    .map_err(|_| {
                        DownlinkFailure::Retriable(
                            "device Provider command was interrupted".to_owned(),
                        )
                    })?
                    .map_err(|_| {
                        DownlinkFailure::Retriable("device Provider command failed".to_owned())
                    })?;
                match receipt {
                    ConfigurationReceipt::Provider(receipt) => {
                        self.publish_provider_state(Some(receipt))
                    }
                    ConfigurationReceipt::Extension(receipt) => {
                        self.publish_extension_state(Some(receipt))
                    }
                }
                .map_err(DownlinkFailure::Fatal)?;
            }
            match self.downlink.observe(envelope.sequence) {
                SequenceVerdict::Accept => {
                    match &envelope.message {
                        ServerToClientMessage::CommandAck(payload) => {
                            self.store.record_command_result(payload).map_err(|error| {
                                DownlinkFailure::Fatal(DaemonError::Store(error))
                            })?;
                            if let Some(error) = &payload.error {
                                self.status.last_error = Some(format!(
                                    "command {}: {:?}",
                                    payload.command_message_id, error.code
                                ));
                            }
                        }
                        ServerToClientMessage::ProviderApply(_)
                        | ServerToClientMessage::CandidateApply(_)
                        | ServerToClientMessage::RepositoryRegister(_)
                        | ServerToClientMessage::ExtensionApply(_) => {}
                        ServerToClientMessage::ClientLock(payload) => {
                            self.apply_client_lock(payload, &envelope.message_id)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::OccupancyOffer(payload) => {
                            self.apply_occupancy_offer(payload)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::OccupancyRelease(payload) => {
                            self.apply_occupancy_release(payload, &envelope.message_id)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::OccupancyForceFence(payload) => {
                            self.apply_occupancy_force_fence(payload, &envelope.message_id)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::WorkerLaunch(payload) => {
                            self.apply_worker_launch(payload)
                                .map_err(|error| DownlinkFailure::Retriable(error.to_string()))?;
                        }
                        ServerToClientMessage::WorkerStop(payload) => {
                            self.apply_worker_stop(payload, &envelope.message_id)
                                .map_err(worker_stop_failure_to_downlink)?;
                        }
                        ServerToClientMessage::RepositoryRescan(payload) => {
                            self.apply_repository_rescan(payload, &envelope.message_id)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::ManagedAppCommand(payload) => {
                            self.apply_managed_app_command(payload, &envelope.message_id)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                    }
                    self.advance_downlink_cursor(&envelope)?;
                    accepted += 1;
                }
                SequenceVerdict::Duplicate => {}
                SequenceVerdict::Gap {
                    replay_from_sequence,
                } => {
                    return Err(DownlinkFailure::Retriable(format!(
                        "server downlink skipped sequences: expected \
                         {replay_from_sequence}, received {}",
                        envelope.sequence
                    )));
                }
                SequenceVerdict::Zero => {
                    return Err(DownlinkFailure::Retriable(
                        "server downlink carried sequence zero".to_owned(),
                    ));
                }
            }
        }
        self.status.downlink_accepted_through = self.downlink.ack_sequence();
        Ok(accepted)
    }

    fn advance_downlink_cursor(
        &mut self,
        envelope: &ServerToClientEnvelope,
    ) -> Result<(), DownlinkFailure> {
        self.store
            .advance_inbox_cursor(&ClientInboxCursorUpdate {
                server_profile_id: self.config.server_profile_id.clone(),
                last_sequence: envelope.sequence,
                last_message_id: Some(envelope.message_id.clone()),
                updated_at: now_rfc3339(),
            })
            .map_err(|error| DownlinkFailure::Fatal(DaemonError::Store(error)))?;
        self.downlink.advance(envelope.sequence).map_err(|error| {
            DownlinkFailure::Fatal(DaemonError::Protocol(format!(
                "downlink cursor rejected an accepted sequence: {error:?}"
            )))
        })?;
        Ok(())
    }

    fn apply_candidate_command(
        &mut self,
        payload: &winwincode_client_port::messages::ServerCandidateApplyPayload,
    ) -> Result<(), DaemonError> {
        let stamp = &payload.occupancy;
        let mirror = self
            .occupancy_mirror()
            .ok_or_else(|| DaemonError::Protocol("candidate command has no occupancy".into()))?;
        if mirror.holder_user_id.as_deref() != Some(payload.requester_user_id.as_str())
            || mirror.mirror_revision != stamp.command.expected_revision
        {
            return Err(DaemonError::Protocol(
                "candidate command has a stale holder or revision".into(),
            ));
        }
        match self.fencing_guard().authorize_command(
            FencedCommandKind::CandidateApply,
            &stamp.occupancy_lease_id,
            stamp.occupancy_fencing_token,
        ) {
            FencingVerdict::Authorized(_) => {}
            FencingVerdict::Rejected(reason) => {
                return Err(DaemonError::Protocol(format!(
                    "candidate command rejected: {reason:?}"
                )));
            }
        }
        if payload.strategy == winwincode_client_port::domain::ApplyStrategy::CreateBranch {
            crate::candidate_branch::create_candidate_branch(
                self,
                &crate::candidate_branch::BranchCreationRequest {
                    repository_binding_id: payload.repository_binding_id.clone(),
                    candidate_ref: payload.candidate_ref.clone(),
                    task_slug: "delivery".into(),
                    requested_branch_name: Some(payload.target_branch.clone()),
                    requested_at: now_rfc3339(),
                },
            )
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        } else {
            let root = self
                .store
                .database_path()
                .parent()
                .ok_or_else(|| {
                    DaemonError::Protocol("candidate integration directory is unavailable".into())
                })?
                .join("candidate-integration");
            crate::apply_engine::apply_candidate_to_branch(
                self,
                &crate::apply_engine::CandidateApplyRequest {
                    application_id: payload.application_id.clone(),
                    approval_id: payload.approval_id.clone(),
                    candidate_digest: payload.candidate_digest.clone(),
                    reconciled_retry: payload.reconciled_retry,
                    repository_binding_id: payload.repository_binding_id.clone(),
                    candidate_ref: payload.candidate_ref.clone(),
                    target_branch: payload.target_branch.clone(),
                    expected_head: payload.expected_head.clone(),
                    strategy: payload.strategy,
                    occupancy_lease_id: stamp.occupancy_lease_id.clone(),
                    occupancy_fencing_token: stamp.occupancy_fencing_token,
                },
                &root,
            )
            .map(|_| ())
            .or_else(|error| {
                if matches!(
                    error.kind(),
                    crate::apply_engine::CandidateApplyErrorKind::ReconciliationRequired
                        | crate::apply_engine::CandidateApplyErrorKind::OutcomeUnknown
                ) {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        }
        Ok(())
    }

    fn apply_repository_rescan(
        &mut self,
        payload: &ServerRepositoryRescanPayload,
        command_message_id: &str,
    ) -> Result<(), DaemonError> {
        let applied = repository_exchange::apply_repository_rescan(
            &mut self.store,
            &self.node_id,
            &self.instance_id,
            command_message_id,
            payload,
            OffsetDateTime::now_utc(),
        )
        .map_err(|error| DaemonError::Protocol(format!("repository rescan failed: {error}")))?;
        if applied.revalidation.is_some() {
            self.status.repository_rescans_applied += 1;
        } else {
            self.status.repository_rescans_rejected += 1;
        }
        Ok(())
    }

    fn accept_enrollment(&mut self, issuance: &EnrollmentIssuance) -> Result<(), DaemonError> {
        let placeholder_node_id = self.node_id.clone();
        let stamp = now_rfc3339();
        adopt_enrollment(
            &mut self.store,
            &self.device_id,
            &IssuedEnrollment {
                client_node_id: issuance.client_node_id.clone(),
                public_client_id: issuance.public_client_id.clone(),
            },
            &stamp,
        )
        .map_err(|error| match error.kind() {
            DeviceStoreErrorKind::InvalidInput => DaemonError::Protocol(format!(
                "the issued enrollment identity is invalid: {error}"
            )),
            _ => DaemonError::Store(error),
        })?;
        self.store
            .adopt_enrolled_stream(&placeholder_node_id, &issuance.client_node_id)
            .map_err(DaemonError::Store)?;
        self.node_id.clone_from(&issuance.client_node_id);
        if issuance.heartbeat_interval_ms > 0 {
            self.heartbeat_interval =
                Duration::from_millis(u64::from(issuance.heartbeat_interval_ms));
        }
        self.store
            .upsert_server_profile(&ServerProfileRecord {
                server_profile_id: self.config.server_profile_id.clone(),
                base_url: self.config.base_url.clone(),
                display_name: self.config.server_display_name.clone(),
                created_at: stamp.clone(),
                last_connected_at: Some(stamp),
            })
            .map_err(DaemonError::Store)?;
        self.enrolled = true;
        self.enroll_frame_pending = false;
        self.hello_announced = false;
        self.next_heartbeat_at = Instant::now() + self.heartbeat_interval;
        self.status.enrolled = true;
        Ok(())
    }

    fn apply_client_lock(
        &mut self,
        payload: &ServerClientLockPayload,
        command_message_id: &str,
    ) -> Result<(), DaemonError> {
        let locked = payload.lock_state == ClientLockState::Locked;
        self.apply_policy(!locked, payload.lock_state)?;
        self.enqueue(ClientToServerMessage::CommandAck(ClientCommandAckPayload {
            command_kind: ClientControlMessageKind::ClientLock,
            command_message_id: command_message_id.to_owned(),
            status: CommandAckStatus::Accepted,
            current_revision: None,
            error: None,
        }))?;
        self.status.client_lock_commands_applied += 1;
        Ok(())
    }

    fn apply_occupancy_offer(
        &mut self,
        payload: &ServerOccupancyOfferPayload,
    ) -> Result<(), DaemonError> {
        let stamp = payload.occupancy.clone();
        let current_revision = self
            .occupancy_mirror
            .as_ref()
            .map_or(0, |mirror| mirror.mirror_revision);

        if self.connection_policy.lock_state == ClientLockState::Locked {
            return self.reject_occupancy_offer(
                &stamp,
                OccupancyRejectReason::ClientLocked,
                current_revision,
            );
        }
        if self
            .occupancy_mirror
            .as_ref()
            .is_some_and(|mirror| mirror_is_exact(mirror, &stamp))
        {
            let record = self.occupancy_mirror.clone().expect("checked above");
            return self.ack_occupancy_mirror(&record);
        }
        if stamp.command.expected_revision != current_revision {
            return self.reject_occupancy_offer(
                &stamp,
                OccupancyRejectReason::LocalStateConflict,
                current_revision,
            );
        }
        let update = OccupancyMirrorUpdate {
            occupancy_lease_id: stamp.occupancy_lease_id.clone(),
            fencing_token: stamp.occupancy_fencing_token,
            holder_user_id: Some(payload.holder_user_id.clone()),
            claim_request_id: Some(payload.claim_request_id.clone()),
            idle_expires_at: payload.idle_expires_at.clone(),
            acknowledged_at: now_rfc3339(),
        };
        match self.store.advance_occupancy_mirror(&update) {
            Ok(
                OccupancyMirrorAdvance::Advanced(record)
                | OccupancyMirrorAdvance::Unchanged(record),
            ) => self.ack_occupancy_mirror(&record),
            Err(error) if error.kind() == DeviceStoreErrorKind::Conflict => self
                .reject_occupancy_offer(
                    &stamp,
                    OccupancyRejectReason::StaleFencingToken,
                    current_revision,
                ),
            Err(error) => Err(DaemonError::Store(error)),
        }
    }

    fn ack_occupancy_mirror(&mut self, record: &OccupancyMirrorRecord) -> Result<(), DaemonError> {
        self.occupancy_mirror = Some(record.clone());
        self.enqueue(occupancy_ack_message(record))?;
        crate::apply_engine::report_recovered_applications(self)
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        self.status.occupancy_offers_acked += 1;
        Ok(())
    }

    fn reject_occupancy_offer(
        &mut self,
        stamp: &OccupancyCommandContext,
        reason: OccupancyRejectReason,
        current_revision: u64,
    ) -> Result<(), DaemonError> {
        self.enqueue(ClientToServerMessage::OccupancyRejected(
            ClientOccupancyRejectedPayload {
                occupancy: OccupancyCommandContext {
                    command: CommandContext {
                        expected_revision: current_revision,
                        idempotency_key: format!(
                            "occupancy-rejected-{}-{}-{}",
                            stamp.occupancy_lease_id,
                            stamp.occupancy_fencing_token,
                            reject_reason_slug(reason)
                        ),
                    },
                    occupancy_lease_id: stamp.occupancy_lease_id.clone(),
                    occupancy_fencing_token: stamp.occupancy_fencing_token,
                },
                reason,
            },
        ))?;
        self.status.occupancy_offers_rejected += 1;
        Ok(())
    }

    fn apply_occupancy_release(
        &mut self,
        payload: &ServerOccupancyReleasePayload,
        command_message_id: &str,
    ) -> Result<(), DaemonError> {
        let stamp = &payload.occupancy;
        let current_revision = self
            .occupancy_mirror
            .as_ref()
            .map_or(0, |mirror| mirror.mirror_revision);
        let guard = self.fencing_guard();
        let stamp_match = guard
            .check_stamp(&stamp.occupancy_lease_id, stamp.occupancy_fencing_token)
            .map(|mirror| (mirror.occupancy_lease_id.clone(), mirror.mirror_revision))
            .map_err(|rejection| (rejection, rejection.wire_error_code()));
        drop(guard);
        let ack = |daemon: &mut Self,
                   status: CommandAckStatus,
                   error: Option<ClientControlError>,
                   current_revision: u64| {
            daemon
                .enqueue(command_ack_message(
                    ClientControlMessageKind::OccupancyRelease,
                    command_message_id,
                    status,
                    Some(current_revision),
                    error,
                ))
                .map(|_| ())
        };
        let (mirror_lease, mirror_revision) = match stamp_match {
            Ok(matched) => matched,
            Err((rejection, code)) => {
                return ack(
                    self,
                    fencing_rejection_status(rejection),
                    Some(control_error(code, rejection)),
                    current_revision,
                );
            }
        };
        if stamp.command.expected_revision != mirror_revision {
            return ack(
                self,
                CommandAckStatus::RejectedRevisionConflict,
                Some(control_error(
                    ClientControlErrorCode::RevisionConflict,
                    FencingRejection::StaleFencingToken,
                )),
                mirror_revision,
            );
        }
        let intent = OccupancyReleaseIntentRecord {
            idempotency_key: stamp.command.idempotency_key.clone(),
            command_message_id: command_message_id.to_owned(),
            occupancy_lease_id: mirror_lease.clone(),
            fencing_token: stamp.occupancy_fencing_token,
            mode: payload.mode,
            affected_worker_sessions: self
                .store
                .count_lease_worker_sessions(&stamp.occupancy_lease_id)
                .map_err(DaemonError::Store)?,
            recorded_at: now_rfc3339(),
        };
        let outcome = self
            .store
            .record_occupancy_release_intent(&intent)
            .map_err(DaemonError::Store)?;
        let mut stopped = 0_u64;
        let status = match outcome {
            OccupancyReleaseIntentOutcome::Recorded(_) => {
                self.status.occupancy_release_intents_recorded += 1;
                CommandAckStatus::Accepted
            }
            OccupancyReleaseIntentOutcome::Duplicate(_) => CommandAckStatus::Duplicate,
        };
        if payload.mode == ClientOccupancyReleaseMode::CancelTasksAndRelease
            && (status == CommandAckStatus::Accepted || self.worker_supervisor.is_some())
        {
            stopped = self.stop_occupancy_workers(&mirror_lease)?;
        }
        self.status.workers_stopped_on_release += stopped;
        ack(self, status, None, mirror_revision)
    }

    fn apply_occupancy_force_fence(
        &mut self,
        payload: &ServerOccupancyForceFencePayload,
        command_message_id: &str,
    ) -> Result<(), DaemonError> {
        let stamp = &payload.occupancy;
        let ack = |daemon: &mut Self,
                   status: CommandAckStatus,
                   error: Option<ClientControlError>,
                   current_revision: u64| {
            daemon
                .enqueue(command_ack_message(
                    ClientControlMessageKind::OccupancyForceFence,
                    command_message_id,
                    status,
                    Some(current_revision),
                    error,
                ))
                .map(|_| ())
        };
        if self
            .occupancy_mirror
            .as_ref()
            .is_some_and(|mirror| mirror_is_exact(mirror, stamp))
        {
            let revision = self
                .occupancy_mirror
                .as_ref()
                .map_or(0, |mirror| mirror.mirror_revision);
            self.stop_fenced_workers(&stamp.occupancy_lease_id, stamp.occupancy_fencing_token)?;
            return ack(self, CommandAckStatus::Duplicate, None, revision);
        }
        let current_revision = self
            .occupancy_mirror
            .as_ref()
            .map_or(0, |mirror| mirror.mirror_revision);
        if stamp.command.expected_revision != current_revision {
            return ack(
                self,
                CommandAckStatus::RejectedRevisionConflict,
                Some(control_error(
                    ClientControlErrorCode::RevisionConflict,
                    FencingRejection::StaleFencingToken,
                )),
                current_revision,
            );
        }
        let (holder, claim, idle) = match self.occupancy_mirror.as_ref() {
            Some(mirror) if mirror.occupancy_lease_id == stamp.occupancy_lease_id => (
                mirror.holder_user_id.clone(),
                mirror.claim_request_id.clone(),
                mirror.idle_expires_at.clone(),
            ),
            _ => (None, None, None),
        };
        let update = OccupancyMirrorUpdate {
            occupancy_lease_id: stamp.occupancy_lease_id.clone(),
            fencing_token: stamp.occupancy_fencing_token,
            holder_user_id: holder,
            claim_request_id: claim,
            idle_expires_at: idle,
            acknowledged_at: now_rfc3339(),
        };
        match self.store.advance_occupancy_mirror(&update) {
            Ok(OccupancyMirrorAdvance::Advanced(record)) => {
                self.occupancy_mirror = Some(record.clone());
                self.status.occupancy_force_fences_applied += 1;
                self.stop_fenced_workers(&stamp.occupancy_lease_id, stamp.occupancy_fencing_token)?;
                ack(
                    self,
                    CommandAckStatus::Accepted,
                    None,
                    record.mirror_revision,
                )
            }
            Ok(OccupancyMirrorAdvance::Unchanged(record)) => {
                self.occupancy_mirror = Some(record);
                ack(self, CommandAckStatus::Duplicate, None, current_revision)
            }
            Err(error) if error.kind() == DeviceStoreErrorKind::Conflict => ack(
                self,
                CommandAckStatus::RejectedStaleFencingToken,
                Some(control_error(
                    ClientControlErrorCode::StaleFencingToken,
                    FencingRejection::StaleFencingToken,
                )),
                current_revision,
            ),
            Err(error) => Err(DaemonError::Store(error)),
        }
    }

    fn apply_managed_app_command(
        &mut self,
        payload: &winwincode_client_port::messages::ServerManagedAppCommandPayload,
        command_message_id: &str,
    ) -> Result<(), DaemonError> {
        let command: &ManagedAppCommand = &payload.command;
        let current_revision = self
            .occupancy_mirror
            .as_ref()
            .map_or(0, |mirror| mirror.mirror_revision);
        let ticket = match self.fencing_guard().authorize_command(
            FencedCommandKind::ManagedApp,
            &command.occupancy_lease_id,
            command.occupancy_fencing_token,
        ) {
            FencingVerdict::Authorized(ticket) => ticket,
            FencingVerdict::Rejected(rejection) => {
                self.enqueue(command_ack_message(
                    ClientControlMessageKind::ManagedAppCommand,
                    command_message_id,
                    fencing_rejection_status(rejection),
                    Some(current_revision),
                    Some(control_error(rejection.wire_error_code(), rejection)),
                ))?;
                return Ok(());
            }
        };
        self.fencing_guard()
            .verify_ticket(&ticket)
            .map_err(|rejection| {
                DaemonError::Protocol(format!("managed app fencing changed: {rejection:?}"))
            })?;
        let Some(supervisor) = self.managed_app_supervisor.clone() else {
            self.enqueue(command_ack_message(
                ClientControlMessageKind::ManagedAppCommand,
                command_message_id,
                CommandAckStatus::RejectedWrongState,
                Some(current_revision),
                Some(stop_error(
                    ClientControlErrorCode::WrongState,
                    "no managed application supervisor is wired",
                    false,
                )),
            ))?;
            return Ok(());
        };
        let repository = self.managed_app_repository.as_ref().and_then(|source| {
            source.repository_root(
                command
                    .config
                    .as_ref()
                    .map_or(command.run_id.as_str(), |config| {
                        config.repository_binding_id.as_str()
                    }),
            )
        });
        let root = match command.operation {
            ManagedAppOperation::Start | ManagedAppOperation::Restart => {
                let Some(root) = repository.as_deref() else {
                    self.enqueue(command_ack_message(
                        ClientControlMessageKind::ManagedAppCommand,
                        command_message_id,
                        CommandAckStatus::RejectedWrongState,
                        Some(current_revision),
                        Some(stop_error(
                            ClientControlErrorCode::RepositoryUnavailable,
                            "managed application repository is unavailable",
                            false,
                        )),
                    ))?;
                    return Ok(());
                };
                root
            }
            ManagedAppOperation::Stop | ManagedAppOperation::Query => Path::new("."),
        };
        match supervisor.apply(command, root) {
            Ok(status) => {
                self.enqueue(ClientToServerMessage::ManagedAppStatus(
                    ClientManagedAppStatusPayload {
                        command_message_id: command_message_id.to_owned(),
                        status,
                    },
                ))?;
                Ok(())
            }
            Err(error) => {
                self.enqueue(command_ack_message(
                    ClientControlMessageKind::ManagedAppCommand,
                    command_message_id,
                    managed_app_error_status(&error),
                    Some(current_revision),
                    Some(stop_error(
                        ClientControlErrorCode::WrongState,
                        "managed application command was refused",
                        false,
                    )),
                ))?;
                Ok(())
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply_worker_launch(
        &mut self,
        payload: &ServerWorkerLaunchPayload,
    ) -> Result<(), DaemonError> {
        if let Some(bytes) = self
            .store
            .worker_launch_receipt(&payload.launch_grant.worker_launch_grant_id)?
        {
            let stored: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| DaemonError::Protocol("Worker launch receipt is corrupt".into()))?;
            if stored.get("launch")
                != Some(
                    &serde_json::to_value(payload).map_err(|_| {
                        DaemonError::Protocol("Worker launch cannot be encoded".into())
                    })?,
                )
            {
                return Err(DaemonError::Protocol("Worker launch replay differs".into()));
            }
            let ack = decode_worker_launch_receipt_ack(&stored)?;
            if let ClientToServerMessage::WorkerLaunchAck(payload) = &ack {
                if matches!(
                    payload.status,
                    WorkerLaunchAckStatus::Accepted | WorkerLaunchAckStatus::Duplicate
                ) {
                    self.status.worker_launches_accepted += 1;
                } else {
                    self.status.worker_launches_rejected += 1;
                }
            }
            self.enqueue(ack)?;
            return Ok(());
        }
        let stamp = &payload.occupancy;
        let grant = &payload.launch_grant;
        let current_revision = self
            .occupancy_mirror
            .as_ref()
            .map_or(0, |mirror| mirror.mirror_revision);

        let guard = self.fencing_guard();
        let ticket = match guard.authorize_command(
            FencedCommandKind::WorkerLaunch,
            &stamp.occupancy_lease_id,
            stamp.occupancy_fencing_token,
        ) {
            FencingVerdict::Authorized(ticket) => ticket,
            FencingVerdict::Rejected(rejection) => {
                drop(guard);
                return self.enqueue_launch_ack(
                    stamp,
                    grant,
                    fencing_launch_status(rejection),
                    current_revision,
                    Some(control_error(rejection.wire_error_code(), rejection)),
                    None,
                );
            }
        };
        drop(guard);

        if grant.client_node_id != self.node_id || grant.client_instance_id != self.instance_id {
            return self.enqueue_launch_ack(
                stamp,
                grant,
                WorkerLaunchAckStatus::RejectedWrongState,
                current_revision,
                Some(ClientControlError {
                    code: ClientControlErrorCode::DeviceInstanceChanged,
                    message: "the launch grant names another device instance".to_owned(),
                    retryable: false,
                }),
                None,
            );
        }

        let rejection = |daemon: &mut Self,
                         status: WorkerLaunchAckStatus,
                         code: ClientControlErrorCode,
                         message: String,
                         retryable: bool| {
            daemon.enqueue_launch_ack(
                stamp,
                grant,
                status,
                current_revision,
                Some(ClientControlError {
                    code,
                    message,
                    retryable,
                }),
                None,
            )
        };
        if grant.state != WorkerLaunchGrantState::Issued {
            return rejection(
                self,
                WorkerLaunchAckStatus::RejectedWrongState,
                ClientControlErrorCode::GrantInvalid,
                "the launch grant is not issued".into(),
                false,
            );
        }
        let Some(supervisor) = self.worker_supervisor.clone() else {
            return rejection(
                self,
                WorkerLaunchAckStatus::RejectedWrongState,
                ClientControlErrorCode::InternalError,
                "no local worker supervisor is wired".to_owned(),
                false,
            );
        };
        if grant.expires_at <= now_rfc3339() {
            if let Some(record) = self.store.worker_process(&grant.worker_session_id)? {
                if record.worker_id != grant.worker_id
                    || record.worker_instance_id != grant.worker_instance_id
                    || record.launch_grant_id != grant.worker_launch_grant_id
                    || record.occupancy_lease_id != stamp.occupancy_lease_id
                {
                    return Err(DaemonError::Protocol(
                        "expired launch process identity differs".into(),
                    ));
                }
                if record.state == crate::supervisor::WORKER_STATE_RUNNING {
                    supervisor
                        .stop(&grant.worker_session_id, false)
                        .map_err(|_| {
                            DaemonError::Protocol(
                                "expired launch process could not be stopped".into(),
                            )
                        })?;
                }
            }
            return rejection(
                self,
                WorkerLaunchAckStatus::RejectedWrongState,
                ClientControlErrorCode::GrantInvalid,
                "the launch grant expired before its launch decision was retained".into(),
                false,
            );
        }
        let Some(material) = self.worker_launch_material.as_ref() else {
            return rejection(
                self,
                WorkerLaunchAckStatus::RejectedWrongState,
                ClientControlErrorCode::GrantInvalid,
                "no local worker launch material source is wired".to_owned(),
                false,
            );
        };
        let Some(directories) =
            material.launch_directories(&grant.worker_session_id, &grant.repository_binding_id)
        else {
            return rejection(
                self,
                WorkerLaunchAckStatus::RejectedWrongState,
                ClientControlErrorCode::GrantInvalid,
                "no local worker directories are available for the launch".to_owned(),
                false,
            );
        };
        let spawn_request = SpawnRequest {
            worker_session_id: &grant.worker_session_id,
            worker_id: &grant.worker_id,
            worker_instance_id: &grant.worker_instance_id,
            occupancy_lease_id: &stamp.occupancy_lease_id,
            occupancy_fencing_token: stamp.occupancy_fencing_token,
            repository_binding_id: &grant.repository_binding_id,
            launch_grant_id: &grant.worker_launch_grant_id,
            product_session_id: non_empty(&grant.product_session_id),
            work_run_id: grant.work_run_id.as_deref().and_then(non_empty),
            source_directory: &directories.source_directory,
            data_directory: &directories.data_directory,
            worker_root: &directories.worker_root,
        };
        let outcome = supervisor.spawn(spawn_request);
        match outcome {
            Ok(spawn_outcome) => {
                let (status, worker_id, worker_instance_id) = match spawn_outcome {
                    SpawnOutcome::Started(handle) => (
                        WorkerLaunchAckStatus::Accepted,
                        handle.worker_id,
                        handle.worker_instance_id,
                    ),
                    SpawnOutcome::AlreadyRunning(record) => (
                        WorkerLaunchAckStatus::Duplicate,
                        record.worker_id,
                        record.worker_instance_id,
                    ),
                };
                self.enqueue_launch_ack(
                    stamp,
                    grant,
                    status,
                    ticket.mirror_revision,
                    None,
                    Some((&worker_id, &worker_instance_id)),
                )
            }
            Err(error) => {
                let (status, code, retryable) = launch_spawn_rejection(&error);
                rejection(self, status, code, error.to_string(), retryable)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply_worker_stop(
        &mut self,
        payload: &ServerWorkerStopPayload,
        command_message_id: &str,
    ) -> Result<(), WorkerStopFailure> {
        let stamp = &payload.occupancy;
        let intent = WorkerStopIntentRecord {
            command_message_id: command_message_id.to_owned(),
            idempotency_key: stamp.command.idempotency_key.clone(),
            worker_session_id: payload.worker_session_id.clone(),
            worker_id: payload.worker_id.clone(),
            reason: payload.reason,
            occupancy_lease_id: stamp.occupancy_lease_id.clone(),
            fencing_token: stamp.occupancy_fencing_token,
            expected_revision: stamp.command.expected_revision,
            receipt_payload: None,
            recorded_at: now_rfc3339(),
            completed_at: None,
        };
        let existing = self
            .store
            .worker_stop_intent(command_message_id)
            .map_err(|error| WorkerStopFailure::Fatal(DaemonError::Store(error)))?
            .or(self
                .store
                .worker_stop_intent_by_idempotency(&intent.idempotency_key)
                .map_err(|error| WorkerStopFailure::Fatal(DaemonError::Store(error)))?);
        if existing.is_none() {
            let current_revision = self
                .occupancy_mirror
                .as_ref()
                .map_or(0, |mirror| mirror.mirror_revision);
            let guard = self.fencing_guard();
            match guard.authorize_command(
                FencedCommandKind::WorkerStop,
                &stamp.occupancy_lease_id,
                stamp.occupancy_fencing_token,
            ) {
                FencingVerdict::Authorized(ticket)
                    if stamp.command.expected_revision == ticket.mirror_revision => {}
                FencingVerdict::Rejected(rejection) => {
                    return self
                        .enqueue(command_ack_message(
                            ClientControlMessageKind::WorkerStop,
                            command_message_id,
                            fencing_rejection_status(rejection),
                            Some(current_revision),
                            Some(control_error(rejection.wire_error_code(), rejection)),
                        ))
                        .map(|_| ())
                        .map_err(WorkerStopFailure::Fatal);
                }
                FencingVerdict::Authorized(_ticket) => {
                    return self
                        .enqueue(command_ack_message(
                            ClientControlMessageKind::WorkerStop,
                            command_message_id,
                            CommandAckStatus::RejectedRevisionConflict,
                            Some(current_revision),
                            Some(stop_error(
                                ClientControlErrorCode::RevisionConflict,
                                "the worker stop expected revision is not current",
                                false,
                            )),
                        ))
                        .map(|_| ())
                        .map_err(WorkerStopFailure::Fatal);
                }
            }
        }
        let stored = match self
            .store
            .record_worker_stop_intent(&intent)
            .map_err(|error| WorkerStopFailure::Fatal(DaemonError::Store(error)))?
        {
            WorkerStopIntentOutcome::Recorded(record)
            | WorkerStopIntentOutcome::Duplicate(record) => record,
        };
        let durable_command_message_id = stored.command_message_id.clone();
        if let Some(receipt_payload) = stored.receipt_payload {
            let receipt: ClientToServerMessage =
                serde_json::from_slice(&receipt_payload).map_err(|error| {
                    WorkerStopFailure::Fatal(DaemonError::Protocol(format!(
                        "worker stop receipt for {command_message_id} is invalid: {error}"
                    )))
                })?;
            self.enqueue(receipt).map_err(WorkerStopFailure::Fatal)?;
            return Ok(());
        }

        let current_revision = self
            .occupancy_mirror
            .as_ref()
            .map_or(0, |mirror| mirror.mirror_revision);
        let result = if existing.is_some() {
            self.worker_stop_recovery_result(payload)?
        } else {
            self.worker_stop_result(payload)?
        };
        let receipt = command_ack_message(
            ClientControlMessageKind::WorkerStop,
            &durable_command_message_id,
            result.0,
            Some(current_revision),
            result.1,
        );
        let receipt_payload = serde_json::to_vec(&receipt).map_err(|error| {
            WorkerStopFailure::Fatal(DaemonError::Protocol(format!(
                "worker stop receipt is not encodable: {error}"
            )))
        })?;
        self.store
            .complete_worker_stop_intent(
                &durable_command_message_id,
                &receipt_payload,
                &now_rfc3339(),
            )
            .map_err(|error| WorkerStopFailure::Fatal(DaemonError::Store(error)))?;
        self.enqueue(receipt).map_err(WorkerStopFailure::Fatal)?;
        Ok(())
    }

    fn worker_stop_recovery_result(
        &self,
        payload: &ServerWorkerStopPayload,
    ) -> Result<(CommandAckStatus, Option<ClientControlError>), WorkerStopFailure> {
        let Some(supervisor) = self.worker_supervisor.clone() else {
            return Err(WorkerStopFailure::Retryable(
                "worker stop recovery requires a wired supervisor; retry".to_owned(),
            ));
        };
        match supervisor.stop_recovered(
            &payload.worker_session_id,
            &payload.worker_id,
            &payload.occupancy.occupancy_lease_id,
            true,
        ) {
            Ok(_) => Ok((CommandAckStatus::Accepted, None)),
            Err(error @ (SupervisorError::NotFound { .. } | SupervisorError::InvalidInput(_))) => {
                Ok((
                    CommandAckStatus::RejectedWrongState,
                    Some(stop_error(
                        ClientControlErrorCode::WrongState,
                        &error.to_string(),
                        false,
                    )),
                ))
            }
            Err(error) => Err(worker_stop_supervisor_failure(
                error,
                "worker stop recovery must retry",
            )),
        }
    }

    fn worker_stop_result(
        &self,
        payload: &ServerWorkerStopPayload,
    ) -> Result<(CommandAckStatus, Option<ClientControlError>), WorkerStopFailure> {
        let Some(supervisor) = self.worker_supervisor.clone() else {
            return Err(WorkerStopFailure::Retryable(
                "worker stop requires a wired supervisor; retry".to_owned(),
            ));
        };
        let Some(record) = supervisor
            .worker_process(&payload.worker_session_id)
            .map_err(|error| {
                worker_stop_supervisor_failure(error, "worker stop lookup must retry")
            })?
        else {
            return Ok((
                CommandAckStatus::RejectedWrongState,
                Some(stop_error(
                    ClientControlErrorCode::WrongState,
                    "worker session has no local process record",
                    false,
                )),
            ));
        };
        if record.worker_id != payload.worker_id {
            return Ok((
                CommandAckStatus::RejectedWrongState,
                Some(stop_error(
                    ClientControlErrorCode::WrongState,
                    "worker session and worker identity do not match",
                    false,
                )),
            ));
        }
        let status = if record.state == crate::supervisor::WORKER_STATE_RUNNING {
            CommandAckStatus::Accepted
        } else {
            CommandAckStatus::Duplicate
        };
        let result = supervisor.stop(&payload.worker_session_id, true);
        match result {
            Ok(_) => Ok((status, None)),
            Err(SupervisorError::Fencing(rejection)) => Ok((
                fencing_rejection_status(rejection),
                Some(control_error(rejection.wire_error_code(), rejection)),
            )),
            Err(error @ (SupervisorError::NotFound { .. } | SupervisorError::InvalidInput(_))) => {
                Ok((
                    CommandAckStatus::RejectedWrongState,
                    Some(stop_error(
                        ClientControlErrorCode::WrongState,
                        &error.to_string(),
                        false,
                    )),
                ))
            }
            Err(error) => Err(worker_stop_supervisor_failure(
                error,
                "worker stop must retry",
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_closure_for_record(
        &self,
        record: &crate::store::WorkerProcessRecord,
    ) -> Option<winwincode_client_port::messages::ClientWorkerProcessClosure> {
        worker_process_closure(record, &self.instance_id)
    }

    fn enqueue_launch_ack(
        &mut self,
        stamp: &OccupancyCommandContext,
        grant: &WorkerLaunchGrant,
        status: WorkerLaunchAckStatus,
        expected_revision: u64,
        error: Option<ClientControlError>,
        worker_identity: Option<(&str, &str)>,
    ) -> Result<(), DaemonError> {
        let (worker_id, worker_instance_id) =
            worker_identity.unwrap_or((&grant.worker_id, &grant.worker_instance_id));
        let closure = if matches!(
            status,
            WorkerLaunchAckStatus::Accepted | WorkerLaunchAckStatus::Duplicate
        ) {
            None
        } else {
            match self.store.worker_process(&grant.worker_session_id)? {
                Some(record)
                    if record.launch_grant_id == grant.worker_launch_grant_id
                        && record.worker_instance_id == grant.worker_instance_id =>
                {
                    self.process_closure_for_record(&record)
                }
                Some(_) => None,
                None => Some(
                    winwincode_client_port::messages::ClientWorkerProcessClosure {
                        worker_launch_grant_id: grant.worker_launch_grant_id.clone(),
                        client_instance_id: grant.client_instance_id.clone(),
                        reporting_client_instance_id: self.instance_id.clone(),
                        occupancy_fencing_token: grant.occupancy_fencing_token,
                        never_started: true,
                        process_boot_digest: None,
                    },
                ),
            }
        };
        let ack = ClientToServerMessage::WorkerLaunchAck(Box::new(ClientWorkerLaunchAckPayload {
            process_closure: closure,
            occupancy: OccupancyCommandContext {
                command: CommandContext {
                    expected_revision,
                    idempotency_key: format!("worker-launch-ack-{}", grant.worker_launch_grant_id),
                },
                occupancy_lease_id: stamp.occupancy_lease_id.clone(),
                occupancy_fencing_token: stamp.occupancy_fencing_token,
            },
            worker_launch_grant_id: grant.worker_launch_grant_id.clone(),
            worker_session_id: grant.worker_session_id.clone(),
            worker_id: worker_id.to_owned(),
            worker_instance_id: worker_instance_id.to_owned(),
            status,
            error,
        }));
        let launch = ServerWorkerLaunchPayload {
            occupancy: stamp.clone(),
            launch_grant: grant.clone(),
        };
        let receipt = serde_json::to_vec(&serde_json::json!({"launch":launch,"ack":ack}))
            .map_err(|_| DaemonError::Protocol("Worker launch receipt cannot be encoded".into()))?;
        self.store.retain_worker_launch_receipt(
            &grant.worker_launch_grant_id,
            &grant.worker_session_id,
            &format!("{status:?}"),
            &stamp.command.idempotency_key,
            &receipt,
            &now_rfc3339(),
        )?;
        self.enqueue(ack)?;
        if matches!(
            status,
            WorkerLaunchAckStatus::Accepted | WorkerLaunchAckStatus::Duplicate
        ) {
            self.status.worker_launches_accepted += 1;
        } else {
            self.status.worker_launches_rejected += 1;
        }
        Ok(())
    }

    fn restore_downlink_cursor(&mut self) -> Result<(), DaemonError> {
        self.downlink = match self.store.inbox_cursor(&self.config.server_profile_id)? {
            Some(cursor) => AckCursor::from_ack(cursor.last_sequence),
            None => AckCursor::new(),
        };
        self.status.downlink_accepted_through = self.downlink.ack_sequence();
        Ok(())
    }

    fn frame_message_id(&self, kind: &str, sequence: u64) -> String {
        format!("{}-{kind}-{sequence}", self.node_id)
    }

    fn register_failure(&mut self, now: Instant, reason: String) {
        self.consecutive_failures += 1;
        let backoff = backoff_for(
            self.consecutive_failures,
            self.config.initial_backoff,
            self.config.max_backoff,
        );
        self.next_attempt_at = now + backoff;
        self.status.consecutive_failures = self.consecutive_failures;
        self.status.current_backoff = backoff;
        self.status.last_error = Some(reason);
    }

    fn reset_backoff(&mut self) {
        self.consecutive_failures = 0;
        self.next_attempt_at = Instant::now();
        self.status.consecutive_failures = 0;
        self.status.current_backoff = Duration::ZERO;
    }

    fn retry_outcome(&self) -> TickOutcome {
        TickOutcome::Retrying {
            after: self.status.current_backoff,
            reason: self.status.last_error.clone().unwrap_or_default(),
        }
    }

    fn idle_ready_in(&self, now: Instant) -> Duration {
        self.next_heartbeat_at.saturating_duration_since(now)
    }

    fn sleep_interruptibly(budget: Duration, shutdown: &AtomicBool) {
        let deadline = Instant::now() + budget;
        while !shutdown.load(Ordering::Relaxed) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            thread::sleep(remaining.min(WAKE_SLICE));
        }
    }
}

enum DownlinkFailure {
    Retriable(String),
    Fatal(DaemonError),
}

enum WorkerStopFailure {
    Retryable(String),
    Fatal(DaemonError),
}

impl From<WorkerStopFailure> for DaemonError {
    fn from(error: WorkerStopFailure) -> Self {
        match error {
            WorkerStopFailure::Retryable(reason) => Self::Protocol(reason),
            WorkerStopFailure::Fatal(error) => error,
        }
    }
}

fn worker_stop_failure_to_downlink(error: WorkerStopFailure) -> DownlinkFailure {
    match error {
        WorkerStopFailure::Retryable(reason) => DownlinkFailure::Retriable(reason),
        WorkerStopFailure::Fatal(error) => DownlinkFailure::Fatal(error),
    }
}

fn worker_stop_supervisor_failure(
    error: SupervisorError,
    retry_message: &str,
) -> WorkerStopFailure {
    match error {
        SupervisorError::Store(error) => WorkerStopFailure::Fatal(DaemonError::Store(error)),
        error => WorkerStopFailure::Retryable(format!("{retry_message}: {error}")),
    }
}

fn map_connect_code_error(error: connect_code::ConnectCodeError) -> DaemonError {
    match error {
        connect_code::ConnectCodeError::Store(store) => DaemonError::Store(store),
        connect_code::ConnectCodeError::NotEnrolled
        | connect_code::ConnectCodeError::Protocol(_) => DaemonError::Protocol(error.to_string()),
    }
}

fn decode_downlink_frame(
    codec: &FrameCodec,
    value: &serde_json::Value,
) -> Result<ServerToClientEnvelope, String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("downlink frame is not encodable: {error}"))?;
    let envelope: ServerToClientEnvelope = codec
        .decode_envelope(&bytes)
        .map_err(|error| format!("downlink frame is not a valid envelope: {error:?}"))?;
    if envelope.schema_version != CLIENT_CONTROL_PORT_SCHEMA_VERSION {
        return Err(format!(
            "downlink frame names schema version {} instead of \
             {CLIENT_CONTROL_PORT_SCHEMA_VERSION}",
            envelope.schema_version
        ));
    }
    Ok(envelope)
}

fn retriable_ack_error(error: OutboxError<DeviceStoreError>) -> Result<String, DaemonError> {
    match error {
        OutboxError::AckAhead { requested, highest } => Ok(format!(
            "server acknowledged sequence {requested} beyond the retained high-water mark {highest}"
        )),
        OutboxError::AckRegression {
            requested,
            acknowledged,
        } => Ok(format!(
            "server acknowledgement regressed to {requested} below the durable {acknowledged}"
        )),
        other => Err(fatal_outbox(other)),
    }
}

fn fatal_outbox(error: OutboxError<DeviceStoreError>) -> DaemonError {
    match error {
        OutboxError::CorruptState(state) => DaemonError::CorruptOutbox(state),
        OutboxError::Store(store) => DaemonError::Store(store),
        other => DaemonError::Protocol(format!(
            "outbox state machine rejected the operation: {other:?}"
        )),
    }
}

fn backoff_for(failures: u64, initial_backoff: Duration, max_backoff: Duration) -> Duration {
    winwincode_network::exponential_delay(
        u32::try_from(failures).unwrap_or(u32::MAX),
        initial_backoff,
        max_backoff,
    )
}

fn validate_config(config: &DaemonConfig) -> Result<(), DaemonError> {
    let check = |value: &str, label: &str, max: usize| {
        if value.is_empty() {
            return Err(DaemonError::Config(format!("{label} must not be empty")));
        }
        if value.len() > max {
            return Err(DaemonError::Config(format!(
                "{label} must contain at most {max} bytes"
            )));
        }
        Ok(())
    };
    check(&config.server_profile_id, "server profile id", 200)?;
    check(&config.base_url, "server base url", 2048)?;
    check(&config.device_display_name, "device display name", 200)?;
    check(&config.client_version, "client version", 200)?;
    if config.max_frames_per_exchange == 0 {
        return Err(DaemonError::Config(
            "max frames per exchange must be positive".to_owned(),
        ));
    }
    for (duration, label) in [
        (config.heartbeat_interval, "heartbeat interval"),
        (config.enroll_poll_interval, "enroll poll interval"),
        (config.initial_backoff, "initial backoff"),
        (config.max_backoff, "max backoff"),
    ] {
        if duration.is_zero() {
            return Err(DaemonError::Config(format!("{label} must be positive")));
        }
    }
    if config.max_backoff < config.initial_backoff {
        return Err(DaemonError::Config(
            "max backoff must not be below the initial backoff".to_owned(),
        ));
    }
    Ok(())
}

fn now_rfc3339() -> String {
    crate::canonical_rfc3339(OffsetDateTime::now_utc())
}

fn mirror_is_exact(mirror: &OccupancyMirrorRecord, stamp: &OccupancyCommandContext) -> bool {
    mirror.occupancy_lease_id == stamp.occupancy_lease_id
        && mirror.fencing_token == stamp.occupancy_fencing_token
}

fn occupancy_ack_message(record: &OccupancyMirrorRecord) -> ClientToServerMessage {
    ClientToServerMessage::OccupancyAck(ClientOccupancyAckPayload {
        occupancy: OccupancyCommandContext {
            command: CommandContext {
                expected_revision: record.mirror_revision,
                idempotency_key: format!(
                    "occupancy-ack-{}-{}",
                    record.occupancy_lease_id, record.fencing_token
                ),
            },
            occupancy_lease_id: record.occupancy_lease_id.clone(),
            occupancy_fencing_token: record.fencing_token,
        },
    })
}

const fn reject_reason_slug(reason: OccupancyRejectReason) -> &'static str {
    match reason {
        OccupancyRejectReason::UnknownLease => "unknown-lease",
        OccupancyRejectReason::StaleFencingToken => "stale-fencing-token",
        OccupancyRejectReason::LocalStateConflict => "local-state-conflict",
        OccupancyRejectReason::ClientLocked => "client-locked",
        OccupancyRejectReason::CapacityExhausted => "capacity-exhausted",
    }
}

const fn fencing_rejection_status(rejection: FencingRejection) -> CommandAckStatus {
    match rejection {
        FencingRejection::MirrorNotSet => CommandAckStatus::RejectedLeaseMismatch,
        FencingRejection::StaleFencingToken | FencingRejection::SupersededIntent => {
            CommandAckStatus::RejectedStaleFencingToken
        }
    }
}

fn decode_worker_launch_receipt_ack(
    stored: &serde_json::Value,
) -> Result<ClientToServerMessage, DaemonError> {
    let corrupt = || DaemonError::Protocol("Worker launch receipt is corrupt".into());
    let mut ack = stored["ack"].clone();
    if let Some(token) = ack.pointer_mut("/payload/processClosure/occupancyFencingToken")
        && token.is_number()
    {
        let value = token
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(corrupt)?;
        *token = serde_json::Value::String(value.to_string());
    }
    let ack = serde_json::from_value(ack).map_err(|_| corrupt())?;
    if !matches!(ack, ClientToServerMessage::WorkerLaunchAck(_)) {
        return Err(corrupt());
    }
    Ok(ack)
}

fn stop_error(code: ClientControlErrorCode, message: &str, retryable: bool) -> ClientControlError {
    ClientControlError {
        code,
        message: message.to_owned(),
        retryable,
    }
}

fn control_error(code: ClientControlErrorCode, rejection: FencingRejection) -> ClientControlError {
    let (message, retryable) = match rejection {
        FencingRejection::MirrorNotSet => {
            ("the device holds no occupancy mirror".to_owned(), false)
        }
        FencingRejection::StaleFencingToken => (
            "the occupancy fencing token is not the current mirror stamp".to_owned(),
            false,
        ),
        FencingRejection::SupersededIntent => (
            "the mirror advanced after this intent was authorized".to_owned(),
            true,
        ),
    };
    ClientControlError {
        code,
        message,
        retryable,
    }
}

fn command_ack_message(
    command_kind: ClientControlMessageKind,
    command_message_id: &str,
    status: CommandAckStatus,
    current_revision: Option<u64>,
    error: Option<ClientControlError>,
) -> ClientToServerMessage {
    ClientToServerMessage::CommandAck(ClientCommandAckPayload {
        command_kind,
        command_message_id: command_message_id.to_owned(),
        status,
        current_revision,
        error,
    })
}

const fn managed_app_error_status(error: &crate::managed_app::ManagedAppError) -> CommandAckStatus {
    match error {
        crate::managed_app::ManagedAppError::StaleLease => {
            CommandAckStatus::RejectedStaleFencingToken
        }
        crate::managed_app::ManagedAppError::LeaseMismatch => {
            CommandAckStatus::RejectedLeaseMismatch
        }
        _ => CommandAckStatus::RejectedWrongState,
    }
}

const fn fencing_launch_status(rejection: FencingRejection) -> WorkerLaunchAckStatus {
    match rejection {
        FencingRejection::MirrorNotSet => WorkerLaunchAckStatus::RejectedLeaseMismatch,
        FencingRejection::StaleFencingToken | FencingRejection::SupersededIntent => {
            WorkerLaunchAckStatus::RejectedStaleFencingToken
        }
    }
}

fn launch_spawn_rejection(
    error: &SupervisorError,
) -> (WorkerLaunchAckStatus, ClientControlErrorCode, bool) {
    match error {
        SupervisorError::Fencing(rejection) => (
            fencing_launch_status(*rejection),
            rejection.wire_error_code(),
            false,
        ),
        SupervisorError::CapacityExhausted { .. } => (
            WorkerLaunchAckStatus::RejectedCapacityExhausted,
            ClientControlErrorCode::CapacityExhausted,
            true,
        ),
        _ => (
            WorkerLaunchAckStatus::RejectedWrongState,
            ClientControlErrorCode::InternalError,
            false,
        ),
    }
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

enum ConfigurationReceipt {
    Provider(winwincode_api::generated::DeviceProviderReceipt),
    Extension(winwincode_api::generated::DeviceExtensionReceipt),
}
struct PendingConfigurationApply {
    extension: bool,
    envelope: winwincode_api::generated::DeviceConfigurationEnvelope,
    worker:
        thread::JoinHandle<Result<ConfigurationReceipt, winwincode_provider::DeviceProviderError>>,
}

pub(crate) fn worker_process_closure(
    record: &crate::store::WorkerProcessRecord,
    reporting_instance: &str,
) -> Option<winwincode_client_port::messages::ClientWorkerProcessClosure> {
    if !matches!(record.state.as_str(), "exited" | "crashed" | "missing") {
        return None;
    }
    let config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(Path::new(&record.data_directory).join("managed-session.json")).ok()?,
    )
    .ok()?;
    if config["workerSessionId"] != record.worker_session_id
        || config["workerInstanceId"] != record.worker_instance_id
        || config["workerId"] != record.worker_id
        || config["occupancyLeaseId"] != record.occupancy_lease_id
    {
        return None;
    }
    let token = config["occupancyFencingToken"].as_str()?;
    let fencing_token = token.parse::<u64>().ok()?;
    if fencing_token == 0 || fencing_token.to_string() != token {
        return None;
    }
    Some(
        winwincode_client_port::messages::ClientWorkerProcessClosure {
            worker_launch_grant_id: record.launch_grant_id.clone(),
            client_instance_id: config["clientInstanceId"].as_str()?.to_owned(),
            reporting_client_instance_id: reporting_instance.to_owned(),
            occupancy_fencing_token: fencing_token,
            never_started: false,
            process_boot_digest: Some(format!(
                "sha256:{:x}",
                Sha256::digest(record.process_start_identity.as_bytes())
            )),
        },
    )
}

#[cfg(test)]
#[path = "mechanism_scheduler_regression_tests.rs"]
mod mechanism_scheduler_regression_tests;

#[cfg(test)]
mod receipt_compatibility_tests {
    use super::*;

    #[test]
    fn private_numeric_closure_receipt_replays_without_mutating_retained_bytes() {
        let ack = ClientToServerMessage::WorkerLaunchAck(Box::new(ClientWorkerLaunchAckPayload {
            occupancy: OccupancyCommandContext {
                command: CommandContext {
                    expected_revision: 1,
                    idempotency_key: "legacy-receipt".into(),
                },
                occupancy_lease_id: "ocl_00000000000000000000000001".into(),
                occupancy_fencing_token: u64::MAX,
            },
            worker_launch_grant_id: "wlg_00000000000000000000000001".into(),
            worker_session_id: "wsn_00000000000000000000000001".into(),
            worker_id: "wrk_00000000000000000000000001".into(),
            worker_instance_id: "wki_00000000000000000000000001".into(),
            status: WorkerLaunchAckStatus::RejectedCapacityExhausted,
            error: None,
            process_closure: Some(
                winwincode_client_port::messages::ClientWorkerProcessClosure {
                    worker_launch_grant_id: "wlg_00000000000000000000000001".into(),
                    client_instance_id: "cix_00000000000000000000000001".into(),
                    reporting_client_instance_id: "cix_00000000000000000000000002".into(),
                    occupancy_fencing_token: u64::MAX,
                    never_started: true,
                    process_boot_digest: None,
                },
            ),
        }));
        let mut stored = serde_json::json!({"ack":ack});
        *stored
            .pointer_mut("/ack/payload/processClosure/occupancyFencingToken")
            .unwrap() = serde_json::json!(u64::MAX);
        let original = serde_json::to_vec(&stored).unwrap();
        assert_eq!(decode_worker_launch_receipt_ack(&stored).unwrap(), ack);
        assert_eq!(serde_json::to_vec(&stored).unwrap(), original);
        let replay =
            serde_json::to_value(decode_worker_launch_receipt_ack(&stored).unwrap()).unwrap();
        assert_eq!(
            replay["payload"]["processClosure"]["occupancyFencingToken"],
            u64::MAX.to_string()
        );
        for invalid in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("01"),
        ] {
            let mut wrong = stored.clone();
            *wrong
                .pointer_mut("/ack/payload/processClosure/occupancyFencingToken")
                .unwrap() = invalid;
            assert!(decode_worker_launch_receipt_ack(&wrong).is_err());
        }
    }
}
