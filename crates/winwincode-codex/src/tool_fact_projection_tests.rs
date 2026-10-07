// SPDX-License-Identifier: Apache-2.0
use super::super::load_stored_run;
use super::super::tests::{delegated_record_and_binding, diagnostic_adapter_config};
use super::*;
use winwincode_domain::{FencingToken, LeaseId};
use winwincode_execution_port::replay::ReplayAcknowledgementStore;

#[tokio::test]
async fn retained_terminal_cannot_overtake_a_durable_core_fact() {
    let root =
        std::env::temp_dir().join(format!("wwc-terminal-core-fact-{}", uuid::Uuid::new_v4()));
    let (mut record, mut binding) = delegated_record_and_binding();
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    binding.authority.lease.fencing_token = FencingToken("1".into());
    binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
    let key = binding.run_key.clone();
    let session = record.kernel_session_id.clone();
    let thread = binding.canonical_thread_id.clone();
    let now = Instant("2026-08-28T00:00:02.000Z".into());
    record.terminal = Some(super::super::StoredTerminal::Cancelled {
        artifacts: Vec::new(),
    });
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    adapter
        .install_active_run(&key, record, binding.clone(), false, false)
        .unwrap();
    adapter.bridge.authority().update_now(&now).unwrap();
    let message = adapter
        .prepare_tool_fact(
            &key,
            &session,
            KernelToolRuntimeEvent {
                source_sequence: 17,
                fact_json:
                    "{\"kind\":\"cell\",\"fact\":{\"schema_version\":1,\"lifecycle\":\"closed\"}}"
                        .into(),
            },
            &now,
        )
        .unwrap();
    adapter.runs.get_mut(&key).unwrap().record.core_tool_pending = Some(message.clone());
    adapter.persist_run(&key).unwrap();
    let polled = crate::CodexCoreAdapter::poll(&mut adapter, &thread, &now)
        .await
        .unwrap();
    let CodexPoll::RuntimeTrace(actual) = polled else {
        panic!("Core fact must precede terminal: {polled:?}")
    };
    assert_eq!(*actual, message);
    let record = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    assert_eq!(record.core_tool_cursor, 17);
    assert!(record.core_tool_pending.is_none());
    assert!(record.terminal.is_some());
    assert!(matches!(
        crate::CodexCoreAdapter::poll(&mut adapter, &thread, &now)
            .await
            .unwrap(),
        CodexPoll::Cancelled { .. }
    ));
    let record = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    assert_eq!(record.core_tool_final_cursor, Some(17));
    drop(adapter);
    let mut restarted = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    restarted
        .install_active_run(&key, record, binding, false, true)
        .unwrap();
    restarted.bridge.authority().update_now(&now).unwrap();
    let CodexPoll::RuntimeTrace(replayed) =
        crate::CodexCoreAdapter::poll(&mut restarted, &thread, &now)
            .await
            .unwrap()
    else {
        panic!("unacknowledged final fact must replay after restart");
    };
    assert_eq!(*replayed, message);
    assert!(matches!(
        crate::CodexCoreAdapter::poll(&mut restarted, &thread, &now)
            .await
            .unwrap(),
        CodexPoll::Cancelled { .. }
    ));
    drop(restarted);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn pending_core_projection_repairs_replay_outbox_crash_and_keeps_source_cursor() {
    for (replay_retained, acknowledged) in [(false, false), (true, false), (true, true)] {
        let root =
            std::env::temp_dir().join(format!("wwc-core-fact-projection-{}", uuid::Uuid::new_v4()));
        let (mut record, mut binding) = delegated_record_and_binding();
        record.workspace = root.join("workspace");
        std::fs::create_dir_all(&record.workspace).unwrap();
        binding.authority.lease.fencing_token = FencingToken("1".into());
        binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
        binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
        binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
        let key = binding.run_key.clone();
        let session = record.kernel_session_id.clone();
        let now = Instant("2026-08-28T00:00:02.000Z".into());
        let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        adapter
            .install_active_run(&key, record, binding.clone(), false, false)
            .unwrap();
        adapter.bridge.authority().update_now(&now).unwrap();
        let mut message = adapter
            .prepare_tool_fact(
                &key,
                &session,
                KernelToolRuntimeEvent {
                    source_sequence: 17,
                    fact_json: "{\"kind\":\"request\",\"fact\":{\"schema_version\":1}}".into(),
                },
                &now,
            )
            .unwrap();
        adapter.runs.get_mut(&key).unwrap().record.core_tool_pending = Some(message.clone());
        adapter.persist_run(&key).unwrap();
        // Crash after retaining the Core projection in replay, before its outbox write.
        if replay_retained {
            RuntimeReplayResponder::default()
                .retain_runtime_event(&mut adapter.store, &adapter.bridge.authority(), &message)
                .unwrap();
            if acknowledged {
                let stream = RuntimeReplayIdentity {
                    lease: message.lease.clone(),
                    worker_session_id: message.worker_session_id.clone(),
                    session_identity: message.session_identity.clone(),
                    codex_thread_id: message.codex_thread_id.clone(),
                }
                .stream_key();
                adapter.store.record_acknowledgement(&stream, 0, 1).unwrap();
            }
        }
        drop(adapter);
        binding.authority.lease.expires_at = Instant("2026-08-28T02:00:00.000Z".into());
        if !replay_retained {
            message.lease = binding.authority.lease.clone();
        }
        let mut restarted = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        let record = load_stored_run(&restarted.store, &key).unwrap().unwrap();
        restarted
            .install_active_run(&key, record, binding, false, true)
            .unwrap();
        restarted.bridge.authority().update_now(&now).unwrap();
        assert_eq!(
            restarted
                .runs
                .get(&key)
                .unwrap()
                .replay
                .iter()
                .any(|frame| frame == &message),
            !acknowledged
        );
        let record = load_stored_run(&restarted.store, &key).unwrap().unwrap();
        assert_eq!(record.core_tool_cursor, 17);
        assert!(record.core_tool_pending.is_none());
        let retained: Vec<_> = restarted
            .outbox
            .pending()
            .unwrap()
            .into_iter()
            .filter_map(|delivery| match delivery.message {
                ExecutionPortMessage::RuntimeEventMessage(frame) => Some(frame),
                _ => None,
            })
            .collect();
        assert_eq!(retained, if acknowledged { vec![] } else { vec![message] });
        drop(restarted);
        let _ = std::fs::remove_dir_all(root);
    }
}
