// SPDX-License-Identifier: Apache-2.0

fn fusion_payload_fixture() -> (
    std::path::PathBuf,
    AdapterStore,
    Arc<ExecutionPortModelBridge>,
    ModelRunBinding,
) {
    use winwincode_execution_port::agent_config::{
        AgentSessionConfigSnapshot, resolve_agent_session_config,
    };
    let (root, store, binding, _, _, _) = child_model_fixture();
    let base: AgentSessionConfigSnapshot =
        serde_json::from_value(sealed_profile("primary", "primary-model", "max")).unwrap();
    let mut settings = base.profile.source.settings;
    settings.fusion = Some(
        serde_json::from_value(json!({"members":[
            {"id":"glm","provider":"zhipu-glm","model":"glm-5.3-flash","reasoning":"max"},
            {"id":"mimo","provider":"xiaomi-mimo","model":"mimo-v2.6-pro","reasoning":"max"},
            {"id":"ds","provider":"deepseek","model":"deepseek-flash","reasoning":"max"}
        ]}))
        .unwrap(),
    );
    let capabilities = serde_json::from_value(json!({
        "capabilityDigest":format!("sha256:{}", "a".repeat(64)),
        "features":[],"maxConcurrentJobs":1,"platform":"aarch64-apple-darwin"
    }))
    .unwrap();
    let config = resolve_agent_session_config(
        &binding.authority.lease.worker_id,
        &capabilities,
        "executor",
        settings,
    )
    .unwrap();
    store
        .save_run(&binding.run_key, &json!({"agentConfig":config}))
        .unwrap();
    let bridge = Arc::new(installed_loopback_bridge(
        &store,
        SharedAuthoritySource::default(),
        "fusion payload",
        &binding.run_key,
        &binding.authority,
        &binding.kernel_session_id,
    ));
    (root, store, bridge, binding)
}

