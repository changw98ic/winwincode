// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
fn task_handoff_storage_fixture(
    size: u64,
    name: &str,
) -> (PathBuf, ProductionCodexAdapter, String) {
    use winwincode_execution_port::{
        action_normalizer::{ActionOperation, ActionSource},
        repository_rule_pack::{PostActionHook, PostActionOutcome},
        task_handoff::TaskHandoffRecord,
    };

    let root = test_root(name);
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let (mut record, mut binding) = delegated_record_and_binding();
    binding.authority.lease.fencing_token = FencingToken("1".into());
    binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    record.task_handoff = Some(TaskHandoffRecord::from_job(
        &record.job,
        &record.workspace.to_string_lossy(),
    ));
    let key = binding.run_key.clone();
    adapter
        .install_active_run(&key, record, binding, false, false)
        .unwrap();
    let now = Instant("2026-08-28T00:00:02.000Z".into());
    for sequence in 1..=size {
        assert!(
            adapter
                .retain_post_action_trace(
                    &key,
                    &format!("offline-command-{sequence:06}"),
                    ActionSource::Shell,
                    ActionOperation::Execute,
                    PostActionOutcome::Succeeded,
                    vec![PostActionHook::RequireVerification],
                    &now,
                )
                .unwrap()
                .is_some()
        );
    }
    (root, adapter, key)
}

#[cfg(unix)]
fn task_handoff_raw_header(adapter: &ProductionCodexAdapter, key: &str) -> serde_json::Value {
    let bytes: Vec<u8> = adapter
        .store
        .lock()
        .unwrap()
        .query_row(
            "SELECT record_json FROM codex_run WHERE run_key=?1",
            [key],
            |row| row.get(0),
        )
        .unwrap();
    let header: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(header.get("postActionTraces").is_none());
    header
}

#[cfg(unix)]
fn task_handoff_assert_full_audit(
    adapter: &ProductionCodexAdapter,
    key: &str,
    size: u64,
) -> crate::storage_mechanism_regression::Metrics {
    use crate::storage_mechanism_regression as measure;

    measure::reset();
    let full: serde_json::Value = adapter.store.load_run(key).unwrap().unwrap();
    let metrics = measure::finish();
    assert_eq!(metrics.post_action_read_rows, size);
    assert_eq!(metrics.post_action_validations, size);
    assert_eq!(
        full["postActionTraces"].as_array().unwrap().len() as u64,
        size
    );
    let original: (String, Vec<u8>) = adapter
        .store
        .lock()
        .unwrap()
        .query_row(
            "SELECT source_key,record_json FROM post_action_trace WHERE run_key=?1 ORDER BY sequence LIMIT 1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    adapter
        .store
        .lock()
        .unwrap()
        .execute(
            "UPDATE post_action_trace SET record_json=?3 WHERE run_key=?1 AND source_key=?2",
            rusqlite::params![key, original.0, b"[]".as_slice()],
        )
        .unwrap();
    assert_eq!(
        adapter.store.load_run::<serde_json::Value>(key),
        Err(crate::store::AdapterStoreError::Corrupt)
    );
    adapter
        .store
        .lock()
        .unwrap()
        .execute(
            "UPDATE post_action_trace SET record_json=?3 WHERE run_key=?1 AND source_key=?2",
            rusqlite::params![key, original.0, original.1],
        )
        .unwrap();
    assert_eq!(
        crate::task_handoff::context_snapshot(&full).unwrap(),
        crate::task_handoff::context_snapshot(&task_handoff_raw_header(adapter, key)).unwrap()
    );
    metrics
}

#[cfg(unix)]
#[test]
fn task_handoff_persist_keeps_skipped_history_uncloned_and_status_current() {
    use crate::storage_mechanism_regression as measure;

    let mut samples = Vec::new();
    for size in [100_u64, 200, 400] {
        let (root, mut adapter, key) =
            task_handoff_storage_fixture(size, "task-handoff-persist-history");
        let run = adapter.runs.get_mut(&key).unwrap();
        run.record.terminal = Some(super::StoredTerminal::Completed {
            summary: "observed execution completed".into(),
            final_message: Some("done".into()),
            artifacts: Vec::new(),
            usage: None,
        });
        let mut expected = serde_json::to_value(&run.record).unwrap();
        expected["taskHandoff"]["Status"] = serde_json::json!("execution_stopped");
        measure::reset();
        adapter.persist_run(&key).unwrap();
        let metrics = measure::finish();
        assert_eq!(task_handoff_raw_header(&adapter, &key), expected);
        let in_memory_status_current = adapter.runs[&key]
            .record
            .task_handoff
            .as_ref()
            .unwrap()
            .status
            == "execution_stopped";
        let full_audit = task_handoff_assert_full_audit(&adapter, &key, size);
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({
                "id":"TASK_HANDOFF_PERSIST", "size":size, "metrics":metrics,
                "in_memory_status_current":in_memory_status_current,
                "full_audit":full_audit, "corrupt_ledger_rejected":true,
                "other_header_fields_preserved":true,
                "classification":"actual persist_run cost after retained post-action history"
            })
        );
        samples.push((size, metrics, in_memory_status_current));
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }
    for (size, metrics, status_current) in samples {
        assert_eq!(metrics.post_action_trace_clones, 0, "history size {size}");
        assert_eq!(metrics.post_action_read_rows, 0);
        assert_eq!(metrics.run_writes, 1);
        assert!(status_current, "persisted and in-memory task status agree");
    }
}

