// SPDX-License-Identifier: Apache-2.0

#[test]
#[ignore = "heavy mechanism regression: exact Job isolation over foreign diagnostic history"]
fn m08_real_diagnostic_queries_isolate_foreign_acked_history() {
    use crate::storage_mechanism_regression as measure;
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, LeaseId, WorkItemId, WorkRunId, WorkerSessionId,
    };
    let mut empty_query_steps = None;
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
        assert_eq!(metrics.diagnostic_validations, 0);
        assert_eq!(metrics.diagnostic_raw_bytes, 0);
        assert!(
            metrics.diagnostic_query_vm_steps <= 64,
            "empty exact Job query budget"
        );
        if let Some(previous) = empty_query_steps {
            assert_eq!(metrics.diagnostic_query_vm_steps, previous);
        }
        empty_query_steps = Some(metrics.diagnostic_query_vm_steps);
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
        assert!(!outbox.has_pending(&current).unwrap());
        assert!(outbox.accepted_references(&current).unwrap().is_empty());
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M08", "size":size, "metrics":metrics, "records_after_final_ack":rows, "transport_rows_after_final_ack":transport_rows, "current_job_empty":true, "unrelated_corrupt_record_blocks_query":false, "classification":"conditional terminal-history cost"})
        );
        drop(outbox);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn indexed_diagnostic_queries_validate_only_the_exact_job() {
    use crate::storage_mechanism_regression as measure;
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, LeaseId, WorkItemId, WorkRunId, WorkerSessionId,
    };
    let mut empty_query_steps = None;
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
        assert_eq!(metrics.diagnostic_validations, 0);
        assert_eq!(metrics.diagnostic_raw_bytes, 0);
        assert!(
            metrics.diagnostic_query_vm_steps <= 64,
            "empty exact Job query budget"
        );
        if let Some(previous) = empty_query_steps {
            assert_eq!(metrics.diagnostic_query_vm_steps, previous);
        }
        empty_query_steps = Some(metrics.diagnostic_query_vm_steps);
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
        assert!(!outbox.has_pending(&current).unwrap());
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M08", "size":size, "metrics":metrics, "records_after_final_ack":rows, "transport_rows_after_final_ack":transport_rows, "current_job_empty":true, "unrelated_corrupt_record_blocks_query":false, "classification":"conditional terminal-history cost"})
        );
        drop(outbox);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

// Exact current-Job retained-body fault cases. The selected envelope is intact.
fn diagnostic_route_fault_rows(store: &AdapterStore) -> Vec<(String, String, Vec<u8>)> {
    let connection = store.lock().unwrap();
    let mut statement = connection
        .prepare("SELECT artifact_id,authority_key,record_json FROM diagnostic_artifact_upload ORDER BY artifact_id")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

type DiagnosticRouteFaultTransportRow = (i64, String, String, Vec<u8>, String);

fn diagnostic_route_fault_transport_rows(
    store: &AdapterStore,
) -> Vec<DiagnosticRouteFaultTransportRow> {
    let connection = store.lock().unwrap();
    let mut statement = connection
        .prepare("SELECT position,delivery_id,frame_digest,frame_json,state FROM execution_outbox ORDER BY position")
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn erase_exact_diagnostic_body_route(store: &AdapterStore, artifact: &ArtifactId) {
    let original = diagnostic_route_fault_rows(store);
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].0, artifact.0);
    assert_eq!(
        store
            .lock()
            .unwrap()
            .execute(
                "UPDATE diagnostic_artifact_upload SET record_json=?1 WHERE artifact_id=?2",
                rusqlite::params![b"{}".as_slice(), artifact.0],
            )
            .unwrap(),
        1
    );
    let damaged = diagnostic_route_fault_rows(store);
    assert_eq!(damaged[0].0, original[0].0);
    assert_eq!(damaged[0].1, original[0].1);
    assert_eq!(damaged[0].2, b"{}");
}

