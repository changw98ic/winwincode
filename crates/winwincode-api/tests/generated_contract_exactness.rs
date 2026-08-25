// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use winwincode_api::generated::{
    CommandCompletedResponse, DeliveryStageProjection, DeliveryStageSessionBindingProjection,
    QueryRequest, QueryResultResponse, RuntimeProjectionSnapshot, SettingsProjection,
    SolutionReviewProjection,
};

fn http_examples() -> Value {
    serde_json::from_str(include_str!(
        "../../../schema/winwincode/v1/examples/control-plane-http.examples.json"
    ))
    .expect("canonical HTTP examples")
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
