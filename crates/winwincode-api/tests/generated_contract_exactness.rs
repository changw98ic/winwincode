// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use winwincode_api::generated::{
    ApprovalProjection, CommandCompletedResponse, ControlPlaneWebSocketSubscribeFrame,
    DeliveryStageProjection, DeliveryStageSessionBindingProjection, QueryRequest,
    QueryResultResponse, RuntimeProjectionSnapshot, SettingsProjection, SolutionReviewProjection,
    StrongFlowReadCursor,
};

fn http_examples() -> Value {
    serde_json::from_str(include_str!(
        "../../../schema/winwincode/v1/examples/control-plane-http.examples.json"
    ))
    .expect("canonical HTTP examples")
}

fn websocket_examples() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/control-plane-websocket.valid.json"
    ))
    .expect("canonical WebSocket examples")
}

#[test]
fn generated_http_responses_reject_operation_relabeling() {
    let examples = http_examples();

    let mut query_response = examples["responses"]["runtimeProjection"].clone();
    query_response["query"] = json!("delivery.get");
    assert!(serde_json::from_value::<QueryResultResponse>(query_response).is_err());

    let mut wrong_query_result = examples["responses"]["runtimeProjection"].clone();
    wrong_query_result["result"] = examples["responses"]["queryPage"]["result"].clone();
    assert!(serde_json::from_value::<QueryResultResponse>(wrong_query_result).is_err());

    let mut command_response = examples["responses"]["commandCompleted"].clone();
    command_response["command"] = json!("delivery.create");
    assert!(serde_json::from_value::<CommandCompletedResponse>(command_response).is_err());
}

#[test]
fn generated_strongflow_queries_require_repository_scope() {
    let examples = http_examples();
    for name in ["deliveryGet", "runtimeProjectionGet"] {
        let mut request = examples["positive"][name].clone();
        request["scope"] = json!({
            "kind": "organization",
            "organizationId": "org_00000000000000000000000000"
        });
        assert!(
            serde_json::from_value::<QueryRequest>(request).is_err(),
            "{name} accepted an incomplete repository scope"
        );
    }
}

#[test]
fn required_nullable_fields_cannot_be_omitted_from_generated_rust_dtos() {
    let valid = json!({
        "revision": 1,
        "defaultModelRoute": null,
        "workerConcurrencyLimit": 2
    });
    assert!(serde_json::from_value::<SettingsProjection>(valid.clone()).is_ok());

    let mut missing = valid;
    missing
        .as_object_mut()
        .expect("settings object")
        .remove("defaultModelRoute");
    assert!(serde_json::from_value::<SettingsProjection>(missing).is_err());
}

#[test]
fn generated_approval_detail_is_required_closed_and_secret_safe() {
    let valid = json!({
        "binding": {
            "executionJobId": "job_00000000000000000000000000",
            "productSessionId": "psn_00000000000000000000000000",
            "sessionIdentity": {
                "codexThreadId": "cdx_00000000000000000000000000",
                "productSessionId": "psn_00000000000000000000000000",
                "workerSessionId": "wsn_00000000000000000000000000"
            },
            "workerSessionId": "wsn_00000000000000000000000000"
        },
        "category": "shell",
        "effectiveDecisionScope": "once",
        "expiresAt": "2026-08-24T12:10:00.000Z",
        "id": "apr_00000000000000000000000000",
        "requestedAt": "2026-08-24T12:00:00.000Z",
        "revision": 1,
        "sanitizedDetail": {
            "kind": "unavailable",
            "reason": "producer_unavailable"
        },
        "state": "pending",
        "subject": "Approve embedded shell execution."
    });
    assert!(serde_json::from_value::<ApprovalProjection>(valid.clone()).is_ok());

    let mut missing = valid.clone();
    missing
        .as_object_mut()
        .expect("Approval object")
        .remove("sanitizedDetail");
    assert!(serde_json::from_value::<ApprovalProjection>(missing).is_err());

    let mut selectable_scope = valid.clone();
    selectable_scope["effectiveDecisionScope"] = json!("worker_session");
    assert!(serde_json::from_value::<ApprovalProjection>(selectable_scope).is_err());

    let mut leaked_detail = valid;
    leaked_detail["sanitizedDetail"]["command"] = json!(["sh", "-c", "SECRET"]);
    assert!(serde_json::from_value::<ApprovalProjection>(leaked_detail).is_err());
}

