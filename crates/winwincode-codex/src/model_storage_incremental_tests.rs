// SPDX-License-Identifier: Apache-2.0

use super::*;
use winwincode_domain::ExecutionMessageId;

struct TempDirectory(PathBuf);
impl TempDirectory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("wwc-incremental-cursor-{}", uuid::Uuid::now_v7())))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stream() -> ReplayStreamKey {
    ReplayStreamKey::new("incremental-model-cursor")
}
fn frame(sequence: u64, is_final: bool) -> ModelChunkFingerprint {
    ModelChunkFingerprint {
        sequence,
        message_id: ExecutionMessageId(format!("message-{sequence}")),
        digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        is_final,
        has_error: false,
    }
}
fn legacy_cursor() -> ModelCursorSnapshot {
    ModelCursorSnapshot {
        confirmed_sequence: 2,
        frames: vec![frame(1, false), frame(2, true)],
        cancellation: None,
        termination: Some(ModelTerminationReason::Completed),
    }
}

#[test]
fn model_cursor_migration_preserves_all_fingerprints_and_reopens() {
    let directory = TempDirectory::new();
    let mut store = AdapterStore::open(directory.path()).unwrap();
    let expected = legacy_cursor();
    {
        let connection = store.lock().unwrap();
        connection
            .execute(
                "DELETE FROM codex_store_migration WHERE migration_key='model-cursor-frames-v1'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO model_cursor(stream_key,snapshot_json) VALUES(?1,?2)",
                params![stream().as_str(), serde_json::to_vec(&expected).unwrap()],
            )
            .unwrap();
    }
    drop(store);
    store = AdapterStore::open(directory.path()).unwrap();
    assert_eq!(
        ModelCursorStore::load(&mut store, &stream()).unwrap(),
        Some(expected.clone())
    );
    assert_eq!(
        store.load_fingerprint(&stream(), 1).unwrap(),
        Some(frame(1, false))
    );
    assert_eq!(
        store.load_head(&stream()).unwrap().termination,
        Some(ModelTerminationReason::Completed)
    );
    assert_eq!(
        store.record_delivery(&stream(), 2, &frame(4, false), None),
        Err(AdapterStoreError::Conflict)
    );
    let header: Vec<u8> = store
        .lock()
        .unwrap()
        .query_row(
            "SELECT snapshot_json FROM model_cursor WHERE stream_key=?1",
            [stream().as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        serde_json::from_slice::<serde_json::Value>(&header)
            .unwrap()
            .get("frames")
            .is_none()
    );
    drop(store);
    let mut reopened = AdapterStore::open(directory.path()).unwrap();
    assert_eq!(
        ModelCursorStore::load(&mut reopened, &stream()).unwrap(),
        Some(expected)
    );
}

#[test]
fn model_cursor_corrupt_legacy_migration_rolls_back_rows_and_marker() {
    let directory = TempDirectory::new();
    let store = AdapterStore::open(directory.path()).unwrap();
    let mut corrupt = legacy_cursor();
    corrupt.frames[0].sequence = 2;
    let original = serde_json::to_vec(&corrupt).unwrap();
    {
        let connection = store.lock().unwrap();
        connection
            .execute(
                "DELETE FROM codex_store_migration WHERE migration_key='model-cursor-frames-v1'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO model_cursor(stream_key,snapshot_json) VALUES(?1,?2)",
                params![stream().as_str(), original],
            )
            .unwrap();
    }
    drop(store);
    assert!(matches!(
        AdapterStore::open(directory.path()),
        Err(AdapterStoreError::Corrupt)
    ));
    let connection = Connection::open(directory.path().join(DATABASE_FILE)).unwrap();
    let retained: Vec<u8> = connection
        .query_row("SELECT snapshot_json FROM model_cursor", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(retained, original);
    let frames: i64 = connection
        .query_row("SELECT count(*) FROM model_cursor_frame", [], |row| {
            row.get(0)
        })
        .unwrap();
    let marker:i64=connection.query_row("SELECT count(*) FROM codex_store_migration WHERE migration_key='model-cursor-frames-v1'",[],|row|row.get(0)).unwrap();
    assert_eq!((frames, marker), (0, 0));
}

#[test]
fn model_cursor_head_rejects_cancellation_digest_phase_and_terminal_conflicts() {
    let directory = TempDirectory::new();
    let mut store = AdapterStore::open(directory.path()).unwrap();
    store
        .record_delivery(&stream(), 0, &frame(1, false), None)
        .unwrap();
    let cancellation = ModelCancellationFingerprint {
        message_id: ExecutionMessageId("cancel".into()),
        confirmed_sequence: 1,
        digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        phase: ModelCancellationPhase::Intent,
    };
    store
        .record_cancellation_intent(&stream(), 1, &cancellation)
        .unwrap();
    let mut head = store.load_head(&stream()).unwrap();
    assert_eq!(head.cancellation.as_ref(), Some(&cancellation));
    for invalid in [
        Sha256Digest("x".repeat(71)),
        Sha256Digest(format!("sha256:{}", "g".repeat(64))),
    ] {
        head.cancellation.as_mut().unwrap().digest = invalid;
        store
            .lock()
            .unwrap()
            .execute(
                "UPDATE model_cursor SET snapshot_json=?1 WHERE stream_key=?2",
                params![serde_json::to_vec(&head).unwrap(), stream().as_str()],
            )
            .unwrap();
        assert_eq!(store.load_head(&stream()), Err(AdapterStoreError::Corrupt));
    }
    head.cancellation = Some(cancellation.clone());
    head.cancellation.as_mut().unwrap().phase = ModelCancellationPhase::Applied;
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE model_cursor SET snapshot_json=?1 WHERE stream_key=?2",
            params![serde_json::to_vec(&head).unwrap(), stream().as_str()],
        )
        .unwrap();
    assert_eq!(store.load_head(&stream()), Err(AdapterStoreError::Corrupt));
    head.cancellation = Some(cancellation);
    head.termination = Some(ModelTerminationReason::MessageConflict);
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE model_cursor SET snapshot_json=?1 WHERE stream_key=?2",
            params![serde_json::to_vec(&head).unwrap(), stream().as_str()],
        )
        .unwrap();
    assert_eq!(store.load_head(&stream()), Err(AdapterStoreError::Corrupt));
}

