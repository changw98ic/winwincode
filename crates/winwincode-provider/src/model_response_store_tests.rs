// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{DeviceProviderStore, model_failure};
use std::path::PathBuf;
use winwincode_domain::{ExecutionMessageId, ExecutionSequence, Sha256Digest};
use winwincode_execution_port::generated::EncodedPayload;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "wwc-model-response-{label}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        )))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> (ModelOpenMessage, Vec<ModelChunkMessage>) {
    let value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut open: ModelOpenMessage = serde_json::from_value(
        value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let request=serde_json::to_vec(&serde_json::json!({"provider":"fixture","request":{"model":"fixture","input":"SYNTHETIC_MODEL_INPUT","stream":true}})).unwrap();
    open.request.data_base64 = STANDARD.encode(&request);
    open.request.payload_digest = Sha256Digest(frame_digest(&request));
    let mut chunks = Vec::new();
    for (index, payload) in [
        serde_json::json!({"type":"created"}),
        serde_json::json!({"type":"output_text_delta","delta":"SYNTHETIC_PRIVATE_RESPONSE"}),
        serde_json::json!({"type":"server_model","model":"fixture"}),
        serde_json::json!({"type":"completed","responseId":"fixture-final","endTurn":true}),
    ]
    .iter()
    .enumerate()
    {
        let mut chunk = model_failure(&open, "SYNTHETIC_FIXTURE");
        chunk.sequence = ExecutionSequence(i64::try_from(index + 1).unwrap());
        chunk.message_id = ExecutionMessageId(format!("xmsg_{index:026}"));
        chunk.error = None;
        chunk.is_final = index == 3;
        let bytes = serde_json::to_vec(payload).unwrap();
        chunk.payload = Some(EncodedPayload {
            content_type: "application/json".into(),
            data_base64: STANDARD.encode(&bytes),
            payload_digest: Sha256Digest(frame_digest(&bytes)),
        });
        chunks.push(chunk);
    }
    (open, chunks)
}

#[test]
fn concurrent_first_open_uses_one_current_schema_and_identity() {
    let directory = TestDirectory::new("concurrent-first-open");
    let barrier = std::sync::Barrier::new(12);
    let identities = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let path = &directory.0;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let store = DeviceProviderStore::open(path).unwrap();
                    let version: i64 = store
                        .connection
                        .query_row("PRAGMA user_version", [], |row| row.get(0))
                        .unwrap();
                    assert_eq!(version, 15);
                    store
                        .snapshot("current-schema-test")
                        .unwrap()
                        .encryption_public_key
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(identities.iter().all(|identity| identity == &identities[0]));
}

#[test]
fn sealed_response_is_duplicate_only_and_suffix_corruption_fails_closed() {
    let directory = TestDirectory::new("immutable-response");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let (open, chunks) = fixture();
    store
        .retain_stored_model_exchange_chunks(&open, &chunks)
        .unwrap();
    store
        .retain_stored_model_exchange_chunks(&open, &chunks)
        .unwrap();
    let mut changed = chunks.clone();
    changed[1].message_id.0 = "xmsg_changed".into();
    assert!(
        store
            .retain_stored_model_exchange_chunks(&open, &changed)
            .is_err()
    );
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
    store
        .connection
        .execute(
            "UPDATE model_response_frame SET frame_json=?1 WHERE sequence=2",
            [serde_json::to_string(&changed[1]).unwrap()],
        )
        .unwrap();
    assert!(
        store.replay_model(&open.model_exchange_id.0, 2).is_err(),
        "normalized changed bytes cannot reuse the seal"
    );
    store
        .connection
        .execute(
            "UPDATE exchanges SET chunks=?1 WHERE exchange_id=?2",
            params![
                serde_json::to_string(&changed).unwrap(),
                open.model_exchange_id.0
            ],
        )
        .unwrap();
    assert!(
        store.replay_model(&open.model_exchange_id.0, 1).is_err(),
        "unsealed source bytes cannot be replayed"
    );
}

