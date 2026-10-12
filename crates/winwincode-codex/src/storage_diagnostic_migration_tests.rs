// SPDX-License-Identifier: Apache-2.0

const LEGACY_DIAGNOSTIC_UPLOAD_SCHEMA: &str = "CREATE TABLE diagnostic_artifact_upload (
 artifact_id TEXT PRIMARY KEY NOT NULL,
 authority_key TEXT NOT NULL,
 record_json BLOB NOT NULL
); CREATE UNIQUE INDEX diagnostic_artifact_authority_idx
 ON diagnostic_artifact_upload(authority_key);";

fn install_legacy_diagnostic_fixture(store: &AdapterStore) {
    let rows = diagnostic_route_fault_rows(store);
    let mut connection = store.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute_batch("DROP TABLE diagnostic_artifact_upload;")
        .unwrap();
    transaction
        .execute(
            "DELETE FROM codex_store_migration WHERE migration_key=?1",
            [DIAGNOSTIC_JOB_ROUTE_MIGRATION],
        )
        .unwrap();
    transaction
        .execute_batch(LEGACY_DIAGNOSTIC_UPLOAD_SCHEMA)
        .unwrap();
    for (artifact, authority, bytes) in rows {
        transaction
            .execute(
                "INSERT INTO diagnostic_artifact_upload VALUES (?1,?2,?3)",
                params![artifact, authority, bytes],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
}

fn diagnostic_route_marker_present(store: &AdapterStore) -> bool {
    store
        .lock()
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_store_migration WHERE migration_key=?1)",
            [DIAGNOSTIC_JOB_ROUTE_MIGRATION],
            |row| row.get(0),
        )
        .unwrap()
}

fn diagnostic_route_table_definition(store: &AdapterStore) -> String {
    store.lock().unwrap().query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='diagnostic_artifact_upload'",
        [], |row| row.get(0),
    ).unwrap()
}

