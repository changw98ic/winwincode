// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use winwincode_api::generated::{
    CommandAcceptedResponse, CommandCompletedResponse, CommandName, CommandRequest,
    ControlPlaneWebSocketClientFrame, QueryName, QueryRequest, QueryResultResponse, Scope,
};
use winwincode_server::{
    ApiError, AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, ControlPlaneApiPort,
    EventSubscription, GeneratedContractDispatcher, QueryFamily, TypedControlPlaneApiPort,
};

const USER_ID: &str = "usr_00000000000000000000000001";
const REQUEST_ID: &str = "req_00000000000000000000000001";

#[derive(Default)]
struct RecordingApplication {
    denied: AtomicBool,
    mismatched_response: AtomicBool,
    authorizations: AtomicUsize,
    command_families: Mutex<Vec<CommandFamily>>,
    query_families: Mutex<Vec<QueryFamily>>,
    event_calls: AtomicUsize,
}

impl TypedControlPlaneApiPort for RecordingApplication {
    fn authorize_scope(
        &self,
        _principal: &AuthenticatedPrincipal,
        _scope: &Scope,
    ) -> Result<(), ApiError> {
        self.authorizations.fetch_add(1, Ordering::Relaxed);
        if self.denied.load(Ordering::Relaxed) {
            return Err(ApiError::new(403, "PERMISSION_DENIED", "scope is denied"));
        }
        Ok(())
    }

    fn command(
        &self,
        _principal: &AuthenticatedPrincipal,
        family: CommandFamily,
        request: CommandRequest,
    ) -> Result<CommandDispatchResponse, ApiError> {
        self.command_families
            .lock()
            .expect("command families")
            .push(family);
        if family == CommandFamily::Enterprise {
            assert!(matches!(
                request,
                CommandRequest::EnterpriseOrganizationUpdateCommand(_)
            ));
            let response: CommandCompletedResponse = serde_json::from_value(json!({
                "schemaVersion": "winwincode/v1",
                "requestId": REQUEST_ID,
                "command": "enterprise.organization.update",
                "outcome": "completed",
                "previousRevision": 1,
                "currentRevision": 2,
                "result": {
                    "id": "org_00000000000000000000000001",
                    "slug": "example",
                    "displayName": "Example",
                    "state": "active",
                    "revision": 2,
                    "updatedAt": "2026-08-27T00:00:00.000Z"
                }
            }))
            .expect("enterprise command response fixture");
            return Ok(CommandDispatchResponse::Completed(Box::new(response)));
        }
        if family != CommandFamily::Settings {
            return Err(ApiError::new(418, "HANDLER_FIXTURE", "handler fixture"));
        }
        assert!(matches!(request, CommandRequest::SettingsUpdateCommand(_)));
        let request_id = if self.mismatched_response.load(Ordering::Relaxed) {
            "req_00000000000000000000000002"
        } else {
            REQUEST_ID
        };
        let response: CommandAcceptedResponse = serde_json::from_value(json!({
            "schemaVersion": "winwincode/v1",
            "requestId": request_id,
            "command": "settings.update",
            "outcome": "accepted",
            "currentRevision": 2,
            "acceptedAt": "2026-08-27T00:00:00Z"
        }))
        .expect("accepted response fixture");
        Ok(CommandDispatchResponse::Accepted(response))
    }