fn final_response_fixture() -> ModelChunkMessage {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let open: winwincode_execution_port::generated::ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    winwincode_provider::model_failure(&open, "SYNTHETIC_PRIVATE_HISTORY")
}

fn successful_final_response_fixture(marker: &str) -> ModelChunkMessage {
    use base64::Engine as _;
    let mut chunk = final_response_fixture();
    let payload = serde_json::to_vec(&serde_json::json!({
        "type":"completed", "responseId":marker, "endTurn":true,
    }))
    .unwrap();
    chunk.error = None;
    chunk.payload = Some(winwincode_execution_port::generated::EncodedPayload {
        content_type: "application/json".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&payload),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload))),
    });
    crate::model_bridge::validate_chunk_for_retention(&chunk).unwrap();
    chunk
}

fn mutate_before_seal_writer_transaction(
    store: &AdapterStore,
    directory: &TempDirectory,
    changed_frame: Option<Vec<u8>>,
) -> Arc<Mutex<Option<Result<usize, String>>>> {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    let recorded = Arc::new(Mutex::new(None));
    let recorded_for_hook = Arc::clone(&recorded);
    let writer_path = directory.path().join(DATABASE_FILE);
    store
        .lock()
        .unwrap()
        .authorizer(Some(move |context: AuthContext<'_>| {
            if context.accessor.is_none()
                && matches!(
                    context.action,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Begin,
                    }
                )
            {
                let mut result = recorded_for_hook.lock().unwrap();
                if result.is_none() {
                    *result = Some((|| -> Result<usize, rusqlite::Error> {
                        let writer = Connection::open(&writer_path)?;
                        writer.execute_batch("BEGIN IMMEDIATE;")?;
                        let count = if let Some(bytes) = &changed_frame {
                            writer.execute(
                                "UPDATE model_call_frame SET frame_json=?1,frame_digest=?2 WHERE run_key='run' AND model_call_id='call' AND sequence=1",
                                params![bytes, format!("sha256:{:x}", Sha256::digest(bytes))],
                            )?
                        } else {
                            writer.execute(
                                "UPDATE model_call_ledger SET model_exchange_id='mdl_00000000000000000000000009' WHERE run_key='run' AND model_call_id='call'",
                                [],
                            )?
                        };
                        writer.execute_batch("COMMIT;")?;
                        Ok(count)
                    })()
                    .map_err(|error| error.to_string()));
                }
            }
            Authorization::Allow
        }))
        .unwrap();
    recorded
}

