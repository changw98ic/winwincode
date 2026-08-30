// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::too_many_arguments, clippy::too_many_lines)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, params};
use winwincode_api::generated::{
    ControlPlaneWebSocketProductSessionChangedEvent,
    ControlPlaneWebSocketProductSessionMessageAppendedEvent, ModelRoute, RepositoryScope,
    RepositoryScopeKind,
};
use winwincode_control_plane::{
    AppendAssistantMessageCommand, AssistantMessageState, CancelProductSessionCommand,
    CloseProductSessionCommand, ContinueProductSessionCommand, CreateProductSessionCommand,
    ForkProductSessionCommand, ProductSessionCommandContext, ProductSessionExecutionConfig,
    ProductSessionPageRequest, ProductSessionService, ProductSessionServiceErrorCode,
    ProductSessionTurnState, ProductSessionTurnTerminalOutcome, SubmitChatMessageCommand,
};
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, CredentialReferenceId, DeliveryId, DeliveryTaskId,
    ExecutionAckSequence, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken,
    Instant, LeaseId, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, ServiceAccountId, Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId,
    WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::generated::{ExecutionOutcomeStatus, ExecutionOutcomeUsage};
use winwincode_session::{
    AuthenticatedActor, ProductSessionState, RouteWriteStatus, SessionBindingIdentity,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseWriteStatus, PublicEventActor,
    PublicEventScope, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-product-session-service-{label}-{}-{sequence}",
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

fn actor_key() -> ReceiptActorKey {
    winwincode_storage::receipt_actor_key(&public_actor()).expect("actor key")
}

fn repository_scope_key() -> ReceiptScopeKey {
    winwincode_storage::receipt_scope_key(&public_scope(1)).expect("repository scope key")
}

fn other_scope_key() -> ReceiptScopeKey {
    winwincode_storage::receipt_scope_key(&public_scope(2)).expect("other repository scope key")
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

fn outbox_payload(directory: &TestDirectory, request: u64, topic: &str) -> Vec<u8> {
    let connection =
        Connection::open(directory.0.join("control-plane.sqlite3")).expect("outbox database");
    connection
        .query_row(
            "SELECT payload FROM outbox WHERE request_id = ?1 AND topic = ?2",
            params![id("req", request), topic],
            |row| row.get(0),
        )
        .expect("outbox payload")
}

fn model_route() -> ModelRoute {
    ModelRoute {
        credential_reference_id: CredentialReferenceId(id("crd", 1)),
        model_id: "fixture-model".into(),
        provider_id: "fixture-provider".into(),
    }
}

fn execution_config(repository: u64) -> ProductSessionExecutionConfig {
    ProductSessionExecutionConfig::try_new(
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId(id("org", 1)),
            workspace_id: WorkspaceId(id("wsp", 1)),
            project_id: ProjectId(id("prj", 1)),
            repository_id: RepositoryId(id("rep", repository)),
        },
        "0123456789abcdef0123456789abcdef01234567",
        "codex-chat",
        3_600,
        1_073_741_824,
    )
    .expect("execution config")
}

fn context(
    scope: &ReceiptScopeKey,
    request: u64,
    expected_revision: u64,
    second: u64,
) -> ProductSessionCommandContext {
    ProductSessionCommandContext {
        receipt_identity: ReceiptIdentity::new(
            actor_key(),
            scope.clone(),
            RequestId(id("req", request)),
        )
        .expect("receipt identity"),
        expected_revision,
        event_id: ControlPlaneEventId(id("evt", request)),
        occurred_at: at(second),
        public_actor: public_actor(),
        public_scope: if scope == &other_scope_key() {
            public_scope(2)
        } else {
            public_scope(1)
        },
    }
}

fn create_command(
    scope: &ReceiptScopeKey,
    session: u64,
    request: u64,
    title: &str,
    second: u64,
) -> CreateProductSessionCommand {
    CreateProductSessionCommand {
        context: context(scope, request, 0, second),
        product_session_id: ProductSessionId(id("psn", session)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
        title: title.into(),
        model_route: model_route(),
    }
}

fn execution_scope(session: u64, delivery: Option<u64>) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
        delivery_id: delivery.map(|value| DeliveryId(id("dlv", value))),
        product_session_id: ProductSessionId(id("psn", session)),
    }
}

fn pool() -> WorkerPoolId {
    WorkerPoolId(id("wpl", 1))
}

