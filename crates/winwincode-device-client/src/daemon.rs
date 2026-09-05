// SPDX-License-Identifier: Apache-2.0

//! Device Client daemon exchange loop (plan sections 9.2 and 18.3).
//!
//! One [`DeviceDaemon`] owns one [`DeviceStore`] and periodically drives the
//! `POST /internal/v1/client/exchange` semantics over an injected
//! [`ExchangeTransport`]:
//!
//! 1. **Persist before send.** Every `client.enroll` / `client.hello` /
//!    `client.heartbeat` frame is encoded with the bounded [`FrameCodec`]
//!    and appended to the durable outbox (the store's
//!    [`FrameOutbox`](winwincode_client_port::exchange::FrameOutbox)
//!    adapter) before any transport call, so a crash can only ever lose an
//!    attempt, never a frame.
//! 2. **Bounded delivery.** Each exchange takes at most
//!    [`DaemonConfig::max_frames_per_exchange`] deliverable frames from the
//!    durable delivery cursor; remainders page through later exchanges.
//!    Empty batches are never sent: the endpoint rejects them, so an idle
//!    daemon waits for the heartbeat cadence instead of polling.
//! 3. **Acknowledgement.** The response's `ackSequence` is persisted through
//!    the outbox state machine and the confirmed prefix is compacted.
//! 4. **Gap replay.** A response carrying `replayFromSequence` moves the
//!    delivery cursor back to `replayFromSequence - 1`, so the next exchange
//!    re-delivers from there.
//! 5. **Enrollment adoption.** The enrollment exchange response carries the
//!    server-issued identity (`cnd_` `clientNodeId`, `publicClientId`, the
//!    one-time Device Credential material, and the requested heartbeat
//!    interval). The daemon backfills the persisted identity
//!    ([`adopt_enrollment`](crate::identity::adopt_enrollment)), re-keys the
//!    durable outbox stream onto the assigned node at the sequence the
//!    server credited, and presents the issued material as the bearer
//!    credential of every later exchange.
//! 6. **Backoff.** Transport failures, malformed responses, and protocol
//!    violations double the exchange backoff from
//!    [`DaemonConfig::initial_backoff`] up to
//!    [`DaemonConfig::max_backoff`]; any successful exchange resets it.
//! 7. **Lifecycle.** [`DeviceDaemon::run`] loops on a plain `std` thread
//!    with interruptible sleeps until an [`AtomicBool`] shutdown flag is
//!    observed. Shutdown never discards taken frames: frames leave the
//!    durable outbox only when their acknowledgement is persisted.
//!
//! Restart semantics: the durable identity decides the phase. Before the
//! enrollment is adopted the daemon sends `client.enroll` (once — the
//! durable enroll frame is redelivered, never re-enqueued) under the local
//! placeholder node id; after adoption the daemon is enrolled, announces
//! `client.hello` for the new launch instance as the first new frame, and
//! the server's instance guard keeps accepting the surviving frames of the
//! previous instance until the hello takes the instance over mid-batch.
//!
//! The [`ExchangeRequest`] / [`ExchangeResponse`] bodies are the endpoint's
//! canonical wire contract: the request carries `{schemaVersion, frames[],
//! ackSequence}`, the response carries `{schemaVersion, ackSequence,
//! replayFromSequence?, frames[], enrollment?}`. The HTTP implementation
//! lives behind [`ExchangeTransport`] and is injectable; the crate ships a
//! dependency-free std TCP implementation in [`crate::http`].

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientLockState, ClientPlatformTarget, PresenceState,
};
use winwincode_client_port::exchange::{
    AckCursor, FrameCodec, FrameCodecError, FrameOutbox, OutboxBatch, OutboxError, OutboxSession,
    OutboxSnapshot, OutboxStateError, SequenceVerdict,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientEnrollPayload, ClientHeartbeatPayload,
    ClientHelloPayload, ClientToServerEnvelope, ClientToServerMessage, CommandContext,
    ServerEnrollmentAcceptedPayload, ServerToClientEnvelope, ServerToClientMessage,
};

use crate::identity::{IdentityRecord, IssuedEnrollment, adopt_enrollment};
use crate::store::{
    ClientInboxCursorUpdate, DeviceStore, DeviceStoreError, DeviceStoreErrorKind,
    ServerProfileRecord, envelope_kind,
};

/// Longest slice the run loop sleeps without re-reading the shutdown flag.
const WAKE_SLICE: Duration = Duration::from_millis(20);

