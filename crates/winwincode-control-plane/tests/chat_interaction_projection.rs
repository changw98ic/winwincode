// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use winwincode_api::generated::{
    ApprovalDecidePayload, ApprovalEffectiveDecisionScope, ApprovalProjectionCategory,
    ApprovalSanitizedDetailProjectionKind, ApprovalSanitizedDetailUnavailableReason,
    ChatInteractionListQuery, ChatInteractionProjection, InputRespondPayload,
};
use winwincode_control_plane::chat_interaction_projection::{
    ChatInteractionProjectionError, ChatInteractionProjectionLedger, ProjectionWriteStatus,
};
use winwincode_domain::{
    Instant, InteractiveInputChoiceId, InteractiveInputValue, ProductSessionId, Revision,
};
use winwincode_execution_port::generated::{
    ApprovalRequestMessage, EncodedPayload, InputRequestMessage,
};

fn worker_messages() -> (InputRequestMessage, ApprovalRequestMessage) {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("valid ExecutionPort fixture");
    let messages = fixture["messages"]
        .as_array()
        .expect("message fixture array");
    let input = messages
        .iter()
        .find(|message| message["kind"] == "input.request")
        .cloned()
        .expect("input request fixture");
    let approval = messages
        .iter()
        .find(|message| message["kind"] == "approval.request")
        .cloned()
        .expect("approval request fixture");
    (
        serde_json::from_value(input).expect("typed input request"),
        serde_json::from_value(approval).expect("typed approval request"),
    )
}

fn query(product_session_id: &ProductSessionId, states: &[&str]) -> ChatInteractionListQuery {
    serde_json::from_value(json!({
        "schemaVersion": "winwincode/v1",
        "requestId": "req_0000000000000000000000000Z",
        "query": "session.interactions.list",
        "actor": { "kind": "user", "id": "usr_00000000000000000000000001" },
        "scope": {
            "kind": "repository",
            "organizationId": "org_00000000000000000000000001",
            "workspaceId": "wsp_00000000000000000000000001",
            "projectId": "prj_00000000000000000000000001",
            "repositoryId": "rep_00000000000000000000000001"
        },
        "parameters": {
            "productSessionId": product_session_id,
            "states": states
        },
        "page": { "cursor": null, "limit": 50 }
    }))
    .expect("typed interaction query")
}

#[test]
fn restart_rebuilds_pending_input_and_approval_without_private_details() {
    let (input, mut approval) = worker_messages();
    approval.action.details = Some(EncodedPayload {
        content_type: "application/json".to_owned(),
        data_base64: "PRIVATE_PAYLOAD".to_owned(),
        payload_digest: winwincode_domain::Sha256Digest(format!("sha256:{}", "a".repeat(64))),
    });
    let mut ledger = ChatInteractionProjectionLedger::default();
    assert_eq!(
        ledger
            .record_input_request(&input)
            .expect("record input")
            .status,
        ProjectionWriteStatus::Applied
    );
    assert_eq!(
        ledger
            .record_approval_request(&approval)
            .expect("record approval")
            .status,
        ProjectionWriteStatus::Applied
    );
    assert_eq!(
        ledger
            .record_input_request(&input)
            .expect("idempotent Worker replay")
            .status,
        ProjectionWriteStatus::Duplicate
    );
    let mut conflicting_approval = approval.clone();
    conflicting_approval
        .action
        .details
        .as_mut()
        .expect("private details")
        .data_base64 = "CHANGED_PRIVATE_PAYLOAD".to_owned();
    assert_eq!(
        ledger.record_approval_request(&conflicting_approval),
        Err(ChatInteractionProjectionError::SourceMessageConflict)
    );

    let serialized = serde_json::to_string(&ledger.snapshot()).expect("safe snapshot JSON");
    assert!(!serialized.contains("PRIVATE_PAYLOAD"));
    assert!(!serialized.contains("data_base64"));
    assert!(!serialized.contains("details"));
    let restored = ChatInteractionProjectionLedger::restore(
        serde_json::from_str(&serialized).expect("durable safe snapshot"),
    )
    .expect("Control Plane restart");
    let response = restored
        .query(
            &query(&input.session_identity.product_session_id, &["pending"]),
            &Instant("2026-08-24T12:01:00.000Z".to_owned()),
        )
        .expect("HTTP interaction snapshot");
    assert_eq!(response.result.items.len(), 2);
    for item in response.result.items {
        let binding = match item {
            ChatInteractionProjection::ChatInputInteractionProjection(value) => value.binding,
            ChatInteractionProjection::ChatApprovalInteractionProjection(value) => {
                assert_eq!(value.approval.subject, "Run the approved test command.");
                assert_eq!(value.approval.category, ApprovalProjectionCategory::Shell);
                assert_eq!(
                    value.approval.effective_decision_scope,
                    ApprovalEffectiveDecisionScope::Once
                );
                assert_eq!(
                    value.approval.sanitized_detail.kind,
                    ApprovalSanitizedDetailProjectionKind::Unavailable
                );
                assert_eq!(
                    value.approval.sanitized_detail.reason,
                    ApprovalSanitizedDetailUnavailableReason::EncodedPayloadRedacted
                );
                value.approval.binding
            }
        };
        assert_eq!(
            binding.product_session_id,
            input.session_identity.product_session_id
        );
        assert_eq!(binding.execution_job_id, input.lease.job_id);
        assert_eq!(binding.worker_session_id, input.worker_session_id);
        assert_eq!(binding.session_identity, input.session_identity);
    }
}

