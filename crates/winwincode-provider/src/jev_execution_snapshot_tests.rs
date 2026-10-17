// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{
    DeviceProviderError, DeviceProviderStore, LocalDeviceProviderSource,
    ProviderConnectionMaterial, ProviderExecutionRequest, ProviderExecutionSource,
    ProviderJevParameters, ProviderModelParameters, ProviderRuntimeStore, StoredJevJudge,
};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use std::{
    io::Write as _,
    net::{TcpListener, TcpStream},
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex,
        atomic::AtomicBool,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
use winwincode_execution_port::agent_config::{
    AgentSessionConfigSnapshot, resolve_agent_session_config,
};

const TEST: &str = "jev::execution_snapshot_tests::configured_jev_retries_freeze_runtime_parameters_and_recheck_live_authority";
const CHILD_CASE: &str = "WWC_JEV_SNAPSHOT_CASE";
const CHILD_ROOT: &str = "WWC_JEV_SNAPSHOT_ROOT";
const CHILD_ENDPOINT: &str = "WWC_JEV_SNAPSHOT_ENDPOINT";
const WAIT: Duration = Duration::from_secs(10);
const OPERATION: &str = "jev-snapshot-original";

#[test]
fn configured_jev_retries_freeze_runtime_parameters_and_recheck_live_authority() {
    if let Ok(case) = std::env::var(CHILD_CASE) {
        exercise_operation(&case);
        return;
    }
    for case in ["runtime", "cancelled", "revoked", "endpoint", "model"] {
        exercise_child(case);
    }
}

fn exercise_child(case: &str) {
    let directory = std::env::temp_dir().join(format!(
        "wwc-jev-snapshot-{case}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let root = directory.join("root.der");
    std::fs::write(&root, cert.der()).unwrap();
    let tls = Arc::new(
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
        "https://localhost:{}/score",
        listener.local_addr().unwrap().port()
    );
    let done = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&done);
    let server = thread::spawn(move || serve(&listener, &tls, &stopped));
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(CHILD_CASE, case)
        .env(CHILD_ROOT, &directory)
        .env(CHILD_ENDPOINT, endpoint)
        .env("WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE", root)
        .output()
        .unwrap();
    done.store(true, Ordering::Release);
    let requests = server.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    assert!(
        output.status.success(),
        "{case}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        requests.len(),
        if case == "runtime" { 4 } else { 1 },
        "{case}"
    );
    assert_eq!(requests[0].0, "Bearer snapshot-original-secret");
    if case == "runtime" {
        assert_eq!(requests[1].0, "Bearer snapshot-rotated-secret");
        assert_eq!(requests[0].1, requests[1].1);
        assert_eq!(requests[1].1["model"], "snapshot-model");
    }
}

#[derive(Debug)]
struct InterleavedSource {
    local: LocalDeviceProviderSource,
    attempts: AtomicUsize,
    retry_ready: Sender<()>,
    changed: Mutex<Receiver<()>>,
}

impl ProviderExecutionSource for InterleavedSource {
    fn model_parameters(
        &self,
        request: &ProviderExecutionRequest,
    ) -> Result<ProviderModelParameters, DeviceProviderError> {
        self.local.model_parameters(request)
    }

    fn jev_parameters(
        &self,
        request: &ProviderExecutionRequest,
    ) -> Result<ProviderJevParameters, DeviceProviderError> {
        self.local.jev_parameters(request)
    }

    fn resolve_connection(
        &self,
        request: &ProviderExecutionRequest,
        can_start: &dyn Fn() -> bool,
    ) -> Result<ProviderConnectionMaterial, DeviceProviderError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 1 {
            assert_eq!(request.physical_attempt, Some(2));
            self.retry_ready.send(()).unwrap();
            self.changed.lock().unwrap().recv_timeout(WAIT).unwrap();
        }
        self.local.resolve_connection(request, can_start)
    }
}

fn session() -> AgentSessionConfigSnapshot {
    resolve_agent_session_config(
        &winwincode_domain::WorkerId("snapshot-worker".into()),
        &serde_json::from_value(serde_json::json!({
            "capabilityDigest":format!("sha256:{}", "a".repeat(64)),
            "features":[],"maxConcurrentJobs":1,"platform":"aarch64-apple-darwin"
        }))
        .unwrap(),
        "executor",
        serde_json::from_value(serde_json::json!({
            "provider":"fixture","model":"fixture","reasoning":"max","tools":[],
            "sandbox":"candidate","instructions":null,"jevJudge":"snapshot-jev"
        }))
        .unwrap(),
    )
    .unwrap()
}

fn settings() -> OpenJevRemoteSettings {
    serde_json::from_value(serde_json::json!({
        "providerId":"snapshot-jev","endpoint":std::env::var(CHILD_ENDPOINT).unwrap(),
        "apiKey":"snapshot-original-secret","modelId":"snapshot-model",
        "timeoutMs":5000,"retries":2,"maxBatchSize":8,"devices":["remote"],"dtypes":["auto"]
    }))
    .unwrap()
}

fn change_settings(
    directory: &Path,
    case: &str,
    ready: &Receiver<()>,
    changed: &Sender<()>,
    cancellation: &winwincode_network::transport::ExchangeCancellation,
) {
    ready.recv_timeout(WAIT).unwrap();
    let config = DeviceProviderStore::open(directory).unwrap();
    let mut latest = settings();
    latest.timeout_ms = 500;
    latest.retries = 0;
    latest.api_key = Some("snapshot-rotated-secret".into());
    match case {
        "endpoint" => latest.endpoint.push_str("/changed"),
        "model" => latest.model_id = Some("changed-model".into()),
        _ => {}
    }
    config.save_jev_settings(&latest).unwrap();
    if case == "revoked" {
        config
            .connection
            .execute("DELETE FROM jev_settings", [])
            .unwrap();
    } else if case == "cancelled" {
        cancellation.cancel();
    }
    changed.send(()).unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "One physical retry interleaving verifies the retained receipt and subsequent operations"
)]
fn exercise_operation(case: &str) {
    let directory = PathBuf::from(std::env::var(CHILD_ROOT).unwrap());
    let initial = settings();
    let config = DeviceProviderStore::open(&directory).unwrap();
    config.save_jev_settings(&initial).unwrap();
    let (ready, pending) = channel();
    let (changed, released) = channel();
    let source = Arc::new(InterleavedSource {
        local: LocalDeviceProviderSource::new(directory.clone()),
        attempts: AtomicUsize::new(0),
        retry_ready: ready,
        changed: Mutex::new(released),
    });
    let store = ProviderRuntimeStore::prepare(&directory)
        .unwrap()
        .with_execution_source(source.clone());
    let session = session();
    let digest = crate::device_jev_context::configured_context_digest(&session, &initial).unwrap();
    let (remote, retry) = initial.to_config_and_runtime().unwrap();
    let cancellation = Arc::new(winwincode_network::transport::ExchangeCancellation::default());
    let runtime = JevRuntime::new(
        vec![Arc::new(OpenJevRemoteProvider::new(
            remote.clone(),
            Arc::new(crate::device_store::device_jev_transport(&remote).unwrap()),
        ))],
        retry,
    )
    .with_cancellation(Some(Arc::clone(&cancellation)))
    .with_execution_source(
        source.clone(),
        store
            .jev_execution_request(OPERATION, &initial.provider_id, true)
            .unwrap(),
        ProviderJevParameters::from(&initial),
    );
    let changed_directory = directory.clone();
    let changed_case = case.to_owned();
    let settings_thread = thread::spawn(move || {
        change_settings(
            &changed_directory,
            &changed_case,
            &pending,
            &changed,
            &cancellation,
        );
    });
    let input = JevHypothesis {
        premise: "The retained test failed".into(),
        hypothesis: "The test passed".into(),
    };
    let options = JevExecutionOptions {
        device: JevDevice::Remote,
        dtype: JevDtype::Auto,
    };
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        let result = store.evaluate_judge_once(OPERATION, &digest, &runtime, input.clone(), options).await.unwrap();
        settings_thread.join().unwrap();
        let StoredJevJudge::Completed { run, replayed:false } = result else { panic!("new operation receipt required") };
        assert_eq!(source.attempts.load(Ordering::SeqCst), 2);
        let saved: String = store.connection.query_row(
            "SELECT request_json FROM jev_judge_exchanges WHERE operation_id=?1", [OPERATION], |row| row.get(0),
        ).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&saved).unwrap()[1], digest);
        let attempts: Vec<(String, String)> = store.connection.prepare(
            "SELECT status,request_json FROM jev_invocation_attempts WHERE operation_id=?1 ORDER BY sequence",
        ).unwrap().query_map([OPERATION], |row| Ok((row.get(0)?,row.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].0, "failed");
        assert_eq!(attempts[0].1, attempts[1].1);
        if case != "runtime" {
            assert!(run.value.is_none(), "{case}");
            assert_eq!(attempts[1].0, "failed");
            if case == "cancelled" || case == "revoked" {
                assert_eq!(run.failures.last().unwrap().network.unwrap().acceptance, Acceptance::NotSent);
            } else {
                assert_eq!(run.failures.last().unwrap().kind, JevProviderErrorKind::InvalidRequest);
            }
            return;
        }
        assert_eq!(run.value, Some([0.1, 0.1, 0.8]), "runtime changes must preserve the active operation");
        assert_eq!(run.failures.len(), 1);
        assert_eq!(attempts[1].0, "succeeded");
        let observation = run.observation.as_ref().unwrap();
        assert_eq!(observation.provider_id, initial.provider_id);
        assert_eq!(observation.model_id, "snapshot-model");
        assert_eq!(observation.input_tokens, 7);
        assert_eq!(observation.output_tokens, Some(2));
        let recovered = ProviderRuntimeStore::open_existing(store.directory()).unwrap()
            .with_execution_source(source.clone());
        let replay = recovered.evaluate_judge_once(OPERATION, &digest, &runtime, input.clone(), options).await.unwrap();
        assert_eq!(replay, StoredJevJudge::Completed { run, replayed:true });
        assert_eq!(source.attempts.load(Ordering::SeqCst), 2, "recovery must not send another request");
        assert_eq!(store.evaluate_configured_judge_once(OPERATION, &session, input.clone()).await.unwrap_err(), DeviceProviderError::StateInvalid);
        let StoredJevJudge::Completed { run, .. } = store.evaluate_configured_judge_once("jev-snapshot-next", &session, input.clone()).await.unwrap() else { panic!("next operation receipt required") };
        assert!(run.value.is_none());
        assert_eq!(run.failures.len(), 1, "next operation adopts retries=0");
        assert_eq!(run.failures[0].network.unwrap().http_status, Some(503));
        let latest = config.resolve_jev_settings(&initial.provider_id).unwrap();
        let next_digest = crate::device_jev_context::configured_context_digest(&session, &latest).unwrap();
        assert_ne!(next_digest, digest);
        let next_request: String = store.connection.query_row(
            "SELECT request_json FROM jev_judge_exchanges WHERE operation_id='jev-snapshot-next'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&next_request).unwrap()[1], next_digest);
        let StoredJevJudge::Completed { run, .. } = store.evaluate_configured_judge_once("jev-snapshot-timeout", &session, input).await.unwrap() else { panic!("timeout operation receipt required") };
        assert!(run.value.is_none());
        assert_eq!(run.failures.len(), 1);
        assert_eq!(run.failures[0].kind, JevProviderErrorKind::Timeout, "next operation adopts timeout=500ms");
        assert_eq!(source.attempts.load(Ordering::SeqCst), 4);
    });
}

