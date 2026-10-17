// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};
use winwincode_api::generated::{
    DeviceConfigurationEnvelope, DeviceProviderConfig, DeviceProviderOutcome,
    DeviceProviderProtocol, DeviceProviderSnapshot,
};
use winwincode_provider::{
    DeviceProviderStore, ProviderRuntimeStore, valid_device_provider_config,
};

#[test]
fn responses_protocol_accepts_custom_https_endpoints_without_chatgpt_binding() {
    let mut config: DeviceProviderConfig = serde_json::from_value(serde_json::json!({
        "providerId":"custom-responses", "displayName":"Custom Responses",
        "endpoint":"https://models.example/v1/responses", "protocol":"openai_responses",
        "modelIds":["custom-model"], "enabled":true
    }))
    .expect("public Responses configuration contract");
    assert_eq!(config.protocol, DeviceProviderProtocol::OpenaiResponses);
    assert!(valid_device_provider_config(&config));
    config.endpoint = "https://another.example/api/responses".into();
    assert!(valid_device_provider_config(&config));
    config.endpoint = "http://another.example/api/responses".into();
    assert!(!valid_device_provider_config(&config));
    config.endpoint = "https://models.example/v1/responses".into();
    for protocol in [
        DeviceProviderProtocol::CodexChatgpt,
        DeviceProviderProtocol::ChatgptPlan,
    ] {
        config.protocol = protocol;
        assert!(!valid_device_provider_config(&config));
    }
}

#[test]
fn responses_structured_output_is_optional_and_exclusive_to_responses() {
    for (protocol, endpoint) in [
        ("openai_responses", "https://models.example/v1/responses"),
        ("anthropic_messages", "https://models.example/v1/messages"),
        (
            "openai_chat_completions",
            "https://models.example/v1/chat/completions",
        ),
        ("canonical", "https://models.example/v1/canonical"),
        (
            "codex_chatgpt",
            "https://chatgpt.com/backend-api/codex/responses",
        ),
        ("chatgpt_plan", "https://api.openai.com/v1/responses"),
    ] {
        let baseline = serde_json::json!({
            "providerId":"structured-test", "displayName":"Structured Test",
            "endpoint":endpoint, "protocol":protocol, "modelIds":["test-model"], "enabled":true
        });
        let config: DeviceProviderConfig = serde_json::from_value(baseline.clone()).unwrap();
        assert!(valid_device_provider_config(&config), "{protocol} default");
        assert!(
            serde_json::to_value(config)
                .unwrap()
                .get("responsesStructuredOutput")
                .is_none()
        );
        for mode in ["json_schema", "json_object", "text"] {
            let mut configured = baseline.clone();
            configured["responsesStructuredOutput"] = mode.into();
            let config: DeviceProviderConfig = serde_json::from_value(configured).unwrap();
            assert_eq!(
                valid_device_provider_config(&config),
                protocol == "openai_responses",
                "{protocol}/{mode}"
            );
        }
        for mode in [serde_json::json!("unsupported"), serde_json::json!(1)] {
            let mut configured = baseline.clone();
            configured["responsesStructuredOutput"] = mode;
            assert!(serde_json::from_value::<DeviceProviderConfig>(configured).is_err());
        }
    }
}

