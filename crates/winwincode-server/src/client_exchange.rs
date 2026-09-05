// SPDX-License-Identifier: Apache-2.0

//! Server-side `POST /internal/v1/client/exchange` endpoint for the
//! `ClientControlPort` (plan 9.2, contract `client-control-port-v1.md`).
//!
//! One HTTP request is one exchange. The request carries a bounded batch of
//! Client → Server frames plus the client's `ackSequence` of the Server →
//! Client stream; the response carries the server's contiguous
//! `ackSequence` (with `replayFromSequence` on a gap), the next bounded
//! Server → Client batch, and — only on the exchange that accepts a fresh
//! enrollment — the issued Device Credential material.
//!
//! Settlement mirrors the frozen `winwincode-client-port` exchange framework
//! on the receiving side: every frame is judged against the durable
//! contiguous acknowledgement cursor (`client_exchange_cursors`) producing
//! the fixed contract outcomes (accept, duplicate, gap with replay hint,
//! conflict, malformed, reacquire), and accepted frames execute their
//! kind-specific effect against the `ClientNode` registry (`client.enroll`,
//! `client.hello`, `client.heartbeat`), the occupancy ledger
//! (`client.occupancy.ack` promotes `reserving -> occupied`,
//! `client.occupancy.rejected` rolls the offer back, and a zero-running
//! heartbeat completes a `draining` lease), and the connect ledger
//! (`client.access.challenge_ack`); the remaining kinds settle at the cursor
//! only and are owned by later lanes.
//!
//! Credential model (plan 17.1): the server issues one random 32-byte
//! Device Credential at enrollment, persists only its `sha256:` digest in
//! `device_credential_digest`, and returns the raw material exactly once in
//! the enrollment exchange response. Every later exchange must present the
//! credential as `Authorization: Bearer <lowercase hex>`; the digest
//! comparison is constant time and every failure is one uniform 401 that
//! never discloses whether the node exists. The credential material crosses
//! only this transport response — the `client.enrollment_accepted` frame
//! itself stays free of it, matching the frozen schema.
//!
//! Cursor durability: the Client → Server acknowledgement cursor and the
//! Server → Client acknowledgement cursor persist in
//! `client_exchange_cursors`, so a Server restart never replays settled
//! frames. Downlink frames persist in the durable `client_downlink_frames`
//! outbox until the device acknowledges their sequence: the exchange
//! delivers every retained frame above the acknowledgement cursor (bounded
//! by the batch size), and acknowledged frames are retained no longer. The
//! `client.access.challenge_ack` frame settles its durable access challenge
//! through the `ConnectCodeService`, completing the connect flow's bounded
//! wait (plan 11.4).

use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use winwincode_client_port::domain::ClientArchitecture;
use winwincode_client_port::domain::ClientChallengeAckStatus;
use winwincode_client_port::domain::ClientControlMessageKind;
use winwincode_client_port::domain::ClientPlatformTarget;
use winwincode_client_port::domain::PresenceState;
use winwincode_client_port::exchange::AckCursor;
use winwincode_client_port::exchange::CommandIdentity;
use winwincode_client_port::exchange::CommandOutcome;
use winwincode_client_port::exchange::DEFAULT_MAX_FRAME_BYTES;
use winwincode_client_port::exchange::DedupRegister;
use winwincode_client_port::exchange::DedupVerdict;
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::exchange::FrameIdentity;
use winwincode_client_port::exchange::SequenceVerdict;
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::ClientEnrollPayload;
use winwincode_client_port::messages::ClientToServerEnvelope;
use winwincode_client_port::messages::ClientToServerMessage;
use winwincode_client_port::messages::ServerEnrollmentAcceptedPayload;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_client_port::messages::ServerToClientMessage;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ClientRegistryServiceErrorKind;
use winwincode_control_plane::ConnectCodeService;
use winwincode_control_plane::OccupancyLeaseState;
use winwincode_domain::Instant;
use winwincode_storage::ClientDownlinkAppend;
use winwincode_storage::ClientExchangeCursors;
use winwincode_storage::ClientNodeRecord;
use winwincode_storage::ClientNodeRegistration;
use winwincode_storage::ClientPresenceState;
use winwincode_storage::ConnectChallengeVerdict;
use winwincode_storage::OccupancyReleaseReason;
use winwincode_storage::ProductStateStorage;
use winwincode_storage::SqliteStorage;

