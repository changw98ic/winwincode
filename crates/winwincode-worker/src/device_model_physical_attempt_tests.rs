// SPDX-License-Identifier: Apache-2.0

//! Real HTTPS requests occupy all three slots while a crashed Worker resumes
//! two durable retries. One retry completes; the cancelled retry sends no HTTP.

use super::{TlsStream, WAIT, member_open, read_request, write_answer};
use crate::device_model::{DeviceModelSendOutcome, DeviceModels};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    net::TcpListener,
    path::Path,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio_rustls::rustls::{
    ServerConfig, ServerConnection,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use winwincode_execution_port::generated::{ExecutionPortMessage, ModelOpenMessage};
use winwincode_provider::{DeviceModelAdmission, DeviceProviderStore};

const TEST: &str = "device_model::concurrency_tests::physical_attempt_tests::three_real_https_attempts_bound_restarted_retries_and_cancelled_waiters";
const DIRECTORY: &str = "WWC_PHYSICAL_ATTEMPT_TEST_DIRECTORY";
const ENDPOINT: &str = "WWC_PHYSICAL_ATTEMPT_TEST_ENDPOINT";
const PHASE: &str = "WWC_PHYSICAL_ATTEMPT_TEST_PHASE";
const RETRY: usize = 600;
const CANCEL: usize = 601;
const REPLAY: usize = 602;
const LIVE: [usize; 3] = [603, 604, 605];
const OTHER: usize = 606;

type Request = (String, String, serde_json::Value, mpsc::Sender<bool>);

#[test]
fn three_real_https_attempts_bound_restarted_retries_and_cancelled_waiters() {
    if let Some(directory) = std::env::var_os(DIRECTORY) {
        let directory = Path::new(&directory);
        if std::env::var(PHASE).as_deref() == Ok("seed") {
            seed_and_crash(directory, &std::env::var(ENDPOINT).unwrap());
        }
        recover_under_three_live_requests(directory);
        return;
    }
    run_network_fixture();
}

#[allow(
    clippy::too_many_lines,
    reason = "the real HTTPS withholding window verifies the complete crash and restored retry timeline"
)]
fn run_network_fixture() {
    let root = tempfile::tempdir().unwrap();
    let providers = root.path().join("providers");
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = root.path().join("root.der");
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
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let (events, requests) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let server = {
        let stopped = Arc::clone(&stopped);
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        let providers = providers.clone();
        thread::spawn(move || {
            serve(
                &listener, &config, &events, &stopped, &active, &peak, &providers,
            );
        })
    };
    let spawn = |phase: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(DIRECTORY, &providers)
            .env(ENDPOINT, &endpoint)
            .env(PHASE, phase)
            .env("WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE", &certificate)
            .env_remove("WWC_DEVICE_PROVIDER_HTTPS_PROXY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let seed = spawn("seed");
    let mut all = Vec::new();
    for expected in ["replay", "retry", "cancel"] {
        let request = requests.recv_timeout(WAIT).expect("seed HTTPS request");
        assert_eq!(request.0, "qwen");
        assert_eq!(request.1, expected);
        request.3.send(expected == "replay").unwrap();
        all.push((request.0, request.1, request.2));
    }
    assert_child(&seed.wait_with_output().unwrap());
    let recovered = spawn("recover");
    let mut held = BTreeMap::new();
    for _ in 0..3 {
        let request = requests
            .recv_timeout(WAIT)
            .expect("three real in-flight HTTPS requests");
        assert_eq!(request.0, "qwen");
        assert!(request.1.starts_with("live-"));
        all.push((request.0, request.1.clone(), request.2));
        held.insert(request.1, request.3);
    }
    let other = requests
        .recv_timeout(WAIT)
        .expect("another provider bypasses qwen's full slots");
    assert_eq!(other.0, "glm");
    assert_eq!(other.1, "other");
    other.3.send(true).unwrap();
    all.push((other.0, other.1, other.2));
    wait_until(|| providers.join("waiting-retries-checked").exists());
    assert!(
        requests.try_recv().is_err(),
        "neither restored retry may bypass three physical slots"
    );
    held.remove("live-0").unwrap().send(true).unwrap();
    let retry = requests
        .recv_timeout(WAIT)
        .expect("restored retry takes a released physical slot");
    assert_eq!(retry.0, "qwen");
    assert_eq!(retry.1, "retry");
    let original = all.iter().find(|(_, marker, _)| marker == "retry").unwrap();
    assert_eq!(
        retry.2, original.2,
        "restart preserves completed tool history and the exact prepared step"
    );
    retry.3.send(true).unwrap();
    all.push((retry.0, retry.1, retry.2));
    for (_, release) in held {
        release.send(true).unwrap();
    }
    let output = recovered.wait_with_output().unwrap();
    stopped.store(true, Ordering::Release);
    server.join().unwrap();
    assert_child(&output);
    assert_eq!(
        peak.load(Ordering::Acquire),
        3,
        "physical qwen concurrency remains three across Worker restart"
    );
    assert_eq!(
        active.load(Ordering::Acquire),
        0,
        "every HTTP slot finishes"
    );
    assert_eq!(all.len(), 8);
    assert_eq!(
        all.iter()
            .filter(|(_, marker, _)| marker == "cancel")
            .count(),
        1,
        "cancelled slot waiter performs only its pre-crash attempt"
    );
    assert_eq!(
        all.iter()
            .filter(|(_, marker, _)| marker == "replay")
            .count(),
        1,
        "completed replay sends zero new HTTP"
    );
    assert!(
        requests.try_recv().is_err(),
        "no unobserved physical request was emitted"
    );
}

#[allow(clippy::too_many_arguments)]
fn serve(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    events: &mpsc::Sender<Request>,
    stopped: &AtomicBool,
    active: &Arc<AtomicUsize>,
    peak: &Arc<AtomicUsize>,
    directory: &Path,
) {
    let started = Instant::now();
    let mut connections = Vec::new();
    while !stopped.load(Ordering::Acquire) && started.elapsed() < WAIT * 3 {
        match listener.accept() {
            Ok((socket, _)) => {
                let config = Arc::clone(config);
                let events = events.clone();
                let active = Arc::clone(active);
                let peak = Arc::clone(peak);
                let directory = directory.to_owned();
                connections.push(thread::spawn(move || {
                    socket.set_nonblocking(false).unwrap();
                    socket.set_read_timeout(Some(WAIT)).unwrap();
                    socket.set_write_timeout(Some(WAIT)).unwrap();
                    let mut stream = TlsStream::new(ServerConnection::new(config).unwrap(), socket);
                    let (provider, request) = read_request(&mut stream);
                    let user = request["messages"].as_array().unwrap().iter().find(|message| message["role"] == "user").unwrap();
                    let marker = user["content"].as_str().or_else(|| user["content"][0]["text"].as_str()).unwrap().to_owned();
                    if provider == "qwen" {
                        let count = active.fetch_add(1, Ordering::AcqRel) + 1;
                        peak.fetch_max(count, Ordering::AcqRel);
                    }
                    std::fs::write(directory.join(format!("request-{marker}")), b"observed").unwrap();
                    let (release, released) = mpsc::channel();
                    events.send((provider.clone(), marker, request, release)).unwrap();
                    let answer = released.recv_timeout(WAIT);
                    if provider == "qwen" { active.fetch_sub(1, Ordering::AcqRel); }
                    if let Ok(success) = answer {
                        if success { write_answer(&mut stream, &provider); }
                        else {
                            write!(stream, "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 3\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
                            stream.flush().unwrap();
                        }
                    }
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
    for connection in connections {
        connection.join().unwrap();
    }
}

fn seed_and_crash(directory: &Path, endpoint: &str) -> ! {
    let mut models = DeviceModels::open(directory).unwrap();
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    for (provider, protocol) in [
        ("qwen", "openai_chat_completions"),
        ("glm", "anthropic_messages"),
    ] {
        let config = serde_json::json!({"providerId":provider,"displayName":"fixture","endpoint":format!("{endpoint}/{provider}"),"protocol":protocol,"modelIds":[format!("{provider}-model")],"enabled":true});
        database
            .execute(
                "INSERT INTO providers VALUES (?1,?2,?3)",
                rusqlite::params![provider, config.to_string(), b"fixture-only-key".as_slice()],
            )
            .unwrap();
    }
    let replay = open("qwen", REPLAY, "replay");
    send(&mut models, &replay);
    wait_until(|| {
        models.next_chunk().unwrap().is_some_and(|chunk| {
            chunk.model_exchange_id == replay.model_exchange_id && chunk.is_final
        })
    });
    for (index, marker) in [(RETRY, "retry"), (CANCEL, "cancel")] {
        let request = open("qwen", index, marker);
        send(&mut models, &request);
        wait_until(|| {
            database
                .query_row(
                    "SELECT COUNT(*) FROM model_retry_state WHERE exchange_id=?1",
                    [&request.model_exchange_id.0],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
                == 1
        });
        assert_eq!(attempts(&database, &request), vec!["failed"]);
    }
    // Preserve actual retry state; no Rust destructor or thread cleanup runs.
    std::process::exit(0);
}

#[allow(
    clippy::too_many_lines,
    reason = "restored retries, exact replay and cancellation share the same three physically occupied slots"
)]
fn recover_under_three_live_requests(directory: &Path) {
    let database = rusqlite::Connection::open(directory.join("providers.sqlite3")).unwrap();
    let store = DeviceProviderStore::open(directory).unwrap();
    let mut models = DeviceModels::open(directory).unwrap();
    for (member, index) in LIVE.into_iter().enumerate() {
        send(&mut models, &open("qwen", index, &format!("live-{member}")));
    }
    wait_until(|| (0..3).all(|member| directory.join(format!("request-live-{member}")).exists()));
    let retry = open("qwen", RETRY, "retry");
    assert!(matches!(
        store.try_model_attempt_permit(&retry).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    let replay = open("qwen", REPLAY, "replay");
    let stored = store.replay_model(&replay.model_exchange_id.0, 1).unwrap();
    assert_eq!(
        models
            .send(
                &ExecutionPortMessage::ModelOpenMessage(replay.clone()),
                Some(Instant::now().checked_sub(Duration::from_secs(1)).unwrap()),
                None
            )
            .unwrap(),
        DeviceModelSendOutcome::Handled
    );
    let mut replayed = Vec::new();
    wait_until(|| {
        let Some(chunk) = models.next_chunk().unwrap() else {
            return false;
        };
        assert_eq!(chunk.model_exchange_id, replay.model_exchange_id);
        let finished = chunk.is_final;
        replayed.push(chunk);
        finished
    });
    assert_eq!(replayed, stored);
    assert!(
        matches!(
            store.try_model_attempt_permit(&retry).unwrap(),
            DeviceModelAdmission::Deferred
        ),
        "completed replay does not acquire a slot"
    );
    let other = open("glm", OTHER, "other");
    send(&mut models, &other);
    wait_until(|| {
        models.next_chunk().unwrap().is_some_and(|chunk| {
            chunk.model_exchange_id == other.model_exchange_id && chunk.is_final
        })
    });
    let cancel = open("qwen", CANCEL, "cancel");
    for request in [&retry, &cancel] {
        assert!(matches!(
            store.try_model_permit(request).unwrap(),
            DeviceModelAdmission::RecoverExchange
        ));
        send(&mut models, request);
    }
    // Both durable retry deadlines pass while three actual requests are held.
    thread::sleep(Duration::from_millis(3250));
    assert_eq!(attempts(&database, &retry), vec!["failed"]);
    assert_eq!(attempts(&database, &cancel), vec!["failed"]);
    store.cancel_model(&cancel.model_exchange_id.0).unwrap();
    wait_until(|| {
        let _ = models.next_chunk().unwrap();
        !models.pending.contains_key(&cancel.model_exchange_id.0)
    });
    assert_eq!(attempts(&database, &cancel), vec!["failed"]);
    std::fs::write(directory.join("waiting-retries-checked"), b"checked").unwrap();
    let expected = LIVE
        .into_iter()
        .chain([RETRY])
        .map(|index| format!("mex_{index:026}"))
        .collect::<BTreeSet<_>>();
    let mut completed = BTreeSet::new();
    wait_until(|| {
        if let Some(chunk) = models.next_chunk().unwrap() {
            assert!(
                chunk.error.is_none(),
                "unexpected failure: {:?}",
                chunk.error
            );
            if chunk.is_final {
                completed.insert(chunk.model_exchange_id.0);
            }
        }
        completed == expected
    });
    assert_eq!(attempts(&database, &retry), vec!["failed", "completed"]);
    assert_eq!(attempts(&database, &cancel), vec!["failed"]);
    // Prove physical permits were released after all four successful calls.
    let slots = (0..3)
        .map(|_| match store.try_model_attempt_permit(&retry).unwrap() {
            DeviceModelAdmission::InvokeAttempt(permit) => permit,
            _ => panic!("completed calls must release every physical slot"),
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        store.try_model_attempt_permit(&retry).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    drop(slots);
}

fn open(provider: &str, index: usize, marker: &str) -> ModelOpenMessage {
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
    let mut open = member_open(&template, provider, index);
    let mut payload: serde_json::Value =
        serde_json::from_slice(&STANDARD.decode(&open.request.data_base64).unwrap()).unwrap();
    payload["request"]["input"][0]["content"][0]["text"] = marker.into();
    if index == RETRY {
        payload["request"]["tools"] = serde_json::json!([{"type":"function","name":"shell","description":"run","parameters":{"type":"object","properties":{}}}]);
        payload["request"]["input"].as_array_mut().unwrap().extend([
            serde_json::json!({"type":"function_call","name":"shell","call_id":"completed-tool","arguments":"{}"}),
            serde_json::json!({"type":"function_call_output","call_id":"completed-tool","output":"already-completed-tool-result"}),
        ]);
    }
    let bytes = serde_json::to_vec(&payload).unwrap();
    open.request.data_base64 = STANDARD.encode(&bytes);
    open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(bytes));
    open
}

fn send(models: &mut DeviceModels, open: &ModelOpenMessage) {
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
}

fn attempts(database: &rusqlite::Connection, open: &ModelOpenMessage) -> Vec<String> {
    database.prepare("SELECT state FROM model_invocation_attempts WHERE exchange_id=?1 ORDER BY attempt_number").unwrap()
        .query_map([&open.model_exchange_id.0], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
}

fn assert_child(output: &Output) {
    assert!(
        output.status.success(),
        "Worker fixture: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < WAIT,
            "physical attempt fixture condition timed out"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[path = "device_model_reopened_retry_tests.rs"]
mod reopened_retry_tests;