#[test]
fn approval_without_typed_producer_detail_is_explicitly_unavailable() {
    let (_, approval) = worker_messages();
    assert!(approval.action.details.is_none());
    let mut ledger = ChatInteractionProjectionLedger::default();
    ledger
        .record_approval_request(&approval)
        .expect("record approval without detail");
    let projection = ledger
        .approval(
            &approval.approval_id,
            &Instant("2026-08-24T12:01:00.000Z".to_owned()),
        )
        .expect("read approval")
        .expect("approval projection");
    assert_eq!(projection.category, ApprovalProjectionCategory::Shell);
    assert_eq!(
        projection.sanitized_detail.reason,
        ApprovalSanitizedDetailUnavailableReason::ProducerUnavailable
    );
}

#[test]
fn duplicate_choice_values_keep_stable_ids_and_only_canonical_values_are_accepted() {
    let (mut input, _) = worker_messages();
    let choices = input.choices.as_mut().expect("choice input fixture");
    let mut repeated = choices[0].clone();
    repeated.id = InteractiveInputChoiceId("ich_00000000000000000000000002".to_owned());
    choices.push(repeated);

    let mut ledger = ChatInteractionProjectionLedger::default();
    let receipt = ledger.record_input_request(&input).expect("record input");
    let snapshot = ledger.snapshot();
    let mut restored = ChatInteractionProjectionLedger::restore(snapshot.clone())
        .expect("restore stable choice identities");
    let response = restored
        .query(
            &query(&input.session_identity.product_session_id, &["pending"]),
            &Instant("2026-08-24T12:01:00.000Z".to_owned()),
        )
        .expect("query restored input");
    let projection = response
        .result
        .items
        .iter()
        .find_map(|item| match item {
            ChatInteractionProjection::ChatInputInteractionProjection(value) => Some(value),
            ChatInteractionProjection::ChatApprovalInteractionProjection(_) => None,
        })
        .expect("input projection");
    assert_eq!(projection.options.len(), 2);
    assert_ne!(projection.options[0].id, projection.options[1].id);
    assert_eq!(projection.options[0].value, projection.options[1].value);
    assert_eq!(restored.snapshot(), snapshot);

    let mut payload = InputRespondPayload {
        execution_job_id: projection.binding.execution_job_id.clone(),
        input_request_id: projection.input_request_id.clone(),
        product_session_id: projection.binding.product_session_id.clone(),
        session_identity: projection.binding.session_identity.clone(),
        status: "provided".to_owned(),
        value: Some(InteractiveInputValue {
            mode: projection.mode.clone(),
            value: "forged-choice-value".to_owned(),
        }),
        worker_session_id: projection.binding.worker_session_id.clone(),
    };
    assert_eq!(
        restored.apply_input_response(
            &receipt.revision,
            &payload,
            &Instant("2026-08-24T12:01:00.000Z".to_owned()),
        ),
        Err(ChatInteractionProjectionError::InvalidField("value.value"))
    );
    payload.value.as_mut().expect("provided value").value = projection.options[1].value.clone();
    restored
        .apply_input_response(
            &receipt.revision,
            &payload,
            &Instant("2026-08-24T12:01:00.000Z".to_owned()),
        )
        .expect("accept canonical repeated choice value");

    let mut duplicate_id = input;
    let first_id = duplicate_id.choices.as_ref().expect("choice input fixture")[0]
        .id
        .clone();
    duplicate_id.choices.as_mut().expect("choice input fixture")[1].id = first_id;
    assert_eq!(
        ChatInteractionProjectionLedger::default().record_input_request(&duplicate_id),
        Err(ChatInteractionProjectionError::InvalidField("choices"))
    );
}

