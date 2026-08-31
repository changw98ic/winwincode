// SPDX-License-Identifier: Apache-2.0

//! Contract tests for the business-neutral local/remote `ExecutionPort` seam.
//!
//! The tests freeze the seam: both adapters accept the same typed frame, the
//! remote adapter uses one canonical JSON envelope, and neither adapter
//! interprets lease, dedupe, or execution outcomes.

use std::collections::VecDeque;

use serde_json::{Value, json};
use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_execution_port::transport::{
    AdapterError, EndpointSide, ExecutionPortCore, FrameDirection, FrameError, LocalWorkerAdapter,
    RemoteExchangeDelivery, RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter,
    TypedFrame, execution_message_id,
};

const VALID_FIXTURE: &str =
    include_str!("../../../tests/fixtures/contracts/execution-port.valid.json");
const PRODUCT_SESSION_BINDING_FIXTURE: &str =
    include_str!("../../../tests/fixtures/contracts/session-binding.product-session.valid.json");
const DELIVERY_STAGE_BINDING_FIXTURE: &str =
    include_str!("../../../tests/fixtures/contracts/session-binding.delivery-stage.valid.json");

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScriptedOutcome {
    Accepted,
    Duplicate,
    Conflict,
    Gap,
    Expired,
    Stale,
    Reacquire,
}