/// Default bounds and profile values of the exchange adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientExchangeConfig {
    /// Maximum encoded size of one frame in bytes (`FrameCodec` bound).
    pub max_frame_bytes: usize,
    /// Maximum number of frames one exchange may carry in either direction.
    pub max_frames_per_exchange: usize,
    /// Heartbeat interval the server profile asks device clients to use.
    pub heartbeat_interval_ms: u32,
}

impl Default for ClientExchangeConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_frames_per_exchange: 64,
            heartbeat_interval_ms: 15_000,
        }
    }
}

impl ClientExchangeConfig {
    /// Validates the adapter bounds; a zero bound would reject every frame.
    ///
    /// # Errors
    ///
    /// Fails when the frame bound or the batch bound is zero.
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

/// Stable failure categories of the exchange transport boundary. The
/// authentication category never carries detail: every credential failure is
/// one uniform response that does not disclose node existence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientExchangeErrorKind {
    /// Missing, malformed, or non-matching Device Credential.
    Authentication,
    /// The request body violated the exchange transport contract.
    InvalidRequest,
    /// Durable state or storage failed; the exchange was not applied.
    Unavailable,
}

/// Secret-free exchange failure.
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

    /// True when the exchange failed Device Credential authentication.
    #[must_use]
    pub const fn is_authentication(&self) -> bool {
        matches!(self.kind, ClientExchangeErrorKind::Authentication)
    }

    /// True when the request violated the transport contract.
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
        };
        formatter.write_str(reason)
    }
}

impl std::error::Error for ClientExchangeError {}

/// HTTP boundary used by the server route without exposing route internals
/// through the generated public API.
pub trait ClientExchangePort: Send + Sync {
    /// Applies one exchange and returns the encoded response body.
    ///
    /// `credential` carries the raw bearer text when an Authorization header
    /// was present. `now` is the canonical application instant so registry
    /// timestamps share one clock with the HTTP boundary.
    ///
    /// # Errors
    ///
    /// Returns only the stable failure categories; credential material and
    /// frame contents never appear in diagnostics.
    fn exchange(
        &self,
        credential: Option<Vec<u8>>,
        request_body: &[u8],
        now: Instant,
    ) -> Result<Vec<u8>, ClientExchangeError>;
}

/// Production exchange over the Server's one product-state database
/// directory. Like the remote Worker exchange, every request opens and
/// closes its own connection so concurrent exchanges never share cursor
/// state in memory.
#[derive(Debug, Clone)]
pub struct ClientExchangeApplication {
    data_directory: PathBuf,
    config: ClientExchangeConfig,
}

