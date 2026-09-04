// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_api::generated::{
    Actor, CommandCompletedResponse, CommandRequest, CredentialReferenceCreateCommand,
    CredentialReferenceCreateCommandCommand, CredentialReferenceCreatePayload,
    CredentialReferenceGetParameters, CredentialReferenceGetQuery,
    CredentialReferenceGetQueryQuery, CredentialReferenceRotateCommand,
    CredentialReferenceRotateCommandCommand, CredentialReferenceRotatePayload, OrganizationScope,
    OrganizationScopeKind, PageRequest, QueryRequest, QueryResultResponse, Scope,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DurableWorkerInteractionOutbound, EventPublishError,
    EventPublisher, OutboxEvent, ProductSessionExecutionConfig,
};
use winwincode_domain::{
    CredentialReferenceId, ExecutionMessageId, OrganizationId, RequestId, Revision, SchemaVersion,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId,
};
use winwincode_domain::{RepositoryScope, UserActor, UserActorKind};
use winwincode_server::{
    ApiError, AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, DurableEventHub,
    DurableEventHubConfig, QueryFamily, StandaloneApplicationClock,
    StandaloneControlPlaneApplication, TypedControlPlaneApiPort,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, SqliteStorage, WorkerAuthenticationIdentity,
    WorkerOutboundQueueConfig, WorkerPlatform, WorkerRegistrationRequest, WorkerRegistryScope,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

#[test]
fn production_main_uses_the_real_application_registry() {
    let source = include_str!("../src/main.rs");
    assert!(source.contains("StandaloneControlPlaneApplication::new"));
    assert!(source.contains("DurableEventPublisher::new"));
    assert!(source.contains("ControlPlane::start_local_with_production_adapters"));
    assert!(!source.contains("ControlPlane::start_local_with_delivery_adapters"));
    assert!(!source.contains("ControlPlane::start_local("));
    for required_name in [
        "WWC_SERVER_REPOSITORY_ROOT",
        "WWC_SERVER_ORGANIZATION_ID",
        "WWC_SERVER_WORKSPACE_ID",
        "WWC_SERVER_PROJECT_ID",
        "WWC_SERVER_REPOSITORY_ID",
        "GITHUB_REPOSITORY",
        "GITHUB_CREDENTIAL_REFERENCE_ID",
        "GITHUB_API_BASE_URL",
        "SECRET_DIRECTORY",
        "PUBLICATION_REQUESTERS",
        "PUBLICATION_APPROVERS",
        "PUBLICATION_APPROVAL_MAX_AGE_MILLIS",
    ] {
        assert!(source.contains(required_name));
    }
    assert!(!source.contains("APPLICATION_ROUTE_UNAVAILABLE"));
    assert!(!source.contains("PendingControlPlaneApplications"));
    assert!(!source.contains("PendingTransportPublisher"));
    let application = include_str!("../src/application.rs");
    for delivery_operation in [
        ".delivery_create(&command)",
        ".delivery_update_spec(&command)",
        ".delivery_approve_task_breakdown(&command)",
        ".delivery_advance(&command)",
        ".delivery_resolve_attention(&command)",
        ".delivery_submit_verdict(&command)",
    ] {
        assert!(application.contains(delivery_operation));
    }
    assert!(application.contains(".publication_publish(&command)"));
    assert!(!application.contains("operation_not_ready"));
    assert!(!application.contains("delivery command requires an applicable StrongFlow transition"));
}

#[test]
fn production_main_attaches_one_supervised_local_runtime_to_the_api_registry() {
    let source = include_str!("../src/main.rs");
    for required_name in [
        "LocalRuntimeSupervisor",
        "ServerExecutionPortCore",
        "RepositoryRuntimeScheduler",
        "ProductSessionExecutionConfig",
        "ProductionCodexAdapter",
        "with_runtime_health",
    ] {
        assert!(
            source.contains(required_name),
            "production Server composition must retain {required_name}",
        );
    }
    assert!(
        source.contains("start_with_scheduler") || source.contains("LocalRuntimeSupervisor::start"),
        "production Server must start the supervised local runtime",
    );
}

#[test]
fn product_session_api_calls_use_the_composed_execution_policy() {
    let source = include_str!("../src/application.rs");
    let marker = "ProductSessionApiService::new(";
    let calls = source
        .match_indices(marker)
        .map(|(offset, _)| {
            let remainder = &source[offset + marker.len()..];
            let end = remainder
                .find(");")
                .expect("ProductSessionApiService constructor call must close");
            &remainder[..end]
        })
        .collect::<Vec<_>>();

    assert!(
        !calls.is_empty() || source.contains("ProductSessionApiService::with_output_gate("),
        "Server must retain a ProductSession API adapter",
    );
    for call in calls {
        assert!(
            call.matches(',').count() >= 2,
            "ProductSessionApiService::new must receive storage, clock, and the startup execution configuration: {call}",
        );
    }
    assert!(
        source.contains("ProductSessionExecutionConfig"),
        "Server composition must retain one startup ProductSessionExecutionConfig",
    );
}

struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

struct FixedClock(u64);

impl StandaloneApplicationClock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }

    fn now_instant(&self) -> winwincode_domain::Instant {
        winwincode_domain::Instant("2027-01-15T08:00:00.000Z".to_owned())
    }
}

