// SPDX-License-Identifier: Apache-2.0

use std::{io::Write as _, sync::Arc, time::Duration};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, rustls};
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_network::{Acceptance, ErrorKind, NetworkFailure, Phase};
use winwincode_worker::{
    WorkerExecutionPort,
    remote_transport::{RemoteWorkerPort, RemoteWorkerPortError},
};

fn outbound_message() -> ExecutionPortMessage {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == "runtime.event")
            .unwrap()
            .clone(),
    )
    .unwrap()
}

async fn status_response(status: u16) -> (String, Vec<u8>, tokio::task::JoinHandle<()>) {
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let certificate = certified.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate.clone()], key.into())
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("https://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut stream = TlsAcceptor::from(Arc::new(tls))
            .accept(stream)
            .await
            .unwrap();
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let read = stream.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "request ended before its declared body");
            request.extend_from_slice(&buffer[..read]);
            assert!(request.len() < 2 * 1024 * 1024);
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
        // A non-200 response must retain its status even if its body is incomplete.
        let response = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Length: 999\r\nRetry-After: 2\r\nConnection: close\r\n\r\nSYNTHETIC_PRIVATE_RESPONSE"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    });
    (origin, certificate.to_vec(), task)
}

#[tokio::test]
async fn worker_retains_every_non_200_status_and_classification_from_real_tls_wire() {
    use std::os::unix::fs::OpenOptionsExt;

    for (status, expected_kind) in [
        (204, ErrorKind::RequestInvalid),
        (400, ErrorKind::RequestInvalid),
        (401, ErrorKind::Authentication),
        (403, ErrorKind::Authorization),
        (413, ErrorKind::RequestInvalid),
        (429, ErrorKind::RateLimited),
        (503, ErrorKind::ServerTransient),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let credential = directory.path().join("credential");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&credential)
            .unwrap()
            .write_all(b"SYNTHETIC_PRIVATE_CREDENTIAL")
            .unwrap();
        let (origin, certificate, server) = status_response(status).await;
        let message = outbound_message();
        let ExecutionPortMessage::RuntimeEventMessage(event) = &message else {
            unreachable!()
        };
        let (mut port, _) = RemoteWorkerPort::open(
            &origin,
            &certificate,
            &credential,
            event.lease.worker_id.clone(),
            event.lease.worker_instance_id.clone(),
            Duration::from_secs(5),
        )
        .unwrap();
        let error = port.send(message).await.unwrap_err();
        let failure = match error {
            RemoteWorkerPortError::Network(failure)
            | RemoteWorkerPortError::NetworkExhausted(failure) => failure,
            other => panic!("HTTP {status} discarded its network facts: {other:?}"),
        };
        assert_eq!(failure.http_status, Some(status));
        assert_eq!(failure.kind, expected_kind);
        assert_eq!(failure.acceptance, Acceptance::ResponseReceived);
        assert_eq!(failure.phase, Phase::ResponseHeaders);
        let db = rusqlite::Connection::open_with_flags(
            directory.path().join("network-requests.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let persisted: String = db
            .query_row(
                "SELECT failure_json FROM network_request_attempts",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let restored: NetworkFailure = serde_json::from_str(&persisted).unwrap();
        assert_eq!(restored, failure);
        assert!(!persisted.contains("SYNTHETIC_PRIVATE"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn worker_body_read_failure_retains_received_response_and_journal() {
    use std::os::unix::fs::OpenOptionsExt;

    let directory = tempfile::tempdir().unwrap();
    let credential = directory.path().join("credential");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&credential)
        .unwrap()
        .write_all(b"SYNTHETIC_PRIVATE_CREDENTIAL")
        .unwrap();
    let (origin, certificate, server) = status_response(200).await;
    let message = outbound_message();
    let ExecutionPortMessage::RuntimeEventMessage(event) = &message else {
        unreachable!()
    };
    let (mut port, _) = RemoteWorkerPort::open(
        &origin,
        &certificate,
        &credential,
        event.lease.worker_id.clone(),
        event.lease.worker_instance_id.clone(),
        Duration::from_secs(5),
    )
    .unwrap();
    let error = port.send(message).await.unwrap_err();
    let failure = match error {
        RemoteWorkerPortError::Network(failure)
        | RemoteWorkerPortError::NetworkExhausted(failure) => failure,
        other => panic!("body interruption discarded network facts: {other:?}"),
    };
    assert_eq!(failure.acceptance, Acceptance::ResponseReceived);
    assert_eq!(failure.phase, Phase::ResponseBody);
    assert!(failure.diagnostic.is_some());
    let db = rusqlite::Connection::open_with_flags(
        directory.path().join("network-requests.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let persisted: String = db
        .query_row(
            "SELECT failure_json FROM network_request_attempts",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let restored: NetworkFailure = serde_json::from_str(&persisted).unwrap();
    assert_eq!(restored, failure);
    assert!(!persisted.contains("SYNTHETIC_PRIVATE"));
    server.await.unwrap();
}
