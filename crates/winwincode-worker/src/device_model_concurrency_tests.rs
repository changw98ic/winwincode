// SPDX-License-Identifier: Apache-2.0

//! The network fixture withholds every answer until every expected request arrives.
//! Run the Device lane in a child process to isolate its TLS-root environment.

#[path = "device_model_recovery_tests.rs"]
mod recovery_tests;

use super::{DeviceModelSendOutcome, DeviceModels};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use tokio_rustls::rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use winwincode_execution_port::generated::{ExecutionPortMessage, ModelOpenMessage};

const PROVIDERS: [&str; 4] = ["glm", "mimo", "deepseek", "qwen"];
const CHILD_DIRECTORY: &str = "WWC_FUSION_CONCURRENCY_TEST_DIRECTORY";
const CHILD_ENDPOINT: &str = "WWC_FUSION_CONCURRENCY_TEST_ENDPOINT";
const WAIT: Duration = Duration::from_secs(15);
type ReceivedRequest = (String, serde_json::Value, mpsc::Sender<()>);
type TlsStream = StreamOwned<ServerConnection, TcpStream>;

#[test]
fn fusion_members_reach_device_https_concurrently_before_any_answer() {
    run_concurrent_members(
        1,
        "device_model::concurrency_tests::fusion_members_reach_device_https_concurrently_before_any_answer",
    );
}

#[test]
fn four_providers_each_run_three_device_https_calls_before_any_answer() {
    run_concurrent_members(
        3,
        "device_model::concurrency_tests::four_providers_each_run_three_device_https_calls_before_any_answer",
    );
}