fn temporary_root(name: &str) -> PathBuf {
    let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-server-application-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn actor(seed: u64) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", seed)),
        kind: UserActorKind::User,
    })
}

fn scope(seed: u64) -> Scope {
    Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", seed)),
    })
}

fn test_principal(seed: u64) -> AuthenticatedPrincipal {
    let repository_scope =
        serde_json::from_value(repository_scope_json(seed)).expect("generated repository Scope");
    AuthenticatedPrincipal::new(actor(seed), vec![scope(seed), repository_scope])
        .expect("principal")
}

fn create(seed: u64) -> CredentialReferenceCreateCommand {
    CredentialReferenceCreateCommand {
        actor: actor(seed),
        command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: CredentialReferenceCreatePayload {
            credential_reference_id: CredentialReferenceId(id("crd", seed)),
            display_name: "Provider credential".to_owned(),
            provider_id: "provider-main".to_owned(),
            vault_locator: "local-fixture://write-only".to_owned(),
        },
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope(seed),
    }
}

fn get(
    command: &CredentialReferenceCreateCommand,
    seed: u64,
    query_scope: Scope,
) -> CredentialReferenceGetQuery {
    CredentialReferenceGetQuery {
        actor: command.actor.clone(),
        page: PageRequest {
            cursor: None,
            limit: 1,
        },
        parameters: CredentialReferenceGetParameters {
            credential_reference_id: command.payload.credential_reference_id.clone(),
        },
        query: CredentialReferenceGetQueryQuery::CredentialReferenceGet,
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: query_scope,
    }
}

fn open_application(
    root: &Path,
    subject: &str,
) -> Result<StandaloneControlPlaneApplication, ApiError> {
    let storage = SqliteStorage::open(root).expect("open application storage");
    compose_application(root, subject, storage)
}

fn compose_application(
    root: &Path,
    _subject: &str,
    storage: SqliteStorage,
) -> Result<StandaloneControlPlaneApplication, ApiError> {
    let hub = Arc::new(
        DurableEventHub::open(root.join("events"), DurableEventHubConfig::default())
            .expect("open event hub"),
    );
    let control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root),
        Box::new(RecordingPublisher),
    )
    .expect("open Control Plane");
    let worker_outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(root).expect("open Worker outbound storage"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("open Worker outbound adapter");
    StandaloneControlPlaneApplication::new_with_clock(
        control_plane,
        storage,
        worker_outbound,
        hub,
        Arc::new(FixedClock(1_800_000_000_000)),
        execution_config(1),
    )
}