#[test]
fn diagnostic_job_route_migration_preserves_bytes_and_reopen_skips_history() {
    use crate::storage_mechanism_regression as measure;
    let root = test_root("stable-job-route-migration");
    let store = AdapterStore::open(&root).unwrap();
    let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
    let upload = fixture_upload(vec![b'y'; 1_024]);
    let retained = outbox.retain(&upload).unwrap();
    let mut accepted_upload = upload.clone();
    accepted_upload.source_id = "command_accepted_migration".to_owned();
    accepted_upload.file_name = "command_accepted_migration.log".to_owned();
    let accepted = outbox.retain(&accepted_upload).unwrap();
    assert!(matches!(
        outbox.apply_ack(&ack(&accepted_upload, &accepted)).unwrap(),
        DiagnosticArtifactAckOutcome::Accepted { .. }
    ));
    drop(outbox);
    install_legacy_diagnostic_fixture(&store);
    let raw = diagnostic_route_fault_rows(&store);
    let transport = diagnostic_route_fault_transport_rows(&store);
    measure::reset();
    let migrated = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
    let migration_metrics = measure::finish();
    assert_eq!(migration_metrics.diagnostic_validations, 2);
    assert!(diagnostic_route_marker_present(&store));
    assert_eq!(diagnostic_route_fault_rows(&store), raw);
    assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
    assert!(migrated.has_pending(&retained.authority).unwrap());
    assert_eq!(
        migrated.accepted_references(&retained.authority).unwrap(),
        vec![accepted.artifact]
    );
    drop(migrated);
    measure::reset();
    let reopened = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
    let reopen_metrics = measure::finish();
    assert_eq!(reopen_metrics.diagnostic_validations, 0);
    assert_eq!(reopen_metrics.diagnostic_raw_bytes, 0);
    assert_eq!(diagnostic_route_fault_rows(&store), raw);
    assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
    println!(
        "STORAGE_MECHANISM {}",
        serde_json::json!({"id":"M08-job-route-migration","migration_metrics":migration_metrics,"reopen_metrics":reopen_metrics,"retained_bytes_unchanged":true,"transport_unchanged":true})
    );
    drop(reopened);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn diagnostic_job_route_legacy_fault_rolls_back_all_schema_and_bytes() {
    for fault in [
        "body-route-loss",
        "artifact-envelope-conflict",
        "authority-envelope-conflict",
    ] {
        let root = test_root(fault);
        let store = AdapterStore::open(&root).unwrap();
        let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
        let upload = fixture_upload(vec![b'y'; 1_024]);
        outbox.retain(&upload).unwrap();
        let mut second = upload.clone();
        second.source_id = "command_second_rollback".to_owned();
        second.file_name = "command_second_rollback.log".to_owned();
        outbox.retain(&second).unwrap();
        drop(outbox);
        install_legacy_diagnostic_fixture(&store);
        let target = diagnostic_route_fault_rows(&store)
            .last()
            .unwrap()
            .0
            .clone();
        match fault {
            "body-route-loss" => {
                store
                    .lock()
                    .unwrap()
                    .execute(
                        "UPDATE diagnostic_artifact_upload SET record_json=?1 WHERE artifact_id=?2",
                        params![b"{}".as_slice(), target],
                    )
                    .unwrap();
            }
            "artifact-envelope-conflict" => {
                store
                    .lock()
                    .unwrap()
                    .execute(
                        "UPDATE diagnostic_artifact_upload SET artifact_id=?1 WHERE artifact_id=?2",
                        params!["zzzz_other_envelope", target],
                    )
                    .unwrap();
            }
            "authority-envelope-conflict" => {
                store.lock().unwrap().execute("UPDATE diagnostic_artifact_upload SET authority_key=?1 WHERE artifact_id=?2",params!["sha256:other-envelope",target]).unwrap();
            }
            _ => unreachable!(),
        }
        let raw = diagnostic_route_fault_rows(&store);
        let transport = diagnostic_route_fault_transport_rows(&store);
        let schema = diagnostic_route_table_definition(&store);
        crate::storage_mechanism_regression::reset();
        assert!(matches!(
            DiagnosticArtifactOutbox::open(store.clone()),
            Err(AdapterStoreError::Corrupt)
        ));
        let partial_metrics = crate::storage_mechanism_regression::finish();
        assert_eq!(partial_metrics.diagnostic_validations, 1);
        assert!(!diagnostic_route_marker_present(&store));
        assert_eq!(diagnostic_route_table_definition(&store), schema);
        assert_eq!(diagnostic_route_fault_rows(&store), raw);
        assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
        let temporary:bool=store.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='diagnostic_artifact_upload_route_legacy_v1')",[],|row|row.get(0)).unwrap();
        assert!(!temporary);
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M08-job-route-migration-rollback","fault":fault,"partial_migration_metrics":partial_metrics,"original_schema_unchanged":true,"retained_bytes_unchanged":true,"marker_absent":true,"transport_unchanged":true})
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn diagnostic_job_route_marker_refuses_weakened_layout() {
    for (fault, sql) in [
        (
            "missing-job-index",
            "DROP INDEX diagnostic_artifact_job_idx;",
        ),
        (
            "weakened-job-index",
            "DROP INDEX diagnostic_artifact_job_idx; CREATE INDEX diagnostic_artifact_job_idx ON diagnostic_artifact_upload(job_id);",
        ),
        (
            "missing-authority-index",
            "DROP INDEX diagnostic_artifact_authority_idx;",
        ),
        (
            "weakened-authority-index",
            "DROP INDEX diagnostic_artifact_authority_idx; CREATE INDEX diagnostic_artifact_authority_idx ON diagnostic_artifact_upload(authority_key);",
        ),
        (
            "missing-route-guard",
            "DROP TRIGGER diagnostic_artifact_route_immutable;",
        ),
        (
            "weakened-route-guard",
            "DROP TRIGGER diagnostic_artifact_route_immutable; CREATE TRIGGER diagnostic_artifact_route_immutable BEFORE UPDATE OF job_id ON diagnostic_artifact_upload WHEN 0 BEGIN SELECT RAISE(ABORT,'inert'); END;",
        ),
        (
            "missing-job-column",
            "DROP TABLE diagnostic_artifact_upload; CREATE TABLE diagnostic_artifact_upload(artifact_id TEXT PRIMARY KEY NOT NULL, authority_key TEXT NOT NULL, record_json BLOB NOT NULL);",
        ),
        (
            "weakened-job-column",
            "DROP TABLE diagnostic_artifact_upload; CREATE TABLE diagnostic_artifact_upload(artifact_id TEXT PRIMARY KEY NOT NULL, authority_key TEXT NOT NULL, job_id TEXT, record_json BLOB NOT NULL);",
        ),
    ] {
        let root = test_root(fault);
        let store = AdapterStore::open(&root).unwrap();
        let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
        drop(outbox);
        assert!(diagnostic_route_marker_present(&store));
        store.lock().unwrap().execute_batch(sql).unwrap();
        let schema = diagnostic_route_table_definition(&store);
        assert!(matches!(
            DiagnosticArtifactOutbox::open(store.clone()),
            Err(AdapterStoreError::Corrupt)
        ));
        assert!(diagnostic_route_marker_present(&store));
        assert_eq!(diagnostic_route_table_definition(&store), schema);
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M08-job-route-layout-guard","fault":fault,"refused_open":true,"weakened_schema_not_rebuilt":true})
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn diagnostic_job_route_is_immutable_and_selected_header_must_match_body() {
    let root = test_root("stable-job-route-guard");
    let store = AdapterStore::open(&root).unwrap();
    let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
    let upload = fixture_upload(vec![b'y'; 1_024]);
    let retained = outbox.retain(&upload).unwrap();
    let raw = diagnostic_route_fault_rows(&store);
    let transport = diagnostic_route_fault_transport_rows(&store);
    for sql in [
        "UPDATE diagnostic_artifact_upload SET artifact_id='art_other'",
        "UPDATE diagnostic_artifact_upload SET authority_key='sha256:other'",
        "UPDATE diagnostic_artifact_upload SET job_id='job_other'",
    ] {
        assert!(store.lock().unwrap().execute(sql, []).is_err());
        assert_eq!(diagnostic_route_fault_rows(&store), raw);
        assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
    }
    let mut body: serde_json::Value = serde_json::from_slice(&raw[0].2).unwrap();
    body["authority_key"] = serde_json::Value::String("sha256:other".to_owned());
    let damaged = serde_json::to_vec(&body).unwrap();
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE diagnostic_artifact_upload SET record_json=?1 WHERE artifact_id=?2",
            params![damaged, retained.artifact.artifact_id.0],
        )
        .unwrap();
    assert_eq!(
        outbox.has_pending(&retained.authority).unwrap_err(),
        AdapterStoreError::Corrupt
    );
    assert_eq!(
        outbox.accepted_references(&retained.authority).unwrap_err(),
        AdapterStoreError::Corrupt
    );
    assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
    drop(outbox);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn diagnostic_job_route_reopen_and_query_ignore_corrupt_foreign_history() {
    use crate::storage_mechanism_regression as measure;
    let root = test_root("stable-job-route-foreign-reopen");
    let store = AdapterStore::open(&root).unwrap();
    let outbox = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
    let current = fixture_upload(vec![b'y'; 1_024]);
    let mut foreign = current.clone();
    foreign.job.job_id =
        winwincode_domain::ExecutionJobId("job_00000000000000000000009999".to_owned());
    foreign.lease.job_id = foreign.job.job_id.clone();
    let retained = outbox.retain(&foreign).unwrap();
    assert!(matches!(
        outbox.apply_ack(&ack(&foreign, &retained)).unwrap(),
        DiagnosticArtifactAckOutcome::Accepted { .. }
    ));
    erase_exact_diagnostic_body_route(&store, &retained.artifact.artifact_id);
    let raw = diagnostic_route_fault_rows(&store);
    let transport = diagnostic_route_fault_transport_rows(&store);
    drop(outbox);
    measure::reset();
    let reopened = DiagnosticArtifactOutbox::open(store.clone()).unwrap();
    let authority = DiagnosticArtifactAuthority {
        snapshot_id: current.snapshot_id.clone(),
        job: current.job.clone(),
        scope: current.scope.clone(),
        lease: current.lease.clone(),
        worker_session_id: current.worker_session_id.clone(),
        session_identity: current.session_identity.clone(),
    };
    assert!(!reopened.has_pending(&authority).unwrap());
    assert!(reopened.accepted_references(&authority).unwrap().is_empty());
    let metrics = measure::finish();
    assert_eq!(metrics.diagnostic_validations, 0);
    assert_eq!(metrics.diagnostic_raw_bytes, 0);
    assert_eq!(diagnostic_route_fault_rows(&store), raw);
    assert_eq!(diagnostic_route_fault_transport_rows(&store), transport);
    println!(
        "STORAGE_MECHANISM {}",
        serde_json::json!({"id":"M08-job-route-foreign-reopen","metrics":metrics,"foreign_bytes_unchanged":true,"transport_unchanged":true})
    );
    drop(reopened);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
