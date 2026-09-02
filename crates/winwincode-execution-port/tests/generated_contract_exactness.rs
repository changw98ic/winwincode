// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, from_value};
use winwincode_execution_port::generated::{
    ActionEnforcementDecision, ActionPolicyKind, ActionPolicyMode, ApprovalDecisionMessageDecision,
    ApprovalDecisionMessageScope, ExecutionPortMessage, InputResponseMessageStatus,
    JobCancelAckMessageStatus, JobCancelMessageReason, JobDispatchResultMessageStatus,
    JobOutcomeAckMessageStatus, WorkerCapabilitySetPlatform, WorkerHeartbeatAckMessageStatus,
    WorkerRegistrationResultMessageLeaseRecovery, WorkerRegistrationResultMessageStatus,
};

fn valid_messages() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("canonical ExecutionPort fixture");
    fixture["messages"]
        .as_array()
        .expect("ExecutionPort messages")
        .clone()
}

#[test]
fn every_canonical_execution_port_message_round_trips_through_the_shared_crate() {
    let messages = valid_messages();
    assert_eq!(messages.len(), 28);

    for message in messages {
        let kind = message["kind"].as_str().expect("message kind");
        let decoded: ExecutionPortMessage = from_value(message.clone())
            .unwrap_or_else(|error| panic!("{kind} must decode: {error}"));
        let encoded = serde_json::to_value(decoded).expect("ExecutionPort message encoding");
        assert_eq!(encoded, message, "{kind} must preserve its canonical shape");
    }
}

#[test]
fn execution_port_messages_reject_unknown_fields_at_the_shared_boundary() {
    for mut message in valid_messages() {
        let kind = message["kind"].as_str().expect("message kind").to_owned();
        message.as_object_mut().expect("message object").insert(
            "unknownField".to_owned(),
            Value::String("reject".to_owned()),
        );
        assert!(
            from_value::<ExecutionPortMessage>(message).is_err(),
            "{kind} must reject unknown fields"
        );
    }
}

#[test]
fn execution_port_inline_string_enums_reject_unknown_values() {
    let cases = [
        ("worker.register", "capabilities.platform"),
        ("worker.capabilities", "capabilities.platform"),
        ("worker.registration_result", "status"),
        ("worker.registration_result", "leaseRecovery"),
        ("worker.heartbeat_ack", "status"),
        ("job.dispatch_result", "status"),
        ("input.response", "status"),
        ("approval.decision", "decision"),
        ("approval.decision", "scope"),
        ("action.enforcement_request", "policyKind"),
        ("action.enforcement_receipt", "policyKind"),
        ("action.enforcement_receipt", "decision"),
        ("action.enforcement_receipt", "policyMode"),
        ("job.cancel", "reason"),
        ("job.cancel_ack", "status"),
        ("job.outcome_ack", "status"),
    ];

    // Keep the enum types in the public generated surface, rather than allowing
    // an inline schema enum to silently regress to an unconstrained String.
    let _: fn(WorkerCapabilitySetPlatform) = |_| {};
    let _: fn(WorkerRegistrationResultMessageStatus) = |_| {};
    let _: fn(WorkerRegistrationResultMessageLeaseRecovery) = |_| {};
    let _: fn(WorkerHeartbeatAckMessageStatus) = |_| {};
    let _: fn(JobDispatchResultMessageStatus) = |_| {};
    let _: fn(InputResponseMessageStatus) = |_| {};
    let _: fn(ApprovalDecisionMessageDecision) = |_| {};
    let _: fn(ApprovalDecisionMessageScope) = |_| {};
    let _: fn(ActionPolicyKind) = |_| {};
    let _: fn(ActionEnforcementDecision) = |_| {};
    let _: fn(ActionPolicyMode) = |_| {};
    let _: fn(JobCancelMessageReason) = |_| {};
    let _: fn(JobCancelAckMessageStatus) = |_| {};
    let _: fn(JobOutcomeAckMessageStatus) = |_| {};

    let fixture = valid_messages();
    for (kind, field) in cases {
        let mut message = fixture
            .iter()
            .find(|message| message["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {kind} fixture"))
            .clone();
        if field == "capabilities.platform" {
            message["capabilities"]["platform"] = Value::String("unsupported-platform".to_owned());
        } else {
            message[field] = Value::String("unsupported-value".to_owned());
        }
        assert!(
            from_value::<ExecutionPortMessage>(message).is_err(),
            "{kind}.{field} must reject an unknown enum value"
        );
    }
}