fn execution_config(seed: u64) -> ProductSessionExecutionConfig {
    let repository_scope: RepositoryScope =
        serde_json::from_value(repository_scope_json(seed)).expect("repository scope");
    ProductSessionExecutionConfig::try_new(
        repository_scope,
        "fixture-checkout-revision",
        "codex-chat",
        3_600,
        1_073_741_824,
    )
    .expect("execution config")
}

#[test]
fn composition_rejects_a_control_plane_from_another_database() {
    let application_root = temporary_root("authority");
    let foreign_root = temporary_root("foreign-authority");
    let hub = Arc::new(
        DurableEventHub::open(
            application_root.join("events"),
            DurableEventHubConfig::default(),
        )
        .expect("open event hub"),
    );
    let foreign_control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&foreign_root),
        Box::new(RecordingPublisher),
    )
    .expect("open foreign Control Plane");
    let storage = SqliteStorage::open(&application_root).expect("open application storage");
    let worker_outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&application_root).expect("open Worker outbound storage"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("open Worker outbound adapter");
    let result = StandaloneControlPlaneApplication::new_with_clock(
        foreign_control_plane,
        storage,
        worker_outbound,
        hub,
        Arc::new(FixedClock(1_800_000_000_000)),
        execution_config(1),
    );
    let Err(error) = result else {
        panic!("foreign Control Plane database must be rejected");
    };
    assert_eq!(error.status(), 500);
    assert_eq!(error.code(), "APPLICATION_CONFIGURATION_INVALID");

    let hub = Arc::new(
        DurableEventHub::open(
            application_root.join("events-outbound-authority"),
            DurableEventHubConfig::default(),
        )
        .expect("open second event hub"),
    );
    let control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&application_root),
        Box::new(RecordingPublisher),
    )
    .expect("open application Control Plane");
    let storage = SqliteStorage::open(&application_root).expect("open application storage");
    let foreign_worker_outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&foreign_root).expect("open foreign Worker outbound storage"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("open foreign Worker outbound adapter");
    let result = StandaloneControlPlaneApplication::new_with_clock(
        control_plane,
        storage,
        foreign_worker_outbound,
        hub,
        Arc::new(FixedClock(1_800_000_000_000)),
        execution_config(1),
    );
    let Err(error) = result else {
        panic!("foreign Worker outbound database must be rejected");
    };
    assert_eq!(error.status(), 500);
    assert_eq!(error.code(), "APPLICATION_CONFIGURATION_INVALID");

    fs::remove_dir_all(application_root).expect("remove application directory");
    fs::remove_dir_all(foreign_root).expect("remove foreign directory");
}