    fn query(
        &self,
        _principal: &AuthenticatedPrincipal,
        family: QueryFamily,
        request: QueryRequest,
    ) -> Result<QueryResultResponse, ApiError> {
        self.query_families
            .lock()
            .expect("query families")
            .push(family);
        if family == QueryFamily::Enterprise {
            assert!(matches!(
                request,
                QueryRequest::EnterpriseOrganizationListQuery(_)
            ));
            let response: QueryResultResponse = serde_json::from_value(json!({
                "schemaVersion": "winwincode/v1",
                "requestId": REQUEST_ID,
                "query": "enterprise.organization.list",
                "page": { "nextCursor": null, "hasMore": false },
                "result": {
                    "kind": "enterprise_organization_page",
                    "snapshotRevision": 2,
                    "items": [{
                        "id": "org_00000000000000000000000001",
                        "slug": "example",
                        "displayName": "Example",
                        "state": "active",
                        "revision": 2,
                        "updatedAt": "2026-08-27T00:00:00.000Z"
                    }]
                }
            }))
            .expect("enterprise query response fixture");
            return Ok(response);
        }
        if family != QueryFamily::Settings {
            return Err(ApiError::new(418, "HANDLER_FIXTURE", "handler fixture"));
        }
        assert!(matches!(request, QueryRequest::SettingsGetQuery(_)));
        let response: QueryResultResponse = serde_json::from_value(json!({
            "schemaVersion": "winwincode/v1",
            "requestId": REQUEST_ID,
            "query": "settings.get",
            "page": { "nextCursor": null, "hasMore": false },
            "result": {
                "revision": 2,
                "defaultModelRoute": null,
                "workerConcurrencyLimit": 4
            }
        }))
        .expect("query response fixture");
        Ok(response)
    }

    fn subscribe(
        &self,
        _principal: &AuthenticatedPrincipal,
        _first_frame: ControlPlaneWebSocketClientFrame,
    ) -> Result<EventSubscription, ApiError> {
        self.event_calls.fetch_add(1, Ordering::Relaxed);
        Err(ApiError::new(503, "EVENT_FIXTURE", "event fixture"))
    }

    fn event_control(
        &self,
        _principal: &AuthenticatedPrincipal,
        _frame: ControlPlaneWebSocketClientFrame,
    ) -> Result<Vec<Value>, ApiError> {
        self.event_calls.fetch_add(1, Ordering::Relaxed);
        Ok(Vec::new())
    }

    fn shutdown(&self) -> Result<(), ApiError> {
        Ok(())
    }
}

fn principal() -> AuthenticatedPrincipal {
    let actor =
        serde_json::from_value(json!({ "kind": "user", "id": USER_ID })).expect("generated Actor");
    let scopes = [organization_scope(), repository_scope()]
        .into_iter()
        .map(|value| serde_json::from_value(value).expect("generated Scope"))
        .collect();
    AuthenticatedPrincipal::new(actor, scopes).expect("principal")
}

fn organization_scope() -> Value {
    json!({
        "kind": "organization",
        "organizationId": "org_00000000000000000000000001"
    })
}

fn repository_scope() -> Value {
    json!({
        "kind": "repository",
        "organizationId": "org_00000000000000000000000001",
        "workspaceId": "wsp_00000000000000000000000001",
        "projectId": "prj_00000000000000000000000001",
        "repositoryId": "rep_00000000000000000000000001"
    })
}

fn command_fixture(command: &str, scope: &Value, payload: &Value) -> Value {
    json!({
        "schemaVersion": "winwincode/v1",
        "command": command,
        "actor": { "kind": "user", "id": USER_ID },
        "scope": scope,
        "requestId": REQUEST_ID,
        "expectedRevision": 1,
        "payload": payload
    })
}

fn settings_command() -> Value {
    command_fixture(
        "settings.update",
        &organization_scope(),
        &json!({
            "patch": {
                "defaultModelRoute": null,
                "workerConcurrencyLimit": 4
            }
        }),
    )
}

fn query_fixture(query: &str, scope: &Value, parameters: &Value) -> Value {
    json!({
        "schemaVersion": "winwincode/v1",
        "query": query,
        "actor": { "kind": "user", "id": USER_ID },
        "scope": scope,
        "requestId": REQUEST_ID,
        "page": { "cursor": null, "limit": 50 },
        "parameters": parameters
    })
}

fn settings_query() -> Value {
    query_fixture("settings.get", &organization_scope(), &json!({}))
}

fn enterprise_organization_command() -> Value {
    command_fixture(
        "enterprise.organization.update",
        &organization_scope(),
        &json!({
            "organizationId": "org_00000000000000000000000001",
            "slug": "example",
            "displayName": "Example",
            "state": "active"
        }),
    )
}

