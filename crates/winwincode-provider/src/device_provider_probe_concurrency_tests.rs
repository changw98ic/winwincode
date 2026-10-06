// SPDX-License-Identifier: Apache-2.0

//! Probe traffic shares the normal model quota; its TLS trust lives only in a child process.

use super::*;
use crate::DeviceModelAdmission;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::Arc,
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const CHILD_DIRECTORY: &str = "WWC_PROVIDER_PROBE_SLOTS_TEST_DIRECTORY";
const CHILD_ENDPOINT: &str = "WWC_PROVIDER_PROBE_SLOTS_TEST_ENDPOINT";
const WAIT: Duration = Duration::from_secs(15);
type TlsStream = StreamOwned<ServerConnection, TcpStream>;

#[test]
fn provider_probe_does_not_send_when_slots_are_full_and_succeeds_after_release() {
    if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
        let store = DeviceProviderStore::open(Path::new(&directory)).unwrap();
        let mut permits = (0..3)
            .map(
                |_| match store.try_provider_model_permit("probe-provider").unwrap() {
                    DeviceModelAdmission::Ready(permit) => permit,
                    DeviceModelAdmission::Deferred => panic!("expected a free slot"),
                },
            )
            .collect::<Vec<_>>();
        assert!(store.test_provider(mutation(), "probe-full").is_err());
        drop(permits.pop());
        store.test_provider(mutation(), "probe-released").unwrap();
        assert!(
            matches!(
                store.try_provider_model_permit("probe-provider").unwrap(),
                DeviceModelAdmission::Ready(_)
            ),
            "completed probe releases its slot"
        );
        return;
    }
    let directory = std::env::temp_dir().join(format!(
        "wwc-probe-slots-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    drop(DeviceProviderStore::open(&directory).unwrap());
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let root = directory.join("root.der");
    fs::write(&root, cert.der()).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!(
        "https://localhost:{}/model",
        listener.local_addr().unwrap().port()
    );
    let server = thread::spawn(move || serve_probe(&listener, Arc::new(config)));
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "device_store::concurrency_tests::provider_probe_does_not_send_when_slots_are_full_and_succeeds_after_release", "--nocapture"])
        .env(CHILD_DIRECTORY, &directory).env(CHILD_ENDPOINT, endpoint)
        .env(DEVICE_PROVIDER_TLS_ROOT_DER_ENVIRONMENT, root).output().unwrap();
    let request = server.join().unwrap();
    fs::remove_dir_all(&directory).unwrap();
    assert!(
        output.status.success(),
        "child: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(request["model"], "probe-model");
}

fn mutation() -> Mutation {
    serde_json::from_value(serde_json::json!({
        "operation":"test", "config":{"providerId":"probe-provider","displayName":"probe fixture","endpoint":std::env::var(CHILD_ENDPOINT).unwrap(),"protocol":"openai_chat_completions","modelIds":["probe-model"],"enabled":true},"apiKey":"fixture-only-key"
    })).unwrap()
}

fn serve_probe(listener: &TcpListener, config: Arc<ServerConfig>) -> serde_json::Value {
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(WAIT)).unwrap();
                socket.set_write_timeout(Some(WAIT)).unwrap();
                let mut stream = StreamOwned::new(ServerConnection::new(config).unwrap(), socket);
                let request = read_request(&mut stream);
                let body = concat!(
                    "data: {\"id\":\"probe-response\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"probe-response\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\n",
                    "data: [DONE]\n\n"
                );
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                stream.flush().unwrap();
                return request;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && started.elapsed() < WAIT =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("probe fixture did not receive its released request: {error}"),
        }
    }
}

fn read_request(stream: &mut TlsStream) -> serde_json::Value {
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
                return serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
            }
        }
    }
}
