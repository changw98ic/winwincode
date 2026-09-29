// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};
use winwincode_api::generated::{DeviceConfigurationEnvelope, DeviceProviderOutcome};
use winwincode_provider::DeviceProviderStore;

#[test]
fn browser_crypto_device_storage_replay_and_tamper() {
    let directory =
        std::env::temp_dir().join(format!("wwc-device-provider-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    let mut store = DeviceProviderStore::open(&directory).expect("private store");
    let snapshot = store
        .snapshot("cnd_00000000000000000000000001")
        .expect("public state");
    let module = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/client/src/device-provider-encryption.ts");
    let mut child = Command::new("node").args(["--input-type=module", "-e", r"
        import { pathToFileURL } from 'node:url';
        import { readFileSync } from 'node:fs';
        const { encryptDeviceProvider } = await import(pathToFileURL(process.argv[1]));
        const snapshot = JSON.parse(readFileSync(0, 'utf8'));
        process.stdout.write(JSON.stringify(await encryptDeviceProvider(snapshot, 'provider_test_save', {
          operation:'save', config:{providerId:'test-provider',displayName:'Local Provider',endpoint:'https://example.com/v1/messages',protocol:'anthropic_messages',modelIds:['test-model'],enabled:true},apiKey:'device-only-test-secret',customHeaders:{'x-opencode-session':'private-session-value'}
        })));
    "]).arg(module).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("WebCrypto runner");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&serde_json::to_vec(&snapshot).expect("snapshot JSON"))
        .expect("write snapshot");
    let result = child.wait_with_output().expect("WebCrypto output");
    assert!(result.status.success());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("device-only-test-secret"));
    let envelope: DeviceConfigurationEnvelope =
        serde_json::from_slice(&result.stdout).expect("encrypted contract");
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
    assert_eq!(
        store
            .resolve("test-provider")
            .expect("device secret")
            .1
            .expose(),
        b"device-only-test-secret"
    );
    assert_private_headers(&store, &directory, &snapshot.client_node_id);
    drop(store);
    let mut store = DeviceProviderStore::open(&directory).expect("restart");
    assert_eq!(
        store
            .apply(&snapshot.client_node_id, &envelope)
            .expect("restart replay"),
        receipt
    );
    let mut tampered = envelope.clone();
    tampered.request_id = "provider_changed_identity".to_owned();
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
        let store = DeviceProviderStore::open(&directory).unwrap();
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
        assert!(
            result[0]
                .error
                .as_ref()
                .unwrap()
                .message
                .starts_with("DEVICE_JEV_UNAVAILABLE:")
        );
        drop(store);
        let reopened = DeviceProviderStore::open(&directory).unwrap();
        assert_eq!(reopened.execute_model(&open).unwrap(), result);
        let db = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
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
    let store = DeviceProviderStore::open(&directory).expect("store");
    let chunks = store
        .execute_model(&open)
        .expect("no configured Provider fails locally");
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_final && chunks[0].error.is_some());
    drop(store);
    let store = DeviceProviderStore::open(&directory).expect("restart");
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
    store
        .cancel_model(&open.model_exchange_id.0)
        .expect("cancel");
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

#[test]
fn old_database_upgrade_preserves_identity_and_provider_state() {
    for version in [2, 3, 4, 5, 6] {
        let directory = std::env::temp_dir().join(format!(
            "wwc-provider-migrate-{}-{version}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let store = DeviceProviderStore::open(&directory).expect("store");
        let before = store.snapshot("device-1").expect("snapshot");
        drop(store);
        let db = rusqlite::Connection::open(directory.join("providers.sqlite3")).expect("db");
        db.execute_batch("DROP TABLE jev_judge_exchanges;")
            .expect("old schema");
        db.execute_batch("ALTER TABLE jev_context_exchanges DROP COLUMN request_json;")
            .expect("old schema");
        if version < 6 {
            db.execute_batch("ALTER TABLE exchanges DROP COLUMN prepared_payload; ALTER TABLE exchanges DROP COLUMN request_open;").expect("old schema");
        }
        if version < 5 {
            db.execute_batch("DROP TABLE jev_settings;")
                .expect("old schema");
        }
        if version < 4 {
            db.execute_batch("DROP TABLE jev_context_exchanges;")
                .expect("old schema");
        }
        if version == 2 {
            db.execute_batch("DROP TABLE provider_headers;")
                .expect("v2 schema");
        }
        if version >= 4 {
            db.execute("INSERT INTO jev_context_exchanges VALUES ('retained-operation', 'retained-digest', 'retained-result')", []).unwrap();
        }
        db.execute("INSERT INTO exchanges (exchange_id, digest, chunks) VALUES ('old-exchange', 'old-digest', '[]')", []).unwrap();
        db.pragma_update(None, "user_version", version)
            .expect("old version");
        drop(db);
        let store = DeviceProviderStore::open(&directory).expect("upgrade");
        assert_eq!(store.snapshot("device-1").expect("after"), before);
        drop(store);
        let store = DeviceProviderStore::open(&directory).expect("reopen");
        assert_eq!(store.snapshot("device-1").expect("reopened"), before);
        let db = rusqlite::Connection::open(directory.join("providers.sqlite3")).expect("db");
        let upgraded: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(upgraded, 8);
        let old: (String, String, Option<String>, Option<Vec<u8>>) = db.query_row(
            "SELECT digest, chunks, request_open, prepared_payload FROM exchanges WHERE exchange_id='old-exchange'", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap();
        assert_eq!(old, ("old-digest".into(), "[]".into(), None, None));
        let entries: i64 = db
            .query_row("SELECT count(*) FROM jev_context_exchanges", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(entries, i64::from(version >= 4));
        if version >= 4 {
            let retained: (String, String, Option<String>) = db.query_row(
                "SELECT digest, result, request_json FROM jev_context_exchanges WHERE operation_id='retained-operation'", [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            assert_eq!(
                retained,
                ("retained-digest".into(), "retained-result".into(), None)
            );
        }
        drop(db);
        drop(store);
        fs::remove_dir_all(directory).expect("cleanup");
    }
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