fn registration(max_slots: u64) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "fixture-control-plane".into(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".into(), "artifact".into()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "local".into(),
        max_slots,
        message_id: ExecutionMessageId(id("xmsg", 1)),
        request_id: RequestId(id("req", 10_001)),
        sent_at: at(2),
        started_at: at(1),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn heartbeat(max_slots: u64) -> WorkerHeartbeatRequest {
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: max_slots,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", 2)),
        observed_at: at(3),
        sent_at: at(3),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn admission_boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
    let mut boundaries = vec![
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
    ];
    if let Some(delivery_id) = &scope.delivery_id {
        boundaries.push(ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        });
    }
    boundaries.extend([
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool(),
        },
    ]);
    boundaries
}

fn prepare_worker(storage: &mut SqliteStorage, max_slots: u64) {
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker(&registration(max_slots))
        .expect("Worker registration");
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(max_slots))
            .expect("Worker heartbeat")
            .status,
        LeaseWriteStatus::Accepted
    );
}

fn prepare_slot(
    storage: &mut SqliteStorage,
    session: u64,
    delivery: Option<u64>,
    job: u64,
    worker_session: u64,
    codex_thread: u64,
    request_seed: u64,
) -> (ExecutionQueueScope, WorkerSlotAuthority) {
    prepare_slot_for_job(
        storage,
        session,
        delivery,
        ExecutionJobId(id("job", job)),
        job,
        worker_session,
        codex_thread,
        request_seed,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_slot_for_job(
    storage: &mut SqliteStorage,
    session: u64,
    delivery: Option<u64>,
    job_id: ExecutionJobId,
    job_seed: u64,
    worker_session: u64,
    codex_thread: u64,
    request_seed: u64,
) -> (ExecutionQueueScope, WorkerSlotAuthority) {
    let scope = execution_scope(session, delivery);
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 10,
        max_queued: 10,
        token_budget: 100_000,
        cost_budget_microunits: 1_000_000,
        max_runtime_millis: 60_000,
    };
    {
        let mut admission = storage.execution_admission().expect("admission");
        for boundary in admission_boundaries(&scope) {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("admission policy");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: scope.clone(),
                user_id: UserId(id("usr", job_seed)),
                worker_pool_id: pool(),
                job_id: job_id.clone(),
                request_id: RequestId(id("req", request_seed)),
                repository_access: ExecutionRepositoryAccess::ReadOnly,
                reserved_tokens: 100,
                reserved_cost_microunits: 1_000,
                runtime_limit_millis: 30_000,
                submitted_at: at(4),
            })
            .expect("admission reserve");
        admission
            .start(&ExecutionReservationStart {
                scope: scope.clone(),
                worker_pool_id: pool(),
                job_id: job_id.clone(),
                request_id: RequestId(id("req", request_seed + 1)),
                expected_revision: 1,
                started_at: at(5),
            })
            .expect("admission start");
    }
    let lease = ExecutionLeaseClaim {
        expires_at: at(50),
        fencing_token: FencingToken(job_seed.to_string()),
        issued_at: at(5),
        job_id,
        lease_id: LeaseId(id("lse", job_seed)),
        message_id: ExecutionMessageId(id("xmsg", request_seed + 2)),
        payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        request_id: RequestId(id("req", request_seed + 2)),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
        attempt: 1,
    };
    assert_eq!(
        storage
            .execution_registry()
            .expect("registry")
            .claim_execution_job(&lease)
            .expect("lease")
            .status,
        LeaseWriteStatus::Accepted
    );
    let authority = WorkerSlotAuthority {
        worker_id: lease.worker_id,
        worker_instance_id: lease.worker_instance_id,
        worker_session_id: WorkerSessionId(id("wsn", worker_session)),
        codex_thread_id: CodexThreadId(id("cdx", codex_thread)),
        job_id: lease.job_id,
        lease_id: lease.lease_id,
        attempt: lease.attempt,
        fencing_token: lease.fencing_token,
    };
    {
        let mut slots = storage.worker_session_slots().expect("slots");
        slots
            .configure_resources(
                &authority.worker_id,
                &authority.worker_instance_id,
                WorkerSlotResourceLimits {
                    max_memory_bytes: 1_000,
                    max_disk_bytes: 1_000,
                    max_processes: 10,
                },
            )
            .expect("resource limits");
        slots
            .open(&WorkerSlotOpenRequest {
                authority: authority.clone(),
                resources: WorkerSlotResources {
                    memory_bytes: 10,
                    disk_bytes: 10,
                    process_slots: 1,
                },
                request_id: RequestId(id("req", request_seed + 3)),
                opened_at: at(6),
            })
            .expect("Worker slot");
    }
    (scope, authority)
}

