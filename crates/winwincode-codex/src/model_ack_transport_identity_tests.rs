// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep pre-retention, actual consumption, gap recovery and cumulative ACK replay in one lifecycle"
)]
async fn model_ack_identity_follows_actual_status_and_advancing_cursor() {
    use winwincode_execution_port::generated::LeaseWriteStatus;

    let root = std::env::temp_dir().join(format!("wwc-model-ack-facts-{}", uuid::Uuid::now_v7()));
    let store = AdapterStore::open(&root).unwrap();
    let authority = authority(&id("cdx", 'A'));
    let bridge = Arc::new(installed_loopback_bridge(
        &store,
        SharedAuthoritySource::default(),
        "actual ACK facts",
        "run",
        &authority,
        "kernel",
    ));
    let mut stream = bridge
        .model_port()
        .stream(ModelPortRequest {
            request_id: "original-call".into(),
            payload_json: json!({
                "requestId":"original-call", "provider":"loopback", "sessionId":"kernel",
                "threadId":authority.session_identity.codex_thread_id.0,
                "request":{"model":"loopback", "input":[]},
            })
            .to_string(),
        })
        .await
        .unwrap();
    let open = bridge.take_messages().unwrap().pop().unwrap();
    let ExecutionPortMessage::ModelOpenMessage(open_frame) = &open else {
        panic!("only the original model open may start this lifecycle");
    };
    bridge.outbox.retain(&open).unwrap();
    let chunks = (1..=3)
        .map(|sequence| {
            test_chunk(
                &authority,
                &open_frame.model_exchange_id,
                sequence,
                false,
                br#"{"type":"output_text_delta","delta":"original"}"#,
                'M',
            )
        })
        .collect::<Vec<_>>();
    // This is the real crash cut between reserve_and_accept's immutable frame
    // commit and the ModelClient's first consumption cursor commit.
    store
        .retain_model_call_frame("run", "original-call", &chunks[0])
        .unwrap();
    let first_at = Instant("2030-01-01T00:00:01.000Z".into());
    let later_at = Instant("2030-01-01T00:00:02.000Z".into());
    for (chunk, at) in [
        (&chunks[0], &first_at),
        (&chunks[0], &later_at),
        (&chunks[2], &later_at),
        (&chunks[1], &later_at),
        (&chunks[0], &later_at),
        (&chunks[2], &later_at),
    ] {
        bridge.accept_chunk(chunk, at).await.unwrap();
    }
    let acknowledgements = bridge.take_messages().unwrap();
    assert_eq!(acknowledgements.len(), 6);
    let expected = [
        (LeaseWriteStatus::Accepted, 1, None),
        (LeaseWriteStatus::Duplicate, 1, None),
        (LeaseWriteStatus::Gap, 1, Some(2)),
        (LeaseWriteStatus::Accepted, 2, None),
        (LeaseWriteStatus::Duplicate, 2, None),
        (LeaseWriteStatus::Accepted, 3, None),
    ];
    let mut ids = std::collections::BTreeSet::new();
    for (message, (status, confirmed, replay_from)) in acknowledgements.iter().zip(expected) {
        let ExecutionPortMessage::ModelAckMessage(ack) = message else {
            panic!("Provider retry must not create another model open");
        };
        assert_eq!(ack.status, status);
        assert_eq!(ack.ack_sequence.0, confirmed);
        assert_eq!(
            ack.replay_from_sequence.as_ref().map(|sequence| sequence.0),
            replay_from
        );
        assert_eq!(ack.lease, open_frame.lease);
        assert_eq!(ack.worker_session_id, open_frame.worker_session_id);
        assert_eq!(ack.session_identity, open_frame.session_identity);
        assert_eq!(ack.model_exchange_id, open_frame.model_exchange_id);
        assert!(ack.error.is_none());
        assert!(ids.insert(ack.message_id.0.clone()));
        assert_eq!(bridge.outbox.retain(message).unwrap().message, *message);
    }
    let original_duplicate = acknowledgements[4].clone();
    let ExecutionPortMessage::ModelAckMessage(original_ack) = &original_duplicate else {
        unreachable!();
    };
    bridge
        .outbox
        .record_sent(&original_ack.message_id.0)
        .unwrap();
    // Advance to cursor 3 has changed the cumulative confirmation facts. A
    // duplicate of chunk 1 now owns another stable, distinct ACK identity.
    let final_at = Instant("2030-01-01T00:00:03.000Z".into());
    bridge.accept_chunk(&chunks[0], &final_at).await.unwrap();
    let mut latest_frames = bridge.take_messages().unwrap();
    assert_eq!(latest_frames.len(), 1);
    let latest = latest_frames.pop().unwrap();
    let ExecutionPortMessage::ModelAckMessage(latest_ack) = &latest else {
        unreachable!();
    };
    assert_eq!(latest_ack.status, LeaseWriteStatus::Duplicate);
    assert_eq!(latest_ack.ack_sequence.0, 3);
    assert_ne!(latest_ack.message_id, original_ack.message_id);
    let retained = bridge.outbox.retain(&latest).unwrap();
    bridge.outbox.record_sent(&retained.delivery_id).unwrap();
    bridge
        .accept_chunk(&chunks[0], &Instant("2030-01-01T00:00:04.000Z".into()))
        .await
        .unwrap();
    let mut replays = bridge.take_messages().unwrap();
    assert_eq!(replays.len(), 1);
    let replay = replays.pop().unwrap();
    let ExecutionPortMessage::ModelAckMessage(replay_ack) = &replay else {
        unreachable!();
    };
    assert_eq!(replay_ack.message_id, latest_ack.message_id);
    assert_ne!(replay_ack.sent_at, latest_ack.sent_at);
    assert_eq!(bridge.outbox.retain(&replay).unwrap(), retained);
    for _ in 0..3 {
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            r#"{"type":"output_text_delta","delta":"original"}"#
        );
    }
    assert_eq!(
        store
            .load_model_call_frames("run", "original-call")
            .unwrap(),
        chunks
    );
    drop(stream);
    drop(bridge);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn model_ack_consumed_terminal_without_live_owner_replays_original_sent_bytes() {
    let (root, bridge, chunk) = terminal_duplicate_fixture();
    let mut replacement = authority(&id("cdx", 'R'));
    replacement.lease.attempt = 2;
    replacement.lease.fencing_token.0 = "2".into();
    replacement.lease.lease_id.0 = id("lse", 'R');
    bridge
        .install_binding(ModelRunBinding {
            run_key: "replacement-run".into(),
            canonical_thread_id: replacement.session_identity.codex_thread_id.clone(),
            kernel_session_id: "kernel-replacement".into(),
            authority: replacement.clone(),
            opened_at: replacement.lease.issued_at.clone(),
        })
        .unwrap();
    assert!(bridge.resolve_chunk_owner(&chunk).is_err());
    let first_at = Instant("2030-01-01T00:00:01.000Z".into());
    assert!(
        bridge
            .acknowledge_consumed_terminal_without_owner(&chunk, &first_at)
            .unwrap()
            .is_some()
    );
    let mut originals = bridge.take_messages().unwrap();
    assert_eq!(originals.len(), 1);
    let original = originals.pop().unwrap();
    let retained = bridge.outbox.retain(&original).unwrap();
    bridge.outbox.record_sent(&retained.delivery_id).unwrap();
    let later_at = Instant("2030-01-01T00:00:02.000Z".into());
    assert!(
        bridge
            .acknowledge_consumed_terminal_without_owner(&chunk, &later_at)
            .unwrap()
            .is_some()
    );
    let mut replays = bridge.take_messages().unwrap();
    assert_eq!(replays.len(), 1);
    let replay = replays.pop().unwrap();
    let ExecutionPortMessage::ModelAckMessage(original_ack) = &original else {
        panic!("only a consumed original terminal may receive this transport cleanup ACK");
    };
    let ExecutionPortMessage::ModelAckMessage(replay_ack) = &replay else {
        unreachable!();
    };
    assert_eq!(
        original_ack.status,
        winwincode_execution_port::generated::LeaseWriteStatus::Duplicate
    );
    assert_eq!(original_ack.message_id, replay_ack.message_id);
    assert_ne!(original_ack.sent_at, replay_ack.sent_at);
    assert_eq!(bridge.outbox.retain(&replay).unwrap(), retained);
    assert!(bridge.exchanges.lock().unwrap().is_empty());
    assert!(bridge.sink.streams.lock().unwrap().is_empty());
    drop(bridge);
    std::fs::remove_dir_all(root).unwrap();
}
