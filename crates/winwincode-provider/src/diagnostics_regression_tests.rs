// SPDX-License-Identifier: Apache-2.0

use crate::{
    DeviceProviderStore, HttpsSseProviderAdapter, HttpsSseProviderConfig, HttpsSseProviderLimits,
    HttpsSseProviderTimeouts,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::params;
use sha2::{Digest, Sha256};
use std::{
    io::{Read as _, Write as _},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use winwincode_execution_port::generated::ModelOpenMessage;

struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
                "wwc-diagnostic-{label}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )))
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open_message() -> ModelOpenMessage {
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
    let payload = serde_json::to_vec(&serde_json::json!({"provider":"diagnostic-fixture","request":{"model":"fixture-model","stream":true,"input":"SYNTHETIC_PRIVATE_INPUT"}})).unwrap();
    open.request.content_type = "application/json".into();
    open.request.data_base64 = STANDARD.encode(&payload);
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
    open
}

fn configure(store: &DeviceProviderStore, endpoint: &str) {
    let config = serde_json::json!({"providerId":"diagnostic-fixture","displayName":"fixture","endpoint":endpoint,"protocol":"canonical","modelIds":["fixture-model"],"enabled":true});
    store
        .connection
        .execute(
            "INSERT INTO providers VALUES (?1,?2,?3)",
            params![
                "diagnostic-fixture",
                config.to_string(),
                b"SYNTHETIC_PRIVATE_CREDENTIAL".as_slice()
            ],
        )
        .unwrap();
}

fn adapter(endpoint: &str, roots: Vec<Vec<u8>>) -> HttpsSseProviderAdapter {
    let config = HttpsSseProviderConfig::try_new(
        "diagnostic-fixture".into(),
        endpoint.into(),
        HttpsSseProviderTimeouts {
            connect: Duration::from_secs(2),
            idle: Duration::from_secs(2),
            total: Duration::from_secs(5),
        },
        HttpsSseProviderLimits {
            response_bytes: 4096,
            event_bytes: 4096,
            events: 32,
        },
    )
    .unwrap();
    let config = if roots.is_empty() {
        config
    } else {
        config.with_specific_tls_roots(roots).unwrap()
    };
    HttpsSseProviderAdapter::try_new(config).unwrap()
}

#[test]
fn version_11_migration_adds_diagnostics_without_changing_paid_attempt_rows() {
    let directory = TestDirectory::new("migration");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    store
        .connection
        .execute_batch("DROP TABLE IF EXISTS model_attempt_diagnostics; DROP TABLE IF EXISTS jev_attempt_diagnostics; PRAGMA user_version=11;")
        .unwrap();
    store.connection.execute("INSERT INTO exchanges (exchange_id,digest) VALUES ('migration-fixture','fixture-digest')", []).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO model_open_attempts VALUES ('migration-fixture',1,'failed',503,NULL)",
            [],
        )
        .unwrap();
    drop(store);
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 12);
    let old: (i64,String,i64) = store.connection.query_row("SELECT attempt,outcome,http_status FROM model_open_attempts WHERE exchange_id='migration-fixture'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(old, (1, "failed".into(), 503));
    let new_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM model_attempt_diagnostics",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        new_rows, 0,
        "migration cannot invent physical attempt evidence for old paid calls"
    );
}

fn failing_tls_provider() -> (String, Vec<u8>, std::thread::JoinHandle<usize>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = certified.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate.clone()], key.into())
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "https://localhost:{}/v1/responses",
        listener.local_addr().unwrap().port()
    );
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(70);
        let mut calls = 0;
        while calls < 4 && Instant::now() < deadline {
            let socket = match listener.accept() {
                Ok((socket, _)) => socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("local fixture accept failed: {:?}", error.kind()),
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config.clone())).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
                assert!(request.len() < 64 * 1024);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 26\r\nConnection: close\r\n\r\nSYNTHETIC_PRIVATE_RESPONSE").unwrap();
            stream.flush().unwrap();
            calls += 1;
        }
        calls
    });
    (endpoint, certificate.to_vec(), server)
}

#[test]
fn four_failed_model_calls_preserve_all_physical_diagnostics_and_paid_budget() {
    let directory = TestDirectory::new("four-attempts");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let open = open_message();
    let (endpoint, certificate, server) = failing_tls_provider();
    configure(&store, &endpoint);
    let started = Instant::now();
    let chunks = store
        .execute_model_using(
            &open,
            || started.elapsed() < Duration::from_secs(90),
            |_, _| Ok(adapter(&endpoint, vec![certificate.clone()])),
        )
        .unwrap();
    assert!(chunks.last().unwrap().is_final);
    assert_eq!(
        server.join().unwrap(),
        4,
        "one finite paid budget permits exactly four HTTP sends"
    );
    let mut statement = store.connection.prepare("SELECT sequence,policy_attempt,outcome,failure_json,stop_reason,finished_ms FROM model_attempt_diagnostics WHERE exchange_id=?1 ORDER BY sequence").unwrap();
    let rows = statement
        .query_map([&open.model_exchange_id.0], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 4);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.0, i64::try_from(index).unwrap() + 1);
        assert_eq!(row.1, i64::try_from(index).unwrap() + 1);
        assert_eq!(row.2, "failed");
        let failure: serde_json::Value = serde_json::from_str(&row.3).unwrap();
        assert_eq!(failure["httpStatus"], 503);
        assert_eq!(failure["kind"], "server_transient");
        assert_eq!(failure["diagnostic"]["code"], "http_status");
        assert!(!row.3.contains("SYNTHETIC_PRIVATE"));
        assert!(row.5.is_some());
    }
    assert_eq!(
        rows.last().unwrap().4.as_deref(),
        Some("retry_budget_exhausted")
    );
    let paid: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM model_open_attempts WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(paid, 4);
    drop(statement);
    let replay = store
        .execute_model_using(
            &open,
            || false,
            |_, _| panic!("stored failure must replay without another provider call"),
        )
        .unwrap();
    assert_eq!(replay, chunks);
}

#[test]
fn connection_wait_keeps_physical_evidence_without_consuming_a_paid_attempt() {
    let directory = TestDirectory::new("connection-wait");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("https://{}/v1/responses", listener.local_addr().unwrap());
    drop(listener);
    configure(&store, &endpoint);
    let open = open_message();
    let started = Instant::now();
    store
        .execute_model_using(
            &open,
            || started.elapsed() < Duration::from_secs(2),
            |_, _| Ok(adapter(&endpoint, vec![])),
        )
        .unwrap();
    let paid: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM model_open_attempts WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(paid, 0);
    let (sequence,policy_attempt,failure,stop): (i64,i64,String,String) = store.connection.query_row("SELECT sequence,policy_attempt,failure_json,stop_reason FROM model_attempt_diagnostics WHERE exchange_id=?1",[&open.model_exchange_id.0],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    assert_eq!((sequence, policy_attempt), (1, 1));
    let failure: serde_json::Value = serde_json::from_str(&failure).unwrap();
    assert_eq!(failure["kind"], "connection_unavailable");
    assert_eq!(failure["acceptance"], "not_sent");
    assert_eq!(failure["diagnostic"]["ioKind"], "connection_refused");
    assert_eq!(stop, "authority_ended");
}
