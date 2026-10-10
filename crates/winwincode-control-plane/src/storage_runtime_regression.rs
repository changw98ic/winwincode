// SPDX-License-Identifier: Apache-2.0

#[test]
#[ignore = "mechanism audit: runtime ledger revalidation and serialization baseline"]
fn m03_real_runtime_ingress_revalidates_old_events_and_serializes_whole_ledger() {
    use crate::storage_mechanism_regression as measure;
    let mut bytes = Vec::new();
    for size in [100_u64, 200, 400] {
        let seed = 2_000_000 + size * 10_000;
        let mut fixture = Fixture::open("runtime-ledger-history-mechanism", seed);
        let (_, job, lease, _) = install_workrun_dispatch_for_terminal(&mut fixture, seed);
        let binding = workrun_binding(&job, &lease, seed, seed);
        fixture
            .accept(
                &ExecutionPortMessage::SessionBindingMessage(binding.clone()),
                binding.sent_at.clone(),
            )
            .unwrap();
        let mut message = RuntimeEventMessage {
            codex_thread_id: binding.codex_thread_id.clone(),
            event: ExecutionEventRecord {
                category: ExecutionEventCategory::Activity,
                event_id: ExecutionEventId(canonical_id("xevt", seed + 30)),
                occurred_at: Instant("2027-01-15T08:00:03.000Z".into()),
                payload: None,
                sequence: ExecutionSequence(1),
                summary: "synthetic offline runtime event".into(),
            },
            kind: RuntimeEventMessageKind::RuntimeEvent,
            lease,
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 30)),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:04.000Z".into()),
            session_identity: binding.session_identity.clone(),
            worker_session_id: binding.worker_session_id.clone(),
        };
        measure::reset();
        for sequence in 1..=size {
            message.event.sequence = ExecutionSequence(i64::try_from(sequence).unwrap());
            message.event.event_id =
                ExecutionEventId(canonical_id("xevt", seed + 1_000 + sequence));
            message.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 1_000 + sequence));
            let response = fixture
                .accept(
                    &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
                    Instant("2027-01-15T08:00:04.100Z".into()),
                )
                .unwrap();
            let [ExecutionPortMessage::RuntimeAckMessage(ack)] = response.as_slice() else {
                panic!("runtime ACK")
            };
            assert_eq!(ack.status, LeaseWriteStatus::Accepted);
            assert_eq!(ack.ack_sequence.0, i64::try_from(sequence).unwrap());
        }
        let metrics = measure::finish();
        assert!(metrics.old_event_validations >= size * (size - 1) / 2);
        assert_eq!(metrics.ledger_digest_serializations, 2 * size);
        assert_eq!(metrics.ledger_state_serializations, size);
        measure::reset();
        let response = fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
                Instant("2027-01-15T08:00:04.200Z".into()),
            )
            .unwrap();
        let [ExecutionPortMessage::RuntimeAckMessage(ack)] = response.as_slice() else {
            panic!("replay runtime ACK")
        };
        assert_eq!(ack.status, LeaseWriteStatus::Duplicate);
        let duplicate = measure::finish();
        assert_eq!(duplicate.old_event_validations, 0);
        assert_eq!(duplicate.ledger_reads, 0);
        assert_eq!(duplicate.ledger_state_serializations, 0);
        let mut changed = message;
        changed.event.summary = "changed receipt body".into();
        let response = fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(changed),
                Instant("2027-01-15T08:00:04.300Z".into()),
            )
            .unwrap();
        let [ExecutionPortMessage::RuntimeAckMessage(ack)] = response.as_slice() else {
            panic!("changed replay ACK")
        };
        assert_eq!(ack.status, LeaseWriteStatus::RejectedConflict);
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M03", "size":size, "metrics":metrics, "duplicate":duplicate, "accepted_events":size, "changed_body_rejected":true})
        );
        bytes.push(metrics.ledger_state_bytes);
        fixture.close();
    }
    assert!(bytes[1] > 3 * bytes[0]);
    assert!(bytes[2] > 3 * bytes[1]);
}