#[test]
fn opening_current_device_store_does_not_require_the_writer_lock() {
    let directory =
        std::env::temp_dir().join(format!("wwc-device-provider-reader-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    drop(ProviderRuntimeStore::prepare(&directory).expect("initialize current schema"));
    let writer = rusqlite::Connection::open(directory.join("providers.sqlite3"))
        .expect("concurrent accounting writer");
    writer
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold writer");
    let reader = DeviceProviderStore::open(&directory);
    writer.execute_batch("ROLLBACK").expect("release writer");
    assert!(
        reader.is_ok(),
        "opening an existing store is a read operation"
    );
    let reader = reader.expect("current schema reader");
    assert_eq!(
        reader.snapshot("reader-device").expect("snapshot").revision,
        0
    );
    drop(reader);
    drop(writer);
    fs::remove_dir_all(directory).expect("remove reader fixture");
}

#[test]
fn current_configuration_remains_available_when_runtime_is_unavailable() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest as _, Sha256};

    let root = std::env::temp_dir().join(format!(
        "wwc-configuration-runtime-boundary-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    for state in ["missing", "corrupt", "unsafe-permissions"] {
        let directory = root.join(state);
        let store = DeviceProviderStore::open(&directory).unwrap();
        let snapshot = store.snapshot("configuration-device").unwrap();
        drop(store);
        let source_id = format!(
            "{:x}",
            Sha256::digest(STANDARD.decode(snapshot.encryption_public_key).unwrap())
        );
        let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
        database
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS provider_runtime_cutover(singleton INTEGER PRIMARY KEY CHECK(singleton=1),source_id TEXT NOT NULL); PRAGMA user_version=17;",
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO provider_runtime_cutover VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET source_id=excluded.source_id",
                [source_id],
            )
            .unwrap();
        drop(database);
        let runtime = directory.join("runtime");
        if runtime.exists() {
            fs::remove_dir_all(&runtime).unwrap();
        }
        let runtime_database = runtime.join("provider-runtime.sqlite3");
        if state != "missing" {
            fs::create_dir(&runtime).unwrap();
            fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(&runtime_database, b"unavailable-runtime-fixture").unwrap();
            fs::set_permissions(
                &runtime_database,
                fs::Permissions::from_mode(if state == "corrupt" { 0o600 } else { 0o644 }),
            )
            .unwrap();
        }
        let mut store = DeviceProviderStore::open(&directory).unwrap_or_else(|error| {
            panic!("configuration open must ignore {state} runtime: {error}")
        });
        let snapshot = store.snapshot("configuration-device").unwrap();
        let envelope = browser_crypto_provider_envelope(&snapshot, "anthropic_messages", None);
        let receipt = store.apply(&snapshot.client_node_id, &envelope).unwrap();
        assert_eq!(receipt.outcome, DeviceProviderOutcome::Saved, "{state}");
        assert_eq!(receipt.revision, 1, "{state}");
        assert_saved_provider_configuration(&store, "anthropic_messages", None);
        assert_eq!(
            store.apply(&snapshot.client_node_id, &envelope).unwrap(),
            receipt,
            "{state} runtime must preserve the configuration receipt"
        );
        drop(store);
        if state == "missing" {
            assert!(!runtime.exists());
        } else {
            assert_eq!(
                fs::read(runtime_database).unwrap(),
                b"unavailable-runtime-fixture"
            );
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_device_key_is_rejected_without_changing_the_database() {
    let directory = std::env::temp_dir().join(format!(
        "wwc-device-provider-bad-key-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    drop(DeviceProviderStore::open(&directory).expect("initialize schema"));
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    database
        .execute_batch("UPDATE identity SET private_key=zeroblob(32);")
        .unwrap();
    assert!(DeviceProviderStore::open(&directory).is_err());
    let key: Vec<u8> = database
        .query_row("SELECT private_key FROM identity", [], |row| row.get(0))
        .unwrap();
    assert_eq!(key, vec![0; 32]);
    database.pragma_update(None, "user_version", 99).unwrap();
    assert!(DeviceProviderStore::open(&directory).is_err());
    assert_eq!(
        database
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        99
    );
    drop(database);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn browser_crypto_device_storage_replay_and_tamper() {
    browser_crypto_device_storage_for_protocol("anthropic_messages", None);
}

#[test]
fn responses_api_key_configuration_survives_encrypted_save_and_restart() {
    browser_crypto_device_storage_for_protocol("openai_responses", None);
}

#[test]
fn text_responses_configuration_survives_encrypted_save_and_restart() {
    browser_crypto_device_storage_for_protocol("openai_responses", Some("text"));
}

#[test]
fn responses_structured_output_survives_encrypted_save_restart_replay_and_tamper() {
    for mode in ["json_schema", "json_object"] {
        browser_crypto_device_storage_for_protocol("openai_responses", Some(mode));
    }
}

#[test]
fn encrypted_invalid_structured_output_is_invalid_request_without_config_writes() {
    for (index, (protocol, mode)) in [
        ("anthropic_messages", "\"json_schema\""),
        ("anthropic_messages", "\"json_object\""),
        ("anthropic_messages", "null"),
        ("openai_responses", "null"),
        ("anthropic_messages", "\"text\""),
        ("openai_responses", "\"unsupported\""),
        ("openai_responses", "\"Text\""),
        ("openai_responses", "false"),
        ("openai_responses", "1"),
        ("openai_responses", "{}"),
    ]
    .into_iter()
    .enumerate()
    {
        let directory = std::env::temp_dir().join(format!(
            "wwc-provider-invalid-format-{}-{index}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let mut store = DeviceProviderStore::open(&directory).unwrap();
        let snapshot = store.snapshot("structured-format-device").unwrap();
        let envelope = browser_crypto_provider_envelope_with_mode_json(&snapshot, protocol, mode);
        let receipt = store.apply(&snapshot.client_node_id, &envelope).unwrap();
        assert_eq!(
            receipt.outcome,
            DeviceProviderOutcome::InvalidRequest,
            "{protocol}/{mode}"
        );
        assert_eq!(receipt.revision, 0);
        assert_eq!(
            store.apply(&snapshot.client_node_id, &envelope).unwrap(),
            receipt
        );
        let after = store.snapshot(&snapshot.client_node_id).unwrap();
        assert_eq!(after.revision, 0);
        assert!(after.providers.is_empty());
        drop(store);
        fs::remove_dir_all(directory).unwrap();
    }
}

fn browser_crypto_device_storage_for_protocol(protocol: &str, structured_output: Option<&str>) {
    let directory = std::env::temp_dir().join(format!(
        "wwc-device-provider-{}-{protocol}-{}",
        std::process::id(),
        structured_output.unwrap_or("default")
    ));
    let _ = fs::remove_dir_all(&directory);
    let mut store = DeviceProviderStore::open(&directory).expect("private store");
    let snapshot = store
        .snapshot("cnd_00000000000000000000000001")
        .expect("public state");
    let envelope = browser_crypto_provider_envelope(&snapshot, protocol, structured_output);
    let receipt = store
        .apply(&snapshot.client_node_id, &envelope)
        .expect("decrypt and save");
    assert_eq!(receipt.outcome, DeviceProviderOutcome::Saved);
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        store
            .apply(&snapshot.client_node_id, &envelope)
            .expect("idempotent replay"),
        receipt
    );
    assert!(
        !serde_json::to_string(
            &store
                .snapshot(&snapshot.client_node_id)
                .expect("redacted projection")
        )
        .expect("public JSON")
        .contains("device-only-test-secret")
    );
    assert_saved_provider_configuration(&store, protocol, structured_output);
    assert_private_headers(&store, &directory, &snapshot.client_node_id);
    drop(store);
    let mut store = DeviceProviderStore::open(&directory).expect("restart");
    assert_saved_provider_configuration(&store, protocol, structured_output);
    assert_eq!(
        store
            .apply(&snapshot.client_node_id, &envelope)
            .expect("restart replay"),
        receipt
    );
    let mut tampered = envelope.clone();
    "provider_changed_identity".clone_into(&mut tampered.request_id);
    tampered.expected_revision = 1;
    assert_eq!(
        store
            .apply(&snapshot.client_node_id, &tampered)
            .expect("bounded rejection")
            .outcome,
        DeviceProviderOutcome::InvalidRequest
    );
    let mut changed = envelope.clone();
    changed.ciphertext.push('A');
    assert!(store.apply(&snapshot.client_node_id, &changed).is_err());
    assert!(store.apply("another-device", &envelope).is_err());
    assert_eq!(
        fs::metadata(directory.join("providers.sqlite3"))
            .expect("database mode")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(store);
    fs::set_permissions(
        directory.join("providers.sqlite3"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("make unsafe file");
    assert!(DeviceProviderStore::open(&directory).is_err());
    fs::remove_dir_all(directory).expect("remove test data");
}

fn assert_saved_provider_configuration(
    store: &DeviceProviderStore,
    protocol: &str,
    structured_output: Option<&str>,
) {
    let (config, secret) = store
        .resolve("test-provider")
        .expect("API key configuration");
    assert_eq!(serde_json::to_value(&config.protocol).unwrap(), protocol);
    assert_eq!(
        serde_json::to_value(&config)
            .unwrap()
            .get("responsesStructuredOutput")
            .and_then(serde_json::Value::as_str),
        structured_output
    );
    assert_eq!(secret.expose(), b"device-only-test-secret");
}

fn browser_crypto_provider_envelope(
    snapshot: &DeviceProviderSnapshot,
    protocol: &str,
    structured_output: Option<&str>,
) -> DeviceConfigurationEnvelope {
    browser_crypto_provider_envelope_with_mode_json(
        snapshot,
        protocol,
        &structured_output
            .map(|mode| serde_json::to_string(mode).unwrap())
            .unwrap_or_default(),
    )
}

fn browser_crypto_provider_envelope_with_mode_json(
    snapshot: &DeviceProviderSnapshot,
    protocol: &str,
    structured_output_json: &str,
) -> DeviceConfigurationEnvelope {
    let module = std::path::Path::new(
        &std::env::var("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"),
    )
    .join("../../apps/client/src/device-provider-encryption.ts");
    let mut child = Command::new("node").args(["--input-type=module", "-e", r"
        import { pathToFileURL } from 'node:url';
        import { readFileSync } from 'node:fs';
        const { encryptDeviceProvider } = await import(pathToFileURL(process.argv[1]));
        const snapshot = JSON.parse(readFileSync(0, 'utf8'));
        const config = {providerId:'test-provider',displayName:'Local Provider',endpoint:'https://example.com/v1/responses',protocol:process.argv[2],modelIds:['test-model'],enabled:true};
        if (process.argv[3] !== '') config.responsesStructuredOutput = JSON.parse(process.argv[3]);
        process.stdout.write(JSON.stringify(await encryptDeviceProvider(snapshot, 'provider_test_save', {
          operation:'save',config,apiKey:'device-only-test-secret',customHeaders:{'x-opencode-session':'private-session-value'}
        })));
    "]).arg(module).arg(protocol).arg(structured_output_json).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("WebCrypto runner");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&serde_json::to_vec(snapshot).expect("snapshot JSON"))
        .expect("write snapshot");
    let result = child.wait_with_output().expect("WebCrypto output");
    assert!(result.status.success());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("device-only-test-secret"));
    serde_json::from_slice(&result.stdout).expect("encrypted contract")
}

#[test]
fn configured_jev_request_cannot_silently_run_as_baseline() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    use winwincode_execution_port::generated::{EncodedPayload, ModelOpenMessage};
    for field in ["winwincodeJevContext", "winwincodeJevTask"] {
        let directory =
            std::env::temp_dir().join(format!("wwc-jev-ingress-{}-{field}", std::process::id()));
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
        let payload = serde_json::to_vec(&serde_json::json!({field:{}, "provider":"unconfigured", "request":{"model":"unconfigured"}})).unwrap();
        open.request = EncodedPayload {
            content_type: "application/json".to_owned(),
            data_base64: STANDARD.encode(&payload),
            payload_digest: winwincode_domain::Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(&payload)
            )),
        };
        let store = ProviderRuntimeStore::prepare(&directory).unwrap();
        let db = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
        let config = serde_json::json!({"providerId":"unconfigured", "displayName":"fixture", "endpoint":"https://example.invalid/v1/responses", "protocol":"canonical", "modelIds":["unconfigured"], "enabled":true});
        db.execute(
            "INSERT INTO providers VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "unconfigured",
                config.to_string(),
                b"fixture-only-key".as_slice()
            ],
        )
        .unwrap();
        drop(db);
        let result = store.execute_model(&open).unwrap();
        assert_eq!(result.len(), 1);
        let error = result[0].error.as_ref().unwrap();
        assert_eq!(
            error.code,
            winwincode_execution_port::generated::ExecutionPortErrorCode::DeviceJevUnavailable
        );
        assert_eq!(error.message, "DEVICE_JEV_UNAVAILABLE");
        drop(store);
        let reopened = ProviderRuntimeStore::prepare(&directory).unwrap();
        assert_eq!(reopened.execute_model(&open).unwrap(), result);
        let db =
            rusqlite::Connection::open(directory.join("runtime/provider-runtime.sqlite3")).unwrap();
        let (saved, digest, prepared): (String, String, Option<Vec<u8>>) = db
            .query_row(
                "SELECT request_open, digest, prepared_payload FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<ModelOpenMessage>(&saved).unwrap(),
            open
        );
        assert_eq!(digest, format!("{:x}", Sha256::digest(saved.as_bytes())));
        assert!(
            prepared.is_none(),
            "a rejected JEV request never reaches the adapter"
        );
        drop(db);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one durable exchange verifies exact replay, cancellation and public error/content projection together"
)]
fn model_failure_replay_cancellation_and_public_projection_stay_local() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    use winwincode_domain::Sha256Digest;
    use winwincode_execution_port::generated::{EncodedPayload, ModelOpenMessage};
    use winwincode_provider::public_model_chunk;
    let directory = std::env::temp_dir().join(format!("wwc-device-model-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("fixture");
    let mut open: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["kind"] == "model.open")
            .expect("model open")
            .clone(),
    )
    .expect("open contract");
    let store = ProviderRuntimeStore::prepare(&directory).expect("store");
    let chunks = store
        .execute_model(&open)
        .expect("no configured Provider fails locally");
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_final && chunks[0].error.is_some());
    drop(store);
    let store = ProviderRuntimeStore::prepare(&directory).expect("restart");
    assert_eq!(store.execute_model(&open).expect("exact replay"), chunks);
    open.request_id.0.push('X');
    assert!(store.execute_model(&open).is_err());
    let mut failure = chunks[0].clone();
    failure.error.as_mut().expect("error").message = "DEVICE_PROVIDER_RATE_LIMITED".into();
    let public = public_model_chunk(&failure).expect("safe public failure");
    let bytes = STANDARD
        .decode(public.payload.expect("visible failure").data_base64)
        .expect("bytes");
    assert!(
        String::from_utf8(bytes)
            .expect("failure text")
            .contains("额度限制")
    );
    failure.error.as_mut().expect("error").message = "private diagnostic device-only-secret".into();
    let public = public_model_chunk(&failure).expect("unknown error redaction");
    let bytes = STANDARD
        .decode(public.payload.expect("bounded failure").data_base64)
        .expect("bytes");
    assert!(
        !String::from_utf8(bytes)
            .expect("failure text")
            .contains("device-only-secret")
    );
    let mut chunk = chunks[0].clone();
    chunk.error = None;
    chunk.is_final = false;
    for (kind, retained) in [
        ("reasoning_delta", false),
        ("function_call", false),
        ("output_text_delta", true),
    ] {
        let bytes = serde_json::to_vec(
            &serde_json::json!({"type":kind,"delta":"content","private":"device-only"}),
        )
        .expect("payload");
        chunk.payload = Some(EncodedPayload {
            content_type: "application/json".into(),
            data_base64: STANDARD.encode(&bytes),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes))),
        });
        let public = public_model_chunk(&chunk).expect("public projection");
        assert_eq!(public.payload.is_some(), retained);
        assert!(public.error.is_none());
        if let Some(payload) = public.payload {
            assert!(
                !String::from_utf8(STANDARD.decode(payload.data_base64).expect("public bytes"))
                    .expect("public text")
                    .contains("device-only")
            );
        }
    }
    let metadata=serde_json::to_vec(&serde_json::json!({"providerErrorKind":"rate_limited","status":429,"providerRetryAfterMillis":2000,"providerRequestId":"device-only-request","diagnostic":{"stage":"response","eventType":"message_delta","fieldPath":"$.private"}})).expect("private error metadata");
    failure.payload = Some(EncodedPayload {
        content_type: "application/json".into(),
        data_base64: STANDARD.encode(&metadata),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&metadata))),
    });
    let public = public_model_chunk(&failure).expect("error metadata stays private");
    let public_text = String::from_utf8(
        STANDARD
            .decode(
                public
                    .payload
                    .expect("generic failure remains visible")
                    .data_base64,
            )
            .expect("public bytes"),
    )
    .expect("public text");
    for private in [
        "device-only-request",
        "providerRetryAfterMillis",
        "message_delta",
        "$.private",
        "device-only-secret",
    ] {
        assert!(!public_text.contains(private));
    }
    assert!(public.error.is_none());
    let mut tampered = failure.clone();
    tampered
        .payload
        .as_mut()
        .expect("metadata")
        .payload_digest
        .0 = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into();
    assert!(
        public_model_chunk(&tampered).is_err(),
        "error metadata must retain the same digest validation as content frames"
    );
    tampered = failure.clone();
    tampered.payload.as_mut().expect("metadata").data_base64 = "not valid base64".into();
    assert!(
        public_model_chunk(&tampered).is_err(),
        "malformed error payloads cannot be silently projected"
    );
    store
        .cancel_model(&open.model_exchange_id.0)
        .expect("cancel");
    assert!(
        store.execute_model(&open).is_err(),
        "cancellation cannot admit conflicting request bytes"
    );
    open.request_id.0.pop();
    assert!(
        store
            .execute_model(&open)
            .expect("cancel fences opens")
            .is_empty()
    );
    assert!(
        store
            .replay_model(&open.model_exchange_id.0, 1)
            .expect("cancel fences replay")
            .is_empty()
    );
    drop(store);
    fs::remove_dir_all(directory).expect("cleanup");
}

fn assert_private_headers(store: &DeviceProviderStore, directory: &std::path::Path, node: &str) {
    let public = serde_json::to_string(&store.snapshot(node).expect("snapshot")).expect("json");
    assert!(!public.contains("private-session-value"));
    assert!(!public.contains("x-opencode-session"));
    let db = rusqlite::Connection::open(directory.join("providers.sqlite3")).expect("private db");
    let headers: String = db
        .query_row(
            "SELECT headers FROM provider_headers WHERE provider_id='test-provider'",
            [],
            |row| row.get(0),
        )
        .expect("private headers");
    assert!(headers.contains("private-session-value"));
    drop(db);
}
