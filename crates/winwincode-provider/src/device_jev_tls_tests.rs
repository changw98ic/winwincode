// SPDX-License-Identifier: Apache-2.0

//! Device `JEv` shares the production Provider trust configuration and verifies TLS.

use super::*;
use crate::{JevDevice, JevDtype, JevExecutionOptions, JevHypothesis, JevRemoteTransport};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const CHILD_CASE: &str = "WWC_JEV_TLS_TEST_CASE";
const CHILD_ENDPOINT: &str = "WWC_JEV_TLS_TEST_ENDPOINT";
const WAIT: Duration = Duration::from_secs(15);
type TlsStream = StreamOwned<ServerConnection, TcpStream>;

#[test]
fn device_jev_uses_the_explicit_provider_root_and_rejects_untrusted_or_wrong_host_https() {
    if let Ok(case) = std::env::var(CHILD_CASE) {
        let settings: crate::OpenJevRemoteSettings = serde_json::from_value(serde_json::json!({
            "providerId":"jev-tls-fixture","endpoint":std::env::var(CHILD_ENDPOINT).unwrap(),
            "apiKey":"owned-jev-tls-secret","modelId":"fixture-model","timeoutMs":5000,"retries":0,
            "devices":["remote"],"dtypes":["auto"]
        }))
        .unwrap();
        let (config, _) = settings.to_config_and_runtime().unwrap();
        let transport = device_jev_transport(&config);
        if ["empty-root", "invalid-der", "missing-file"].contains(&case.as_str()) {
            assert_eq!(
                transport.unwrap_err(),
                if case == "missing-file" {
                    DeviceProviderError::FilesystemUnavailable
                } else {
                    DeviceProviderError::StateInvalid
                }
            );
            return;
        }
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(transport.unwrap().score(
                "fixture-model".into(),
                vec![JevHypothesis {
                    premise: "The retained tool reported failure".into(),
                    hypothesis: "The command passed".into(),
                }],
                JevExecutionOptions {
                    device: JevDevice::Remote,
                    dtype: JevDtype::Auto,
                },
            ));
        if case == "trusted" {
            let batch = result.unwrap();
            assert_eq!(batch.input_tokens, 7);
            assert_eq!(batch.output_tokens, Some(2));
            assert_eq!(batch.resolved_model_id.as_deref(), Some("fixture-model"));
            assert_eq!(batch.evaluations.len(), 1);
        } else {
            let failure = result.unwrap_err().network_failure();
            assert_eq!(failure.kind, winwincode_network::ErrorKind::TlsInvalid);
            // TCP connected before TLS failed. The transport has no proof of
            // NotSent; the parent separately checks that no HTTP arrived.
            assert_eq!(failure.acceptance, winwincode_network::Acceptance::Unknown);
        }
        return;
    }
    for case in [
        "trusted",
        "unconfigured",
        "wrong-root",
        "wrong-host",
        "empty-root",
        "invalid-der",
        "missing-file",
    ] {
        exercise_tls_case(case);
    }
}

fn exercise_tls_case(case: &str) {
    let directory = std::env::temp_dir().join(format!(
        "wwc-jev-tls-{case}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).unwrap();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let root = directory.join("root.der");
    match case {
        "wrong-root" => fs::write(
            &root,
            generate_simple_self_signed(vec!["localhost".into()])
                .unwrap()
                .cert
                .der(),
        )
        .unwrap(),
        "empty-root" => fs::write(&root, []).unwrap(),
        "invalid-der" => fs::write(&root, b"not a DER certificate").unwrap(),
        "missing-file" => {}
        _ => fs::write(&root, cert.der()).unwrap(),
    }
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hostname = if case == "wrong-host" {
        "127.0.0.1"
    } else {
        "localhost"
    };
    let endpoint = format!(
        "https://{hostname}:{}/score",
        listener.local_addr().unwrap().port()
    );
    let done = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&done);
    let server = thread::spawn(move || serve_jev(&listener, &Arc::new(config), &stopped));
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.args(["--exact","device_store::jev_tls_tests::device_jev_uses_the_explicit_provider_root_and_rejects_untrusted_or_wrong_host_https","--nocapture"])
        .env(CHILD_CASE,case).env(CHILD_ENDPOINT,endpoint);
    if case == "unconfigured" {
        child.env_remove(DEVICE_PROVIDER_TLS_ROOT_DER_ENVIRONMENT);
    } else {
        child.env(DEVICE_PROVIDER_TLS_ROOT_DER_ENVIRONMENT, &root);
    }
    let output = child.output().unwrap();
    done.store(true, Ordering::Release);
    let requests = server.join().unwrap();
    fs::remove_dir_all(directory).unwrap();
    assert!(
        output.status.success(),
        "{case} child: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        requests.len(),
        usize::from(case == "trusted"),
        "TLS failure must not send a paid SystemOne request"
    );
    if let Some(request) = requests.first() {
        assert_eq!(request["model"], "fixture-model");
        assert_eq!(
            request["state"]["premises"]["premise_0"],
            "The retained tool reported failure"
        );
        assert_eq!(
            request["state"]["items"]["item_0"]["hypothesis"],
            "The command passed"
        );
    }
}

fn serve_jev(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    done: &AtomicBool,
) -> Vec<serde_json::Value> {
    let started = Instant::now();
    let mut requests = Vec::new();
    while !done.load(Ordering::Acquire) && started.elapsed() < WAIT {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(WAIT)).unwrap();
                socket.set_write_timeout(Some(WAIT)).unwrap();
                let mut stream =
                    StreamOwned::new(ServerConnection::new(Arc::clone(config)).unwrap(), socket);
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                requests.push(request);
                let body = r#"{"model":"fixture-model","answers":{"item_0":{"type":"choice","choice":"neutral","confidence":0.8,"probabilities":{"entailment":0.1,"contradiction":0.1,"neutral":0.8}}},"usage":{"input_tokens":7,"output_tokens":2}}"#;
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                stream.flush().unwrap();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("JEv TLS fixture accept failed: {error}"),
        }
    }
    requests
}

fn read_request(stream: &mut TlsStream) -> Option<serde_json::Value> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 64 * 1024);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                return Some(serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap());
            }
        }
    }
}