/// Failure of one exchange attempt at the transport boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeTransportError {
    message: String,
}

impl ExchangeTransportError {
    /// Builds one transport failure carrying a human-readable reason.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The failure reason.
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

/// The network boundary of the exchange loop.
///
/// The daemon composes one implementation per session and hands it the
/// encoded canonical request body together with the bearer credential to
/// present (`None` before the enrollment issued one). Tests and embedders
/// supply their own implementations; the crate ships the minimal std HTTP
/// implementation [`crate::HttpExchangeTransport`].
pub trait ExchangeTransport: Send + Sync {
    /// Sends one encoded exchange request and returns the encoded response.
    ///
    /// # Errors
    ///
    /// Returns a transport failure when the request cannot be delivered or
    /// no response can be received. The daemon answers every failure with
    /// exponential backoff and re-delivers the same durable frames.
    fn exchange(
        &self,
        credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError>;
}

/// Canonical exchange request body: a bounded batch of client-to-server
/// frames (each value is one serialized `ClientToServerEnvelope`) plus this
/// client's contiguous acknowledgement of the server-to-client stream. The
/// routing identity rides inside each frame envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeRequest {
    /// Wire contract schema version (`winwincode/v1`).
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    /// The bounded client-to-server frame batch.
    pub frames: Vec<serde_json::Value>,
    /// Highest server-to-client sequence this client consecutively accepted.
    #[serde(rename = "ackSequence")]
    pub ack_sequence: u64,
}

/// The one-time enrollment issuance carried only by the enrollment exchange
/// response. The raw credential material crosses the transport exactly once
/// and never enters a frame payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentIssuance {
    /// Server-assigned canonical `clientNodeId` (`cnd_` identity).
    #[serde(rename = "clientNodeId")]
    pub client_node_id: String,
    /// Server-assigned public `publicClientId`.
    #[serde(rename = "publicClientId")]
    pub public_client_id: String,
    /// Raw Device Credential material as lowercase hex of the 32 secret
    /// bytes; the server persists only `deviceCredentialDigest`.
    #[serde(rename = "deviceCredential")]
    pub device_credential: String,
    /// The persisted `sha256:` digest of the issued material.
    #[serde(rename = "deviceCredentialDigest")]
    pub device_credential_digest: String,
    /// Heartbeat interval the server profile asks this device to use.
    #[serde(rename = "heartbeatIntervalMs")]
    pub heartbeat_interval_ms: u32,
    /// Server timestamp the device should clock-drift against (RFC 3339).
    #[serde(rename = "serverTime")]
    pub server_time: String,
    /// First sequence of the server-to-client stream this device continues
    /// at.
    #[serde(rename = "downlinkFromSequence")]
    pub downlink_from_sequence: u64,
}

/// Canonical exchange response body: the server's acknowledgement of the
/// client batch (with the gap replay hint when present) plus a bounded
/// server-to-client frame batch and — only on the exchange that accepted a
/// fresh enrollment — the issued Device Credential material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeResponse {
    /// Wire contract schema version (`winwincode/v1`).
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    /// Highest client-to-server sequence the server consecutively accepted.
    #[serde(rename = "ackSequence")]
    pub ack_sequence: u64,
    /// First missing client-to-server sequence; the receiving cursor stays
    /// at `ackSequence` and the client must replay from here.
    #[serde(rename = "replayFromSequence", default)]
    pub replay_from_sequence: Option<u64>,
    /// The bounded server-to-client frame batch.
    #[serde(default)]
    pub frames: Vec<serde_json::Value>,
    /// The one-time enrollment issuance on the accepting exchange.
    #[serde(default)]
    pub enrollment: Option<EnrollmentIssuance>,
}

