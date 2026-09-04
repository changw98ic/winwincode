// SPDX-License-Identifier: Apache-2.0

//! Bounded envelope codec and frame identity for the exchange layer.
//!
//! One exchange frame is one serialized [`Envelope`]. The codec converts
//! between the typed envelope and JSON bytes under a caller-configured byte
//! bound; the contract keeps frame count and byte limits in adapter
//! configuration, deliberately outside the message contract. The codec also
//! derives the stable `sha256:` payload digest that the deduplication and
//! idempotency rules compare (`重放与去重按 messageId、sequence 和 payload
//! digest 判定`).
//!
//! The payload digest covers the canonical serialization of the envelope
//! payload only, not the transport-only envelope fields. Both peers must use
//! these helpers so the digest convention agrees on both ends.

use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;

use crate::messages::Envelope;

/// Adapter default for the maximum encoded frame size in bytes.
///
/// This is an adapter configuration default, not a message-contract constant;
/// deployments may choose any bound through [`FrameCodec::new`].
pub const DEFAULT_MAX_FRAME_BYTES: usize = 256 * 1024;

/// Computes the stable payload digest of canonical payload bytes.
///
/// The digest is the `sha256:` prefixed lowercase hexadecimal SHA-256,
/// matching the digest convention already used by the `ExecutionPort` replay
/// layer.
#[must_use]
pub fn payload_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// The identity fields one frame carries for replay and deduplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameIdentity {
    /// Sender-assigned frame identity.
    pub message_id: String,
    /// Sender-stream monotonic contiguous position.
    pub sequence: u64,
    /// Digest of the canonical payload bytes.
    pub payload_digest: String,
}

impl FrameIdentity {
    /// Builds one frame identity.
    #[must_use]
    pub fn new(
        message_id: impl Into<String>,
        sequence: u64,
        payload_digest: impl Into<String>,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            sequence,
            payload_digest: payload_digest.into(),
        }
    }
}

/// One encoded frame retained for (re)delivery.
///
/// `frame` holds the original envelope bytes; they are returned unchanged by
/// duplicate and resume paths instead of rebuilding a message from current
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFrame {
    /// Sender-assigned frame identity.
    pub message_id: String,
    /// Sender-stream monotonic contiguous position.
    pub sequence: u64,
    /// Digest of the canonical payload bytes.
    pub payload_digest: String,
    /// The encoded envelope bytes.
    pub frame: Vec<u8>,
}

impl StoredFrame {
    /// Extracts the identity fields of one stored frame.
    #[must_use]
    pub fn identity(&self) -> FrameIdentity {
        FrameIdentity {
            message_id: self.message_id.clone(),
            sequence: self.sequence,
            payload_digest: self.payload_digest.clone(),
        }
    }
}

/// Codec failure modes for bounded frame encoding and decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameCodecError {
    /// The input contained no bytes.
    EmptyFrame,
    /// The frame exceeded the configured byte bound.
    TooLarge {
        /// Encoded size in bytes.
        size: usize,
        /// The configured maximum in bytes.
        max_frame_bytes: usize,
    },
    /// The value could not be serialized.
    Serialization(String),
    /// The bytes were not a valid frame value.
    Deserialization(String),
}

/// Bounded `serde_json` codec between typed envelopes and frame bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameCodec {
    max_frame_bytes: usize,
}

impl FrameCodec {
    /// Creates a codec enforcing `max_frame_bytes` on every encode and
    /// decode. A bound of zero rejects every frame.
    #[must_use]
    pub const fn new(max_frame_bytes: usize) -> Self {
        Self { max_frame_bytes }
    }

    /// Returns the configured frame byte bound.
    #[must_use]
    pub const fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Encodes one value into bounded frame bytes.
    ///
    /// # Errors
    ///
    /// Fails when serialization fails or the encoded frame exceeds the
    /// configured byte bound.
    pub fn encode<T: Serialize>(&self, value: &T) -> Result<Vec<u8>, FrameCodecError> {
        let frame = serde_json::to_vec(value)
            .map_err(|error| FrameCodecError::Serialization(error.to_string()))?;
        self.check_bound(frame.len())?;
        Ok(frame)
    }

    /// Decodes one value from bounded frame bytes.
    ///
    /// # Errors
    ///
    /// Fails when the bytes are empty, exceed the configured byte bound, or
    /// are not a valid value.
    pub fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T, FrameCodecError> {
        self.check_bound(bytes.len())?;
        serde_json::from_slice(bytes)
            .map_err(|error| FrameCodecError::Deserialization(error.to_string()))
    }

    /// Encodes one envelope into a stored frame, deriving the payload digest.
    ///
    /// # Errors
    ///
    /// Fails when serialization fails or the encoded frame exceeds the
    /// configured byte bound.
    pub fn encode_envelope<M: Serialize>(
        &self,
        envelope: &Envelope<M>,
    ) -> Result<StoredFrame, FrameCodecError> {
        let identity = Self::envelope_identity(envelope)?;
        let frame = self.encode(envelope)?;
        Ok(StoredFrame {
            message_id: identity.message_id,
            sequence: identity.sequence,
            payload_digest: identity.payload_digest,
            frame,
        })
    }