#[test]
fn fusion_member_open_digest_binds_route_and_rejects_invalid_marker() {
    let mut payload = json!({"provider":"primary","request":{"model":"model","input":[]}});
    let primary = super::model_open_request_digest(&payload).unwrap();
    assert_eq!(primary, model_call_digest(&payload["request"]).unwrap());
    payload["provider"] = json!("zhipu-glm");
    assert_eq!(super::model_open_request_digest(&payload).unwrap(), primary);
    payload["fusionMember"] = json!("glm");
    let member = super::model_open_request_digest(&payload).unwrap();
    assert_ne!(member, primary);
    for (field, value) in [("fusionMember", "mimo"), ("provider", "other-provider")] {
        let mut changed = payload.clone();
        changed[field] = json!(value);
        assert_ne!(super::model_open_request_digest(&changed).unwrap(), member);
    }
    for invalid in [json!(null), json!(false), json!(7), json!(""), json!(" ")] {
        payload["fusionMember"] = invalid;
        assert_eq!(
            super::model_open_request_digest(&payload),
            Err(super::BridgeError::InvalidPayload)
        );
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one Fusion call keeps its original identity across pending and completed replay"
)]
async fn fusion_member_bridge_payload_preserves_trusted_member_and_replay_identity() {
    use winwincode_fusion::{FusionBlindPrompt, FusionProvider, FusionProviderRequest};
    let (root, store, bridge, binding) = fusion_payload_fixture();
    let provider = crate::FusionModelPortProvider::new(
        bridge.fusion_model_port("glm".into()),
        binding.kernel_session_id.clone(),
        binding.canonical_thread_id.0.clone(),
        "fusion-turn".into(),
    );
    let input = FusionProviderRequest {
        panel_id: "panel".into(),
        candidate_id: "glm".into(),
        request_id: "fusion-call".into(),
        provider: "zhipu-glm".into(),
        model: "glm-5.3-flash".into(),
        reasoning_effort: Some("max".into()),
        prompt: FusionBlindPrompt {
            question: "Check retained claim".into(),
            canonical_context: json!({"fixture":true}),
            constraints: Vec::new(),
            expected_output_schema: winwincode_fusion::claims::default_claim_output_schema(),
        },
        max_total_tokens: None,
    };
    let mut completion = provider.complete(input.clone());
    assert!(futures::poll!(completion.as_mut()).is_pending());
    let messages = bridge.take_messages().unwrap();
    assert_eq!(messages.len(), 1);
    let ExecutionPortMessage::ModelOpenMessage(open) = &messages[0] else {
        panic!("the real Fusion provider must emit one ModelOpen");
    };
    bridge.outbox.retain(&messages[0]).unwrap();
    let mut payload = super::decode_payload(&open.request).unwrap();
    assert_eq!(payload["fusionMember"], "glm");
    assert_eq!(payload["provider"], input.provider);
    assert_eq!(payload["request"]["model"], input.model);
    assert_eq!(payload["requestId"], input.request_id);
    payload.as_object_mut().unwrap().remove("fusionMember");
    let request = ModelPortRequest {
        request_id: input.request_id.clone(),
        payload_json: payload.to_string(),
    };
    let super::OpenStreamPreparation::Open(exchange) =
        bridge.prepare_member_stream(&request, Some("glm")).unwrap()
    else {
        panic!("the retained pending call must keep its exchange");
    };
    assert_eq!(exchange.model_call_id, input.request_id);
    assert_eq!(exchange.exchange, open.model_exchange_id);
    let repeated = bridge.open_command(&exchange).unwrap();
    assert_eq!(repeated.request, open.request);
    assert_eq!(repeated.metadata.message_id, open.message_id);
    assert_eq!(repeated.request_id, open.request_id);
    assert!(bridge.take_messages().unwrap().is_empty());
    for (sequence, final_frame, payload, message) in [
        (
            1,
            false,
            r#"{"type":"output_item_done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"{\"claims\":[]}"}]}}"#,
            'P',
        ),
        (
            2,
            true,
            r#"{"type":"completed","responseId":"fusion-response","endTurn":true}"#,
            'Q',
        ),
    ] {
        let chunk = test_chunk(
            &binding.authority,
            &open.model_exchange_id,
            sequence,
            final_frame,
            payload.as_bytes(),
            message,
        );
        bridge
            .accept_chunk(&chunk, &binding.opened_at)
            .await
            .unwrap();
    }
    let answer = completion.await.unwrap();
    assert_eq!(answer.answer, json!({"claims":[]}));
    assert_eq!(
        provider.complete(input.clone()).await.unwrap().answer,
        answer.answer
    );
    assert!(
        bridge
            .take_messages()
            .unwrap()
            .iter()
            .all(|message| !matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
    );
    bridge.fence_binding(&binding).unwrap();
    let mut cancelled = input;
    cancelled.request_id = "cancelled-fusion-call".into();
    assert!(provider.complete(cancelled).await.is_err());
    assert!(bridge.take_messages().unwrap().is_empty());
    drop(bridge);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn fusion_member_legacy_pending_open_is_preserved_and_completed_frames_replay() {
    let (root, store, bridge, binding) = fusion_payload_fixture();
    let request = ModelPortRequest {
        request_id: "legacy-fusion-call".into(),
        payload_json: json!({
            "requestId":"legacy-fusion-call","provider":"zhipu-glm",
            "sessionId":binding.kernel_session_id,"threadId":binding.canonical_thread_id.0,
            "request":{"model":"glm-5.3-flash","reasoning":{"effort":"max"},"input":[]}
        })
        .to_string(),
    };
    let super::OpenStreamPreparation::Open(exchange) =
        bridge.prepare_member_stream(&request, Some("glm")).unwrap()
    else {
        panic!("the legacy pending call must have an exchange");
    };
    let mut command = bridge.open_command(&exchange).unwrap();
    command.request = encoded(request.payload_json.as_bytes());
    bridge.client.lock().await.open(command).await.unwrap();
    let messages = bridge.take_messages().unwrap();
    assert_eq!(messages.len(), 1);
    let ExecutionPortMessage::ModelOpenMessage(legacy) = &messages[0] else {
        panic!("retain the original legacy ModelOpen");
    };
    bridge.outbox.retain(&messages[0]).unwrap();
    assert_eq!(
        bridge
            .open_command(&exchange)
            .err()
            .expect("the legacy pending open must conflict")
            .code,
        "MODEL_BRIDGE_CONFLICT"
    );
    assert_eq!(
        bridge
            .outbox
            .retained_model_open(&exchange.exchange)
            .unwrap()
            .as_ref(),
        Some(legacy)
    );
    assert!(bridge.take_messages().unwrap().is_empty());
    let chunk = final_chunk(legacy, 'L');
    store
        .retain_model_call_frame(&binding.run_key, &request.request_id, &chunk)
        .unwrap();
    store
        .mark_model_call_provider_final(&binding.run_key, &request.request_id)
        .unwrap();
    let super::OpenStreamPreparation::Replay(replay) =
        bridge.prepare_member_stream(&request, Some("glm")).unwrap()
    else {
        panic!("completed legacy frames must replay without reopening");
    };
    let frames: Vec<_> = replay.collect().await;
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].as_ref().unwrap(),
        r#"{"type":"response.completed","call":"L"}"#
    );
    assert_eq!(
        bridge
            .outbox
            .retained_model_open(&exchange.exchange)
            .unwrap()
            .as_ref(),
        Some(legacy)
    );
    assert!(bridge.take_messages().unwrap().is_empty());
    drop(bridge);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
