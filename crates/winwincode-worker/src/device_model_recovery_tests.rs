// SPDX-License-Identifier: Apache-2.0

use super::{TlsStream, WAIT, member_open, read_request, write_answer};
use crate::device_model::{DeviceModelSendOutcome, DeviceModels};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use sha2::{Digest as _, Sha256};
use std::{
    io::Write as _,
    net::TcpListener,
    path::Path,
    process::Command,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tokio_rustls::rustls::{
    ServerConfig, ServerConnection,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use winwincode_execution_port::generated::{
    ExecutionPortMessage, ModelChunkMessage, ModelOpenMessage,
};
use winwincode_provider::{DeviceModelAdmission, DeviceModelPermit, DeviceProviderStore};

const CHILD_DIRECTORY: &str = "WWC_WORKER_MODEL_RECOVERY_TEST_DIRECTORY";
const CHILD_ENDPOINT: &str = "WWC_WORKER_MODEL_RECOVERY_TEST_ENDPOINT";
const TEST_NAME: &str = "device_model::concurrency_tests::recovery_tests::worker_retry_releases_backoff_slot_and_reacquires_without_repeating_completed_tools";
const RESTART_TEST: &str = "device_model::concurrency_tests::recovery_tests::worker_restart_retry_reacquires_real_slots_before_another_http_attempt";
const CHILD_PHASE: &str = "WWC_WORKER_MODEL_RECOVERY_TEST_PHASE";

#[test]
fn worker_retry_releases_backoff_slot_and_reacquires_without_repeating_completed_tools() {
    if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
        run_worker_retry(
            Path::new(&directory),
            &std::env::var(CHILD_ENDPOINT).unwrap(),
        );
        return;
    }
    run_retry_fixture(TEST_NAME);
}

#[test]
fn worker_restart_retry_reacquires_real_slots_before_another_http_attempt() {
    if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
        if std::env::var(CHILD_PHASE).as_deref() == Ok("crash_after_retry") {
            crash_after_retained_retry(
                Path::new(&directory),
                &std::env::var(CHILD_ENDPOINT).unwrap(),
            );
        }
        run_reopened_retry(Path::new(&directory));
        return;
    }
    run_retry_fixture(RESTART_TEST);
}

