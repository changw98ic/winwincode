// SPDX-License-Identifier: Apache-2.0

use serde_json::json;
use winwincode_api::generated::{
    CommandRequest, DeliverySubmitVerdictCommand, DeliverySubmitVerdictPayload,
};

fn canonical_command() -> serde_json::Value {
    json!({
        "schemaVersion": "winwincode/v1",
        "command": "delivery.submit_verdict",
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
        "expectedRevision": 7,
        "payload": {
            "deliveryId": "dlv_01J00000000000000000000000",
            "candidateDigest": format!("sha256:{}", "a".repeat(64))
        }
    })
}

#[test]
fn generated_submit_verdict_types_reject_wrong_discriminator_and_untrusted_fields() {
    let mut wrong_command = canonical_command();
    wrong_command["command"] = json!("delivery.advance");
    assert!(serde_json::from_value::<DeliverySubmitVerdictCommand>(wrong_command).is_err());

    for forbidden in [
        "attention",
        "credential",
        "criterionResults",
        "evidence",
        "rawRuntimeFacts",
        "runtimeEvents",
        "status",
        "verdict",
        "verification",
    ] {
        let mut payload = canonical_command()["payload"].clone();
        payload[forbidden] = json!({});
        assert!(
            serde_json::from_value::<DeliverySubmitVerdictPayload>(payload).is_err(),
            "accepted forbidden field {forbidden}"
        );

        let mut request = canonical_command();
        request["payload"][forbidden] = json!({});
        assert!(
            serde_json::from_value::<CommandRequest>(request).is_err(),
            "command union accepted forbidden field {forbidden}"
        );
    }

    let mut workspace_scope = canonical_command();
    workspace_scope["scope"] = json!({
        "kind": "workspace",
        "organizationId": "org_01J00000000000000000000000",
        "workspaceId": "wsp_01J00000000000000000000000"
    });
    assert!(
        serde_json::from_value::<DeliverySubmitVerdictCommand>(workspace_scope.clone()).is_err()
    );
    assert!(serde_json::from_value::<CommandRequest>(workspace_scope).is_err());

    let mut unknown_envelope = canonical_command();
    unknown_envelope["legacyVerdict"] = json!({ "status": "pass" });
    assert!(serde_json::from_value::<DeliverySubmitVerdictCommand>(unknown_envelope).is_err());
}

#[test]
fn canonical_submit_verdict_command_round_trips_through_generated_types() {
    let value = canonical_command();
    let command: DeliverySubmitVerdictCommand =
        serde_json::from_value(value.clone()).expect("canonical command DTO");
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
