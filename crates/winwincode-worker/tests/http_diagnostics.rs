// SPDX-License-Identifier: Apache-2.0

use std::io::Cursor;

use winwincode_client_port::worker_relay::WorkerRelayResponse;
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_network::{Acceptance, ErrorKind, Phase};
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

#[tokio::test]
async fn worker_retains_the_status_forwarded_by_device() {
    for (status, expected_kind) in [
        (204, ErrorKind::RequestInvalid),
        (400, ErrorKind::RequestInvalid),
        (401, ErrorKind::Authentication),
        (403, ErrorKind::Authorization),
        (413, ErrorKind::RequestInvalid),
        (429, ErrorKind::RateLimited),
        (503, ErrorKind::ServerTransient),
    ] {
        let message = outbound_message();
        let ExecutionPortMessage::RuntimeEventMessage(event) = &message else {
            unreachable!()
        };
        let mut response = serde_json::to_vec(&WorkerRelayResponse {
            status,
            body: "SYNTHETIC_PRIVATE_RESPONSE".into(),
        })
        .unwrap();
        response.push(b'\n');
        let (mut port, _) = RemoteWorkerPort::with_io(
            event.lease.worker_id.clone(),
            event.lease.worker_instance_id.clone(),
            Cursor::new(response),
            Vec::new(),
        );
        let error = port.send(message).await.unwrap_err();
        let failure = match error {
            RemoteWorkerPortError::Network(failure)
            | RemoteWorkerPortError::NetworkExhausted(failure) => failure,
            other => panic!("forwarded HTTP {status} discarded its network facts: {other:?}"),
        };
        assert_eq!(failure.http_status, Some(status));
        assert_eq!(failure.kind, expected_kind);
        assert_eq!(failure.acceptance, Acceptance::ResponseReceived);
        assert_eq!(failure.phase, Phase::ResponseHeaders);
        assert!(!failure.to_string().contains("SYNTHETIC_PRIVATE"));
    }
}

#[tokio::test]
async fn device_pipe_closure_stops_worker_delivery() {
    let message = outbound_message();
    let ExecutionPortMessage::RuntimeEventMessage(event) = &message else {
        unreachable!()
    };
    let (mut port, handle) = RemoteWorkerPort::with_io(
        event.lease.worker_id.clone(),
        event.lease.worker_instance_id.clone(),
        Cursor::new(Vec::<u8>::new()),
        Vec::new(),
    );
    assert_eq!(
        port.send(message).await,
        Err(RemoteWorkerPortError::Transport)
    );
    assert_eq!(
        handle.terminal_error(),
        Some(RemoteWorkerPortError::Transport)
    );
}
