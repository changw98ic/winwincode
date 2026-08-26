use serde_json::Value;

use winwincode_execution_port::generated::ExecutionPortMessage;
use winwincode_execution_port::typed_replay::{
    ReplayStreamKind, acknowledgement_from_message, frame_from_message, stream_key_from_message,
};

fn fixture_message(kind: &str) -> ExecutionPortMessage {
    let fixture = fixture_value(kind);
    serde_json::from_value(fixture).expect("generated fixture message")
}

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

#[test]
fn runtime_artifact_and_model_chunks_map_to_one_replay_frame_shape() {
    let cases = [
        (
            "runtime.event",
            ReplayStreamKind::Runtime,
            "xevt_0000000000000000000000000B",
        ),
        (
            "artifact.chunk",
            ReplayStreamKind::Artifact,
            "xmsg_0000000000000000000000000D",
        ),
        (
            "model.chunk",
            ReplayStreamKind::Model,
            "xmsg_0000000000000000000000000G",
        ),
    ];

    for (kind, expected_stream_kind, expected_event_id) in cases {
        let mapped = frame_from_message(&fixture_message(kind))
            .unwrap_or_else(|error| panic!("{kind} maps to replay frame: {error:?}"));
        assert_eq!(mapped.kind, expected_stream_kind);
        assert_eq!(mapped.frame.event_id, expected_event_id);
        assert_eq!(mapped.frame.sequence, 1);
        let expected_digest = match kind {
            "runtime.event" => {
                "sha256:2749ae4e612d3fa1eccb3b67ddcdc763a4c0b7dcc66fa4750841dec273efeb7f"
            }
            "artifact.chunk" => {
                "sha256:8337cee3c579e17ac238effde1c69d408c92fe555582a08e77e079622338218f"
            }
            "model.chunk" => {
                "sha256:df0c732d96ee0c394404992c17469ffc8d4f453ed2a436088ad47254d34b0317"
            }
            _ => unreachable!("covered above"),
        };
        assert_eq!(mapped.frame.digest, expected_digest);
        assert!(!mapped.frame.frame.is_empty());
        assert!(!mapped.stream.as_str().is_empty());
    }
}

#[test]
fn each_stream_control_and_frame_uses_the_same_canonical_key() {
    let groups = [
        ["runtime.event", "runtime.ack", "runtime.replay_request"],
        ["artifact.open", "artifact.chunk", "artifact.ack"],
        ["model.open", "model.chunk", "model.ack"],
    ];

    for group in groups {
        let keys = group.map(|kind| {
            stream_key_from_message(&fixture_message(kind))
                .unwrap_or_else(|error| panic!("{kind} has a stream key: {error:?}"))
                .stream
        });
        assert_eq!(keys[0], keys[1], "open and chunk must address one stream");
        assert_eq!(keys[1], keys[2], "chunk and ack must address one stream");
    }
}

#[test]
fn gap_acknowledgement_uses_the_generic_ack_rule_for_every_stream() {
    for kind in ["runtime.ack", "artifact.ack", "model.ack"] {
        let mut message = fixture_value(kind);
        message["status"] = Value::String("gap".to_owned());
        message["replayFromSequence"] = Value::from(2);
        let mapped = acknowledgement_from_message(
            &serde_json::from_value::<ExecutionPortMessage>(message).expect("gap fixture message"),
        )
        .unwrap_or_else(|error| panic!("{kind} gap maps: {error:?}"));
        assert_eq!(
            mapped.acknowledgement.status,
            winwincode_execution_port::replay::ReplayAcknowledgementStatus::Gap
        );
        assert_eq!(mapped.acknowledgement.ack_sequence, 1);
        assert_eq!(mapped.acknowledgement.replay_from_sequence, Some(2));
    }
}

#[test]
fn artifact_and_model_frame_fields_guard_the_generic_identity_and_sequence() {
    let mut artifact = fixture_value("artifact.chunk");
    artifact["messageId"] = Value::String(String::new());
    assert_eq!(
        frame_from_message(
            &serde_json::from_value::<ExecutionPortMessage>(artifact)
                .expect("artifact fixture message"),
        )
        .expect_err("empty artifact message id is not a replay identity"),
        winwincode_execution_port::typed_replay::TypedReplayMappingError::EmptyIdentity
    );

    let mut model = fixture_value("model.chunk");
    model["sequence"] = Value::from(0);
    assert_eq!(
        frame_from_message(
            &serde_json::from_value::<ExecutionPortMessage>(model).expect("model fixture message"),
        )
        .expect_err("zero model sequence is not a replay sequence"),
        winwincode_execution_port::typed_replay::TypedReplayMappingError::InvalidSequence
    );
}

#[test]
fn runtime_artifact_and_model_acknowledgements_map_to_one_ack_shape() {
    let cases = [
        ("runtime.ack", ReplayStreamKind::Runtime),
        ("artifact.ack", ReplayStreamKind::Artifact),
        ("model.ack", ReplayStreamKind::Model),
    ];

    for (kind, expected_stream_kind) in cases {
        let mapped = acknowledgement_from_message(&fixture_message(kind))
            .unwrap_or_else(|error| panic!("{kind} maps to replay ack: {error:?}"));
        assert_eq!(mapped.kind, expected_stream_kind);
        assert_eq!(mapped.acknowledgement.ack_sequence, 1);
        assert_eq!(
            mapped.acknowledgement.status,
            winwincode_execution_port::replay::ReplayAcknowledgementStatus::Accepted
        );
        assert_eq!(mapped.acknowledgement.replay_from_sequence, None);
        assert!(!mapped.stream.as_str().is_empty());
    }
}

#[test]
fn unsequenced_control_messages_are_not_silently_stored_as_replay_frames() {
    for kind in [
        "runtime.replay_request",
        "runtime.ack",
        "artifact.open",
        "artifact.ack",
        "model.open",
        "model.ack",
    ] {
        let error = frame_from_message(&fixture_message(kind))
            .expect_err("open is a stream declaration, not a sequenced frame");
        assert_eq!(
            error,
            winwincode_execution_port::typed_replay::TypedReplayMappingError::UnsequencedMessage
        );
    }
}
