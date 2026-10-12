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
    let request=serde_json::to_vec(&serde_json::json!({"provider":"fixture","request":{"model":"fixture","input":"SYNTHETIC_MIGRATION_INPUT","stream":true}})).unwrap();
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

fn legacy_store(directory: &TestDirectory, open: &ModelOpenMessage, bytes: &str) {
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    store
        .connection
        .execute_batch(
            "DROP TRIGGER model_response_update; DROP TRIGGER model_response_delete;
         DROP TRIGGER prepared_model_payload_immutable;
         ALTER TABLE exchanges DROP COLUMN prepared_payload_digest;
         DROP TABLE model_response_frame; DROP TABLE model_response_exchange;
         DROP TABLE model_retry_state;
         ALTER TABLE model_invocation_attempts DROP COLUMN response_bytes_received;
         PRAGMA user_version=13;",
        )
        .unwrap();
    let request_open = serde_json::to_string(open).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO exchanges(exchange_id,digest,chunks,request_open) VALUES(?1,?2,?3,?4)",
            params![
                open.model_exchange_id.0,
                format!("{:x}", Sha256::digest(request_open.as_bytes())),
                bytes,
                request_open
            ],
        )
        .unwrap();
    store.connection.execute(
        "INSERT INTO model_invocation_attempts(exchange_id,attempt_number,adapter_request_id,state,response_bytes) VALUES(?1,1,'original-physical-request','completed',x'010203')",
        [&open.model_exchange_id.0],
    ).unwrap();
}

#[test]
fn schema13_complete_owned_response_migrates_atomically_with_canonical_seal() {
    let directory = TestDirectory::new("owned-v13");
    let (open, chunks) = fixture();
    let original = serde_json::to_string_pretty(&chunks).unwrap();
    legacy_store(&directory, &open, &original);
    let connection = Connection::open(directory.0.join("providers.sqlite3")).unwrap();
    let prepared = b"{\n  \"private\": \"SYNTHETIC_PREPARED_BYTES\"\n}";
    connection
        .execute(
            "UPDATE exchanges SET prepared_payload=?1",
            [prepared.as_slice()],
        )
        .unwrap();
    drop(connection);
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        14
    );
    let retained: String = store
        .connection
        .query_row("SELECT chunks FROM exchanges", [], |row| row.get(0))
        .unwrap();
    assert_eq!(retained, original, "private original bytes are retained");
    let (migrated_prepared, migrated_digest): (Vec<u8>, String) = store
        .connection
        .query_row(
            "SELECT prepared_payload,prepared_payload_digest FROM exchanges",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(migrated_prepared, prepared);
    assert_eq!(migrated_digest, frame_digest(prepared));
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 3).unwrap(),
        chunks[2..]
    );
    assert!(
        store
            .replay_model(&open.model_exchange_id.0, 5)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .model_exchanges_for_owner(Some(&open.worker_session_id.0), None)
            .unwrap(),
        vec![open.model_exchange_id.0.clone()]
    );
    assert!(
        store
            .model_exchanges_for_owner(Some("foreign-session"), None)
            .unwrap()
            .is_empty()
    );
    let seal: String = store
        .connection
        .query_row(
            "SELECT response_digest FROM model_response_exchange",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        seal,
        validate_model_response(&open.model_exchange_id.0, &chunks, None)
            .unwrap()
            .digest
    );
    let attempt: (i64,String,String,Vec<u8>) = store.connection.query_row("SELECT attempt_number,adapter_request_id,state,response_bytes FROM model_invocation_attempts",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    assert_eq!(
        attempt,
        (
            1,
            "original-physical-request".into(),
            "completed".into(),
            vec![1, 2, 3]
        )
    );
    drop(store);
    let reopened = DeviceProviderStore::open(&directory.0).unwrap();
    assert_eq!(
        reopened.replay_model(&open.model_exchange_id.0, 4).unwrap(),
        chunks[3..]
    );
}

#[test]
fn schema13_unfinished_prepared_request_binding_corruption_rolls_back() {
    for corruption in ["missing_request", "wrong_sha", "wrong_exchange"] {
        let directory = TestDirectory::new(corruption);
        let (open, chunks) = fixture();
        legacy_store(&directory, &open, &serde_json::to_string(&chunks).unwrap());
        let connection = Connection::open(directory.0.join("providers.sqlite3")).unwrap();
        connection
            .execute(
                "UPDATE exchanges SET chunks=NULL,prepared_payload=?1",
                [b"SYNTHETIC_PREPARED_PAYLOAD".as_slice()],
            )
            .unwrap();
        match corruption {
            "missing_request" => {
                connection
                    .execute_batch("UPDATE exchanges SET request_open=NULL")
                    .unwrap();
            }
            "wrong_sha" => {
                connection
                    .execute_batch("UPDATE exchanges SET digest='bad'")
                    .unwrap();
            }
            _ => {
                let mut changed = open.clone();
                changed.model_exchange_id.0 = "mdl_00000000000000000000000099".into();
                let request = serde_json::to_string(&changed).unwrap();
                let digest = format!("{:x}", Sha256::digest(request.as_bytes()));
                connection
                    .execute(
                        "UPDATE exchanges SET request_open=?1,digest=?2",
                        params![request, digest],
                    )
                    .unwrap();
            }
        }
        drop(connection);
        assert!(DeviceProviderStore::open(&directory.0).is_err());
        let connection = Connection::open(directory.0.join("providers.sqlite3")).unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            13
        );
        let prepared: Vec<u8> = connection
            .query_row("SELECT prepared_payload FROM exchanges", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(prepared, b"SYNTHETIC_PREPARED_PAYLOAD");
        assert!(!connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('exchanges') WHERE name='prepared_payload_digest')",[],|row|row.get::<_,bool>(0)).unwrap());
    }
}