fn run_retry_fixture(test_name: &str) {
    let directory = tempfile::tempdir().unwrap();
    let providers = directory.path().join("providers");
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = directory.path().join("root.der");
    std::fs::write(&certificate, cert.der()).unwrap();
    let config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "https://localhost:{}/qwen",
        listener.local_addr().unwrap().port()
    );
    let server_directory = providers.clone();
    let server = thread::spawn(move || serve_retry(&listener, &config, &server_directory));
    if test_name == RESTART_TEST {
        let first = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD_DIRECTORY, &providers)
            .env(CHILD_ENDPOINT, &endpoint)
            .env(CHILD_PHASE, "crash_after_retry")
            .env("WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE", &certificate)
            .env_remove("WWC_DEVICE_PROVIDER_HTTPS_PROXY")
            .output()
            .unwrap();
        assert!(
            first.status.success(),
            "first process: {}",
            String::from_utf8_lossy(&first.stderr)
        );
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_DIRECTORY, &providers)
        .env(CHILD_ENDPOINT, &endpoint)
        .env("WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE", certificate)
        .env_remove("WWC_DEVICE_PROVIDER_HTTPS_PROXY")
        .output()
        .unwrap();
    let server_result = server.join();
    if !output.status.success() || server_result.is_err() {
        let retained = directory.keep();
        panic!(
            "Worker recovery fixture retained at {} (server completed={}): {} {}",
            retained.display(),
            server_result.is_ok(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let requests = server_result.unwrap();
    assert_eq!(requests.len(), 2, "only two actual model attempts");
    assert_eq!(requests[0], requests[1], "retry preserves the model step");
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        1,
        "a completed tool result occurs once in both attempts' history",
    );
    assert!(messages.iter().any(|message| {
        message["role"] == "tool"
            && message["content"]
                .as_str()
                .unwrap()
                .ends_with("already-completed-tool-result")
    }));
}

fn serve_retry(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    directory: &Path,
) -> Vec<serde_json::Value> {
    let mut requests = Vec::new();
    for attempt in 0..2 {
        let started = Instant::now();
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && started.elapsed() < WAIT =>
                {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("expected actual Worker HTTPS attempt: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        let mut stream = TlsStream::new(ServerConnection::new(Arc::clone(config)).unwrap(), socket);
        let (provider, request) = read_request(&mut stream);
        assert_eq!(provider, "qwen");
        requests.push(request);
        if attempt == 0 {
            write!(stream, "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
            stream.flush().unwrap();
        } else {
            std::fs::write(directory.join("second-request-observed"), b"observed").unwrap();
            wait_until(|| directory.join("release-second-answer").exists());
            write_answer(&mut stream, "qwen");
        }
    }
    requests
}

fn run_worker_retry(directory: &Path, endpoint: &str) {
    let mut models = DeviceModels::open(directory).unwrap();
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    configure_retry_provider(&database, endpoint);
    let open = completed_tool_history_open();
    let store = DeviceProviderStore::open(directory).unwrap();
    let held = (0..2).map(|_| acquire(&store, &open)).collect::<Vec<_>>();
    assert_eq!(
        models
            .send(
                &ExecutionPortMessage::ModelOpenMessage(open.clone()),
                Some(Instant::now() + WAIT),
                None
            )
            .unwrap(),
        DeviceModelSendOutcome::Handled,
    );
    wait_until(|| {
        assert!(models.next_chunk().unwrap().is_none());
        attempts(&database)
            .first()
            .is_some_and(|state| state == "failed")
    });
    let mut backoff_slot = None;
    wait_until(|| match store.try_model_attempt_permit(&open).unwrap() {
        DeviceModelAdmission::InvokeAttempt(permit) => {
            backoff_slot = Some(permit);
            true
        }
        DeviceModelAdmission::Deferred => false,
        _ => panic!("expected physical invocation admission"),
    });
    assert_eq!(held.len(), 2);
    assert_full(&store, &open);
    let occupied_since = Instant::now();
    while occupied_since.elapsed() < Duration::from_millis(1250) {
        assert!(models.next_chunk().unwrap().is_none());
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(attempts(&database), vec!["failed"]);
    assert!(!directory.join("second-request-observed").exists());
    drop(backoff_slot);
    wait_until(|| {
        assert!(models.next_chunk().unwrap().is_none());
        directory.join("second-request-observed").exists()
    });
    assert_eq!(attempts(&database), vec!["failed", "invoking"]);
    assert_full(&store, &open);
    std::fs::write(directory.join("release-second-answer"), b"release").unwrap();
    let chunks = completed_chunks(&mut models);
    assert!(chunks.last().unwrap().is_final);
    assert!(chunks.iter().all(|chunk| chunk.error.is_none()));
    assert_eq!(attempts(&database), vec!["failed", "completed"]);
    let replay_slot = acquire(&store, &open);
    assert_full(&store, &open);
    assert_eq!(
        models
            .send(
                &ExecutionPortMessage::ModelOpenMessage(open),
                Some(Instant::now() + WAIT),
                None
            )
            .unwrap(),
        DeviceModelSendOutcome::Handled,
    );
    let replay = completed_chunks(&mut models);
    assert_eq!(replay, chunks, "durable replay returns the existing result");
    assert_eq!(
        attempts(&database),
        vec!["failed", "completed"],
        "replay creates no new model request"
    );
    drop(replay_slot);
    drop(held);
}

fn configure_retry_provider(database: &rusqlite::Connection, endpoint: &str) {
    let config = serde_json::json!({"providerId":"qwen","displayName":"fixture", "endpoint":endpoint,
        "protocol":"openai_chat_completions","modelIds":["qwen-model"],"enabled":true});
    database
        .execute(
            "INSERT INTO providers VALUES (?1,?2,?3)",
            rusqlite::params!["qwen", config.to_string(), b"fixture-only-key".as_slice()],
        )
        .unwrap();
}

fn crash_after_retained_retry(directory: &Path, endpoint: &str) -> ! {
    let mut models = DeviceModels::open(directory).unwrap();
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    configure_retry_provider(&database, endpoint);
    let open = completed_tool_history_open();
    assert_eq!(
        models
            .send(
                &ExecutionPortMessage::ModelOpenMessage(open),
                Some(Instant::now() + WAIT),
                None
            )
            .unwrap(),
        DeviceModelSendOutcome::Handled
    );
    wait_until(|| {
        database
            .query_row("SELECT COUNT(*) FROM model_retry_state", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
            == 1
    });
    assert_eq!(attempts(&database), vec!["failed"]);
    // Abrupt process termination intentionally skips every Rust destructor.
    std::process::exit(0);
}

fn run_reopened_retry(directory: &Path) {
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    let store = DeviceProviderStore::open(directory).unwrap();
    let open = completed_tool_history_open();
    assert!(matches!(
        store.try_model_permit(&open).unwrap(),
        DeviceModelAdmission::RecoverExchange
    ));
    let mut held = (0..3).map(|_| acquire(&store, &open)).collect::<Vec<_>>();
    let mut models = DeviceModels::open(directory).unwrap();
    assert_eq!(
        models
            .send(
                &ExecutionPortMessage::ModelOpenMessage(open.clone()),
                Some(Instant::now() + WAIT),
                None
            )
            .unwrap(),
        DeviceModelSendOutcome::Handled
    );
    thread::sleep(Duration::from_millis(1250));
    assert_eq!(attempts(&database), vec!["failed"]);
    assert!(
        !directory.join("second-request-observed").exists(),
        "durable recovery owns no implicit slot"
    );
    drop(held.pop());
    wait_until(|| directory.join("second-request-observed").exists());
    assert_eq!(attempts(&database), vec!["failed", "invoking"]);
    assert_full(&store, &open);
    std::fs::write(directory.join("release-second-answer"), b"release").unwrap();
    let chunks = completed_chunks(&mut models);
    assert!(chunks.iter().all(|chunk| chunk.error.is_none()));
    assert_eq!(attempts(&database), vec!["failed", "completed"]);
    held.push(acquire(&store, &open));
    assert_full(&store, &open);
    drop(models);
    let mut replay = DeviceModels::open(directory).unwrap();
    assert_eq!(
        replay
            .send(
                &ExecutionPortMessage::ModelOpenMessage(open),
                Some(Instant::now().checked_sub(Duration::from_secs(1)).unwrap()),
                None
            )
            .unwrap(),
        DeviceModelSendOutcome::Handled
    );
    assert_eq!(
        completed_chunks(&mut replay),
        chunks,
        "expired completed replay starts no attempt and occupies no slot"
    );
    assert_eq!(attempts(&database), vec!["failed", "completed"]);
}

fn completed_tool_history_open() -> ModelOpenMessage {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let template: ModelOpenMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "model.open")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let mut open = member_open(&template, "qwen", 500);
    let mut payload: serde_json::Value =
        serde_json::from_slice(&STANDARD.decode(&open.request.data_base64).unwrap()).unwrap();
    payload["request"]["tools"] = serde_json::json!([{"type":"function","name":"shell","description":"run", "parameters":{"type":"object","properties":{}}}]);
    payload["request"]["input"].as_array_mut().unwrap().extend([
        serde_json::json!({"type":"function_call","name":"shell","call_id":"completed-tool","arguments":"{}"}),
        serde_json::json!({"type":"function_call_output","call_id":"completed-tool","output":"already-completed-tool-result"}),
    ]);
    let bytes = serde_json::to_vec(&payload).unwrap();
    open.request.data_base64 = STANDARD.encode(&bytes);
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(bytes));
    open
}

fn acquire(store: &DeviceProviderStore, open: &ModelOpenMessage) -> DeviceModelPermit {
    match store.try_model_attempt_permit(open).unwrap() {
        DeviceModelAdmission::InvokeAttempt(permit) => permit,
        DeviceModelAdmission::Deferred => panic!("expected an available actual-invocation slot"),
        _ => panic!("expected physical invocation admission"),
    }
}

fn assert_full(store: &DeviceProviderStore, open: &ModelOpenMessage) {
    assert!(matches!(
        store.try_model_attempt_permit(open).unwrap(),
        DeviceModelAdmission::Deferred
    ));
}

fn attempts(database: &rusqlite::Connection) -> Vec<String> {
    database
        .prepare("SELECT state FROM model_invocation_attempts ORDER BY attempt_number")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn completed_chunks(models: &mut DeviceModels) -> Vec<ModelChunkMessage> {
    let mut chunks = Vec::new();
    wait_until(|| {
        let Some(chunk) = models.next_chunk().unwrap() else {
            return false;
        };
        let finished = chunk.is_final;
        chunks.push(chunk);
        finished
    });
    chunks
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < WAIT,
            "Worker recovery condition did not become true"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