/// Static configuration of one daemon session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Server profile the daemon exchanges with; also the inbox-cursor key.
    pub server_profile_id: String,
    /// Exchange endpoint base URL.
    pub base_url: String,
    /// Human-readable server profile name persisted on enrollment.
    pub server_display_name: String,
    /// Requested device display name for `client.enroll`.
    pub device_display_name: String,
    /// Supported release target of this device.
    pub platform: ClientPlatformTarget,
    /// CPU architecture of this device.
    pub architecture: ClientArchitecture,
    /// Device client software version.
    pub client_version: String,
    /// Idle cadence for `client.heartbeat`; the enrollment acceptance may
    /// override it with the server-requested interval.
    pub heartbeat_interval: Duration,
    /// Cadence for the enrollment wait while no deliverable enroll frame
    /// exists (the endpoint rejects empty batches, so the wait never sends).
    pub enroll_poll_interval: Duration,
    /// Maximum client-to-server frames per exchange batch.
    pub max_frames_per_exchange: usize,
    /// First exponential-backoff step after a failed exchange.
    pub initial_backoff: Duration,
    /// Backoff ceiling (`上限 30s` in production defaults).
    pub max_backoff: Duration,
    /// Worker session capacity report skeleton; real capacity is owned by
    /// the later occupancy and worker lanes.
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
            heartbeat_interval: Duration::from_secs(15),
            enroll_poll_interval: Duration::from_secs(1),
            max_frames_per_exchange: 16,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            capacity: ClientCapacityReport {
                max_concurrent_worker_sessions: 0,
                running_worker_sessions: 0,
                reserved_worker_sessions: 0,
                draining_worker_sessions: 0,
            },
        }
    }
}

/// Fatal daemon failure; retriable exchange failures are reported through
/// [`DaemonStatus`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    /// The daemon configuration is invalid.
    Config(String),
    /// The local store failed.
    Store(DeviceStoreError),
    /// The durable outbox violates its invariants; recovery is refused
    /// (`缺失即按损坏状态拒绝恢复`).
    CorruptOutbox(OutboxStateError),
    /// A durable-state invariant the daemon itself violated, or an
    /// enrollment issuance that contradicts the durable identity.
    Protocol(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(formatter, "device daemon config error: {message}"),
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

/// Observed counters and scheduling state of one daemon session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonStatus {
    /// Whether `client.enrollment_accepted` was persisted.
    pub enrolled: bool,
    /// Exchanges attempted (a failed transport call counts).
    pub exchanges_started: u64,
    /// Exchanges whose response was applied.
    pub exchanges_succeeded: u64,
    /// Failed exchanges in a row right now.
    pub consecutive_failures: u64,
    /// `client.heartbeat` frames enqueued.
    pub heartbeats_enqueued: u64,
    /// Client-to-server frames handed to the transport so far.
    pub frames_sent: u64,
    /// Absolute server acknowledgement cursor after the last exchange.
    pub acked_through: u64,
    /// Server-to-client frames consecutively accepted (absolute cursor).
    pub downlink_accepted_through: u64,
    /// Server-to-client commands recorded but not yet handled by a lane.
    pub unhandled_downlink_commands: u64,
    /// Gap replays performed.
    pub replays: u64,
    /// Backoff applied to the next exchange attempt.
    pub current_backoff: Duration,
    /// Reason of the most recent retriable failure.
    pub last_error: Option<String>,
}

/// One run-loop step's observable result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// One exchange round-trip was applied.
    Exchanged {
        /// Client-to-server frames in the request batch.
        frames_sent: usize,
        /// Server acknowledgement recorded by this exchange.
        acked_through: u64,
        /// Server-to-client frames in the response batch.
        downlink_frames: usize,
    },
    /// Nothing was due; the loop should wait.
    Waiting {
        /// Time until the next pending report or poll.
        ready_in: Duration,
    },
    /// The exchange attempt failed and backed off.
    Retrying {
        /// Backoff applied before the next attempt.
        after: Duration,
        /// Failure reason.
        reason: String,
    },
}

/// The Device Client daemon: one persistent exchange loop over one durable
/// outbox.
pub struct DeviceDaemon {
    config: DaemonConfig,
    store: DeviceStore,
    transport: Arc<dyn ExchangeTransport>,
    codec: FrameCodec,
    session: OutboxSession,
    device_id: String,
    node_id: String,
    instance_id: String,
    enroll_idempotency_key: String,
    credential: Option<String>,
    enrolled: bool,
    enroll_frame_pending: bool,
    hello_announced: bool,
    delivery_cursor: u64,
    downlink: AckCursor,
    heartbeat_interval: Duration,
    next_heartbeat_at: Instant,
    next_attempt_at: Instant,
    consecutive_failures: u64,
    status: DaemonStatus,
}