fn continue_command(
    scope_key: &ReceiptScopeKey,
    session: u64,
    request: u64,
    runtime_authority: WorkerSlotAuthority,
    execution_scope: ExecutionQueueScope,
) -> ContinueProductSessionCommand {
    ContinueProductSessionCommand {
        context: context(scope_key, request, 1, 10),
        product_session_id: ProductSessionId(id("psn", session)),
        binding_identity: SessionBindingIdentity::product_session(
            ProductSessionId(id("psn", session)),
            runtime_authority.job_id.clone(),
        )
        .expect("ProductSession binding"),
        runtime_authority,
        execution_scope,
        worker_pool_id: pool(),
        model_exchange_id: ModelExchangeId(id("mdl", request)),
    }
}

fn submit_command(
    scope: &ReceiptScopeKey,
    session: u64,
    request: u64,
    expected_revision: u64,
    message: &str,
    second: u64,
) -> SubmitChatMessageCommand {
    SubmitChatMessageCommand {
        context: context(scope, request, expected_revision, second),
        product_session_id: ProductSessionId(id("psn", session)),
        message: message.into(),
        execution_config: execution_config(if scope == &other_scope_key() { 2 } else { 1 }),
    }
}

fn assistant_command(
    scope_key: &ReceiptScopeKey,
    session: u64,
    request: u64,
    runtime_authority: &WorkerSlotAuthority,
    execution_scope: &ExecutionQueueScope,
    model_exchange: u64,
    stream_sequence: u64,
    delta: &str,
    state: AssistantMessageState,
) -> AppendAssistantMessageCommand {
    AppendAssistantMessageCommand {
        context: context(scope_key, request, 2, 20 + request % 20),
        product_session_id: ProductSessionId(id("psn", session)),
        binding_identity: SessionBindingIdentity::product_session(
            ProductSessionId(id("psn", session)),
            runtime_authority.job_id.clone(),
        )
        .expect("assistant binding"),
        runtime_authority: runtime_authority.clone(),
        execution_scope: execution_scope.clone(),
        worker_pool_id: pool(),
        model_exchange_id: ModelExchangeId(id("mdl", model_exchange)),
        stream_sequence,
        public_text_delta: delta.into(),
        state,
        terminal_outcome: match state {
            AssistantMessageState::Streaming => None,
            AssistantMessageState::Completed => Some(ProductSessionTurnTerminalOutcome {
                status: ExecutionOutcomeStatus::Succeeded,
                usage: Some(ExecutionOutcomeUsage {
                    runtime_millis: 15,
                    tokens: 10,
                    cost_microunits: 1,
                }),
                last_event_sequence: ExecutionAckSequence(
                    i64::try_from(stream_sequence).expect("stream sequence"),
                ),
                finished_at: at(20 + request % 20),
            }),
            AssistantMessageState::Cancelled => Some(ProductSessionTurnTerminalOutcome {
                status: ExecutionOutcomeStatus::Cancelled,
                usage: None,
                last_event_sequence: ExecutionAckSequence(
                    i64::try_from(stream_sequence).expect("stream sequence"),
                ),
                finished_at: at(20 + request % 20),
            }),
            AssistantMessageState::Failed => Some(ProductSessionTurnTerminalOutcome {
                status: ExecutionOutcomeStatus::Failed,
                usage: None,
                last_event_sequence: ExecutionAckSequence(
                    i64::try_from(stream_sequence).expect("stream sequence"),
                ),
                finished_at: at(20 + request % 20),
            }),
        },
    }
}