impl ClientExchangeApplication {
    /// Composes the exchange application over one product-state directory.
    ///
    /// # Errors
    ///
    /// Fails when the adapter configuration violates its bounds.
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
        })
    }

    /// Applies one exchange against a freshly opened storage connection.
    fn apply(
        &self,
        credential: Option<&[u8]>,
        request_body: &[u8],
        now: &Instant,
    ) -> Result<ExchangeResponseBody, ClientExchangeError> {
        let request = parse_request(request_body)?;
        let codec = FrameCodec::new(self.config.max_frame_bytes);
        let envelopes = decode_frames(&codec, &request, self.config.max_frames_per_exchange)?;
        // Fail fast before any durable access: an unauthenticated exchange
        // may only carry enroll frames.
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
                request.ack_sequence,
                now,
            ),
        };
        Box::new(storage)
            .close()
            .map_err(|_| ClientExchangeError::unavailable())?;
        response
    }

    /// Settles the unauthenticated enrollment exchange: every frame must be
    /// `client.enroll`, the node must be missing (fresh identity) or still
    /// `pending_enrollment` without any issued credential (facts refresh),
    /// and one Device Credential is issued per accepted enrollment. A node
    /// with a credential never re-enrolls.
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
                // An unauthenticated exchange may only carry enroll frames.
                return Err(ClientExchangeError::authentication());
            }
        }
        // A fresh device sends its local placeholder as `clientNodeId`; only
        // a canonical server-assigned id can name an existing row.
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
                self.create_enrollment(storage, None, 0, payload, envelopes, now)
            }
            // A pending node without any credential yet may refresh its
            // enrollment facts. Once a credential was issued — pending or
            // not — enroll is refused forever; rotation is
            // `client.credential_rotate`'s authenticated job, and an
            // unauthenticated re-issue would let anyone hijack the identity.
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
                let downlink_high_water = cursors.server_to_client_ack_sequence;
                self.create_enrollment(
                    storage,
                    Some(&record),
                    downlink_high_water,
                    payload,
                    envelopes,
                    now,
                )
            }
            // An enrolled or revoked node never re-enrolls; the uniform
            // failure does not disclose which of the two it is.
            Some(_) => Err(ClientExchangeError::authentication()),
        }
    }

    /// Creates or refreshes the `pending_enrollment` node, issues one Device
    /// Credential, settles the enroll frames on the fresh per-node stream,
    /// and appends the `client.enrollment_accepted` downlink frame to the
    /// durable outbox. The frame is delivered inside this response together
    /// with the credential material and the server profile, and stays durable
    /// until the device acknowledges its sequence.
    ///
    /// `downlink_high_water` is the acknowledged downlink sequence the fresh
    /// acceptance frame is ordered after.
    #[allow(clippy::too_many_lines)]
    fn create_enrollment(
        &self,
        storage: &mut SqliteStorage,
        existing: Option<&ClientNodeRecord>,
        downlink_high_water: u64,
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
                    // The random public device number collided; draw a new
                    // one. The node id itself is fresh so only this check can
                    // repeat.
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
                BatchOutcome::Accepted { .. } => {}
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

        let codec = FrameCodec::new(self.config.max_frame_bytes);
        // The next free stream position: one past the acknowledged
        // high-water or the highest retained frame, whichever is higher.
        let mut downlink = storage
            .client_downlink_outbox()
            .map_err(|_| ClientExchangeError::unavailable())?;
        let outbox_high_water = downlink
            .high_water(&receipt.record.client_node_id)
            .map_err(|_| ClientExchangeError::unavailable())?;
        let acceptance_sequence = downlink_high_water
            .max(outbox_high_water)
            .checked_add(1)
            .ok_or(ClientExchangeError::invalid_request())?;
        let acceptance = ServerToClientEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: generate_prefixed_id("msg_")?,
            client_node_id: node_id.clone(),
            client_instance_id: instance.clone(),
            sequence: acceptance_sequence,
            occurred_at: now.0.clone(),
            message: ServerToClientMessage::EnrollmentAccepted(ServerEnrollmentAcceptedPayload {
                public_client_id: receipt.record.public_client_id.clone(),
                heartbeat_interval_ms: self.config.heartbeat_interval_ms,
                server_time: now.0.clone(),
            }),
        };
        let stored = codec
            .encode_envelope(&acceptance)
            .map_err(|_| ClientExchangeError::unavailable())?;
        let frame_text = std::str::from_utf8(&stored.frame)
            .map_err(|_| ClientExchangeError::unavailable())?
            .to_owned();
        let appended = downlink
            .append(
                &ClientDownlinkAppend::try_new(
                    node_id.clone(),
                    acceptance.message_id.clone(),
                    acceptance_sequence,
                    frame_text,
                )
                .map_err(|_| ClientExchangeError::unavailable())?,
                now,
            )
            .map_err(|_| ClientExchangeError::unavailable())?;
        debug_assert_eq!(appended.sequence, acceptance_sequence);
        let frame_value: Value = serde_json::from_str(&appended.frame)
            .map_err(|_| ClientExchangeError::unavailable())?;

        // The acceptance frame was persisted in the durable outbox and is
        // delivered inside this response; the cursor records it as the
        // delivered downlink high-water, and the durable row is retained
        // until the device acknowledges its sequence.
        {
            let mut registry = ClientRegistryService::new(storage);
            registry
                .advance_exchange_cursors(&node_id, ack_sequence, acceptance_sequence)
                .map_err(|_| ClientExchangeError::unavailable())?;
        }

        Ok(ExchangeResponseBody {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence,
            replay_from_sequence: replay_hint,
            frames: vec![frame_value],
            enrollment: Some(EnrollmentIssuance {
                client_node_id: node_id,
                public_client_id: receipt.record.public_client_id,
                device_credential: credential_hex,
                device_credential_digest: digest,
                heartbeat_interval_ms: self.config.heartbeat_interval_ms,
                server_time: now.0.clone(),
                downlink_from_sequence: acceptance_sequence,
            }),
        })
    }

    /// Settles one authenticated exchange: constant-time credential match,
    /// per-frame cursor judgement, `client.hello` instance takeover and
    /// presence, `client.heartbeat` projection, `client.access.challenge_ack`
    /// settlement, and the durable downlink batch.
    #[allow(clippy::too_many_lines)]
    fn authenticated_exchange(
        &self,
        storage: &mut SqliteStorage,
        node_id: &str,
        secret: Option<&[u8; 32]>,
        envelopes: &[ClientToServerEnvelope],
        client_ack: u64,
        now: &Instant,
    ) -> Result<ExchangeResponseBody, ClientExchangeError> {
        let Some(secret) = secret else {
            return Err(ClientExchangeError::authentication());
        };
        // A non-canonical node id cannot name an enrolled node; the uniform
        // rejection keeps node existence undisclosed.
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
        if !credential_digest_matches(secret, stored_digest) {
            return Err(ClientExchangeError::authentication());
        }
        if record.presence_state == ClientPresenceState::Revoked {
            return Err(ClientExchangeError::authentication());
        }
        let cursors = {
            let mut registry = ClientRegistryService::new(storage);
            cursors_or_unavailable(&mut registry, node_id)?
        };
        // An acknowledgement may only name delivered downlink positions:
        // either the durable cursor or a frame still retained in the outbox
        // (delivered, but not yet acknowledged).
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
            // An enrolled node never re-enrolls: the frame is refused at the
            // conflict outcome and the batch settles no further.
            if matches!(envelope.message, ClientToServerMessage::Enroll(_)) {
                break;
            }
            if matches!(envelope.message, ClientToServerMessage::Hello(_))
                && envelope.client_instance_id != effective_instance
            {
                // `client.hello` takes the instance over (the old instance is
                // superseded) before the frame is judged, so the guard
                // accepts the new instance and later old-instance frames are
                // refused as reacquire-required.
                supersede_instance(storage, &record, envelope, now);
                settler.take_over_instance(&envelope.client_instance_id);
            }
            let identity = FrameCodec::envelope_identity(envelope)
                .map_err(|_| ClientExchangeError::invalid_request())?;
            let command = command_identity(&envelope.message, &identity.payload_digest);
            match settler.ingest(&envelope.client_instance_id, &identity, command.as_ref()) {
                BatchOutcome::Accepted {
                    command: CommandOutcome::Fresh,
                } => apply_effect(storage, &self.data_directory, node_id, envelope, now)?,
                BatchOutcome::Accepted { .. } | BatchOutcome::Duplicate => {}
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

        // Durable downlink: every frame above the acknowledged sequence is
        // delivered until the device acknowledges it; acknowledged frames are
        // retained no longer.
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
        downlink
            .retain_through(node_id, cursors.server_to_client_ack_sequence)
            .map_err(|_| ClientExchangeError::unavailable())?;

        Ok(ExchangeResponseBody {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence,
            replay_from_sequence: replay_hint,
            frames,
            enrollment: None,
        })
    }
}