    /// Decodes one envelope from bounded frame bytes.
    ///
    /// # Errors
    ///
    /// Fails when the bytes are empty, exceed the configured byte bound, or
    /// are not a valid envelope.
    pub fn decode_envelope<M: DeserializeOwned>(
        &self,
        bytes: &[u8],
    ) -> Result<Envelope<M>, FrameCodecError> {
        self.decode(bytes)
    }

    /// Derives the frame identity of one envelope: `messageId`, `sequence`,
    /// and the digest over the canonical payload bytes.
    ///
    /// # Errors
    ///
    /// Fails when the payload cannot be serialized.
    pub fn envelope_identity<M: Serialize>(
        envelope: &Envelope<M>,
    ) -> Result<FrameIdentity, FrameCodecError> {
        let payload = serde_json::to_vec(&envelope.message)
            .map_err(|error| FrameCodecError::Serialization(error.to_string()))?;
        Ok(FrameIdentity {
            message_id: envelope.message_id.clone(),
            sequence: envelope.sequence,
            payload_digest: payload_digest(&payload),
        })
    }

    fn check_bound(&self, size: usize) -> Result<(), FrameCodecError> {
        if size == 0 {
            return Err(FrameCodecError::EmptyFrame);
        }
        if size > self.max_frame_bytes {
            return Err(FrameCodecError::TooLarge {
                size,
                max_frame_bytes: self.max_frame_bytes,
            });
        }
        Ok(())
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::ClientCapacityReport;
    use crate::domain::ClientLockState;
    use crate::messages::ClientHeartbeatPayload;
    use crate::messages::ClientToServerEnvelope;
    use crate::messages::ClientToServerMessage;

    use super::*;

    fn heartbeat_envelope(sequence: u64) -> ClientToServerEnvelope {
        Envelope {
            schema_version: crate::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: format!("msg-{sequence}"),
            client_node_id: "node_01j2".to_owned(),
            client_instance_id: "inst_01j2".to_owned(),
            sequence,
            occurred_at: "2026-01-02T12:00:00Z".to_owned(),
            message: ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
                capacity: ClientCapacityReport {
                    max_concurrent_worker_sessions: 3,
                    running_worker_sessions: 1,
                    reserved_worker_sessions: 0,
                    draining_worker_sessions: 0,
                },
                accepting_connections: true,
                lock_state: ClientLockState::Unlocked,
                presence_state: crate::domain::PresenceState::Online,
                occupancy_lease_id: None,
            }),
        }
    }

    #[test]
    fn payload_digest_is_prefixed_sha256() {
        assert_eq!(
            payload_digest(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn payload_digest_distinguishes_payloads() {
        assert_ne!(payload_digest(b"one"), payload_digest(b"two"));
    }

    #[test]
    fn codec_round_trips_envelope_bytes() {
        let codec = FrameCodec::default();
        let envelope = heartbeat_envelope(1);
        let bytes = codec.encode(&envelope).expect("encode envelope");
        assert!(
            bytes
                .windows(b"\"client.heartbeat\"".len())
                .any(|window| window == b"\"client.heartbeat\""),
            "the exact wire kind string must survive encoding"
        );
        let decoded: ClientToServerEnvelope = codec.decode(&bytes).expect("decode envelope");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn encode_envelope_seals_identity_and_digest() {
        let codec = FrameCodec::default();
        let envelope = heartbeat_envelope(7);
        let stored = codec.encode_envelope(&envelope).expect("seal envelope");
        assert_eq!(stored.message_id, "msg-7");
        assert_eq!(stored.sequence, 7);
        let payload = serde_json::to_vec(&envelope.message).expect("serialize payload");
        assert_eq!(stored.payload_digest, payload_digest(&payload));
        assert_eq!(stored.frame, codec.encode(&envelope).expect("re-encode"));
    }

    #[test]
    fn envelope_identity_matches_sealed_frame() {
        let codec = FrameCodec::default();
        let envelope = heartbeat_envelope(2);
        let identity = FrameCodec::envelope_identity(&envelope).expect("derive identity");
        assert_eq!(
            codec.encode_envelope(&envelope).expect("seal").identity(),
            identity
        );
    }

    #[test]
    fn codec_enforces_the_frame_bound() {
        let codec = FrameCodec::new(16);
        let envelope = heartbeat_envelope(1);
        let encoded = codec.encode(&envelope);
        let size = match encoded {
            Err(FrameCodecError::TooLarge {
                size,
                max_frame_bytes,
            }) => {
                assert_eq!(max_frame_bytes, 16);
                size
            }
            other => panic!("expected TooLarge, got {other:?}"),
        };
        assert!(
            codec
                .decode::<ClientToServerEnvelope>(&vec![b'x'; size + 1])
                .is_err(),
            "oversized input must be rejected before parsing"
        );
    }

    #[test]
    fn decode_rejects_empty_and_malformed_bytes() {
        let codec = FrameCodec::default();
        assert_eq!(
            codec.decode::<ClientToServerEnvelope>(&[]),
            Err(FrameCodecError::EmptyFrame)
        );
        assert!(matches!(
            codec.decode::<ClientToServerEnvelope>(b"{}"),
            Err(FrameCodecError::Deserialization(_))
        ));
    }

    #[test]
    fn default_codec_uses_the_adapter_default_bound() {
        assert_eq!(
            FrameCodec::default().max_frame_bytes(),
            DEFAULT_MAX_FRAME_BYTES
        );
    }
}
