// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::too_many_lines)]

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use serde_json::json;
use winwincode_api::generated::{ApprovalDecideCommand, InputRespondCommand, ModelRoute};
use winwincode_control_plane::{
    ChatInteractionApiService, ChatInteractionService, ChatInteractionServiceErrorCode,
    CollaborationInboxItemId, CollaborationInboxItemState, CollaborationInboxSourcePort,
    ContinueProductSessionCommand, CreateProductSessionCommand, DurableCollaborationInboxSource,
    DurableWorkerInteractionOutbound, GateCandidateIdentity, GateDecisionFact,
    GateInteractionActor, GateInteractionAuthority, GateInteractionCommandContext,
    GateInteractionService, GateInteractionState, GateInteractionSubject, ProductSessionApiClock,
    ProductSessionCommandContext, ProductSessionService, RecordApprovalInteractionCommand,
    RecordInputInteractionCommand, RegisterGateInteractionCommand, RoutableGateDecision,
    WorkerInteractionAuthority, WorkerInteractionConnectionErrorKind,
    WorkerInteractionDeliveryError, WorkerInteractionDeliveryErrorKind,
    WorkerInteractionOutboundPort,
};
use winwincode_domain::{
    ApprovalId, CodexThreadId, ControlPlaneEventId, CredentialReferenceId, DeliveryId,
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    ModelExchangeId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Revision, Sha256Digest, UserId, WorkContractId, WorkItemId, WorkRunId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind};
use winwincode_execution_port::action_gateway::GateDecision;
use winwincode_execution_port::generated::{
    ApprovalRequestMessage, ExecutionPortMessage, InputRequestMessage, LeaseRenewMessage,
};
use winwincode_execution_port::transport::{ExecutionPortCore, RemoteTransportAdapter};
use winwincode_session::SessionBindingIdentity;
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionLeaseRenewal, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationStart,
    LeaseWriteStatus, ProductStateStorage, PublicEventActor, PublicEventScope, ReceiptActorKey,
    ReceiptIdentity, ReceiptScopeKey, SqliteStorage, WorkerAuthenticationIdentity,
    WorkerHeartbeatRequest, WorkerOutboundAuthority, WorkerOutboundQueueConfig, WorkerPlatform,
    WorkerPoolId, WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotRecoveryAction, WorkerSlotRecoveryRequest, WorkerSlotResourceLimits,
    WorkerSlotResources,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-gate-interaction-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-02-15T08:00:{second:02}.000Z"))
}

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn actor_key() -> ReceiptActorKey {
    winwincode_storage::receipt_actor_key(&public_actor()).expect("actor key")
}

fn repository_scope_key() -> ReceiptScopeKey {
    winwincode_storage::receipt_scope_key(&public_scope(1)).expect("repository scope")
}

fn public_actor() -> PublicEventActor {
    PublicEventActor::User {
        id: UserId(id("usr", 1)),
    }
}

fn public_scope(repository: u64) -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", repository)),
    }
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
    }
}

fn model_route() -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        model_id: "fixture-model".into(),
        provider_id: "fixture-provider".into(),
    }
}

fn receipt(scope: &ReceiptScopeKey, request: u64) -> ReceiptIdentity {
    ReceiptIdentity::new(actor_key(), scope.clone(), RequestId(id("req", request)))
        .expect("receipt identity")
}

fn product_context(
    scope: &ReceiptScopeKey,
    request: u64,
    expected_revision: u64,
    second: u64,
) -> ProductSessionCommandContext {
    ProductSessionCommandContext {
        receipt_identity: receipt(scope, request),
        expected_revision,
        event_id: ControlPlaneEventId(id("evt", request)),
        occurred_at: at(second),
        public_actor: public_actor(),
        public_scope: public_scope(1),
    }
}

fn execution_scope(delivery: bool) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
        product_session_id: ProductSessionId(id("psn", 1)),
        delivery_id: delivery.then(|| DeliveryId(id("dlv", 1))),
    }
}

fn pool() -> WorkerPoolId {
    WorkerPoolId(id("wpl", 1))
}

fn worker_registration() -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "fixture-control-plane".into(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".into()],
        capability_digest: digest('a'),
        security_zone: "local".into(),
        max_slots: 2,
        message_id: ExecutionMessageId(id("xmsg", 1)),
        request_id: RequestId(id("req", 1_001)),
        sent_at: at(2),
        started_at: at(1),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn heartbeat() -> WorkerHeartbeatRequest {
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 2,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 2,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", 2)),
        observed_at: at(3),
        sent_at: at(3),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
    let mut values = vec![
        ExecutionAdmissionBoundary::Organization {
            organization_id: scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool(),
        },
    ];
    if let Some(delivery_id) = &scope.delivery_id {
        values.push(ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        });
    }
    values
}

fn prepare_runtime(storage: &mut SqliteStorage, delivery: bool) -> WorkerSlotAuthority {
    let scope = execution_scope(delivery);
    {
        let mut registry = storage.execution_registry().expect("registry");
        registry
            .register_worker(&worker_registration())
            .expect("register Worker");
        assert_eq!(
            registry
                .record_heartbeat(&heartbeat())
                .expect("heartbeat")
                .status,
            LeaseWriteStatus::Accepted
        );
    }
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 2,
        max_queued: 2,
        token_budget: Some(10_000),
        cost_budget_microunits: Some(10_000),
        max_runtime_millis: Some(60_000),
    };
    {
        let mut admission = storage.execution_admission().expect("admission");
        for boundary in boundaries(&scope) {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("configure admission");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: scope.clone(),
                user_id: UserId(id("usr", 1)),
                worker_pool_id: pool(),
                job_id: ExecutionJobId(id("job", 1)),
                request_id: RequestId(id("req", 1_010)),
                repository_access: ExecutionRepositoryAccess::ReadOnly,
                reserved_tokens: Some(100),
                reserved_cost_microunits: Some(100),
                runtime_limit_millis: Some(30_000),
                submitted_at: at(4),
            })
            .expect("reserve");
        admission
            .start(&ExecutionReservationStart {
                scope: scope.clone(),
                worker_pool_id: pool(),
                job_id: ExecutionJobId(id("job", 1)),
                request_id: RequestId(id("req", 1_011)),
                expected_revision: 1,
                started_at: at(5),
            })
            .expect("start");
    }
    let lease = ExecutionLeaseClaim {
        expires_at: at(50),
        fencing_token: FencingToken("1".into()),
        issued_at: at(5),
        job_id: ExecutionJobId(id("job", 1)),
        lease_id: LeaseId(id("lse", 1)),
        message_id: ExecutionMessageId(id("xmsg", 12)),
        payload_digest: digest('b'),
        request_id: RequestId(id("req", 1_012)),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
        attempt: 1,
    };
    assert_eq!(
        storage
            .execution_registry()
            .expect("registry")
            .claim_execution_job(&lease)
            .expect("claim lease")
            .status,
        LeaseWriteStatus::Accepted
    );
    let authority = WorkerSlotAuthority {
        worker_id: lease.worker_id,
        worker_instance_id: lease.worker_instance_id,
        worker_session_id: WorkerSessionId(id("wsn", 1)),
        codex_thread_id: CodexThreadId(id("cdx", 1)),
        job_id: lease.job_id,
        lease_id: lease.lease_id,
        attempt: lease.attempt,
        fencing_token: lease.fencing_token,
    };
    let mut slots = storage.worker_session_slots().expect("slots");
    slots
        .configure_resources(
            &authority.worker_id,
            &authority.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000,
                max_disk_bytes: 1_000,
                max_processes: 4,
            },
        )
        .expect("slot limits");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: authority.clone(),
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 1_013)),
            opened_at: at(6),
        })
        .expect("open slot");
    authority
}