impl DeviceDaemon {
    /// Starts one daemon session over an open store and an injected
    /// transport.
    ///
    /// Restores the phase from the durable identity: an adopted enrollment
    /// (`clientNodeId` persisted) resumes enrolled with the issued bearer
    /// credential, a fresh identity enrolls under the local placeholder node
    /// id. The server-to-client acknowledgement cursor restores from the
    /// durable inbox cursor. Pending frames keep their original stream
    /// sequence, message id, and launch instance — the server accepts them
    /// as the still-current instance and the announcement hello of this
    /// launch takes the instance over mid-batch.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Config`] for an invalid configuration,
    /// [`DaemonError::Store`] for store failures, and
    /// [`DaemonError::Protocol`] when durable state is inconsistent.
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
        let credential = if enrolled {
            Some(identity.credential().material_hex())
        } else {
            None
        };
        let enroll_idempotency_key = format!("enroll-{device_id}-{instance_id}");
        let heartbeat_interval = config.heartbeat_interval;
        let mut daemon = Self {
            config,
            store,
            transport,
            codec: FrameCodec::default(),
            session: OutboxSession::new(),
            device_id,
            node_id,
            instance_id,
            enroll_idempotency_key,
            credential,
            enrolled,
            enroll_frame_pending: false,
            hello_announced: false,
            delivery_cursor: 0,
            downlink: AckCursor::new(),
            heartbeat_interval,
            next_heartbeat_at: Instant::now() + heartbeat_interval,
            next_attempt_at: Instant::now(),
            consecutive_failures: 0,
            status: DaemonStatus {
                enrolled,
                ..DaemonStatus::default()
            },
        };
        if !daemon.enrolled {
            // The durable enroll frame is redelivered as-is; never enqueue a
            // second one (`client.enroll` must stay the first stream frame).
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
            // Crash window between the identity adoption and the profile
            // write: heal the profile from the configuration.
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

    /// The observed counters of this session.
    #[must_use]
    pub fn status(&self) -> &DaemonStatus {
        &self.status
    }

    /// This device's stable local device id.
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The exchange routing node id: the server-assigned `clientNodeId`
    /// after enrollment, the local placeholder before it.
    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.node_id
    }

    /// This launch's `clientInstanceId`.
    #[must_use]
    pub fn client_instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Whether `client.enrollment_accepted` has been persisted.
    #[must_use]
    pub const fn is_enrolled(&self) -> bool {
        self.enrolled
    }

    /// Mutable store access for callers composing additional local work.
    #[must_use]
    pub fn store_mut(&mut self) -> &mut DeviceStore {
        &mut self.store
    }

