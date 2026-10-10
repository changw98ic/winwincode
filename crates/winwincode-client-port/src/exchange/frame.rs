use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;

use crate::messages::Envelope;

pub const DEFAULT_MAX_FRAME_BYTES: usize = 256 * 1024;

#[must_use]
pub fn payload_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameIdentity {
    pub message_id: String,
    pub sequence: u64,
    pub payload_digest: String,
}

impl FrameIdentity {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFrame {
    pub message_id: String,
    pub sequence: u64,
    pub payload_digest: String,
    pub frame: Vec<u8>,
}

impl StoredFrame {
    #[must_use]
    pub fn identity(&self) -> FrameIdentity {
        FrameIdentity {
            message_id: self.message_id.clone(),
            sequence: self.sequence,
            payload_digest: self.payload_digest.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameCodecError {
    EmptyFrame,
    TooLarge { size: usize, max_frame_bytes: usize },
    Serialization(String),
    Deserialization(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameCodec {
    max_frame_bytes: usize,
}

impl FrameCodec {
    #[must_use]
    pub const fn new(max_frame_bytes: usize) -> Self {
        Self { max_frame_bytes }
    }

    #[must_use]
    pub const fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    pub fn encode<T: Serialize>(&self, value: &T) -> Result<Vec<u8>, FrameCodecError> {
        let frame = serde_json::to_vec(value)
            .map_err(|error| FrameCodecError::Serialization(error.to_string()))?;
        self.check_bound(frame.len())?;
        Ok(frame)
    }

    pub fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T, FrameCodecError> {
        self.check_bound(bytes.len())?;
        serde_json::from_slice(bytes)
            .map_err(|error| FrameCodecError::Deserialization(error.to_string()))
    }

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

    pub fn decode_envelope<M: DeserializeOwned>(
        &self,
        bytes: &[u8],
    ) -> Result<Envelope<M>, FrameCodecError> {
        self.decode(bytes)
    }

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