fn replace_runtime_authority(
    storage: &mut SqliteStorage,
    original: &WorkerSlotAuthority,
) -> WorkerSlotAuthority {
    let mut registration = worker_registration();
    registration.worker_instance_id = WorkerInstanceId(id("wki", 2));
    registration.message_id = ExecutionMessageId(id("xmsg", 21));
    registration.request_id = RequestId(id("req", 1_021));
    registration.started_at = at(50);
    registration.sent_at = at(50);
    let replacement_lease = ExecutionLeaseClaim {
        expires_at: at(59),
        fencing_token: FencingToken("2".into()),
        issued_at: at(50),
        job_id: original.job_id.clone(),
        lease_id: LeaseId(id("lse", 2)),
        message_id: ExecutionMessageId(id("xmsg", 22)),
        payload_digest: digest('c'),
        request_id: RequestId(id("req", 1_022)),
        worker_id: original.worker_id.clone(),
        worker_instance_id: registration.worker_instance_id.clone(),
        attempt: 2,
    };
    {
        let mut registry = storage.execution_registry().expect("replacement registry");
        registry
            .register_worker(&registration)
            .expect("register replacement Worker");
        assert_eq!(
            registry
                .claim_execution_job(&replacement_lease)
                .expect("claim replacement lease")
                .status,
            LeaseWriteStatus::Accepted
        );
    }
    let mut slots = storage.worker_session_slots().expect("replacement slots");
    slots
        .configure_resources(
            &replacement_lease.worker_id,
            &replacement_lease.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000,
                max_disk_bytes: 1_000,
                max_processes: 4,
            },
        )
        .expect("replacement slot limits");
    let receipt = slots
        .reconcile_restart(&WorkerSlotRecoveryRequest {
            worker_id: replacement_lease.worker_id,
            worker_instance_id: replacement_lease.worker_instance_id,
            request_id: RequestId(id("req", 1_023)),
            recovered_at: at(51),
        })
        .expect("replace runtime authority");
    let [WorkerSlotRecoveryAction::Recovered { slot }] = receipt.actions.as_slice() else {
        panic!("the exact runtime slot must move to the replacement authority");
    };
    slot.authority.clone()
}

fn prepare_product_session(
    storage: &mut SqliteStorage,
    scope_key: &ReceiptScopeKey,
    runtime: &WorkerSlotAuthority,
    delivery: bool,
) -> u64 {
    let mut service = ProductSessionService::new(storage);
    service
        .create(&CreateProductSessionCommand {
            context: product_context(scope_key, 1, 0, 7),
            product_session_id: ProductSessionId(id("psn", 1)),
            project_id: ProjectId(id("prj", 1)),
            repository_id: RepositoryId(id("rep", 1)),
            title: "Gate route fixture".into(),
            model_route: model_route(),
        })
        .expect("create ProductSession");
    let binding_identity = if delivery {
        SessionBindingIdentity::delivery_work_run(
            DeliveryId(id("dlv", 1)),
            WorkContractId(id("wct", 1)),
            Revision(1),
            WorkItemId(id("wit", 1)),
            Revision(1),
            WorkRunId(id("wrn", 1)),
            ProductSessionId(id("psn", 1)),
            runtime.job_id.clone(),
        )
        .expect("Delivery binding")
    } else {
        SessionBindingIdentity::product_session(
            ProductSessionId(id("psn", 1)),
            runtime.job_id.clone(),
        )
        .expect("ProductSession binding")
    };
    service
        .continue_session(&ContinueProductSessionCommand {
            context: product_context(scope_key, 2, 1, 8),
            product_session_id: ProductSessionId(id("psn", 1)),
            binding_identity,
            runtime_authority: runtime.clone(),
            execution_scope: execution_scope(delivery),
            worker_pool_id: pool(),
            model_exchange_id: ModelExchangeId(id("mdl", 1)),
        })
        .expect("continue ProductSession")
        .record
        .session()
        .revision()
}

fn setup_input(
    label: &str,
) -> (
    TestDirectory,
    SqliteStorage,
    ReceiptScopeKey,
    WorkerSlotAuthority,
    u64,
) {
    let directory = TestDirectory::new(label);
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let scope = repository_scope_key();
    let runtime = prepare_runtime(&mut storage, false);
    let session_revision = prepare_product_session(&mut storage, &scope, &runtime, false);
    (directory, storage, scope, runtime, session_revision)
}

fn input_request(runtime: &WorkerSlotAuthority) -> InputRequestMessage {
    serde_json::from_value(json!({
        "allowEmpty": false,
        "choices": null,
        "expiresAt": at(40),
        "inputRequestId": id("inp", 1),
        "kind": "input.request",
        "lease": {
            "attempt": runtime.attempt,
            "expiresAt": at(50),
            "fencingToken": runtime.fencing_token,
            "issuedAt": at(5),
            "jobId": runtime.job_id,
            "leaseId": runtime.lease_id,
            "workerId": runtime.worker_id,
            "workerInstanceId": runtime.worker_instance_id
        },
        "messageId": id("xmsg", 100),
        "mode": "text",
        "prompt": "Enter the fixture value.",
        "requestId": id("req", 100),
        "schemaVersion": "winwincode/v1",
        "sentAt": at(10),
        "sessionIdentity": {
            "codexThreadId": runtime.codex_thread_id,
            "productSessionId": id("psn", 1),
            "workerSessionId": runtime.worker_session_id
        },
        "workerSessionId": runtime.worker_session_id
    }))
    .expect("input request")
}

fn input_authority(product_session_revision: u64) -> WorkerInteractionAuthority {
    WorkerInteractionAuthority {
        execution_scope: execution_scope(false),
        worker_pool_id: pool(),
        product_session_revision,
        job_revision: 2,
        worker_slot_revision: 1,
    }
}

fn input_response_command(value: &str) -> InputRespondCommand {
    serde_json::from_value(json!({
        "actor": {
            "id": id("usr", 1),
            "kind": "user"
        },
        "command": "input.respond",
        "expectedRevision": 1,
        "payload": {
            "executionJobId": id("job", 1),
            "inputRequestId": id("inp", 1),
            "productSessionId": id("psn", 1),
            "sessionIdentity": {
                "codexThreadId": id("cdx", 1),
                "productSessionId": id("psn", 1),
                "workerSessionId": id("wsn", 1)
            },
            "status": "provided",
            "value": {
                "mode": "text",
                "value": value
            },
            "workerSessionId": id("wsn", 1)
        },
        "requestId": id("req", 200),
        "schemaVersion": "winwincode/v1",
        "scope": {
            "kind": "repository",
            "organizationId": id("org", 1),
            "workspaceId": id("wsp", 1),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1)
        }
    }))
    .expect("input.respond command")
}

struct FixtureClock(VecDeque<Instant>);

impl ProductSessionApiClock for FixtureClock {
    fn now(&mut self) -> Instant {
        self.0.pop_front().expect("fixture clock value")
    }
}

#[derive(Default)]
struct CapturingOutbound {
    fail_next: bool,
    frames: Vec<Vec<u8>>,
}

