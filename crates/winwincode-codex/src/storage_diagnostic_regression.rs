// SPDX-License-Identifier: Apache-2.0

#[test]
#[ignore = "mechanism audit: retained diagnostic artifact validation cost baseline"]
fn m08_real_diagnostic_queries_validate_acked_artifacts_of_other_jobs() {
    use crate::storage_mechanism_regression as measure;
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, LeaseId, WorkItemId, WorkRunId, WorkerSessionId,
    };
    for size in [100_u64, 200, 400] {
        let root = test_root("history-mechanism");
        let store = AdapterStore::open(&root).unwrap();
        let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
        for sequence in 1..=size {
            let identity_seed = 1_000_000 + sequence;
            let mut upload = fixture_upload(vec![b'x'; 1_024]);
            upload.run_key = format!("offline-run-{sequence}");
            upload.job.job_id = ExecutionJobId(format!("job_{identity_seed:026}"));
            upload.lease.job_id = upload.job.job_id.clone();
            upload.lease.lease_id = LeaseId(format!("lse_{identity_seed:026}"));
            let work_run_id = WorkRunId(format!("wrn_{identity_seed:026}"));
            let ExecutionScope::WorkRunExecutionScope(scope) = &mut upload.scope else {
                panic!("canonical WorkRun diagnostic fixture")
            };
            scope.work_run_id = work_run_id.clone();
            scope.work_item_id = WorkItemId(format!("wit_{identity_seed:026}"));
            upload.job.work_input.as_mut().unwrap().work_item.id = scope.work_item_id.clone();
            upload.job.scope = upload.scope.clone();
            upload.session_identity.work_run_id = Some(work_run_id);
            upload.worker_session_id = WorkerSessionId(format!("wsn_{identity_seed:026}"));
            upload.session_identity.worker_session_id = upload.worker_session_id.clone();
            upload.session_identity.codex_thread_id =
                CodexThreadId(format!("cdx_{identity_seed:026}"));
            let retained = outbox.retain(&upload).unwrap();
            assert!(matches!(
                outbox.apply_ack(&ack(&upload, &retained)).unwrap(),
                DiagnosticArtifactAckOutcome::Accepted { .. }
            ));
        }
        let current_upload = fixture_upload(vec![b'y'; 1_024]);
        let current = DiagnosticArtifactAuthority {
            snapshot_id: current_upload.snapshot_id.clone(),
            job: current_upload.job.clone(),
            scope: current_upload.scope.clone(),
            lease: current_upload.lease.clone(),
            worker_session_id: current_upload.worker_session_id.clone(),
            session_identity: current_upload.session_identity.clone(),
        };
        measure::reset();
        assert!(!outbox.has_pending(&current).unwrap());
        assert!(outbox.accepted_references(&current).unwrap().is_empty());
        let metrics = measure::finish();
        assert_eq!(metrics.diagnostic_validations, 2 * size);
        assert_eq!(metrics.diagnostic_raw_bytes, 2 * size * 1_024);
        let rows: i64 = store
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM diagnostic_artifact_upload",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, i64::try_from(size).unwrap());
        let transport_rows: i64 = store
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM execution_outbox", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(transport_rows, 0);
        // This fault is confined to an unrelated record in this temporary fixture.
        store.lock().unwrap().execute("UPDATE diagnostic_artifact_upload SET record_json = ?1 WHERE artifact_id = (SELECT MIN(artifact_id) FROM diagnostic_artifact_upload)", [b"{}".as_slice()]).unwrap();
        assert!(outbox.has_pending(&current).is_err());
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M08", "size":size, "metrics":metrics, "records_after_final_ack":rows, "transport_rows_after_final_ack":transport_rows, "current_job_empty":true, "unrelated_corrupt_record_blocks_query":true, "classification":"conditional terminal-history cost"})
        );
        drop(outbox);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