/// Reads the durable outbox high-water of one node, or fails the exchange as
/// unavailable.
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

/// One settled frame of an exchange batch, mirroring the fixed contract
/// outcomes of the receive-side framework. A durable exchange starts from
/// the persisted cursor, so the batch settler below applies the same
/// judgement the frozen in-memory receive stream applies: every refused
/// outcome stops the batch with an unchanged acknowledgement, and only a
/// gap carries the replay hint upward into the response.
enum BatchOutcome {
    /// The frame was accepted; `command` says whether an attached command is
    /// fresh or an idempotent replay.
    Accepted { command: CommandOutcome },
    /// The frame replayed an already accepted position; confirm it without
    /// executing again.
    Duplicate,
    /// The sequence is beyond the contiguous cursor.
    Gap { replay_from_sequence: u64 },
    /// Conflict, malformed shape, or a superseded instance: nothing was
    /// recorded and the batch settles no further.
    Refused,
}

/// Receive-side settlement of one exchange batch over the durable
/// per-node cursor (`client_exchange_cursors`). The acknowledgement cursor
/// and dedup records start from durable state every exchange, mirroring the
/// frozen receive-side judgement on the server's mirrored side.
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
        // Instance guard: frames of a superseded instance are refused until a
        // `client.hello` takes the instance over.
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
                let command_outcome = match command {
                    None => CommandOutcome::Fresh,
                    Some(command) => match self.dedup.check_command(command) {
                        DedupVerdict::Conflict => return BatchOutcome::Refused,
                        DedupVerdict::Duplicate => CommandOutcome::IdempotentReplay,
                        DedupVerdict::New => CommandOutcome::Fresh,
                    },
                };
                if self.dedup.check_frame(identity) == DedupVerdict::Conflict {
                    return BatchOutcome::Refused;
                }
                self.dedup.record_frame(identity);
                if let Some(command) = command {
                    self.dedup.record_command(command);
                }
                let _ = self.ack.advance(identity.sequence);
                BatchOutcome::Accepted {
                    command: command_outcome,
                }
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
}