impl WorkerInteractionOutboundPort for CapturingOutbound {
    fn deliver(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<(), WorkerInteractionDeliveryError> {
        self.frames
            .push(serde_json::to_vec(message).expect("outbound frame"));
        if std::mem::take(&mut self.fail_next) {
            return Err(WorkerInteractionDeliveryError::new(
                WorkerInteractionDeliveryErrorKind::Unavailable,
                "fixture transport lost after durable commit",
            ));
        }
        Ok(())
    }
}

fn interaction_query() -> winwincode_api::generated::ChatInteractionListQuery {
    serde_json::from_value(json!({
        "actor": {
            "id": id("usr", 1),
            "kind": "user"
        },
        "page": {
            "cursor": null,
            "limit": 20
        },
        "parameters": {
            "productSessionId": id("psn", 1),
            "states": ["resolved"]
        },
        "query": "session.interactions.list",
        "requestId": id("req", 300),
        "schemaVersion": "winwincode/v1",
        "scope": {
            "kind": "repository",
            "organizationId": id("org", 1),
            "workspaceId": id("wsp", 1),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1)
        }
    }))
    .expect("interaction query")
}

#[test]
fn input_response_replays_the_same_worker_frame_after_transport_loss() {
    let (directory, mut storage, scope, runtime, session_revision) = setup_input("input-replay");
    let record = RecordInputInteractionCommand {
        authority: input_authority(session_revision),
        request: input_request(&runtime),
    };
    let first_record = ChatInteractionService::new(&mut storage)
        .record_input(&record)
        .expect("record input request");
    assert_eq!(first_record.revision.0, 1);
    let record_replay = ChatInteractionService::new(&mut storage)
        .record_input(&record)
        .expect("record replay");
    assert!(record_replay.replayed);
    assert_eq!(record_replay.status, first_record.status);
    assert_eq!(record_replay.revision, first_record.revision);
    assert_eq!(
        record_replay.product_session_id,
        first_record.product_session_id
    );

    let mut changed_record = record.clone();
    changed_record.request.prompt = "Changed prompt".into();
    assert_eq!(
        ChatInteractionService::new(&mut storage)
            .record_input(&changed_record)
            .expect_err("changed Worker request")
            .code(),
        ChatInteractionServiceErrorCode::RequestConflict
    );

    let command = input_response_command("SUPER_SECRET_INPUT_VALUE");
    let mut first_clock = FixtureClock(VecDeque::from([at(20)]));
    let mut outbound = CapturingOutbound {
        fail_next: true,
        frames: Vec::new(),
    };
    let error = ChatInteractionApiService::new(&mut storage, &mut first_clock, &mut outbound)
        .respond_input(command.clone())
        .expect_err("transport loss");
    assert_eq!(
        error.code(),
        ChatInteractionServiceErrorCode::WorkerDelivery
    );
    assert_eq!(outbound.frames.len(), 1);

    let mut replay_clock = FixtureClock(VecDeque::from([at(30)]));
    ChatInteractionApiService::new(&mut storage, &mut replay_clock, &mut outbound)
        .respond_input(command.clone())
        .expect("replayed delivery");
    assert_eq!(outbound.frames.len(), 2);
    assert_eq!(outbound.frames[0], outbound.frames[1]);

    let mut changed = command;
    changed.payload.value = serde_json::from_value(json!({
        "mode": "text",
        "value": "CHANGED_SECRET_VALUE"
    }))
    .expect("changed value");
    let mut conflict_clock = FixtureClock(VecDeque::from([at(31)]));
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut conflict_clock, &mut outbound)
            .respond_input(changed)
            .expect_err("changed browser replay")
            .code(),
        ChatInteractionServiceErrorCode::RequestConflict
    );
    assert_eq!(outbound.frames.len(), 2);

    drop(storage);
    let mut reopened = SqliteStorage::open(&directory.0).expect("reopen storage");
    let page = ChatInteractionService::new(&mut reopened)
        .interactions(&scope, &interaction_query(), &at(31))
        .expect("restart-restored interactions");
    assert_eq!(page.result.items.len(), 1);
    drop(reopened);

    let connection =
        Connection::open(directory.0.join("control-plane.sqlite3")).expect("state database");
    let catalog: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id LIKE 'chat-interactions:%'",
            [],
            |row| row.get(0),
        )
        .expect("Chat catalog");
    let catalog = String::from_utf8(catalog).expect("catalog UTF-8");
    assert!(!catalog.contains("SUPER_SECRET_INPUT_VALUE"));
    assert!(!catalog.contains("CHANGED_SECRET_VALUE"));
    assert!(!catalog.contains("credentialReferenceId"));
    assert!(!catalog.contains("providerId"));

    let mut statement = connection
        .prepare("SELECT payload FROM outbox WHERE topic = 'chat-interactions.invalidated.v1'")
        .expect("public invalidation query");
    let payloads = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("public invalidation rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("public invalidation payloads");
    assert_eq!(payloads.len(), 2);
    for payload in payloads {
        let _: winwincode_api::generated::ControlPlaneWebSocketChatInteractionsInvalidatedEvent =
            serde_json::from_slice(&payload).expect("generated invalidation payload");
        let text = String::from_utf8(payload).expect("public payload UTF-8");
        assert!(!text.contains("SUPER_SECRET_INPUT_VALUE"));
        assert!(!text.contains("leaseId"));
        assert!(!text.contains("fencingToken"));
        assert!(!text.contains("credentialReferenceId"));
    }
}

#[test]
fn replay_after_runtime_replacement_reports_stale_authority_instead_of_unavailability() {
    let (directory, mut storage, _scope, runtime, session_revision) =
        setup_input("input-replay-stale-authority");
    ChatInteractionService::new(&mut storage)
        .record_input(&RecordInputInteractionCommand {
            authority: input_authority(session_revision),
            request: input_request(&runtime),
        })
        .expect("record input request");
    let command = input_response_command("REPLACEMENT_ROUTE_INPUT_SECRET");
    let mut unavailable = CapturingOutbound {
        fail_next: true,
        frames: Vec::new(),
    };
    let mut first_clock = FixtureClock(VecDeque::from([at(20)]));
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut first_clock, &mut unavailable)
            .respond_input(command.clone())
            .expect_err("first delivery is unavailable")
            .code(),
        ChatInteractionServiceErrorCode::WorkerDelivery
    );

    let replacement = replace_runtime_authority(&mut storage, &runtime);
    assert_eq!(replacement.worker_session_id, runtime.worker_session_id);
    assert_ne!(replacement.worker_instance_id, runtime.worker_instance_id);
    assert_ne!(replacement.fencing_token, runtime.fencing_token);
    let outbound_storage = SqliteStorage::open(&directory.0).expect("outbound storage");
    let mut outbound = DurableWorkerInteractionOutbound::new(
        outbound_storage,
        WorkerOutboundQueueConfig::default(),
    )
    .expect("durable outbound");
    for replayed_at in [at(55), at(56)] {
        let mut replay_clock = FixtureClock(VecDeque::from([replayed_at]));
        assert_eq!(
            ChatInteractionApiService::new(&mut storage, &mut replay_clock, &mut outbound)
                .respond_input(command.clone())
                .expect_err("stale durable route is rejected")
                .code(),
            ChatInteractionServiceErrorCode::AuthorityMismatch
        );
    }
    let connection = Connection::open(directory.0.join("control-plane.sqlite3"))
        .expect("outbound queue inspection");
    let queued: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM internal_worker_outbound_messages",
            [],
            |row| row.get(0),
        )
        .expect("outbound queue count");
    assert_eq!(queued, 0);
}

