// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{DeviceProviderStore, JevContextRequest, StoredJevContext};
use std::{
    io::Write as _,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{SystemTime, UNIX_EPOCH},
};
use winwincode_execution_port::jev_decision::JevPolicy;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "wwc-jev-diagnostic-{label}-{}-{}",
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

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future)
}

fn options() -> JevExecutionOptions {
    JevExecutionOptions {
        device: JevDevice::Cpu,
        dtype: JevDtype::Float32,
    }
}

fn input() -> JevHypothesis {
    JevHypothesis {
        premise: "SYNTHETIC_PRIVATE_PREMISE".into(),
        hypothesis: "fixture hypothesis".into(),
    }
}

fn context_input() -> JevContextRequest {
    JevContextRequest {
        task: "SYNTHETIC_PRIVATE_TASK".into(),
        candidate: "SYNTHETIC_PRIVATE_CANDIDATE".into(),
        protected: false,
        archive_eligible: false,
    }
}

fn policy() -> JevPolicy {
    JevPolicy {
        version: "fixture-v1".into(),
        minimum_confidence: 0.6,
        pin_threshold: 0.8,
        keep_threshold: 0.8,
        compact_threshold: 0.8,
        drop_threshold: 0.8,
        task_memory_threshold: 0.5,
        project_memory_threshold: 0.7,
        long_term_memory_threshold: 0.9,
    }
}

fn remote_config() -> OpenJevRemoteConfig {
    OpenJevRemoteConfig::try_new(OpenJevRemoteConfigRequest {
        provider_id: "diagnostic-jev".into(),
        endpoint: "https://nli.invalid/score".into(),
        model_id: "fixture-model".into(),
        max_batch_size: 4,
        devices: vec![JevDevice::Cpu],
        dtypes: vec![JevDtype::Float32],
        timeout: Duration::from_secs(2),
        api_key: None,
    })
    .unwrap()
}

fn runtime(transport: Arc<dyn JevRemoteTransport>) -> JevRuntime {
    JevRuntime::new(
        vec![Arc::new(OpenJevRemoteProvider::new(
            remote_config(),
            transport,
        ))],
        JevRuntimeConfig {
            timeout: Duration::from_secs(3),
            retries: 0,
        },
    )
}

fn response_transport(body: Vec<u8>) -> (HttpsJevRemoteTransport, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/score", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "fixture was not called"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture accept failed: {:?}", error.kind()),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
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
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
        stream.flush().unwrap();
    });
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into();
    (
        HttpsJevRemoteTransport {
            agent,
            endpoint,
            api_key: Some("SYNTHETIC_PRIVATE_CREDENTIAL".into()),
            system_one: false,
            timeout: Duration::from_secs(3),
            cancellation: None,
        },
        server,
    )
}