#[test]
fn multiple_sessions_create_fork_query_close_replay_and_restart_deterministically() {
    let directory = TestDirectory::new("lifecycle");
    let scope = repository_scope_key();
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let (first, second, forked, closed) = {
        let mut service = ProductSessionService::new(&mut storage);
        let first_command = create_command(&scope, 1, 1, "First", 1);
        let first = service.create(&first_command).expect("first session");
        let second = service
            .create(&create_command(&scope, 2, 2, "Second", 2))
            .expect("second session");
        assert_eq!(first.record.session().revision(), 1);
        assert_eq!(second.record.session().revision(), 1);
        assert_eq!(first.catalog_revision, 1);
        assert_eq!(second.catalog_revision, 2);

        let replay = service.create(&first_command).expect("create replay");
        assert!(replay.replayed);
        assert_eq!(replay.record, first.record);
        assert_eq!(replay.catalog_revision, 1);

        let fork_command = ForkProductSessionCommand {
            context: context(&scope, 3, 1, 3),
            source_product_session_id: ProductSessionId(id("psn", 1)),
            product_session_id: ProductSessionId(id("psn", 3)),
            title: "Fork".into(),
        };
        let forked = service.fork(&fork_command).expect("fork session");
        assert_eq!(
            forked.record.forked_from(),
            Some(&ProductSessionId(id("psn", 1)))
        );
        assert!(forked.record.bindings().is_empty());
        assert_eq!(
            service.fork(&fork_command).expect("fork replay").record,
            forked.record
        );

        let closed = service
            .close(&CloseProductSessionCommand {
                context: context(&scope, 4, 1, 4),
                product_session_id: ProductSessionId(id("psn", 2)),
            })
            .expect("close second");
        assert_eq!(closed.record.session().state(), ProductSessionState::Closed);
        assert_eq!(closed.record.session().revision(), 2);
        let list = service.list(&scope).expect("session list");
        assert_eq!(
            list.iter()
                .map(|record| record.session().id().0.as_str())
                .collect::<Vec<_>>(),
            vec![id("psn", 1), id("psn", 2), id("psn", 3)]
        );
        assert!(
            service
                .list(&other_scope_key())
                .expect("isolated scope")
                .is_empty()
        );
        (first.record, second.record, forked.record, closed.record)
    };
    drop(storage);

    let mut reopened = SqliteStorage::open(&directory.0).expect("storage restart");
    let service = ProductSessionService::new(&mut reopened);
    assert_eq!(
        service
            .get(&scope, &ProductSessionId(id("psn", 1)))
            .expect("first after restart"),
        Some(first)
    );
    assert_eq!(
        service
            .get(&scope, &ProductSessionId(id("psn", 2)))
            .expect("second after restart")
            .expect("second"),
        closed
    );
    assert_ne!(second, closed);
    assert_eq!(
        service
            .get(&scope, &ProductSessionId(id("psn", 3)))
            .expect("fork after restart"),
        Some(forked)
    );
}

#[test]
fn continue_joins_exact_worker_slots_and_keeps_sibling_bindings_after_restart() {
    let directory = TestDirectory::new("bindings");
    let scope_key = repository_scope_key();
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    {
        let mut service = ProductSessionService::new(&mut storage);
        service
            .create(&create_command(&scope_key, 10, 10, "Ten", 1))
            .expect("session ten");
        service
            .create(&create_command(&scope_key, 11, 11, "Eleven", 2))
            .expect("session eleven");
    }
    prepare_worker(&mut storage, 2);
    let (scope_ten, authority_ten) = prepare_slot(&mut storage, 10, None, 10, 10, 10, 100);
    let (scope_eleven, authority_eleven) =
        prepare_slot(&mut storage, 11, Some(11), 11, 11, 11, 200);
    let (ten, eleven) = {
        let mut service = ProductSessionService::new(&mut storage);
        let command_ten = continue_command(&scope_key, 10, 20, authority_ten.clone(), scope_ten);
        let ten = service
            .continue_session(&command_ten)
            .expect("continue ten");
        let mut command_eleven =
            continue_command(&scope_key, 11, 21, authority_eleven.clone(), scope_eleven);
        command_eleven.binding_identity = SessionBindingIdentity::delivery_stage(
            DeliveryId(id("dlv", 11)),
            Some(DeliveryTaskId(id("dtk", 11))),
            StageRunId(id("run", 11)),
            ProductSessionId(id("psn", 11)),
            authority_eleven.job_id.clone(),
        )
        .expect("DeliveryStage binding");
        let eleven = service
            .continue_session(&command_eleven)
            .expect("continue eleven");
        assert_eq!(ten.record.session().state(), ProductSessionState::Running);
        assert_eq!(
            eleven.record.session().state(),
            ProductSessionState::Running
        );
        assert_eq!(ten.record.bindings().len(), 1);
        assert_eq!(eleven.record.bindings().len(), 1);
        assert_eq!(
            ten.record.bindings()[0].binding().worker_session_id(),
            Some(&authority_ten.worker_session_id)
        );
        assert_eq!(
            eleven.record.bindings()[0].binding().codex_thread_id(),
            Some(&authority_eleven.codex_thread_id)
        );
        assert_eq!(
            eleven.record.bindings()[0].binding().stage_run_id(),
            Some(&StageRunId(id("run", 11)))
        );
        let replay = service
            .continue_session(&command_ten)
            .expect("continue replay");
        assert!(replay.replayed);
        assert_eq!(replay.record, ten.record);
        (ten.record, eleven.record)
    };
    drop(storage);

    let mut reopened = SqliteStorage::open(&directory.0).expect("storage restart");
    let service = ProductSessionService::new(&mut reopened);
    assert_eq!(
        service
            .get(&scope_key, &ProductSessionId(id("psn", 10)))
            .expect("ten after restart"),
        Some(ten)
    );
    assert_eq!(
        service
            .get(&scope_key, &ProductSessionId(id("psn", 11)))
            .expect("eleven after restart"),
        Some(eleven)
    );
}

