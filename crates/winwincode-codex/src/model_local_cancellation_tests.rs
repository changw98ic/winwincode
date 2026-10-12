// SPDX-License-Identifier: Apache-2.0

fn local_cancel_request(lease: &ModelLeaseAuthority, call: &str) -> ModelPortRequest {
    ModelPortRequest {
        request_id: call.into(),
        payload_json: json!({"requestId":call,"provider":"loopback","sessionId":"kernel",
            "threadId":lease.session_identity.codex_thread_id.0,
            "request":{"model":"loopback","input":[]}})
        .to_string(),
    }
}

fn local_cancel_open(bridge: &ExecutionPortModelBridge) -> ModelOpenMessage {
    bridge
        .take_messages()
        .unwrap()
        .into_iter()
        .find_map(|message| match message {
            ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
            _ => None,
        })
        .expect("one actual model open")
}

fn local_cancel_cursor(
    store: &AdapterStore,
    open: &ModelOpenMessage,
) -> crate::model_port_client::ModelCursorSnapshot {
    let mapped = winwincode_execution_port::typed_replay::stream_key_from_message(
        &ExecutionPortMessage::ModelOpenMessage(open.clone()),
    )
    .unwrap();
    ModelCursorStore::load(&mut store.clone(), &mapped.stream)
        .unwrap()
        .unwrap()
}

