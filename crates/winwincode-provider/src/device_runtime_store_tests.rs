// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{DeviceProviderStore, model_failure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rusqlite::params;
use sha2::Sha256;
use winwincode_execution_port::generated::{ModelChunkMessage, ModelOpenMessage};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "provider-runtime-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        Self(root)
    }

    fn legacy(&self) -> Connection {
        let store = DeviceProviderStore::open(&self.0).unwrap();
        drop(store);
        fs::remove_dir_all(self.0.join("runtime")).unwrap();
        let connection = Connection::open(self.0.join("providers.sqlite3")).unwrap();
        connection
            .execute_batch(
                "DROP TABLE provider_runtime_cutover;
                 ALTER TABLE jev_context_exchanges DROP COLUMN original_receipt_digest;
                 ALTER TABLE jev_judge_exchanges DROP COLUMN original_receipt_digest;
                 PRAGMA user_version=16;",
            )
            .unwrap();
        connection
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request() -> ModelOpenMessage {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut open: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let payload = serde_json::to_vec(&serde_json::json!({
        "provider":"fixture-provider","request":{"model":"fixture-model"}
    }))
    .unwrap();
    open.request = winwincode_execution_port::generated::EncodedPayload {
        content_type: "application/json".into(),
        data_base64: STANDARD.encode(&payload),
        payload_digest: winwincode_domain::Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(&payload)
        )),
    };
    open
}

fn origin(connection: &Connection) -> String {
    let bytes: Vec<u8> = connection
        .query_row("SELECT private_key FROM identity", [], |row| row.get(0))
        .unwrap();
    let key = p256::SecretKey::from_slice(&bytes).unwrap();
    format!(
        "{:x}",
        Sha256::digest(key.public_key().to_encoded_point(false).as_bytes())
    )
}

fn retain_call(connection: &Connection) -> (ModelOpenMessage, Vec<ModelChunkMessage>) {
    let open = request();
    let request = serde_json::to_string(&open).unwrap();
    let chunks = vec![model_failure(&open, "retained terminal")];
    let payload = STANDARD.decode(&open.request.data_base64).unwrap();
    connection.execute("INSERT INTO exchanges(exchange_id,digest,request_open,chunks,prepared_payload,prepared_payload_digest) VALUES(?1,?2,?3,?4,?5,?6)", params![open.model_exchange_id.0,format!("{:x}",Sha256::digest(request.as_bytes())),request,serde_json::to_string(&chunks).unwrap(),payload,crate::model_response_store::frame_digest(&payload)]).unwrap();
    let validated = crate::model_response_store::validate_model_response(
        &open.model_exchange_id.0,
        &chunks,
        Some(&request),
    )
    .unwrap();
    crate::model_response_store::insert_model_response_index(
        connection,
        &open.model_exchange_id.0,
        &validated,
    )
    .unwrap();
    connection.execute("INSERT INTO model_invocation_attempts(exchange_id,attempt_number,adapter_request_id,state,request_json,request_payload,accounting_chunks,response_bytes,response_bytes_received) VALUES(?1,1,'retained-adapter-request','completed',?2,?3,?4,?5,77)",params![open.model_exchange_id.0,request,payload,serde_json::to_string(&chunks).unwrap(),b"historical-secret-body".as_slice()]).unwrap();
    (open, chunks)
}