#[test]
fn legacy_jev_failure_json_remains_readable() {
    let failure: JevAttemptFailure = serde_json::from_value(serde_json::json!({
        "providerId": "fixture",
        "kind": "unavailable",
        "latency": { "secs": 0, "nanos": 0 }
    }))
    .unwrap();
    let encoded = serde_json::to_value(failure).unwrap();
    assert!(encoded.get("network").is_none());
    assert!(encoded.get("attempt").is_none());
    assert_ne!(
        encoded.get("connectionWait"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn malformed_json_diagnostic_is_persisted_and_replayed_without_another_call() {
    let directory = TestDirectory::new("malformed-json");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let (transport, server) = response_transport(b"{\n\"SYNTHETIC_PRIVATE_RESPONSE\":}".to_vec());
    let runtime = runtime(Arc::new(transport));
    let digest = format!("sha256:{}", "1".repeat(64));
    let result = block_on(store.evaluate_context_once(
        "jev:diagnostic-fixture:0",
        &digest,
        &runtime,
        context_input(),
        &policy(),
        options(),
    ))
    .unwrap();
    server.join().unwrap();
    let StoredJevContext::Completed { run, replayed } = result else {
        panic!("first scoring operation must retain a completed failure receipt");
    };
    assert!(!replayed);
    assert!(run.value.is_none());
    assert_eq!(run.failures.len(), 1);
    let encoded = serde_json::to_value(&run.failures[0]).unwrap();
    assert_eq!(encoded["attempt"], 1);
    assert_eq!(encoded["connectionWait"], false);
    assert_eq!(encoded["network"]["kind"], "protocol_invalid");
    assert_eq!(encoded["network"]["phase"], "decode");
    assert_eq!(encoded["network"]["httpStatus"], 200);
    assert_eq!(encoded["network"]["diagnostic"]["code"], "json_syntax");
    assert_eq!(encoded["network"]["diagnostic"]["line"], 2);
    assert!(encoded["network"]["diagnostic"]["column"].as_u64().unwrap() > 0);
    assert!(!encoded.to_string().contains("SYNTHETIC_PRIVATE"));
    let journal: (String, String) = store.connection.query_row(
        "SELECT role,failure_json FROM jev_attempt_diagnostics WHERE operation_id=?1 AND sequence=1",
        ["jev:diagnostic-fixture:0"],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(journal.0, "context");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&journal.1).unwrap(),
        encoded
    );
    drop(store);
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let replay = block_on(store.evaluate_context_once(
        "jev:diagnostic-fixture:0",
        &digest,
        &runtime,
        context_input(),
        &policy(),
        options(),
    ))
    .unwrap();
    assert_eq!(
        replay,
        StoredJevContext::Completed {
            run,
            replayed: true
        }
    );
    let count: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM jev_attempt_diagnostics", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[derive(Debug)]
struct DisconnectedTransport(Arc<AtomicBool>);

impl JevRemoteTransport for DisconnectedTransport {
    fn score(
        &self,
        _model_id: String,
        _items: Vec<JevHypothesis>,
        _options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>> {
        let allowed = Arc::clone(&self.0);
        Box::pin(async move {
            allowed.store(false, Ordering::SeqCst);
            let error = ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "SYNTHETIC_PRIVATE_IO_MESSAGE",
            ));
            Err(JevProviderError::from_network(
                winwincode_network::classify_ureq(
                    &error,
                    true,
                    winwincode_network::Phase::ResponseHeaders,
                ),
            ))
        })
    }
}

#[test]
fn connection_wait_is_retained_before_authority_ends() {
    let directory = TestDirectory::new("connection-wait");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let allowed = Arc::new(AtomicBool::new(true));
    let runtime = runtime(Arc::new(DisconnectedTransport(Arc::clone(&allowed))))
        .with_attempt_journal(
            store.connection.path().unwrap(),
            "jev:connection-wait:0",
            "context",
        );
    let run = block_on(
        runtime.evaluate_authorized(input(), options(), &|| allowed.load(Ordering::SeqCst)),
    );
    assert!(run.value.is_none());
    assert_eq!(
        run.failures.len(),
        2,
        "connection failure and later authority stop are separate facts"
    );
    let encoded = serde_json::to_value(&run.failures[0]).unwrap();
    assert_eq!(encoded["attempt"], 1);
    assert_eq!(encoded["connectionWait"], true);
    assert_eq!(encoded["network"]["kind"], "connection_unavailable");
    assert_eq!(encoded["network"]["acceptance"], "not_sent");
    assert_eq!(
        encoded["network"]["diagnostic"]["ioKind"],
        "connection_refused"
    );
    let persisted: String = store
        .connection
        .query_row(
            "SELECT failure_json FROM jev_attempt_diagnostics WHERE sequence=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&persisted).unwrap(),
        encoded
    );
    assert!(!persisted.contains("SYNTHETIC_PRIVATE"));
    let stopped = serde_json::to_value(&run.failures[1]).unwrap();
    assert!(stopped.get("attempt").is_none());
    assert_eq!(stopped["network"]["kind"], "authority_expired");
    assert_eq!(stopped["network"]["diagnostic"]["code"], "authority_ended");
    let persisted_stop: String = store
        .connection
        .query_row(
            "SELECT failure_json FROM jev_attempt_diagnostics WHERE sequence=2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&persisted_stop).unwrap(),
        stopped
    );
}

#[test]
fn invalid_score_invariant_is_distinct_from_json_syntax() {
    let error = parse_jev_scores(&serde_json::json!({
        "entailment": 0.8, "contradiction": 0.8, "neutral": 0.1,
        "SYNTHETIC_PRIVATE_FIELD": "SYNTHETIC_PRIVATE_VALUE"
    }))
    .unwrap_err();
    let encoded = serde_json::to_value(error.network_failure()).unwrap();
    assert_eq!(encoded["kind"], "protocol_invalid");
    assert_eq!(encoded["phase"], "decode");
    assert_eq!(encoded["diagnostic"]["code"], "response_invariant");
    assert!(!encoded.to_string().contains("SYNTHETIC_PRIVATE"));
}

#[derive(Debug)]
struct CountedFailureTransport(Arc<AtomicUsize>);

impl JevRemoteTransport for CountedFailureTransport {
    fn score(
        &self,
        _model_id: String,
        _items: Vec<JevHypothesis>,
        _options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(JevProviderError::from_network(
                winwincode_network::NetworkFailure::http(503, None),
            ))
        })
    }
}