fn gate_context(
    scope: &ReceiptScopeKey,
    request: u64,
    second: u64,
) -> GateInteractionCommandContext {
    GateInteractionCommandContext {
        receipt_identity: receipt(scope, request),
        event_id: ControlPlaneEventId(id("evt", request)),
        occurred_at: at(second),
    }
}

fn approval_subject() -> GateInteractionSubject {
    GateInteractionSubject::Approval(ApprovalId(id("apr", 1)))
}

fn gate_authority(
    runtime: &WorkerSlotAuthority,
    product_session_revision: u64,
) -> GateInteractionAuthority {
    GateInteractionAuthority {
        execution_scope: execution_scope(false),
        worker_pool_id: pool(),
        product_session_revision: Some(product_session_revision),
        work_contract_id: None,
        work_contract_revision: None,
        work_item_id: None,
        work_item_revision: None,
        work_run_id: None,
        job_revision: 2,
        worker_slot_revision: 1,
        runtime: runtime.clone(),
        lease_expires_at: at(50),
        gate: GateDecisionFact {
            decision: RoutableGateDecision::from_gate(&GateDecision::RequestPlanDelta {
                reason: "sealed plan delta".into(),
            })
            .expect("routable Gate decision"),
            action_id: "action:shell:fixture".into(),
            action_digest: digest('c'),
            envelope_version: 3,
            envelope_digest: digest('d'),
            decision_revision: 4,
            candidate: Some(GateCandidateIdentity {
                candidate_ref: format!("git-candidate:sha256:{}", "e".repeat(64)),
                candidate_digest: digest('e'),
                candidate_revision: 2,
            }),
        },
    }
}

fn register_approval(
    storage: &mut SqliteStorage,
    scope: &ReceiptScopeKey,
    authority: &GateInteractionAuthority,
) {
    GateInteractionService::new(storage)
        .register(&RegisterGateInteractionCommand {
            context: gate_context(scope, 400, 10),
            subject: approval_subject(),
            authority: authority.clone(),
            authorized_actor: GateInteractionActor::User(UserId(id("usr", 1))),
            expires_at: at(40),
            attention_decisions: Vec::new(),
        })
        .expect("register Approval");
}

fn approval_request(runtime: &WorkerSlotAuthority) -> ApprovalRequestMessage {
    serde_json::from_value(json!({
        "action": {
            "category": "shell",
            "sanitizedDetail": {
                "kind": "available",
                "operation": "execute",
                "targetSummaries": ["program:fixture;argument_count:0"],
                "targetCount": 1,
                "workingDirectory": "workspace",
                "riskLevel": "high",
                "reasonCode": "sandbox_escalation",
                "requestSha256": digest('f')
            },
            "summary": "Run the sealed fixture action."
        },
        "approvalId": id("apr", 1),
        "expiresAt": at(40),
        "kind": "approval.request",
        "lease": {
            "attempt": runtime.attempt,
            "expiresAt": at(50),
            "fencingToken": runtime.fencing_token,
            "issuedAt": at(5),
            "jobId": runtime.job_id,
            "leaseId": runtime.lease_id,
            "workerId": runtime.worker_id,
            "workerInstanceId": runtime.worker_instance_id
        },
        "messageId": id("xmsg", 410),
        "requestId": id("req", 410),
        "schemaVersion": "winwincode/v1",
        "sentAt": at(12),
        "sessionIdentity": {
            "codexThreadId": runtime.codex_thread_id,
            "productSessionId": id("psn", 1),
            "workerSessionId": runtime.worker_session_id
        },
        "workerSessionId": runtime.worker_session_id
    }))
    .expect("approval request")
}

fn approval_decide_command(
    request: u64,
    actor: u64,
    expected_revision: i64,
    reason: &str,
) -> ApprovalDecideCommand {
    serde_json::from_value(json!({
        "actor": {
            "id": id("usr", actor),
            "kind": "user"
        },
        "command": "approval.decide",
        "expectedRevision": expected_revision,
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
            "reason": reason
        },
        "requestId": id("req", request),
        "schemaVersion": "winwincode/v1",
        "scope": {
            "kind": "repository",
            "organizationId": id("org", 1),
            "workspaceId": id("wsp", 1),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1)
        }
    }))
    .expect("approval.decide command")
}

fn approval_get_query() -> winwincode_api::generated::ApprovalGetQuery {
    serde_json::from_value(json!({
        "actor": { "id": id("usr", 1), "kind": "user" },
        "page": { "cursor": null, "limit": 1 },
        "parameters": { "approvalId": id("apr", 1) },
        "query": "approval.get",
        "requestId": id("req", 500),
        "schemaVersion": "winwincode/v1",
        "scope": {
            "kind": "repository",
            "organizationId": id("org", 1),
            "workspaceId": id("wsp", 1),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1)
        }
    }))
    .expect("approval.get query")
}

fn approval_list_query() -> winwincode_api::generated::ApprovalListQuery {
    serde_json::from_value(json!({
        "actor": { "id": id("usr", 1), "kind": "user" },
        "page": { "cursor": null, "limit": 20 },
        "parameters": { "states": ["approved"] },
        "query": "approval.list",
        "requestId": id("req", 501),
        "schemaVersion": "winwincode/v1",
        "scope": {
            "kind": "repository",
            "organizationId": id("org", 1),
            "workspaceId": id("wsp", 1),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1)
        }
    }))
    .expect("approval.list query")
}

fn assert_pending(storage: &mut SqliteStorage, scope: &ReceiptScopeKey) {
    let gate = GateInteractionService::new(storage)
        .get(scope, &approval_subject())
        .expect("read Gate")
        .expect("Gate record");
    assert_eq!(gate.state, GateInteractionState::Pending);
    let approval = ChatInteractionService::new(storage)
        .approval_get(scope, &approval_get_query(), &at(20))
        .expect("read Approval");
    assert_eq!(approval.result.state, "pending");
}

#[test]
fn approval_without_trusted_action_detail_cannot_be_approved() {
    let (_directory, mut storage, scope, runtime, session_revision) =
        setup_input("approval-disabled");
    register_approval(
        &mut storage,
        &scope,
        &gate_authority(&runtime, session_revision),
    );
    let mut request = approval_request(&runtime);
    request.action.sanitized_detail = None;
    ChatInteractionService::new(&mut storage)
        .record_approval(&RecordApprovalInteractionCommand {
            public_scope: public_scope(1),
            request,
        })
        .expect("record disabled Approval");
    let projection = ChatInteractionService::new(&mut storage)
        .approval_get(&scope, &approval_get_query(), &at(20))
        .expect("read disabled Approval")
        .result;
    assert!(!projection.decision_enabled);

    let mut clock = FixtureClock(VecDeque::from([at(21)]));
    let mut outbound = CapturingOutbound::default();
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut clock, &mut outbound)
            .decide_approval(approval_decide_command(610, 1, 1, "DETAIL_REQUIRED"))
            .expect_err("missing trusted facts must disable approval")
            .code(),
        ChatInteractionServiceErrorCode::InvalidInput
    );
    assert!(outbound.frames.is_empty());
    assert_pending(&mut storage, &scope);
}

