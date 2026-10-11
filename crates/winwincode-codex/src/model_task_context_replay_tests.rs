// SPDX-License-Identifier: Apache-2.0

fn task_context_replay_job(
    lease: &ModelLeaseAuthority,
) -> winwincode_execution_port::generated::ExecutionJob {
    serde_json::from_value(json!({
        "attempt": lease.lease.attempt,
        "executionProfile": "executor",
        "goal": "Preserve the exact synthetic task context request",
        "jobId": lease.lease.job_id,
        "limits": {"deadlineAt": null, "maxArtifactBytes": 1_048_576, "maxRuntimeSeconds": null},
        "payloadDigest": format!("sha256:{}", "a".repeat(64)),
        "scope": {"kind": "product-session", "productSessionId": lease.session_identity.product_session_id},
        "workspace": {"checkoutRevision": "synthetic-source-revision", "repositoryId": id("repo", 'A'), "writeMode": "candidate"}
    }))
    .unwrap()
}

fn task_context_replay_open(bridge: &ExecutionPortModelBridge) -> ModelOpenMessage {
    let messages = bridge.take_messages().unwrap();
    assert_eq!(messages.len(), 1, "one exact queued ModelOpen");
    match messages.into_iter().next().unwrap() {
        ExecutionPortMessage::ModelOpenMessage(open) => open,
        other => panic!("expected actual ModelOpen, received {other:?}"),
    }
}

fn task_context_replay_ledger_digest(store: &AdapterStore, call: &str) -> String {
    store
        .lock()
        .unwrap()
        .query_row(
            "SELECT request_digest FROM model_call_ledger WHERE run_key='run' AND model_call_id=?1",
            [call],
            |row| row.get(0),
        )
        .unwrap()
}

