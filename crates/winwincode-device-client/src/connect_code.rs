// SPDX-License-Identifier: Apache-2.0

//! Dynamic connect code lifecycle on the device (CLIENT-200.2, plan
//! sections 11.1 and 11.3).
//!
//! The device generates its own 8-digit one-time connect code, publishes it
//! to the Control Plane as a `sha256` digest inside a durable
//! `client.connect_code.published` frame (persist-before-send through the
//! daemon's outbox path), and answers every `client.access.challenge` by
//! checking that the challenged code generation is still the local current,
//! unexpired, non-revoked code on an unlocked node.
//!
//! Secret boundary: the plaintext code exists only in the process memory of
//! whoever generated it ([`ConnectCodePlaintext`] redacts its `Debug`
//! output). It never enters the durable store, the outbox, or any log
//! payload — only the digest does. A restart therefore loses the ability to
//! *display* the code but never the ability to *verify* challenges against
//! the durable digest.

use std::fmt;
use std::time::Duration;

use getrandom::fill as getrandom_fill;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use winwincode_client_port::domain::{ClientChallengeAckStatus, ClientLockState, ConnectCodeState};
use winwincode_client_port::exchange::{FrameCodec, OutboxSession};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientAccessChallengeAckPayload,
    ClientConnectCodePublishedPayload, ClientToServerEnvelope, ClientToServerMessage,
    CommandContext, ServerAccessChallengePayload,
};

use crate::identity::generate_prefixed_id;
use crate::store::{ConnectCodeStateRecord, ConnectionPolicyRecord, DeviceStore, DeviceStoreError};

/// Digit count of the dynamic connect code (plan 11.3: `8 位数字`).
pub const CONNECT_CODE_DIGITS: usize = 8;
/// Default validity window of one publication (plan 11.3: `默认 2 分钟有效`).
pub const CONNECT_CODE_TTL: Duration = Duration::from_mins(2);
/// Canonical connect code id prefix, matching the schema's
/// `ClientConnectCodeId` pattern (`cct_` + 26 Crockford characters).
const CONNECT_CODE_ID_PREFIX: &str = "cct_";
/// Largest byte accepted by the uniform digit rejection sampler: the first
/// multiple of ten at or below 256 keeps every digit equally likely.
const UNBIASED_DIGIT_BYTE_CEILING: u8 = 250;
/// Bound on the weak-shape rejection loop; reachable only if the entropy
/// source itself is broken.
const MAX_GENERATION_ATTEMPTS: usize = 64;
const MAX_ID_BYTES: usize = 200;

/// The plaintext 8-digit connect code, held in process memory only.
///
/// `Debug` is manually redacted and there is no `Display`, so the code can
/// only leave process memory through [`ConnectCodePlaintext::expose`] or
/// [`ConnectCodePlaintext::grouped`] — the two explicit presentation
/// surfaces (plan 11.1 local display, §16.8 CLI fallback).
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectCodePlaintext(String);

impl ConnectCodePlaintext {
    /// Wraps an already validated digit string (tests and callers that
    /// received the code from [`generate_connect_code`]).
    #[must_use]
    pub(crate) const fn from_digits(code: String) -> Self {
        Self(code)
    }

    /// The plaintext code. Never log, persist, or frame the returned value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The grouped presentation of the plan's local display (`6842 1975`).
    #[must_use]
    pub fn grouped(&self) -> String {
        self.0
            .as_bytes()
            .chunks(4)
            .map(std::str::from_utf8)
            .collect::<Result<Vec<_>, _>>()
            .map_or_else(|_| self.0.clone(), |groups| groups.join(" "))
    }
}

impl fmt::Debug for ConnectCodePlaintext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ConnectCodePlaintext([redacted])")
    }
}

/// A freshly generated (or refreshed) publication: the one-time plaintext
/// for local display plus the durable record whose digest was published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedConnectCode {
    /// The plaintext code; display it once, then drop it.
    pub plaintext: ConnectCodePlaintext,
    /// The durable state that replaced the previous generation.
    pub record: ConnectCodeStateRecord,
}

/// Local verdict for one `client.access.challenge` (plan 11.4 step 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeVerdict {
    /// The challenged code is the current, active, unexpired generation and
    /// the node accepts new connections.
    Confirmed,
    /// The challenge names an unknown code id or a foreign digest (any older
    /// generation after a refresh lands here).
    UnknownCode,
    /// The code was revoked (locally disabled) before use.
    CodeRevoked,
    /// The code's 120-second window has passed.
    CodeExpired,
    /// The node is locked; every challenge is refused (plan 12.1).
    Locked,
    /// New connections are locally disabled without a full lock.
    NewConnectionsDisabled,
}