#[test]
fn request_and_runtime_identity_conflicts_fail_without_guessing_or_cross_session_reuse() {
    let directory = TestDirectory::new("identity-conflicts");
    let scope_key = repository_scope_key();
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let first_create = create_command(&scope_key, 20, 30, "Twenty", 1);
    {
        let mut service = ProductSessionService::new(&mut storage);
        service.create(&first_create).expect("session twenty");
        service
            .create(&create_command(&scope_key, 21, 31, "Twenty-one", 2))
            .expect("session twenty-one");
        let mut conflicting_create = first_create.clone();
        conflicting_create.title = "changed request body".into();
        assert_eq!(
            service
                .create(&conflicting_create)
                .expect_err("request id with changed command must conflict")
                .code(),
            ProductSessionServiceErrorCode::RequestConflict
        );
    }
    prepare_worker(&mut storage, 1);
    let (slot_scope, authority) = prepare_slot(&mut storage, 20, None, 20, 20, 20, 300);
    let mut wrong_authority = authority.clone();
    wrong_authority.codex_thread_id = CodexThreadId(id("cdx", 999));
    let mut service = ProductSessionService::new(&mut storage);
    assert_eq!(
        service
            .continue_session(&continue_command(
                &scope_key,
                20,
                40,
                wrong_authority,
                slot_scope.clone(),
            ))
            .expect_err("foreign CodexThread must fail")
            .code(),
        ProductSessionServiceErrorCode::BindingIdentityMismatch
    );

    let mut foreign_scope = slot_scope.clone();
    foreign_scope.product_session_id = ProductSessionId(id("psn", 21));
    let mut foreign_command =
        continue_command(&scope_key, 20, 41, authority.clone(), foreign_scope);
    foreign_command.binding_identity = SessionBindingIdentity::product_session(
        ProductSessionId(id("psn", 20)),
        authority.job_id.clone(),
    )
    .expect("binding");
    assert_eq!(
        service
            .continue_session(&foreign_command)
            .expect_err("foreign execution scope must fail")
            .code(),
        ProductSessionServiceErrorCode::BindingIdentityMismatch
    );

    service
        .continue_session(&continue_command(
            &scope_key,
            20,
            42,
            authority.clone(),
            slot_scope,
        ))
        .expect("exact binding");
    assert_eq!(
        service
            .continue_session(&continue_command(
                &scope_key,
                21,
                43,
                authority,
                execution_scope(21, None),
            ))
            .expect_err("WorkerSession cannot be guessed into another ProductSession")
            .code(),
        ProductSessionServiceErrorCode::BindingIdentityMismatch
    );
    assert_eq!(
        service
            .get(&scope_key, &ProductSessionId(id("psn", 21)))
            .expect("session twenty-one")
            .expect("session exists")
            .bindings()
            .len(),
        0
    );
}