fn local_cancel_seals(store: &AdapterStore) -> i64 {
    store
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM model_call_response_seal", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Four actual peer calls cover empty and partial cancel, exact retry, lifecycle and immutable source evidence"
)]
async fn mechanism_actual_cancellation_closes_all_calls_without_response_seals() {
    use crate::model_port_client::{ModelCancellationPhase, ModelMessageMetadata};

    let root = std::env::temp_dir().join(format!("wwc-local-cancel-all-{}", uuid::Uuid::now_v7()));
    let store = AdapterStore::open(&root).unwrap();
    let lease = authority(&id("cdx", 'A'));
    let cancelled_at = Instant("2030-01-01T00:00:01.000Z".into());
    let bridge = Arc::new(installed_loopback_bridge(
        &store,
        SharedAuthoritySource::default().with_store(store.clone()),
        "local cancel four peers",
        "run",
        &lease,
        "kernel",
    ));
    store
        .register_performance_run("run", ExecutionMode::React, ObserverMode::Off)
        .unwrap();
    let mut calls = Vec::new();
    for index in 0..4 {
        let call = format!("local-cancel-peer-{index}");
        let request = local_cancel_request(&lease, &call);
        let mut live = bridge.model_port().stream(request.clone()).await.unwrap();
        let open = local_cancel_open(&bridge);
        if index % 2 == 1 {
            let partial = test_chunk(
                &lease,
                &open.model_exchange_id,
                1,
                false,
                b"{\"type\":\"response.output_text.delta\",\"delta\":\"synthetic peer prefix\"}",
                'P',
            );
            bridge
                .accept_chunk(&partial, &lease.lease.issued_at)
                .await
                .unwrap();
            assert!(live.next().await.unwrap().is_ok());
        }
        calls.push((call, request, live, open));
    }
    bridge.take_messages().unwrap();
    let recovered = &calls[1].3;
    let metadata = ModelMessageMetadata {
        message_id: super::execution_message_id(b"model-cancel", &recovered.model_exchange_id.0),
        sent_at: cancelled_at.clone(),
    };
    let first = bridge
        .client
        .lock()
        .await
        .cancel_exchange(&recovered.model_exchange_id, metadata.clone())
        .await
        .unwrap();
    let retry = bridge
        .client
        .lock()
        .await
        .cancel_exchange(&recovered.model_exchange_id, metadata)
        .await
        .unwrap();
    assert!(!first.replayed);
    assert!(retry.replayed);
    bridge.take_messages().unwrap();
    bridge
        .cancel_thread(&lease.session_identity.codex_thread_id, &cancelled_at)
        .await
        .unwrap();
    let cancel_messages = bridge.take_messages().unwrap();
    let cancel_acks = cancel_messages
        .iter()
        .filter(|message| matches!(message, ExecutionPortMessage::ModelAckMessage(_)))
        .count();
    let mut cases = Vec::new();
    for (index, (call, request, live, open)) in calls.iter_mut().enumerate() {
        assert_eq!(
            live.next().await.unwrap().unwrap_err().code,
            "MODEL_STREAM_TERMINATED"
        );
        assert!(live.next().await.is_none());
        let cursor = local_cancel_cursor(&store, open);
        let prefix = u64::try_from(index % 2).unwrap();
        assert_eq!(cursor.confirmed_sequence, prefix);
        assert_eq!(cursor.termination, Some(ModelTerminationReason::Cancelled));
        assert_eq!(
            cursor.cancellation.as_ref().unwrap().phase,
            ModelCancellationPhase::Applied
        );
        assert_eq!(
            store.load_model_call_frames("run", call).unwrap().len() as u64,
            prefix
        );
        assert_eq!(
            store.model_call_phase("run", call).unwrap(),
            Some(ModelCallPhase::ProviderFinal)
        );
        let reopen = bridge.model_port().stream(request.clone()).await;
        assert!(
            reopen.is_err(),
            "cancelled empty/partial source is not a complete response"
        );
        cases.push(json!({"call":call,"prefix_frames":prefix,"confirmed_sequence":cursor.confirmed_sequence,
            "cancel_applied":true,"lifecycle_closed":true,"reopen_rejected":true}));
    }
    assert_eq!(cancel_acks, 4);
    assert_eq!(local_cancel_seals(&store), 0);
    assert!(bridge.exchanges.lock().unwrap().is_empty());
    assert!(bridge.sink.lock().unwrap().is_empty());
    bridge
        .cancel_thread(&lease.session_identity.codex_thread_id, &cancelled_at)
        .await
        .unwrap();
    assert!(
        bridge.take_messages().unwrap().is_empty(),
        "closed exact thread retry adds no model operation"
    );
    let completed: i64 = store.lock().unwrap().query_row(
        "SELECT COUNT(*) FROM performance_operation WHERE operation_kind='primary_model' AND completed=1", [], |row| row.get(0)).unwrap();
    assert_eq!(completed, 4);
    let durable_sources = calls
        .iter()
        .map(|(call, request, _, open)| {
            (
                call.clone(),
                request.clone(),
                open.clone(),
                local_cancel_cursor(&store, open),
            )
        })
        .collect::<Vec<_>>();
    drop(calls);
    drop(bridge);
    drop(store);
    let restarted_store = AdapterStore::open(&root).unwrap();
    let restarted = Arc::new(installed_loopback_bridge(
        &restarted_store,
        SharedAuthoritySource::default().with_store(restarted_store.clone()),
        "local cancel database restart",
        "run",
        &lease,
        "kernel",
    ));
    for (call, request, open, original_cursor) in &durable_sources {
        assert!(
            restarted
                .model_port()
                .stream(request.clone())
                .await
                .is_err()
        );
        assert_eq!(
            local_cancel_cursor(&restarted_store, open),
            *original_cursor
        );
        assert_eq!(
            restarted_store
                .load_model_call_frames("run", call)
                .unwrap()
                .len() as u64,
            original_cursor.confirmed_sequence
        );
        assert_eq!(
            restarted_store.model_call_phase("run", call).unwrap(),
            Some(ModelCallPhase::ProviderFinal)
        );
    }
    assert_eq!(local_cancel_seals(&restarted_store), 0);
    assert!(restarted.take_messages().unwrap().is_empty());
    assert!(restarted.sink.lock().unwrap().is_empty());
    assert!(restarted.exchanges.lock().unwrap().is_empty());
    crate::audit_model_metrics::report(
        "actual-local-cancel-four-peers",
        &json!({"schema_version":1,
        "cases":cases,"cancel_acks":cancel_acks,"completed_primary_operations":completed,
        "response_seals":0,"remaining_exchanges":0,"remaining_sinks":0,"database_reopened":true,
        "cancelled_reopens_rejected_after_database_restart":4,"restart_provider_opens":0,
        "restart_cursor_and_source_frames_unchanged":true,"physical_provider_requests":0}),
    );
    drop(restarted);
    drop(restarted_store);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Strict empty and partial rejection must precede actual complete final, duplicate and durable replay"
)]
async fn mechanism_actual_real_provider_final_still_requires_complete_response_seal() {
    let root = std::env::temp_dir().join(format!("wwc-real-final-seal-{}", uuid::Uuid::now_v7()));
    let store = AdapterStore::open(&root).unwrap();
    let lease = authority(&id("cdx", 'A'));
    let bridge = Arc::new(installed_loopback_bridge(
        &store,
        SharedAuthoritySource::default().with_store(store.clone()),
        "real final seal",
        "run",
        &lease,
        "kernel",
    ));
    let mut live = bridge
        .model_port()
        .stream(local_cancel_request(&lease, "real-final-call"))
        .await
        .unwrap();
    let open = local_cancel_open(&bridge);
    let owner = bridge.exchanges.lock().unwrap()[&open.model_exchange_id.0].clone();
    let final_frame = test_chunk(
        &lease,
        &open.model_exchange_id,
        2,
        true,
        b"{\"type\":\"response.completed\",\"endTurn\":true}",
        'F',
    );
    assert!(
        bridge
            .retain_provider_final_frame(&owner, &final_frame, &lease.lease.issued_at)
            .is_err()
    );
    assert_eq!(local_cancel_seals(&store), 0);
    assert_eq!(
        store.model_call_phase("run", "real-final-call").unwrap(),
        Some(ModelCallPhase::InFlight)
    );
    let partial = test_chunk(
        &lease,
        &open.model_exchange_id,
        1,
        false,
        b"{\"type\":\"response.output_text.delta\",\"delta\":\"synthetic complete prefix\"}",
        'P',
    );
    bridge
        .accept_chunk(&partial, &lease.lease.issued_at)
        .await
        .unwrap();
    assert!(live.next().await.unwrap().is_ok());
    assert!(
        bridge
            .retain_provider_final_frame(&owner, &final_frame, &lease.lease.issued_at)
            .is_err()
    );
    assert_eq!(local_cancel_seals(&store), 0);
    assert_eq!(
        store.model_call_phase("run", "real-final-call").unwrap(),
        Some(ModelCallPhase::InFlight)
    );
    bridge
        .accept_chunk(&final_frame, &lease.lease.issued_at)
        .await
        .unwrap();
    assert!(live.next().await.unwrap().is_ok());
    assert!(live.next().await.is_none());
    assert_eq!(local_cancel_seals(&store), 1);
    assert_eq!(
        store.model_call_phase("run", "real-final-call").unwrap(),
        Some(ModelCallPhase::ProviderFinal)
    );
    assert!(matches!(
        bridge
            .accept_chunk(&final_frame, &lease.lease.issued_at)
            .await
            .unwrap(),
        ModelChunkDisposition::Duplicate {
            confirmed_sequence: 2
        }
    ));
    assert_eq!(local_cancel_seals(&store), 1);
    let mut replay = bridge
        .model_port()
        .stream(local_cancel_request(&lease, "real-final-call"))
        .await
        .unwrap();
    let mut replay_items = 0;
    while let Some(item) = replay.next().await {
        assert!(item.is_ok());
        replay_items += 1;
    }
    assert_eq!(replay_items, 2);
    assert!(
        !bridge
            .take_messages()
            .unwrap()
            .iter()
            .any(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
    );
    crate::audit_model_metrics::report(
        "actual-real-provider-final-strict-seal",
        &json!({"schema_version":1,
        "empty_response_rejected":true,"partial_response_rejected":true,"complete_frames":2,
        "response_seals":1,"exact_final_duplicate":true,"replay_items":replay_items,"new_provider_opens":0,
        "physical_provider_requests":0}),
    );
    drop(replay);
    drop(live);
    drop(bridge);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