#[test]
fn approval_decision_is_atomic_restart_stable_and_replay_delivered() {
    let (directory, mut storage, scope, runtime, session_revision) = setup_input("approval-atomic");
    let authority = gate_authority(&runtime, session_revision);
    register_approval(&mut storage, &scope, &authority);

    let mut stale_fence = approval_request(&runtime);
    stale_fence.message_id = ExecutionMessageId(id("xmsg", 411));
    stale_fence.request_id = RequestId(id("req", 411));
    stale_fence.lease.fencing_token = FencingToken("2".into());
    assert_eq!(
        ChatInteractionService::new(&mut storage)
            .record_approval(&RecordApprovalInteractionCommand {
                public_scope: public_scope(1),
                request: stale_fence,
            })
            .expect_err("stale Worker fence")
            .code(),
        ChatInteractionServiceErrorCode::AuthorityMismatch
    );

    ChatInteractionService::new(&mut storage)
        .record_approval(&RecordApprovalInteractionCommand {
            public_scope: public_scope(1),
            request: approval_request(&runtime),
        })
        .expect("record Approval request");
    assert_pending(&mut storage, &scope);
    let mut collaboration_source = DurableCollaborationInboxSource::new(Box::new(
        SqliteStorage::open(&directory.0).expect("open collaboration source"),
    ));
    let pending_cut = collaboration_source
        .snapshot(&repository_scope())
        .expect("load pending collaboration Approval");
    assert_eq!(pending_cut.items.len(), 1);
    assert_eq!(
        pending_cut.items[0].id,
        CollaborationInboxItemId::Approval(ApprovalId(id("apr", 1)))
    );
    assert_eq!(
        pending_cut.items[0].state,
        CollaborationInboxItemState::Pending
    );
    assert_eq!(
        pending_cut.items[0]
            .candidate
            .as_ref()
            .expect("candidate identity")
            .candidate_revision,
        2
    );

    let mut outbound = CapturingOutbound::default();
    let mut foreign_clock = FixtureClock(VecDeque::from([at(15)]));
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut foreign_clock, &mut outbound)
            .decide_approval(approval_decide_command(420, 2, 1, "FOREIGN_ACTOR_REASON"))
            .expect_err("foreign actor")
            .code(),
        ChatInteractionServiceErrorCode::ActorMismatch
    );
    let mut stale_revision_clock = FixtureClock(VecDeque::from([at(16)]));
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut stale_revision_clock, &mut outbound)
            .decide_approval(approval_decide_command(421, 1, 2, "STALE_REVISION_REASON"))
            .expect_err("stale Approval revision")
            .code(),
        ChatInteractionServiceErrorCode::RevisionConflict
    );
    let mut expired_clock = FixtureClock(VecDeque::from([at(40)]));
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut expired_clock, &mut outbound)
            .decide_approval(approval_decide_command(422, 1, 1, "EXPIRED_REASON"))
            .expect_err("expired Approval")
            .code(),
        ChatInteractionServiceErrorCode::Expired
    );
    assert!(outbound.frames.is_empty());
    assert_pending(&mut storage, &scope);

    let trigger = Connection::open(directory.0.join("control-plane.sqlite3"))
        .expect("state database trigger connection");
    trigger
        .execute_batch(
            "CREATE TRIGGER fail_gate_secondary BEFORE UPDATE ON product_state
             WHEN OLD.stream_id LIKE 'gate-interaction:%'
             BEGIN SELECT RAISE(ABORT, 'fixture gate secondary failure'); END;",
        )
        .expect("install Gate failure trigger");
    let command = approval_decide_command(430, 1, 1, "SUPER_SECRET_APPROVAL_REASON");
    let mut failed_commit_clock = FixtureClock(VecDeque::from([at(20)]));
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut failed_commit_clock, &mut outbound)
            .decide_approval(command.clone())
            .expect_err("secondary Gate write fails")
            .code(),
        ChatInteractionServiceErrorCode::Storage
    );
    assert!(outbound.frames.is_empty());
    assert_pending(&mut storage, &scope);
    trigger
        .execute_batch("DROP TRIGGER fail_gate_secondary;")
        .expect("remove Gate failure trigger");
    drop(trigger);

    outbound.fail_next = true;
    let mut lost_delivery_clock = FixtureClock(VecDeque::from([at(21)]));
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut lost_delivery_clock, &mut outbound)
            .decide_approval(command.clone())
            .expect_err("lost Worker delivery")
            .code(),
        ChatInteractionServiceErrorCode::WorkerDelivery
    );
    let mut replay_clock = FixtureClock(VecDeque::from([at(30)]));
    ChatInteractionApiService::new(&mut storage, &mut replay_clock, &mut outbound)
        .decide_approval(command.clone())
        .expect("replayed Approval delivery");
    assert_eq!(outbound.frames.len(), 2);
    assert_eq!(outbound.frames[0], outbound.frames[1]);
    let decision: ExecutionPortMessage =
        serde_json::from_slice(&outbound.frames[0]).expect("Approval decision frame");
    let ExecutionPortMessage::ApprovalDecisionMessage(decision) = decision else {
        panic!("expected Approval decision frame");
    };
    assert_eq!(
        decision.reason.as_deref(),
        Some("SUPER_SECRET_APPROVAL_REASON")
    );

    let mut changed = command;
    changed.payload.reason = "CHANGED_APPROVAL_REASON".into();
    let mut conflict_clock = FixtureClock(VecDeque::from([at(31)]));
    assert_eq!(
        ChatInteractionApiService::new(&mut storage, &mut conflict_clock, &mut outbound)
            .decide_approval(changed)
            .expect_err("changed Approval replay")
            .code(),
        ChatInteractionServiceErrorCode::RequestConflict
    );
    assert_eq!(outbound.frames.len(), 2);

    let terminal_gate = GateInteractionService::new(&mut storage)
        .get(&scope, &approval_subject())
        .expect("read terminal Gate")
        .expect("terminal Gate");
    assert_eq!(terminal_gate.state, GateInteractionState::Approved);
    let approved_cut = collaboration_source
        .snapshot(&repository_scope())
        .expect("load approved collaboration Approval");
    assert_eq!(
        approved_cut.items[0].state,
        CollaborationInboxItemState::Approved
    );
    drop(storage);

    let mut reopened = SqliteStorage::open(&directory.0).expect("reopen storage");
    let get = ChatInteractionService::new(&mut reopened)
        .approval_get(&scope, &approval_get_query(), &at(31))
        .expect("restart Approval get");
    assert_eq!(get.result.state, "approved");
    let detail = serde_json::to_value(&get.result).expect("canonical Approval projection");
    assert_eq!(detail["category"], "shell");
    assert_eq!(detail["effectiveDecisionScope"], "once");
    assert_eq!(detail["decisionEnabled"], true);
    assert_eq!(
        detail["sanitizedDetail"],
        json!({
            "kind": "available",
            "operation": "execute",
            "targetSummaries": ["program:fixture;argument_count:0"],
            "targetCount": 1,
            "workingDirectory": "workspace",
            "riskLevel": "high",
            "reasonCode": "sandbox_escalation",
            "requestSha256": digest('f')
        })
    );
    assert!(detail.get("command").is_none());
    assert!(detail.get("cwd").is_none());
    assert!(detail.get("files").is_none());
    assert!(detail.get("network").is_none());
    assert!(detail.get("mcp").is_none());
    assert!(detail.get("risk").is_none());
    assert!(detail.get("reason").is_none());
    let list = ChatInteractionService::new(&mut reopened)
        .approval_list(&scope, &approval_list_query(), &at(31))
        .expect("restart Approval list");
    assert_eq!(list.result.items, vec![get.result]);
    drop(reopened);

    let connection =
        Connection::open(directory.0.join("control-plane.sqlite3")).expect("state database");
    let mut statement = connection
        .prepare(
            "SELECT payload FROM product_state
             WHERE stream_id LIKE 'chat-interactions:%' OR stream_id LIKE 'gate-interaction:%'",
        )
        .expect("private state query");
    let states = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("private state rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("private state payloads");
    for state in states {
        let text = String::from_utf8(state).expect("private state UTF-8");
        assert!(!text.contains("SUPER_SECRET_APPROVAL_REASON"));
        assert!(!text.contains("CHANGED_APPROVAL_REASON"));
        assert!(!text.contains("FOREIGN_ACTOR_REASON"));
        assert!(!text.contains("STALE_REVISION_REASON"));
        assert!(!text.contains("EXPIRED_REASON"));
        assert!(!text.contains("U0VDUkVUX0FDVElPTl9QQVlMT0FE"));
        assert!(!text.contains("dataBase64"));
        assert!(!text.contains("details"));
    }

    let mut statement = connection
        .prepare("SELECT payload FROM outbox WHERE topic = 'approval.changed.v1'")
        .expect("Approval public event query");
    let public_events = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("Approval public event rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("Approval public event payloads");
    assert_eq!(public_events.len(), 2);
    for payload in public_events {
        let event: winwincode_api::generated::ControlPlaneWebSocketApprovalChangedEvent =
            serde_json::from_slice(&payload).expect("generated Approval event");
        assert!(matches!(event.state.as_str(), "pending" | "approved"));
        assert_eq!(event.decision_reason, None);
        let text = String::from_utf8(payload).expect("public Approval UTF-8");
        assert!(!text.contains("SUPER_SECRET_APPROVAL_REASON"));
        assert!(!text.contains("leaseId"));
        assert!(!text.contains("fencingToken"));
        assert!(!text.contains("dataBase64"));
    }
}

#[derive(Default)]
struct AcceptedWorkerFrames(Vec<ExecutionPortMessage>);

impl ExecutionPortCore for AcceptedWorkerFrames {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        self.0.push(message.clone());
        Ok(())
    }
}