#[test]
fn credential_service_replays_conflicts_isolates_scope_and_recovers_after_restart() {
    let root = temporary_root("credential");
    let subject = id("usr", 1);
    let principal = test_principal(1);
    let command = create(1);
    let application = open_application(&root, &subject).expect("application");

    let first = application
        .command(
            &principal,
            CommandFamily::CredentialReference,
            CommandRequest::CredentialReferenceCreateCommand(command.clone()),
        )
        .expect("create Credential reference");
    let replay = application
        .command(
            &principal,
            CommandFamily::CredentialReference,
            CommandRequest::CredentialReferenceCreateCommand(command.clone()),
        )
        .expect("replay Credential reference create");
    assert_eq!(first, replay);
    let CommandDispatchResponse::Completed(first) = first else {
        panic!("Credential reference create completes synchronously");
    };
    let CommandCompletedResponse::CredentialReferenceCreateCompletedResponse(first) = *first else {
        panic!("exact create response variant");
    };
    assert_eq!(first.current_revision, Revision(1));
    assert_eq!(first.previous_revision, Revision(0));
    assert_eq!(first.result.provider_id, "provider-main");
    let public_json = serde_json::to_string(&first).expect("public create response");
    assert!(!public_json.contains("local-fixture://write-only"));
    assert!(!public_json.contains("vaultLocator"));

    let mut stale_rotate = CredentialReferenceRotateCommand {
        actor: command.actor.clone(),
        command: CredentialReferenceRotateCommandCommand::CredentialReferenceRotate,
        expected_revision: Revision(0),
        payload: CredentialReferenceRotatePayload {
            credential_reference_id: command.payload.credential_reference_id.clone(),
            vault_locator: "local-fixture://rotated".to_owned(),
        },
        request_id: RequestId(id("req", 2)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: command.scope.clone(),
    };
    let conflict = application
        .command(
            &principal,
            CommandFamily::CredentialReference,
            CommandRequest::CredentialReferenceRotateCommand(stale_rotate.clone()),
        )
        .expect_err("stale revision");
    assert_eq!(conflict.status(), 409);
    assert_eq!(conflict.code(), "REVISION_CONFLICT");

    stale_rotate.expected_revision = Revision(1);
    stale_rotate.request_id = RequestId(id("req", 3));
    application
        .command(
            &principal,
            CommandFamily::CredentialReference,
            CommandRequest::CredentialReferenceRotateCommand(stale_rotate),
        )
        .expect("current revision rotates");

    let foreign = application
        .query(
            &principal,
            QueryFamily::CredentialReference,
            QueryRequest::CredentialReferenceGetQuery(get(&command, 4, scope(99))),
        )
        .expect_err("foreign scope is isolated");
    assert_eq!(foreign.status(), 403);
    assert_eq!(foreign.code(), "PERMISSION_DENIED");

    application.shutdown().expect("first shutdown");
    let restarted = open_application(&root, &subject).expect("restart application");
    let recovered = restarted
        .query(
            &principal,
            QueryFamily::CredentialReference,
            QueryRequest::CredentialReferenceGetQuery(get(&command, 5, command.scope.clone())),
        )
        .expect("recover Credential reference");
    let QueryResultResponse::CredentialReferenceGetResultResponse(recovered) = recovered else {
        panic!("exact get response variant");
    };
    assert_eq!(recovered.result.rotation_version, 2);
    assert_eq!(recovered.result.revision, Revision(2));
    restarted.shutdown().expect("second shutdown");
    fs::remove_dir_all(root).expect("remove temporary application directory");
}

#[test]
fn product_session_routes_replay_conflict_page_and_recover_after_restart() {
    let root = temporary_root("product-session");
    let subject = id("usr", 1);
    let principal = test_principal(1);
    let application = open_application(&root, &subject).expect("application");
    let create: CommandRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 20),
        "command": "session.create",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "expectedRevision": 0,
        "payload": {
            "productSessionId": id("psn", 1),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1),
            "title": "Server-composed session",
            "modelRoute": {
                "providerId": "provider-main",
                "modelId": "model-main",
                "credentialReferenceId": id("crd", 1)
            }
        }
    }))
    .expect("generated session.create command");
    let created = application
        .command(&principal, CommandFamily::Session, create.clone())
        .expect("create ProductSession");
    assert_eq!(
        created,
        application
            .command(&principal, CommandFamily::Session, create)
            .expect("replay ProductSession create")
    );
    let created = completed_json(created);
    assert_eq!(created["currentRevision"], 1);
    assert_eq!(created["result"]["state"], "idle");

    let stale_chat = chat_submit_request(21, 0);
    let conflict = application
        .command(&principal, CommandFamily::Session, stale_chat)
        .expect_err("stale Chat revision");
    assert_eq!(conflict.code(), "REVISION_CONFLICT");
    let submitted = application
        .command(
            &principal,
            CommandFamily::Session,
            chat_submit_request(22, 1),
        )
        .expect("submit Chat message");
    let submitted = completed_json(submitted);
    assert_eq!(submitted["currentRevision"], 2);
    assert_eq!(submitted["result"]["state"], "running");

    assert_chat_message(&application, &principal);

    let cancel: CommandRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 24),
        "command": "session.cancel",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "expectedRevision": 2,
        "payload": {
            "productSessionId": id("psn", 1),
            "reason": "user requested cancellation"
        }
    }))
    .expect("generated session.cancel command");
    let cancelled = application
        .command(&principal, CommandFamily::Session, cancel)
        .expect("cancel ProductSession");
    let cancelled = completed_json(cancelled);
    assert_eq!(cancelled["currentRevision"], 3);
    assert_eq!(cancelled["result"]["state"], "cancelled");

    application.shutdown().expect("first shutdown");
    let restarted = open_application(&root, &subject).expect("restart application");
    let recovered = restarted
        .query(&principal, QueryFamily::Session, session_get_request(25, 1))
        .expect("recover ProductSession");
    let recovered = serde_json::to_value(recovered).expect("encode recovered response");
    assert_eq!(recovered["result"]["revision"], 3);
    assert_eq!(recovered["result"]["state"], "cancelled");
    let foreign = restarted
        .query(
            &principal,
            QueryFamily::Session,
            session_get_request(26, 99),
        )
        .expect_err("foreign repository is isolated");
    assert_eq!(foreign.code(), "RESOURCE_NOT_FOUND");

    restarted.shutdown().expect("second shutdown");
    fs::remove_dir_all(root).expect("remove temporary application directory");
}