fn run_concurrent_members(per_provider: usize, test_name: &str) {
    let expected = per_provider * PROVIDERS.len();
    if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
        run_device_members(
            Path::new(&directory),
            &std::env::var(CHILD_ENDPOINT).unwrap(),
            per_provider,
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let root = directory.path().join("root.der");
    std::fs::write(&root, cert.der()).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    listener.set_nonblocking(true).unwrap();
    let (request_tx, requests) = mpsc::channel();
    let server =
        thread::spawn(move || accept_members(&listener, &Arc::new(config), &request_tx, expected));
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_DIRECTORY, directory.path().join("providers"))
        .env(CHILD_ENDPOINT, endpoint)
        .env("WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE", root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut received = Vec::new();
    for _ in 0..expected {
        match requests.recv_timeout(WAIT) {
            Ok(request) => received.push(request),
            Err(_) => break,
        }
    }
    // Release even a failed run so its fixture and child cannot leak.
    for (_, _, release) in &received {
        let _ = release.send(());
    }
    if received.len() != expected {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert_eq!(
        received.len(),
        expected,
        "every actual HTTPS request must arrive before any response is released; child: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "child: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_received_members(&received, per_provider);
}

fn assert_received_members(received: &[ReceivedRequest], per_provider: usize) {
    assert_eq!(
        received
            .iter()
            .map(|(provider, _, _)| provider.as_str())
            .collect::<BTreeSet<_>>(),
        PROVIDERS.into_iter().collect(),
    );
    let mut counts = BTreeMap::new();
    for (provider, _, _) in received {
        *counts.entry(provider.as_str()).or_insert(0) += 1;
    }
    assert!(counts.values().all(|count| *count == per_provider));
    let prompts = received
        .iter()
        .map(|(provider, request, _)| {
            assert_eq!(request["model"], format!("{provider}-model"));
            let user = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "user")
                .unwrap();
            user["content"]
                .as_str()
                .or_else(|| user["content"][0]["text"].as_str())
                .unwrap()
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        prompts.len(),
        1,
        "all members receive the same blind prompt"
    );
}

fn accept_members(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    request_tx: &mpsc::Sender<ReceivedRequest>,
    expected: usize,
) {
    let started = Instant::now();
    let mut members = Vec::new();
    while members.len() < expected && started.elapsed() < WAIT {
        match listener.accept() {
            Ok((socket, _)) => {
                let config = Arc::clone(config);
                let request_tx = request_tx.clone();
                members.push(thread::spawn(move || {
                    socket.set_read_timeout(Some(WAIT)).unwrap();
                    socket.set_write_timeout(Some(WAIT)).unwrap();
                    let mut stream =
                        StreamOwned::new(ServerConnection::new(config).unwrap(), socket);
                    let (provider, request) = read_request(&mut stream);
                    let (release, released) = mpsc::channel();
                    request_tx
                        .send((provider.clone(), request, release))
                        .unwrap();
                    if released.recv_timeout(WAIT).is_ok() {
                        write_answer(&mut stream, &provider);
                    }
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
    for member in members {
        member.join().unwrap();
    }
}

fn read_request(stream: &mut TlsStream) -> (String, serde_json::Value) {
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0);
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&request[..end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if request.len() >= end + 4 + length {
                let provider = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .trim_start_matches('/')
                    .to_owned();
                return (
                    provider,
                    serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap(),
                );
            }
        }
    }
}

fn write_answer(stream: &mut TlsStream, provider: &str) {
    let body = if provider == "qwen" {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"id":format!("response-{provider}"),"choices":[{"index":0,"delta":{"role":"assistant","content":"{\"claims\":[]}"},"finish_reason":null}]}),
            serde_json::json!({"id":format!("response-{provider}"),"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}),
        )
    } else {
        [
            serde_json::json!({"type":"message_start","message":{"id":format!("response-{provider}"),"type":"message","role":"assistant","model":format!("{provider}-model"),"content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"{\"claims\":[]}"}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}),
            serde_json::json!({"type":"message_stop"}),
        ]
        .into_iter()
        .fold(String::new(), |mut body, event| {
            write!(body, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
            body
        })
    };
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    stream.flush().unwrap();
}

fn run_device_members(directory: &Path, endpoint: &str, per_provider: usize) {
    let expected = per_provider * PROVIDERS.len();
    let mut models = DeviceModels::open(directory).unwrap();
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    for provider in PROVIDERS {
        let protocol = if provider == "qwen" {
            "openai_chat_completions"
        } else {
            "anthropic_messages"
        };
        let config = serde_json::json!({"providerId":provider,"displayName":provider,"endpoint":format!("{endpoint}/{provider}"),"protocol":protocol,"modelIds":[format!("{provider}-model")],"enabled":true});
        database
            .execute(
                "INSERT INTO providers VALUES (?1, ?2, ?3)",
                rusqlite::params![provider, config.to_string(), b"fixture-only-key".as_slice()],
            )
            .unwrap();
    }
    drop(database);
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
    for (provider_index, provider) in PROVIDERS.into_iter().enumerate() {
        for member in 0..per_provider {
            let open = member_open(&template, provider, provider_index * per_provider + member);
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
        }
        if provider_index == 0 && per_provider == 3 {
            // The parent still awaits the other nine HTTP requests, so no GLM slot can finish.
            let fourth = member_open(&template, provider, expected);
            assert_eq!(
                models
                    .send(
                        &ExecutionPortMessage::ModelOpenMessage(fourth.clone()),
                        Some(Instant::now() + WAIT),
                        None
                    )
                    .unwrap(),
                DeviceModelSendOutcome::NotStarted
            );
            assert!(
                !winwincode_provider::DeviceProviderStore::open(directory)
                    .unwrap()
                    .model_start_recorded(&fourth)
                    .unwrap()
            );
            assert_eq!(models.pending.len(), 3);
        }
    }
    assert_eq!(
        models.pending.len(),
        expected,
        "one task opens all members before collecting any answer"
    );
    let mut completed = BTreeSet::new();
    let started = Instant::now();
    while completed.len() < expected && started.elapsed() < WAIT {
        if let Some(chunk) = models.next_chunk().unwrap() {
            assert!(chunk.error.is_none(), "member failed: {:?}", chunk.error);
            assert_eq!(chunk.lease, template.lease);
            assert_eq!(chunk.session_identity, template.session_identity);
            if chunk.is_final {
                let terminal: serde_json::Value = serde_json::from_slice(
                    &STANDARD
                        .decode(&chunk.payload.unwrap().data_base64)
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(terminal["type"], "completed");
                assert_eq!(terminal["endTurn"], true);
                completed.insert(chunk.model_exchange_id.0);
            }
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(completed.len(), expected);
}

fn member_open(template: &ModelOpenMessage, provider: &str, index: usize) -> ModelOpenMessage {
    let mut open = template.clone();
    open.model_exchange_id.0 = format!("mex_{index:026}");
    open.request_id.0 = format!("req_{index:026}");
    open.message_id.0 = format!("xmsg_{index:026}");
    let payload = serde_json::to_vec(&serde_json::json!({"requestId":open.request_id.0,"provider":provider,"sessionId":template.worker_session_id.0,"threadId":template.session_identity.codex_thread_id.0,"turnId":"fusion-turn","request":{"model":format!("{provider}-model"),"instructions":"Answer independently","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Same frozen candidate"}]}],"tools":[],"tool_choice":"auto","parallel_tool_calls":false,"stream":true,"store":false}})).unwrap();
    open.request.content_type = "application/json".into();
    open.request.data_base64 = STANDARD.encode(&payload);
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
    open
}