fn outbound_authority(runtime: &WorkerSlotAuthority) -> WorkerOutboundAuthority {
    WorkerOutboundAuthority {
        slot: runtime.clone(),
        lease_issued_at: at(5),
        lease_expires_at: at(50),
    }
}

fn approval_decision(runtime: &WorkerSlotAuthority, reason: &str) -> ExecutionPortMessage {
    ExecutionPortMessage::ApprovalDecisionMessage(
        serde_json::from_value(json!({
            "approvalId": id("apr", 900),
            "decidedAt": at(22),
            "decision": "approved",
            "kind": "approval.decision",
            "lease": {
                "attempt": runtime.attempt,
                "expiresAt": at(50),
                "fencingToken": runtime.fencing_token,
                "issuedAt": at(5),
                "jobId": runtime.job_id,
                "leaseId": runtime.lease_id,
                "workerId": runtime.worker_id,
                "workerInstanceId": runtime.worker_instance_id
            },
            "messageId": id("xmsg", 900),
            "reason": reason,
            "schemaVersion": "winwincode/v1",
            "scope": "once",
            "sentAt": at(22),
            "sessionIdentity": {
                "codexThreadId": runtime.codex_thread_id,
                "productSessionId": id("psn", 1),
                "workerSessionId": runtime.worker_session_id
            },
            "workerSessionId": runtime.worker_session_id
        }))
        .expect("approval decision fixture"),
    )
}

fn unsupported_worker_message() -> ExecutionPortMessage {
    ExecutionPortMessage::WorkerHeartbeatAckMessage(
        serde_json::from_value(json!({
            "error": null,
            "heartbeatSequence": 1,
            "kind": "worker.heartbeat_ack",
            "messageId": id("xmsg", 901),
            "nextHeartbeatWithinMs": 1000,
            "schemaVersion": "winwincode/v1",
            "sentAt": at(20),
            "serverTime": at(20),
            "status": "accepted",
            "workerId": id("wrk", 1),
            "workerInstanceId": id("wki", 1)
        }))
        .expect("unsupported outbound fixture"),
    )
}

fn assert_database_files_exclude(directory: &TestDirectory, needles: &[&str]) {
    let database = directory.0.join("control-plane.sqlite3");
    for path in [
        database.clone(),
        PathBuf::from(format!("{}-wal", database.display())),
        PathBuf::from(format!("{}-shm", database.display())),
    ] {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        for needle in needles {
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "restricted interaction payload remained in a database file"
            );
        }
    }
}