fn assert_chat_message(
    application: &StandaloneControlPlaneApplication,
    principal: &AuthenticatedPrincipal,
) {
    let query: QueryRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 23),
        "query": "session.messages.list",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "parameters": { "productSessionId": id("psn", 1) },
        "page": { "cursor": null, "limit": 20 }
    }))
    .expect("generated session.messages.list query");
    let response = application
        .query(principal, QueryFamily::Session, query)
        .expect("read public Chat ledger");
    let response = serde_json::to_value(response).expect("encode messages response");
    assert_eq!(response["result"]["items"][0]["content"], "Run the checks");
    assert_eq!(response["result"]["items"][0]["role"], "user");
}

fn repository_scope_json(seed: u64) -> serde_json::Value {
    serde_json::json!({
        "kind": "repository",
        "organizationId": id("org", seed),
        "workspaceId": id("wsp", seed),
        "projectId": id("prj", seed),
        "repositoryId": id("rep", seed)
    })
}

fn completed_json(response: CommandDispatchResponse) -> serde_json::Value {
    let CommandDispatchResponse::Completed(response) = response else {
        panic!("ProductSession command must complete synchronously");
    };
    serde_json::to_value(response).expect("encode completed response")
}

fn chat_submit_request(request: u64, expected_revision: i64) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "command": "chat.submit",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "expectedRevision": expected_revision,
        "payload": {
            "productSessionId": id("psn", 1),
            "message": "Run the checks"
        }
    }))
    .expect("generated chat.submit command")
}

fn session_get_request(request: u64, scope_seed: u64) -> QueryRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "query": "session.get",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(scope_seed),
        "parameters": { "productSessionId": id("psn", 1) },
        "page": { "cursor": null, "limit": 1 }
    }))
    .expect("generated session.get query")
}