#[test]
fn schema13_corrupt_response_rolls_back_version_index_and_original_bytes() {
    let (open, chunks) = fixture();
    let mut corrupt = Vec::new();
    corrupt.push(("invalid-json", "[invalid".to_owned()));
    for (label, mutate) in [
        ("duplicate-sequence", 0),
        ("gap", 1),
        ("cross-lease", 2),
        ("payload-digest", 3),
        ("early-final", 4),
        ("missing-final", 5),
        ("cross-thread", 6),
        ("exchange-conflict", 7),
    ] {
        let mut frames = chunks.clone();
        match mutate {
            0 => frames[1].sequence = frames[0].sequence.clone(),
            1 => frames[1].sequence = ExecutionSequence(3),
            2 => frames[1].lease.lease_id.0 = "lse_other".into(),
            3 => {
                frames[1].payload.as_mut().unwrap().payload_digest.0 =
                    format!("sha256:{}", "0".repeat(64));
            }
            4 => frames[1].is_final = true,
            5 => frames[3].is_final = false,
            6 => frames[1].session_identity.codex_thread_id.0 = "cdx_other".into(),
            7 => frames[1].model_exchange_id.0 = "mdl_other".into(),
            _ => unreachable!(),
        }
        corrupt.push((label, serde_json::to_string(&frames).unwrap()));
    }
    for (label, original) in corrupt {
        let directory = TestDirectory::new(label);
        legacy_store(&directory, &open, &original);
        assert!(DeviceProviderStore::open(&directory.0).is_err(), "{label}");
        let connection = Connection::open(directory.0.join("providers.sqlite3")).unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            13,
            "{label}"
        );
        assert_eq!(
            connection
                .query_row("SELECT chunks FROM exchanges", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            original,
            "{label}"
        );
        let tables: i64 = connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('model_response_frame','model_response_exchange','model_retry_state')",[],|row|row.get(0)).unwrap();
        assert_eq!(
            tables, 0,
            "migration rollback removes all derived tables: {label}"
        );
        let columns: i64 = connection.query_row("SELECT COUNT(*) FROM pragma_table_info('model_invocation_attempts') WHERE name='response_bytes_received'",[],|row|row.get(0)).unwrap();
        assert_eq!(columns, 0, "{label}");
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM model_invocation_attempts",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1,
            "no physical attempts are added: {label}"
        );
    }
}

#[test]
fn sealed_response_is_duplicate_only_and_suffix_corruption_fails_closed() {
    let directory = TestDirectory::new("immutable-response");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let (open, chunks) = fixture();
    store
        .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &chunks)
        .unwrap();
    store
        .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &chunks)
        .unwrap();
    let mut changed = chunks.clone();
    changed[1].message_id.0 = "xmsg_changed".into();
    assert!(
        store
            .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &changed)
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
        .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &chunks)
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
            .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &chunks)
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
            .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &chunks)
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
            .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &chunks)
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
        .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &chunks)
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
            .retain_stored_model_exchange_chunks(&open.model_exchange_id.0, &chunks)
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

#[test]
fn schema13_response_request_source_digest_corruption_rolls_back_migration() {
    let (open, chunks) = fixture();
    for changed_request in [true, false] {
        let directory = TestDirectory::new("source-digest-v13");
        let original = serde_json::to_string(&chunks).unwrap();
        legacy_store(&directory, &open, &original);
        let connection = Connection::open(directory.0.join("providers.sqlite3")).unwrap();
        if changed_request {
            let mut changed = open.clone();
            changed.route.route = "changed-gateway".into();
            connection
                .execute(
                    "UPDATE exchanges SET request_open=?1",
                    [serde_json::to_string(&changed).unwrap()],
                )
                .unwrap();
        } else {
            connection
                .execute_batch("UPDATE exchanges SET digest='bad'")
                .unwrap();
        }
        drop(connection);
        assert!(DeviceProviderStore::open(&directory.0).is_err());
        let connection = Connection::open(directory.0.join("providers.sqlite3")).unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            13
        );
        assert_eq!(
            connection
                .query_row("SELECT chunks FROM exchanges", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            original
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='model_response_exchange'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}