/// Decoded exchange request: the bounded frame batch plus the client's
/// contiguous acknowledgement of the Server → Client stream.
#[derive(Debug, Deserialize)]
struct ExchangeRequestBody {
    #[serde(rename = "schemaVersion")]
    schema_version: String,
    #[serde(default)]
    frames: Vec<Value>,
    #[serde(rename = "ackSequence", default)]
    ack_sequence: u64,
}

/// Encoded exchange response body.
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
}

/// One-time enrollment issuance carried only by the enrollment exchange
/// response. The raw credential material crosses the transport exactly once
/// and never enters a frame payload.
#[derive(Debug, Serialize)]
struct EnrollmentIssuance {
    #[serde(rename = "clientNodeId")]
    client_node_id: String,
    #[serde(rename = "publicClientId")]
    public_client_id: String,
    /// Raw Device Credential material as lowercase hex of the 32 secret
    /// bytes; the server persists only `deviceCredentialDigest`.
    #[serde(rename = "deviceCredential")]
    device_credential: String,
    #[serde(rename = "deviceCredentialDigest")]
    device_credential_digest: String,
    #[serde(rename = "heartbeatIntervalMs")]
    heartbeat_interval_ms: u32,
    #[serde(rename = "serverTime")]
    server_time: String,
    #[serde(rename = "downlinkFromSequence")]
    downlink_from_sequence: u64,
}

fn parse_request(request_body: &[u8]) -> Result<ExchangeRequestBody, ClientExchangeError> {
    serde_json::from_slice(request_body).map_err(|_| ClientExchangeError::invalid_request())
}