#[test]
fn sealed_response_read_does_not_mix_old_metadata_with_concurrently_changed_suffix() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::{Arc, Mutex};

    let directory = TestDirectory::new("response-read-snapshot");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let (open, chunks) = fixture();
    store
        .retain_stored_model_exchange_chunks(&open, &chunks)
        .unwrap();
    let mut changed = chunks[1].clone();
    let payload = serde_json::to_vec(&serde_json::json!({
        "type": "output_text_delta",
        "delta": "SYNTHETIC_MUTATED_AFTER_SEAL_READ",
    }))
    .unwrap();
    changed.payload = Some(EncodedPayload {
        content_type: "application/json".into(),
        data_base64: STANDARD.encode(&payload),
        payload_digest: Sha256Digest(frame_digest(&payload)),
    });
    validate_response_frame(&changed).unwrap();
    let changed_bytes = serde_json::to_string(&changed).unwrap();
    let changed_digest = frame_digest(changed_bytes.as_bytes());
    let writer_path = directory.0.join("providers.sqlite3");
    let exchange = open.model_exchange_id.0.clone();
    let mutation = Arc::new(Mutex::new(None::<Result<i64, String>>));
    let mutation_for_hook = Arc::clone(&mutation);

    // SQLite invokes this hook while preparing the real suffix SELECT. The
    // earlier metadata query has returned; the second WAL connection commits
    // changed, individually valid response bytes and the trigger revokes its
    // seal. This does not replace the production reader or its SQL results.
    store
        .connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if context.accessor.is_none()
                && matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "model_response_frame",
                        column_name: "frame_json",
                    }
                )
            {
                let mut recorded = mutation_for_hook.lock().unwrap();
                if recorded.is_none() {
                    *recorded = Some((|| -> Result<i64, rusqlite::Error> {
                        let writer = Connection::open(&writer_path)?;
                        writer.execute_batch("BEGIN IMMEDIATE;")?;
                        let changed_rows = writer.execute(
                            "UPDATE model_response_frame SET frame_json=?1,frame_digest=?2 WHERE exchange_id=?3 AND sequence=2",
                            params![changed_bytes, changed_digest, exchange],
                        )?;
                        let seals: i64 = writer.query_row(
                            "SELECT COUNT(*) FROM model_response_exchange WHERE exchange_id=?1",
                            [&exchange],
                            |row| row.get(0),
                        )?;
                        writer.execute_batch("COMMIT;")?;
                        if changed_rows != 1 {
                            return Err(rusqlite::Error::InvalidQuery);
                        }
                        Ok(seals)
                    })()
                    .map_err(|error| error.to_string()));
                }
            }
            Authorization::Allow
        }))
        .unwrap();
    let response = store.replay_model(&open.model_exchange_id.0, 2);
    store
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(
        *mutation.lock().unwrap(),
        Some(Ok(0)),
        "the second connection must actually commit and revoke the seal between metadata and suffix preparation"
    );
    assert!(store.connection.is_autocommit());
    let persisted: String = store
        .connection
        .query_row(
            "SELECT frame_json FROM model_response_frame WHERE exchange_id=?1 AND sequence=2",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<ModelChunkMessage>(&persisted).unwrap(),
        changed
    );
    assert!(
        store.replay_model(&open.model_exchange_id.0, 2).is_err(),
        "a new read must reject the already revoked seal"
    );
    assert!(
        store.replay_model(&open.model_exchange_id.0, 5).is_err(),
        "an empty suffix still requires its current complete response seal"
    );
    assert_eq!(
        response.unwrap(),
        chunks[1..],
        "one read may return the old sealed snapshot, but must never mix its old metadata with the newly changed suffix"
    );
}

#[test]
fn sealed_response_read_snapshot_preserves_caller_transaction_on_success_and_error() {
    for revoked in [false, true] {
        let directory = TestDirectory::new("response-read-caller-transaction");
        let store = DeviceProviderStore::open(&directory.0).unwrap();
        let (open, chunks) = fixture();
        store
            .retain_stored_model_exchange_chunks(&open, &chunks)
            .unwrap();
        if revoked {
            store.connection.execute("UPDATE model_response_frame SET frame_digest='invalid' WHERE exchange_id=?1 AND sequence=2", [&open.model_exchange_id.0]).unwrap();
        }
        store.connection.execute_batch("CREATE TABLE outer_reader_marker(value INTEGER); BEGIN; INSERT INTO outer_reader_marker(value) VALUES(1);").unwrap();
        let response = store.replay_model(&open.model_exchange_id.0, 1);
        if revoked {
            assert!(response.is_err());
        } else {
            assert_eq!(response.unwrap(), chunks);
            assert!(
                store
                    .replay_model(&open.model_exchange_id.0, 5)
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            !store.connection.is_autocommit(),
            "the reader must retain ownership of the caller's outer transaction"
        );
        let retained: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM outer_reader_marker", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained, 1);
        store.connection.execute_batch("ROLLBACK;").unwrap();
        let committed: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM outer_reader_marker", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            committed, 0,
            "neither successful reads nor error cleanup may commit the caller's write"
        );
        assert!(store.connection.is_autocommit());
    }
}

