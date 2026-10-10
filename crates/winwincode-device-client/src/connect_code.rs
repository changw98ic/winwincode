use std::fmt;
use std::time::Duration;

use getrandom::fill as getrandom_fill;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use winwincode_client_port::domain::{ClientLockState, ConnectCodeState};
use winwincode_client_port::exchange::{FrameCodec, OutboxSession};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientConnectCodePublishedPayload, ClientToServerEnvelope,
    ClientToServerMessage, CommandContext,
};

use crate::identity::generate_prefixed_id;
use crate::store::{ConnectCodeStateRecord, ConnectionPolicyRecord, DeviceStore, DeviceStoreError};

pub const CONNECT_CODE_DIGITS: usize = 8;
pub const CONNECT_CODE_TTL: Duration = Duration::from_mins(2);
const CONNECT_CODE_ID_PREFIX: &str = "cct_";
const UNBIASED_DIGIT_BYTE_CEILING: u8 = 250;
const MAX_GENERATION_ATTEMPTS: usize = 64;
const MAX_ID_BYTES: usize = 200;

#[derive(Clone, PartialEq, Eq)]
pub struct ConnectCodePlaintext(String);

impl ConnectCodePlaintext {
    #[must_use]
    pub(crate) const fn from_digits(code: String) -> Self {
        Self(code)
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedConnectCode {
    pub plaintext: ConnectCodePlaintext,
    pub record: ConnectCodeStateRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectCodeError {
    Store(DeviceStoreError),
    NotEnrolled,
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

#[must_use]
pub fn connect_code_digest(code: &ConnectCodePlaintext) -> String {
    format!("sha256:{:x}", Sha256::digest(code.expose().as_bytes()))
}

pub fn connection_policy(store: &DeviceStore) -> Result<ConnectionPolicyRecord, DeviceStoreError> {
    Ok(store
        .connection_policy()?
        .unwrap_or(ConnectionPolicyRecord {
            accepting_connections: true,
            lock_state: ClientLockState::Unlocked,
            updated_at: String::new(),
        }))
}

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
    let stamp = rfc3339(now);
    let expires_at = rfc3339(
        now + time::Duration::try_from(ttl)
            .map_err(|error| ConnectCodeError::Protocol(format!("ttl out of range: {error}")))?,
    );
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

pub fn set_connection_policy(
    store: &mut DeviceStore,
    accepting_connections: bool,
    lock_state: ClientLockState,
    now: OffsetDateTime,
) -> Result<ConnectionPolicyRecord, DeviceStoreError> {
    let record = ConnectionPolicyRecord {
        accepting_connections,
        lock_state,
        updated_at: rfc3339(now),
    };
    store.put_connection_policy(&record)?;
    Ok(record)
}

#[must_use]
pub fn published_message(record: &ConnectCodeStateRecord) -> ClientToServerMessage {
    ClientToServerMessage::ConnectCodePublished(ClientConnectCodePublishedPayload {
        command: CommandContext {
            expected_revision: 0,
            idempotency_key: format!("connect-code-publish-{}", record.connect_code_id),
        },
        connect_code_id: record.connect_code_id.clone(),
        code_digest: record.code_digest.clone(),
        generation: record.generation,
        expires_at: record.expires_at.clone(),
        remaining_attempts: 5,
    })
}

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
        occurred_at: rfc3339(now),
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

fn rfc3339(time: OffsetDateTime) -> String {
    crate::canonical_rfc3339(time)
}
