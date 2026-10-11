// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use winwincode_execution_port::{
    generated::{ExecutionPortMessage, LeaseWriteStatus},
    replay::{
        ReplayAuthority, ReplayDecision, ReplayFrame, ReplaySequence, ReplaySnapshot,
        ReplayStateMachine, ReplayStore, ReplayStreamKey,
    },
    typed_replay::{
        ReplayStreamKind, acknowledgement_from_message, frame_from_message, stream_key_from_message,
    },
};

const OPEN_PAYLOAD: &str = r#"{"tools":[{"kind":"FunctionCall","name":"read_file","namespace":"repo.fs"},{"kind":"CustomToolCall","name":"apply_patch","namespace":"repo.patch"}]}"#;
const OPEN_PAYLOAD_BASE64: &str = "eyJ0b29scyI6W3sia2luZCI6IkZ1bmN0aW9uQ2FsbCIsIm5hbWUiOiJyZWFkX2ZpbGUiLCJuYW1lc3BhY2UiOiJyZXBvLmZzIn0seyJraW5kIjoiQ3VzdG9tVG9vbENhbGwiLCJuYW1lIjoiYXBwbHlfcGF0Y2giLCJuYW1lc3BhY2UiOiJyZXBvLnBhdGNoIn1dfQ==";
const FUNCTION_PAYLOAD: &str = r#"{"arguments":"{\"path\":\"README.md\"}","callId":"call-read","kind":"FunctionCall","name":"read_file","namespace":"repo.fs","type":"tool_call"}"#;
const FUNCTION_PAYLOAD_BASE64: &str = "eyJhcmd1bWVudHMiOiJ7XCJwYXRoXCI6XCJSRUFETUUubWRcIn0iLCJjYWxsSWQiOiJjYWxsLXJlYWQiLCJraW5kIjoiRnVuY3Rpb25DYWxsIiwibmFtZSI6InJlYWRfZmlsZSIsIm5hbWVzcGFjZSI6InJlcG8uZnMiLCJ0eXBlIjoidG9vbF9jYWxsIn0=";
const CUSTOM_PAYLOAD: &str = r#"{"callId":"call-patch","input":"*** Begin Patch","kind":"CustomToolCall","name":"apply_patch","namespace":"repo.patch","type":"tool_call"}"#;
const CUSTOM_PAYLOAD_BASE64: &str = "eyJjYWxsSWQiOiJjYWxsLXBhdGNoIiwiaW5wdXQiOiIqKiogQmVnaW4gUGF0Y2giLCJraW5kIjoiQ3VzdG9tVG9vbENhbGwiLCJuYW1lIjoiYXBwbHlfcGF0Y2giLCJuYW1lc3BhY2UiOiJyZXBvLnBhdGNoIiwidHlwZSI6InRvb2xfY2FsbCJ9";
const CHANGED_NAMESPACE_PAYLOAD: &str = r#"{"arguments":"{\"path\":\"README.md\"}","callId":"call-read","kind":"FunctionCall","name":"read_file","namespace":"foreign.fs","type":"tool_call"}"#;
const CHANGED_NAMESPACE_PAYLOAD_BASE64: &str = "eyJhcmd1bWVudHMiOiJ7XCJwYXRoXCI6XCJSRUFETUUubWRcIn0iLCJjYWxsSWQiOiJjYWxsLXJlYWQiLCJraW5kIjoiRnVuY3Rpb25DYWxsIiwibmFtZSI6InJlYWRfZmlsZSIsIm5hbWVzcGFjZSI6ImZvcmVpZ24uZnMiLCJ0eXBlIjoidG9vbF9jYWxsIn0=";

fn fixture_value(kind: &str) -> Value {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("canonical ExecutionPort fixture");
    fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .unwrap_or_else(|| panic!("missing fixture message {kind}"))
        .clone()
}

