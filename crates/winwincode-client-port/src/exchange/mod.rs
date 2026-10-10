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
        assert_eq!(session.acknowledge(&mut outbox, 5).expect("acknowledge"), 5);
        assert_eq!(session.compact_confirmed(&mut outbox).expect("compact"), 3);
        let batch = session.deliverable(&mut outbox, 5, 4).expect("retry batch");
        assert!(
            batch.frames.is_empty(),
            "a fully confirmed stream has nothing to deliver"
        );

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