#[test]
fn failed_attempt_journal_prevents_a_second_inference_call() {
    let directory = TestDirectory::new("journal-failure");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    store
        .connection
        .execute("DROP TABLE jev_attempt_diagnostics", [])
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = JevRuntime::new(
        vec![Arc::new(OpenJevRemoteProvider::new(
            remote_config(),
            Arc::new(CountedFailureTransport(Arc::clone(&calls))),
        ))],
        JevRuntimeConfig {
            timeout: Duration::from_secs(3),
            retries: 3,
        },
    )
    .with_attempt_journal(
        store.connection.path().unwrap(),
        "jev:journal-failure:0",
        "context",
    );
    let run = block_on(runtime.evaluate(input(), options()));
    assert!(run.value.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(run.failures.len(), 2);
    let encoded = serde_json::to_value(run.failures.last().unwrap()).unwrap();
    assert_eq!(encoded["network"]["kind"], "storage_unavailable");
    assert_eq!(encoded["network"]["diagnostic"]["code"], "storage");
}

#[test]
fn schema_diagnostics_identify_safe_fields_without_retaining_invalid_values() {
    let valid = serde_json::json!({
        "model": "fixture-model",
        "answers": { "item_0": { "type": "choice", "choice": "entailment", "confidence": 0.8,
            "probabilities": { "entailment": 0.8, "contradiction": 0.1, "neutral": 0.1 } } },
        "usage": { "input_tokens": 10, "output_tokens": 2 }
    });
    for (pointer, field) in [
        ("/model", "model"),
        ("/answers", "answers"),
        ("/answers/item_0/type", "answer_type"),
        ("/answers/item_0/probabilities", "probabilities"),
        ("/answers/item_0/confidence", "confidence"),
        ("/usage/input_tokens", "usage_input_tokens"),
        ("/usage/output_tokens", "usage_output_tokens"),
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(pointer).unwrap() = if pointer == "/model" {
            serde_json::json!({ "SYNTHETIC_PRIVATE_FIELD": "SYNTHETIC_PRIVATE_VALUE" })
        } else {
            serde_json::json!("SYNTHETIC_PRIVATE_VALUE")
        };
        let error = parse_system_one_response(&invalid, 1).unwrap_err();
        let encoded = serde_json::to_value(error.network_failure()).unwrap();
        assert_eq!(encoded["diagnostic"]["code"], "json_schema");
        assert_eq!(encoded["diagnostic"]["field"], field);
        assert!(!encoded.to_string().contains("SYNTHETIC_PRIVATE"));
    }
}

#[test]
fn stopping_before_send_is_durable_and_does_not_create_an_inference_attempt() {
    for cancelled in [false, true] {
        let directory = TestDirectory::new("stop-before-send");
        let store = DeviceProviderStore::open(&directory.0).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let cancellation = Arc::new(winwincode_network::transport::ExchangeCancellation::default());
        if cancelled {
            cancellation.cancel();
        }
        let runtime = runtime(Arc::new(CountedFailureTransport(Arc::clone(&calls))))
            .with_cancellation(Some(cancellation))
            .with_attempt_journal(
                store.connection.path().unwrap(),
                "jev:stop-before-send:0",
                "context",
            );
        let run = block_on(runtime.evaluate_authorized(input(), options(), &|| cancelled));
        assert!(run.value.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(run.failures.len(), 1);
        let encoded = serde_json::to_value(&run.failures[0]).unwrap();
        assert!(encoded.get("attempt").is_none());
        assert_eq!(
            encoded["network"]["kind"],
            if cancelled {
                "cancelled"
            } else {
                "authority_expired"
            }
        );
        assert_eq!(encoded["network"]["acceptance"], "not_sent");
        assert_eq!(encoded["network"]["diagnostic"]["code"], "authority_ended");
        let persisted: String = store
            .connection
            .query_row(
                "SELECT failure_json FROM jev_attempt_diagnostics",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&persisted).unwrap(),
            encoded
        );
        let paid_rows: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM model_open_attempts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(paid_rows, 0);
    }
}

#[derive(Debug)]
struct PendingTransport(Arc<AtomicUsize>);

impl JevRemoteTransport for PendingTransport {
    fn score(
        &self,
        _model_id: String,
        _items: Vec<JevHypothesis>,
        _options: JevExecutionOptions,
    ) -> BoxFuture<'static, Result<RemoteJevScoreBatch, JevProviderError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::future::pending())
    }
}