fn enterprise_organization_query() -> Value {
    query_fixture(
        "enterprise.organization.list",
        &organization_scope(),
        &json!({ "states": [] }),
    )
}

#[test]
fn every_generated_operation_has_one_stable_application_family() {
    assert_command_families();
    assert_query_families();
}

fn assert_command_families() {
    let command_cases = [
        (CommandName::SessionCreate, CommandFamily::Session),
        (CommandName::ChatSubmit, CommandFamily::Session),
        (CommandName::InputRespond, CommandFamily::Session),
        (CommandName::SessionCancel, CommandFamily::Session),
        (CommandName::SessionClose, CommandFamily::Session),
        (CommandName::DeliveryCreate, CommandFamily::Delivery),
        (CommandName::DeliveryUpdateSpec, CommandFamily::Delivery),
        (
            CommandName::DeliveryApproveTaskBreakdown,
            CommandFamily::Delivery,
        ),
        (CommandName::DeliveryAdvance, CommandFamily::Delivery),
        (
            CommandName::DeliveryResolveAttention,
            CommandFamily::Delivery,
        ),
        (CommandName::DeliverySubmitVerdict, CommandFamily::Delivery),
        (CommandName::SettingsUpdate, CommandFamily::Settings),
        (
            CommandName::CredentialReferenceCreate,
            CommandFamily::CredentialReference,
        ),
        (
            CommandName::CredentialReferenceRotate,
            CommandFamily::CredentialReference,
        ),
        (
            CommandName::CredentialReferenceRevoke,
            CommandFamily::CredentialReference,
        ),
        (
            CommandName::CredentialReferenceDelete,
            CommandFamily::CredentialReference,
        ),
        (CommandName::ApprovalDecide, CommandFamily::Approval),
        (CommandName::WorkerDrain, CommandFamily::Worker),
        (CommandName::WorkerEnable, CommandFamily::Worker),
        (CommandName::PublicationPublish, CommandFamily::Publication),
        (CommandName::PublicationCancel, CommandFamily::Publication),
        (
            CommandName::EnterpriseOrganizationUpdate,
            CommandFamily::Enterprise,
        ),
        (
            CommandName::EnterpriseMembershipUpdate,
            CommandFamily::Enterprise,
        ),
        (
            CommandName::EnterpriseProjectRepositoryUpdate,
            CommandFamily::Enterprise,
        ),
        (
            CommandName::EnterprisePolicyUpdate,
            CommandFamily::Enterprise,
        ),
        (
            CommandName::EnterpriseFleetUpdate,
            CommandFamily::Enterprise,
        ),
        (
            CommandName::EnterpriseIntegrationUpdate,
            CommandFamily::Enterprise,
        ),
        (
            CommandName::CollaborationNotificationAck,
            CommandFamily::Collaboration,
        ),
        (
            CommandName::CollaborationPresenceUpdate,
            CommandFamily::Collaboration,
        ),
    ];
    for (name, family) in command_cases {
        assert_eq!(CommandFamily::from_name(&name), family);
    }
}