#[test]
fn worker_management_routes_replay_conflict_isolate_and_recover_after_restart() {
    let root = temporary_root("worker-management");
    let subject = id("usr", 1);
    let principal = test_principal(1);
    let mut storage = SqliteStorage::open(&root).expect("open application storage");
    storage
        .execution_registry()
        .expect("open Worker registry")
        .register_worker_for_scope(&worker_registration(), &worker_scope(1))
        .expect("register Worker");
    let application = compose_application(&root, &subject, storage).expect("application");

    let initial = application
        .query(&principal, QueryFamily::Worker, worker_get_request(30, 1))
        .expect("read enabled Worker");
    let initial = serde_json::to_value(initial).expect("encode Worker result");
    assert_eq!(initial["result"]["state"], "enabled");
    assert_eq!(initial["result"]["revision"], 0);

    let drain = worker_command_request("worker.drain", 31, 0);
    let drained = application
        .command(&principal, CommandFamily::Worker, drain.clone())
        .expect("drain Worker");
    assert_eq!(
        drained,
        application
            .command(&principal, CommandFamily::Worker, drain)
            .expect("replay Worker drain")
    );
    let drained = completed_json(drained);
    assert_eq!(drained["currentRevision"], 1);
    assert_eq!(drained["result"]["state"], "draining");
    assert_eq!(drained["result"]["capacity"], 4);
    assert!(drained["result"].get("availableCapacity").is_none());

    let stale = application
        .command(
            &principal,
            CommandFamily::Worker,
            worker_command_request("worker.enable", 32, 0),
        )
        .expect_err("stale Worker revision");
    assert_eq!(stale.code(), "REVISION_CONFLICT");
    application
        .command(
            &principal,
            CommandFamily::Worker,
            worker_command_request("worker.enable", 33, 1),
        )
        .expect("enable Worker");

    application.shutdown().expect("first shutdown");
    let restarted = open_application(&root, &subject).expect("restart application");
    let recovered = restarted
        .query(&principal, QueryFamily::Worker, worker_get_request(34, 1))
        .expect("recover Worker");
    let recovered = serde_json::to_value(recovered).expect("encode recovered Worker");
    assert_eq!(recovered["result"]["revision"], 2);
    assert_eq!(recovered["result"]["state"], "enabled");
    let foreign = restarted
        .query(&principal, QueryFamily::Worker, worker_get_request(35, 99))
        .expect_err("foreign Worker scope");
    assert_eq!(foreign.code(), "RESOURCE_NOT_FOUND");

    restarted.shutdown().expect("second shutdown");
    fs::remove_dir_all(root).expect("remove temporary application directory");
}

fn worker_registration() -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::TransportPrincipal {
            issuer: "fixture-issuer".to_owned(),
            subject: "worker-1".to_owned(),
            credential_fingerprint: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        capabilities: vec!["artifact_stream".to_owned(), "shell".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "build-local".to_owned(),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", 1)),
        request_id: RequestId(id("req", 1)),
        sent_at: winwincode_domain::Instant("2027-01-15T08:00:01.000Z".to_owned()),
        started_at: winwincode_domain::Instant("2027-01-15T08:00:00.000Z".to_owned()),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn worker_scope(seed: u64) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: winwincode_domain::WorkspaceId(id("wsp", seed)),
        project_id: winwincode_domain::ProjectId(id("prj", seed)),
        repository_id: winwincode_domain::RepositoryId(id("rep", seed)),
    }
}

fn worker_command_request(command: &str, request: u64, expected_revision: i64) -> CommandRequest {
    let payload = if command == "worker.drain" {
        serde_json::json!({
            "workerId": id("wrk", 1),
            "reason": "planned maintenance"
        })
    } else {
        serde_json::json!({ "workerId": id("wrk", 1) })
    };
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "command": command,
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "expectedRevision": expected_revision,
        "payload": payload
    }))
    .expect("generated Worker command")
}

fn worker_get_request(request: u64, scope_seed: u64) -> QueryRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "query": "worker.get",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(scope_seed),
        "parameters": { "workerId": id("wrk", 1) },
        "page": { "cursor": null, "limit": 1 }
    }))
    .expect("generated worker.get query")
}

fn settings_update_request(
    request: u64,
    scope_seed: u64,
    expected_revision: i64,
    worker_concurrency_limit: i64,
) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "command": "settings.update",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": {
            "kind": "organization",
            "organizationId": id("org", scope_seed)
        },
        "expectedRevision": expected_revision,
        "payload": {
            "patch": {
                "defaultModelRoute": null,
                "workerConcurrencyLimit": worker_concurrency_limit
            }
        }
    }))
    .expect("generated settings.update command")
}

fn settings_get_request(request: u64, scope_seed: u64) -> QueryRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "query": "settings.get",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": {
            "kind": "organization",
            "organizationId": id("org", scope_seed)
        },
        "parameters": {},
        "page": { "cursor": null, "limit": 1 }
    }))
    .expect("generated settings.get query")
}