    /// Returns the durable outbox snapshot (an empty stream is the zero
    /// snapshot).
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] when the store read fails.
    pub fn outbox_snapshot(&mut self) -> Result<OutboxSnapshot, DaemonError> {
        FrameOutbox::load(&mut self.store)
            .map_err(DaemonError::Store)
            .map(Option::unwrap_or_default)
    }

    /// Persists one client-to-server message into the durable outbox
    /// (persist-before-send) and returns its sequence.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] or [`DaemonError::CorruptOutbox`] when
    /// the durable append fails, and [`DaemonError::Protocol`] when the
    /// message cannot be encoded.
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

    /// Takes the store back (for close or handover to another owner).
    #[must_use]
    pub fn into_store(self) -> DeviceStore {
        self.store
    }

    /// Runs the loop until `shutdown` is observed.
    ///
    /// Shutdown is graceful: no exchange is abandoned mid-transport, and
    /// frames leave the durable outbox only when their acknowledgement is
    /// persisted, so an interrupted session resumes from the store.
    ///
    /// # Errors
    ///
    /// Returns the first fatal [`DaemonError`] (store failure, corrupt
    /// outbox, enrollment identity contradiction).
    pub fn run(&mut self, shutdown: &AtomicBool) -> Result<DaemonStatus, DaemonError> {
        while !shutdown.load(Ordering::Relaxed) {
            let outcome = self.tick(Instant::now())?;
            let ready_in = match outcome {
                TickOutcome::Waiting { ready_in }
                | TickOutcome::Retrying {
                    after: ready_in, ..
                } => ready_in,
                TickOutcome::Exchanged { .. } => Duration::ZERO,
            };
            Self::sleep_interruptibly(ready_in, shutdown);
        }
        Ok(self.status.clone())
    }

    /// Performs at most one exchange round-trip (or schedules one).
    ///
    /// Public so tests and embedders can drive the loop deterministically
    /// without spawning a thread.
    ///
    /// # Errors
    ///
    /// Returns the first fatal [`DaemonError`]; retriable failures are
    /// reported as [`TickOutcome::Retrying`] instead.
    pub fn tick(&mut self, now: Instant) -> Result<TickOutcome, DaemonError> {
        if now < self.next_attempt_at {
            return Ok(TickOutcome::Waiting {
                ready_in: self.next_attempt_at.duration_since(now),
            });
        }
        self.ensure_pending_reports(now)?;
        let batch = match self.session.deliverable(
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
            // The endpoint rejects empty batches: an idle enrolled daemon
            // waits for the heartbeat cadence, and an enrollment wait paces
            // its next poll without sending.
            let ready_in = if self.enrolled {
                self.idle_ready_in(now)
            } else {
                self.config.enroll_poll_interval
            };
            return Ok(TickOutcome::Waiting { ready_in });
        }
        self.status.exchanges_started += 1;
        let request = self.build_request(&batch)?;
        let request_bytes = serde_json::to_vec(&request)
            .map_err(|error| DaemonError::Protocol(format!("request encoding failed: {error}")))?;
        let response_bytes = match self
            .transport
            .exchange(self.credential.as_deref(), &request_bytes)
        {
            Ok(bytes) => bytes,
            Err(error) => {
                let reason = format!("exchange transport failed: {}", error.message());
                self.register_failure(now, reason);
                return Ok(self.retry_outcome());
            }
        };
        let response: ExchangeResponse = match serde_json::from_slice(&response_bytes) {
            Ok(response) => response,
            Err(error) => {
                let reason = format!("exchange response is not a valid body: {error}");
                self.register_failure(now, reason);
                return Ok(self.retry_outcome());
            }
        };
        if response.schema_version != CLIENT_CONTROL_PORT_SCHEMA_VERSION {
            let reason = format!(
                "exchange response names schema version {} instead of \
                 {CLIENT_CONTROL_PORT_SCHEMA_VERSION}",
                response.schema_version
            );
            self.register_failure(now, reason);
            return Ok(self.retry_outcome());
        }
        if let Some(replay_from) = response.replay_from_sequence {
            self.apply_gap(&response, replay_from, &batch, now)
        } else {
            self.apply_accepted(&response, &batch, now)
        }
    }

    /// Enqueues the enroll, hello, and heartbeat reports the current phase
    /// requires (each is durable before any send).
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
                capacity: self.config.capacity,
                accepting_connections: true,
                lock_state: ClientLockState::Unlocked,
                presence_state: PresenceState::Online,
            }))?;
            self.hello_announced = true;
            self.next_heartbeat_at = now + self.heartbeat_interval;
            return Ok(());
        }
        if now >= self.next_heartbeat_at {
            self.enqueue(ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
                capacity: self.config.capacity,
                accepting_connections: true,
                lock_state: ClientLockState::Unlocked,
                presence_state: PresenceState::Online,
                // Occupancy mirroring is owned by a later lane.
                occupancy_lease_id: None,
            }))?;
            self.status.heartbeats_enqueued += 1;
            self.next_heartbeat_at = now + self.heartbeat_interval;
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
        // Everything at or below the contiguous acknowledgement is confirmed;
        // anything above stays durable for redelivery.
        self.delivery_cursor = ack;
        self.status.frames_sent += u64::try_from(batch.frames.len()).unwrap_or(u64::MAX);
        self.status.acked_through = ack;
        self.status.exchanges_succeeded += 1;
        self.reset_backoff();

        let downlink_frames =
            match self.ingest_downlink(&response.frames, response.enrollment.as_ref()) {
                Ok(count) => count,
                Err(DownlinkFailure::Retriable(reason)) => {
                    // The client acknowledgement was already persisted; the
                    // offending downlink batch is simply refused and the server
                    // replays it after our next ackSequence report.
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
        // Re-deliver from the first missing frame; confirmed frames above the
        // gap are replayed too and deduplicated by the receiver.
        self.delivery_cursor = replay_from - 1;
        self.status.replays += 1;
        self.status.exchanges_succeeded += 1;
        self.reset_backoff();
        let downlink_frames = match self.ingest_downlink(&response.frames, None) {
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

    /// Ingests the server-to-client batch: validates contiguous sequences,
    /// persists the acknowledgement cursor, and handles the enrollment
    /// response. Other commands are counted for their owning lanes.
    fn ingest_downlink(
        &mut self,
        frames: &[serde_json::Value],
        enrollment: Option<&EnrollmentIssuance>,
    ) -> Result<usize, DownlinkFailure> {
        let mut accepted = 0_usize;
        let mut acceptance: Option<ServerEnrollmentAcceptedPayload> = None;
        for value in frames {
            let envelope: ServerToClientEnvelope =
                decode_downlink_frame(&self.codec, value).map_err(DownlinkFailure::Retriable)?;
            match self.downlink.observe(envelope.sequence) {
                SequenceVerdict::Accept => {
                    self.downlink.advance(envelope.sequence).map_err(|error| {
                        DownlinkFailure::Fatal(DaemonError::Protocol(format!(
                            "downlink cursor rejected an accepted sequence: {error:?}"
                        )))
                    })?;
                    self.store
                        .advance_inbox_cursor(&ClientInboxCursorUpdate {
                            server_profile_id: self.config.server_profile_id.clone(),
                            last_sequence: envelope.sequence,
                            last_message_id: Some(envelope.message_id.clone()),
                            updated_at: now_rfc3339(),
                        })
                        .map_err(|error| DownlinkFailure::Fatal(DaemonError::Store(error)))?;
                    if let ServerToClientMessage::EnrollmentAccepted(payload) = envelope.message {
                        acceptance = Some(payload);
                    } else {
                        // Worker launch/stop, occupancy, repository, lock,
                        // and credential commands are owned by later
                        // device-client lanes; the skeleton records them
                        // without acting.
                        self.status.unhandled_downlink_commands += 1;
                    }
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
        if let Some(payload) = acceptance {
            let Some(issuance) = enrollment else {
                return Err(DownlinkFailure::Retriable(
                    "enrollment acceptance arrived without the enrollment issuance".to_owned(),
                ));
            };
            if payload.public_client_id != issuance.public_client_id {
                return Err(DownlinkFailure::Fatal(DaemonError::Protocol(format!(
                    "enrollment acceptance names publicClientId {} but the issuance \
                     carries {}",
                    payload.public_client_id, issuance.public_client_id
                ))));
            }
            self.accept_enrollment(issuance)
                .map_err(DownlinkFailure::Fatal)?;
        } else if enrollment.is_some() {
            return Err(DownlinkFailure::Retriable(
                "enrollment issuance arrived without an acceptance frame".to_owned(),
            ));
        }
        self.status.downlink_accepted_through = self.downlink.ack_sequence();
        Ok(accepted)
    }

    /// Adopts the server-issued enrollment: identity backfill, durable
    /// stream re-key onto the assigned node, bearer credential, server
    /// profile, and the server-requested heartbeat cadence.
    fn accept_enrollment(&mut self, issuance: &EnrollmentIssuance) -> Result<(), DaemonError> {
        let placeholder_node_id = self.node_id.clone();
        let stamp = now_rfc3339();
        adopt_enrollment(
            &mut self.store,
            &self.device_id,
            &IssuedEnrollment {
                client_node_id: issuance.client_node_id.clone(),
                public_client_id: issuance.public_client_id.clone(),
                credential_material: issuance.device_credential.clone(),
                credential_digest: issuance.device_credential_digest.clone(),
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
        self.credential = Some(issuance.device_credential.clone());
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

    /// Time until the next steady-state heartbeat; only consulted once
    /// enrolled (the enrollment waiting period polls exchanges instead).
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

/// Why one downlink batch could not be fully ingested.
enum DownlinkFailure {
    /// The server misbehaved; the exchange is retried with backoff.
    Retriable(String),
    /// The local durable state failed; the daemon must stop.
    Fatal(DaemonError),
}

/// Decodes one server-to-client frame value under the codec bound.
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

/// Maps an outbox acknowledgement failure to a retriable reason or a fatal
/// daemon error.
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

/// Maps every outbox failure the daemon cannot recover from by retrying.
fn fatal_outbox(error: OutboxError<DeviceStoreError>) -> DaemonError {
    match error {
        OutboxError::CorruptState(state) => DaemonError::CorruptOutbox(state),
        OutboxError::Store(store) => DaemonError::Store(store),
        other => DaemonError::Protocol(format!(
            "outbox state machine rejected the operation: {other:?}"
        )),
    }
}

/// Exponential backoff for the n-th consecutive failure, capped at
/// `max_backoff`.
fn backoff_for(failures: u64, initial_backoff: Duration, max_backoff: Duration) -> Duration {
    let mut backoff = initial_backoff;
    for _ in 1..failures {
        let doubled = backoff.saturating_mul(2);
        backoff = doubled;
        if backoff >= max_backoff {
            break;
        }
    }
    backoff.min(max_backoff)
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

/// RFC 3339 UTC stamp of the current wall clock (server-side style).
fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