impl ChallengeVerdict {
    /// The wire verdict for `client.access.challenge_ack`. The frozen v1
    /// schema carries exactly two statuses, so every local rejection maps to
    /// `stale_generation`; the precise local reason stays in
    /// [`ChallengeVerdict`].
    #[must_use]
    pub const fn wire_status(self) -> ClientChallengeAckStatus {
        match self {
            Self::Confirmed => ClientChallengeAckStatus::Confirmed,
            Self::UnknownCode
            | Self::CodeRevoked
            | Self::CodeExpired
            | Self::Locked
            | Self::NewConnectionsDisabled => ClientChallengeAckStatus::StaleGeneration,
        }
    }

    /// Whether the challenge was confirmed.
    #[must_use]
    pub const fn is_confirmed(self) -> bool {
        matches!(self, Self::Confirmed)
    }
}

/// Failure of a connect-code publication or frame append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectCodeError {
    /// The durable store failed.
    Store(DeviceStoreError),
    /// The publication requires an adopted enrollment (`clientNodeId`).
    NotEnrolled,
    /// The entropy source failed too often for the weak-shape rejection
    /// loop, or a frame could not be encoded.
    Protocol(String),
}

impl fmt::Display for ConnectCodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "connect code store failure: {error}"),
            Self::NotEnrolled => write!(
                formatter,
                "connect code publication requires an adopted enrollment"
            ),
            Self::Protocol(message) => write!(formatter, "connect code failure: {message}"),
        }
    }
}

impl std::error::Error for ConnectCodeError {}

impl From<DeviceStoreError> for ConnectCodeError {
    fn from(error: DeviceStoreError) -> Self {
        Self::Store(error)
    }
}

/// Generates one strong 8-digit connect code with `getrandom`.
///
/// Digits use rejection sampling (uniform, unbiased). Weak shapes are
/// rejected and regenerated: all-identical digits and full ascending or
/// descending runs (`11111111`, `12345678`, `87654321`).
///
/// # Errors
///
/// Returns [`ConnectCodeError::Protocol`] when the entropy source fails or
/// the rejection loop exhausts its (practically unreachable) attempt bound.
pub fn generate_connect_code() -> Result<ConnectCodePlaintext, ConnectCodeError> {
    for _ in 0..MAX_GENERATION_ATTEMPTS {
        let digits = random_digits(CONNECT_CODE_DIGITS)?;
        let code: String = digits.iter().map(|digit| (b'0' + digit) as char).collect();
        if !is_weak_connect_code(&code) {
            return Ok(ConnectCodePlaintext::from_digits(code));
        }
    }
    Err(ConnectCodeError::Protocol(
        "the connect code entropy source never produced a strong shape".to_owned(),
    ))
}

/// Draws `count` uniform decimal digits via rejection sampling.
fn random_digits(count: usize) -> Result<Vec<u8>, ConnectCodeError> {
    let mut digits = Vec::with_capacity(count);
    let mut chunk = [0_u8; 32];
    while digits.len() < count {
        getrandom_fill(&mut chunk)
            .map_err(|error| ConnectCodeError::Protocol(format!("entropy failure: {error}")))?;
        for byte in chunk {
            if byte < UNBIASED_DIGIT_BYTE_CEILING {
                digits.push(byte % 10);
                if digits.len() == count {
                    break;
                }
            }
        }
    }
    Ok(digits)
}

/// Whether `code` is malformed or one of the rejected weak shapes: not
/// exactly [`CONNECT_CODE_DIGITS`] ASCII digits, all digits identical, or a
/// full ascending/descending run.
#[must_use]
pub fn is_weak_connect_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    if bytes.len() != CONNECT_CODE_DIGITS || !bytes.iter().all(u8::is_ascii_digit) {
        return true;
    }
    let digits: Vec<u8> = bytes.iter().map(|byte| byte - b'0').collect();
    digits.iter().all(|digit| *digit == digits[0])
        || digits.windows(2).all(|window| window[1] == window[0] + 1)
        || digits.windows(2).all(|window| window[1] + 1 == window[0])
}

/// The `sha256:` digest published to the server instead of the plaintext.
#[must_use]
pub fn connect_code_digest(code: &ConnectCodePlaintext) -> String {
    format!("sha256:{:x}", Sha256::digest(code.expose().as_bytes()))
}

