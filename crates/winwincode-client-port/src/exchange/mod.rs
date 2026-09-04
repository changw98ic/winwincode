// SPDX-License-Identifier: Apache-2.0

//! Transport-neutral exchange framework for the `ClientControlPort`.
//!
//! This module implements the transmission semantics of
//! `POST /internal/v1/client/exchange` (see
//! `docs/contracts/client-control-port-v1.md`) as a pure synchronous library:
//! no IO, no async runtime, no adapter configuration beyond the frame byte
//! bound. Both peers — the Device Client and the Control Plane — run the same
//! machinery on both directions:
//!
//! - the send side allocates sequences from 1 without gaps
//!   ([`SequenceAllocator`]), persists every frame through a durable outbox
//!   before sending ([`FrameOutbox`]), and tracks the peer's contiguous
//!   acknowledgement ([`OutboxSession::acknowledge`]) so confirmed frames may
//!   be compacted ([`CompactingOutbox`]);
//! - the receive side validates sequences against the contiguous
//!   acknowledgement ([`AckCursor`]), returns `replayFromSequence` on gaps,
//!   judges frame replays and command idempotency conflicts
//!   ([`DedupRegister`]), guards the `clientInstanceId`
//!   ([`InstanceTracker`]), and produces the fixed contract outcomes
//!   ([`IngestDecision`]);
//! - the codec seals and opens bounded frames ([`FrameCodec`]) with the
//!   stable payload digest both sides compare.
//!
//! One exchange then composes these pieces: the request carries a bounded
//! batch from the sender's outbox plus its `ackSequence` of the reverse
//! stream; the response carries the acknowledgement (and replay hint) of the
//! request batch plus the next bounded batch of the reverse stream. The
//! scenario tests at the bottom of this module walk the fixed result table
//! end to end.

pub mod dedup;
pub mod frame;
pub mod inbound;
pub mod outbox;
pub mod sequence;

pub use dedup::CommandIdentity;
pub use dedup::DedupRegister;
pub use dedup::DedupVerdict;
pub use frame::DEFAULT_MAX_FRAME_BYTES;
pub use frame::FrameCodec;
pub use frame::FrameCodecError;
pub use frame::FrameIdentity;
pub use frame::StoredFrame;
pub use frame::payload_digest;
pub use inbound::CommandOutcome;
pub use inbound::InboundStream;
pub use inbound::IngestDecision;
pub use inbound::InstanceTracker;
pub use inbound::InstanceVerdict;
pub use outbox::CompactingOutbox;
pub use outbox::FrameOutbox;
pub use outbox::InMemoryOutbox;
pub use outbox::OutboxBatch;
pub use outbox::OutboxError;
pub use outbox::OutboxInputError;
pub use outbox::OutboxSession;
pub use outbox::OutboxSnapshot;
pub use outbox::OutboxStateError;
pub use sequence::AckAdvanceError;
pub use sequence::AckCursor;
pub use sequence::SequenceAllocator;
pub use sequence::SequenceError;
pub use sequence::SequenceVerdict;

#[cfg(test)]
mod tests {
    use crate::domain::ClientCapacityReport;
    use crate::domain::ClientLockState;
    use crate::messages::ClientHeartbeatPayload;
    use crate::messages::ClientToServerEnvelope;
    use crate::messages::ClientToServerMessage;
    use crate::messages::Envelope;

    use super::*;

    const INSTANCE: &str = "inst-01j2";