#[test]
fn chat_submit_retains_route_replays_exactly_and_rejects_changed_body_or_public_identity() {
    let directory = TestDirectory::new("chat-submit");
    let scope = repository_scope_key();
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let mut service = ProductSessionService::new(&mut storage);
    let created = service
        .create(&create_command(&scope, 30, 50, "Chat", 1))
        .expect("session create");
    assert_eq!(created.record.model_route(), &model_route());

    let submit = submit_command(&scope, 30, 51, 1, "Implement the durable Chat ledger", 2);
    let accepted = service.submit_chat(&submit).expect("chat submit");
    assert_eq!(accepted.message.role, "user");
    assert_eq!(accepted.message.state, "completed");
    assert_eq!(accepted.message.sequence, 1);
    assert_eq!(accepted.turn_intent.state, ProductSessionTurnState::Pending);
    assert_eq!(accepted.turn_intent.model_route, model_route());
    assert_eq!(accepted.mutation.record.session().revision(), 2);
    assert_eq!(
        accepted.mutation.record.session().state(),
        ProductSessionState::Running
    );

    let mut replay_with_new_server_facts = submit.clone();
    replay_with_new_server_facts.context.event_id = ControlPlaneEventId(id("evt", 9_051));
    replay_with_new_server_facts.context.occurred_at = at(3);
    let replay = service
        .submit_chat(&replay_with_new_server_facts)
        .expect("exact client replay");
    assert!(replay.mutation.replayed);
    assert_eq!(replay.message, accepted.message);
    assert_eq!(replay.mutation.record.messages().len(), 1);
    assert_eq!(replay.mutation.record.turn_intents().len(), 1);

    let mut changed = submit.clone();
    changed.message = "changed body".into();
    assert_eq!(
        service
            .submit_chat(&changed)
            .expect_err("changed replay must fail")
            .code(),
        ProductSessionServiceErrorCode::RequestConflict
    );

    let mut mismatched_identity = create_command(&scope, 31, 52, "Mismatch", 4);
    mismatched_identity.context.public_actor = PublicEventActor::ServiceAccount {
        id: ServiceAccountId(id("svc", 1)),
    };
    assert_eq!(
        service
            .create(&mismatched_identity)
            .expect_err("receipt/public actor mismatch must fail")
            .code(),
        ProductSessionServiceErrorCode::InvalidInput
    );
    assert!(
        service
            .get(&scope, &ProductSessionId(id("psn", 31)))
            .expect("read after rejected create")
            .is_none()
    );

    let mut mismatched_scope = create_command(&scope, 32, 53, "Scope mismatch", 5);
    mismatched_scope.context.public_scope = public_scope(2);
    assert_eq!(
        service
            .create(&mismatched_scope)
            .expect_err("receipt/public scope mismatch must fail")
            .code(),
        ProductSessionServiceErrorCode::InvalidInput
    );
    assert!(
        service
            .get(&scope, &ProductSessionId(id("psn", 32)))
            .expect("read after rejected scope")
            .is_none()
    );
    drop(service);
    drop(storage);

    let changed_payload = outbox_payload(&directory, 50, "product-session.changed.v1");
    let changed: ControlPlaneWebSocketProductSessionChangedEvent =
        serde_json::from_slice(&changed_payload).expect("generated changed event");
    assert_eq!(changed.product_session_id, ProductSessionId(id("psn", 30)));
    assert_eq!(changed.status, "active");
    let changed_json = String::from_utf8(changed_payload).expect("public changed JSON");
    for forbidden in [
        "credentialReferenceId",
        "leaseId",
        "fencingToken",
        "routes",
        "bindings",
    ] {
        assert!(!changed_json.contains(forbidden));
    }

    let message_payload = outbox_payload(&directory, 51, "product-session.message.appended.v1");
    let appended: ControlPlaneWebSocketProductSessionMessageAppendedEvent =
        serde_json::from_slice(&message_payload).expect("generated message event");
    assert_eq!(appended.message, accepted.message);
    let message_json = String::from_utf8(message_payload).expect("public message JSON");
    for forbidden in [
        "credentialReferenceId",
        "leaseId",
        "fencingToken",
        "routes",
        "modelRoute",
    ] {
        assert!(!message_json.contains(forbidden));
    }
}