#[test]
fn settings_routes_replay_conflict_isolate_and_recover_after_restart() {
    let root = temporary_root("settings");
    let subject = id("usr", 1);
    let principal = test_principal(1);
    let application = open_application(&root, &subject).expect("application");
    let update = settings_update_request(50, 1, 0, 3);

    let first = application
        .command(&principal, CommandFamily::Settings, update.clone())
        .expect("update settings");
    let replay = application
        .command(&principal, CommandFamily::Settings, update.clone())
        .expect("replay settings update");
    assert_eq!(first, replay);
    let changed = application
        .command(
            &principal,
            CommandFamily::Settings,
            settings_update_request(50, 1, 0, 4),
        )
        .expect_err("changed settings request replay");
    assert_eq!(changed.code(), "IDEMPOTENCY_CONFLICT");
    let stale = application
        .command(
            &principal,
            CommandFamily::Settings,
            settings_update_request(51, 1, 0, 4),
        )
        .expect_err("stale settings revision");
    assert_eq!(stale.code(), "REVISION_CONFLICT");

    let current = application
        .query(
            &principal,
            QueryFamily::Settings,
            settings_get_request(52, 1),
        )
        .expect("read settings");
    let current = serde_json::to_value(current).expect("encode settings");
    assert_eq!(current["result"]["revision"], 1);
    assert_eq!(current["result"]["workerConcurrencyLimit"], 3);
    assert_eq!(
        current["result"]["defaultModelRoute"],
        serde_json::Value::Null
    );
    let foreign = application
        .query(
            &principal,
            QueryFamily::Settings,
            settings_get_request(53, 99),
        )
        .expect("read isolated settings");
    let foreign = serde_json::to_value(foreign).expect("encode isolated settings");
    assert_eq!(foreign["result"]["revision"], 0);
    assert_eq!(foreign["result"]["workerConcurrencyLimit"], 1);

    application.shutdown().expect("first shutdown");
    let restarted = open_application(&root, &subject).expect("restart application");
    let recovered = restarted
        .query(
            &principal,
            QueryFamily::Settings,
            settings_get_request(54, 1),
        )
        .expect("recover settings");
    let recovered = serde_json::to_value(recovered).expect("encode recovered settings");
    assert_eq!(recovered["result"], current["result"]);

    restarted.shutdown().expect("second shutdown");
    fs::remove_dir_all(root).expect("remove temporary application directory");
}

#[test]
fn authorization_and_strongflow_query_use_real_application_boundaries() {
    let root = temporary_root("routing");
    let subject = id("usr", 1);
    let application = open_application(&root, &subject).expect("application");
    let principal = test_principal(1);
    let foreign = test_principal(2);
    assert_eq!(
        application
            .authorize_scope(&foreign, &scope(1))
            .expect_err("foreign principal")
            .code(),
        "PERMISSION_DENIED"
    );

    let delivery_query: QueryRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 8),
        "query": "delivery.get",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": {
            "kind": "repository",
            "organizationId": id("org", 1),
            "workspaceId": id("wsp", 1),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1)
        },
        "parameters": {
            "deliveryId": id("dlv", 1),
            "atCursor": null
        },
        "page": { "cursor": null, "limit": 20 }
    }))
    .expect("generated Delivery get");
    let unavailable = application
        .query(&principal, QueryFamily::Delivery, delivery_query)
        .expect_err("StrongFlow fails closed without installed trusted sources");
    assert_eq!(unavailable.status(), 503);
    assert_eq!(unavailable.code(), "TRUSTED_FACTS_UNAVAILABLE");

    assert_empty_delivery_catalog(&application, &principal);
    assert_empty_publication_catalog(&application, &principal);
    assert_empty_interaction_catalogs(&application, &principal);
    assert_missing_approval_command_uses_real_service(&application, &principal);
    assert_missing_publication_cancel_uses_real_service(&application, &principal);

    application.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("remove temporary application directory");
}