#[test]
fn production_outbound_is_durable_authority_bound_and_transport_identical() {
    const INPUT_SECRET: &str = "QUEUE_ONLY_INPUT_SECRET_8f38a1";
    const APPROVAL_SECRET: &str = "QUEUE_ONLY_APPROVAL_REASON_73bc12";

    let (directory, mut storage, _scope, runtime, session_revision) =
        setup_input("production-outbound");
    let record = RecordInputInteractionCommand {
        authority: input_authority(session_revision),
        request: input_request(&runtime),
    };
    ChatInteractionService::new(&mut storage)
        .record_input(&record)
        .expect("record input request");

    let outbound_storage = SqliteStorage::open(&directory.0).expect("outbound storage");
    let mut outbound = DurableWorkerInteractionOutbound::new(
        outbound_storage,
        WorkerOutboundQueueConfig::default(),
    )
    .expect("durable outbound");
    assert_eq!(outbound.database_path(), storage.database_path());
    let authority = outbound_authority(&runtime);

    let unsupported = outbound
        .deliver(&unsupported_worker_message())
        .expect_err("only interaction responses are accepted");
    assert_eq!(
        unsupported.kind(),
        WorkerInteractionDeliveryErrorKind::Rejected
    );
    assert!(
        outbound
            .claim_page(&authority, &at(20), None, 10)
            .expect("empty queue")
            .claims
            .is_empty()
    );

    let command = input_response_command(INPUT_SECRET);
    let mut first_clock = FixtureClock(VecDeque::from([at(20)]));
    ChatInteractionApiService::new(&mut storage, &mut first_clock, &mut outbound)
        .respond_input(command.clone())
        .expect("HTTP command completes after durable enqueue");
    let approval = approval_decision(&runtime, APPROVAL_SECRET);
    outbound
        .deliver(&approval)
        .expect("enqueue approval decision");
    outbound
        .deliver(&approval)
        .expect("exact approval replay is accepted");
    let mut changed_approval = approval_decision(&runtime, "CHANGED_QUEUE_APPROVAL_REASON");
    if let ExecutionPortMessage::ApprovalDecisionMessage(message) = &mut changed_approval {
        message.message_id = match &approval {
            ExecutionPortMessage::ApprovalDecisionMessage(original) => original.message_id.clone(),
            _ => unreachable!("approval fixture"),
        };
    }
    assert_eq!(
        outbound
            .deliver(&changed_approval)
            .expect_err("changed message body conflicts")
            .kind(),
        WorkerInteractionDeliveryErrorKind::Rejected
    );

    let first_page = outbound
        .claim_page(&authority, &at(23), None, 10)
        .expect("claim interaction frames");
    assert_eq!(first_page.claims.len(), 2);
    let first_encoded = first_page
        .claims
        .iter()
        .map(|claim| (claim.message_id().0.clone(), claim.encoded_frame().to_vec()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut local_worker = AcceptedWorkerFrames::default();
    for claim in &first_page.claims {
        let remote = RemoteTransportAdapter::<AcceptedWorkerFrames>::decode(claim.encoded_frame())
            .expect("remote typed frame");
        assert_eq!(&remote, claim.typed_frame());
        claim
            .deliver_local(&mut local_worker)
            .expect("local typed frame");
        let debug = format!("{claim:?}");
        assert!(!debug.contains(INPUT_SECRET));
        assert!(!debug.contains(APPROVAL_SECRET));
    }
    assert_eq!(local_worker.0.len(), 2);

    drop(outbound);
    let restarted_storage = SqliteStorage::open(&directory.0).expect("restart outbound storage");
    let mut restarted = DurableWorkerInteractionOutbound::new(
        restarted_storage,
        WorkerOutboundQueueConfig::default(),
    )
    .expect("restart durable outbound");
    let replay = restarted
        .claim_page(&authority, &at(24), None, 10)
        .expect("restart replays unacknowledged frames");
    assert_eq!(replay.claims.len(), 2);
    for claim in &replay.claims {
        assert!(claim.replayed());
        assert_eq!(
            first_encoded.get(&claim.message_id().0),
            Some(&claim.encoded_frame().to_vec())
        );
    }

    let mut stale = authority.clone();
    stale.slot.fencing_token = FencingToken("2".into());
    assert_eq!(
        restarted
            .claim_page(&stale, &at(24), None, 10)
            .expect_err("stale fence cannot claim")
            .kind(),
        WorkerInteractionConnectionErrorKind::AuthorityRejected
    );
    let first_message_id = replay.claims[0].message_id().clone();
    assert_eq!(
        restarted
            .acknowledge(&stale, &first_message_id, &at(25))
            .expect_err("stale fence cannot acknowledge")
            .kind(),
        WorkerInteractionConnectionErrorKind::AuthorityRejected
    );
    for claim in &replay.claims {
        restarted
            .acknowledge(&authority, claim.message_id(), &at(25))
            .expect("exact authority acknowledges");
    }
    assert!(
        restarted
            .claim_page(&authority, &at(26), None, 10)
            .expect("queue is empty after acknowledgements")
            .claims
            .is_empty()
    );

    restarted.close().expect("close outbound adapter");
    Box::new(storage).close().expect("close state storage");
    assert_database_files_exclude(
        &directory,
        &[
            INPUT_SECRET,
            APPROVAL_SECRET,
            "CHANGED_QUEUE_APPROVAL_REASON",
        ],
    );
}

#[test]
fn production_outbound_replays_original_approval_after_lease_renewal() {
    let (directory, mut storage, _scope, runtime, _revision) = setup_input("renewed-outbound");
    let mut outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&directory.0).expect("outbound storage"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("outbound");
    let authority = outbound_authority(&runtime);
    let approval = approval_decision(&runtime, "retained original approval");
    outbound.deliver(&approval).expect("enqueue original");
    let before = outbound
        .claim_page(&authority, &at(23), None, 10)
        .expect("before renewal");
    let original = before.claims[0].encoded_frame().to_vec();
    let mut renewed = authority.clone();
    renewed.lease_expires_at = at(58);
    assert_eq!(
        storage
            .execution_registry()
            .expect("registry")
            .renew_execution_lease(&ExecutionLeaseRenewal {
                expires_at: at(58),
                prior_expires_at: at(50),
                fencing_token: runtime.fencing_token.clone(),
                job_id: runtime.job_id.clone(),
                lease_id: runtime.lease_id.clone(),
                message_id: ExecutionMessageId(id("xmsg", 910)),
                request_id: RequestId(id("req", 910)),
                sent_at: at(24),
                worker_id: runtime.worker_id.clone(),
                worker_instance_id: runtime.worker_instance_id.clone(),
                attempt: runtime.attempt,
            })
            .expect("renew")
            .status,
        LeaseWriteStatus::Accepted
    );
    outbound.close().expect("close outbound");
    let mut outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&directory.0).expect("reopen"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("outbound");
    let after = outbound
        .claim_page(&renewed, &at(51), None, 10)
        .expect("historical approval");
    assert_eq!(after.claims.len(), 1);
    assert_eq!(after.claims[0].encoded_frame(), original);
    assert_eq!(after.claims[0].typed_frame().message(), &approval);
    assert!(after.claims[0].replayed());
    let mut invented = approval.clone();
    if let ExecutionPortMessage::ApprovalDecisionMessage(message) = &mut invented {
        message.message_id = ExecutionMessageId(id("xmsg", 911));
        message.lease.expires_at = at(55);
    }
    assert_eq!(
        outbound
            .deliver(&invented)
            .expect_err("unaccepted historic deadline")
            .kind(),
        WorkerInteractionDeliveryErrorKind::Rejected
    );
    outbound
        .acknowledge(&renewed, after.claims[0].message_id(), &at(52))
        .expect("ack old message");
    assert!(
        outbound
            .claim_page(&renewed, &at(53), None, 10)
            .expect("drained")
            .claims
            .is_empty()
    );
    outbound.close().expect("close");
    Box::new(storage).close().expect("close storage");
}

#[test]
fn production_renewal_is_atomic_durable_and_uses_canonical_worker_transport() {
    let (directory, mut storage, _scope, runtime, _revision) = setup_input("renewal-transport");
    let config = WorkerOutboundQueueConfig {
        max_claim_page_size: 1,
        ..WorkerOutboundQueueConfig::default()
    };
    let mut outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&directory.0).expect("outbound storage"),
        config,
    )
    .expect("outbound");
    let approval = approval_decision(&runtime, "pending approval");
    outbound
        .deliver(&approval)
        .expect("pending original interaction");
    let mut lease = match &approval {
        ExecutionPortMessage::ApprovalDecisionMessage(message) => message.lease.clone(),
        _ => unreachable!("approval fixture"),
    };
    lease.expires_at = at(58);
    let renewal: LeaseRenewMessage = serde_json::from_value(json!({
        "kind": "lease.renew", "lease": lease,
        "messageId": id("xmsg", 920), "priorExpiresAt": at(50),
        "requestId": id("req", 920), "schemaVersion": "winwincode/v1", "sentAt": at(24)
    }))
    .expect("renewal fixture");
    let message = ExecutionPortMessage::LeaseRenewMessage(renewal.clone());
    assert_eq!(
        outbound
            .deliver(&message)
            .expect_err("must use atomic renewal entry")
            .kind(),
        WorkerInteractionDeliveryErrorKind::Rejected
    );
    let mut foreign = runtime.clone();
    foreign.worker_session_id = WorkerSessionId(id("wsn", 999));
    assert!(outbound.renew_lease(&foreign, &renewal).is_err());
    assert_eq!(
        storage
            .execution_registry()
            .expect("registry")
            .load_lease(&runtime.job_id)
            .expect("load")
            .expect("lease")
            .expires_at,
        at(50)
    );
    let authority = outbound
        .renew_lease(&runtime, &renewal)
        .expect("atomic typed renewal");
    assert_eq!(authority.lease_expires_at, at(58));
    assert_eq!(
        outbound
            .renew_lease(&runtime, &renewal)
            .expect("exact replay"),
        authority
    );
    let mut changed = renewal.clone();
    changed.prior_expires_at = at(49);
    assert!(outbound.renew_lease(&runtime, &changed).is_err());
    let earlier_authority = authority;
    let mut second = renewal.clone();
    second.lease.expires_at = at(59);
    second.prior_expires_at = at(58);
    second.message_id = ExecutionMessageId(id("xmsg", 921));
    second.request_id = RequestId(id("req", 921));
    second.sent_at = at(25);
    let authority = outbound
        .renew_lease(&runtime, &second)
        .expect("second renewal");
    outbound.close().expect("close");
    let mut outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&directory.0).expect("reopen"),
        config,
    )
    .expect("outbound");
    let claims = outbound
        .claim_pending(&authority, &at(25))
        .expect("ordered recovered batch");
    assert_eq!(claims.len(), 3);
    assert_eq!(claims[0].message_id(), &renewal.message_id);
    assert_eq!(claims[1].message_id(), &second.message_id);
    assert_eq!(claims[2].typed_frame().message(), &approval);
    let claim = claims
        .iter()
        .find(|claim| claim.message_id() == &renewal.message_id)
        .expect("renewal frame persisted with lease");
    assert_eq!(claim.typed_frame().message(), &message);
    let remote = RemoteTransportAdapter::<AcceptedWorkerFrames>::decode(claim.encoded_frame())
        .expect("remote canonical frame");
    assert_eq!(&remote, claim.typed_frame());
    let mut local = AcceptedWorkerFrames::default();
    claim
        .deliver_local(&mut local)
        .expect("local canonical frame");
    assert_eq!(local.0, vec![message]);
    outbound
        .acknowledge(&authority, &renewal.message_id, &at(26))
        .expect("ack renewal");
    outbound
        .acknowledge(&authority, &second.message_id, &at(26))
        .expect("ack second renewal");
    assert_eq!(
        outbound
            .renew_lease(&runtime, &renewal)
            .expect("settled replay"),
        earlier_authority
    );
    let remaining = outbound
        .claim_pending(&authority, &at(27))
        .expect("remaining approval");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].typed_frame().message(), &approval);
    outbound.close().expect("close");
    Box::new(storage).close().expect("close storage");
}