/// The effective connection policy: the durable row, or the default
/// (accepting new connections, unlocked) before the first policy write.
///
/// # Errors
///
/// Returns a store failure when the read fails.
pub fn connection_policy(store: &DeviceStore) -> Result<ConnectionPolicyRecord, DeviceStoreError> {
    Ok(store
        .connection_policy()?
        .unwrap_or(ConnectionPolicyRecord {
            accepting_connections: true,
            lock_state: ClientLockState::Unlocked,
            updated_at: String::new(),
        }))
}

/// Generates (or refreshes) the dynamic connect code and persists its
/// durable digest state.
///
/// Every publication replaces the previous generation: the old code stops
/// validating challenges immediately, and the new row carries
/// `generation + 1`. The plaintext is returned for local display only; it is
/// never written to the store.
///
/// # Errors
///
/// Returns [`ConnectCodeError::Store`] for durable failures and
/// [`ConnectCodeError::Protocol`] for entropy failures.
pub fn publish_connect_code(
    store: &mut DeviceStore,
    issued_by_instance_id: &str,
    now: OffsetDateTime,
    ttl: Duration,
) -> Result<PublishedConnectCode, ConnectCodeError> {
    if issued_by_instance_id.is_empty() || issued_by_instance_id.len() > MAX_ID_BYTES {
        return Err(ConnectCodeError::Protocol(
            "the issuing client instance id must be non-empty and bounded".to_owned(),
        ));
    }
    let plaintext = generate_connect_code()?;
    let generation = store
        .connect_code_state()?
        .map_or(1, |stored| stored.generation + 1);
    let stamp = rfc3339(now)?;
    let expires_at = rfc3339(
        now + time::Duration::try_from(ttl)
            .map_err(|error| ConnectCodeError::Protocol(format!("ttl out of range: {error}")))?,
    )?;
    let record = ConnectCodeStateRecord {
        connect_code_id: generate_prefixed_id(CONNECT_CODE_ID_PREFIX)?,
        code_digest: connect_code_digest(&plaintext),
        generation,
        issued_by_instance_id: issued_by_instance_id.to_owned(),
        expires_at,
        state: ConnectCodeState::Active,
        created_at: stamp.clone(),
        updated_at: stamp,
    };
    store.replace_connect_code_state(&record)?;
    Ok(PublishedConnectCode { plaintext, record })
}

/// Revokes the current code (the local disable, plan 11.1 `禁止新连接`'s
/// code-side counterpart). Returns the revoked record, or `None` when no
/// active code exists.
///
/// # Errors
///
/// Returns a store failure when the write fails.
pub fn revoke_connect_code(
    store: &mut DeviceStore,
    now: OffsetDateTime,
) -> Result<Option<ConnectCodeStateRecord>, DeviceStoreError> {
    let stamp = rfc3339(now)?;
    if store.revoke_connect_code_state(&stamp)? {
        Ok(store.connect_code_state()?)
    } else {
        Ok(None)
    }
}

/// Persists a new connection policy (lock state plus whether new
/// connections are accepted) and returns the stored record.
///
/// # Errors
///
/// Returns a store failure when the write fails.
pub fn set_connection_policy(
    store: &mut DeviceStore,
    accepting_connections: bool,
    lock_state: ClientLockState,
    now: OffsetDateTime,
) -> Result<ConnectionPolicyRecord, DeviceStoreError> {
    let record = ConnectionPolicyRecord {
        accepting_connections,
        lock_state,
        updated_at: rfc3339(now)?,
    };
    store.put_connection_policy(&record)?;
    Ok(record)
}

/// Evaluates one `client.access.challenge` against the local code
/// generation and connection policy (plan 11.4 step 7: `确认该 code
/// generation 仍有效并 ACK`).
///
/// The verdict order is: unknown code/foreign digest, revoked, expired,
/// locked, new connections disabled, confirmed.
///
/// # Errors
///
/// Returns a store failure when the durable reads fail or a stored stamp is
/// not RFC 3339.
pub fn evaluate_access_challenge(
    store: &DeviceStore,
    challenge: &ServerAccessChallengePayload,
    now: OffsetDateTime,
) -> Result<ChallengeVerdict, DeviceStoreError> {
    let policy = connection_policy(store)?;
    let Some(record) = store.connect_code_state()? else {
        return Ok(ChallengeVerdict::UnknownCode);
    };
    if record.connect_code_id != challenge.connect_code_id
        || record.code_digest != challenge.code_digest
    {
        // Any older generation after a refresh fails both comparisons; a
        // foreign digest can never match the current publication.
        return Ok(ChallengeVerdict::UnknownCode);
    }
    if record.state != ConnectCodeState::Active {
        return Ok(ChallengeVerdict::CodeRevoked);
    }
    let expires_at = parse_rfc3339(&record.expires_at)?;
    if now >= expires_at {
        return Ok(ChallengeVerdict::CodeExpired);
    }
    if policy.lock_state == ClientLockState::Locked {
        return Ok(ChallengeVerdict::Locked);
    }
    if !policy.accepting_connections {
        return Ok(ChallengeVerdict::NewConnectionsDisabled);
    }
    Ok(ChallengeVerdict::Confirmed)
}