/// Decodes every frame under the codec byte bound and rejects the whole
/// batch on an oversized batch, a version mismatch, or a mixed
/// `clientNodeId` routing.
fn decode_frames(
    codec: &FrameCodec,
    request: &ExchangeRequestBody,
    max_frames: usize,
) -> Result<Vec<ClientToServerEnvelope>, ClientExchangeError> {
    if request.frames.is_empty() || request.frames.len() > max_frames {
        return Err(ClientExchangeError::invalid_request());
    }
    if request.schema_version != CLIENT_CONTROL_PORT_SCHEMA_VERSION {
        // An unmatched contract version rejects the whole batch.
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
            // An unmatched contract version rejects the whole batch.
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

/// The enroll payload of one frame, or a transport rejection for any other
/// kind on an unauthenticated enrollment exchange.
fn enroll_payload(
    envelope: &ClientToServerEnvelope,
) -> Result<&ClientEnrollPayload, ClientExchangeError> {
    match &envelope.message {
        ClientToServerMessage::Enroll(payload) => Ok(payload),
        _ => Err(ClientExchangeError::authentication()),
    }
}

/// Extracts the command idempotency identity of the 10 Client → Server
/// command kinds (`19 = 8 plain + 11 fenced` across both directions; the
/// 6 Client → Server report kinds carry none).
fn command_identity(
    message: &ClientToServerMessage,
    payload_digest: &str,
) -> Option<CommandIdentity> {
    let key = match message {
        ClientToServerMessage::Enroll(payload) => &payload.command.idempotency_key,
        ClientToServerMessage::ConnectCodePublished(payload) => &payload.command.idempotency_key,
        ClientToServerMessage::AccessChallengeAck(payload) => &payload.command.idempotency_key,
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
        ClientToServerMessage::Hello(_)
        | ClientToServerMessage::Heartbeat(_)
        | ClientToServerMessage::RepositoryStatus(_)
        | ClientToServerMessage::WorkerState(_)
        | ClientToServerMessage::WorkerReconcile(_)
        | ClientToServerMessage::CommandAck(_) => return None,
    };
    Some(CommandIdentity::new(key.clone(), payload_digest))
}

/// Applies the kind-specific effect of one freshly accepted frame. Effects
/// are report facts: a refused projection (illegal transition, lost revision
/// race, unknown challenge) changes no frame settlement. The occupancy
/// mirror-revision facts (ack mirror revision, rejection current revision,
/// release/force-fence effective revision) feed the Server's durable view
/// the occupancy downlink stamps are computed against.
fn apply_effect(
    storage: &mut SqliteStorage,
    data_directory: &Path,
    node_id: &str,
    envelope: &ClientToServerEnvelope,
    now: &Instant,
) -> Result<(), ClientExchangeError> {
    match &envelope.message {
        ClientToServerMessage::Hello(payload) => {
            let mut registry = ClientRegistryService::new(storage);
            if let Some(target) = presence_target(payload.presence_state)
                && let Some(record) = registry
                    .snapshot(node_id)
                    .map_err(|_| ClientExchangeError::unavailable())?
            {
                // Illegal transitions and revision races are ignored
                // report facts; the next hello or heartbeat re-reports.
                let _ = registry.update_presence(node_id, target, record.revision);
            }
            Ok(())
        }
        ClientToServerMessage::Heartbeat(payload) => {
            let mut registry = ClientRegistryService::new(storage);
            if let Some(record) = registry
                .snapshot(node_id)
                .map_err(|_| ClientExchangeError::unavailable())?
            {
                // `pending_enrollment` and `revoked` nodes refuse heartbeats.
                let _ = registry.heartbeat(
                    node_id,
                    payload.capacity.running_worker_sessions,
                    now,
                    record.revision,
                );
            }
            // Drain automation (plan 12.4): a `draining` lease releases once
            // the device reports no running worker session. A refused
            // judgement is an ignored report fact.
            if payload.capacity.running_worker_sessions == 0 {
                let mut occupancy = ClientOccupancyService::new(storage);
                if let Ok(Some(lease)) = occupancy.active_lease_for_node(node_id)
                    && lease.state == OccupancyLeaseState::Draining
                {
                    let _ = occupancy.drain_complete(&lease.occupancy_lease_id);
                }
            }
            Ok(())
        }
        ClientToServerMessage::AccessChallengeAck(payload) => {
            // The device verdict settles the durable challenge the connect
            // flow is waiting on. A verdict the device no longer holds a
            // matching generation for is carried by the `stale_generation`
            // status and settles as a rejection.
            let verdict = match payload.status {
                ClientChallengeAckStatus::Confirmed => ConnectChallengeVerdict::Confirmed,
                ClientChallengeAckStatus::StaleGeneration => ConnectChallengeVerdict::Rejected,
            };
            let mut connect = ConnectCodeService::new(storage);
            let _ = connect.settle_challenge(
                &payload.challenge_id,
                node_id,
                &payload.connect_code_id,
                verdict,
                now,
            );
            Ok(())
        }
        ClientToServerMessage::OccupancyAck(payload) => {
            // The device persisted the occupancy mirror: the exact lease and
            // token promote `reserving -> occupied` (plan 12.2, contract
            // 9.3). A stale or rolled-back offer refuses the promotion as an
            // ignored report fact and changes no frame settlement.
            let _ = crate::client_occupancy::observe_client_mirror_revision(
                data_directory,
                node_id,
                payload.occupancy.command.expected_revision,
                now,
            );
            let mut occupancy = ClientOccupancyService::new(storage);
            let _ = occupancy.record_acknowledgement(
                &payload.occupancy.occupancy_lease_id,
                payload.occupancy.occupancy_fencing_token,
                None,
                now,
            );
            Ok(())
        }
        ClientToServerMessage::OccupancyRejected(payload) => {
            // The device refused the offer: the `reserving` lease rolls back
            // to `released` with the `client_rejected` reason (contract 4).
            // The rejection also carries the device's current mirror
            // revision, which re-syncs the Server view for the next offer.
            let _ = crate::client_occupancy::observe_client_mirror_revision(
                data_directory,
                node_id,
                payload.occupancy.command.expected_revision,
                now,
            );
            let mut occupancy = ClientOccupancyService::new(storage);
            let _ = occupancy.reject_offer(
                &payload.occupancy.occupancy_lease_id,
                payload.occupancy.occupancy_fencing_token,
                OccupancyReleaseReason::ClientRejected,
                now,
            );
            Ok(())
        }
        ClientToServerMessage::CommandAck(payload) => {
            // A release or force-fence ack reports the effective device
            // mirror revision; keep the Server view current.
            if matches!(
                payload.command_kind,
                ClientControlMessageKind::OccupancyRelease
                    | ClientControlMessageKind::OccupancyForceFence
            ) && let Some(revision) = payload.current_revision
            {
                let _ = crate::client_occupancy::observe_client_mirror_revision(
                    data_directory,
                    node_id,
                    revision,
                    now,
                );
            }
            Ok(())
        }
        // The remaining kinds settle at the cursor in this lane; their
        // Control Plane effects belong to the repository, worker, and
        // candidate lanes.
        _ => Ok(()),
    }
}

/// Persists a `client.hello` instance takeover: the old instance is
/// superseded and the hello's software facts refresh the device projection.
/// A lost revision race only delays the takeover until the next hello.
fn supersede_instance(
    storage: &mut SqliteStorage,
    record: &ClientNodeRecord,
    envelope: &ClientToServerEnvelope,
    now: &Instant,
) {
    let ClientToServerMessage::Hello(payload) = &envelope.message else {
        return;
    };
    let mut registry = ClientRegistryService::new(storage);
    let Ok(current) = registry.snapshot(&record.client_node_id) else {
        return;
    };
    let Some(current) = current else {
        return;
    };
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
        return;
    };
    let _ = registry.register(&registration, current.revision, now);
}

/// Maps a device-reported presence fact onto a registry transition target.
/// `pending_enrollment` and `revoked` are never client-reportable.
const fn presence_target(state: PresenceState) -> Option<ClientPresenceState> {
    match state {
        PresenceState::Online => Some(ClientPresenceState::Online),
        PresenceState::Degraded => Some(ClientPresenceState::Degraded),
        PresenceState::Offline => Some(ClientPresenceState::Offline),
        PresenceState::Locked => Some(ClientPresenceState::Locked),
        PresenceState::PendingEnrollment | PresenceState::Revoked => None,
    }
}

/// Issues one random 32-byte Device Credential and returns its lowercase hex
/// material plus the persisted `sha256:` digest.
fn issue_credential() -> Result<(String, String), ClientExchangeError> {
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).map_err(|_| ClientExchangeError::unavailable())?;
    Ok((hex_encode(&secret), credential_digest(&secret)))
}

