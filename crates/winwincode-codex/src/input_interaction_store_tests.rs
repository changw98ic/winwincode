// SPDX-License-Identifier: Apache-2.0

use super::{
    AdapterStore, AdapterStoreError, DATABASE_FILE, StoredInputOperation, StoredInputOperationState,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn fixture_root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "winwincode-input-call-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
}

fn pending_operation() -> StoredInputOperation {
    StoredInputOperation {
        input_request_id: "inp_AAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        run_key: "input-run".into(),
        kernel_session_id: "kernel-session".into(),
        question_id: "continue".into(),
        turn_id: "turn".into(),
        request_digest: format!("sha256:{}", "1".repeat(64)),
        choice_identities: Vec::new(),
        resolution_digest: None,
        state: StoredInputOperationState::Pending,
    }
}

fn resolution_digest() -> String {
    format!("sha256:{}", "2".repeat(64))
}

fn retained_answer(operation: &StoredInputOperation) -> Value {
    json!({"answers": {(operation.input_request_id.clone()): {
        "resolutionDigest": resolution_digest(), "values": ["continue"]
    }}})
}

#[test]
fn input_answer_and_operation_resolution_commit_together_and_survive_reopen() {
    let root = fixture_root();
    let operation = pending_operation();
    let empty = json!({"answers": {}});
    let answered = retained_answer(&operation);
    {
        let store = AdapterStore::open(&root).unwrap();
        store.retain_input_operation(&operation).unwrap();
        store
            .retain_input_call(&operation.run_key, &operation.request_digest, &empty)
            .unwrap();
        store
            .resolve_input_call_answer(&operation, &resolution_digest(), &empty, &answered)
            .unwrap();
    }
    {
        let store = AdapterStore::open(&root).unwrap();
        assert_eq!(
            store
                .load_input_call::<Value>(&operation.run_key, &operation.request_digest)
                .unwrap(),
            Some(answered.clone())
        );
        let resolved = store
            .load_input_operation(&operation.input_request_id)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.state, StoredInputOperationState::Resolved);
        assert_eq!(
            resolved.resolution_digest.as_deref(),
            Some(resolution_digest().as_str())
        );
        store
            .resolve_input_call_answer(&resolved, &resolution_digest(), &answered, &answered)
            .unwrap();
        assert_eq!(
            store.resolve_input_call_answer(
                &resolved,
                &format!("sha256:{}", "3".repeat(64)),
                &answered,
                &empty
            ),
            Err(AdapterStoreError::Conflict)
        );
        assert_eq!(
            store
                .load_input_call::<Value>(&operation.run_key, &operation.request_digest)
                .unwrap(),
            Some(answered)
        );
        assert_eq!(
            store
                .load_input_operation(&operation.input_request_id)
                .unwrap(),
            Some(resolved)
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn input_answer_sql_failure_rolls_back_the_call_and_operation_before_reopen() {
    let root = fixture_root();
    let operation = pending_operation();
    let empty = json!({"answers": {}});
    let answered = retained_answer(&operation);
    {
        let store = AdapterStore::open(&root).unwrap();
        store.retain_input_operation(&operation).unwrap();
        store
            .retain_input_call(&operation.run_key, &operation.request_digest, &empty)
            .unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_input_resolution BEFORE UPDATE OF state ON input_operation
             BEGIN SELECT RAISE(ABORT, 'input resolution fault'); END;",
            )
            .unwrap();
        assert_eq!(
            store.resolve_input_call_answer(&operation, &resolution_digest(), &empty, &answered),
            Err(AdapterStoreError::Unavailable)
        );
    }
    {
        let store = AdapterStore::open(&root).unwrap();
        assert_eq!(
            store
                .load_input_call::<Value>(&operation.run_key, &operation.request_digest)
                .unwrap(),
            Some(empty.clone())
        );
        assert_eq!(
            store
                .load_input_operation(&operation.input_request_id)
                .unwrap(),
            Some(operation.clone())
        );
        store
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_input_resolution;")
            .unwrap();
        assert_eq!(
            store.resolve_input_call_answer(&operation, &resolution_digest(), &answered, &answered),
            Err(AdapterStoreError::Conflict)
        );
        assert_eq!(
            store
                .load_input_operation(&operation.input_request_id)
                .unwrap(),
            Some(operation.clone())
        );
        store
            .resolve_input_call_answer(&operation, &resolution_digest(), &empty, &answered)
            .unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn input_call_migration_preserves_an_existing_pending_question_and_its_identity() {
    let root = fixture_root();
    std::fs::create_dir_all(&root).unwrap();
    let operation = pending_operation();
    {
        let connection = rusqlite::Connection::open(root.join(DATABASE_FILE)).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE input_operation (
               input_request_id TEXT PRIMARY KEY NOT NULL, run_key TEXT NOT NULL,
               kernel_session_id TEXT NOT NULL, question_id TEXT NOT NULL, turn_id TEXT NOT NULL,
               request_digest TEXT NOT NULL, choice_identities_json BLOB NOT NULL,
               resolution_digest TEXT, state TEXT NOT NULL
             );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO input_operation VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'pending')",
                params![
                    operation.input_request_id,
                    operation.run_key,
                    operation.kernel_session_id,
                    operation.question_id,
                    operation.turn_id,
                    operation.request_digest,
                    b"[]".as_slice()
                ],
            )
            .unwrap();
    }
    let empty = json!({"answers": {}});
    {
        let store = AdapterStore::open(&root).unwrap();
        assert_eq!(
            store
                .load_input_operation(&operation.input_request_id)
                .unwrap(),
            Some(operation.clone())
        );
        assert!(
            store
                .load_input_call::<Value>(&operation.run_key, &operation.request_digest)
                .unwrap()
                .is_none()
        );
        store
            .retain_input_call(&operation.run_key, &operation.request_digest, &empty)
            .unwrap();
    }
    {
        let store = AdapterStore::open(&root).unwrap();
        assert_eq!(
            store
                .load_input_operation(&operation.input_request_id)
                .unwrap(),
            Some(operation.clone())
        );
        assert_eq!(
            store
                .load_input_call::<Value>(&operation.run_key, &operation.request_digest)
                .unwrap(),
            Some(empty)
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