fn digest(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

fn encoded_payload(payload: &str, base64: &str) -> Value {
    json!({
        "contentType": "application/json",
        "dataBase64": base64,
        "payloadDigest": digest(payload),
    })
}

fn model_open() -> ExecutionPortMessage {
    let mut message = fixture_value("model.open");
    message["request"] = encoded_payload(OPEN_PAYLOAD, OPEN_PAYLOAD_BASE64);
    serde_json::from_value(message).expect("namespaced model.open")
}

fn model_chunk(
    sequence: u64,
    message_id: &str,
    payload: &str,
    base64: &str,
) -> ExecutionPortMessage {
    let mut message = fixture_value("model.chunk");
    message["sequence"] = Value::from(sequence);
    message["messageId"] = Value::String(message_id.to_owned());
    message["payload"] = encoded_payload(payload, base64);
    serde_json::from_value(message).expect("namespaced model.chunk")
}

fn cancellation_ack() -> ExecutionPortMessage {
    let mut message = fixture_value("model.ack");
    message["ackSequence"] = Value::from(0);
    message["status"] = Value::String("rejected_conflict".to_owned());
    message["error"] = json!({
        "code": "CANCELLED",
        "message": "model exchange cancelled by Worker",
        "retryable": false,
    });
    serde_json::from_value(message).expect("cancellation model.ack")
}

fn encoded_payload_value(
    message: &ExecutionPortMessage,
) -> &winwincode_execution_port::generated::EncodedPayload {
    match message {
        ExecutionPortMessage::ModelOpenMessage(message) => &message.request,
        ExecutionPortMessage::ModelChunkMessage(message) => message
            .payload
            .as_ref()
            .expect("fixture model chunk carries a payload"),
        _ => panic!("fixture must carry an opaque model payload"),
    }
}

#[derive(Clone, Default)]
struct DurableRows(BTreeMap<ReplayStreamKey, ReplaySnapshot>);

#[derive(Clone, Default)]
struct DurableStore {
    rows: DurableRows,
    writes: usize,
}

impl DurableStore {
    fn restart(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            writes: 0,
        }
    }
}

impl ReplayStore for DurableStore {
    type Error = &'static str;

    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        Ok(self.rows.0.get(stream).cloned())
    }

    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_highest_sequence: ReplaySequence,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error> {
        let snapshot = self.rows.0.entry(stream.clone()).or_default();
        if snapshot.highest_sequence != expected_highest_sequence {
            return Err("durable replay cursor changed");
        }
        snapshot.events.push(frame.clone());
        snapshot.highest_sequence = expected_highest_sequence
            .checked_add(1)
            .ok_or("durable replay sequence overflow")?;
        self.writes += 1;
        Ok(())
    }
}

struct ActiveLease;

impl ReplayAuthority for ActiveLease {
    type Context = ();
    type Error = &'static str;