type TlsStream = StreamOwned<ServerConnection, TcpStream>;

fn serve(
    listener: &TcpListener,
    tls: &Arc<ServerConfig>,
    done: &AtomicBool,
) -> Vec<(String, serde_json::Value)> {
    let mut requests = Vec::new();
    while !done.load(Ordering::Acquire) {
        let socket = match listener.accept() {
            Ok((socket, _)) => socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(error) => panic!("TLS fixture accept: {error}"),
        };
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        let mut stream = StreamOwned::new(ServerConnection::new(Arc::clone(tls)).unwrap(), socket);
        requests.push(read_request(&mut stream));
        let index = requests.len() - 1;
        let (status, body) = if index == 0 || index == 2 {
            ("503 Service Unavailable", "{}")
        } else {
            thread::sleep(Duration::from_millis(1500));
            (
                "200 OK",
                r#"{"model":"snapshot-model","answers":{"item_0":{"type":"choice","choice":"neutral","confidence":0.8,"probabilities":{"entailment":0.1,"contradiction":0.1,"neutral":0.8}}},"usage":{"input_tokens":7,"output_tokens":2}}"#,
            )
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.flush();
    }
    requests
}

fn read_request(stream: &mut TlsStream) -> (String, serde_json::Value) {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 64 * 1024);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let header = |name: &str| {
                headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case(name).then_some(value.trim())
                    })
                    .unwrap()
            };
            let length: usize = header("content-length").parse().unwrap();
            if bytes.len() >= end + 4 + length {
                return (
                    header("authorization").to_owned(),
                    serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap(),
                );
            }
        }
    }
}