/// Computes the persisted `sha256:` digest of one credential secret.
fn credential_digest(secret: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(secret))
}

/// Constant-time comparison of the supplied secret's digest against the
/// stored digest. The fixed digest shape makes the length check public.
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

/// Decodes the presented bearer text into the 32 credential bytes. Both hex
/// cases are accepted; the presentation encoding is public.
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

/// Crockford Base32 alphabet shared with the canonical identity encodings.
const IDENTITY_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Generates one canonical `prefix` + 26 character Crockford identifier.
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

/// Generates one 10-digit public device number (public, never a credential).
fn generate_public_client_id() -> Result<String, ClientExchangeError> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|_| ClientExchangeError::unavailable())?;
    let value = u64::from_be_bytes(random) % 10_000_000_000;
    Ok(format!("{value:0>10}"))
}

/// Whether `value` has the canonical `cnd_` + 26 character Crockford shape
/// a server-assigned node id carries. Only canonical ids can name an
/// existing row; device-local placeholders always take the fresh path.
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
                    expires_at: "2026-01-02T12:00:00.000Z".to_owned(),
                },
            ),
            ClientToServerMessage::AccessChallengeAck(Box::new(
                winwincode_client_port::messages::ClientAccessChallengeAckPayload {
                    command: command.clone(),
                    challenge_id: "chal_1".to_owned(),
                    connect_code_id: "code_1".to_owned(),
                    status: ClientChallengeAckStatus::Confirmed,
                },
            )),
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
                    occupancy: occupancy.clone(),
                    worker_launch_grant_id: "wlg_1".to_owned(),
                    worker_session_id: "ws_1".to_owned(),
                    worker_id: "worker_1".to_owned(),
                    worker_instance_id: "winst_1".to_owned(),
                    status: winwincode_client_port::domain::WorkerLaunchAckStatus::Accepted,
                    error: None,
                },
            )),
            ClientToServerMessage::CandidateRetained(
                winwincode_client_port::messages::ClientCandidateRetainedPayload {
                    occupancy: occupancy.clone(),
                    worker_session_id: "ws_1".to_owned(),
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
                },
            ),
            ClientToServerMessage::CandidateApplyResult(
                winwincode_client_port::messages::ClientCandidateApplyResultPayload {
                    occupancy,
                    receipt: winwincode_client_port::domain::LocalApplyReceipt {
                        local_apply_receipt_id: "lar_1".to_owned(),
                        candidate_ref: "cand_1".to_owned(),
                        repository_binding_id: "rb_1".to_owned(),
                        target_branch: "main".to_owned(),
                        expected_head: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                        strategy: winwincode_client_port::domain::ApplyStrategy::CherryPick,
                        result: winwincode_client_port::domain::ApplyResult::Applied,
                        resulting_commit: None,
                        conflict_artifact_ref: None,
                        created_at: "2026-01-02T12:00:00.000Z".to_owned(),
                        revision: 1,
                    },
                },
            ),
        ];
        assert_eq!(commands.len(), 10, "the client command kinds");
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
                    occupancy_lease_id: None,
                    worker_session_id: "ws_1".to_owned(),
                    worker_instance_id: "winst_1".to_owned(),
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
        let application =
            ClientExchangeApplication::open("unused", &ClientExchangeConfig::default())
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
        let application =
            ClientExchangeApplication::open("unused", &ClientExchangeConfig::default())
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
}
