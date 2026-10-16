// SPDX-License-Identifier: Apache-2.0

//! Response publication must acquire its writer before reading a WAL snapshot.

use super::*;
use std::{sync::mpsc, time::Duration};

#[test]
fn response_publication_waits_for_a_concurrent_writer_and_preserves_original_frames() {
    let root = test_directory("model-response-writer");
    let publisher = ProviderRuntimeStore::prepare(&root).unwrap();
    let writer = ProviderRuntimeStore::prepare(&root).unwrap();
    let open = fixture_open();
    let mut chunk = model_chunk(&open, 1);
    chunk.is_final = true;
    let original = vec![chunk];
    writer
        .connection
        .execute_batch("BEGIN IMMEDIATE; UPDATE runtime_origin SET source_id=source_id;")
        .unwrap();
    let (starting, started) = mpsc::channel();
    let (completed, completion) = mpsc::channel();
    let request = open.clone();
    let chunks = original.clone();
    let task = std::thread::spawn(move || {
        starting.send(()).unwrap();
        let result = publisher.retain_stored_model_exchange_chunks(&request, &chunks);
        assert!(completed.send((result, publisher)).is_ok());
    });
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        matches!(
            completion.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "a competing writer must leave publication waiting rather than fail its WAL upgrade"
    );
    writer.connection.execute_batch("COMMIT").unwrap();
    let (result, publisher) = completion.recv_timeout(Duration::from_secs(10)).unwrap();
    result.unwrap();
    task.join().unwrap();
    assert_eq!(
        publisher
            .replay_model(&open.model_exchange_id.0, 1)
            .unwrap(),
        original
    );
    drop(publisher);
    drop(writer);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn nested_response_publication_preserves_the_callers_rollback_boundary() {
    let root = test_directory("model-response-nested-rollback");
    let store = ProviderRuntimeStore::prepare(&root).unwrap();
    let open = fixture_open();
    let mut chunk = model_chunk(&open, 1);
    chunk.is_final = true;
    let original = vec![chunk];
    store.connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    store
        .retain_stored_model_exchange_chunks(&open, &original)
        .unwrap();
    assert!(!store.connection.is_autocommit());
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        original
    );
    store.connection.execute_batch("ROLLBACK").unwrap();
    assert!(
        store
            .replay_model(&open.model_exchange_id.0, 1)
            .unwrap()
            .is_empty()
    );
    drop(store);
    let reopened = ProviderRuntimeStore::prepare(&root).unwrap();
    assert!(reopened.list_stored_model_exchanges().unwrap().is_empty());
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

fn fixture_open() -> ModelOpenMessage {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap()
}

fn test_directory(case: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "wwc-{case}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