    fn validate_active_lease(
        &self,
        _stream: &ReplayStreamKey,
        _context: &Self::Context,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn model_open_and_chunks_preserve_namespaced_function_and_custom_tool_payloads() {
    let open = model_open();
    let function = model_chunk(
        1,
        "xmsg_0000000000000000000000000G",
        FUNCTION_PAYLOAD,
        FUNCTION_PAYLOAD_BASE64,
    );
    let custom = model_chunk(
        2,
        "xmsg_0000000000000000000000000J",
        CUSTOM_PAYLOAD,
        CUSTOM_PAYLOAD_BASE64,
    );
    for (message, expected_payload, expected_base64) in [
        (&open, OPEN_PAYLOAD, OPEN_PAYLOAD_BASE64),
        (&function, FUNCTION_PAYLOAD, FUNCTION_PAYLOAD_BASE64),
        (&custom, CUSTOM_PAYLOAD, CUSTOM_PAYLOAD_BASE64),
    ] {
        let payload = encoded_payload_value(message);
        assert_eq!(payload.data_base64, expected_base64);
        assert_eq!(payload.payload_digest.0, digest(expected_payload));
        let encoded = serde_json::to_vec(message).expect("encode generated model message");
        let decoded: ExecutionPortMessage =
            serde_json::from_slice(&encoded).expect("decode generated model message");
        assert_eq!(encoded_payload_value(&decoded), payload);
    }
    let open_stream = stream_key_from_message(&open).expect("map model.open stream");
    assert_eq!(open_stream.kind, ReplayStreamKind::Model);
    assert_eq!(
        stream_key_from_message(&function)
            .expect("map FunctionCall stream")
            .stream,
        open_stream.stream
    );
    assert_eq!(
        stream_key_from_message(&custom)
            .expect("map CustomToolCall stream")
            .stream,
        open_stream.stream
    );
}

#[test]
fn parallel_namespaced_tools_restart_in_order_and_changed_namespace_conflicts() {
    let first = frame_from_message(&model_chunk(
        1,
        "xmsg_0000000000000000000000000G",
        FUNCTION_PAYLOAD,
        FUNCTION_PAYLOAD_BASE64,
    ))
    .expect("map namespaced FunctionCall");
    let second = frame_from_message(&model_chunk(
        2,
        "xmsg_0000000000000000000000000J",
        CUSTOM_PAYLOAD,
        CUSTOM_PAYLOAD_BASE64,
    ))
    .expect("map namespaced CustomToolCall");
    assert_eq!(first.stream, second.stream);
    let machine = ReplayStateMachine::new();
    let mut store = DurableStore::default();
    assert_eq!(
        machine
            .accept(&mut store, &ActiveLease, &first.stream, &(), &first.frame)
            .expect("accept FunctionCall"),
        ReplayDecision::Accepted {
            highest_sequence: 1
        }
    );
    assert_eq!(
        machine
            .accept(&mut store, &ActiveLease, &second.stream, &(), &second.frame)
            .expect("accept CustomToolCall"),
        ReplayDecision::Accepted {
            highest_sequence: 2
        }
    );

    let changed = frame_from_message(&model_chunk(
        1,
        "xmsg_0000000000000000000000000G",
        CHANGED_NAMESPACE_PAYLOAD,
        CHANGED_NAMESPACE_PAYLOAD_BASE64,
    ))
    .expect("map changed namespace");
    assert_eq!(
        machine
            .accept(
                &mut store,
                &ActiveLease,
                &changed.stream,
                &(),
                &changed.frame,
            )
            .expect("changed namespace is a replay decision"),
        ReplayDecision::Conflict {
            highest_sequence: 2
        }
    );
    let duplicate = machine
        .accept(&mut store, &ActiveLease, &first.stream, &(), &first.frame)
        .expect("exact FunctionCall replay");
    assert!(matches!(duplicate, ReplayDecision::Duplicate { .. }));
    assert_eq!(store.writes, 2);

    let mut restarted = store.restart();
    let replay = machine
        .resume(&mut restarted, &ActiveLease, &first.stream, &(), 0, 10)
        .expect("restart namespaced tool replay");
    assert_eq!(replay.events, [first.frame.clone(), second.frame.clone()]);
    assert_eq!(restarted.writes, 0);
}

#[test]
fn cancellation_keeps_namespaced_tool_frames_byte_exact_for_restart_replay() {
    let first = frame_from_message(&model_chunk(
        1,
        "xmsg_0000000000000000000000000G",
        FUNCTION_PAYLOAD,
        FUNCTION_PAYLOAD_BASE64,
    ))
    .expect("map namespaced FunctionCall");
    let machine = ReplayStateMachine::new();
    let mut store = DurableStore::default();
    machine
        .accept(&mut store, &ActiveLease, &first.stream, &(), &first.frame)
        .expect("accept namespaced tool frame");

    let cancellation = cancellation_ack();
    let mapped = acknowledgement_from_message(&cancellation).expect("map cancellation ack");
    assert_eq!(mapped.stream, first.stream);
    assert_eq!(
        mapped.acknowledgement.status,
        winwincode_execution_port::replay::ReplayAcknowledgementStatus::RejectedConflict
    );
    assert_eq!(
        serde_json::to_value(&cancellation).expect("encode cancellation ack")["error"]["code"],
        "CANCELLED"
    );
    assert_eq!(
        match &cancellation {
            ExecutionPortMessage::ModelAckMessage(message) => message.status.clone(),
            _ => panic!("fixture must be model.ack"),
        },
        LeaseWriteStatus::RejectedConflict
    );

    let mut restarted = store.restart();
    let replay = machine
        .resume(&mut restarted, &ActiveLease, &first.stream, &(), 0, 10)
        .expect("replay after cancellation");
    assert_eq!(replay.events, [first.frame]);
    assert_eq!(restarted.writes, 0);
}