impl ScriptedOutcome {
    fn is_error(&self) -> bool {
        !matches!(self, Self::Accepted | Self::Duplicate)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScriptedCore {
    outcomes: VecDeque<ScriptedOutcome>,
    seen: Vec<String>,
}

impl ScriptedCore {
    fn new(outcomes: impl IntoIterator<Item = ScriptedOutcome>) -> Self {
        Self {
            outcomes: outcomes.into_iter().collect(),
            seen: Vec::new(),
        }
    }
}

impl ExecutionPortCore for ScriptedCore {
    type Error = ScriptedOutcome;
    type Output = ScriptedOutcome;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        let value = serde_json::to_value(message).expect("generated message is serializable");
        self.seen.push(
            value
                .get("kind")
                .and_then(Value::as_str)
                .expect("generated message has a kind")
                .to_owned(),
        );
        let outcome = self
            .outcomes
            .pop_front()
            .expect("scripted outcome is present");
        if outcome.is_error() {
            Err(outcome)
        } else {
            Ok(outcome)
        }
    }
}

fn fixture_messages() -> Vec<(FrameDirection, ExecutionPortMessage)> {
    let fixture: Value = serde_json::from_str(VALID_FIXTURE).expect("valid fixture JSON");
    fixture["messages"]
        .as_array()
        .expect("valid fixture messages array")
        .iter()
        .map(|value| {
            let message: ExecutionPortMessage =
                serde_json::from_value(value.clone()).expect("valid generated message");
            let direction = FrameDirection::for_message(&message).expect("known message kind");
            (direction, message)
        })
        .collect()
}

fn worker_frame() -> TypedFrame {
    let (direction, message) = fixture_messages()
        .into_iter()
        .find(|(direction, _)| *direction == FrameDirection::WorkerToControlPlane)
        .expect("worker-to-control-plane fixture message");
    TypedFrame::new(direction, message).expect("fixture frame is canonical")
}

#[test]
fn all_canonical_fixture_messages_round_trip_through_remote_json() {
    let messages = fixture_messages();
    assert_eq!(messages.len(), 28);

    for (direction, message) in messages {
        let frame = TypedFrame::new(direction, message).expect("typed frame is valid");
        let encoded = RemoteTransportAdapter::<ScriptedCore>::encode(&frame)
            .expect("canonical frame encoding");
        let decoded = RemoteTransportAdapter::<ScriptedCore>::decode(&encoded)
            .expect("canonical frame decoding");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn remote_exchange_round_trips_bounded_canonical_frames_and_exact_delivery_ids() {
    let worker = worker_frame();
    let worker_bytes =
        RemoteTransportAdapter::<ScriptedCore>::encode(&worker).expect("canonical Worker frame");
    let request = RemoteExchangeRequest::new(
        winwincode_domain::WorkerId("wrk_00000000000000000000000001".to_owned()),
        winwincode_domain::WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
        Vec::new(),
        worker_bytes,
    )
    .expect("bounded request");
    let decoded = RemoteExchangeRequest::decode(&request.encode().expect("request encode"))
        .expect("request decode");
    assert_eq!(decoded, request);

    let (direction, message) = fixture_messages()
        .into_iter()
        .find(|(direction, _)| *direction == FrameDirection::ControlPlaneToWorker)
        .expect("Control Plane fixture");
    let id = execution_message_id(&message).expect("message identity");
    let frame = TypedFrame::new(direction, message).expect("typed response frame");
    let response = RemoteExchangeResponse::new(vec![RemoteExchangeDelivery {
        delivery_id: id,
        frame: RemoteTransportAdapter::<ScriptedCore>::encode(&frame)
            .expect("canonical response frame"),
    }])
    .expect("bounded response");
    let decoded = RemoteExchangeResponse::decode(&response.encode().expect("response encode"))
        .expect("response decode");
    assert_eq!(decoded, response);
}

#[test]
fn product_session_and_delivery_stage_bindings_round_trip_identically_locally_and_remotely() {
    for (fixture, expected_stage_run) in [
        (PRODUCT_SESSION_BINDING_FIXTURE, None),
        (
            DELIVERY_STAGE_BINDING_FIXTURE,
            Some("run_00000000000000000000000009"),
        ),
    ] {
        let message: ExecutionPortMessage =
            serde_json::from_str(fixture).expect("canonical SessionBinding fixture");
        let ExecutionPortMessage::SessionBindingMessage(binding) = &message else {
            panic!("fixture must decode as SessionBindingMessage");
        };
        assert_eq!(
            binding.stage_run_id.as_ref().map(|id| id.0.as_str()),
            expected_stage_run
        );
        assert_eq!(
            binding
                .session_identity
                .stage_run_id
                .as_ref()
                .map(|id| id.0.as_str()),
            expected_stage_run,
        );

        let frame = TypedFrame::new(FrameDirection::WorkerToControlPlane, message)
            .expect("SessionBinding direction");
        let encoded = RemoteTransportAdapter::<ScriptedCore>::encode(&frame)
            .expect("canonical remote SessionBinding frame");
        let decoded = RemoteTransportAdapter::<ScriptedCore>::decode(&encoded)
            .expect("canonical remote SessionBinding decoding");
        assert_eq!(decoded, frame);

        let mut local_core = ScriptedCore::new([ScriptedOutcome::Accepted]);
        let mut local = LocalWorkerAdapter::new(&mut local_core, EndpointSide::ControlPlane);
        let local_result = local.accept(&frame);

        let mut remote_core = ScriptedCore::new([ScriptedOutcome::Accepted]);
        let mut remote = RemoteTransportAdapter::new(&mut remote_core, EndpointSide::ControlPlane);
        let remote_result = remote.accept(&encoded);

        assert_eq!(local_result, remote_result);
        assert_eq!(local_core.seen, remote_core.seen);
        assert_eq!(local_core.seen, ["session.binding"]);
    }
}

#[test]
fn local_and_remote_adapters_have_value_identical_scripted_outcomes() {
    let outcomes = [
        ScriptedOutcome::Accepted,
        ScriptedOutcome::Duplicate,
        ScriptedOutcome::Conflict,
        ScriptedOutcome::Gap,
        ScriptedOutcome::Expired,
        ScriptedOutcome::Stale,
        ScriptedOutcome::Reacquire,
    ];
    for expected in outcomes {
        let frame = worker_frame();
        let remote_bytes = RemoteTransportAdapter::<ScriptedCore>::encode(&frame)
            .expect("canonical frame encoding");

        let mut local_core = ScriptedCore::new([expected.clone()]);
        let mut local = LocalWorkerAdapter::new(&mut local_core, EndpointSide::ControlPlane);
        let local_result = local.accept(&frame);

        let mut remote_core = ScriptedCore::new([expected.clone()]);
        let mut remote = RemoteTransportAdapter::new(&mut remote_core, EndpointSide::ControlPlane);
        let remote_result = remote.accept(&remote_bytes);

        assert_eq!(
            local_result, remote_result,
            "outcome parity for {expected:?}"
        );
        assert_eq!(local_core.seen, remote_core.seen);
        assert_eq!(local_core.seen.len(), 1);
    }
}

#[test]
fn local_adapter_rejects_a_frame_for_the_other_endpoint_before_core() {
    let frame = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        fixture_messages()
            .into_iter()
            .find(|(direction, _)| *direction == FrameDirection::ControlPlaneToWorker)
            .expect("control-plane-to-worker fixture message")
            .1,
    )
    .expect("typed frame is valid");
    let mut core = ScriptedCore::new([ScriptedOutcome::Accepted]);
    let mut local = LocalWorkerAdapter::new(&mut core, EndpointSide::ControlPlane);

    assert!(matches!(
        local.accept(&frame),
        Err(AdapterError::Frame(FrameError::DirectionMismatch { .. }))
    ));
    assert!(core.seen.is_empty());
}

#[test]
fn remote_adapter_rejects_wrong_direction_unknown_fields_and_error_frames() {
    let frame = worker_frame();
    let encoded =
        RemoteTransportAdapter::<ScriptedCore>::encode(&frame).expect("canonical frame encoding");
    let mut wrong_direction: Value = serde_json::from_slice(&encoded).expect("frame JSON");
    wrong_direction["direction"] = json!("control-plane-to-worker");

    let mut core = ScriptedCore::new([ScriptedOutcome::Accepted]);
    let mut remote = RemoteTransportAdapter::new(&mut core, EndpointSide::ControlPlane);
    assert!(matches!(
        remote.accept(&serde_json::to_vec(&wrong_direction).expect("wrong direction JSON")),
        Err(AdapterError::Frame(FrameError::DirectionMismatch { .. }))
    ));

    let mut unknown: Value = serde_json::from_slice(&encoded).expect("frame JSON");
    unknown["unexpected"] = json!(true);
    assert!(matches!(
        remote.accept(&serde_json::to_vec(&unknown).expect("unknown field JSON")),
        Err(AdapterError::Frame(FrameError::Malformed(_)))
    ));

    let error_frame = json!({
        "frameType": "error",
        "direction": "worker-to-control-plane",
        "error": {
            "code": "INFRASTRUCTURE_ERROR",
            "message": "fixture error",
            "retryable": true
        }
    });
    assert!(matches!(
        remote.accept(&serde_json::to_vec(&error_frame).expect("error frame JSON")),
        Err(AdapterError::Frame(FrameError::ErrorFrame))
    ));
    assert!(core.seen.is_empty());
}