#[test]
fn cancelling_an_active_call_retains_unknown_acceptance_without_retry() {
    let directory = TestDirectory::new("stop-active-call");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let cancellation = Arc::new(winwincode_network::transport::ExchangeCancellation::default());
    let runtime = runtime(Arc::new(PendingTransport(Arc::clone(&calls))))
        .with_cancellation(Some(Arc::clone(&cancellation)))
        .with_attempt_journal(
            store.connection.path().unwrap(),
            "jev:stop-active-call:0",
            "judge",
        );
    let cancel_calls = Arc::clone(&calls);
    let canceller = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        while cancel_calls.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "fixture never entered its call");
            std::thread::sleep(Duration::from_millis(1));
        }
        cancellation.cancel();
    });
    let run = block_on(runtime.evaluate(input(), options()));
    canceller.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(run.value.is_none());
    assert_eq!(run.failures.len(), 1);
    let encoded = serde_json::to_value(&run.failures[0]).unwrap();
    assert!(encoded.get("attempt").is_none());
    assert_eq!(encoded["network"]["kind"], "cancelled");
    assert_eq!(encoded["network"]["acceptance"], "unknown");
    assert_eq!(encoded["network"]["diagnostic"]["code"], "authority_ended");
    let persisted: String = store
        .connection
        .query_row(
            "SELECT failure_json FROM jev_attempt_diagnostics",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&persisted).unwrap(),
        encoded
    );
}

#[test]
fn field_failure_after_http_response_retains_status_and_safe_field() {
    let directory = TestDirectory::new("http-field");
    let store = DeviceProviderStore::open(&directory.0).unwrap();
    let body = serde_json::to_vec(&serde_json::json!({
        "entailment": "SYNTHETIC_PRIVATE_RESPONSE", "contradiction": 0.1, "neutral": 0.1
    }))
    .unwrap();
    let (transport, server) = response_transport(body);
    let runtime = runtime(Arc::new(transport)).with_attempt_journal(
        store.connection.path().unwrap(),
        "jev:http-field:0",
        "judge",
    );
    let run = block_on(runtime.evaluate(input(), options()));
    server.join().unwrap();
    assert!(run.value.is_none());
    assert_eq!(run.failures.len(), 1);
    let encoded = serde_json::to_value(&run.failures[0]).unwrap();
    assert_eq!(encoded["network"]["httpStatus"], 200);
    assert_eq!(encoded["network"]["diagnostic"]["code"], "json_schema");
    assert_eq!(encoded["network"]["diagnostic"]["field"], "entailment");
    let persisted: String = store
        .connection
        .query_row(
            "SELECT failure_json FROM jev_attempt_diagnostics",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&persisted).unwrap(),
        encoded
    );
    assert!(!persisted.contains("SYNTHETIC_PRIVATE"));
}