fn assert_query_families() {
    let query_cases = [
        (QueryName::SessionList, QueryFamily::Session),
        (QueryName::SessionGet, QueryFamily::Session),
        (QueryName::SessionMessagesList, QueryFamily::Session),
        (QueryName::SessionInteractionsList, QueryFamily::Session),
        (QueryName::RuntimeProjectionGet, QueryFamily::Runtime),
        (QueryName::DeliveryList, QueryFamily::Delivery),
        (QueryName::DeliveryGet, QueryFamily::Delivery),
        (QueryName::CandidateList, QueryFamily::Delivery),
        (QueryName::CandidateReviewGet, QueryFamily::Delivery),
        (QueryName::CandidateFilesList, QueryFamily::Delivery),
        (QueryName::CandidateDiffGet, QueryFamily::Delivery),
        (QueryName::EvidenceGet, QueryFamily::Delivery),
        (QueryName::EvidenceArtifactContentGet, QueryFamily::Delivery),
        (QueryName::SettingsGet, QueryFamily::Settings),
        (QueryName::ModelRouteAvailabilityList, QueryFamily::Settings),
        (
            QueryName::CredentialReferenceList,
            QueryFamily::CredentialReference,
        ),
        (
            QueryName::CredentialReferenceGet,
            QueryFamily::CredentialReference,
        ),
        (QueryName::ApprovalList, QueryFamily::Approval),
        (QueryName::ApprovalGet, QueryFamily::Approval),
        (QueryName::WorkerList, QueryFamily::Worker),
        (QueryName::WorkerGet, QueryFamily::Worker),
        (QueryName::PublicationList, QueryFamily::Publication),
        (QueryName::PublicationGet, QueryFamily::Publication),
        (
            QueryName::EnterpriseOrganizationList,
            QueryFamily::Enterprise,
        ),
        (QueryName::EnterpriseMembershipList, QueryFamily::Enterprise),
        (QueryName::EnterpriseProjectList, QueryFamily::Enterprise),
        (QueryName::EnterprisePolicyList, QueryFamily::Enterprise),
        (QueryName::EnterpriseFleetList, QueryFamily::Enterprise),
        (QueryName::EnterpriseUsageList, QueryFamily::Enterprise),
        (QueryName::EnterpriseAuditList, QueryFamily::Enterprise),
        (
            QueryName::EnterpriseIntegrationList,
            QueryFamily::Enterprise,
        ),
        (
            QueryName::CollaborationActivityList,
            QueryFamily::Collaboration,
        ),
        (
            QueryName::CollaborationNotificationList,
            QueryFamily::Collaboration,
        ),
        (
            QueryName::CollaborationPresenceList,
            QueryFamily::Collaboration,
        ),
    ];
    for (name, family) in query_cases {
        assert_eq!(QueryFamily::from_name(&name), family);
    }
}

#[test]
fn representative_generated_commands_reach_each_application_family() {
    let application = Arc::new(RecordingApplication::default());
    let dispatcher = GeneratedContractDispatcher::new(application.clone());
    let command_cases = [
        command_fixture(
            "session.close",
            &repository_scope(),
            &json!({ "productSessionId": "psn_00000000000000000000000001" }),
        ),
        command_fixture(
            "delivery.advance",
            &repository_scope(),
            &json!({ "deliveryId": "dlv_00000000000000000000000001" }),
        ),
        settings_command(),
        command_fixture(
            "credential.reference.delete",
            &organization_scope(),
            &json!({ "credentialReferenceId": "crd_00000000000000000000000001" }),
        ),
        command_fixture(
            "approval.decide",
            &repository_scope(),
            &json!({
                "approvalId": "apr_00000000000000000000000001",
                "binding": {
                    "executionJobId": "job_00000000000000000000000001",
                    "productSessionId": "psn_00000000000000000000000001",
                    "workerSessionId": "wsn_00000000000000000000000001",
                    "sessionIdentity": {
                        "productSessionId": "psn_00000000000000000000000001",
                        "workerSessionId": "wsn_00000000000000000000000001",
                        "codexThreadId": "cdx_00000000000000000000000001"
                    }
                },
                "decision": "approve",
                "reason": "fixture"
            }),
        ),
        command_fixture(
            "worker.drain",
            &organization_scope(),
            &json!({
                "workerId": "wrk_00000000000000000000000001",
                "reason": "fixture"
            }),
        ),
        enterprise_organization_command(),
        command_fixture(
            "publication.cancel",
            &repository_scope(),
            &json!({
                "publicationId": "pub_00000000000000000000000001",
                "reason": "fixture"
            }),
        ),
        command_fixture(
            "collaboration.notification.ack",
            &organization_scope(),
            &json!({ "throughSequence": 1 }),
        ),
    ];
    for request in command_cases {
        let _ = dispatcher.command(&principal(), request);
    }
    assert_eq!(
        *application.command_families.lock().expect("commands"),
        vec![
            CommandFamily::Session,
            CommandFamily::Delivery,
            CommandFamily::Settings,
            CommandFamily::CredentialReference,
            CommandFamily::Approval,
            CommandFamily::Worker,
            CommandFamily::Enterprise,
            CommandFamily::Publication,
            CommandFamily::Collaboration,
        ]
    );
}