#[test]
fn assistant_public_stream_is_ordered_bounded_restart_safe_and_stably_paged() {
    let directory = TestDirectory::new("assistant-stream");
    let scope_key = repository_scope_key();
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let execution_job_id = {
        let mut service = ProductSessionService::new(&mut storage);
        service
            .create(&create_command(&scope_key, 40, 60, "Stream", 1))
            .expect("session create");
        service
            .submit_chat(&submit_command(&scope_key, 40, 61, 1, "Say hello", 2))
            .expect("chat submit")
            .turn_intent
            .execution_job_id
    };
    prepare_worker(&mut storage, 1);
    let (execution_scope, authority) =
        prepare_slot_for_job(&mut storage, 40, None, execution_job_id, 40, 40, 40, 600);
    {
        let mut continue_chat = continue_command(
            &scope_key,
            40,
            62,
            authority.clone(),
            execution_scope.clone(),
        );
        continue_chat.context.expected_revision = 2;
        continue_chat.model_exchange_id = ModelExchangeId(id("mdl", 40));
        ProductSessionService::new(&mut storage)
            .continue_session(&continue_chat)
            .expect("bind submitted turn");
    }

    let final_record = {
        let mut service = ProductSessionService::new(&mut storage);
        let first = assistant_command(
            &scope_key,
            40,
            63,
            &authority,
            &execution_scope,
            40,
            1,
            "Hello, ",
            AssistantMessageState::Streaming,
        );
        let streamed = service
            .append_assistant_message(&first)
            .expect("first assistant delta");
        assert_eq!(streamed.message.state, "streaming");
        assert_eq!(streamed.message.sequence, 2);

        let gap = assistant_command(
            &scope_key,
            40,
            64,
            &authority,
            &execution_scope,
            40,
            3,
            "gap",
            AssistantMessageState::Streaming,
        );
        assert_eq!(
            service
                .append_assistant_message(&gap)
                .expect_err("stream gap must fail")
                .code(),
            ProductSessionServiceErrorCode::StreamSequenceConflict
        );

        let unsafe_output = assistant_command(
            &scope_key,
            40,
            65,
            &authority,
            &execution_scope,
            40,
            2,
            "sk-proj-123456789012345678901234567890",
            AssistantMessageState::Streaming,
        );
        assert_eq!(
            service
                .append_assistant_message(&unsafe_output)
                .expect_err("credential-shaped output must fail")
                .code(),
            ProductSessionServiceErrorCode::CredentialLeak
        );

        let final_delta = assistant_command(
            &scope_key,
            40,
            66,
            &authority,
            &execution_scope,
            40,
            2,
            "world!",
            AssistantMessageState::Completed,
        );
        let completed = service
            .append_assistant_message(&final_delta)
            .expect("final assistant delta");
        assert_eq!(completed.message.content, "Hello, world!");
        assert_eq!(completed.message.state, "completed");
        assert_eq!(
            completed.mutation.record.session().state(),
            ProductSessionState::Idle
        );
        assert_eq!(completed.mutation.record.session().revision(), 3);
        assert_eq!(
            completed.mutation.record.turn_intents()[0].state,
            ProductSessionTurnState::Completed
        );
        completed.mutation.record
    };
    drop(storage);

    let mut reopened = SqliteStorage::open(&directory.0).expect("storage restart");
    let service = ProductSessionService::new(&mut reopened);
    let first_page = service
        .messages_page(
            &scope_key,
            &ProductSessionId(id("psn", 40)),
            &ProductSessionPageRequest {
                cursor: None,
                limit: 1,
            },
        )
        .expect("first message page");
    assert_eq!(first_page.items.len(), 1);
    assert!(first_page.has_more());
    let second_page = service
        .messages_page(
            &scope_key,
            &ProductSessionId(id("psn", 40)),
            &ProductSessionPageRequest {
                cursor: first_page.next_cursor,
                limit: 1,
            },
        )
        .expect("second message page after restart");
    assert_eq!(second_page.items[0].content, "Hello, world!");
    assert!(!second_page.has_more());
    assert_eq!(
        service
            .get(&scope_key, &ProductSessionId(id("psn", 40)))
            .expect("session after restart"),
        Some(final_record)
    );
    assert_eq!(
        service
            .messages_page(
                &other_scope_key(),
                &ProductSessionId(id("psn", 40)),
                &ProductSessionPageRequest {
                    cursor: None,
                    limit: 10,
                },
            )
            .expect_err("cross-scope message read must fail")
            .code(),
        ProductSessionServiceErrorCode::NotFound
    );
}