/// Builds the `client.connect_code.published` payload's message for one
/// publication. The digest rides alone; the plaintext is not part of the
/// message type at all.
#[must_use]
pub fn published_message(record: &ConnectCodeStateRecord) -> ClientToServerMessage {
    ClientToServerMessage::ConnectCodePublished(ClientConnectCodePublishedPayload {
        command: CommandContext {
            expected_revision: 0,
            idempotency_key: format!("connect-code-publish-{}", record.connect_code_id),
        },
        connect_code_id: record.connect_code_id.clone(),
        code_digest: record.code_digest.clone(),
        expires_at: record.expires_at.clone(),
    })
}

/// Builds the `client.access.challenge_ack` message for one verdict.
///
/// The idempotency key is deterministic per challenge id, so a server replay
/// of the same unanswered challenge produces the same key with the same
/// payload and replays the first verdict instead of creating a second one.
#[must_use]
pub fn challenge_ack_message(
    challenge: &ServerAccessChallengePayload,
    verdict: ChallengeVerdict,
) -> ClientToServerMessage {
    ClientToServerMessage::AccessChallengeAck(Box::new(ClientAccessChallengeAckPayload {
        command: CommandContext {
            expected_revision: 0,
            idempotency_key: format!("challenge-ack-{}", challenge.challenge_id),
        },
        challenge_id: challenge.challenge_id.clone(),
        connect_code_id: challenge.connect_code_id.clone(),
        status: verdict.wire_status(),
    }))
}

/// Appends the durable `client.connect_code.published` frame for one
/// publication to the outbox (persist-before-send), using the same
/// message-id convention as the daemon's enqueue path.
///
/// The frame stays pending on the durable stream until the server
/// acknowledges it; a restart re-delivers it unchanged.
///
/// # Errors
///
/// Returns a store failure when the durable append fails and
/// [`ConnectCodeError::Protocol`] when the envelope cannot be encoded.
pub fn enqueue_published_frame(
    store: &mut DeviceStore,
    client_node_id: &str,
    client_instance_id: &str,
    record: &ConnectCodeStateRecord,
    now: OffsetDateTime,
) -> Result<u64, ConnectCodeError> {
    const KIND: &str = "client.connect_code.published";
    let session = OutboxSession::new();
    let expected = session.next_sequence(store).map_err(map_outbox_error)?;
    let envelope = ClientToServerEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: format!("{client_node_id}-{KIND}-{expected}"),
        client_node_id: client_node_id.to_owned(),
        client_instance_id: client_instance_id.to_owned(),
        sequence: expected,
        occurred_at: rfc3339(now)?,
        message: published_message(record),
    };
    let stored = FrameCodec::default()
        .encode_envelope(&envelope)
        .map_err(|error| {
            ConnectCodeError::Protocol(format!("published frame encoding failed: {error:?}"))
        })?;
    session
        .enqueue(store, expected, &stored)
        .map_err(map_outbox_error)
}

/// Maps an outbox state-machine failure onto the connect-code error set.
fn map_outbox_error(
    error: winwincode_client_port::exchange::OutboxError<DeviceStoreError>,
) -> ConnectCodeError {
    use winwincode_client_port::exchange::OutboxError;
    match error {
        OutboxError::Store(store) => ConnectCodeError::Store(store),
        OutboxError::CorruptState(state) => ConnectCodeError::Store(DeviceStoreError::adapter(
            format!("the durable outbox is corrupt: {state:?}"),
        )),
        other => ConnectCodeError::Protocol(format!(
            "the outbox state machine rejected the publication: {other:?}"
        )),
    }
}

/// RFC 3339 UTC stamp of the caller's clock observation.
fn rfc3339(time: OffsetDateTime) -> Result<String, DeviceStoreError> {
    time.format(&Rfc3339)
        .map_err(|error| DeviceStoreError::adapter(format!("timestamp formatting failed: {error}")))
}

/// Parses a stored or wire RFC 3339 stamp, fail-closing on corruption.
fn parse_rfc3339(stamp: &str) -> Result<OffsetDateTime, DeviceStoreError> {
    OffsetDateTime::parse(stamp, &Rfc3339).map_err(|error| {
        DeviceStoreError::adapter(format!("stored timestamp {stamp} is not RFC 3339: {error}"))
    })
}