#[test]
fn representative_generated_queries_reach_each_application_family() {
    let application = Arc::new(RecordingApplication::default());
    let dispatcher = GeneratedContractDispatcher::new(application.clone());
    let query_cases = [
        query_fixture(
            "session.list",
            &repository_scope(),
            &json!({ "states": [] }),
        ),
        query_fixture(
            "runtime.projection.get",
            &repository_scope(),
            &json!({
                "kind": "product-session",
                "productSessionId": "psn_00000000000000000000000001"
            }),
        ),
        query_fixture(
            "delivery.list",
            &repository_scope(),
            &json!({ "states": [] }),
        ),
        settings_query(),
        query_fixture(
            "credential.reference.list",
            &organization_scope(),
            &json!({ "providerId": null }),
        ),
        query_fixture(
            "approval.list",
            &repository_scope(),
            &json!({ "states": [] }),
        ),
        query_fixture(
            "worker.list",
            &organization_scope(),
            &json!({ "states": [] }),
        ),
        query_fixture(
            "publication.list",
            &repository_scope(),
            &json!({ "deliveryId": null, "states": [] }),
        ),
        enterprise_organization_query(),
        query_fixture(
            "collaboration.activity.list",
            &organization_scope(),
            &json!({
                "categories": [],
                "deliveryId": null,
                "productSessionId": null
            }),
        ),
    ];
    for request in query_cases {
        let _ = dispatcher.query(&principal(), request);
    }
    assert_eq!(
        *application.query_families.lock().expect("queries"),
        vec![
            QueryFamily::Session,
            QueryFamily::Runtime,
            QueryFamily::Delivery,
            QueryFamily::Settings,
            QueryFamily::CredentialReference,
            QueryFamily::Approval,
            QueryFamily::Worker,
            QueryFamily::Publication,
            QueryFamily::Enterprise,
            QueryFamily::Collaboration,
        ]
    );
}

#[test]
fn generated_command_and_query_are_authorized_routed_and_correlated() {
    let application = Arc::new(RecordingApplication::default());
    let dispatcher = GeneratedContractDispatcher::new(application.clone());

    let command = dispatcher
        .command(&principal(), settings_command())
        .expect("command response");
    assert_eq!(command["requestId"], REQUEST_ID);
    assert_eq!(command["command"], "settings.update");

    let query = dispatcher
        .query(&principal(), settings_query())
        .expect("query response");
    assert_eq!(query["requestId"], REQUEST_ID);
    assert_eq!(query["query"], "settings.get");

    assert_eq!(application.authorizations.load(Ordering::Relaxed), 2);
    assert_eq!(
        *application.command_families.lock().expect("commands"),
        vec![CommandFamily::Settings]
    );
    assert_eq!(
        *application.query_families.lock().expect("queries"),
        vec![QueryFamily::Settings]
    );
}

#[test]
fn enterprise_contracts_use_the_one_generated_http_dispatcher() {
    let application = Arc::new(RecordingApplication::default());
    let dispatcher = GeneratedContractDispatcher::new(application.clone());

    let query = dispatcher
        .query(&principal(), enterprise_organization_query())
        .expect("enterprise query response");
    assert_eq!(query["query"], "enterprise.organization.list");
    assert_eq!(query["result"]["snapshotRevision"], 2);

    let command = dispatcher
        .command(&principal(), enterprise_organization_command())
        .expect("enterprise command response");
    assert_eq!(command["command"], "enterprise.organization.update");
    assert_eq!(command["previousRevision"], 1);
    assert_eq!(command["currentRevision"], 2);

    assert_eq!(application.authorizations.load(Ordering::Relaxed), 2);
    assert_eq!(
        *application.query_families.lock().expect("queries"),
        vec![QueryFamily::Enterprise]
    );
    assert_eq!(
        *application.command_families.lock().expect("commands"),
        vec![CommandFamily::Enterprise]
    );
}

