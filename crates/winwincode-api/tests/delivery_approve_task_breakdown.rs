// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use winwincode_api::generated::{
    CommandRequest, DeliveryTaskBreakdownCreateCommand, DeliveryTaskBreakdownCreatePayload,
    ErrorCode, TerminalErrorCode,
};

fn canonical_work_item_command() -> Value {
    json!({
        "schemaVersion": "winwincode/v1",
        "command": "delivery.task_breakdown.create",
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
        "expectedRevision": 1,
        "payload": {
            "deliveryId": "dlv_01J00000000000000000000000",
            "expectedRevision": 1,
            "contractRevision": 1,
            "items": [{
                "id": "wit_01J00000000000000000000000",
                "title": "Implement approved change",
                "goal": "Implement the approved requirement",
                "criterionIds": ["crt_01J00000000000000000000000"],
                "dependsOn": []
            }]
        }
    })
}

fn removed_approval_command() -> Value {
    let mut command = canonical_work_item_command();
    command["command"] = json!("delivery.approve_task_breakdown");
    command["payload"] = json!({
        "deliveryId": "dlv_01J00000000000000000000000",
        "reviewSetSha256": format!("sha256:{}", "a".repeat(64))
    });
    command
}

#[test]
fn removed_task_breakdown_approval_command_is_rejected() {
    let command = removed_approval_command();
    assert!(serde_json::from_value::<CommandRequest>(command.clone()).is_err());
    assert!(serde_json::from_value::<DeliveryTaskBreakdownCreateCommand>(command).is_err());
}

#[test]
fn generated_work_item_create_accepts_canonical_payload() {
    let value = canonical_work_item_command();
    let payload: DeliveryTaskBreakdownCreatePayload =
        serde_json::from_value(value["payload"].clone()).expect("canonical WorkItem payload");
    assert_eq!(
        serde_json::to_value(payload).expect("canonical payload JSON"),
        value["payload"]
    );

    let command: DeliveryTaskBreakdownCreateCommand =
        serde_json::from_value(value.clone()).expect("canonical WorkItem command");
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
fn generated_work_item_create_rejects_missing_or_unknown_payload_fields() {
    let value = canonical_work_item_command();

    for missing in [
        "deliveryId",
        "expectedRevision",
        "contractRevision",
        "items",
    ] {
        let mut payload = value["payload"].clone();
        payload
            .as_object_mut()
            .expect("payload object")
            .remove(missing);
        assert!(
            serde_json::from_value::<DeliveryTaskBreakdownCreatePayload>(payload).is_err(),
            "payload accepted missing field {missing}"
        );
    }

    let mut payload = value["payload"].clone();
    payload["reviewSetSha256"] = json!(format!("sha256:{}", "a".repeat(64)));
    assert!(serde_json::from_value::<DeliveryTaskBreakdownCreatePayload>(payload).is_err());
}

#[test]
fn generated_work_item_create_requires_repository_scope() {
    let mut command = canonical_work_item_command();
    command["scope"] = json!({
        "kind": "workspace",
        "organizationId": "org_01J00000000000000000000000",
        "workspaceId": "wsp_01J00000000000000000000000"
    });

    assert!(serde_json::from_value::<DeliveryTaskBreakdownCreateCommand>(command.clone()).is_err());
    assert!(serde_json::from_value::<CommandRequest>(command).is_err());
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