#[test]
fn responses_require_exact_session_worker_job_identity_revision_and_expiry() {
    let (input, approval) = worker_messages();
    let mut ledger = ChatInteractionProjectionLedger::default();
    let input_receipt = ledger.record_input_request(&input).expect("record input");
    let approval_receipt = ledger
        .record_approval_request(&approval)
        .expect("record approval");
    let pending = ledger
        .query(
            &query(&input.session_identity.product_session_id, &["pending"]),
            &Instant("2026-08-24T12:01:00.000Z".to_owned()),
        )
        .expect("pending query");
    let input_projection = pending
        .result
        .items
        .iter()
        .find_map(|item| match item {
            ChatInteractionProjection::ChatInputInteractionProjection(value) => Some(value),
            ChatInteractionProjection::ChatApprovalInteractionProjection(_) => None,
        })
        .expect("input projection");
    let approval_projection = pending
        .result
        .items
        .iter()
        .find_map(|item| match item {
            ChatInteractionProjection::ChatApprovalInteractionProjection(value) => {
                Some(&value.approval)
            }
            ChatInteractionProjection::ChatInputInteractionProjection(_) => None,
        })
        .expect("approval projection");
    let input_payload = InputRespondPayload {
        execution_job_id: input_projection.binding.execution_job_id.clone(),
        input_request_id: input_projection.input_request_id.clone(),
        product_session_id: input_projection.binding.product_session_id.clone(),
        session_identity: input_projection.binding.session_identity.clone(),
        status: "provided".to_owned(),
        value: Some(InteractiveInputValue {
            mode: input_projection.mode.clone(),
            value: "candidate".to_owned(),
        }),
        worker_session_id: input_projection.binding.worker_session_id.clone(),
    };
    let mut foreign = input_payload.clone();
    foreign.product_session_id = ProductSessionId("psn_0000000000000000000000000Y".to_owned());
    assert_eq!(
        ledger.apply_input_response(
            &input_receipt.revision,
            &foreign,
            &Instant("2026-08-24T12:01:00.000Z".to_owned())
        ),
        Err(ChatInteractionProjectionError::BindingMismatch(
            "productSessionId"
        ))
    );
    assert_eq!(
        ledger.apply_input_response(
            &Revision(input_receipt.revision.0 + 1),
            &input_payload,
            &Instant("2026-08-24T12:01:00.000Z".to_owned())
        ),
        Err(ChatInteractionProjectionError::RevisionConflict {
            expected: Revision(input_receipt.revision.0 + 1),
            actual: input_receipt.revision.clone()
        })
    );
    assert_eq!(
        ledger.apply_input_response(&input_receipt.revision, &input_payload, &input.expires_at),
        Err(ChatInteractionProjectionError::Expired)
    );

    let approval_payload = ApprovalDecidePayload {
        approval_id: approval_projection.id.clone(),
        binding: approval_projection.binding.clone(),
        decision: "approve".to_owned(),
        reason: "Reviewed.".to_owned(),
    };
    ledger
        .apply_approval_decision(
            &approval_receipt.revision,
            &approval_payload,
            &Instant("2026-08-24T12:01:00.000Z".to_owned()),
        )
        .expect("exact approval binding");
    assert!(
        !serde_json::to_string(&ledger.snapshot())
            .expect("safe resolved snapshot")
            .contains("Reviewed.")
    );
}

#[test]
fn expiry_is_rebuilt_as_public_state_and_pending_query_fails_closed() {
    let (input, _) = worker_messages();
    let mut ledger = ChatInteractionProjectionLedger::default();
    ledger.record_input_request(&input).expect("record input");
    let pending = ledger
        .query(
            &query(&input.session_identity.product_session_id, &["pending"]),
            &input.expires_at,
        )
        .expect("pending snapshot after expiry");
    assert!(pending.result.items.is_empty());
    let expired = ledger
        .query(
            &query(&input.session_identity.product_session_id, &["expired"]),
            &input.expires_at,
        )
        .expect("expired snapshot");
    assert_eq!(expired.result.items.len(), 1);
}

#[test]
fn websocket_event_is_only_a_bound_reload_invalidation() {
    let (input, _) = worker_messages();
    let mut ledger = ChatInteractionProjectionLedger::default();
    ledger.record_input_request(&input).expect("record input");
    let event = ledger
        .invalidation(input.session_identity.product_session_id.clone())
        .expect("invalidation event");
    let json = serde_json::to_value(event).expect("WebSocket JSON");
    assert_eq!(json["type"], "chat-interactions.invalidated.v1");
    assert_eq!(
        json["reloadQueries"],
        json!(["session.interactions.list", "approval.list"])
    );
    assert!(json.get("prompt").is_none());
    assert!(json.get("binding").is_none());
}
