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
//! 6. **Occupancy mirroring.** `client.occupancy.offer` persists the
//!    occupancy mirror (strictly token-monotonic, revision-bumped) and only
//!    then answers `client.occupancy.ack`; `client.occupancy.release`
//!    records a durable release intent after the fencing stamp passes; and
//!    `client.occupancy.force_fence` overwrites the mirror with the higher
//!    token, immediately invalidating every intent authorized under the
//!    previous revision (plan 12.2/12.4/12.6). The mirror survives
//!    disconnects and restarts; recovery semantics stay server-owned.
//! 7. **Backoff.** Transport failures, malformed responses, and protocol
//!    violations double the exchange backoff from
//!    [`DaemonConfig::initial_backoff`] up to
//!    [`DaemonConfig::max_backoff`]; any successful exchange resets it.
//! 8. **Lifecycle.** [`DeviceDaemon::run`] loops on a plain `std` thread
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
    ClientArchitecture, ClientCapacityReport, ClientControlError, ClientControlErrorCode,
    ClientControlMessageKind, ClientLockState, ClientPlatformTarget, CommandAckStatus,
    OccupancyRejectReason, PresenceState,
};
use winwincode_client_port::exchange::{
    AckCursor, FrameCodec, FrameCodecError, FrameOutbox, OutboxBatch, OutboxError, OutboxSession,
    OutboxSnapshot, OutboxStateError, SequenceVerdict,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientCommandAckPayload, ClientEnrollPayload,
    ClientHeartbeatPayload, ClientHelloPayload, ClientOccupancyAckPayload,
    ClientOccupancyRejectedPayload, ClientToServerEnvelope, ClientToServerMessage, CommandContext,
    OccupancyCommandContext, ServerAccessChallengePayload, ServerClientLockPayload,
    ServerEnrollmentAcceptedPayload, ServerOccupancyForceFencePayload, ServerOccupancyOfferPayload,
    ServerOccupancyReleasePayload, ServerToClientEnvelope, ServerToClientMessage,
};