#[test]
fn generated_rust_dtos_preserve_object_level_one_of_constraints() {
    let examples = http_examples();

    let mut review =
        examples["responses"]["deliveryDetailPendingReview"]["result"]["solutionReview"].clone();
    assert!(serde_json::from_value::<SolutionReviewProjection>(review.clone()).is_ok());
    review["reviewerId"] = json!("usr_00000000000000000000000000");
    review["reviewedAt"] = json!("2026-08-24T09:02:00.000Z");
    assert!(serde_json::from_value::<SolutionReviewProjection>(review).is_err());

    let invalid_binding = json!({
        "bindingId": "binding:runtime:1",
        "productSessionId": "psn_00000000000000000000000000",
        "executionJobId": "job_00000000000000000000000000",
        "workerSessionId": null,
        "codexThreadId": "cdx_00000000000000000000000000",
        "boundAt": "2026-08-24T10:00:00.000Z"
    });
    assert!(
        serde_json::from_value::<DeliveryStageSessionBindingProjection>(invalid_binding).is_err()
    );

    let pending_binding = json!({
        "bindingId": "binding:runtime:pending",
        "productSessionId": "psn_00000000000000000000000000",
        "executionJobId": "job_00000000000000000000000000",
        "workerSessionId": null,
        "codexThreadId": null,
        "boundAt": "2026-08-24T10:00:00.000Z",
        "sessionIdentity": null,
        "stageRunId": null,
        "workerId": null,
        "leaseId": null,
        "attempt": null,
        "fencingToken": null,
        "sourceIdentity": null
    });
    assert!(
        serde_json::from_value::<DeliveryStageSessionBindingProjection>(pending_binding).is_ok(),
        "pending DeliveryStageSessionBindingProjection must be decodable"
    );

    let stages = &examples["responses"]["deliveryDetailPendingReview"]["result"]["stages"];
    assert!(serde_json::from_value::<DeliveryStageProjection>(stages[0].clone()).is_ok());
    assert!(serde_json::from_value::<DeliveryStageProjection>(stages[1].clone()).is_ok());
    let mut forged_human_stage = stages[1].clone();
    forged_human_stage["sessionBinding"] = stages[0]["sessionBinding"].clone();
    assert!(serde_json::from_value::<DeliveryStageProjection>(forged_human_stage).is_err());

    let mut runtime = examples["responses"]["runtimeProjection"]["result"].clone();
    runtime["stageRunId"] = Value::Null;
    assert!(serde_json::from_value::<RuntimeProjectionSnapshot>(runtime).is_err());

    let mut empty_changes =
        examples["responses"]["deliveryDetailPendingReview"]["result"]["solutionReview"].clone();
    empty_changes["reviewStatus"] = json!("changes_requested");
    empty_changes["decision"] = json!("request_changes");
    empty_changes["comments"] = Value::Null;
    empty_changes["requestedChanges"] = json!([]);
    empty_changes["reviewerId"] = json!("usr_00000000000000000000000000");
    empty_changes["reviewedAt"] = json!("2026-08-24T09:02:00.000Z");
    assert!(serde_json::from_value::<SolutionReviewProjection>(empty_changes).is_err());
}

#[test]
fn generated_event_cursor_handoff_is_typed_and_fail_closed() {
    let websocket = websocket_examples();
    let subscribe = websocket["transcripts"]
        .as_array()
        .expect("transcripts")
        .iter()
        .find(|transcript| {
            transcript["name"] == "product-session-runtime-invalidation-reloads-runtime-only"
        })
        .expect("snapshot handoff transcript")["frames"][0]
        .clone();
    assert!(
        serde_json::from_value::<ControlPlaneWebSocketSubscribeFrame>(subscribe.clone()).is_ok()
    );

    let mut unknown_origin = subscribe.clone();
    unknown_origin["startAt"] = json!("after-http-snapshot");
    assert!(serde_json::from_value::<ControlPlaneWebSocketSubscribeFrame>(unknown_origin).is_err());

    let mut malformed_cursor = subscribe.clone();
    malformed_cursor["startAt"]["stream"] = json!({
        "kind": "product-session",
        "deliveryId": "dlv_00000000000000000000000000"
    });
    assert!(
        serde_json::from_value::<ControlPlaneWebSocketSubscribeFrame>(malformed_cursor).is_err()
    );

    let mut empty_cursor_with_event = subscribe.clone();
    empty_cursor_with_event["startAt"]["eventId"] = json!("evt_00000000000000000000000042");
    assert!(
        serde_json::from_value::<ControlPlaneWebSocketSubscribeFrame>(empty_cursor_with_event)
            .is_err()
    );

    let mut positive_cursor_without_event = subscribe;
    positive_cursor_without_event["startAt"]["sequence"] = json!(1);
    assert!(
        serde_json::from_value::<ControlPlaneWebSocketSubscribeFrame>(
            positive_cursor_without_event
        )
        .is_err()
    );

    let examples = http_examples();
    let runtime = examples["responses"]["runtimeProjection"]["result"].clone();
    assert!(serde_json::from_value::<RuntimeProjectionSnapshot>(runtime.clone()).is_ok());

    let mut wrong_stream = runtime.clone();
    wrong_stream["eventCursor"]["stream"] = json!({
        "kind": "product-session",
        "productSessionId": "psn_00000000000000000000000000"
    });
    assert!(serde_json::from_value::<RuntimeProjectionSnapshot>(wrong_stream).is_err());

    let mut missing_runtime_cursor = runtime;
    missing_runtime_cursor
        .as_object_mut()
        .expect("runtime object")
        .remove("eventCursor");
    assert!(serde_json::from_value::<RuntimeProjectionSnapshot>(missing_runtime_cursor).is_err());

    let read_cursor = examples["responses"]["runtimeProjection"]["result"]["readCursor"].clone();
    let mut missing_sealed_cursor = read_cursor;
    missing_sealed_cursor
        .as_object_mut()
        .expect("StrongFlow cursor object")
        .remove("eventCursor");
    assert!(serde_json::from_value::<StrongFlowReadCursor>(missing_sealed_cursor).is_err());
}