#[test]
fn pending_approval_survives_renewal_without_extending_its_own_deadline() {
    for decision_second in [21, 40] {
        let (directory, mut storage, scope, runtime, revision) =
            setup_input("renewed-pending-approval");
        register_approval(&mut storage, &scope, &gate_authority(&runtime, revision));
        let request = approval_request(&runtime);
        ChatInteractionService::new(&mut storage)
            .record_approval(&RecordApprovalInteractionCommand {
                public_scope: public_scope(1),
                request: request.clone(),
            })
            .expect("original pending approval");
        let mut outbound = DurableWorkerInteractionOutbound::new(
            SqliteStorage::open(&directory.0).expect("outbound connection"),
            WorkerOutboundQueueConfig::default(),
        )
        .expect("outbound");
        let mut lease = request.lease.clone();
        lease.expires_at = at(58);
        let renewal: LeaseRenewMessage = serde_json::from_value(json!({
            "kind": "lease.renew", "lease": lease,
            "messageId": id("xmsg", 930), "priorExpiresAt": at(50),
            "requestId": id("req", 930), "schemaVersion": "winwincode/v1", "sentAt": at(20)
        }))
        .expect("renewal");
        let current = outbound.renew_lease(&runtime, &renewal).expect("renew");
        outbound.close().expect("close outbound");
        Box::new(storage).close().expect("close storage");
        let mut storage = SqliteStorage::open(&directory.0).expect("restart storage");
        let mut outbound = DurableWorkerInteractionOutbound::new(
            SqliteStorage::open(&directory.0).expect("restart outbound connection"),
            WorkerOutboundQueueConfig::default(),
        )
        .expect("restart outbound");
        let mut clock = FixtureClock(VecDeque::from([at(decision_second)]));
        let result = ChatInteractionApiService::new(&mut storage, &mut clock, &mut outbound)
            .decide_approval(approval_decide_command(931, 1, 1, "renewed lease decision"));
        if decision_second == 40 {
            assert_eq!(
                result
                    .expect_err("original approval deadline remains final")
                    .code(),
                ChatInteractionServiceErrorCode::Expired
            );
            assert_pending(&mut storage, &scope);
        } else {
            result.expect("new approval decision after renewal");
        }
        let claims = outbound
            .claim_pending(&current, &at(decision_second))
            .expect("claim current lease");
        assert_eq!(claims.len(), if decision_second == 40 { 1 } else { 2 });
        assert!(matches!(
            claims[0].typed_frame().message(),
            ExecutionPortMessage::LeaseRenewMessage(_)
        ));
        if decision_second == 21 {
            let ExecutionPortMessage::ApprovalDecisionMessage(decision) =
                claims[1].typed_frame().message()
            else {
                panic!("approval decision follows renewal");
            };
            assert_eq!(
                decision.lease, request.lease,
                "original signed request identity is retained"
            );
            assert_eq!(decision.sent_at, at(21));
            let original = claims[1].encoded_frame().to_vec();
            let mut clock = FixtureClock(VecDeque::from([at(22)]));
            ChatInteractionApiService::new(&mut storage, &mut clock, &mut outbound)
                .decide_approval(approval_decide_command(931, 1, 1, "renewed lease decision"))
                .expect("exact decision replay");
            let replay = outbound
                .claim_pending(&current, &at(22))
                .expect("replay batch");
            assert_eq!(replay.len(), 2);
            assert_eq!(replay[1].encoded_frame(), original);
        }
        outbound.close().expect("close outbound");
        Box::new(storage).close().expect("close storage");
    }
}

#[test]
fn pending_input_uses_current_renewed_lease_after_original_expiry() {
    let (directory, mut storage, _scope, runtime, revision) = setup_input("late-input-renewal");
    let mut request = input_request(&runtime);
    request.expires_at = at(59);
    ChatInteractionService::new(&mut storage)
        .record_input(&RecordInputInteractionCommand {
            authority: input_authority(revision),
            request: request.clone(),
        })
        .unwrap();
    let mut outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&directory.0).unwrap(),
        WorkerOutboundQueueConfig::default(),
    )
    .unwrap();
    let mut lease = request.lease.clone();
    lease.expires_at = at(58);
    let renewal: LeaseRenewMessage = serde_json::from_value(json!({
        "kind": "lease.renew", "lease": lease,
        "messageId": id("xmsg", 1940), "priorExpiresAt": at(50),
        "requestId": id("req", 1940), "schemaVersion": "winwincode/v1", "sentAt": at(20)
    }))
    .unwrap();
    let current = outbound.renew_lease(&runtime, &renewal).unwrap();
    let mut clock = FixtureClock(VecDeque::from([at(55)]));
    ChatInteractionApiService::new(&mut storage, &mut clock, &mut outbound)
        .respond_input(input_response_command("late but authorized input"))
        .expect("old message period cannot shorten the current lease");
    let claims = outbound.claim_pending(&current, &at(56)).unwrap();
    assert!(
        claims
            .iter()
            .any(|claim| matches!(claim.typed_frame().message(),
        ExecutionPortMessage::InputResponseMessage(response) if response.lease == request.lease))
    );
    outbound.close().unwrap();
    Box::new(storage).close().unwrap();
}