#[test]
fn malformed_actor_scope_and_response_fail_before_publication() {
    let application = Arc::new(RecordingApplication::default());
    let dispatcher = GeneratedContractDispatcher::new(application.clone());

    let mut unknown = settings_command();
    unknown["unknown"] = json!(true);
    let error = dispatcher
        .command(&principal(), unknown)
        .expect_err("unknown field");
    assert_eq!(error.code(), "INVALID_REQUEST");

    let mut foreign_actor = settings_command();
    foreign_actor["actor"]["id"] = json!("usr_00000000000000000000000002");
    let error = dispatcher
        .command(&principal(), foreign_actor)
        .expect_err("foreign actor");
    assert_eq!(error.code(), "PERMISSION_DENIED");

    let actor =
        serde_json::from_value(json!({ "kind": "user", "id": USER_ID })).expect("generated Actor");
    let repository_only = AuthenticatedPrincipal::new(
        actor,
        vec![serde_json::from_value(repository_scope()).expect("repository Scope")],
    )
    .expect("repository-only principal");
    let error = dispatcher
        .command(&repository_only, settings_command())
        .expect_err("session scope does not authorize organization");
    assert_eq!(error.code(), "PERMISSION_DENIED");
    assert_eq!(application.authorizations.load(Ordering::Relaxed), 0);

    application.denied.store(true, Ordering::Relaxed);
    let error = dispatcher
        .command(&principal(), settings_command())
        .expect_err("denied scope");
    assert_eq!(error.code(), "PERMISSION_DENIED");
    assert!(
        application
            .command_families
            .lock()
            .expect("commands")
            .is_empty()
    );

    application.denied.store(false, Ordering::Relaxed);
    application
        .mismatched_response
        .store(true, Ordering::Relaxed);
    let error = dispatcher
        .command(&principal(), settings_command())
        .expect_err("mismatched response");
    assert_eq!(error.code(), "APPLICATION_RESPONSE_INVALID");
}

#[test]
fn websocket_frames_are_generated_and_state_checked_before_the_event_port() {
    let application = Arc::new(RecordingApplication::default());
    let dispatcher = GeneratedContractDispatcher::new(application.clone());

    let Err(error) = dispatcher.subscribe(
        &principal(),
        json!({
            "type": "transport.pong.v1",
            "nonce": "0123456789abcdef"
        }),
    ) else {
        panic!("pong cannot start subscription");
    };
    assert_eq!(error.code(), "SUBSCRIPTION_REQUIRED");

    let actor =
        serde_json::from_value(json!({ "kind": "user", "id": USER_ID })).expect("generated Actor");
    let repository_only = AuthenticatedPrincipal::new(
        actor,
        vec![serde_json::from_value(repository_scope()).expect("repository Scope")],
    )
    .expect("repository-only principal");
    let error = dispatcher
        .event_control(
            &repository_only,
            json!({
                "type": "transport.ack.v1",
                "subscriptionId": "sub_00000000000000000000000001",
                "cursor": {
                    "scope": organization_scope(),
                    "stream": { "kind": "scope" },
                    "sequence": 1,
                    "eventId": "evt_00000000000000000000000001"
                }
            }),
        )
        .expect_err("session authorization must cover acknowledgement scope");
    assert_eq!(error.code(), "PERMISSION_DENIED");
    assert_eq!(application.authorizations.load(Ordering::Relaxed), 0);

    let error = dispatcher
        .event_control(
            &principal(),
            json!({
                "type": "transport.subscribe.v1",
                "subscriptionId": "sub_00000000000000000000000001",
                "subscription": {
                    "scope": organization_scope(),
                    "stream": { "kind": "scope" },
                    "eventTypes": ["activity.recorded.v1"]
                },
                "startAt": "latest"
            }),
        )
        .expect_err("subscribe requires new socket");
    assert_eq!(error.code(), "WRONG_STATE");
    assert_eq!(application.event_calls.load(Ordering::Relaxed), 0);
}
