// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[tokio::test]
async fn device_primary_accounting_without_jev_sums_all_measured_attempts() {
    Box::pin(assert_device_primary_accounting_without_jev(false)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn device_primary_accounting_without_jev_keeps_missing_attempt_usage_unknown() {
    Box::pin(assert_device_primary_accounting_without_jev(true)).await;
}

#[cfg(unix)]
#[allow(
    clippy::too_many_lines,
    reason = "Cover Device binding, retained frames, Core completion and reopen as one accounting lifecycle"
)]
async fn assert_device_primary_accounting_without_jev(missing_attempt_usage: bool) {
    use base64::Engine as _;
    use futures::StreamExt as _;
    use sha2::{Digest as _, Sha256};
    use winwincode_domain::ExecutionSequence;
    use winwincode_execution_port::generated::{
        EncodedPayload, ExecutionOutcomeUsage, ExecutionOutcomeUsageAccountingStatus,
        LeaseWriteStatus, ModelChunkMessage, ModelChunkMessageKind, ProductSessionExecutionScope,
        ProductSessionExecutionScopeKind,
    };

    let root = test_root("device-primary-accounting-without-jev");
    let directory = root.join("providers");
    let device = winwincode_provider::ProviderRuntimeStore::prepare(&directory).unwrap();
    let runtime_directory = device.directory().to_path_buf();
    let config = diagnostic_adapter_config(&root);
    assert!(config.jev_context.is_none());
    assert!(config.jev_judge.is_none());
    let mut adapter = ProductionCodexAdapter::open(config)
        .unwrap()
        .with_provider_runtime(runtime_directory.clone())
        .unwrap();
    let (mut record, mut binding) = delegated_record_and_binding();
    binding.authority.session_identity.product_session_id =
        ProductSessionId("psn_00000000000000000000000001".into());
    binding.authority.session_identity.work_run_id = None;
    binding.authority.lease.fencing_token = FencingToken("1".into());
    binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
    record.job.scope = ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
        kind: ProductSessionExecutionScopeKind::ProductSession,
        product_session_id: binding
            .authority
            .session_identity
            .product_session_id
            .clone(),
    });
    record.job.execution_profile = "default".into();
    record.job.work_input = None;
    record.role_policy = None;
    record.agent_config = super::production_agent_session_config(
        &adapter.config,
        &binding.authority.lease.worker_id,
        &record.job,
        None,
    )
    .unwrap();
    record.job_digest = super::stage_product_job_digest(&record.job).unwrap();
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    let key = binding.run_key.clone();
    let now = Instant("2026-08-28T00:00:02.000Z".into());
    adapter
        .store
        .register_performance_run(&key, ExecutionMode::React, ObserverMode::Off)
        .unwrap();
    adapter
        .install_active_run(&key, record, binding.clone(), false, false)
        .unwrap();
    let request = winwincode_kernel::ModelPortRequest {
        request_id: "original-chat-call".into(),
        payload_json: serde_json::json!({
            "requestId":"original-chat-call", "provider":"fixture-provider",
            "sessionId":binding.kernel_session_id,
            "threadId":binding.canonical_thread_id.0,
            "request":{"model":"fixture-model", "input":[]},
        })
        .to_string(),
    };
    let mut stream = adapter
        .bridge
        .model_port()
        .stream(request.clone())
        .await
        .unwrap();
    let open = adapter
        .take_execution_messages()
        .unwrap()
        .into_iter()
        .find_map(|message| match message {
            ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
            _ => None,
        })
        .unwrap();
    let payload = |bytes: &[u8]| EncodedPayload {
        content_type: "application/json".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    };
    let final_bytes = br#"{"type":"completed","tokenUsage":{"inputTokens":1,"cachedInputTokens":0,"outputTokens":1},"actualCostMicros":2}"#;
    let chunk = ModelChunkMessage {
        error: None,
        is_final: true,
        kind: ModelChunkMessageKind::ModelChunk,
        lease: open.lease.clone(),
        message_id: ExecutionMessageId("xmsg_00000000000000000000000001".into()),
        model_exchange_id: open.model_exchange_id.clone(),
        payload: Some(payload(final_bytes)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now.clone(),
        sequence: ExecutionSequence(1),
        session_identity: open.session_identity.clone(),
        worker_session_id: open.worker_session_id.clone(),
    };
    let db =
        rusqlite::Connection::open(runtime_directory.join("provider-runtime.sqlite3")).unwrap();
    let saved = serde_json::to_string(&open).unwrap();
    db.execute(
        "INSERT INTO exchanges(exchange_id,digest,request_open) VALUES(?1,?2,?3)",
        rusqlite::params![
            open.model_exchange_id.0,
            format!("{:x}", Sha256::digest(saved.as_bytes())),
            saved
        ],
    )
    .unwrap();
    device
        .retain_stored_model_exchange_chunks(&open, std::slice::from_ref(&chunk))
        .unwrap();
    let mut failed = chunk.clone();
    failed.message_id = ExecutionMessageId("xmsg_00000000000000000000000002".into());
    let failed_bytes: &[u8] = if missing_attempt_usage {
        br#"{"type":"error","error":{"code":"SERVER","message":"failed physical attempt"}}"#
    } else {
        br#"{"type":"error","error":{"code":"SERVER","message":"failed physical attempt"},"tokenUsage":{"inputTokens":2,"cachedInputTokens":0,"outputTokens":1},"actualCostMicros":3}"#
    };
    failed.payload = Some(payload(failed_bytes));
    let original_request =
        serde_json::from_str::<serde_json::Value>(&request.payload_json).unwrap();
    let request_payload = original_request["request"].to_string();
    // This focused Adapter fixture retains legal physical frames. The production
    // retry vertical creates the same rows through the real Provider transport.
    for (attempt, state, frame) in [(1, "failed", &failed), (2, "completed", &chunk)] {
        db.execute(
            "INSERT INTO model_invocation_attempts(exchange_id,attempt_number,adapter_request_id,state,accounting_chunks,request_json,request_payload) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![open.model_exchange_id.0,attempt,format!("physical-{attempt}"),state,serde_json::to_string(&[frame]).unwrap(),saved,request_payload.as_bytes()],
        ).unwrap();
    }
    assert!(device.model_jev_receipts(&open).unwrap().is_empty());
    assert!(device.model_jev_judge_receipts(&open).unwrap().is_empty());
    adapter.accept_model_chunk(&chunk, &now).await.unwrap();
    // The Device canonical decoder emits a normalized `completed` payload.
    // The bridge must deliver and replay those exact original bytes to Core.
    let original_final = std::str::from_utf8(final_bytes).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(final_bytes).unwrap()["type"],
        "completed"
    );
    assert_eq!(stream.next().await.unwrap().unwrap(), original_final);
    assert!(stream.next().await.is_none());
    let expected_tokens = if missing_attempt_usage { None } else { Some(5) };
    let lower = if missing_attempt_usage { 2 } else { 5 };
    let status = if missing_attempt_usage {
        ExecutionOutcomeUsageAccountingStatus::Unknown
    } else {
        ExecutionOutcomeUsageAccountingStatus::Known
    };
    let usage = adapter
        .store
        .retained_outcome_usage(&key, 1)
        .unwrap()
        .unwrap();
    assert_eq!(usage.tokens, expected_tokens);
    assert_eq!(usage.known_tokens, lower);
    assert_eq!(usage.accounting_status, status);
    assert_eq!(usage.cost_microunits, expected_tokens);
    assert_eq!(
        adapter
            .store
            .delegated_performance_totals(&key)
            .unwrap()
            .pending_model_calls,
        0
    );
    adapter.accept_model_chunk(&chunk, &now).await.unwrap();
    assert_eq!(
        adapter.store.retained_outcome_usage(&key, 1).unwrap(),
        Some(usage.clone())
    );
    let mut replay = adapter.bridge.model_port().stream(request).await.unwrap();
    assert_eq!(replay.next().await.unwrap().unwrap(), original_final);
    assert!(replay.next().await.is_none());
    let acknowledgements = adapter.take_execution_messages().unwrap();
    assert_eq!(acknowledgements.len(), 2);
    let acknowledgements = acknowledgements
        .iter()
        .map(|message| match message {
            ExecutionPortMessage::ModelAckMessage(ack) => ack,
            _ => {
                panic!("exact replay must emit only original and duplicate ACKs, never a new open")
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(acknowledgements[0].status, LeaseWriteStatus::Accepted);
    assert_eq!(acknowledgements[1].status, LeaseWriteStatus::Duplicate);
    assert_ne!(
        acknowledgements[0].message_id,
        acknowledgements[1].message_id
    );
    for acknowledgement in &acknowledgements {
        assert_eq!(acknowledgement.ack_sequence.0, 1);
        assert_eq!(acknowledgement.model_exchange_id, chunk.model_exchange_id);
        assert_eq!(acknowledgement.lease, chunk.lease);
        assert_eq!(acknowledgement.worker_session_id, chunk.worker_session_id);
        assert_eq!(acknowledgement.session_identity, chunk.session_identity);
        assert!(acknowledgement.error.is_none());
        assert!(acknowledgement.replay_from_sequence.is_none());
    }
    let original_duplicate_ack = acknowledgements[1].clone();
    for acknowledgement in &acknowledgements {
        adapter
            .record_execution_delivery_sent(&acknowledgement.message_id.0)
            .unwrap();
    }
    let replay_at = Instant("2026-08-28T00:00:05.000Z".into());
    adapter
        .accept_model_chunk(&chunk, &replay_at)
        .await
        .unwrap();
    assert_eq!(
        adapter.take_execution_messages().unwrap(),
        vec![ExecutionPortMessage::ModelAckMessage(
            original_duplicate_ack
        )]
    );
    assert_eq!(
        adapter.store.retained_outcome_usage(&key, 1).unwrap(),
        Some(usage.clone())
    );
    // Accounting queries take a caller-supplied runtime. A real terminal
    // retains the complete original operation window, including replay time.
    let operation_window: (String, Option<String>) = adapter
        .store
        .lock()
        .unwrap()
        .query_row(
            "SELECT MIN(started_at), MAX(completed_at) FROM performance_operation_accounted WHERE run_key=?1",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let began = Instant(operation_window.0.clone());
    let terminal_runtime = crate::performance::elapsed_millis(&began, &replay_at)
        .unwrap()
        .max(operation_window.1.as_ref().map_or(0, |ended| {
            crate::performance::elapsed_millis(&began, &Instant(ended.clone())).unwrap()
        }))
        .max(1);
    let expected_terminal_usage = ExecutionOutcomeUsage {
        runtime_millis: terminal_runtime,
        ..usage.clone()
    };
    adapter
        .accept_turn_complete(
            &key,
            &TurnCompleteEvent {
                turn_id: "turn-fixture".into(),
                last_agent_message: Some("completed Chat result".into()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: Some(1),
                time_to_first_token_ms: None,
            },
            &replay_at,
        )
        .unwrap();
    let retained = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    let Some(super::StoredTerminal::Completed {
        usage: Some(terminal_usage),
        ..
    }) = retained.terminal
    else {
        panic!("accounting incompleteness must preserve the completed Chat result");
    };
    assert_eq!(terminal_usage, expected_terminal_usage);
    let original_terminal_usage = terminal_usage;
    drop(replay);
    drop(stream);
    drop(db);
    drop(device);
    drop(adapter);
    let reopened = ProductionCodexAdapter::open(diagnostic_adapter_config(&root))
        .unwrap()
        .with_provider_runtime(runtime_directory)
        .unwrap();
    let retained = load_stored_run(&reopened.store, &key).unwrap().unwrap();
    let Some(super::StoredTerminal::Completed {
        usage: Some(terminal_usage),
        ..
    }) = retained.terminal
    else {
        panic!("restart must retain the original completed Chat result");
    };
    assert_eq!(terminal_usage, original_terminal_usage);
    assert_eq!(
        reopened.store.retained_outcome_usage(&key, 1).unwrap(),
        Some(usage)
    );
    let retained_window: (String, Option<String>) = reopened
        .store
        .lock()
        .unwrap()
        .query_row(
            "SELECT MIN(started_at), MAX(completed_at) FROM performance_operation_accounted WHERE run_key=?1",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained_window, operation_window);
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}