use crate::connect_code::{self, PublishedConnectCode};
use crate::fencing::{FencingGuard, FencingRejection, FencingTicket};
use crate::identity::{IdentityRecord, IssuedEnrollment, adopt_enrollment};
use crate::store::{
    ClientInboxCursorUpdate, ConnectCodeStateRecord, ConnectionPolicyRecord, DeviceStore,
    DeviceStoreError, DeviceStoreErrorKind, OccupancyMirrorAdvance, OccupancyMirrorRecord,
    OccupancyMirrorUpdate, OccupancyReleaseIntentOutcome, OccupancyReleaseIntentRecord,
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
    /// `client.connect_code.published` frames enqueued by this session.
    pub connect_codes_published: u64,
    /// `client.access.challenge` frames answered with a
    /// `client.access.challenge_ack`.
    pub access_challenges_answered: u64,
    /// Access challenges whose local verdict was `confirmed`.
    pub access_challenges_confirmed: u64,
    /// `client.client_lock` commands applied and acknowledged.
    pub client_lock_commands_applied: u64,
    /// `client.occupancy.offer` frames accepted (mirror persisted, ack
    /// enqueued).
    pub occupancy_offers_acked: u64,
    /// `client.occupancy.offer` frames refused with
    /// `client.occupancy.rejected` (locked node, revision divergence, or a
    /// non-advancing fencing token).
    pub occupancy_offers_rejected: u64,
    /// `client.occupancy.release` commands recorded as new durable release
    /// intents (replayed duplicates are not counted again).
    pub occupancy_release_intents_recorded: u64,
    /// `client.occupancy.force_fence` commands applied (mirror overwritten
    /// with the higher token).
    pub occupancy_force_fences_applied: u64,
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
    /// In-memory mirror of the durable connection policy; every write goes
    /// through the store first and refreshes this mirror.
    connection_policy: ConnectionPolicyRecord,
    /// In-memory mirror of the durable occupancy mirror; every write goes
    /// through the store first (persist-before-send) and refreshes this
    /// field. Restored from the store at start and never cleared by a
    /// disconnect.
    occupancy_mirror: Option<OccupancyMirrorRecord>,
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
        let connection_policy =
            connect_code::connection_policy(&store).map_err(DaemonError::Store)?;
        // Plan 18.3: the restart scan rebuilds the occupancy mirror from the
        // durable row — a restart never clears it; the server-side recovery
        // flow reconciles against whatever the device still mirrors.
        let occupancy_mirror = store.occupancy_mirror().map_err(DaemonError::Store)?;
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
            connection_policy,
            occupancy_mirror,
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

    /// Generates a new dynamic connect code (or refreshes the current one:
    /// the previous generation stops validating challenges immediately and
    /// the new publication carries `generation + 1`), persists its digest
    /// state, and enqueues the durable `client.connect_code.published`
    /// frame. The plaintext rides the returned value only — never the store,
    /// the outbox, or any log.
    ///
    /// Requires an adopted enrollment: a pending publication frame on the
    /// placeholder stream could never be re-keyed onto the assigned node.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Protocol`] before the enrollment adoption and
    /// [`DaemonError::Store`] for durable failures.
    pub fn publish_connect_code(&mut self) -> Result<PublishedConnectCode, DaemonError> {
        self.publish_connect_code_with_ttl(connect_code::CONNECT_CODE_TTL)
    }

    /// [`DeviceDaemon::publish_connect_code`] with an explicit validity
    /// window (tests and policy-driven callers).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`DeviceDaemon::publish_connect_code`].
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

    /// Revokes the current connect code (the local disable): every later
    /// challenge naming it is refused. Returns the revoked record, or `None`
    /// when no active code exists.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] for durable failures.
    pub fn revoke_connect_code(&mut self) -> Result<Option<ConnectCodeStateRecord>, DaemonError> {
        connect_code::revoke_connect_code(&mut self.store, OffsetDateTime::now_utc())
            .map_err(DaemonError::Store)
    }

    /// Locks the client locally: `acceptingConnections = false` and
    /// `lockState = locked`, durably, mirrored into every later hello and
    /// heartbeat. While locked, every access challenge is refused.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] for durable failures.
    pub fn lock_client(&mut self) -> Result<ConnectionPolicyRecord, DaemonError> {
        self.apply_policy(false, ClientLockState::Locked)
    }

    /// Unlocks the client locally: `acceptingConnections = true` and
    /// `lockState = unlocked`, durably.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] for durable failures.
    pub fn unlock_client(&mut self) -> Result<ConnectionPolicyRecord, DaemonError> {
        self.apply_policy(true, ClientLockState::Unlocked)
    }

    /// Disables (or re-enables) new connections without changing the lock
    /// state (plan 11.1 `禁止新连接`).
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] for durable failures.
    pub fn set_accepting_connections(
        &mut self,
        accepting: bool,
    ) -> Result<ConnectionPolicyRecord, DaemonError> {
        let lock_state = self.connection_policy.lock_state;
        self.apply_policy(accepting, lock_state)
    }

    /// Persists and mirrors one connection policy update.
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

    /// The durable connect-code state (digest-bearing; no plaintext).
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] for durable failures.
    pub fn connect_code_state(&self) -> Result<Option<ConnectCodeStateRecord>, DaemonError> {
        self.store.connect_code_state().map_err(DaemonError::Store)
    }

    /// The current durable connection policy.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] for durable failures.
    pub fn connection_policy(&self) -> Result<ConnectionPolicyRecord, DaemonError> {
        connect_code::connection_policy(&self.store).map_err(DaemonError::Store)
    }

    /// The current occupancy mirror (`None` while the device holds no
    /// occupancy). It survives disconnects and restarts and is only advanced
    /// by `client.occupancy.offer` and `client.occupancy.force_fence`.
    #[must_use]
    pub const fn occupancy_mirror(&self) -> Option<&OccupancyMirrorRecord> {
        self.occupancy_mirror.as_ref()
    }

    /// A fencing guard over the current mirror — the entry point the worker
    /// epic calls before any fenced local action (worker launch/stop,
    /// candidate apply, repository mutation): `daemon.fencing_guard()
    /// .authorize_command(kind, lease, token)`.
    #[must_use]
    pub fn fencing_guard(&self) -> FencingGuard {
        FencingGuard::new(self.occupancy_mirror.clone())
    }

    /// Re-validates a previously authorized fencing ticket against the
    /// current mirror immediately before executing the command (the
    /// invalidate semantics: an offer or force-fence handled in between
    /// strands every outstanding ticket with `SupersededIntent`).
    ///
    /// # Errors
    ///
    /// Returns the fencing rejection when the ticket is no longer current.
    pub fn verify_fencing_ticket(&self, ticket: &FencingTicket) -> Result<(), FencingRejection> {
        self.fencing_guard().verify_ticket(ticket)
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
                accepting_connections: self.connection_policy.accepting_connections,
                lock_state: self.connection_policy.lock_state,
                presence_state: PresenceState::Online,
            }))?;
            self.hello_announced = true;
            self.next_heartbeat_at = now + self.heartbeat_interval;
            return Ok(());
        }
        if now >= self.next_heartbeat_at {
            self.enqueue(ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
                capacity: self.config.capacity,
                accepting_connections: self.connection_policy.accepting_connections,
                lock_state: self.connection_policy.lock_state,
                presence_state: PresenceState::Online,
                // The mirrored lease rides every heartbeat, rebuilt from the
                // durable mirror (plan 18.3: a restart keeps reporting it).
                occupancy_lease_id: self
                    .occupancy_mirror
                    .as_ref()
                    .map(|mirror| mirror.occupancy_lease_id.clone()),
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
    /// response, access challenges (answer with a `challenge_ack` verdict),
    /// client-lock commands (persist the policy, acknowledge), and the
    /// occupancy commands (mirror advance with persist-before-send acks,
    /// release intents, force-fence overwrites). Commands owned by later
    /// lanes are counted without acting.
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
                    match envelope.message {
                        ServerToClientMessage::EnrollmentAccepted(payload) => {
                            acceptance = Some(payload);
                        }
                        ServerToClientMessage::AccessChallenge(payload) => {
                            self.answer_access_challenge(&payload)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::ClientLock(payload) => {
                            self.apply_client_lock(&payload, &envelope.message_id)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::OccupancyOffer(payload) => {
                            self.apply_occupancy_offer(&payload)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::OccupancyRelease(payload) => {
                            self.apply_occupancy_release(&payload, &envelope.message_id)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        ServerToClientMessage::OccupancyForceFence(payload) => {
                            self.apply_occupancy_force_fence(&payload, &envelope.message_id)
                                .map_err(DownlinkFailure::Fatal)?;
                        }
                        _ => {
                            // Worker launch/stop, candidate apply,
                            // repository, rescan, and credential commands
                            // are owned by later device-client lanes; the
                            // skeleton records them without acting. The
                            // cursor has already advanced, so the skipped
                            // frame never blocks the stream.
                            self.status.unhandled_downlink_commands += 1;
                        }
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

    /// Answers one `client.access.challenge`: validates the challenged code
    /// generation against the durable local state and enqueues the
    /// `client.access.challenge_ack` verdict (persist-before-send, so a
    /// crash cannot drop the answer).
    ///
    /// The ack is answered for every challenge — `stale_generation` is the
    /// frozen v1 schema's only negative verdict, so local rejections
    /// (unknown/older generation, revoked, expired, locked, new connections
    /// disabled) all map onto it while the precise local reason stays in the
    /// status counters and logs never carry code material.
    fn answer_access_challenge(
        &mut self,
        payload: &ServerAccessChallengePayload,
    ) -> Result<(), DaemonError> {
        let verdict = connect_code::evaluate_access_challenge(
            &self.store,
            payload,
            OffsetDateTime::now_utc(),
        )
        .map_err(DaemonError::Store)?;
        self.enqueue(connect_code::challenge_ack_message(payload, verdict))?;
        self.status.access_challenges_answered += 1;
        if verdict.is_confirmed() {
            self.status.access_challenges_confirmed += 1;
        }
        Ok(())
    }

    /// Applies one `client.client_lock` command: persists the new durable
    /// policy (locked also disables new connections) and acknowledges the
    /// command with `client.command_ack`, per the contract's explicit-ack
    /// rule for server commands without a dedicated ack. The idempotency key
    /// derives from the command's own key, so a server replay re-acks
    /// idempotently.
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

    /// Applies one `client.occupancy.offer` (plan 12.2): persists the
    /// occupancy mirror and only then answers `client.occupancy.ack`
    /// carrying the lease, the token, and the new mirror revision
    /// (persist-before-send, so the lease enters `occupied` only after the
    /// durable fact exists). A locked node, a revision divergence, or a
    /// non-advancing token answers `client.occupancy.rejected` instead and
    /// never touches the mirror — a stale offer can never roll it back.
    ///
    /// An offer repeating the exact stored lease/token pair is the
    /// idempotent replay of an unanswered first ack: the ack is re-enqueued
    /// unchanged and the revision does not move.
    fn apply_occupancy_offer(
        &mut self,
        payload: &ServerOccupancyOfferPayload,
    ) -> Result<(), DaemonError> {
        let stamp = payload.occupancy.clone();
        let current_revision = self
            .occupancy_mirror
            .as_ref()
            .map_or(0, |mirror| mirror.mirror_revision);

        // Plan 12.1: a locked node refuses every occupancy application.
        if self.connection_policy.lock_state == ClientLockState::Locked {
            return self.reject_occupancy_offer(
                &stamp,
                OccupancyRejectReason::ClientLocked,
                current_revision,
            );
        }
        // Idempotent replay: the mirror already holds this exact lease and
        // token, so re-ack the persisted record without advancing it.
        if self
            .occupancy_mirror
            .as_ref()
            .is_some_and(|mirror| mirror_is_exact(mirror, &stamp))
        {
            let record = self.occupancy_mirror.clone().expect("checked above");
            return self.ack_occupancy_mirror(&record);
        }
        // The offer was computed against the mirror revision the server
        // last saw acknowledged; any divergence means the server's view of
        // this device moved (or replayed) and the offer must not land.
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

    /// Persists the acked mirror state in memory and enqueues the durable
    /// `client.occupancy.ack` (the mirror write already committed, so the
    /// ack frame is the persist-before-send tail).
    fn ack_occupancy_mirror(&mut self, record: &OccupancyMirrorRecord) -> Result<(), DaemonError> {
        self.occupancy_mirror = Some(record.clone());
        self.enqueue(occupancy_ack_message(record))?;
        self.status.occupancy_offers_acked += 1;
        Ok(())
    }

    /// Enqueues the durable `client.occupancy.rejected` frame for one
    /// refused offer, echoing the offered stamp so the Control Plane can
    /// settle the `reserving` lease back to `released`, and carrying the
    /// current local mirror revision in `expectedRevision`.
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

    /// Applies one `client.occupancy.release` (plan 12.4): the command must
    /// carry the current occupancy stamp; passing commands record a durable
    /// release intent (mode plus affected worker-session count — actually
    /// stopping workers belongs to the worker epic) and are answered with
    /// `client.command_ack`. Stale or unknown stamps are refused with a
    /// rejected ack and no local action. The mirror itself is never
    /// mutated: only an offer or a force-fence advances it.
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
                    release_rejection_status(rejection),
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
            occupancy_lease_id: mirror_lease,
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
        let status = match outcome {
            // Persist-before-send: the intent is durable before the ack.
            OccupancyReleaseIntentOutcome::Recorded(_) => {
                self.status.occupancy_release_intents_recorded += 1;
                CommandAckStatus::Accepted
            }
            OccupancyReleaseIntentOutcome::Duplicate(_) => CommandAckStatus::Duplicate,
        };
        ack(self, status, None, mirror_revision)
    }

    /// Applies one `client.occupancy.force_fence` (plan 12.6): overwrites
    /// the mirror with the strictly higher token and answers
    /// `client.command_ack`. Every intent authorized under the previous
    /// revision is invalidated from this moment
    /// ([`DeviceDaemon::verify_fencing_ticket`] refuses them), which is the
    /// "镜像更新后旧 token 的未处理命令立即失效" rule. A non-advancing fence is
    /// refused and never rolls the mirror back.
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
        // Idempotent replay of an unanswered ack: the mirror already holds
        // the fenced stamp, so re-ack it without advancing.
        if self
            .occupancy_mirror
            .as_ref()
            .is_some_and(|mirror| mirror_is_exact(mirror, stamp))
        {
            let revision = self
                .occupancy_mirror
                .as_ref()
                .map_or(0, |mirror| mirror.mirror_revision);
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
        // Descriptive facts ride only a lease-matched fence: a fence that
        // supersedes a foreign lease carries no holder/claim facts.
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

/// Maps a connect-code failure onto the daemon error set.
fn map_connect_code_error(error: connect_code::ConnectCodeError) -> DaemonError {
    match error {
        connect_code::ConnectCodeError::Store(store) => DaemonError::Store(store),
        connect_code::ConnectCodeError::NotEnrolled
        | connect_code::ConnectCodeError::Protocol(_) => DaemonError::Protocol(error.to_string()),
    }
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

/// Whether the mirror holds exactly the offered/fenced lease and token —
/// the identity test behind idempotent replays of the mirror-advancing
/// commands.
fn mirror_is_exact(mirror: &OccupancyMirrorRecord, stamp: &OccupancyCommandContext) -> bool {
    mirror.occupancy_lease_id == stamp.occupancy_lease_id
        && mirror.fencing_token == stamp.occupancy_fencing_token
}

/// Builds the durable `client.occupancy.ack` for one persisted mirror: the
/// ack's `expectedRevision` is the persisted mirror revision (the
/// contract's `mirrorRevision` ack fact), and the idempotency key is
/// deterministic per lease/token so a replayed offer re-acks with the same
/// key and payload.
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

/// Stable idempotency suffix for one reject reason.
const fn reject_reason_slug(reason: OccupancyRejectReason) -> &'static str {
    match reason {
        OccupancyRejectReason::UnknownLease => "unknown-lease",
        OccupancyRejectReason::StaleFencingToken => "stale-fencing-token",
        OccupancyRejectReason::LocalStateConflict => "local-state-conflict",
        OccupancyRejectReason::ClientLocked => "client-locked",
        OccupancyRejectReason::CapacityExhausted => "capacity-exhausted",
    }
}

/// The command-ack status for one release fencing rejection.
const fn release_rejection_status(rejection: FencingRejection) -> CommandAckStatus {
    match rejection {
        FencingRejection::MirrorNotSet => CommandAckStatus::RejectedLeaseMismatch,
        FencingRejection::StaleFencingToken | FencingRejection::SupersededIntent => {
            CommandAckStatus::RejectedStaleFencingToken
        }
    }
}

/// Builds the machine-readable error fact for one fencing rejection.
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

/// Builds one durable `client.command_ack` frame.
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