#[test]
fn cancellation_routes_exact_current_authority_replays_and_remains_distinct_from_close() {
    let directory = TestDirectory::new("cancel-routes");
    let scope_key = repository_scope_key();
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let execution_job_id = {
        let mut service = ProductSessionService::new(&mut storage);
        service
            .create(&create_command(&scope_key, 50, 70, "Cancel", 1))
            .expect("session create");
        service
            .submit_chat(&submit_command(&scope_key, 50, 71, 1, "Run task", 2))
            .expect("chat submit")
            .turn_intent
            .execution_job_id
    };
    prepare_worker(&mut storage, 1);
    let (execution_scope, authority) =
        prepare_slot_for_job(&mut storage, 50, None, execution_job_id, 50, 50, 50, 700);
    {
        let mut bind = continue_command(&scope_key, 50, 72, authority.clone(), execution_scope);
        bind.context.expected_revision = 2;
        bind.model_exchange_id = ModelExchangeId(id("mdl", 50));
        ProductSessionService::new(&mut storage)
            .continue_session(&bind)
            .expect("bind submitted turn");
    }
    let mut service = ProductSessionService::new(&mut storage);
    let cancel = CancelProductSessionCommand {
        context: context(&scope_key, 73, 2, 10),
        product_session_id: ProductSessionId(id("psn", 50)),
        actor: AuthenticatedActor::User(UserId(id("usr", 1))),
        reason: "user requested cancellation".into(),
    };
    let mut wrong_actor = cancel.clone();
    wrong_actor.actor = AuthenticatedActor::ServiceAccount(ServiceAccountId(id("svc", 1)));
    assert_eq!(
        service
            .cancel_session(&wrong_actor)
            .expect_err("foreign actor must fail before routing")
            .code(),
        ProductSessionServiceErrorCode::ActorMismatch
    );
    let mut stale_revision = cancel.clone();
    stale_revision.context.expected_revision = 1;
    assert_eq!(
        service
            .cancel_session(&stale_revision)
            .expect_err("stale revision must fail before routing")
            .code(),
        ProductSessionServiceErrorCode::RevisionConflict
    );
    let accepted = service.cancel_session(&cancel).expect("cancel session");
    assert_eq!(accepted.routing.status, RouteWriteStatus::Applied);
    assert_eq!(accepted.routing.routes.len(), 1);
    let route = &accepted.routing.routes[0];
    assert_eq!(route.job.execution_job_id, authority.job_id);
    assert_eq!(route.job.expected_revision, 2);
    assert_eq!(
        route
            .worker
            .as_ref()
            .expect("Worker route")
            .runtime
            .fencing_token,
        authority.fencing_token
    );
    assert_eq!(
        route
            .model_stream
            .as_ref()
            .expect("model route")
            .model_exchange_id,
        ModelExchangeId(id("mdl", 50))
    );
    assert_eq!(
        accepted.mutation.record.session().state(),
        ProductSessionState::Cancelled
    );
    assert_eq!(accepted.mutation.record.session().revision(), 3);

    let replay = service.cancel_session(&cancel).expect("cancel replay");
    assert!(replay.mutation.replayed);
    assert_eq!(replay.routing.status, RouteWriteStatus::Duplicate);
    assert_eq!(replay.routing.request_id, RequestId(id("req", 73)));
    assert_eq!(replay.routing.routes, accepted.routing.routes);

    let mut changed = cancel.clone();
    changed.reason = "changed cancellation reason".into();
    assert_eq!(
        service
            .cancel_session(&changed)
            .expect_err("changed cancel replay must fail")
            .code(),
        ProductSessionServiceErrorCode::RequestConflict
    );

    let closed = service
        .close(&CloseProductSessionCommand {
            context: context(&scope_key, 74, 3, 11),
            product_session_id: ProductSessionId(id("psn", 50)),
        })
        .expect("close cancelled session");
    assert_eq!(closed.record.session().state(), ProductSessionState::Closed);
    assert_eq!(closed.record.session().revision(), 4);

    drop(service);
    drop(storage);
    let connection = Connection::open(directory.0.join("control-plane.sqlite3"))
        .expect("open SQLite for corruption fixture");
    let (stream_id, payload): (String, Vec<u8>) = connection
        .query_row(
            "SELECT stream_id, payload FROM product_state WHERE stream_id LIKE 'product-sessions:%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load ProductSession catalog payload");
    let mut value: serde_json::Value =
        serde_json::from_slice(&payload).expect("decode ProductSession catalog payload");
    value["sessions"][id("psn", 50)]["cancellation"]["routes"][0]["workerSlotRevision"] =
        serde_json::Value::Null;
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            (
                serde_json::to_vec(&value).expect("encode corrupt fixture"),
                stream_id,
            ),
        )
        .expect("write corrupt cancellation route");
    drop(connection);

    let mut reopened = SqliteStorage::open(&directory.0).expect("restart after corruption");
    assert_eq!(
        ProductSessionService::new(&mut reopened)
            .get(&scope_key, &ProductSessionId(id("psn", 50)))
            .expect_err("incomplete cancellation route must fail closed")
            .code(),
        ProductSessionServiceErrorCode::CorruptState
    );
}

#[test]
fn session_list_cursor_is_scope_filter_and_revision_bound_across_restart() {
    let directory = TestDirectory::new("session-pages");
    let scope = repository_scope_key();
    let mut storage = SqliteStorage::open(&directory.0).expect("storage");
    let first_page = {
        let mut service = ProductSessionService::new(&mut storage);
        service
            .create(&create_command(&scope, 60, 80, "Sixty", 1))
            .expect("first session");
        service
            .create(&create_command(&scope, 61, 81, "Sixty-one", 2))
            .expect("second session");
        service
            .list_page(
                &scope,
                &[ProductSessionState::Idle],
                &ProductSessionPageRequest {
                    cursor: None,
                    limit: 1,
                },
            )
            .expect("first session page")
    };
    assert!(first_page.has_more());
    assert_eq!(
        first_page.items[0].session().id(),
        &ProductSessionId(id("psn", 60))
    );
    drop(storage);

    let mut reopened = SqliteStorage::open(&directory.0).expect("storage restart");
    let service = ProductSessionService::new(&mut reopened);
    let second_page = service
        .list_page(
            &scope,
            &[ProductSessionState::Idle],
            &ProductSessionPageRequest {
                cursor: first_page.next_cursor.clone(),
                limit: 1,
            },
        )
        .expect("second session page");
    assert_eq!(
        second_page.items[0].session().id(),
        &ProductSessionId(id("psn", 61))
    );
    assert!(!second_page.has_more());
    assert_eq!(
        service
            .list_page(
                &other_scope_key(),
                &[ProductSessionState::Idle],
                &ProductSessionPageRequest {
                    cursor: first_page.next_cursor,
                    limit: 1,
                },
            )
            .expect_err("cursor cannot cross scope")
            .code(),
        ProductSessionServiceErrorCode::CursorInvalid
    );
}
