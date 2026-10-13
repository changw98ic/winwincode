// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
#[test]
fn verification_start_delivers_every_retained_trace_before_next_core_poll() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Box::pin(verification_start_delivery_body()));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[cfg(unix)]
async fn verification_start_delivery_body() {
    for role in ["reviewer", "verifier"] {
        let root = test_root(&format!("{role}-start-delivery"));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut job = executor_job();
        job.execution_profile = role.into();
        job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
        job.work_input.as_mut().unwrap().candidate_ref =
            Some(format!("refs/winwincode/candidates/{}", "a".repeat(64)));
        let (_, binding) = delegated_record_and_binding();
        let mut lease = binding.authority.lease;
        lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
        lease.fencing_token = FencingToken("1".into());
        let worker_session = binding.authority.worker_session_id;
        let run_key = CodexRunKey {
            job_id: job.job_id.clone(),
            attempt: job.attempt,
            fencing_token: lease.fencing_token.clone(),
            payload_digest: job.payload_digest.clone(),
        };
        let revision = WorkspaceRevision(format!("git-tree:{}", "1".repeat(40)));
        let snapshot = winwincode_domain::SnapshotId("snap_00000000000000000000000001".into());
        let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        let session = Box::pin(adapter.ensure_thread(CodexThreadStart {
            snapshot_id: Some(&snapshot),
            run_key: &run_key,
            worker_id: &lease.worker_id,
            job: &job,
            lease: &lease,
            worker_session_id: &worker_session,
            workspace: &workspace,
            workspace_revision: &revision,
        }))
        .await
        .unwrap();
        let key = run_key.canonical_digest().unwrap().0;
        let now = lease.issued_at.clone();
        let CodexPoll::RuntimeTrace(first) = adapter
            .accept_turn_started(&key, "verification-turn", &now)
            .unwrap()
        else {
            panic!("verification policy must be delivered first");
        };
        assert_eq!(first.event.sequence.0, 1);
        let retained: Vec<_> = adapter
            .pending_execution_deliveries()
            .unwrap()
            .into_iter()
            .filter_map(|delivery| match delivery.message {
                ExecutionPortMessage::RuntimeEventMessage(event) => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0], *first);
        let next = Box::pin(adapter.poll(&session.thread_id, &now))
            .await
            .unwrap();
        let CodexPoll::RuntimeTrace(second) = next else {
            panic!("retained start trace must precede another Core poll: {next:?}");
        };
        assert_eq!(*second, retained[1]);
        assert_eq!(second.event.sequence.0, 2);
        assert!(matches!(
            adapter
                .accept_turn_started(&key, "verification-turn", &now)
                .unwrap(),
            CodexPoll::Pending
        ));
        assert_eq!(adapter.pending_execution_deliveries().unwrap().len(), 2);
        drop(adapter);
        std::fs::remove_dir_all(root).unwrap();
    }
}