#[allow(
    clippy::too_many_lines,
    reason = "One exact live or database restart scenario binds semantic admission, frozen wire request and completed response replay"
)]
async fn run_task_context_replay(database_reopened: bool, jev_enabled: bool) {
    use winwincode_execution_port::agent_config::{
        AgentSessionConfigSnapshot, resolve_agent_session_config,
    };
    use winwincode_execution_port::task_handoff::TaskHandoffRecord;

    let root =
        std::env::temp_dir().join(format!("wwc-task-context-replay-{}", uuid::Uuid::now_v7()));
    let store = AdapterStore::open(&root).unwrap();
    let lease = authority(&id("cdx", 'A'));
    let job = task_context_replay_job(&lease);
    let workspace = root.join("candidate").to_string_lossy().into_owned();
    let mut agent_config = sealed_profile("loopback", "loopback", "provider_default");
    if jev_enabled {
        let base: AgentSessionConfigSnapshot = serde_json::from_value(agent_config).unwrap();
        let mut settings = base.profile.source.settings;
        settings.jev_context = Some(
            serde_json::from_value(json!({
                "provider": "synthetic-offline-jev",
                "policy": {"version": "synthetic-replay-v1", "minimumConfidence": 0.6,
                    "pinThreshold": 0.8, "keepThreshold": 0.8, "compactThreshold": 0.8,
                    "dropThreshold": 0.8, "taskMemoryThreshold": 0.5,
                    "projectMemoryThreshold": 0.7, "longTermMemoryThreshold": 0.9}
            }))
            .unwrap(),
        );
        settings.jev_judge = Some("synthetic-offline-judge".into());
        agent_config = serde_json::to_value(
            resolve_agent_session_config(
                &base.identity.worker_id,
                &base.identity.capabilities,
                &base.identity.role,
                settings,
            )
            .unwrap(),
        )
        .unwrap();
    }
    let mut run = json!({
        "job": job, "workspace": workspace,
        "jobDigest": crate::stage_product::stage_product_job_digest(&job).unwrap(),
        "agentConfig": agent_config,
        "taskHandoff": TaskHandoffRecord::from_job(&job, &workspace)
    });
    store.save_run("run", &run).unwrap();
    store
        .register_performance_run("run", ExecutionMode::React, ObserverMode::Off)
        .unwrap();
    let bridge = Arc::new(installed_loopback_bridge(
        &store,
        SharedAuthoritySource::default().with_store(store.clone()),
        "task context replay",
        "run",
        &lease,
        "kernel",
    ));
    let request = ModelPortRequest {
        request_id: "task-context-call".into(),
        payload_json: json!({
            "requestId": "task-context-call", "provider": "loopback", "sessionId": "kernel",
            "threadId": lease.session_identity.codex_thread_id.0,
            "request": {"model": "loopback", "instructions": "Exact original role instructions.", "input": []}
        }).to_string(),
    };
    let raw: serde_json::Value = serde_json::from_str(&request.payload_json).unwrap();
    for field in [
        "winwincodeJevContext",
        "winwincodeJevTask",
        "winwincodeJevJudge",
        "unexpectedHostField",
    ] {
        let mut injected = request.clone();
        let mut payload = raw.clone();
        payload[field] = json!({"synthetic": "untrusted Core injection"});
        injected.payload_json = payload.to_string();
        assert!(
            bridge.model_port().stream(injected).await.is_err(),
            "Core cannot supply host-owned or unknown envelope field {field}"
        );
        assert!(bridge.take_messages().unwrap().is_empty());
        assert_eq!(
            store.model_call_phase("run", &request.request_id).unwrap(),
            None
        );
    }
    for malformed in [serde_json::Value::Null, json!([])] {
        let mut invalid = request.clone();
        let mut payload = raw.clone();
        payload["request"] = malformed;
        invalid.payload_json = payload.to_string();
        assert!(
            bridge.model_port().stream(invalid).await.is_err(),
            "Core request must be an object"
        );
        assert!(bridge.take_messages().unwrap().is_empty());
        assert_eq!(
            store.model_call_phase("run", &request.request_id).unwrap(),
            None
        );
    }
    let raw_digest = model_call_digest(&raw["request"]).unwrap();
    let live = bridge.model_port().stream(request.clone()).await.unwrap();
    let open = task_context_replay_open(&bridge);
    // The adapter persists each queued ModelOpen before transport. Exercise that
    // real outbox barrier, rather than calling prepare_host_payload in isolation.
    bridge
        .outbox
        .retain(&ExecutionPortMessage::ModelOpenMessage(open.clone()))
        .unwrap();
    let wire = super::decode_payload(&open.request).unwrap();
    if jev_enabled {
        assert_eq!(wire["winwincodeJevContext"], run["agentConfig"]);
        assert_eq!(wire["winwincodeJevTask"]["job"], run["job"]);
        assert_eq!(wire["winwincodeJevTask"]["jobDigest"], run["jobDigest"]);
        assert_eq!(
            wire["winwincodeJevJudge"],
            json!([]),
            "empty input prepares no Judge call"
        );
    }
    let wire_digest = model_call_digest(&wire["request"]).unwrap();
    assert_ne!(
        raw_digest, wire_digest,
        "the host suffix changes the wire request"
    );
    assert_eq!(
        task_context_replay_ledger_digest(&store, &request.request_id),
        raw_digest.0
    );
    let instructions = wire["request"]["instructions"].as_str().unwrap();
    assert!(instructions.starts_with("Exact original role instructions."));
    for field in [
        "Task",
        "Status",
        "Workspace",
        "Validation",
        "Changes",
        "Dependencies",
        "Unverified",
    ] {
        assert!(
            instructions.contains(&format!("\n{field}: ")),
            "full seven-field host record: {field}"
        );
    }
    assert!(instructions.contains("synthetic-source-revision"));
    assert!(instructions.contains("unverified_by_controller"));
    run["taskHandoff"]["Unverified"]["late-result"] = json!("new receipt not yet observed");
    store.save_run("run", &run).unwrap();
    let updated = crate::task_handoff::context_snapshot(&run)
        .unwrap()
        .unwrap();
    assert!(updated.contains("late-result"));
    assert!(!instructions.contains("late-result"));
    assert_eq!(
        store.model_call_phase("run", &request.request_id).unwrap(),
        Some(ModelCallPhase::InFlight)
    );
    assert_eq!(local_cancel_seals(&store), 0);
    drop(live);

    let (store, bridge) = if database_reopened {
        drop(bridge);
        drop(store);
        let reopened = AdapterStore::open(&root).unwrap();
        let restarted = Arc::new(installed_loopback_bridge(
            &reopened,
            SharedAuthoritySource::default().with_store(reopened.clone()),
            "task context actual database reopen",
            "run",
            &lease,
            "kernel",
        ));
        (reopened, restarted)
    } else {
        (store, bridge)
    };
    let mut recovered = request.clone();
    let mut recovered_payload = raw.clone();
    recovered_payload["request"]["client_metadata"] =
        json!({"session_id": "synthetic-recovery", "turn_attempt": 2});
    recovered.payload_json = recovered_payload.to_string();
    let mut resumed = bridge.model_port().stream(recovered.clone()).await
        .expect("same semantic request must reopen the exact retained exchange with its first frozen task context");
    let reopens = bridge.take_messages().unwrap();
    if database_reopened {
        assert_eq!(
            reopens,
            vec![ExecutionPortMessage::ModelOpenMessage(open.clone())],
            "database restart resends the identical retained exchange, request bytes and authority"
        );
    } else {
        assert!(
            reopens.is_empty(),
            "active exact exchange duplicate adds no ModelOpen"
        );
    }
    assert_eq!(
        bridge
            .outbox
            .retained_model_open(&open.model_exchange_id)
            .unwrap(),
        Some(open.clone())
    );
    assert_eq!(
        task_context_replay_ledger_digest(&store, &request.request_id),
        raw_digest.0
    );
    assert_eq!(local_cancel_seals(&store), 0);
    let wrong_route = Arc::new(ExecutionPortModelBridge::new(
        store.clone(),
        ExecutionOutbox::open(store.clone()).unwrap(),
        ModelGatewayRoute {
            capability: "model".into(),
            route: "different-synthetic-route".into(),
        },
        "loopback".into(),
        SharedAuthoritySource::default().with_store(store.clone()),
    ));
    wrong_route
        .install_binding(ModelRunBinding {
            run_key: "run".into(),
            canonical_thread_id: lease.session_identity.codex_thread_id.clone(),
            kernel_session_id: "kernel".into(),
            authority: lease.clone(),
            opened_at: lease.lease.issued_at.clone(),
        })
        .unwrap();
    let wrong_route_failure = wrong_route
        .model_port()
        .stream(request.clone())
        .await
        .err()
        .unwrap();
    assert_eq!(
        wrong_route_failure.code, "MODEL_BRIDGE_CONFLICT",
        "retained open route remains exact"
    );
    assert!(wrong_route.take_messages().unwrap().is_empty());
    drop(wrong_route);
    for field in ["input", "instructions"] {
        let mut changed = recovered.clone();
        let mut payload = recovered_payload.clone();
        payload["request"][field] = if field == "input" {
            json!([{"role": "user", "content": "different semantic request"}])
        } else {
            json!("Different Core role instructions.")
        };
        changed.payload_json = payload.to_string();
        assert!(
            bridge.model_port().stream(changed).await.is_err(),
            "same call cannot change {field}"
        );
        assert!(bridge.take_messages().unwrap().is_empty());
    }
    let final_frame = final_chunk(&open, 'T');
    bridge
        .accept_chunk(&final_frame, &lease.lease.issued_at)
        .await
        .unwrap();
    assert!(resumed.next().await.unwrap().is_ok());
    assert!(resumed.next().await.is_none());
    bridge.take_messages().unwrap();
    assert_eq!(
        local_cancel_seals(&store),
        1,
        "real complete response keeps its strict seal"
    );

    let mut next_request = request.clone();
    next_request.request_id = "task-context-next-call".into();
    let mut next_payload = raw;
    next_payload["requestId"] = json!(next_request.request_id);
    next_request.payload_json = next_payload.to_string();
    let mut next = bridge
        .model_port()
        .stream(next_request.clone())
        .await
        .unwrap();
    let next_open = task_context_replay_open(&bridge);
    assert_ne!(next_open.model_exchange_id, open.model_exchange_id);
    let next_wire = super::decode_payload(&next_open.request).unwrap();
    assert!(
        next_wire["request"]["instructions"]
            .as_str()
            .unwrap()
            .contains("late-result"),
        "new call takes the current task state"
    );
    bridge
        .outbox
        .retain(&ExecutionPortMessage::ModelOpenMessage(next_open.clone()))
        .unwrap();
    bridge
        .accept_chunk(&final_chunk(&next_open, 'N'), &lease.lease.issued_at)
        .await
        .unwrap();
    assert!(next.next().await.unwrap().is_ok());
    assert!(next.next().await.is_none());
    bridge.take_messages().unwrap();
    assert_eq!(local_cancel_seals(&store), 2);
    drop(next);
    drop(resumed);
    drop(bridge);
    drop(store);

    let final_store = AdapterStore::open(&root).unwrap();
    let final_bridge = Arc::new(installed_loopback_bridge(
        &final_store,
        SharedAuthoritySource::default().with_store(final_store.clone()),
        "task context completed database replay",
        "run",
        &lease,
        "kernel",
    ));
    let mut complete = final_bridge.model_port().stream(recovered).await.unwrap();
    assert!(complete.next().await.unwrap().is_ok());
    assert!(complete.next().await.is_none());
    assert!(
        final_bridge.take_messages().unwrap().is_empty(),
        "sealed completed response opens no exchange"
    );
    assert_eq!(
        final_bridge
            .outbox
            .retained_model_open(&open.model_exchange_id)
            .unwrap(),
        Some(open.clone())
    );
    assert_eq!(
        task_context_replay_ledger_digest(&final_store, &request.request_id),
        raw_digest.0
    );
    assert_eq!(
        final_store
            .load_model_call_frames("run", &request.request_id)
            .unwrap(),
        vec![final_frame]
    );
    assert_eq!(local_cancel_seals(&final_store), 2);
    crate::audit_model_metrics::report(
        if jev_enabled {
            "actual-task-context-jev-database-reopen"
        } else if database_reopened {
            "actual-task-context-database-reopen"
        } else {
            "actual-task-context-live-reopen"
        },
        &json!({"schema_version":1,"inflight_database_reopened":database_reopened,
            "sealed_jev_context_enabled":jev_enabled,"sealed_jev_task_and_empty_judge_preserved":jev_enabled,
            "completed_database_reopened":true,"first_frozen_context_preserved":true,
            "full_seven_field_context_preserved":true,"raw_semantic_ledger_digest_unchanged":true,
            "wire_digest_includes_host_context":true,"changed_semantic_requests_rejected":2,
            "core_host_field_injections_rejected":4,"nonobject_core_requests_rejected":2,"wrong_retained_route_rejected":true,
            "identical_exchange_resends":usize::from(database_reopened),"additional_inflight_exchange_opens":0,
            "new_request_uses_current_task_state":true,"complete_response_seals":2,
            "completed_replay_opens":0,"physical_provider_requests":0}),
    );
    drop(complete);
    drop(final_bridge);
    drop(final_store);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn mechanism_actual_task_context_live_reopen_preserves_frozen_wire_identity() {
    run_task_context_replay(false, false).await;
}

#[tokio::test]
async fn mechanism_actual_task_context_database_reopen_preserves_frozen_wire_identity() {
    run_task_context_replay(true, false).await;
}

#[tokio::test]
async fn mechanism_actual_task_context_jev_database_reopen_preserves_frozen_wire_identity() {
    run_task_context_replay(true, true).await;
}