#[test]
#[allow(clippy::too_many_lines)]
fn migration_preserves_calls_frames_cancellation_and_usage_without_copying_credentials() {
    let fixture = Fixture::new();
    let connection = fixture.legacy();
    let (open, chunks) = retain_call(&connection);
    connection.execute("INSERT INTO providers VALUES('secret-provider','configuration','provider-secret-canary')",[]).unwrap();
    connection
        .execute(
            "INSERT INTO provider_headers VALUES('secret-provider','header-secret-canary')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO jev_settings VALUES('secret-provider','jev-secret-canary')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO extensions VALUES('mcp','private','mcp-secret-canary','{}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO receipts VALUES('original-config-request','digest','receipt')",
            [],
        )
        .unwrap();
    connection.execute("INSERT INTO extension_receipts VALUES('original-extension-request','digest','receipt')",[]).unwrap();
    connection.execute("INSERT INTO exchanges(exchange_id,digest,cancelled) VALUES('cancelled-before-open','',1)",[]).unwrap();
    connection.execute("INSERT INTO exchanges(exchange_id,digest,request_open) VALUES('pending-exchange','original-digest','original-request')",[]).unwrap();
    connection.execute("INSERT INTO model_invocation_attempts(exchange_id,attempt_number,adapter_request_id,state,request_json,request_payload) VALUES('pending-exchange',2,'unknown-adapter-request','invoking','original-request',x'000102')",[]).unwrap();
    connection
        .execute(
            "INSERT INTO model_retry_state VALUES('pending-exchange',2,1,123,'retry_after')",
            [],
        )
        .unwrap();
    connection.execute("INSERT INTO model_attempt_diagnostics VALUES('pending-exchange',1,2,10,NULL,'in_flight',NULL,NULL)",[]).unwrap();
    connection.execute("INSERT INTO jev_context_exchanges VALUES('jev:retained:one','digest','context-request','context-result','succeeded')",[]).unwrap();
    connection.execute("INSERT INTO jev_judge_exchanges VALUES('judge:retained:one','judge-request','judge-result','succeeded')",[]).unwrap();
    connection
        .execute(
            "INSERT INTO jev_attempt_diagnostics VALUES('jev:retained:one',1,'context',12,'{}')",
            [],
        )
        .unwrap();
    connection.execute("INSERT INTO jev_invocation_attempts VALUES('jev:retained:one',1,'physical-request','succeeded','{\"observation\":{\"cost\":47}}')",[]).unwrap();
    connection.execute("INSERT INTO jev_invocation_attempts VALUES('jev:retained:one',2,'unknown-physical-request','pending',NULL)",[]).unwrap();
    connection
        .execute(
            "INSERT INTO accounting_closed_attempts VALUES(?1,?2,?3)",
            params![
                open.lease.job_id.0,
                open.lease.attempt,
                open.lease.lease_id.0
            ],
        )
        .unwrap();
    let original_key: Vec<u8> = connection
        .query_row("SELECT private_key FROM identity", [], |row| row.get(0))
        .unwrap();
    drop(connection);

    let store = DeviceProviderStore::open(&fixture.0).unwrap();
    assert_eq!(
        store.runtime_directory(),
        fs::canonicalize(fixture.0.join("runtime")).unwrap()
    );
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
    assert!(store.model_cancelled("cancelled-before-open").unwrap());
    assert_eq!(store.runtime.connection.query_row("SELECT state FROM model_invocation_attempts WHERE adapter_request_id='unknown-adapter-request'",[],|row|row.get::<_,String>(0)).unwrap(),"invoking");
    assert_eq!(store.runtime.connection.query_row("SELECT request_payload FROM model_invocation_attempts WHERE adapter_request_id='unknown-adapter-request'",[],|row|row.get::<_,Vec<u8>>(0)).unwrap(),vec![0,1,2]);
    assert_eq!(store.runtime.connection.query_row("SELECT response_bytes,response_bytes_received FROM model_invocation_attempts WHERE adapter_request_id='retained-adapter-request'",[],|row|Ok((row.get::<_,Option<Vec<u8>>>(0)?,row.get::<_,i64>(1)?))).unwrap(),(None,77));
    assert_eq!(store.connection.query_row("SELECT response_bytes FROM model_invocation_attempts WHERE adapter_request_id='retained-adapter-request'",[],|row|row.get::<_,Vec<u8>>(0)).unwrap(),b"historical-secret-body");
    assert_eq!(
        store
            .runtime
            .connection
            .query_row(
                "SELECT status FROM jev_invocation_attempts WHERE sequence=2",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "pending"
    );
    assert_eq!(
        store
            .runtime
            .connection
            .query_row(
                "SELECT count(*) FROM accounting_closed_attempts",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    let mut extra = open.clone();
    extra.model_exchange_id.0 = "mdl_00000000000000000000000099".into();
    assert!(store.execute_model(&extra).is_err());
    let table_names = store
        .runtime
        .connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(table_names.len(), 12);
    assert!(!table_names.iter().any(|name| {
        [
            "identity",
            "providers",
            "provider_headers",
            "jev_settings",
            "extensions",
        ]
        .contains(&name.as_str())
    }));
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        17
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT private_key FROM identity", [], |row| row
                .get::<_, Vec<u8>>(0))
            .unwrap(),
        original_key
    );
    drop(store);
    let bytes = fs::read(fixture.0.join("runtime").join(DATABASE)).unwrap();
    for secret in [
        b"provider-secret-canary".as_slice(),
        b"header-secret-canary",
        b"jev-secret-canary",
        b"mcp-secret-canary",
        b"historical-secret-body",
        original_key.as_slice(),
    ] {
        assert!(!bytes.windows(secret.len()).any(|value| value == secret));
    }
    fs::rename(
        fixture.0.join("providers.sqlite3"),
        fixture.0.join("archived.sqlite3"),
    )
    .unwrap();
    let runtime = ProviderRuntimeStore::open_existing(&fixture.0.join("runtime")).unwrap();
    assert!(runtime.execution_source.is_none());
    assert_eq!(
        runtime.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
}

#[test]
fn completed_copy_before_cutover_is_reused_without_reissuing_calls() {
    let fixture = Fixture::new();
    let mut source = fixture.legacy();
    let (open, chunks) = retain_call(&source);
    let source_id = origin(&source);
    let transaction = source
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let runtime = migrate_runtime(&transaction, &fixture.0.join("runtime"), &source_id).unwrap();
    drop(runtime);
    drop(transaction);
    assert_eq!(
        source
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        16
    );
    drop(source);
    let store = DeviceProviderStore::open(&fixture.0).unwrap();
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
    store
        .runtime
        .connection
        .execute(
            "INSERT INTO exchanges(exchange_id,digest,cancelled) VALUES('after-cutover','',1)",
            [],
        )
        .unwrap();
    drop(store);
    let reopened = DeviceProviderStore::open(&fixture.0).unwrap();
    assert!(reopened.model_cancelled("after-cutover").unwrap());
    assert_eq!(
        reopened
            .runtime
            .connection
            .query_row(
                "SELECT count(*) FROM model_invocation_attempts",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn failed_copy_rolls_back_target_and_can_resume_after_source_is_readable() {
    let fixture = Fixture::new();
    let source = fixture.legacy();
    let (open, chunks) = retain_call(&source);
    source
        .execute_batch("ALTER TABLE model_attempt_diagnostics RENAME TO retained_diagnostics;")
        .unwrap();
    drop(source);
    assert!(DeviceProviderStore::open(&fixture.0).is_err());
    assert!(!fixture.0.join("runtime").join(DATABASE).exists());
    let source = Connection::open(fixture.0.join("providers.sqlite3")).unwrap();
    assert_eq!(
        source
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        16
    );
    source
        .execute_batch("ALTER TABLE retained_diagnostics RENAME TO model_attempt_diagnostics;")
        .unwrap();
    drop(source);
    let store = DeviceProviderStore::open(&fixture.0).unwrap();
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
}

#[test]
fn runtime_open_never_creates_a_missing_database_or_falls_back_to_device_storage() {
    let fixture = Fixture::new();
    assert!(ProviderRuntimeStore::open_existing(&fixture.0).is_err());
    assert!(!fixture.0.join(DATABASE).exists());
    let store = DeviceProviderStore::open(&fixture.0).unwrap();
    drop(store);
    fs::remove_dir_all(fixture.0.join("runtime")).unwrap();
    assert!(DeviceProviderStore::open(&fixture.0).is_err());
    assert!(!fixture.0.join("runtime").exists());
    fs::hard_link(
        fixture.0.join("providers.sqlite3"),
        fixture.0.join(DATABASE),
    )
    .unwrap();
    assert!(ProviderRuntimeStore::open_existing(&fixture.0).is_err());
}

#[test]
fn a_runtime_from_another_device_cannot_complete_cutover() {
    let fixture = Fixture::new();
    let source = fixture.legacy();
    let source_id = origin(&source);
    let runtime = migrate_runtime(&source, &fixture.0.join("runtime"), &source_id).unwrap();
    runtime
        .connection
        .execute("UPDATE runtime_origin SET source_id=?1", ["0".repeat(64)])
        .unwrap();
    drop(runtime);
    drop(source);
    assert!(DeviceProviderStore::open(&fixture.0).is_err());
    let source = Connection::open(fixture.0.join("providers.sqlite3")).unwrap();
    assert_eq!(
        source
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        16
    );
}

#[test]
fn rejected_source_commit_preserves_v16_and_reuses_the_published_copy() {
    let fixture = Fixture::new();
    let mut source = fixture.legacy();
    let (open, chunks) = retain_call(&source);
    let source_id = origin(&source);
    source.commit_hook(Some(|| true)).unwrap();
    let transaction = source
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let runtime = migrate_runtime(&transaction, &fixture.0.join("runtime"), &source_id).unwrap();
    transaction.execute_batch("CREATE TABLE provider_runtime_cutover(singleton INTEGER PRIMARY KEY CHECK(singleton=1),source_id TEXT NOT NULL); PRAGMA user_version=17;").unwrap();
    transaction
        .execute(
            "INSERT INTO provider_runtime_cutover VALUES(1,?1)",
            [&source_id],
        )
        .unwrap();
    assert!(transaction.commit().is_err());
    source.commit_hook(None::<fn() -> bool>).unwrap();
    assert_eq!(
        source
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        16
    );
    assert_eq!(
        source
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='provider_runtime_cutover'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    drop(runtime);
    drop(source);
    let store = DeviceProviderStore::open(&fixture.0).unwrap();
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
    assert_eq!(
        store
            .runtime
            .connection
            .query_row(
                "SELECT count(*) FROM model_invocation_attempts",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn migrated_payload_immutability_and_response_seal_triggers_still_apply() {
    let fixture = Fixture::new();
    let source = fixture.legacy();
    let (open, chunks) = retain_call(&source);
    drop(source);
    let store = DeviceProviderStore::open(&fixture.0).unwrap();
    assert!(
        store
            .runtime
            .connection
            .execute(
                "UPDATE exchanges SET prepared_payload=x'00' WHERE exchange_id=?1",
                [&open.model_exchange_id.0]
            )
            .is_err()
    );
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        chunks
    );
    store
        .runtime
        .connection
        .execute(
            "UPDATE model_response_frame SET frame_digest='tampered' WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
        )
        .unwrap();
    assert_eq!(
        store
            .runtime
            .connection
            .query_row("SELECT count(*) FROM model_response_exchange", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    assert!(store.replay_model(&open.model_exchange_id.0, 1).is_err());
    assert_eq!(
        store
            .runtime
            .connection
            .query_row(
                "SELECT chunks FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        serde_json::to_string(&chunks).unwrap()
    );
}

fn legacy_jev_runs(
    resolved_model: &str,
) -> (
    crate::JevRun<crate::JevContextEvaluation>,
    crate::JevRun<[f32; 3]>,
) {
    let observation = crate::JevObservation {
        provider_id: "fixture-jev".into(),
        model_id: "requested-jev-model".into(),
        latency: Duration::from_millis(1),
        input_tokens: 11,
        output_tokens: Some(7),
        resolved_model_id: Some(resolved_model.into()),
        batch_size: 4,
        device: crate::JevDevice::Remote,
        confidence: 0.75,
    };
    let scores = serde_json::json!({"entailment":0.75,"contradiction":0.125,"neutral":0.125});
    let context = crate::JevRun {
        value: Some(
            serde_json::from_value(serde_json::json!({
                "hypotheses":{
                    "critical":scores,"relevant":scores,"compressible":scores,"disposable":scores
                },
                "decision":{
                    "decision":"PIN","confidence":1.0,"provider":"fixture-jev",
                    "model":resolved_model,"policyVersion":"v1","reasonCodes":["protected_by_system"]
                }
            }))
            .unwrap(),
        ),
        observation: Some(observation.clone()),
        failures: Vec::new(),
    };
    let judge = crate::JevRun {
        value: Some([0.75, 0.125, 0.125]),
        observation: Some(crate::JevObservation {
            batch_size: 1,
            ..observation
        }),
        failures: Vec::new(),
    };
    (context, judge)
}

#[test]
#[allow(clippy::too_many_lines)]
fn legacy_jev_model_echoes_are_redacted_without_changing_accounting_and_cutover_reentry() {
    let fixture = Fixture::new();
    let mut source = fixture.legacy();
    let (open, _) = retain_call(&source);
    let secret = "sk-legacy-jev-response-secret-canary";
    let (context, judge) = legacy_jev_runs(secret);
    let context_operation = format!("jev:{}:0", open.model_exchange_id.0);
    let clean_operation = format!("jev:{}:1", open.model_exchange_id.0);
    let judge_operation = format!("judge:{}:0", open.model_exchange_id.0);
    let context_request = "{\"fixture\":\"context\"}";
    let judge_request = "{\"fixture\":\"judge\"}";
    source.execute("INSERT INTO jev_settings VALUES('fixture-jev',?1)", [serde_json::json!({"providerId":"fixture-jev","endpoint":"https://fixture.invalid/jev","apiKey":secret,"modelId":"requested-jev-model","timeoutMs":1000,"retries":0,"maxBatchSize":4,"devices":["remote"],"dtypes":["auto"]}).to_string()]).unwrap();
    source
        .execute(
            "INSERT INTO jev_context_exchanges VALUES(?1,?2,?3,?4,'succeeded')",
            params![
                context_operation,
                format!("{:x}", Sha256::digest(context_request.as_bytes())),
                context_request,
                serde_json::to_string(&context).unwrap()
            ],
        )
        .unwrap();
    let mut clean = context.clone();
    clean.observation.as_mut().unwrap().resolved_model_id = None;
    clean.value.as_mut().unwrap().decision.model = "requested-jev-model".into();
    let clean_result = serde_json::to_string(&clean).unwrap();
    source
        .execute(
            "INSERT INTO jev_context_exchanges VALUES(?1,?2,?3,?4,'succeeded')",
            params![
                clean_operation,
                format!("{:x}", Sha256::digest(context_request.as_bytes())),
                context_request,
                clean_result
            ],
        )
        .unwrap();
    source
        .execute(
            "INSERT INTO jev_judge_exchanges VALUES(?1,?2,?3,'succeeded')",
            params![
                judge_operation,
                judge_request,
                serde_json::to_string(&judge).unwrap()
            ],
        )
        .unwrap();
    for (operation, run, count) in [
        (&context_operation, &context.observation, 4),
        (&judge_operation, &judge.observation, 1),
    ] {
        let value = serde_json::json!({
            "resolved_model_id":secret,"input_tokens":11,"output_tokens":7,"batch_size":count,
            "scores":{"entailment":0.75,"contradiction":0.125,"neutral":0.125}
        });
        source.execute("INSERT INTO jev_invocation_attempts VALUES(?1,1,'original-physical-request','succeeded',?2)", params![operation,serde_json::json!({"value":value,"observation":run}).to_string()]).unwrap();
    }
    source.execute_batch("ALTER TABLE jev_context_exchanges ADD COLUMN original_receipt_digest TEXT; ALTER TABLE jev_judge_exchanges ADD COLUMN original_receipt_digest TEXT;").unwrap();
    let before_runtime = ProviderRuntimeStore {
        connection: Connection::open(fixture.0.join("providers.sqlite3")).unwrap(),
        execution_source: None,
        directory: fixture.0.clone(),
    };
    let before = before_runtime
        .accounting_statement(&open.lease)
        .unwrap()
        .unwrap();
    drop(before_runtime);
    source.execute_batch("ALTER TABLE jev_context_exchanges DROP COLUMN original_receipt_digest; ALTER TABLE jev_judge_exchanges DROP COLUMN original_receipt_digest;").unwrap();
    for (table, count) in [("jev_context_exchanges", 5), ("jev_judge_exchanges", 4)] {
        assert_eq!(
            source
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .count(),
            count
        );
    }
    let context_digest = before
        .receipts
        .iter()
        .find(|receipt| receipt.slot == context_operation)
        .unwrap()
        .source_digest
        .0
        .clone();
    let source_id = origin(&source);
    let transaction = source
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let runtime = migrate_runtime(&transaction, &fixture.0.join("runtime"), &source_id).unwrap();
    assert_eq!(
        runtime.accounting_statement(&open.lease).unwrap(),
        Some(before.clone())
    );
    verify_copy(&transaction, &runtime.connection).unwrap();
    runtime
        .connection
        .execute(
            "UPDATE jev_context_exchanges SET original_receipt_digest=?1 WHERE operation_id=?2",
            params![format!("sha256:{}", "0".repeat(64)), context_operation],
        )
        .unwrap();
    assert!(migrate_runtime(&transaction, &fixture.0.join("runtime"), &source_id).is_err());
    runtime
        .connection
        .execute(
            "UPDATE jev_context_exchanges SET original_receipt_digest=?1 WHERE operation_id=?2",
            params![context_digest, context_operation],
        )
        .unwrap();
    drop(runtime);
    drop(transaction);
    assert_eq!(
        source
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        16
    );
    drop(source);

    let store = DeviceProviderStore::open(&fixture.0).unwrap();
    assert_eq!(
        store.accounting_statement(&open.lease).unwrap(),
        Some(before.clone())
    );
    for (table, column) in [
        ("jev_context_exchanges", "result"),
        ("jev_judge_exchanges", "result"),
        ("jev_invocation_attempts", "result_json"),
    ] {
        let retained = store
            .runtime
            .connection
            .prepare(&format!(
                "SELECT {column} FROM {table} ORDER BY operation_id"
            ))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(retained.iter().all(|result| !result.contains(secret)));
        let archived = store
            .connection
            .prepare(&format!("SELECT {column} FROM {table}"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(archived.iter().any(|result| result.contains(secret)));
    }
    for (table, operation, original) in [
        (
            "jev_context_exchanges",
            &context_operation,
            serde_json::to_string(&context).unwrap(),
        ),
        (
            "jev_judge_exchanges",
            &judge_operation,
            serde_json::to_string(&judge).unwrap(),
        ),
    ] {
        let (redacted, digest): (String, String) = store
            .runtime
            .connection
            .query_row(
                &format!(
                    "SELECT result,original_receipt_digest FROM {table} WHERE operation_id=?1"
                ),
                [operation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            digest,
            before
                .receipts
                .iter()
                .find(|receipt| &receipt.slot == operation)
                .unwrap()
                .source_digest
                .0
        );
        let redacted: serde_json::Value = serde_json::from_str(&redacted).unwrap();
        let original: serde_json::Value = serde_json::from_str(&original).unwrap();
        assert_eq!(
            redacted["observation"]["resolvedModelId"],
            serde_json::Value::Null
        );
        assert_eq!(
            redacted["observation"]["inputTokens"],
            original["observation"]["inputTokens"]
        );
        assert_eq!(
            redacted["observation"]["outputTokens"],
            original["observation"]["outputTokens"]
        );
        if table == "jev_context_exchanges" {
            assert_eq!(
                redacted["value"]["decision"]["model"],
                "requested-jev-model"
            );
            assert_eq!(
                redacted["value"]["hypotheses"],
                original["value"]["hypotheses"]
            );
        } else {
            assert_eq!(redacted["value"], original["value"]);
        }
    }
    assert_eq!(store.runtime.connection.query_row("SELECT result,original_receipt_digest FROM jev_context_exchanges WHERE operation_id=?1",[clean_operation],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?))).unwrap(),(clean_result,None));
    drop(store);
    for entry in fs::read_dir(fixture.0.join("runtime")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            assert!(
                !fs::read(path)
                    .unwrap()
                    .windows(secret.len())
                    .any(|bytes| bytes == secret.as_bytes())
            );
        }
    }
    let reopened = DeviceProviderStore::open(&fixture.0).unwrap();
    assert_eq!(
        reopened.accounting_statement(&open.lease).unwrap(),
        Some(before)
    );
}
