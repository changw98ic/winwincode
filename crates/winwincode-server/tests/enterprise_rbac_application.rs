// SPDX-License-Identifier: Apache-2.0

use std::{fs, sync::Arc};

use serde_json::{Value, json};
use winwincode_api::generated::{CommandRequest, QueryRequest, QueryResultResponse};
use winwincode_control_plane::EnterpriseRbacService;
use winwincode_server::{
    CommandDispatchResponse, EnterpriseManagementApplicationPort,
    EnterpriseRbacManagementApplication, UnavailableEnterpriseManagementApplication,
};
use winwincode_storage::SqliteStorage;

#[test]
fn generated_rbac_commands_and_queries_use_one_durable_authority() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-enterprise-rbac-application-{}",
        std::process::id()
    ));
    fs::remove_dir_all(&root).ok();
    let rbac = Arc::new(EnterpriseRbacService::new(Box::new(
        SqliteStorage::open(&root).expect("RBAC storage"),
    )));
    let application = EnterpriseRbacManagementApplication::new(
        rbac,
        Arc::new(UnavailableEnterpriseManagementApplication),
    );

    for command in rbac_commands() {
        let completed = application.command(command).expect("route RBAC command");
        assert!(matches!(completed, CommandDispatchResponse::Completed(_)));
    }
    for (query, expected) in rbac_queries() {
        let result = application.query(query).expect("route RBAC query");
        assert_eq!(result_kind(&result), expected);
    }

    drop(application);
    fs::remove_dir_all(root).expect("remove RBAC storage");
}

fn rbac_commands() -> Vec<CommandRequest> {
    [
        json!({
            "schemaVersion": "winwincode/v1",
            "command": "enterprise.organization.update",
            "actor": actor(), "scope": organization_scope(),
            "requestId": request(1), "expectedRevision": 0,
            "payload": {
                "organizationId": organization_id(), "slug": "example",
                "displayName": "Example", "state": "active"
            }
        }),
        json!({
            "schemaVersion": "winwincode/v1",
            "command": "enterprise.role.update",
            "actor": actor(), "scope": organization_scope(),
            "requestId": request(2), "expectedRevision": 1,
            "payload": {
                "roleId": role_id(), "displayName": "Reader", "state": "active",
                "rules": [{ "permission": "organization_read", "effect": "allow" }],
                "inheritedRoles": [], "conflictingRoleIds": []
            }
        }),
        json!({
            "schemaVersion": "winwincode/v1",
            "command": "enterprise.team.update",
            "actor": actor(), "scope": organization_scope(),
            "requestId": request(3), "expectedRevision": 2,
            "payload": {
                "teamId": team_id(), "displayName": "Readers", "state": "active",
                "roleAssignments": [role_assignment()]
            }
        }),
        json!({
            "schemaVersion": "winwincode/v1",
            "command": "enterprise.membership.update",
            "actor": actor(), "scope": organization_scope(),
            "requestId": request(4), "expectedRevision": 3,
            "payload": {
                "membershipId": membership_id(), "actorId": user_id(),
                "displayName": "Member", "state": "active",
                "teamIds": [team_id()], "roleAssignments": []
            }
        }),
    ]
    .into_iter()
    .map(|value| serde_json::from_value(value).expect("generated RBAC command"))
    .collect()
}

fn rbac_queries() -> Vec<(QueryRequest, &'static str)> {
    [
        (
            "enterprise.organization.list",
            "enterprise_organization_page",
        ),
        ("enterprise.role.list", "enterprise_role_page"),
        ("enterprise.team.list", "enterprise_team_page"),
        ("enterprise.membership.list", "enterprise_membership_page"),
    ]
    .into_iter()
    .enumerate()
    .map(|(offset, (query, expected))| {
        let parameters = match query {
            "enterprise.role.list" => json!({ "permissions": [], "states": [] }),
            "enterprise.membership.list" => {
                json!({ "roleIds": [], "states": [], "teamIds": [] })
            }
            _ => json!({ "states": [] }),
        };
        let request: QueryRequest = serde_json::from_value(json!({
            "schemaVersion": "winwincode/v1", "query": query,
            "actor": actor(), "scope": organization_scope(),
            "requestId": request(u8::try_from(10 + offset).expect("request number")),
            "parameters": parameters, "page": { "cursor": null, "limit": 50 }
        }))
        .expect("generated RBAC query");
        (request, expected)
    })
    .collect()
}

fn result_kind(result: &QueryResultResponse) -> &str {
    match result {
        QueryResultResponse::EnterpriseOrganizationListResultResponse(_) => {
            "enterprise_organization_page"
        }
        QueryResultResponse::EnterpriseMembershipListResultResponse(_) => {
            "enterprise_membership_page"
        }
        QueryResultResponse::EnterpriseTeamListResultResponse(_) => "enterprise_team_page",
        QueryResultResponse::EnterpriseRoleListResultResponse(_) => "enterprise_role_page",
        _ => "unexpected",
    }
}

fn role_assignment() -> Value {
    json!({
        "roleId": role_id(), "roleVersion": 1,
        "scope": organization_scope(), "scopeMode": "descendants",
        "notBefore": null, "expiresAt": null
    })
}

fn actor() -> Value {
    json!({ "kind": "user", "id": user_id() })
}

fn organization_scope() -> Value {
    json!({ "kind": "organization", "organizationId": organization_id() })
}

fn request(number: u8) -> String {
    format!("req_{number:026}")
}

const fn organization_id() -> &'static str {
    "org_00000000000000000000000001"
}
const fn user_id() -> &'static str {
    "usr_00000000000000000000000001"
}
const fn role_id() -> &'static str {
    "rol_00000000000000000000000001"
}
const fn team_id() -> &'static str {
    "tem_00000000000000000000000001"
}
const fn membership_id() -> &'static str {
    "mem_00000000000000000000000001"
}