    fn heartbeat(message_id: &str, sequence: u64) -> ClientToServerEnvelope {
        Envelope {
            schema_version: crate::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: message_id.to_owned(),
            client_node_id: "node_01j2".to_owned(),
            client_instance_id: INSTANCE.to_owned(),
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

    /// One direction of one exchange: the sender drains a bounded batch of
    /// its outbox, the receiver ingests every frame, and the receiver's
    /// acknowledgement (with the replay hint on gaps) flows back.
    fn exchange_one_direction(
        session: OutboxSession,
        outbox: &mut InMemoryOutbox,
        inbound: &mut InboundStream,
        sender_instance: &str,
        after_sequence: u64,
        max_frames: usize,
    ) -> u64 {
        let batch = session
            .deliverable(outbox, after_sequence, max_frames)
            .expect("delivery batch");
        let mut acked = after_sequence;
        for frame in &batch.frames {
            let decision = inbound.ingest(sender_instance, &frame.identity(), None);
            assert!(
                decision.accepted(),
                "the happy path accepts every in-order frame: {decision:?}"
            );
            acked = decision.ack_sequence();
        }
        session
            .acknowledge(outbox, acked)
            .expect("acknowledge batch");
        acked
    }

    #[test]
    fn a_full_exchange_delivers_replays_and_compacts() {
        // The Device Client fills its durable outbox with five frames.
        let codec = FrameCodec::default();
        let mut outbox = InMemoryOutbox::new();
        let session = OutboxSession::new();
        let mut allocator = SequenceAllocator::new();
        for _ in 1..=5 {
            let sequence = allocator.allocate().expect("allocate sequence");
            let sealed = codec
                .encode_envelope(&heartbeat(&format!("msg-{sequence}"), sequence))
                .expect("seal envelope");
            session
                .enqueue(&mut outbox, sequence, &sealed)
                .expect("persist before send");
        }

        // The server receives frames 1 and 2, confirms them, and the
        // exchange ends; the client persists the acknowledgement and may
        // compact the confirmed prefix.
        let mut server_inbound = InboundStream::with_instance(INSTANCE);
        assert_eq!(
            exchange_one_direction(session, &mut outbox, &mut server_inbound, INSTANCE, 0, 2),
            2
        );
        assert_eq!(server_inbound.ack_sequence(), 2);
        assert_eq!(session.compact_confirmed(&mut outbox).expect("compact"), 2);
        assert_eq!(
            session
                .deliverable(&mut outbox, 0, 8)
                .expect("batch")
                .frames
                .len(),
            3,
            "confirmed frames leave the outbox, unconfirmed frames never do"
        );

        // A network reorder delivers frame 5 before 3 and 4: the server
        // answers `replayFromSequence` and keeps its acknowledgement at 2.
        let pending = session
            .deliverable(&mut outbox, 2, 8)
            .expect("resume batch");
        let frame3 = pending.frames[0].identity();
        let frame4 = pending.frames[1].identity();
        let frame5 = pending.frames[2].identity();
        assert_eq!(
            server_inbound.ingest(INSTANCE, &frame5, None),
            IngestDecision::Gap {
                ack_sequence: 2,
                replay_from_sequence: 3
            }
        );
        // Frame 3 arrives and is accepted; a retry of it is a duplicate.
        assert_eq!(
            server_inbound.ingest(INSTANCE, &frame3, None),
            IngestDecision::Accepted {
                ack_sequence: 3,
                command: CommandOutcome::Fresh
            }
        );
        assert_eq!(
            server_inbound.ingest(INSTANCE, &frame3, None),
            IngestDecision::Duplicate { ack_sequence: 3 }
        );
        // Frame 4 closes the window and frame 5 completes the batch.
        assert_eq!(
            server_inbound.ingest(INSTANCE, &frame4, None),
            IngestDecision::Accepted {
                ack_sequence: 4,
                command: CommandOutcome::Fresh
            }
        );
        assert_eq!(
            server_inbound.ingest(INSTANCE, &frame5, None),
            IngestDecision::Accepted {
                ack_sequence: 5,
                command: CommandOutcome::Fresh
            }
        );
        // The retry exchange carries the peer acknowledgement; the outbox
        // records it and can compact everything.
        assert_eq!(session.acknowledge(&mut outbox, 5).expect("acknowledge"), 5);
        assert_eq!(session.compact_confirmed(&mut outbox).expect("compact"), 3);
        let batch = session.deliverable(&mut outbox, 5, 4).expect("retry batch");
        assert!(
            batch.frames.is_empty(),
            "a fully confirmed stream has nothing to deliver"
        );

        // The receiver's dedup records can now be compacted through the
        // confirmed prefix.
        assert_eq!(server_inbound.compact_dedup_through(5), 5);
        assert_eq!(
            server_inbound.ingest(INSTANCE, &frame5, None),
            IngestDecision::Duplicate { ack_sequence: 5 },
            "even compacted identities stay duplicates by sequence position"
        );
    }

    #[test]
    fn a_device_restart_supersedes_the_old_instance() {
        let mut server_inbound = InboundStream::with_instance(INSTANCE);
        server_inbound.ingest(INSTANCE, &FrameIdentity::new("msg-1", 1, "sha256:01"), None);

        // The device restarted and reports a new clientInstanceId: the old
        // instance is replaced and its frames are refused.
        let new_instance = "inst-02k9";
        assert_eq!(
            server_inbound.observe_instance(new_instance),
            InstanceVerdict::Replaced {
                superseded: INSTANCE.to_owned()
            }
        );
        assert_eq!(
            server_inbound.ingest(INSTANCE, &FrameIdentity::new("msg-2", 2, "sha256:02"), None),
            IngestDecision::ReacquireRequired { ack_sequence: 1 }
        );

        // The new instance continues the same per-node stream.
        assert_eq!(
            server_inbound.ingest(
                new_instance,
                &FrameIdentity::new("msg-2", 2, "sha256:02"),
                None
            ),
            IngestDecision::Accepted {
                ack_sequence: 2,
                command: CommandOutcome::Fresh
            }
        );
        assert_eq!(server_inbound.current_instance(), Some(new_instance));
    }

    #[test]
    fn an_instance_mismatch_never_advances_the_acknowledgement() {
        let mut server_inbound = InboundStream::with_instance(INSTANCE);
        server_inbound.ingest(INSTANCE, &FrameIdentity::new("msg-1", 1, "sha256:01"), None);
        for sequence in 2..=4 {
            let forged = FrameIdentity::new(
                format!("msg-{sequence}"),
                sequence,
                format!("sha256:{sequence:02x}"),
            );
            assert_eq!(
                server_inbound.ingest("inst-forged", &forged, None),
                IngestDecision::ReacquireRequired { ack_sequence: 1 }
            );
        }
        assert_eq!(server_inbound.ack_sequence(), 1);
    }

    #[test]
    fn sequence_allocation_and_codec_agree_with_the_outbox_record() {
        let codec = FrameCodec::new(1024);
        let mut allocator = SequenceAllocator::new();
        let mut outbox = InMemoryOutbox::new();
        let session = OutboxSession::new();

        for _ in 0..3 {
            let sequence = allocator.allocate().expect("allocate");
            let envelope = heartbeat(&format!("msg-{sequence}"), sequence);
            let sealed = codec.encode_envelope(&envelope).expect("seal");
            assert_eq!(sealed.sequence, sequence);
            assert_eq!(sealed.identity().sequence, sequence);
            session
                .enqueue(&mut outbox, sequence, &sealed)
                .expect("enqueue");
        }

        let batch = session.deliverable(&mut outbox, 0, 10).expect("batch");
        for stored in &batch.frames {
            let decoded: ClientToServerEnvelope =
                codec.decode_envelope(&stored.frame).expect("decode");
            assert_eq!(
                FrameCodec::envelope_identity(&decoded).expect("identity"),
                stored.identity()
            );
        }
        assert!(SequenceAllocator::from_next(batch.highest_sequence + 1).is_ok());
    }
}