#[test]
fn model_response_publication_seals_current_frame_bytes_in_one_writer_transaction() {
    use rusqlite::hooks::{AuthContext, Authorization};
    let directory = TempDirectory::new();
    let store = AdapterStore::open(directory.path()).unwrap();
    let original = successful_final_response_fixture("SYNTHETIC_ORIGINAL_RESPONSE");
    let changed = successful_final_response_fixture("SYNTHETIC_CHANGED_RESPONSE");
    let changed_bytes = serde_json::to_vec(&changed).unwrap();
    store
        .claim_model_call(
            "run",
            "call",
            &original.model_exchange_id,
            &Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .unwrap();
    store
        .retain_model_call_frame("run", "call", &original)
        .unwrap();
    let mutation =
        mutate_before_seal_writer_transaction(&store, &directory, Some(changed_bytes.clone()));
    let result = store.seal_model_call_response("run", "call");
    store
        .lock()
        .unwrap()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(
        *mutation.lock().unwrap(),
        Some(Ok(1)),
        "the exact second-connection frame mutation must actually commit"
    );
    result.unwrap();
    let connection = store.lock().unwrap();
    let retained: Vec<u8> = connection.query_row("SELECT frame_json FROM model_call_frame WHERE run_key='run' AND model_call_id='call' AND sequence=1", [], |row|row.get(0)).unwrap();
    assert_eq!(retained, changed_bytes);
    let (count, seal): (i64, String) = connection.query_row("SELECT frame_count,response_digest FROM model_call_response_seal WHERE run_key='run' AND model_call_id='call'", [], |row|Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!(
        count, 1,
        "the mutation deliberately preserves the original count"
    );
    let mut digest = Sha256::new();
    digest.update((retained.len() as u64).to_le_bytes());
    digest.update(&retained);
    assert_eq!(
        seal,
        format!("sha256:{:x}", digest.finalize()),
        "published seal must bind the complete current body, never a prior independently read body"
    );
    assert!(connection.is_autocommit());
}

#[test]
fn model_response_publication_rejects_ledger_binding_changed_before_writer_transaction() {
    use rusqlite::hooks::{AuthContext, Authorization};
    let directory = TempDirectory::new();
    let store = AdapterStore::open(directory.path()).unwrap();
    let original = successful_final_response_fixture("SYNTHETIC_ORIGINAL_RESPONSE");
    store
        .claim_model_call(
            "run",
            "call",
            &original.model_exchange_id,
            &Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .unwrap();
    store
        .retain_model_call_frame("run", "call", &original)
        .unwrap();
    let mutation = mutate_before_seal_writer_transaction(&store, &directory, None);
    let result = store.seal_model_call_response("run", "call");
    store
        .lock()
        .unwrap()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(
        *mutation.lock().unwrap(),
        Some(Ok(1)),
        "the exact second-connection ledger mutation must actually commit"
    );
    let connection = store.lock().unwrap();
    let exchange: String = connection.query_row("SELECT model_exchange_id FROM model_call_ledger WHERE run_key='run' AND model_call_id='call'", [], |row|row.get(0)).unwrap();
    assert_eq!(exchange, "mdl_00000000000000000000000009");
    assert_eq!(
        result,
        Err(AdapterStoreError::Corrupt),
        "writer transaction must validate its current ledger binding against the full response"
    );
    let seals: i64 = connection.query_row("SELECT COUNT(*) FROM model_call_response_seal WHERE run_key='run' AND model_call_id='call'", [], |row|row.get(0)).unwrap();
    assert_eq!(
        seals, 0,
        "invalidated binding must not publish a response seal"
    );
    assert!(connection.is_autocommit());
}

#[test]
fn model_response_seal_rejects_corrupt_historical_payload_and_wrong_exchange() {
    for invalid_payload in [true, false] {
        let directory = TempDirectory::new();
        let store = AdapterStore::open(directory.path()).unwrap();
        let mut chunk = final_response_fixture();
        store
            .claim_model_call(
                "run",
                "call",
                &chunk.model_exchange_id,
                &Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            )
            .unwrap();
        if invalid_payload {
            use base64::Engine as _;
            chunk.payload = Some(winwincode_execution_port::generated::EncodedPayload {
                content_type: "application/json".into(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(b"{}"),
                payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            });
        } else {
            chunk.model_exchange_id.0 = "mdl_other".into();
        }
        store
            .retain_model_call_frame("run", "call", &chunk)
            .unwrap();
        assert_eq!(
            store.seal_model_call_response("run", "call"),
            Err(AdapterStoreError::Corrupt)
        );
        let count: i64 = store
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM model_call_response_seal", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn sealed_model_response_rejects_append_and_corrupt_seal() {
    let directory = TempDirectory::new();
    let store = AdapterStore::open(directory.path()).unwrap();
    let mut chunk = final_response_fixture();
    store
        .claim_model_call(
            "run",
            "call",
            &chunk.model_exchange_id,
            &Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .unwrap();
    store
        .retain_model_call_frame("run", "call", &chunk)
        .unwrap();
    store.seal_model_call_response("run", "call").unwrap();
    store.seal_model_call_response("run", "call").unwrap();
    chunk.sequence = winwincode_domain::ExecutionSequence(2);
    assert_eq!(
        store.retain_model_call_frame("run", "call", &chunk),
        Err(AdapterStoreError::Conflict)
    );
    let mut changed = final_response_fixture();
    changed.message_id.0 = "xmsg_changed".into();
    let bytes = serde_json::to_vec(&changed).unwrap();
    assert!(
        store
            .lock()
            .unwrap()
            .execute(
                "UPDATE model_call_frame SET frame_json=?1,frame_digest=?2",
                params![bytes, format!("sha256:{:x}", Sha256::digest(&bytes))]
            )
            .is_err(),
        "sealed original body+digest cannot change together"
    );
    assert!(
        store
            .lock()
            .unwrap()
            .execute("DELETE FROM model_call_frame", [])
            .is_err()
    );
    assert!(store.lock().unwrap().execute("INSERT INTO model_call_frame(run_key,model_call_id,sequence,frame_digest,frame_json) SELECT run_key,model_call_id,2,frame_digest,frame_json FROM model_call_frame",[]).is_err());
    assert!(
        store
            .lock()
            .unwrap()
            .execute(
                "UPDATE model_call_response_seal SET response_digest='bad'",
                []
            )
            .is_err()
    );
    store.seal_model_call_response("run", "call").unwrap();
    drop(store);
    let store = AdapterStore::open(directory.path()).unwrap();
    assert_eq!(
        store.load_model_call_frames("run", "call").unwrap(),
        vec![final_response_fixture()]
    );
    store
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER model_call_response_seal_immutable")
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE model_call_response_seal SET response_digest='bad'",
            [],
        )
        .unwrap();
    assert_eq!(
        store.seal_model_call_response("run", "call"),
        Err(AdapterStoreError::Corrupt)
    );
}

#[test]
fn sealed_model_response_marker_rejects_delete_and_replace() {
    let mut violations = Vec::new();
    for operation in ["delete", "replace"] {
        let directory = TempDirectory::new();
        let store = AdapterStore::open(directory.path()).unwrap();
        let chunk = final_response_fixture();
        store
            .claim_model_call(
                "run",
                "call",
                &chunk.model_exchange_id,
                &Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            )
            .unwrap();
        store
            .retain_model_call_frame("run", "call", &chunk)
            .unwrap();
        store.seal_model_call_response("run", "call").unwrap();
        let connection = store.lock().unwrap();
        let before: (i64, String) = connection
            .query_row(
                "SELECT frame_count,response_digest FROM model_call_response_seal WHERE run_key='run' AND model_call_id='call'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let result = match operation {
            "delete" => connection.execute(
                "DELETE FROM model_call_response_seal WHERE run_key='run' AND model_call_id='call'",
                [],
            ),
            "replace" => connection.execute(
                "INSERT OR REPLACE INTO model_call_response_seal(run_key,model_call_id,frame_count,response_digest) VALUES('run','call',1,?1)",
                [format!("sha256:{}", "0".repeat(64))],
            ),
            _ => unreachable!(),
        };
        let after: Option<(i64, String)> = connection
            .query_row(
                "SELECT frame_count,response_digest FROM model_call_response_seal WHERE run_key='run' AND model_call_id='call'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .unwrap();
        let binding: (String, String) = connection
            .query_row(
                "SELECT model_exchange_id,request_digest FROM model_call_ledger WHERE run_key='run' AND model_call_id='call'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(connection.is_autocommit());
        drop(connection);
        assert_eq!(
            store.load_model_call_frames("run", "call").unwrap(),
            vec![chunk.clone()],
            "the exact existing frames must remain intact: {operation}"
        );
        assert_eq!(
            binding,
            (
                chunk.model_exchange_id.0,
                format!("sha256:{}", "a".repeat(64))
            ),
            "the exact ledger binding must remain intact: {operation}"
        );
        if result.is_ok() || after.as_ref() != Some(&before) {
            violations.push(format!(
                "{operation}: write_result={result:?}, seal_unchanged={}",
                after.as_ref() == Some(&before)
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "a published Adapter seal must reject DELETE and REPLACE: {violations:?}"
    );
}

#[test]
fn retained_model_open_lookup_reads_only_the_exact_request() {
    let directory = TempDirectory::new();
    let store = AdapterStore::open(directory.path()).unwrap();
    let outbox = crate::outbox::ExecutionOutbox::open(store.clone()).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let open: winwincode_execution_port::generated::ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let message = ExecutionPortMessage::ModelOpenMessage(open.clone());
    outbox.retain(&message).unwrap();
    let own_bytes = serde_json::to_vec(&message).unwrap().len();
    let mut foreign_bytes = 0_usize;
    {
        let connection = store.lock().unwrap();
        for index in 0..500 {
            let bytes=serde_json::to_vec(&serde_json::json!({"kind":"foreign-private-record","modelExchangeId":format!("foreign-{index}"),"syntheticPadding":"x".repeat(4096)})).unwrap();
            foreign_bytes += bytes.len();
            connection.execute("INSERT INTO execution_outbox(delivery_id,family,correlation_key,acknowledgement_required,state,frame_digest,frame_json) VALUES(?1,'model_open',?1,0,'sent_attempt','foreign',?2)",params![format!("foreign-{index}"),bytes]).unwrap();
        }
        let mut plan=connection.prepare("EXPLAIN QUERY PLAN SELECT frame_digest,frame_json FROM execution_outbox WHERE family='model_open' AND json_extract(frame_json,'$.modelExchangeId')=?1 ORDER BY position").unwrap();
        let descriptions = plan
            .query_map([&open.model_exchange_id.0], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            descriptions
                .iter()
                .any(|line| line.contains("execution_outbox_model_exchange")),
            "{descriptions:?}"
        );
    }
    crate::audit_model_metrics::reset();
    for _ in 0..100 {
        assert_eq!(
            outbox.retained_model_open(&open.model_exchange_id).unwrap(),
            Some(open.clone())
        );
    }
    assert!(
        outbox
            .retained_model_open(&ModelExchangeId("absent".into()))
            .unwrap()
            .is_none()
    );
    let metrics = crate::audit_model_metrics::take();
    assert_eq!(metrics.model_open_rows_read, 100);
    assert_eq!(
        metrics.model_open_json_bytes_read,
        u64::try_from(100 * own_bytes).unwrap()
    );
    crate::audit_model_metrics::report(
        "exact-model-open-lookup",
        &serde_json::json!({"foreign_rows":500,"foreign_private_bytes":foreign_bytes,"exact_lookups":100,"foreign_rows_deserialized":0,"metrics":metrics}),
    );
}

#[test]
fn sealed_model_call_binding_is_immutable_while_completion_phase_advances() {
    let directory = TempDirectory::new();
    let store = AdapterStore::open(directory.path()).unwrap();
    let chunk = final_response_fixture();
    store
        .claim_model_call(
            "run",
            "call",
            &chunk.model_exchange_id,
            &Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .unwrap();
    store
        .retain_model_call_frame("run", "call", &chunk)
        .unwrap();
    store.seal_model_call_response("run", "call").unwrap();
    for sql in [
        "UPDATE model_call_ledger SET model_exchange_id='changed'",
        "UPDATE model_call_ledger SET request_digest='changed'",
        "UPDATE model_call_ledger SET run_key='changed'",
        "UPDATE model_call_ledger SET model_call_id='changed'",
        "DELETE FROM model_call_ledger",
    ] {
        assert!(store.lock().unwrap().execute_batch(sql).is_err(), "{sql}");
        store.seal_model_call_response("run", "call").unwrap();
    }
    store.mark_model_call_provider_final("run", "call").unwrap();
    store.commit_model_call("run", "call").unwrap();
    assert_eq!(
        store.model_call_phase("run", "call").unwrap(),
        Some(ModelCallPhase::CoreCommitted)
    );
    drop(store);
    let reopened = AdapterStore::open(directory.path()).unwrap();
    assert_eq!(
        reopened.load_model_call_frames("run", "call").unwrap(),
        vec![chunk]
    );
    assert_eq!(
        reopened.model_call_phase("run", "call").unwrap(),
        Some(ModelCallPhase::CoreCommitted)
    );
}