#[test]
fn normalized_body_and_digest_mutation_delete_and_insert_invalidate_the_seal() {
    let (open, chunks) = fixture();
    for mode in ["body-and-digest", "delete", "insert"] {
        let directory = TestDirectory::new(mode);
        let store = DeviceProviderStore::open(&directory.0).unwrap();
        store
            .retain_stored_model_exchange_chunks(&open, &chunks)
            .unwrap();
        let mut changed = chunks[1].clone();
        changed.message_id.0 = "xmsg_changed".into();
        let bytes = serde_json::to_string(&changed).unwrap();
        match mode {
            "body-and-digest" => {
                store.connection.execute("UPDATE model_response_frame SET frame_json=?1,frame_digest=?2 WHERE sequence=2",params![bytes,frame_digest(bytes.as_bytes())]).unwrap();
            }
            "delete" => {
                store
                    .connection
                    .execute("DELETE FROM model_response_frame WHERE sequence=2", [])
                    .unwrap();
            }
            "insert" => {
                store.connection.execute("INSERT INTO model_response_frame(exchange_id,sequence,frame_json,frame_digest) VALUES(?1,5,?2,?3)",params![open.model_exchange_id.0,bytes,frame_digest(bytes.as_bytes())]).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM model_response_exchange", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0,
            "{mode}"
        );
        assert!(
            store.replay_model(&open.model_exchange_id.0, 2).is_err(),
            "{mode}"
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT chunks FROM exchanges", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            serde_json::to_string(&chunks).unwrap(),
            "private canonical source survives: {mode}"
        );
        drop(store);
        let reopened = DeviceProviderStore::open(&directory.0).unwrap();
        assert!(
            reopened.replay_model(&open.model_exchange_id.0, 1).is_err(),
            "reopen cannot invent a new seal: {mode}"
        );
    }
}

#[test]
fn complete_response_publication_rolls_back_source_on_index_failure_then_reopens() {
    let directory = TestDirectory::new("publication-transaction");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let (open, chunks) = fixture();
    let request_open = serde_json::to_string(&open).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO exchanges(exchange_id,digest,request_open) VALUES(?1,?2,?3)",
            params![
                open.model_exchange_id.0,
                format!("{:x}", Sha256::digest(request_open.as_bytes())),
                request_open
            ],
        )
        .unwrap();
    store.connection.execute_batch("CREATE TRIGGER inject_response_index_failure BEFORE INSERT ON model_response_exchange BEGIN SELECT RAISE(ABORT,'synthetic publication failure'); END;").unwrap();
    assert!(
        store
            .retain_stored_model_exchange_chunks(&open, &chunks)
            .is_err()
    );
    assert!(
        store
            .connection
            .query_row("SELECT chunks FROM exchanges", [], |row| row
                .get::<_, Option<String>>(0))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM model_response_frame", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    store
        .connection
        .execute_batch("DROP TRIGGER inject_response_index_failure")
        .unwrap();
    store
        .retain_stored_model_exchange_chunks(&open, &chunks)
        .unwrap();
    assert!(
        store
            .connection
            .execute(
                "UPDATE model_response_exchange SET response_digest=?1",
                [format!("sha256:{}", "a".repeat(64))]
            )
            .is_err()
    );
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 3).unwrap(),
        chunks[2..]
    );
    drop(store);
    let reopened = DeviceProviderStore::open(&directory.0).unwrap();
    assert_eq!(
        reopened.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
}

#[test]
fn response_source_request_digest_and_exchange_mutation_invalidate_the_seal() {
    let (open, chunks) = fixture();
    for field in ["request_open", "digest", "exchange_id"] {
        let directory = TestDirectory::new(field);
        let store = DeviceProviderStore::open(&directory.0).unwrap();
        store
            .retain_stored_model_exchange_chunks(&open, &chunks)
            .unwrap();
        let sql = match field {
            "request_open" => "UPDATE exchanges SET request_open='corrupt binding'",
            "digest" => "UPDATE exchanges SET digest='corrupt source digest'",
            "exchange_id" => "UPDATE exchanges SET exchange_id='mdl_rebound'",
            _ => unreachable!(),
        };
        store.connection.execute_batch(sql).unwrap();
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM model_response_exchange", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0,
            "{field}"
        );
        let exchange = if field == "exchange_id" {
            "mdl_rebound"
        } else {
            &open.model_exchange_id.0
        };
        assert!(store.replay_model(exchange, 1).is_err(), "{field}");
        assert_eq!(
            store
                .connection
                .query_row("SELECT chunks FROM exchanges", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            serde_json::to_string(&chunks).unwrap(),
            "original response retained: {field}"
        );
        drop(store);
        assert!(
            DeviceProviderStore::open(&directory.0)
                .unwrap()
                .replay_model(exchange, 1)
                .is_err(),
            "{field}"
        );
    }
}
