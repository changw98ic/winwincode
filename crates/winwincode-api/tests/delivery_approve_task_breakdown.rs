// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use winwincode_api::generated::{
    CommandRequest, DeliveryApproveTaskBreakdownCommand, DeliveryApproveTaskBreakdownPayload,
    ErrorCode, TerminalErrorCode,
};

fn canonical_command() -> Value {
    json!({
        "schemaVersion": "winwincode/v1",
        "command": "delivery.approve_task_breakdown",
        "actor": {
            "kind": "user",
            "id": "usr_01J00000000000000000000000"
        },
        "scope": {
            "kind": "repository",
            "organizationId": "org_01J00000000000000000000000",
            "workspaceId": "wsp_01J00000000000000000000000",
            "projectId": "prj_01J00000000000000000000000",
            "repositoryId": "rep_01J00000000000000000000000"
        },
        "requestId": "req_01J00000000000000000000000",
        "expectedRevision": 11,
        "payload": {
            "deliveryId": "dlv_01J00000000000000000000000",
            "reviewSetSha256": format!("sha256:{}", "a".repeat(64))
        }
    })
}

#[test]
fn generated_task_breakdown_command_accepts_only_review_identity() {
    let value = canonical_command();
    let payload: DeliveryApproveTaskBreakdownPayload =
        serde_json::from_value(value["payload"].clone()).expect("canonical task approval payload");
    assert_eq!(
        serde_json::to_value(payload).expect("canonical payload JSON"),
        value["payload"]
    );

    let command: DeliveryApproveTaskBreakdownCommand =
        serde_json::from_value(value.clone()).expect("canonical task approval command");
    assert_eq!(
        serde_json::to_value(command).expect("canonical command JSON"),
        value
    );

    let request: CommandRequest =
        serde_json::from_value(value.clone()).expect("canonical command union");
    assert_eq!(
        serde_json::to_value(request).expect("canonical command union JSON"),
        value
    );
}

#[test]
fn generated_task_breakdown_command_rejects_caller_authored_task_fields() {
    for forbidden in [
        "tasks",
        "taskProposals",
        "owner",
        "ownerActorId",
        "status",
        "review",
        "solutionReview",
    ] {
        let mut payload = canonical_command()["payload"].clone();
        payload[forbidden] = json!([]);
        assert!(
            serde_json::from_value::<DeliveryApproveTaskBreakdownPayload>(payload).is_err(),
            "generated payload accepted forbidden field {forbidden}"
        );

        let mut command = canonical_command();
        command["payload"][forbidden] = json!([]);
        assert!(
            serde_json::from_value::<DeliveryApproveTaskBreakdownCommand>(command.clone()).is_err(),
            "generated command accepted forbidden field {forbidden}"
        );
        assert!(
            serde_json::from_value::<CommandRequest>(command).is_err(),
            "generated command union accepted forbidden field {forbidden}"
        );
    }
}

#[test]
fn generated_http_error_codes_spell_revision_conflict_exactly() {
    assert_eq!(
        serde_json::to_value(ErrorCode::RevisionConflict).expect("generic error code JSON"),
        json!("REVISION_CONFLICT")
    );
    assert_eq!(
        serde_json::to_value(TerminalErrorCode::RevisionConflict)
            .expect("terminal error code JSON"),
        json!("REVISION_CONFLICT")
    );
}
