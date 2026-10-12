// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[test]
#[ignore = "heavy mechanism regression: incremental durable post-action source and trace writes"]
fn m09_real_post_action_intents_keep_incremental_source_and_trace_cost() {
    use crate::storage_mechanism_regression as measure;
    use winwincode_execution_port::{
        action_normalizer::{ActionOperation, ActionSource},
        repository_rule_pack::{PostActionHook, PostActionOutcome},
    };
    let mut totals = Vec::new();
    for size in [100_u64, 200, 400] {
        let root = test_root("post-action-history-mechanism");
        let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        let (mut record, mut binding) = delegated_record_and_binding();
        binding.authority.lease.fencing_token = FencingToken("1".into());
        binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
        binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
        binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
        record.workspace = root.join("workspace");
        std::fs::create_dir_all(&record.workspace).unwrap();
        let key = binding.run_key.clone();
        adapter
            .install_active_run(&key, record, binding, false, false)
            .unwrap();
        let now = Instant("2026-08-28T00:00:02.000Z".into());
        measure::reset();
        for sequence in 1..=size {
            let event = adapter
                .retain_post_action_trace(
                    &key,
                    &format!("offline-command-{sequence:06}"),
                    ActionSource::Shell,
                    ActionOperation::Execute,
                    PostActionOutcome::Succeeded,
                    vec![PostActionHook::RequireVerification],
                    &now,
                )
                .unwrap();
            assert!(event.is_some());
        }
        let metrics = measure::finish();
        assert_eq!(metrics.run_writes, 0);
        assert_eq!(metrics.post_action_writes, 2 * size);
        assert_eq!(metrics.post_action_lookups, 3 * size);
        assert_eq!(
            adapter.runs[&key].record.post_action_traces.len(),
            usize::try_from(size).unwrap()
        );
        assert!(
            adapter.runs[&key]
                .record
                .post_action_traces
                .iter()
                .all(|trace| trace.retained)
        );
        measure::reset();
        assert!(
            adapter
                .retain_post_action_trace(
                    &key,
                    "empty-action",
                    ActionSource::Shell,
                    ActionOperation::Execute,
                    PostActionOutcome::Succeeded,
                    vec![],
                    &now
                )
                .unwrap()
                .is_none()
        );
        assert!(
            adapter
                .retain_post_action_trace(
                    &key,
                    "offline-command-000001",
                    ActionSource::Shell,
                    ActionOperation::Execute,
                    PostActionOutcome::Succeeded,
                    vec![PostActionHook::RequireVerification],
                    &now
                )
                .unwrap()
                .is_none()
        );
        let noops = measure::finish();
        assert_eq!(noops.run_writes, 0);
        assert_eq!(noops.run_write_bytes, 0);
        assert_eq!(noops.post_action_writes, 0);
        assert_eq!(noops.post_action_write_bytes, 0);
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M09", "size":size, "metrics":metrics, "noops":noops, "retained_traces":size, "classification":"conditional nonempty post-action hook cost"})
        );
        totals.push(metrics.post_action_write_bytes);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(totals[1] < 3 * totals[0]);
    assert!(totals[2] < 3 * totals[1]);
}

#[test]
fn incremental_post_action_intents_use_indexed_sources_and_trace_commits() {
    use crate::storage_mechanism_regression as measure;
    use winwincode_execution_port::{
        action_normalizer::{ActionOperation, ActionSource},
        repository_rule_pack::{PostActionHook, PostActionOutcome},
    };
    let mut totals = Vec::new();
    for size in [100_u64, 200, 400] {
        let root = test_root("post-action-history-mechanism");
        let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        let (mut record, mut binding) = delegated_record_and_binding();
        binding.authority.lease.fencing_token = FencingToken("1".into());
        binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
        binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
        binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
        record.workspace = root.join("workspace");
        std::fs::create_dir_all(&record.workspace).unwrap();
        let key = binding.run_key.clone();
        adapter
            .install_active_run(&key, record, binding, false, false)
            .unwrap();
        let now = Instant("2026-08-28T00:00:02.000Z".into());
        measure::reset();
        for sequence in 1..=size {
            let event = adapter
                .retain_post_action_trace(
                    &key,
                    &format!("offline-command-{sequence:06}"),
                    ActionSource::Shell,
                    ActionOperation::Execute,
                    PostActionOutcome::Succeeded,
                    vec![PostActionHook::RequireVerification],
                    &now,
                )
                .unwrap();
            assert!(event.is_some());
        }
        let metrics = measure::finish();
        assert_eq!(metrics.run_writes, 0);
        assert_eq!(metrics.post_action_writes, 2 * size);
        assert_eq!(metrics.post_action_lookups, 3 * size);
        assert_eq!(
            adapter.runs[&key].record.post_action_traces.len(),
            usize::try_from(size).unwrap()
        );
        assert!(
            adapter.runs[&key]
                .record
                .post_action_traces
                .iter()
                .all(|trace| trace.retained)
        );
        measure::reset();
        assert!(
            adapter
                .retain_post_action_trace(
                    &key,
                    "empty-action",
                    ActionSource::Shell,
                    ActionOperation::Execute,
                    PostActionOutcome::Succeeded,
                    vec![],
                    &now
                )
                .unwrap()
                .is_none()
        );
        assert!(
            adapter
                .retain_post_action_trace(
                    &key,
                    "offline-command-000001",
                    ActionSource::Shell,
                    ActionOperation::Execute,
                    PostActionOutcome::Succeeded,
                    vec![PostActionHook::RequireVerification],
                    &now
                )
                .unwrap()
                .is_none()
        );
        let noops = measure::finish();
        assert_eq!(noops.run_writes, 0);
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M09", "size":size, "metrics":metrics, "noops":noops, "retained_traces":size, "classification":"conditional nonempty post-action hook cost"})
        );
        totals.push(metrics.post_action_write_bytes);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(totals[1] < 3 * totals[0]);
    assert!(totals[2] < 3 * totals[1]);
}