fn assert_empty_interaction_catalogs(
    application: &StandaloneControlPlaneApplication,
    principal: &AuthenticatedPrincipal,
) {
    for (family, request) in [
        (
            QueryFamily::Session,
            serde_json::json!({
                "schemaVersion": "winwincode/v1",
                "requestId": id("req", 42),
                "query": "session.interactions.list",
                "actor": { "kind": "user", "id": id("usr", 1) },
                "scope": repository_scope_json(1),
                "parameters": {
                    "productSessionId": id("psn", 1),
                    "states": []
                },
                "page": { "cursor": null, "limit": 20 }
            }),
        ),
        (
            QueryFamily::Approval,
            serde_json::json!({
                "schemaVersion": "winwincode/v1",
                "requestId": id("req", 43),
                "query": "approval.list",
                "actor": { "kind": "user", "id": id("usr", 1) },
                "scope": repository_scope_json(1),
                "parameters": { "states": [] },
                "page": { "cursor": null, "limit": 20 }
            }),
        ),
    ] {
        let query: QueryRequest =
            serde_json::from_value(request).expect("generated interaction query");
        let response = application
            .query(principal, family, query)
            .expect("read interaction catalog");
        let response = serde_json::to_value(response).expect("encode interaction catalog");
        assert_eq!(response["result"]["items"], serde_json::json!([]));
        assert_eq!(response["page"]["hasMore"], false);
    }
}

fn assert_missing_approval_command_uses_real_service(
    application: &StandaloneControlPlaneApplication,
    principal: &AuthenticatedPrincipal,
) {
    let command: CommandRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 44),
        "command": "approval.decide",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "expectedRevision": 1,
        "payload": {
            "approvalId": id("apr", 1),
            "binding": {
                "executionJobId": id("job", 1),
                "productSessionId": id("psn", 1),
                "sessionIdentity": {
                    "codexThreadId": id("cdx", 1),
                    "productSessionId": id("psn", 1),
                    "workerSessionId": id("wsn", 1)
                },
                "workerSessionId": id("wsn", 1)
            },
            "decision": "approve",
            "reason": "missing approval"
        }
    }))
    .expect("generated approval.decide command");
    let error = application
        .command(principal, CommandFamily::Approval, command)
        .expect_err("missing Approval is rejected by the real service");
    assert_eq!(error.status(), 404);
    assert_eq!(error.code(), "RESOURCE_NOT_FOUND");
}

fn assert_empty_delivery_catalog(
    application: &StandaloneControlPlaneApplication,
    principal: &AuthenticatedPrincipal,
) {
    let query: QueryRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 39),
        "query": "delivery.list",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "parameters": { "states": [] },
        "page": { "cursor": null, "limit": 20 }
    }))
    .expect("generated delivery.list query");
    let response = application
        .query(principal, QueryFamily::Delivery, query)
        .expect("read Delivery catalog");
    let response = serde_json::to_value(response).expect("encode Delivery catalog");
    assert_eq!(response["result"]["items"], serde_json::json!([]));
    assert_eq!(response["page"]["hasMore"], false);
}

fn assert_missing_publication_cancel_uses_real_service(
    application: &StandaloneControlPlaneApplication,
    principal: &AuthenticatedPrincipal,
) {
    let command: CommandRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 41),
        "command": "publication.cancel",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "expectedRevision": 1,
        "payload": {
            "publicationId": id("pub", 1),
            "reason": "withdraw missing publication"
        }
    }))
    .expect("generated publication.cancel command");
    let error = application
        .command(principal, CommandFamily::Publication, command)
        .expect_err("missing Publication is rejected by the real service");
    assert_eq!(error.status(), 404);
    assert_eq!(error.code(), "RESOURCE_NOT_FOUND");
}

fn assert_empty_publication_catalog(
    application: &StandaloneControlPlaneApplication,
    principal: &AuthenticatedPrincipal,
) {
    let query: QueryRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 40),
        "query": "publication.list",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": repository_scope_json(1),
        "parameters": { "deliveryId": null, "states": [] },
        "page": { "cursor": null, "limit": 20 }
    }))
    .expect("generated publication.list query");
    let response = application
        .query(principal, QueryFamily::Publication, query)
        .expect("read Publication catalog");
    let response = serde_json::to_value(response).expect("encode Publication catalog");
    assert_eq!(response["result"]["items"], serde_json::json!([]));
    assert_eq!(response["page"]["hasMore"], false);
}