#[cfg(unix)]
#[test]
fn task_handoff_model_consumers_read_header_without_post_action_history() {
    use crate::storage_mechanism_regression as measure;

    let mut samples = Vec::new();
    for size in [100_u64, 200, 400] {
        let (root, adapter, key) = task_handoff_storage_fixture(size, "task-handoff-model-history");
        let header = task_handoff_raw_header(&adapter, &key);
        let expected_context = crate::task_handoff::context_snapshot(&header)
            .unwrap()
            .unwrap();
        let binding = &adapter.runs[&key].binding;
        let port = adapter.bridge.model_port();
        measure::reset();
        let overhead = port
            .compaction_context_overhead_bytes(&binding.canonical_thread_id.0)
            .unwrap();
        let overhead_metrics = measure::finish();
        assert_eq!(overhead, expected_context.len());
        let request = winwincode_kernel::ModelPortRequest {
            request_id: "header-cost-request".into(),
            payload_json: serde_json::json!({
                "requestId":"header-cost-request", "provider":"fixture-provider",
                "sessionId":binding.kernel_session_id,
                "threadId":binding.canonical_thread_id.0,
                "request":{"model":"fixture-model", "input":[],
                    "instructions":"original role instructions"}
            })
            .to_string(),
        };
        measure::reset();
        adapter
            .bridge
            .prepare_open_stream_for_storage_measurement(&request)
            .unwrap();
        let preparation_metrics = measure::finish();
        let full_audit = task_handoff_assert_full_audit(&adapter, &key, size);
        let mut wrong_binding = header.clone();
        wrong_binding["taskHandoff"]["Task"]["jobId"] =
            serde_json::json!("job_00000000000000000000000002");
        adapter.store.save_run(&key, &wrong_binding).unwrap();
        assert!(
            port.compaction_context_overhead_bytes(&binding.canonical_thread_id.0)
                .is_err()
        );
        assert!(
            adapter
                .bridge
                .prepare_open_stream_for_storage_measurement(&request)
                .is_err()
        );
        adapter.store.save_run(&key, &header).unwrap();
        assert_eq!(task_handoff_raw_header(&adapter, &key), header);
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({
                "id":"TASK_HANDOFF_MODEL_HEADER", "size":size,
                "overhead_metrics":overhead_metrics,
                "preparation_metrics":preparation_metrics,
                "context_bytes":overhead,
                "full_audit":full_audit, "corrupt_ledger_rejected":true,
                "context_full_equals_header":true,
                "task_binding_mismatch_rejected":true,
                "classification":"actual Kernel ModelPort overhead and prepare_member_stream"
            })
        );
        samples.push((size, overhead_metrics, preparation_metrics));
        drop(port);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }
    for (size, overhead, preparation) in samples {
        for metrics in [overhead, preparation] {
            assert_eq!(metrics.post_action_read_rows, 0, "history size {size}");
            assert_eq!(metrics.post_action_validations, 0);
            assert_eq!(metrics.post_action_trace_clones, 0);
            assert_eq!(metrics.run_header_reads, 1);
        }
    }
}
