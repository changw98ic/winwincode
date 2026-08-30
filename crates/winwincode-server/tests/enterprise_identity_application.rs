// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::sync::Arc;

use serde_json::{Value, json};
use winwincode_api::generated::{CommandRequest, QueryRequest, QueryResultResponse};
use winwincode_control_plane::EnterpriseIdentityService;
use winwincode_server::{
    CommandDispatchResponse, EnterpriseIdentityManagementApplication,
    EnterpriseManagementApplicationPort, UnavailableEnterpriseManagementApplication,
};
use winwincode_storage::SqliteStorage;

#[test]
fn generated_identity_command_and_query_use_the_one_durable_service() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-enterprise-identity-application-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let identity = Arc::new(EnterpriseIdentityService::new(Box::new(
        SqliteStorage::open(&root).expect("identity storage"),
    )));
    let application = EnterpriseIdentityManagementApplication::new(
        identity,
        Arc::new(UnavailableEnterpriseManagementApplication),
    );

    let completed = application
        .command(identity_command())
        .expect("route generated identity command");
    let CommandDispatchResponse::Completed(completed) = completed else {
        panic!("identity mutation must complete synchronously");
    };
    let completed = serde_json::to_value(completed).expect("completed response JSON");
    assert_eq!(completed["command"], "enterprise.identity.update");
    assert_eq!(completed["result"]["kind"], "service_account");
    assert_eq!(completed["currentRevision"], 1);

    let listed = application
        .query(identity_query())
        .expect("route generated identity query");
    assert!(matches!(
        listed,
        QueryResultResponse::EnterpriseIdentityListResultResponse(_)
    ));
    let listed = serde_json::to_value(listed).expect("query response JSON");
    assert_eq!(listed["query"], "enterprise.identity.list");
    assert_eq!(listed["result"]["kind"], "enterprise_identity_page");
    assert_eq!(listed["result"]["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["result"]["items"][0]["id"], service_account_id());

    drop(application);
    fs::remove_dir_all(root).expect("remove identity storage");
}

fn identity_command() -> CommandRequest {
    serde_json::from_value(json!({
        "schemaVersion": "winwincode/v1",
        "command": "enterprise.identity.update",
        "actor": actor(),
        "scope": organization_scope(),
        "requestId": "req_00000000000000000000000091",
        "expectedRevision": 0,
        "payload": {
            "kind": "service_account",
            "action": "upsert",
            "serviceAccountId": service_account_id(),
            "displayName": "Application fixture",
            "authorizedScopes": [organization_scope()]
        }
    }))
    .expect("generated identity command")
}

fn identity_query() -> QueryRequest {
    serde_json::from_value(json!({
        "schemaVersion": "winwincode/v1",
        "query": "enterprise.identity.list",
        "actor": actor(),
        "scope": organization_scope(),
        "requestId": "req_00000000000000000000000092",
        "parameters": { "kinds": [], "states": [] },
        "page": { "cursor": null, "limit": 50 }
    }))
    .expect("generated identity query")
}

fn actor() -> Value {
    json!({
        "kind": "user",
        "id": "usr_00000000000000000000000001"
    })
}

fn organization_scope() -> Value {
    json!({
        "kind": "organization",
        "organizationId": "org_00000000000000000000000001"
    })
}

const fn service_account_id() -> &'static str {
    "svc_00000000000000000000000001"
}
