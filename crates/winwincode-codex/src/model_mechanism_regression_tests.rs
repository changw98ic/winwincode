// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
#[ignore = "mechanism audit: measured intake and complete-cursor replay cost at 500/1000/2527 frames"]
#[allow(
    clippy::too_many_lines,
    reason = "Keep actual bridge intake, durable cursor replay, and original growth assertions in one audit fixture"
)]
async fn mechanism_actual_model_intake_and_complete_cursor_restore() {
    let mut cases = Vec::new();
    for count in [500_usize, 1000, 2527] {
        let root =
            std::env::temp_dir().join(format!("wwc-model-mechanism-{}", uuid::Uuid::now_v7()));
        let store = AdapterStore::open(&root).unwrap();
        let lease = authority(&id("cdx", 'A'));
        let bridge = Arc::new(installed_loopback_bridge(
            &store,
            SharedAuthoritySource::default().with_store(store.clone()),
            "model mechanism",
            "run",
            &lease,
            "kernel",
        ));
        let request = ModelPortRequest {
            request_id: "synthetic-call".into(),
            payload_json: json!({"requestId":"synthetic-call", "provider":"loopback", "sessionId":"kernel",
                "threadId":lease.session_identity.codex_thread_id.0, "request":{"model":"loopback","input":[]}}).to_string(),
        };
        let mut live = bridge.model_port().stream(request.clone()).await.unwrap();
        let open = bridge
            .take_messages()
            .unwrap()
            .into_iter()
            .find_map(|m| match m {
                ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                _ => None,
            })
            .unwrap();
        // Synthetic payloads contain no formal model content. Match the observed
        // per-frame envelope scale as closely as the canonical test envelope allows.
        let profile: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/mechanism/model-frame-size-profile.json"
        ))
        .unwrap();
        let target_sizes = profile["frame_json_size_by_sequence"].as_array().unwrap();
        let frames = (1..=count).map(|sequence| {
            let target_size = usize::try_from(
                target_sizes[(sequence - 1) * 2527 / count].as_u64().unwrap(),
            )
            .expect("synthetic frame size fits usize");
            let make = |padding: usize| {
                let payload = json!({"type": if sequence == count {"response.completed"} else {"response.output_text.delta"},
                    "delta":"x".repeat(padding), "endTurn": sequence == count}).to_string();
                test_chunk(&lease, &open.model_exchange_id, i64::try_from(sequence).expect("synthetic sequence fits i64"), sequence == count, payload.as_bytes(), 'P')
            };
            let base = make(0);
            let base_size = serde_json::to_vec(&base).unwrap().len();
            let approximate_padding = target_size.saturating_sub(base_size) * 3 / 4;
            let mut closest = (base_size.abs_diff(target_size), base);
            for padding in approximate_padding.saturating_sub(4)..=approximate_padding + 4 {
                let frame = make(padding);
                let size = serde_json::to_vec(&frame).unwrap().len();
                let distance = size.abs_diff(target_size);
                if distance < closest.0 { closest = (distance, frame); }
            }
            closest.1
        }).collect::<Vec<_>>();
        let stored_bytes = frames
            .iter()
            .map(|x| serde_json::to_vec(x).unwrap().len() as u64)
            .sum::<u64>();
        crate::audit_model_metrics::reset();
        let started = std::time::Instant::now();
        let mut frame_timings = Vec::new();
        for frame in &frames {
            let before = std::time::Instant::now();
            let disposition = bridge
                .accept_chunk(frame, &lease.lease.issued_at)
                .await
                .unwrap();
            assert!(matches!(
                disposition,
                ModelChunkDisposition::Delivered { .. }
            ));
            let text = live.next().await.unwrap().unwrap();
            let expected = base64::engine::general_purpose::STANDARD
                .decode(&frame.payload.as_ref().unwrap().data_base64)
                .unwrap();
            assert_eq!(text.as_bytes(), expected);
            if frame.sequence.0 == 1 || frame.sequence.0 % 250 == 0 || frame.is_final {
                frame_timings.push(json!({"sequence":frame.sequence.0,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
                    "accept_ms":before.elapsed().as_secs_f64()*1000.0}));
            }
        }
        let intake_ms = started.elapsed().as_secs_f64() * 1000.0;
        let intake = crate::audit_model_metrics::take();
        assert_eq!(
            intake.cursor_writes, count as u64,
            "each actual live frame writes the full cursor"
        );
        assert!(
            intake.full_frame_rows_read >= (count * (count - 1) / 2) as u64,
            "actual accept must read each prior complete frame"
        );
        assert!(
            intake.fingerprint_visits >= (count * (count - 1)) as u64,
            "actual cursor validation repeatedly visits prior fingerprints"
        );
        assert_eq!(
            store.model_call_phase("run", "synthetic-call").unwrap(),
            Some(ModelCallPhase::ProviderFinal)
        );
        assert!(
            live.next().await.is_none(),
            "the live stream must close once after final"
        );
        let delivered = store
            .load_model_call_frames("run", "synthetic-call")
            .unwrap();
        assert_eq!(
            delivered, frames,
            "all synthetic frames must survive exact durable replay"
        );
        crate::audit_model_metrics::reset();
        let restored_at = std::time::Instant::now();
        let final_duplicate = bridge
            .accept_chunk(frames.last().unwrap(), &lease.lease.issued_at)
            .await
            .unwrap();
        assert!(matches!(
            final_duplicate,
            ModelChunkDisposition::Duplicate { .. }
        ));
        let restore_ms = restored_at.elapsed().as_secs_f64() * 1000.0;
        let restore = crate::audit_model_metrics::take();
        assert!(
            restore.cursor_reads >= count as u64,
            "one detached final restores the whole cursor once per retained frame"
        );
        assert!(restore.fingerprint_visits >= (count * count) as u64);
        let mut replay = bridge.model_port().stream(request).await.unwrap();
        for frame in &frames {
            let text = replay.next().await.unwrap().unwrap();
            assert_eq!(
                text.as_bytes(),
                base64::engine::general_purpose::STANDARD
                    .decode(&frame.payload.as_ref().unwrap().data_base64)
                    .unwrap()
            );
        }
        assert!(replay.next().await.is_none());
        assert!(
            !bridge
                .take_messages()
                .unwrap()
                .iter()
                .any(|m| matches!(m, ExecutionPortMessage::ModelOpenMessage(_))),
            "complete local replay opens no physical model request"
        );
        cases.push(json!({"frames":count,"stored_frame_json_bytes":stored_bytes,"target_reference_frame_bytes":2_658_792,"synthetic_size_profile":"secret-free actual per-sequence JSON lengths; payload is synthetic x padding; canonical test envelope can impose a minimum size",
            "intake_ms":intake_ms,"intake_metrics":intake,"intake_checkpoints":frame_timings,"exact_final_restore_ms":restore_ms,"restore_metrics":restore,
            "physical_provider_requests":0,"all_final_frames_preserved":true,"replay_opens":0}));
        drop(replay);
        drop(live);
        drop(bridge);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    crate::audit_model_metrics::report(
        "actual-model-intake",
        &json!({"schema_version":1,
        "source_commit":"e994faa55ac2baad964bea5c43a23d919497a9de", "method":"ExecutionPortModelBridge::accept_chunk -> actual AdapterStore and WorkerModelPortClient", "cases":cases}),
    );
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep failed and replacement bridge requests with original isolation assertions in one fixture"
)]
async fn mechanism_actual_failed_partial_response_keeps_next_request_isolated() {
    use winwincode_execution_port::generated::{ExecutionPortError, ExecutionPortErrorCode};
    let root =
        std::env::temp_dir().join(format!("wwc-model-failed-partial-{}", uuid::Uuid::now_v7()));
    let store = AdapterStore::open(&root).unwrap();
    let lease = authority(&id("cdx", 'A'));
    let bridge = Arc::new(installed_loopback_bridge(
        &store,
        SharedAuthoritySource::default().with_store(store.clone()),
        "partial isolation",
        "run",
        &lease,
        "kernel",
    ));
    let request = |call: &str| {
        ModelPortRequest {request_id:call.into(), payload_json:json!({"requestId":call,
        "provider":"loopback","sessionId":"kernel","threadId":lease.session_identity.codex_thread_id.0,
        "request":{"model":"loopback","input":[]}}).to_string()}
    };
    let mut first = bridge
        .model_port()
        .stream(request("failed-partial-call"))
        .await
        .unwrap();
    let first_open = bridge
        .take_messages()
        .unwrap()
        .into_iter()
        .find_map(|m| match m {
            ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
            _ => None,
        })
        .unwrap();
    let partial = test_chunk(
        &lease,
        &first_open.model_exchange_id,
        1,
        false,
        b"{\"type\":\"output_text_delta\",\"delta\":\"synthetic-partial-first-call\"}",
        'P',
    );
    bridge
        .accept_chunk(&partial, &lease.lease.issued_at)
        .await
        .unwrap();
    assert!(
        first
            .next()
            .await
            .unwrap()
            .unwrap()
            .contains("synthetic-partial-first-call")
    );
    let mut failed = test_chunk(&lease, &first_open.model_exchange_id, 2, true, b"{}", 'F');
    failed.payload = None;
    failed.error = Some(ExecutionPortError {
        code: ExecutionPortErrorCode::DeviceProviderUpstreamFailed,
        message: "synthetic offline failure".into(),
        retryable: false,
    });
    bridge
        .accept_chunk(&failed, &lease.lease.issued_at)
        .await
        .unwrap();
    assert!(first.next().await.unwrap().is_err());
    assert!(first.next().await.is_none());
    let mut second = bridge
        .model_port()
        .stream(request("fresh-second-call"))
        .await
        .unwrap();
    let second_open = bridge
        .take_messages()
        .unwrap()
        .into_iter()
        .find_map(|m| match m {
            ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
            _ => None,
        })
        .unwrap();
    assert_ne!(first_open.model_exchange_id, second_open.model_exchange_id);
    let complete = test_chunk(
        &lease,
        &second_open.model_exchange_id,
        1,
        true,
        b"{\"type\":\"completed\",\"delta\":\"synthetic-complete-second-call\",\"endTurn\":true}",
        'S',
    );
    bridge
        .accept_chunk(&complete, &lease.lease.issued_at)
        .await
        .unwrap();
    let delivered = second.next().await.unwrap().unwrap();
    assert!(delivered.contains("synthetic-complete-second-call"));
    assert!(!delivered.contains("synthetic-partial-first-call"));
    assert!(second.next().await.is_none());
    assert_eq!(
        store
            .load_model_call_frames("run", "failed-partial-call")
            .unwrap(),
        vec![partial, failed]
    );
    assert_eq!(
        store
            .load_model_call_frames("run", "fresh-second-call")
            .unwrap(),
        vec![complete]
    );
    crate::audit_model_metrics::report(
        "actual-failed-partial-request-isolation",
        &json!({"schema_version":1,
        "actual_path":"ExecutionPortModelBridge -> WorkerModelPortClient -> AdapterStore", "first_call_failed_after_partial":true,
        "second_call_new_exchange_and_sequence_one":true,"response_fragments_mixed":false,"physical_provider_requests":0,
        "scope_limit":"real model bridge and stream sink; embedded Core tool execution is checked separately by full Worker fixture"}),
    );
    drop(first);
    drop(second);
    drop(bridge);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