#[test]
fn diagnostic_current_job_route_loss_cannot_hide_pending_evidence() {
    use crate::storage_mechanism_regression as measure;
    let root = test_root("exact-pending-route-loss");
    let store = AdapterStore::open(&root).unwrap();
    let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
    let upload = fixture_upload(vec![b'y'; 1_024]);
    let retained = outbox.retain(&upload).unwrap();
    assert!(outbox.has_pending(&retained.authority).unwrap());
    erase_exact_diagnostic_body_route(&store, &retained.artifact.artifact_id);
    let damaged = diagnostic_route_fault_rows(&store);
    let transport = diagnostic_route_fault_transport_rows(&store);
    measure::reset();
    let outcome = outbox.has_pending(&retained.authority);
    let metrics = measure::finish();
    assert_eq!(diagnostic_route_fault_rows(&store), damaged);
    assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
    println!(
        "STORAGE_MECHANISM {}",
        serde_json::json!({"id":"M08-current-pending-route-loss","query_result":format!("{outcome:?}"),"metrics":metrics,"retained_envelope_unchanged":true,"damaged_body_unchanged":true,"transport_unchanged":true})
    );
    assert_eq!(outcome.unwrap_err(), AdapterStoreError::Corrupt);
    drop(outbox);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn diagnostic_current_job_route_loss_cannot_hide_accepted_reference() {
    use crate::storage_mechanism_regression as measure;
    let root = test_root("exact-accepted-route-loss");
    let store = AdapterStore::open(&root).unwrap();
    let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
    let upload = fixture_upload(vec![b'y'; 1_024]);
    let retained = outbox.retain(&upload).unwrap();
    assert!(matches!(
        outbox.apply_ack(&ack(&upload, &retained)).unwrap(),
        DiagnosticArtifactAckOutcome::Accepted { .. }
    ));
    assert_eq!(
        outbox.accepted_references(&retained.authority).unwrap(),
        vec![retained.artifact.clone()]
    );
    erase_exact_diagnostic_body_route(&store, &retained.artifact.artifact_id);
    let damaged = diagnostic_route_fault_rows(&store);
    let transport = diagnostic_route_fault_transport_rows(&store);
    measure::reset();
    let outcome = outbox.accepted_references(&retained.authority);
    let metrics = measure::finish();
    assert_eq!(diagnostic_route_fault_rows(&store), damaged);
    assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
    println!(
        "STORAGE_MECHANISM {}",
        serde_json::json!({"id":"M08-current-accepted-route-loss","query_result":format!("{outcome:?}"),"metrics":metrics,"retained_envelope_unchanged":true,"damaged_body_unchanged":true,"transport_unchanged":true})
    );
    assert_eq!(outcome.unwrap_err(), AdapterStoreError::Corrupt);
    drop(outbox);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn diagnostic_exact_upload_retain_preserves_ack_progress_and_rejects_payload_conflicts() {
    let mut cases = Vec::new();
    for (mode, final_ack, extend_current) in [
        ("partial-ack", false, false),
        ("final-ack", true, false),
        ("final-ack-extended-current-lease", true, true),
    ] {
        let root = test_root(mode);
        let store = AdapterStore::open(&root).unwrap();
        let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
        let mut upload = fixture_upload(vec![b'y'; DIAGNOSTIC_CHUNK_BYTES + 16]);
        upload.lease.issued_at = Instant("2030-01-01T00:00:00.000Z".to_owned());
        upload.lease.expires_at = Instant("2030-01-01T00:05:00.000Z".to_owned());
        upload.created_at = Instant("2030-01-01T00:01:00.000Z".to_owned());
        let retained = outbox.retain(&upload).unwrap();
        let mut acknowledgement = ack(&upload, &retained);
        if !final_ack {
            acknowledgement.ack_sequence = ExecutionAckSequence(1);
        }
        let outcome = outbox.apply_ack(&acknowledgement).unwrap();
        assert!(if final_ack {
            matches!(outcome, DiagnosticArtifactAckOutcome::Accepted { .. })
        } else {
            matches!(outcome, DiagnosticArtifactAckOutcome::Pending)
        });
        let mut current = retained.authority.clone();
        if extend_current {
            current.lease.expires_at = Instant("2030-01-01T00:10:00.000Z".to_owned());
        }
        assert_eq!(outbox.has_pending(&current).unwrap(), !final_ack);
        assert_eq!(
            outbox.accepted_references(&current).unwrap(),
            if final_ack {
                vec![retained.artifact.clone()]
            } else {
                Vec::new()
            }
        );
        let raw = diagnostic_route_fault_rows(&store);
        let transport = diagnostic_route_fault_transport_rows(&store);
        let changes_before = store.lock().unwrap().total_changes();
        let duplicate = outbox.retain(&upload);
        let changes_after = store.lock().unwrap().total_changes();
        assert_eq!(changes_after, changes_before);
        assert_eq!(diagnostic_route_fault_rows(&store), raw);
        assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
        let exact_replay = match &duplicate {
            Ok(replay) => {
                replay.artifact == retained.artifact
                    && replay.authority == retained.authority
                    && replay.run_key == retained.run_key
                    && replay.deliveries.is_empty()
                    && replay.already_accepted == final_ack
            }
            Err(_) => false,
        };
        let mut conflict = upload.clone();
        conflict.bytes[0] = b'q';
        assert_eq!(
            outbox.retain(&conflict).unwrap_err(),
            AdapterStoreError::Conflict
        );
        assert_eq!(store.lock().unwrap().total_changes(), changes_before);
        assert_eq!(diagnostic_route_fault_rows(&store), raw);
        assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
        let duplicate_result = match &duplicate {
            Ok(_) => "retained".to_owned(),
            Err(error) => format!("error:{error:?}"),
        };
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M08-diagnostic-immutable-retain","mode":mode,"duplicate_result":duplicate_result,"exact_replay":exact_replay,"zero_duplicate_sql_mutations":true,"original_body_unchanged":true,"transport_unchanged":true,"original_payload_conflict_rejected":true,"extended_current_authority_uses_original_upload":extend_current})
        );
        cases.push(exact_replay);
        drop(outbox);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    // Every table row executes before the old behavior is reported as one red.
    assert!(cases.into_iter().all(|exact_replay| exact_replay));
}
